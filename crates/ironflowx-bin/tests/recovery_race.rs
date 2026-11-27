//! Two recoveries pass file preflight before either delivers a key (SRV-7).

#[allow(dead_code)]
#[path = "../../../vendor/foundation/crates/ironflowx-operations/tests/leak/mod.rs"]
mod leak;

use std::env;
use std::io::{BufRead as _, BufReader, ErrorKind, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ironflowx_bin::recover_key;
use ironflowx_core::{EnvRef, KeyId, KeyOrigin, Secret, set_key_hook};
use ironflowx_operations::{Engine, HostClock};
use ironflowx_proto::{Call, ListKeysRequest, Operations as _, RevokeKeyRequest};
use serde_json::Value;

static SOCKET: OnceLock<PathBuf> = OnceLock::new();
const LATE: Duration = Duration::from_secs(10);

fn hold_recovery(_: EnvRef, id: KeyId, secret: Option<&Secret>) {
    let mut socket = UnixStream::connect(SOCKET.get().unwrap()).unwrap();
    socket.set_read_timeout(Some(LATE)).unwrap();
    // The fault hook's private socket is separate from the stdout and stderr under test.
    writeln!(socket, "{id}\n{}", secret.unwrap().expose()).unwrap();
    let mut go = [0_u8];
    socket.read_exact(&mut go).expect("release the recovery");
    assert_eq!(go, [1]);
}

fn recover_at_gate(
    dir: &Path,
    path: &Path,
    listener: &UnixListener,
) -> (Child, KeyId, Secret, UnixStream) {
    let worker = Command::new(env::current_exe().unwrap())
        .arg("--recovery-worker")
        .arg(dir)
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + LATE;
    let socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("recovery did not reach the post-preflight hook: {e}"),
        }
    };
    socket.set_read_timeout(Some(LATE)).unwrap();
    let mut reader = BufReader::new(socket);
    let mut id = String::new();
    let mut secret = String::new();
    reader.read_line(&mut id).unwrap();
    reader.read_line(&mut secret).unwrap();
    let id = id.trim_end().parse().unwrap();
    assert_eq!(secret.pop(), Some('\n'), "key line ends with a newline");
    (worker, id, Secret::new(secret), reader.into_inner())
}

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);
    if arguments
        .next()
        .is_some_and(|arg| arg == "--recovery-worker")
    {
        let data_dir = arguments.next().expect("data directory");
        SOCKET
            .set(arguments.next().expect("gate socket path").into())
            .unwrap();
        assert!(arguments.next().is_none());
        set_key_hook(hold_recovery);
        return ironflowx_cli::end(recover_key(Path::new(&data_dir), false));
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(the_loser_names_only_its_undelivered_key_and_keeps_the_winners_file());
    ExitCode::SUCCESS
}

async fn the_loser_names_only_its_undelivered_key_and_keeps_the_winners_file() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let dir = temp.path().join("engine");
    let opened = Engine::open(&dir, HostClock, None).await.unwrap();
    let bootstrap = opened.minted.unwrap();
    let path = temp.path().join("gate");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();

    // Both workers reach preflight's far side before either inserts a key. The winner
    // publishes before the loser resumes. The socket gates delivery, independent of delays.
    let (winner, winner_id, winner_key, mut go) = recover_at_gate(&dir, &path, &listener);
    let (loser, loser_id, loser_key, mut follow) = recover_at_gate(&dir, &path, &listener);
    go.write_all(&[1]).unwrap();
    let won = winner.wait_with_output().unwrap();
    assert_eq!(won.status.code(), Some(0));
    assert!(won.stdout.is_empty());
    assert!(won.stderr.is_empty());
    follow.write_all(&[1]).unwrap();
    let lost = loser.wait_with_output().unwrap();

    assert_eq!(lost.status.code(), Some(1));
    assert!(
        lost.stdout.is_empty(),
        "the losing recovery wrote to stdout"
    );
    let stderr = std::str::from_utf8(&lost.stderr).unwrap();
    assert_eq!(
        leak::leak(stderr, [winner_key.expose(), loser_key.expose()]),
        None,
        "the losing recovery disclosed a credential on stderr"
    );
    let error: Value = serde_json::from_str(stderr.lines().last().unwrap())
        .expect("the final stderr line is an envelope");
    assert_eq!(error["code"], "internal");
    assert_eq!(error["retryable"], false);
    let message = error["message"].as_str().unwrap();
    assert!(message.contains(&format!(
        "cannot deliver recovered key {loser_id} to admin.key"
    )));
    assert!(!message.contains(&winner_id.to_string()));
    let hint = error["hint"].as_str().unwrap();
    assert_eq!(
        hint,
        format!(
            "use a working admin key or recover one with ironflowx recover-key --stdout, then revoke {loser_id}"
        )
    );

    let file = dir.join("admin.key");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.strip_suffix('\n') == Some(winner_key.expose()),
        "kept the winner's whole key"
    );
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_name().as_encoded_bytes().ends_with(b".tmp"));
    }
    let cx = Call::with_key(winner_key);
    let engine = &opened.engine;
    let page = engine
        .list_keys(&cx, ListKeysRequest::default())
        .await
        .unwrap();
    let records: Vec<_> = page
        .keys
        .iter()
        .map(|key| (key.key_id, key.origin))
        .collect();
    assert_eq!(
        records,
        [
            (loser_id, KeyOrigin::Recovered),
            (winner_id, KeyOrigin::Recovered),
            (
                ironflowx_operations::key_id(&bootstrap).unwrap(),
                KeyOrigin::Minted
            ),
        ]
    );
    engine
        .revoke_key(
            &cx,
            RevokeKeyRequest {
                key_id: loser_id.to_string(),
            },
        )
        .await
        .unwrap();
    opened.engine.close().await;
}
