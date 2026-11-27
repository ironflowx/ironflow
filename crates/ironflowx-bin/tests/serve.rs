//! `ironflowx serve`, as a process.

#[path = "../../../vendor/foundation/crates/ironflowx-operations/tests/leak/mod.rs"]
mod leak;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::time::{Duration, Instant};

use ironflowx_cell::new_env_ref;
use ironflowx_core::{ErrorKind, Scope, Secret};
use ironflowx_operations::{Engine, HostClock};
use ironflowx_proto::{
    Call, Client, CreateKeyRequest, EmitRequest, KeyStatus, ListEnvironmentsRequest,
    ListKeysRequest, OpError, Operations as _, RevokeKeyRequest, WhoamiRequest,
};
use ironflowx_store::{Bootstrap, ControlPlane};
use leak::{files, leak, leak_in};
use serde_json::Value;

const ID: &str = "0190b5f0-0000-7000-8000-000000000001";

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ironflowx"));
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"IRONFLOWX_") {
            command.env_remove(name);
        }
    }
    command
}

fn command(data_dir: &Path, listen: &str) -> Command {
    let mut command = binary();
    command.args(["serve", "--listen", listen, "--data-dir"]);
    command.arg(data_dir).stdin(Stdio::null());
    command
}

/// A served engine; killed when dropped.
struct Served {
    child: Child,
    /// What it has logged so far: its `listening` line.
    listening: String,
    log: BufReader<ChildStderr>,
    addr: SocketAddr,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Served {
    /// Starts it and waits until it is ready.
    fn start(data_dir: &Path, listen: &str) -> Self {
        Self::from_command(command(data_dir, listen))
    }

    fn from_command(mut command: Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut log = BufReader::new(child.stderr.take().unwrap());
        let mut listening = String::new();
        let addr: SocketAddr = loop {
            let mut raw = String::new();
            assert_ne!(log.read_line(&mut raw).unwrap(), 0, "{listening}");
            let line: Value = serde_json::from_str(&raw).unwrap();
            listening.push_str(&raw);
            if line["fields"]["message"] == "listening" {
                let url = line["fields"]["url"].as_str().unwrap();
                break url.strip_prefix("http://").unwrap().parse().unwrap();
            }
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], addr.port()));
        let served = Self {
            child,
            listening,
            log,
            addr,
        };
        let started = Instant::now();
        while !get(served.addr, "/ready").starts_with("HTTP/1.0 200 ") {
            assert!(started.elapsed() < Duration::from_secs(30), "not ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        served
    }

    /// Stops it; all it logged. It printed nothing to stdout.
    fn stop(mut self) -> String {
        self.child.kill().unwrap();
        let mut printed = String::new();
        let mut stdout = self.child.stdout.take().unwrap();
        stdout.read_to_string(&mut printed).unwrap();
        assert_eq!(printed, "");
        let mut log = std::mem::take(&mut self.listening);
        self.log.read_to_string(&mut log).unwrap();
        log
    }
}

fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(stream, "GET {path} HTTP/1.0\r\n\r\n").unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    answer
}

/// `path` as a log line holds it: a JSON string, its quotes included.
fn logged(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}

/// Runs a start that must fail: its exit code and its log.
fn refused(data_dir: &Path, listen: &str) -> (Option<i32>, String) {
    refusal(command(data_dir, listen).output().unwrap())
}

fn refusal(out: Output) -> (Option<i32>, String) {
    let log = String::from_utf8(out.stderr).unwrap();
    let envelope: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    let (kind, retryable) = match out.status.code() {
        Some(1) => ("internal", false),
        Some(2) => ("invalid", false),
        Some(8) => ("unavailable", true),
        other => panic!("unexpected status {other:?}"),
    };
    assert_eq!(envelope["code"], kind);
    assert_eq!(envelope["retryable"], retryable);
    assert_eq!(out.stdout, b"");
    (out.status.code(), log)
}

/// A directory without the setgid bit, which one made in it takes on Linux, as this one may have.
fn private() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o700)).unwrap();
    dir
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn the_first_start_writes_the_key_and_a_later_one_keeps_what_is_there() {
    let dir = private();
    let data_dir = dir.path().join("a/data");
    let key_file = data_dir.join("admin.key");
    let log = Served::start(&data_dir, "127.0.0.1:0").stop();
    assert_eq!((mode(&data_dir), mode(&key_file)), (0o700, 0o400));
    let written = std::fs::read_to_string(&key_file).unwrap();
    let key = written.strip_suffix('\n').unwrap();
    assert_eq!((key.len(), &key[..4]), (84, "ixk_"));
    assert!(log.contains(&logged(&key_file)), "{log}");
    assert_eq!(leak(&log, [key]), None, "{log}");
    assert!(!log.contains("TLS proxy"), "{log}");
    let names: Vec<_> = std::fs::read_dir(&data_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("admin.key"))
        .collect();
    assert_eq!(names, ["admin.key"]);

    // Not stale: this directory's engine wrote it.
    let log = Served::start(&data_dir, "127.0.0.1:0").stop();
    assert_eq!(std::fs::read_to_string(&key_file).unwrap(), written);
    assert!(!log.contains("admin.key"), "{log}");
    assert_eq!(leak(&log, [key]), None, "{log}");
    std::fs::remove_file(&key_file).unwrap();
    let served = Served::start(&data_dir, "127.0.0.1:0");
    let cx = Call::with_key(Secret::new(key.to_owned()));
    let client = Client::new(&format!("http://{}", served.addr)).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let me = runtime
        .block_on(client.whoami(&cx, WhoamiRequest {}))
        .unwrap();
    assert_eq!(me.key_id.to_string(), key[4..40]);
    served.stop();
    assert!(!key_file.exists());
}

#[test]
fn a_data_directory_that_is_there_keeps_its_mode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o750)).unwrap();
    Served::start(dir.path(), "127.0.0.1:0").stop();
    assert_eq!(mode(dir.path()), 0o750);
}

#[test]
fn a_key_file_without_a_control_plane_is_stale_before_the_address_is_bound() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = taken.local_addr().unwrap().to_string();
    for link in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("admin.key");
        if link {
            std::os::unix::fs::symlink("nothing", &key_file).unwrap();
        } else {
            std::fs::write(&key_file, "another install's").unwrap();
        }
        let (code, log) = refused(dir.path(), &listen);
        assert_eq!(code, Some(2), "{log}");
        assert!(log.contains("admin.key is another install's"), "{log}");
        assert!(!log.contains("listen"), "{log}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["admin.key"]);
        if !link {
            assert_eq!(std::fs::read(&key_file).unwrap(), b"another install's");
        }
    }
}

/// SRV-7: an open writes `admin.key` only once it has marked the bootstrap.
#[test]
fn a_key_file_before_the_bootstrap_is_marked_is_stale() {
    for bootstrapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let control = ControlPlane::open(&dir.path().join("control.db")).unwrap();
        if bootstrapped {
            let (_, digest) = ironflowx_authz::mint(ID.parse().unwrap(), [7; 32]);
            let b = Bootstrap {
                env: new_env_ref(),
                tenant: ID.parse().unwrap(),
                project: ID.parse().unwrap(),
                key: ironflowx_authz::KeyParts {
                    id: ID.parse().unwrap(),
                    digest,
                },
                by_seed: false,
            };
            control.ensure_bootstrap(&b, &HostClock).unwrap();
        }
        drop(control);
        let key_file = dir.path().join("admin.key");
        std::fs::write(&key_file, "another install's").unwrap();
        let (code, log) = refused(dir.path(), "127.0.0.1:0");
        assert_eq!(code, Some(2), "{log}");
        assert!(log.contains("admin.key is another install's"), "{log}");
        assert_eq!(std::fs::read(&key_file).unwrap(), b"another install's");
        // The engine did not open: it would have made these.
        assert!(!dir.path().join("engine.lock").exists());
        assert!(!dir.path().join("ledgers").exists());
    }
}

/// Whether the bootstrap is marked cannot be read, so the open says why.
#[test]
fn a_key_file_beside_a_broken_control_plane_is_not_called_stale() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("control.db"), "not a database").unwrap();
    std::fs::write(dir.path().join("admin.key"), "a key").unwrap();
    let (code, log) = refused(dir.path(), "127.0.0.1:0");
    assert_eq!(code, Some(8), "{log}");
    assert!(!log.contains("another install's"), "{log}");
    assert!(log.contains("not a database"), "{log}");
}

/// D62: a start that cannot listen changes nothing on disk.
#[test]
fn an_address_in_use_leaves_no_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = taken.local_addr().unwrap().to_string();
    let (code, log) = refused(&data_dir, &listen);
    assert_eq!(code, Some(8), "{log}");
    assert!(log.contains(&format!("cannot listen: {listen}: ")), "{log}");
    assert!(!data_dir.exists());
}

#[test]
fn a_locked_engine_is_unavailable_and_a_bad_data_path_is_internal() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let opened = runtime
        .block_on(Engine::open(dir.path(), HostClock, None))
        .unwrap();
    let (code, log) = refused(dir.path(), "127.0.0.1:0");
    assert_eq!(code, Some(8), "{log}");
    assert!(log.contains("the engine root is in use"), "{log}");
    runtime.block_on(opened.engine.close());

    let file = dir.path().join("file");
    std::fs::write(&file, "").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink("nothing", &link).unwrap();
    for data_dir in [file.join("data"), file, link] {
        let (code, log) = refused(&data_dir, "127.0.0.1:0");
        assert_eq!(code, Some(1), "{log}");
        let path = logged(&data_dir);
        let path = &path[1..path.len() - 1];
        assert!(log.contains(&format!("cannot create {path}: ")), "{log}");
    }
}

#[test]
fn an_address_off_this_machine_warns_that_keys_travel_in_clear() {
    let dir = tempfile::tempdir().unwrap();
    let log = Served::start(dir.path(), "0.0.0.0:0").stop();
    let warned: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|l: &Value| l["level"] == "WARN")
        .collect();
    assert_eq!(warned.len(), 1, "{log}");
    let message = &warned[0]["fields"]["message"];
    assert_eq!(message, "keys travel in clear without a TLS proxy");
}

/// SRV-6, of an engine: each call of one client is answered for its own key, whichever came before.
/// TEN-3: no key a call or an answer held is logged, or stored but in `admin.key`.
#[tokio::test]
async fn one_client_gets_each_keys_own_answer_and_no_key_is_kept() {
    let dir = private();
    let served = Served::start(dir.path(), "127.0.0.1:0");
    let client = Client::new(&format!("http://{}", served.addr)).unwrap();
    let call = |key: &str| Call::with_key(Secret::new(key.to_owned()));
    let admin = std::fs::read_to_string(dir.path().join("admin.key")).unwrap();
    let admin = admin.trim().to_owned();
    let mut made = Vec::new();
    for scopes in [
        vec![Scope::Read],
        vec![Scope::Read, Scope::Write, Scope::Admin],
    ] {
        let request = CreateKeyRequest {
            scopes: scopes.into_iter().collect(),
            ..Default::default()
        };
        let created = client.create_key(&call(&admin), request).await.unwrap();
        let key = created.key.expose().to_owned();
        assert_eq!(
            (key.len(), &key[4..40]),
            (84, &*created.info.key_id.to_string())
        );
        made.push(key);
    }
    let [read, revoked] = &made[..] else {
        unreachable!()
    };
    let request = RevokeKeyRequest {
        key_id: revoked[4..40].to_owned(),
    };
    client.revoke_key(&call(&admin), request).await.unwrap();

    let refused = |kind, message: &str, hint: Option<&str>| OpError {
        kind,
        message: message.into(),
        hint: hint.map(str::to_owned),
    };
    let missing = refused(
        ErrorKind::PermissionDenied,
        "missing scope: admin",
        Some("create a key with scope admin, or use one that has it"),
    );
    for _ in 0..3 {
        for (cx, whoami, keys) in [
            (
                Call::anonymous(),
                Err(refused(
                    ErrorKind::Unauthenticated,
                    "authentication required",
                    None,
                )),
                Err(refused(
                    ErrorKind::Unauthenticated,
                    "authentication required",
                    None,
                )),
            ),
            (call(read), Ok(&read[4..40]), Err(missing.clone())),
            (
                call(revoked),
                Err(refused(ErrorKind::Unauthenticated, "invalid API key", None)),
                Err(refused(ErrorKind::Unauthenticated, "invalid API key", None)),
            ),
            (call(&admin), Ok(&admin[4..40]), Ok(())),
        ] {
            let me = client.whoami(&cx, WhoamiRequest {}).await;
            let me = me.map(|me| me.key_id.to_string());
            assert_eq!(me, whoami.map(str::to_owned));
            let listed = client.list_keys(&cx, ListKeysRequest::default()).await;
            assert_eq!(listed.as_ref().map(|_| ()).map_err(Clone::clone), keys);
            if let Ok(page) = listed {
                let listed: Vec<_> = page
                    .keys
                    .iter()
                    .map(|k| (k.key_id.to_string(), k.status))
                    .collect();
                let expected = [
                    (&revoked[4..40], KeyStatus::Revoked),
                    (&read[4..40], KeyStatus::Active),
                    (&admin[4..40], KeyStatus::Active),
                ];
                assert_eq!(listed, expected.map(|(id, status)| (id.to_owned(), status)));
            }
        }
    }
    let request = ListEnvironmentsRequest {};
    let environments = client
        .list_environments(&call(read), request)
        .await
        .unwrap();
    assert_eq!(environments.len(), 1);
    let control = dir.path().join("control.db");
    let control = rusqlite::Connection::open(control).unwrap();
    let sql = "SELECT id || '.' || token FROM environments";
    let env: String = control.query_row(sql, [], |row| row.get(0)).unwrap();
    assert_eq!(environments[0].env.to_string(), env);
    assert_eq!(environments[0].address.to_string(), "default/default");
    drop(control);

    let log = served.stop();
    let all = || [&admin, read, revoked].map(String::as_str);
    assert_eq!(leak(&log, all()), None);
    let files = files(dir.path());
    let held = |file: &&(_, _)| leak_in(file, all()).is_some();
    let held: Vec<_> = files.iter().filter(held).collect();
    assert_eq!(held, [&("admin.key".into(), format!("{admin}\n"))]);
    // What is stored is searched.
    for key in all() {
        assert!(files.iter().any(|(_, bytes)| bytes.contains(&key[4..40])));
    }
}

/// The deadline of `accept.rs`'s tests, on a real connection.
#[test]
fn a_silent_connection_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let served = Served::start(dir.path(), "127.0.0.1:0");
    let mut stream = TcpStream::connect(served.addr).unwrap();
    let late = Some(Duration::from_secs(30));
    stream.set_read_timeout(late).unwrap();
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}

fn wait_for(path: &Path) {
    let started = Instant::now();
    while !path.exists() && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `serve` under `umask`.
fn under_umask(umask: &str, data_dir: &Path) -> Child {
    let shell = "umask $2; exec \"$0\" serve --listen 127.0.0.1:0 --data-dir \"$1\"";
    Command::new("sh")
        .args(["-c", shell, env!("CARGO_BIN_EXE_ironflowx")])
        .arg(data_dir)
        .arg(umask)
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn a_directory_above_the_data_directory_is_private_too() {
    let dir = private();
    let above = dir.path().join("above");
    let data_dir = above.join("data");
    let mut child = under_umask("0000", &data_dir);
    wait_for(&data_dir.join("admin.key"));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!((mode(&above), mode(&data_dir)), (0o700, 0o700));
}

#[test]
fn the_key_file_is_its_owners_to_read_whatever_the_umask() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("admin.key");
    // A bootstrap left unmarked, its ledger built: the next open mints and makes no other file.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let open = Engine::open(dir.path(), HostClock, None);
    let engine = runtime.block_on(open).unwrap().engine;
    runtime.block_on(engine.close());
    let control = rusqlite::Connection::open(dir.path().join("control.db")).unwrap();
    let unmark = "UPDATE environments SET ledger_created_at = NULL";
    assert_eq!(control.execute(unmark, []).unwrap(), 1);
    drop(control);
    let mut child = under_umask("0477", dir.path());
    wait_for(&key_file);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(mode(&key_file), 0o400);
}

#[test]
fn bad_settings_are_usage_errors_before_any_files_are_written() {
    let dir = private();
    for (variable, expected) in [
        ("IRONFLOWX_DATA_DIR", "invalid IRONFLOWX_DATA_DIR"),
        ("IRONFLOWX_LISTEN", "invalid IRONFLOWX_LISTEN"),
        ("IRONFLOWX_SEED_FILE", "invalid IRONFLOWX_SEED_FILE"),
        ("IRONFLOWX_LOG_LEVEL", "invalid IRONFLOWX_LOG_LEVEL"),
        ("IRONFLOWX_LOG_FORMAT", "invalid IRONFLOWX_LOG_FORMAT"),
        (
            "IRONFLOWX_SEED",
            "malformed seed: a key is ixk_<key-id>_<secret>",
        ),
    ] {
        let (code, log) = refusal(
            binary()
                .arg("serve")
                .current_dir(dir.path())
                .env(variable, "")
                .output()
                .unwrap(),
        );
        assert_eq!(code, Some(2), "{log}");
        assert!(log.contains(expected), "{log}");
    }
    let secret = "not-a-key-but-secret";
    for key_file in [None, Some(dir.path().join(secret))] {
        let mut cmd = binary();
        cmd.arg("serve").current_dir(dir.path());
        let expected = if let Some(file) = key_file {
            cmd.arg("--seed-file").arg(file);
            "cannot read the seed file"
        } else {
            cmd.env("IRONFLOWX_SEED", secret);
            "malformed seed"
        };
        let (code, log) = refusal(cmd.output().unwrap());
        assert_eq!(code, Some(2), "{log}");
        assert!(log.contains(expected), "{log}");
        assert!(!log.contains(secret), "{log}");
    }
    for flag in [false, true] {
        let mut cmd = binary();
        cmd.arg("serve")
            .current_dir(dir.path())
            .env("IRONFLOWX_SEED", "");
        if flag {
            cmd.args(["--seed-file", "missing"]);
        } else {
            cmd.env("IRONFLOWX_SEED_FILE", "missing");
        }
        let (code, log) = refusal(cmd.output().unwrap());
        assert_eq!(code, Some(2), "{log}");
        assert!(log.contains("supply only one seed source"), "{log}");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn generated_seeds_authenticate_without_creating_a_key_file() {
    let dir = private();
    let generated = binary()
        .args(["seed", "generate"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        (generated.status.code(), generated.stderr),
        (Some(0), vec![])
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let seed: Value = serde_json::from_slice(&generated.stdout).unwrap();
    let key = seed["key"].as_str().unwrap();
    let file = dir.path().join("seed");
    let padded = format!(" \n{key}\t\n");
    std::fs::write(&file, &padded).unwrap();

    for source in ["flag", "variable", "both", "inline"] {
        let data = dir.path().join(source);
        let mut cmd = command(&data, "127.0.0.1:0");
        match source {
            "flag" => {
                cmd.arg("--seed-file").arg(&file);
            }
            "variable" => {
                cmd.env("IRONFLOWX_SEED_FILE", &file);
            }
            "both" => {
                cmd.arg("--seed-file")
                    .arg(&file)
                    .env("IRONFLOWX_SEED_FILE", "missing");
            }
            "inline" => {
                cmd.env("IRONFLOWX_SEED", &padded);
            }
            _ => unreachable!(),
        }
        let served = Served::from_command(cmd);
        let client = Client::new(&format!("http://{}", served.addr)).unwrap();
        let cx = Call::with_key(Secret::new(key.to_owned()));
        let me = client.whoami(&cx, WhoamiRequest {}).await.unwrap();
        assert_eq!(me.key_id.to_string(), seed["key_id"]);
        assert_eq!(me.scopes, [Scope::Read, Scope::Write, Scope::Admin].into());
        assert_eq!(leak(&served.stop(), [key]), None);
        assert!(!data.join("admin.key").exists());
    }
}

#[test]
fn directories_and_addresses_come_from_the_flag_then_the_variable() {
    let dir = private();
    for source in ["flag", "variable", "both"] {
        let data = dir.path().join(source);
        let mut cmd = binary();
        cmd.arg("serve").current_dir(dir.path());
        match source {
            "flag" => {
                cmd.args(["--listen", "127.0.0.1:0", "--data-dir"])
                    .arg(&data);
            }
            "variable" => {
                cmd.env("IRONFLOWX_LISTEN", "127.0.0.1:0")
                    .env("IRONFLOWX_DATA_DIR", &data);
            }
            "both" => {
                cmd.args(["--listen", "127.0.0.1:0", "--data-dir"])
                    .arg(&data);
                cmd.env("IRONFLOWX_LISTEN", "invalid")
                    .env("IRONFLOWX_DATA_DIR", "unused");
            }
            _ => unreachable!(),
        }
        let served = Served::from_command(cmd);
        assert!(data.join("control.db").is_file());
        served.stop();
    }
    assert!(!dir.path().join("unused").exists());
    assert!(!dir.path().join(".ironflowx").exists());
}

#[test]
fn log_settings_come_from_the_flag_then_the_variable() {
    let dir = private();
    let data = dir.path().join("broken");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(data.join("control.db"), "not a database").unwrap();
    for (flag, value, variable, other) in [
        ("--log-level", "error", "IRONFLOWX_LOG_LEVEL", "info"),
        ("--log-format", "text", "IRONFLOWX_LOG_FORMAT", "json"),
    ] {
        for source in ["flag", "variable", "both"] {
            let mut cmd = command(&data, "127.0.0.1:0");
            match source {
                "flag" => {
                    cmd.args([flag, value]);
                }
                "variable" => {
                    cmd.env(variable, value);
                }
                "both" => {
                    cmd.args([flag, value]).env(variable, other);
                }
                _ => unreachable!(),
            }
            let (code, log) = refusal(cmd.output().unwrap());
            assert_eq!(code, Some(8), "{log}");
            let lines: Vec<_> = log.lines().collect();
            if flag == "--log-level" {
                assert_eq!(lines.len(), 2, "{log}");
                let error: Value = serde_json::from_str(lines[0]).unwrap();
                assert_eq!(error["level"], "ERROR");
            } else {
                assert_eq!(lines.len(), 3, "{log}");
                assert!(
                    lines[0].contains(" INFO ") && lines[0].contains("listening"),
                    "{log}"
                );
                assert!(
                    lines[1].contains(" ERROR ") && lines[1].contains("not a database"),
                    "{log}"
                );
                assert!(
                    !lines[0].starts_with('{') && !lines[1].starts_with('{'),
                    "{log}"
                );
            }
        }
    }
}

#[test]
fn only_the_unknown_variable_name_is_logged_and_old_temporaries_are_kept() {
    let dir = private();
    let path = dir.path().join("admin.key.earlier.tmp");
    let protected = "the only copy of an earlier key";
    std::fs::write(&path, protected).unwrap();
    let mut cmd = command(dir.path(), "127.0.0.1:0");
    cmd.env("IRONFLOWX_LISEN", "a private value")
        .env("IRONFLOWX_URL", "a private value")
        .env("IRONFLOWX_KEY", "a private value");
    let log = Served::from_command(cmd).stop();
    assert_eq!(
        log.matches("IRONFLOWX_LISEN is not a setting").count(),
        1,
        "{log}"
    );
    assert!(!log.contains("IRONFLOWX_URL"), "{log}");
    assert!(!log.contains("IRONFLOWX_KEY"), "{log}");
    assert!(!log.contains("a private value"), "{log}");
    assert!(!log.contains(protected), "{log}");
    assert!(log.contains(&logged(&path)), "{log}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), protected);
}

#[test]
fn an_unassigned_numeric_bind_address_is_a_usage_error() {
    let dir = private();
    let data = dir.path().join("data");
    let (code, log) = refused(&data, "[2001:db8::1]:0");
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("cannot listen"), "{log}");
    assert!(!data.exists());
}

#[test]
fn the_host_preserves_a_control_plane_of_another_format() {
    let dir = private();
    let control = dir.path().join("control.db");
    drop(ControlPlane::open(&control).unwrap());
    let db = rusqlite::Connection::open(&control).unwrap();
    db.execute_batch("UPDATE meta SET format = 2").unwrap();
    drop(db);
    let before = std::fs::read(&control).unwrap();
    let (code, log) = refused(dir.path(), "127.0.0.1:0");
    assert_eq!(code, Some(1), "{log}");
    assert!(log.contains("another format version"), "{log}");
    assert_eq!(std::fs::read(&control).unwrap(), before);
    assert!(!dir.path().join("ledgers").exists());
}

#[test]
fn an_inconclusive_read_never_opens_beside_an_occupied_key_name() {
    for dangling in [false, true] {
        let dir = private();
        let control = dir.path().join("control.db");
        let db = rusqlite::Connection::open(&control).unwrap();
        db.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        let key = dir.path().join("admin.key");
        if dangling {
            std::os::unix::fs::symlink("missing", &key).unwrap();
        } else {
            std::fs::write(&key, "old bytes").unwrap();
        }
        let (code, log) = refused(dir.path(), "127.0.0.1:0");
        db.execute_batch("ROLLBACK").unwrap();
        drop(db);
        assert_eq!(code, Some(8), "{log}");
        assert!(log.contains("database is locked"), "{log}");
        assert!(!dir.path().join("engine.lock").exists());
        assert!(!ironflowx_operations::is_bootstrapped(dir.path()).unwrap());
        if dangling {
            assert_eq!(std::fs::read_link(&key).unwrap(), Path::new("missing"));
        } else {
            assert_eq!(std::fs::read(&key).unwrap(), b"old bytes");
        }
    }
}

#[test]
fn a_marked_directory_with_a_key_can_be_retried_after_a_read_error() {
    let dir = private();
    Served::start(dir.path(), "127.0.0.1:0").stop();
    let key = dir.path().join("admin.key");
    let saved = std::fs::read(&key).unwrap();
    let control = dir.path().join("control.db");
    let db = rusqlite::Connection::open(&control).unwrap();
    db.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    let (code, log) = refused(dir.path(), "127.0.0.1:0");
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);
    assert_eq!(code, Some(8), "{log}");
    assert!(log.contains("database is locked"), "{log}");
    let log = Served::start(dir.path(), "127.0.0.1:0").stop();
    assert!(!log.contains("the admin key is written"), "{log}");
    assert_eq!(std::fs::read(&key).unwrap(), saved);
}

#[test]
fn a_destination_lookup_error_is_not_permission_to_mint() {
    let dir = private();
    std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o600)).unwrap();
    let out = command(dir.path(), "127.0.0.1:0").output();
    std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o700)).unwrap();
    let (code, log) = refusal(out.unwrap());
    assert_eq!(code, Some(8), "{log}");
    assert!(
        log.contains("cannot inspect") && log.contains("admin.key"),
        "{log}"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_failed_probe_refuses_before_open_and_leaves_no_files() {
    let dir = private();
    std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o500)).unwrap();
    let out = command(dir.path(), "127.0.0.1:0").output();
    std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o700)).unwrap();
    let (code, log) = refusal(out.unwrap());
    assert_eq!(code, Some(1), "{log}");
    assert!(log.contains("cannot probe writes and hard links"), "{log}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_call_logs_only_its_operation_outcome_duration_and_presented_key_id() {
    let dir = private();
    let mut cmd = command(dir.path(), "127.0.0.1:0");
    cmd.args(["--log-level", "trace"]);
    let served = Served::from_command(cmd);
    let key = std::fs::read_to_string(dir.path().join("admin.key")).unwrap();
    let key = key.trim();
    let client = Client::new(&format!("http://{}", served.addr)).unwrap();
    let known = Call::with_key(Secret::new(key.to_owned()));
    let (other, _) = ironflowx_authz::mint(ID.parse().unwrap(), [5; 32]);
    let other = other.expose();
    let malformed = format!("{}!", &key[..83]);
    for (call, expected) in [
        (
            Call::with_key(Secret::new(other.to_owned())),
            Err(ErrorKind::Unauthenticated),
        ),
        (
            Call::with_key(Secret::new(malformed.clone())),
            Err(ErrorKind::Unauthenticated),
        ),
        (Call::anonymous(), Err(ErrorKind::Unauthenticated)),
        (known, Ok(())),
    ] {
        let answer = client.whoami(&call, WhoamiRequest {}).await;
        assert_eq!(answer.map(|_| ()).map_err(|e| e.kind), expected);
    }

    client
        .emit(
            &Call::with_key(Secret::new(key.to_owned())),
            EmitRequest {
                name: "log.test".into(),
                data: Some(br#""accepted_event_data_marker""#.to_vec()),
                metadata: Some(br#"{"meta":"accepted_event_metadata_marker"}"#.to_vec()),
                version: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();

    let rpc = ironflowx_proto::Operation::Emit.rpc();
    let rejected = "ironflowx.v1.IdentityService/unknown_operation_marker";
    let create = ironflowx_proto::Operation::CreateKey.rpc();
    for (operation, body, status) in [
        (rpc, "{\"event_data_marker\"", "HTTP/1.0 400 "),
        (
            create,
            r#"{"scopes":["unsupported_scope_marker"]}"#,
            "HTTP/1.0 400 ",
        ),
        (rejected, "{}", "HTTP/1.0 404 "),
    ] {
        let mut socket = TcpStream::connect(served.addr).unwrap();
        write!(
            socket,
            "POST /{operation} HTTP/1.0\r\nauthorization: Bearer {key}\r\n\
             content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        assert!(
            reply.starts_with(status),
            "{}",
            reply.lines().next().unwrap()
        );
    }
    let log = served.stop();
    assert_eq!(leak(&log, [key, other]), None);
    assert!(!log.contains(&malformed), "{log}");
    for marker in [
        "event_data_marker",
        "accepted_event_metadata_marker",
        "unsupported_scope_marker",
        "unknown_operation_marker",
    ] {
        assert!(!log.contains(marker), "{log}");
    }
    assert!(!log.contains("connectrpc"), "{log}");
    let lines: Vec<_> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["fields"]["message"] == "call")
        .collect();

    let whoami = ironflowx_proto::Operation::Whoami.rpc();
    let expected = [
        (whoami, "unauthenticated", Some(ID)),
        (whoami, "unauthenticated", None),
        (whoami, "unauthenticated", None),
        (whoami, "success", Some(&key[4..40])),
        (rpc, "success", Some(&key[4..40])),
        (rpc, "refused", None),
        (create, "invalid", None),
        ("unknown", "refused", None),
    ];
    assert_eq!(lines.len(), expected.len(), "{log}");
    for (line, (rpc, outcome, id)) in lines.into_iter().zip(expected) {
        let fields = &line["fields"];
        assert_eq!(fields["operation"], rpc);
        assert_eq!(fields["outcome"], outcome);
        assert_eq!(
            fields.get("key_id"),
            id.map(|id| Value::String(id.into())).as_ref()
        );
        assert!(fields["duration_ms"].as_f64().unwrap() >= 0.0);
    }
}

#[test]
fn a_credential_pasted_in_a_data_path_is_hidden_in_logs_and_envelopes() {
    let dir = private();
    let (key, _) = ironflowx_authz::mint(ID.parse().unwrap(), [11; 32]);
    let key = key.expose();
    for input in [key, &key[41..]] {
        let root = dir.path().join(input);
        std::fs::create_dir(&root).unwrap();
        let path = root.join("admin.key");
        std::fs::write(&path, b"another installation").unwrap();
        let (code, log) = refused(&root, "127.0.0.1:0");
        assert_eq!(code, Some(2));
        assert!(
            !log.contains(input),
            "a credential reached a startup refusal"
        );
        assert!(log.contains("<hidden>/admin.key"), "{log}");
        assert_eq!(std::fs::read(&path).unwrap(), b"another installation");

        std::fs::remove_file(path).unwrap();
        let log = Served::start(&root, "127.0.0.1:0").stop();
        assert!(!log.contains(input), "a credential reached a startup log");
        assert!(log.contains("<hidden>/admin.key"), "{log}");
    }
}

#[test]
fn a_control_plane_lookup_error_is_not_a_missing_database() {
    let dir = private();
    let control = dir.path().join("control.db");
    std::os::unix::fs::symlink("control.db", &control).unwrap();
    std::fs::write(dir.path().join("admin.key"), b"existing key file").unwrap();
    let (code, log) = refused(dir.path(), "127.0.0.1:0");
    assert_eq!(code, Some(8), "{log}");
    assert!(!log.contains("another install's"), "{log}");
    assert_eq!(
        std::fs::read_link(control).unwrap(),
        Path::new("control.db")
    );
    assert_eq!(
        std::fs::read(dir.path().join("admin.key")).unwrap(),
        b"existing key file"
    );
    assert!(!dir.path().join("engine.lock").exists());
}

#[test]
fn listening_uses_the_exact_address_selected_by_flags_and_variables() {
    let dir = private();
    std::fs::write(dir.path().join("control.db"), "not a database").unwrap();
    let flag_port = TcpListener::bind("127.0.0.1:0").unwrap();
    let var_port = TcpListener::bind("127.0.0.1:0").unwrap();
    let flag = flag_port.local_addr().unwrap().to_string();
    let variable = var_port.local_addr().unwrap().to_string();
    for (use_flag, use_var, expected) in [
        (true, false, &flag),
        (false, true, &variable),
        (true, true, &flag),
    ] {
        let mut cmd = binary();
        cmd.args(["serve", "--data-dir"]).arg(dir.path());
        if use_flag {
            cmd.args(["--listen", &flag]);
        }
        if use_var {
            cmd.env("IRONFLOWX_LISTEN", &variable);
        }

        let (code, log) = refusal(cmd.output().unwrap());
        assert_eq!(code, Some(8), "{log}");
        let envelope: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        let message = envelope["message"].as_str().unwrap();
        assert!(
            message.starts_with(&format!("cannot listen: {expected}: ")),
            "{message}"
        );
        assert!(!dir.path().join("engine.lock").exists());
    }
}

#[test]
fn old_temporary_warnings_use_unambiguous_locators_and_keep_the_files() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let parent = private();
    let dir_42 = "a".repeat(42);
    let dir_43 = "a".repeat(43);
    let dir_80 = "a".repeat(80);
    let credential = format!("{}A", "B".repeat(42));
    let supported = |name: &OsStr| !cfg!(target_os = "macos") || name.to_str().is_some();
    let cases = [
        (OsStr::new("safe"), false, true),
        (OsStr::new(&dir_42), false, true),
        (OsStr::new(&dir_43), false, false),
        (OsStr::new(&dir_80), false, false),
        (OsStr::new(&credential), false, false),
        (OsStr::new("relative"), true, false),
        (OsStr::new("newline\nroot"), false, false),
        (OsStr::from_bytes(b"non-utf8-\xff"), false, false),
    ];
    for (root_name, relative_flag, absolute) in cases.into_iter().filter(|row| supported(row.0)) {
        let root = parent.path().join(root_name);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("control.db"), b"not a database").unwrap();
        let safe = "admin.key.leftover.tmp";
        let names = [
            OsStr::new("admin.key.ixk_must_hide.tmp"),
            OsStr::new("admin.key.with\nnewline.tmp"),
            OsStr::new("admin.key.literal\\n.tmp"),
            OsStr::from_bytes(b"admin.key.invalid-\xff.tmp"),
        ];
        let concealed = || names.into_iter().filter(|name| supported(name));
        for name in std::iter::once(OsStr::new(safe)).chain(concealed()) {
            std::fs::write(root.join(name), b"protected contents").unwrap();
        }
        let configured = if relative_flag {
            Path::new(root_name)
        } else {
            &root
        };
        for format in ["json", "text"] {
            let mut cmd = command(configured, "127.0.0.1:0");
            cmd.current_dir(parent.path())
                .args(["--log-format", format]);
            let (code, log) = refusal(cmd.output().unwrap());
            assert_eq!(code, Some(8), "{log}");
            assert_eq!(
                log.matches("temporary admin key is kept").count(),
                1 + concealed().count(),
                "{log}"
            );
            assert_eq!(
                log.matches("inspect admin.key.*.tmp").count(),
                concealed().count(),
                "{log}"
            );
            assert!(!log.contains("protected contents"), "{log}");
            assert!(
                !log.contains("ixk_must_hide"),
                "printed a credential-shaped name"
            );
            assert!(
                !log.contains("newline.tmp") && !log.contains("invalid-"),
                "{log}"
            );
            let visible = log.lines().find(|line| line.contains(safe)).unwrap();
            if absolute {
                assert!(
                    visible.contains(root.join(safe).to_str().unwrap()),
                    "{visible}"
                );
                assert!(!visible.contains("path_base"), "{visible}");
            } else {
                assert!(visible.contains("configured data directory"), "{visible}");
                assert!(!visible.contains("/admin.key.leftover.tmp"), "{visible}");
            }
        }
        for name in std::iter::once(OsStr::new(safe)).chain(concealed()) {
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                b"protected contents"
            );
        }
    }
}

#[test]
fn a_seeded_stale_start_warns_once_about_each_matching_temporary() {
    let dir = private();
    let root = dir.path();
    std::fs::write(root.join("control.db"), b"").unwrap();
    let admin = root.join("admin.key");
    std::fs::write(&admin, b"old admin").unwrap();
    let matching = root.join("admin.key.previous.tmp");
    let other = root.join("admin.key.tmp");
    std::fs::write(&matching, b"previous key").unwrap();
    std::fs::write(&other, b"unrelated").unwrap();
    let (key, _) = ironflowx_authz::mint(ID.parse().unwrap(), [5; 32]);
    let mut cmd = command(root, "127.0.0.1:0");
    let (code, log) = refusal(cmd.env("IRONFLOWX_SEED", key.expose()).output().unwrap());
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("another install's"), "{log}");
    assert_eq!(
        log.matches("temporary admin key is kept").count(),
        1,
        "{log}"
    );
    assert!(log.contains(&logged(&matching)), "{log}");
    assert!(!log.contains(&logged(&other)), "{log}");
    assert!(!root.join("engine.lock").exists());
    for (file, contents) in [
        (admin, b"old admin".as_slice()),
        (matching, b"previous key".as_slice()),
        (other, b"unrelated".as_slice()),
    ] {
        assert_eq!(std::fs::read(file).unwrap(), contents);
    }
}

#[test]
fn a_short_probe_write_refuses_before_open_and_removes_the_source() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    use std::os::unix::process::CommandExt as _;

    const ROOT: &str = "IRONFLOWX_TEST_SHORT_PROBE_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        // Only this child has the limit. At 84 bytes the 85-byte probe write must fail.
        let max = getrlimit(Resource::Fsize).maximum;
        setrlimit(
            Resource::Fsize,
            Rlimit {
                current: Some(84),
                maximum: max,
            },
        )
        .unwrap();
        panic!(
            "cannot launch host: {}",
            command(Path::new(&root), "127.0.0.1:0").exec()
        );
    }

    let dir = private();
    // Ignore SIGXFSZ so the write reports EFBIG and the host can clean up.
    let out = Command::new("/bin/sh")
        .args(["-c", "trap '' XFSZ && exec \"$@\"", "short-probe"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_short_probe_write_refuses_before_open_and_removes_the_source",
        ])
        .env(ROOT, dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    let envelope: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(envelope["code"], "internal");
    let message = envelope["message"].as_str().unwrap();
    assert!(
        message.starts_with("cannot probe writes and hard links"),
        "{message}"
    );
    assert!(message.contains("File too large"), "{message}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
