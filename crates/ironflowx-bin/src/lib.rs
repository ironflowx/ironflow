//! The host: opens an engine on a data directory, serves it and answers for how it is
//! (architecture.md, "Host").

#![forbid(unsafe_code)]

mod accept;
mod key_file;
mod logging;
mod recovery;

pub use recovery::{generate_seed, recover_key};

#[cfg(feature = "__fault-points")]
pub use logging::panic_in_handler;

use std::convert::Infallible;
use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use ironflowx_core::{ErrorKind, KeyId, Secret};
use ironflowx_doors::{Readiness, Slot, WaitingOn, router};
use ironflowx_operations::{
    ControlPlaneState, Engine, EngineError, HostClock, LedgerState, OpenError, Opened, SeedState,
    StoreError, is_bootstrapped, key_id,
};
use ironflowx_proto::OpError;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const DRAIN_LIMIT: Duration = Duration::from_secs(10);

fn drain_deadline(start: Instant) -> Instant {
    start + DRAIN_LIMIT
}

/// Why a host did not serve.
/// Its `Display` shows the inner error, and it has no `source()`, so a reporter that walks the
/// chain shows the cause once (D84).
#[derive(Debug)]
pub enum ServeError {
    /// The data directory holds an `admin.key` that no finished bootstrap of it wrote.
    StaleKeyFile(PathBuf),
    Listen(io::Error),
    Signals(io::Error),
    LedgerFault,
    DataDir(PathBuf, io::Error),
    KeyLookup(PathBuf, io::Error),
    Engine(EngineError),
    Probe(PathBuf, io::Error),
    /// The engine opened and minted a key, which could not be written to `admin.key`.
    KeyFile(PathBuf, io::Error, KeyId),
}

impl ServeError {
    /// The process's exit code.
    pub fn exit_code(&self) -> u8 {
        self.kind().exit_code()
    }

    /// The refusal the binary writes after its log.
    pub fn envelope(&self) -> OpError {
        OpError {
            kind: self.kind(),
            message: self.to_string(),
            hint: match self {
                Self::KeyFile(_, _, id) => {
                    Some(format!("run ironflowx recover-key, then revoke {id}"))
                }
                _ => None,
            },
        }
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::StaleKeyFile(_) => ErrorKind::Invalid,
            Self::KeyLookup(..) | Self::LedgerFault => ErrorKind::Unavailable,
            Self::Listen(e) if e.kind() == io::ErrorKind::AddrInUse => ErrorKind::Unavailable,
            Self::Listen(_) => ErrorKind::Invalid,
            Self::DataDir(..) | Self::Probe(..) | Self::KeyFile(..) | Self::Signals(_) => {
                ErrorKind::Internal
            }
            Self::Engine(e) => engine_kind(e),
        }
    }
}

const fn engine_kind(error: &EngineError) -> ErrorKind {
    match error {
        EngineError::MalformedSeed | EngineError::NotAnEngineRoot | EngineError::SeedConflict => {
            ErrorKind::Invalid
        }
        EngineError::Format | EngineError::Orphaned | EngineError::LedgerMissing => {
            ErrorKind::Internal
        }
        EngineError::Locked | EngineError::Io(_) => ErrorKind::Unavailable,
        EngineError::Ledger(e) => match e {
            OpenError::WrongIncarnation
            | OpenError::Format
            | OpenError::Corrupt
            | OpenError::Missing => ErrorKind::Internal,
            OpenError::Locked | OpenError::Io(_) => ErrorKind::Unavailable,
        },
        EngineError::Store(e) => store_kind(e),
    }
}

const fn store_kind(error: &StoreError) -> ErrorKind {
    match error {
        StoreError::Format => ErrorKind::Internal,
        StoreError::Failed(_) => ErrorKind::Unavailable,
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::StaleKeyFile(path) => format!(
                "{} is another install's: no engine of this data directory wrote it",
                path.display()
            ),
            Self::Listen(e) => format!("cannot listen: {e}"),
            Self::Signals(e) => format!("cannot watch for shutdown signals: {e}"),
            Self::LedgerFault => "the ledger is unavailable".into(),
            Self::DataDir(path, e) => format!("cannot create {}: {e}", path.display()),
            Self::KeyLookup(path, e) => format!("cannot inspect {}: {e}", path.display()),
            Self::Engine(e @ EngineError::MalformedSeed) => return e.fmt(f),
            Self::Engine(e) => e.to_string(),
            Self::Probe(path, e) => format!(
                "cannot probe writes and hard links in {}: {e}",
                key_file::diagnostic_directory(path)
            ),
            Self::KeyFile(path, e, id) => {
                format!("cannot write {} for minted key {id}: {e}", path.display())
            }
        };
        f.write_str(&logging::public_text(&message))
    }
}

impl std::error::Error for ServeError {}

/// Listens on `listen`, opens the engine in `data_dir` and serves it, until it cannot.
///
/// # Errors
///
/// [`ServeError`], before anything is served or the data directory changed for a stale
/// `admin.key` without a `control.db` and for an address it cannot listen on.
pub async fn serve(data_dir: &Path, listen: SocketAddr) -> Result<(), ServeError> {
    serve_with_seed(data_dir, listen, None).await
}

/// Like [`serve`], with a seed the host supplies.
///
/// # Errors
///
/// As [`serve`], also for a malformed seed before the listener is bound.
pub async fn serve_with_seed(
    data_dir: &Path,
    listen: SocketAddr,
    seed: Option<Secret>,
) -> Result<(), ServeError> {
    if seed.as_ref().is_some_and(|key| key_id(key).is_none()) {
        return Err(ServeError::Engine(EngineError::MalformedSeed));
    }
    let mut signals = StopSignals::listen().map_err(ServeError::Signals)?;
    let listener = bind(data_dir, listen)?;
    let stop = CancellationToken::new();
    tokio::select! {
        biased;
        never = signals.watch(&stop) => match never {},
        result = run_with_seed(listener, data_dir, seed, &stop) => result,
    }
}

/// Watches both signals from before the listening address is published until close completes.
struct StopSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl StopSignals {
    fn listen() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn next(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }

    async fn watch(&mut self, stop: &CancellationToken) -> Infallible {
        self.next().await;
        stop.cancel();
        self.next().await;
        std::process::exit(i32::from(ErrorKind::Internal.exit_code()));
    }
}

/// The steps before the data directory is touched (D62).
fn bind(data_dir: &Path, listen: SocketAddr) -> Result<TcpListener, ServeError> {
    if key_file::exists(data_dir) && matches!(data_dir.join("control.db").try_exists(), Ok(false)) {
        return Err(ServeError::StaleKeyFile(key_file::path(data_dir)));
    }
    let listener = TcpListener::bind(listen)
        .map_err(|e| ServeError::Listen(io::Error::new(e.kind(), format!("{listen}: {e}"))))?;
    let bound = listener.local_addr().map_err(ServeError::Listen)?;
    tracing::info!(url = format!("http://{bound}"), "listening");
    if !bound.ip().is_loopback() {
        tracing::warn!("keys travel in clear without a TLS proxy");
    }
    Ok(listener)
}

/// Serves from a bound listener, with signal handling left to the caller.
/// Serves `/health` and the door at once, and the engine in `data_dir` when it has opened.
///
/// # Errors
///
/// As [`serve`].
pub async fn run(listener: TcpListener, data_dir: &Path) -> Result<(), ServeError> {
    run_with_seed(listener, data_dir, None, &CancellationToken::new()).await
}

async fn run_with_seed(
    listener: TcpListener,
    data_dir: &Path,
    seed: Option<Secret>,
    stop: &CancellationToken,
) -> Result<(), ServeError> {
    let listener = listener
        .set_nonblocking(true)
        .and_then(|()| tokio::net::TcpListener::from_std(listener))
        .map_err(ServeError::Listen)?;
    let slot = Arc::new(Slot::starting());
    let door = router(slot.clone());
    let mut connections = accept::Connections::default();
    let mut opening = pin!(open(data_dir, seed));

    let opened = tokio::select! {
        biased;
        () = stop.cancelled() => None,
        opened = &mut opening => Some(Arc::new(opened?)),
        never = connections.accept(&listener, &door) => match never {},
    };
    if let Some(engine) = &opened {
        slot.open(
            Arc::new(logging::Logged(engine.clone())),
            EngineReadiness(engine.clone()),
        );
        tokio::select! {
            biased;
            _ = engine.faulted() => {}
            () = stop.cancelled() => {}
            never = connections.accept(&listener, &door) => match never {},
        }
    }

    let deadline = drain_deadline(Instant::now());
    slot.drain();
    drop(listener);
    tracing::info!("stopping");

    let engine = match opened {
        Some(engine) => {
            let mut draining = pin!(connections.drain(deadline));
            tokio::select! {
                biased;
                _ = engine.faulted() => {
                    tracing::warn!("ledger fault during drain");
                    draining.await;
                }
                () = &mut draining => {}
            }
            engine
        }
        None => {
            // Open owns bootstrap and key publication. Keep it running while the transport
            // drains on its own deadline.
            let (engine, ()) = tokio::join!(opening, connections.drain(deadline));
            Arc::new(engine?)
        }
    };
    slot.close();
    let engine = Arc::into_inner(engine).expect("drain dropped every engine reference");
    if engine.close_with_state().await == LedgerState::Unavailable {
        Err(ServeError::LedgerFault)
    } else {
        Ok(())
    }
}

/// Opens the engine in `data_dir`, creating it if it is missing, and writes a key it minted.
async fn open(data_dir: &Path, seed: Option<Secret>) -> Result<Engine, ServeError> {
    let dir = data_dir.to_owned();
    let has_seed = seed.is_some();
    blocking(move || prepare(&dir, has_seed, is_bootstrapped)).await?;
    let Opened {
        engine,
        minted,
        lost_secret,
        seed,
    } = Engine::open(data_dir, HostClock, seed)
        .await
        .map_err(ServeError::Engine)?;
    if lost_secret {
        tracing::warn!("an earlier start minted a key nobody received; this open revoked it");
    }
    if seed == Some(SeedState::Revoked) {
        tracing::warn!("the seed is revoked");
    }
    if let Some(key) = minted {
        let dir = data_dir.to_owned();
        let path = key_file::path(data_dir);
        let id = key_id(&key).expect("the engine minted a key");
        match blocking(move || key_file::write(&dir, &key)).await {
            Ok(()) => {
                tracing::info!(path = %logging::public_text(&path.to_string_lossy()), "the admin key is written")
            }
            Err(e) => {
                engine.close().await;
                tracing::error!(key_id = %id, "run ironflowx recover-key, then revoke the minted key ID");
                return Err(ServeError::KeyFile(path, e, id));
            }
        }
    }
    Ok(engine)
}

/// Creates the data directory, and refuses an `admin.key` that no open of it wrote: one writes it
/// only once the bootstrap is marked.
fn prepare(
    data_dir: &Path,
    has_seed: bool,
    mut read_bootstrap: impl FnMut(&Path) -> Result<bool, EngineError>,
) -> Result<(), ServeError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(data_dir)
        .map_err(|e| ServeError::DataDir(data_dir.to_owned(), e))?;
    key_file::warn_temporaries(data_dir);
    let key_path = key_file::path(data_dir);
    if has_seed {
        if key_file::exists(data_dir) && matches!(read_bootstrap(data_dir), Ok(false)) {
            return Err(ServeError::StaleKeyFile(key_path));
        }
        return Ok(());
    }

    let bootstrap = read_bootstrap(data_dir);
    if !matches!(bootstrap, Ok(true)) {
        // There is a second read in open, so even a failed read here may precede a mint.
        match std::fs::symlink_metadata(&key_path) {
            Ok(_) => {
                // A host could have marked the bootstrap and published between the two reads.
                bootstrap.map_err(ServeError::Engine)?;
                return match read_bootstrap(data_dir).map_err(ServeError::Engine)? {
                    true => Ok(()),
                    false => Err(ServeError::StaleKeyFile(key_path)),
                };
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(ServeError::KeyLookup(key_path, e)),
        }
        key_file::probe(data_dir).map_err(|e| ServeError::Probe(data_dir.to_owned(), e))?;
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("the host's blocking steps do not panic")
}

/// An open engine, as `/ready` sees it.
struct EngineReadiness(Arc<Engine>);

impl Readiness for EngineReadiness {
    async fn ready(&self) -> Result<(), WaitingOn> {
        let health = self.0.health().await;
        match (health.control_plane, health.ledger) {
            (ControlPlaneState::Failed, _) => Err(WaitingOn::ControlPlane),
            (ControlPlaneState::Ok, LedgerState::Unavailable) => Err(WaitingOn::Ledger),
            (ControlPlaneState::Ok, LedgerState::Open) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ironflowx_proto::{Call, EmitRequest, ListEnvironmentsRequest, Operations as _};

    use super::*;

    #[test]
    fn every_open_refusal_has_its_table_code() {
        let io = || io::Error::other("fault");
        for (error, expected) in [
            (EngineError::MalformedSeed, 2),
            (EngineError::NotAnEngineRoot, 2),
            (EngineError::Locked, 8),
            (EngineError::Format, 1),
            (EngineError::Orphaned, 1),
            (EngineError::LedgerMissing, 1),
            (EngineError::SeedConflict, 2),
            (EngineError::Io(io()), 8),
            (EngineError::Store(StoreError::Format), 1),
            (EngineError::Store(StoreError::Failed("fault".into())), 8),
            (EngineError::Ledger(OpenError::Locked), 8),
            (EngineError::Ledger(OpenError::WrongIncarnation), 1),
            (EngineError::Ledger(OpenError::Format), 1),
            (EngineError::Ledger(OpenError::Corrupt), 1),
            (EngineError::Ledger(OpenError::Missing), 1),
            (EngineError::Ledger(OpenError::Io(io())), 8),
        ] {
            let error = ServeError::Engine(error);
            assert_eq!(error.exit_code(), expected, "{error}");
            assert_eq!(error.envelope().kind.exit_code(), expected, "{error}");
        }
    }

    #[test]
    fn signal_registration_failure_is_internal() {
        let error = ServeError::Signals(io::Error::other("fixture"));
        assert_eq!(error.envelope().kind, ErrorKind::Internal);
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn publication_after_an_unmarked_read_is_not_called_stale() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut winner = None;
        let ready = prepare(data_dir, false, |dir| {
            let marked = is_bootstrapped(dir);
            if winner.is_none() {
                assert!(matches!(marked, Ok(false)));
                let opened = runtime
                    .block_on(Engine::open(dir, HostClock, None))
                    .unwrap();
                key_file::write(dir, opened.minted.as_ref().unwrap()).unwrap();
                winner = Some(opened);
            }
            marked
        });
        assert!(ready.is_ok(), "{ready:?}");
        let lock = runtime.block_on(Engine::open(data_dir, HostClock, None));
        assert!(matches!(lock, Err(EngineError::Locked)));
        let winner = winner.unwrap();
        let key = std::fs::read_to_string(key_file::path(data_dir)).unwrap();
        assert!(key.trim_end() == winner.minted.as_ref().unwrap().expose());
        runtime.block_on(winner.engine.close());
    }

    #[tokio::test]
    async fn an_absent_destination_can_be_probed_after_a_transient_read_error() {
        use std::os::unix::fs::PermissionsExt as _;

        for finishing in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let data_dir = dir.path();
            let control = data_dir.join("control.db");
            if finishing {
                let opened = Engine::open(data_dir, HostClock, None).await.unwrap();
                opened.engine.close().await;
                let db = rusqlite::Connection::open(&control).unwrap();
                let unmark = "UPDATE environments SET ledger_created_at = NULL";
                assert_eq!(db.execute(unmark, []).unwrap(), 1);
            }
            let db = rusqlite::Connection::open(&control).unwrap();
            db.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
                .unwrap();
            let mut locker = Some(db);
            let result = prepare(data_dir, false, |dir| {
                let db = locker
                    .take()
                    .expect("only one read when the key name is absent");
                let state = is_bootstrapped(dir);
                assert!(matches!(state, Err(EngineError::Store(_))), "{state:?}");
                db.execute_batch("ROLLBACK").unwrap();
                drop(db);
                state
            });
            result.unwrap();
            assert!(!is_bootstrapped(data_dir).unwrap());
            assert!(!key_file::exists(data_dir));
            assert!(!std::fs::read_dir(data_dir).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(b".admin-key-probe.")
            }));

            // With no write permission, the same inconclusive branch must fail at the probe.
            let blocked = std::fs::Permissions::from_mode(0o500);
            std::fs::set_permissions(data_dir, blocked).unwrap();
            let result = prepare(data_dir, false, |_| {
                Err(EngineError::Store(StoreError::Failed(
                    "database is locked".into(),
                )))
            });
            std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(matches!(result, Err(ServeError::Probe(..))), "{result:?}");
            assert!(!is_bootstrapped(data_dir).unwrap());

            let opened = Engine::open(data_dir, HostClock, None).await.unwrap();
            assert!(opened.minted.is_some());
            assert_eq!(opened.lost_secret, finishing);
            opened.engine.close().await;
        }
    }

    /// SRV-9: each part of the engine's health that keeps `/ready` from 200.
    #[tokio::test]
    async fn readiness_is_the_engines_health() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Engine::open(dir.path(), HostClock, None).await.unwrap();
        let cx = Call::with_key(opened.minted.unwrap());
        let engine = Arc::new(opened.engine);
        let readiness = EngineReadiness(engine.clone());
        assert_eq!(readiness.ready().await, Ok(()));

        let control = rusqlite::Connection::open(dir.path().join("control.db")).unwrap();
        let gone = "ALTER TABLE environments RENAME TO gone";
        let back = "ALTER TABLE gone RENAME TO environments";
        control.execute_batch(gone).unwrap();
        assert_eq!(readiness.ready().await, Err(WaitingOn::ControlPlane));
        control.execute_batch(back).unwrap();
        assert_eq!(readiness.ready().await, Ok(()));

        // The fold of the next event fails, and so the ledger.
        let listed = engine.list_environments(&cx, ListEnvironmentsRequest {});
        let env = listed.await.unwrap().remove(0).env;
        let ledger = dir.path().join("ledgers").join(env.id().to_string());
        let trigger =
            "CREATE TRIGGER t BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'x'); END";
        let read = rusqlite::Connection::open(ledger.join("read.db")).unwrap();
        read.execute_batch(trigger).unwrap();
        let faulted = engine.faulted();
        let event = EmitRequest {
            name: "order.placed".into(),
            data: None,
            metadata: None,
            version: None,
            idempotency_key: None,
        };
        engine.emit(&cx, event).await.unwrap();
        let late = Duration::from_secs(10);
        tokio::time::timeout(late, faulted).await.expect("faulted");
        assert_eq!(readiness.ready().await, Err(WaitingOn::Ledger));
        control.execute_batch(gone).unwrap();
        assert_eq!(readiness.ready().await, Err(WaitingOn::ControlPlane));
    }
}
