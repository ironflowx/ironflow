//! The local recovery command's refusals and delivery.

use std::fs::Permissions;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use ironflowx_core::{KeyId, KeyOrigin, Secret};
use ironflowx_operations::{Engine, HostClock, key_id};
use ironflowx_proto::{Call, ListKeysRequest, Operations as _};
use serde_json::{Value, json};

fn recovery(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ironflowx"));
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"IRONFLOWX_") {
            cmd.env_remove(name);
        }
    }
    cmd.args(["recover-key", "--data-dir"]).arg(dir);
    cmd
}

fn refusal(out: Output, exit: i32) -> Value {
    assert_eq!(out.status.code(), Some(exit));
    assert!(out.stdout.is_empty(), "a refusal prints no key to stdout");
    serde_json::from_slice(&out.stderr).expect("one stderr envelope")
}

#[test]
fn recovery_does_not_create_a_missing_data_directory() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    for dir in [temp.path().join("missing"), file.join("child")] {
        for args in [&[][..], &["--stdout"][..]] {
            let error = refusal(recovery(&dir).args(args).output().unwrap(), 2);
            assert_eq!(
                error,
                json!({
                    "code": "invalid",
                    "message": "the data directory does not exist",
                    "retryable": false,
                })
            );
            assert!(!dir.exists());
        }
    }
}

#[test]
fn recovery_refuses_a_regular_file_as_the_data_directory() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("not-a-directory");
    std::fs::write(&file, b"keep me").unwrap();
    for args in [&[][..], &["--stdout"][..]] {
        assert_eq!(
            refusal(recovery(&file).args(args).output().unwrap(), 2),
            json!({
                "code": "invalid",
                "message": "the data path is not a directory",
                "retryable": false,
            })
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
    }
}

#[test]
fn recovery_refuses_an_unfinished_bootstrap() {
    let dir = tempfile::tempdir().unwrap();
    for args in [&[][..], &["--stdout"][..]] {
        assert_eq!(
            refusal(recovery(dir.path()).args(args).output().unwrap(), 2),
            json!({
                "code": "invalid",
                "message": "the bootstrap is not finished",
                "retryable": false,
            })
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn recovery_distinguishes_wrong_format_from_a_control_plane_fault() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), HostClock, None).await.unwrap();
    opened.engine.close().await;
    let control = dir.path().join("control.db");
    let db = rusqlite::Connection::open(&control).unwrap();
    db.execute_batch("UPDATE meta SET format = 1").unwrap();
    drop(db);
    assert_eq!(
        refusal(recovery(dir.path()).output().unwrap(), 1),
        json!({
            "code": "internal",
            "message": "control plane: another format version",
            "retryable": false,
        })
    );
    std::fs::write(&control, b"not a database").unwrap();
    let error = refusal(recovery(dir.path()).output().unwrap(), 8);
    assert_eq!(error["code"], "unavailable");
    assert_eq!(error["retryable"], true);
    assert_eq!(
        error["hint"],
        "list recovered keys before retrying and again after reopening the engine. Revoke any key whose secret was lost"
    );
    assert!(!dir.path().join("admin.key").exists());
}

#[tokio::test]
async fn a_failed_probe_leaves_no_recovered_key() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), HostClock, None).await.unwrap();
    let admin = opened.minted.unwrap();
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o500)).unwrap();
    let out = recovery(dir.path()).output();
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();

    let error = refusal(out.unwrap(), 1);
    assert_eq!(error["code"], "internal");
    assert_eq!(error["retryable"], false);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("cannot probe writes and hard links in ")
    );
    assert_eq!(keys(&opened.engine, admin).await.len(), 1, "only bootstrap");
    assert!(!dir.path().join("admin.key").exists());
    opened.engine.close().await;
}

#[test]
fn failed_probe_prints_only_the_safe_directory_locator() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    for (name, absolute) in [
        ("quoted\"root", true),
        ("back\\slash", true),
        ("new\nline", true),
        ("relative", false),
        ("ordinary", true),
    ] {
        let dir = temp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, Permissions::from_mode(0o500)).unwrap();
        let argument = if absolute { &dir } else { Path::new(name) };
        let out = recovery(argument).current_dir(temp.path()).output();
        std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();

        let error = refusal(out.unwrap(), 1);
        let locator = if name == "ordinary" {
            dir.to_str().unwrap()
        } else {
            "the configured data directory"
        };
        assert_eq!(
            error["message"],
            format!("cannot probe writes and hard links in {locator}: permission denied")
        );
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }
}

#[test]
fn a_short_probe_write_keeps_its_diagnostic_path_safe() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    use std::os::unix::process::CommandExt as _;

    const ROOT: &str = "IRONFLOWX_TEST_RECOVERY_SHORT_PROBE_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        setrlimit(
            Resource::Fsize,
            Rlimit {
                current: Some(84),
                maximum: getrlimit(Resource::Fsize).maximum,
            },
        )
        .unwrap();
        panic!(
            "cannot launch recovery: {}",
            recovery(Path::new(&root)).exec()
        );
    }

    let temp = tempfile::tempdir_in("/tmp").unwrap();
    for (name, printable) in [("quoted\"root", false), ("ordinary", true)] {
        let dir = temp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        // Ignore SIGXFSZ so the 85-byte write reports EFBIG after creating the temporary.
        let out = Command::new("/bin/sh")
            .args(["-c", "trap '' XFSZ && exec \"$@\"", "short-probe"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_short_probe_write_keeps_its_diagnostic_path_safe",
            ])
            .env(ROOT, &dir)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&out.stderr).expect("one stderr envelope");
        let locator = if printable {
            dir.to_str().unwrap()
        } else {
            "the configured data directory"
        };
        let cause = std::io::Error::from(rustix::io::Errno::FBIG);
        assert_eq!(
            error["message"],
            format!("cannot probe writes and hard links in {locator}: {cause}")
        );
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn stdout_recovery_works_offline_and_leaves_an_occupied_destination_alone() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), HostClock, None).await.unwrap();
    opened.engine.close().await;

    let file = dir.path().join("admin.key");
    let missing = dir.path().join("no-target");
    std::os::unix::fs::symlink(&missing, &file).unwrap();
    let refusal = refusal(recovery(dir.path()).output().unwrap(), 2);
    assert_eq!(refusal["code"], "invalid");
    assert_eq!(
        refusal["hint"],
        "use --stdout to recover without changing admin.key"
    );

    let out = recovery(dir.path())
        .env("IRONFLOWX_DATA_DIR", dir.path().join("ignored"))
        .arg("--stdout")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(std::fs::read_link(&file).unwrap(), missing);
    assert!(!missing.exists());
    let answer: Value = serde_json::from_slice(&out.stdout).expect("a JSON key object");
    let key = Secret::new(answer["key"].as_str().unwrap().to_owned());
    let id = key_id(&key).expect("a formatted key");
    assert_eq!(answer["key_id"], id.to_string());
    assert!(out.stderr.is_empty());

    let opened = Engine::open(dir.path(), HostClock, None).await.unwrap();
    let records = keys(&opened.engine, key).await;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], (id, KeyOrigin::Recovered));
    assert_eq!(records[1].1, KeyOrigin::Minted);
    opened.engine.close().await;
}

#[tokio::test]
async fn a_broken_stdout_reports_the_committed_key_id() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), HostClock, None).await.unwrap();
    let bootstrap = opened.minted.unwrap();
    let bootstrap_id = key_id(&bootstrap).unwrap();

    let (out, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let error = refusal(
        recovery(dir.path())
            .arg("--stdout")
            .stdout(Stdio::from(OwnedFd::from(out)))
            .output()
            .unwrap(),
        1,
    );
    let records = keys(&opened.engine, bootstrap).await;
    assert_eq!(records.len(), 2);
    let (id, origin) = records[0];
    assert_eq!(origin, KeyOrigin::Recovered);
    assert_eq!(records[1], (bootstrap_id, KeyOrigin::Minted));
    assert_eq!(error["code"], "internal");
    assert_eq!(error["retryable"], false);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("cannot deliver recovered key {id} to stdout:"))
    );
    assert_eq!(
        error["hint"],
        format!(
            "use a working admin key or recover one with ironflowx recover-key --stdout, then revoke {id}"
        )
    );
    assert!(!dir.path().join("admin.key").exists());
    opened.engine.close().await;
}

#[test]
fn an_uninspectable_data_directory_is_unavailable_in_both_modes() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let parent = temp.path().join("inaccessible");
    let dir = parent.join("engine");
    std::fs::create_dir_all(&dir).unwrap();
    for args in [&[][..], &["--stdout"][..]] {
        std::fs::set_permissions(&parent, Permissions::from_mode(0o600)).unwrap();
        let out = recovery(&dir).args(args).output();
        std::fs::set_permissions(&parent, Permissions::from_mode(0o700)).unwrap();
        let cause = std::io::Error::from(rustix::io::Errno::ACCESS);
        assert_eq!(
            refusal(out.unwrap(), 8),
            json!({
                "code": "unavailable",
                "message": format!("cannot inspect {}: {cause}", dir.display()),
                "retryable": true,
            })
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}

#[test]
fn an_uninspectable_destination_is_refused_without_echoing_the_directory_name() {
    let temp = tempfile::tempdir().unwrap();
    let (_, seed) = ironflowx_operations::generate_seed();
    let dir = temp.path().join(seed.expose());
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, Permissions::from_mode(0o600)).unwrap();
    let out = recovery(&dir).output();
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();

    let error = refusal(out.unwrap(), 8);
    assert_eq!(error["code"], "unavailable");
    assert_eq!(error["retryable"], true);
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with("cannot inspect admin.key in the configured data directory:"));
    assert!(!message.contains(&seed.expose()[41..]));
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
}

async fn keys(engine: &Engine, key: Secret) -> Vec<(KeyId, KeyOrigin)> {
    let cx = Call::with_key(key);
    let page = engine
        .list_keys(&cx, ListKeysRequest::default())
        .await
        .unwrap();
    assert_eq!(page.next, None);
    page.keys
        .iter()
        .map(|key| (key.key_id, key.origin))
        .collect()
}
