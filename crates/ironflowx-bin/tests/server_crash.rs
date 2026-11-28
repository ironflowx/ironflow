//! The real host, on its own clock, killed by a parent that checks the reopened data over RPC.

#[path = "../../../vendor/foundation/crates/ironflowx-operations/tests/leak/mod.rs"]
#[allow(dead_code)]
mod leak;
#[path = "server_crash/process.rs"]
mod process;
#[path = "server_crash/workload.rs"]
mod workload;

use std::fs;
use std::path::Path;

use ironflowx_core::{ErrorKind, FaultPoint, KeyId, KeyOrigin, Scope, Secret};
use ironflowx_operations::key_id;
use ironflowx_proto::{
    Call, Client, KeyInfo, KeyStatus, ListKeysRequest, OpError, Operations as _, RevokeKeyRequest,
    WhoamiRequest,
};
use process::{Process, Told};

fn key_file_present(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("cannot inspect admin.key: {:?}", error.kind()),
    }
}

fn file_key(path: &Path) -> Secret {
    let mut text = fs::read_to_string(path).expect("the whole admin.key is readable");
    assert!(text.pop() == Some('\n'), "admin.key ends in a newline");
    let key = Secret::new(text);
    assert!(key_id(&key).is_some(), "admin.key is one valid key");
    key
}

fn call(key: &Secret) -> Call {
    Call::with_key(Secret::new(key.expose().to_owned()))
}

fn expect_answer<T>(answer: Result<T, OpError>, message: &str) -> T {
    answer.unwrap_or_else(|error| panic!("{message}: {:?}", error.kind))
}

async fn admin(client: &Client, key: &Secret, id: KeyId) {
    let who = client.whoami(&call(key), WhoamiRequest {}).await;
    let who = expect_answer(who, "the expected key authenticates");
    assert_eq!(who.key_id, Some(id));
    assert_eq!(
        who.scopes,
        [Scope::Admin, Scope::Read, Scope::Write, Scope::Execute].into()
    );
}

async fn refused(client: &Client, key: &Secret) {
    let answer = client.whoami(&call(key), WhoamiRequest {}).await;
    assert_eq!(
        answer.err().map(|error| error.kind),
        Some(ErrorKind::Unauthenticated),
        "the undisclosed key is refused"
    );
}

async fn keys(client: &Client, key: &Secret) -> Vec<KeyInfo> {
    let page = client
        .list_keys(&call(key), ListKeysRequest::default())
        .await;
    let page = expect_answer(page, "keys list");
    assert!(page.next.is_none(), "all keys fit");
    page.keys
}

async fn revoke(client: &Client, admin_key: &Secret, orphan: &Told) {
    let answer = client
        .revoke_key(
            &call(admin_key),
            RevokeKeyRequest {
                key_id: orphan.id.to_string(),
            },
        )
        .await;
    expect_answer(answer, "the orphan key can be revoked");
    refused(client, &orphan.key).await;
}

#[tokio::test]
async fn bootstrap_publication() {
    for point in [
        FaultPoint::BeforeBootstrapCommit,
        FaultPoint::AfterBootstrapCommit,
        FaultPoint::BeforeAdminKeyLink,
        FaultPoint::AfterAdminKeyLink,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let path = root.join("admin.key");
        let mut host = Process::serve(&root, None, Some((point, 1)));
        host.url();
        host.at(point);
        let dead = host.kill();
        let [original] = dead.keys.as_slice() else {
            panic!("exactly one minted key was told at {}", point.name());
        };
        assert_eq!(
            key_file_present(&path),
            point == FaultPoint::AfterAdminKeyLink,
            "admin.key presence when killed at {}",
            point.name()
        );

        let mut restart = Process::serve(&root, None, None);
        let client = restart.ready().await;
        match point {
            FaultPoint::BeforeBootstrapCommit | FaultPoint::AfterBootstrapCommit => {
                let key = file_key(&path);
                let id = key_id(&key).unwrap();
                assert_ne!(id, original.id, "the restart minted a different key");
                admin(&client, &key, id).await;
                refused(&client, &original.key).await;
                let mut expected = vec![(id, KeyOrigin::Minted, KeyStatus::Active)];
                if point == FaultPoint::AfterBootstrapCommit {
                    expected.push((original.id, KeyOrigin::Minted, KeyStatus::Revoked));
                }
                check_keys(&client, &key, &expected).await;
                let logs = restart.stop();
                let [replacement] = logs.keys.as_slice() else {
                    panic!("the restart told exactly one key");
                };
                assert_file_key(&key, replacement);
                assert_eq!(
                    logs.lost_secret(),
                    point == FaultPoint::AfterBootstrapCommit
                );
                logs.check_secrets([&original.key, &key]);
            }
            FaultPoint::BeforeAdminKeyLink => {
                assert!(
                    !key_file_present(&path),
                    "later open still has no admin.key"
                );
                let recovered = Process::recover(&root, None).success();
                let key = file_key(&path);
                let [replacement] = recovered.keys.as_slice() else {
                    panic!("recovery told exactly one key");
                };
                assert_file_key(&key, replacement);
                admin(&client, &key, replacement.id).await;
                check_keys(
                    &client,
                    &key,
                    &[
                        (original.id, KeyOrigin::Minted, KeyStatus::Active),
                        (replacement.id, KeyOrigin::Recovered, KeyStatus::Active),
                    ],
                )
                .await;
                admin(&client, &original.key, original.id).await;
                revoke(&client, &key, original).await;
                check_keys(
                    &client,
                    &key,
                    &[
                        (original.id, KeyOrigin::Minted, KeyStatus::Revoked),
                        (replacement.id, KeyOrigin::Recovered, KeyStatus::Active),
                    ],
                )
                .await;
                let logs = restart.stop();
                assert!(
                    !logs.lost_secret(),
                    "later open does not report a lost mint"
                );
                assert!(logs.keys.is_empty(), "later open minted nothing");
                logs.check_secrets([&original.key, &key]);
            }
            FaultPoint::AfterAdminKeyLink => {
                let key = file_key(&path);
                assert_file_key(&key, original);
                admin(&client, &key, original.id).await;
                check_keys(
                    &client,
                    &key,
                    &[(original.id, KeyOrigin::Minted, KeyStatus::Active)],
                )
                .await;
                let logs = restart.stop();
                assert!(
                    !logs.lost_secret(),
                    "later open does not report a lost mint"
                );
                assert!(logs.keys.is_empty(), "later open minted nothing");
                logs.check_secrets([&original.key, &key]);
            }
            _ => unreachable!(),
        }
    }
}

async fn check_keys(client: &Client, key: &Secret, expected: &[(KeyId, KeyOrigin, KeyStatus)]) {
    let listed = keys(client, key).await;
    assert_eq!(listed.len(), expected.len(), "exact key count");
    for &(id, origin, status) in expected {
        let row = listed
            .iter()
            .find(|row| row.key_id == id)
            .expect("expected key is listed");
        assert_eq!(
            (row.origin, row.status),
            (origin, status),
            "listed key {id}"
        );
    }
}

#[tokio::test]
async fn recovery_publication() {
    for point in [
        FaultPoint::BeforeRecoveredCommit,
        FaultPoint::AfterRecoveredCommit,
        FaultPoint::AfterAdminKeyLink,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let path = root.join("admin.key");
        let seed_file = dir.path().join("seed");
        let (seed_id, seed) = ironflowx_operations::generate_seed();
        process::write_seed(&seed_file, &seed);
        let mut host = Process::serve(&root, Some(&seed_file), None);
        let client = host.ready().await;
        admin(&client, &seed, seed_id).await;
        host.stop().check_secrets([&seed]);
        assert!(
            !key_file_present(&path),
            "seeded startup creates no admin.key"
        );

        let recovery = Process::recover(&root, Some(point));
        recovery.at(point);
        let dead = recovery.kill();
        dead.check_secrets([&seed]);
        let [original] = dead.keys.as_slice() else {
            panic!("exactly one recovered key was told at {}", point.name());
        };
        assert_eq!(
            key_file_present(&path),
            point == FaultPoint::AfterAdminKeyLink,
            "recovered admin.key presence at {}",
            point.name()
        );

        let mut restart = Process::serve(&root, Some(&seed_file), None);
        let client = restart.ready().await;
        admin(&client, &seed, seed_id).await;
        let mut expected = vec![(seed_id, KeyOrigin::Seed, KeyStatus::Active)];
        if point != FaultPoint::BeforeRecoveredCommit {
            expected.push((original.id, KeyOrigin::Recovered, KeyStatus::Active));
        }
        check_keys(&client, &seed, &expected).await;
        match point {
            FaultPoint::BeforeRecoveredCommit => refused(&client, &original.key).await,
            FaultPoint::AfterRecoveredCommit => {
                assert!(
                    !key_file_present(&path),
                    "committed recovery still has no admin.key"
                );
                admin(&client, &original.key, original.id).await;
                revoke(&client, &seed, original).await;
                let retry = Process::recover(&root, None).success();
                let [replacement] = retry.keys.as_slice() else {
                    panic!("the next recovery told exactly one key");
                };
                let key = file_key(&path);
                assert_file_key(&key, replacement);
                admin(&client, &key, replacement.id).await;
                expected[1].2 = KeyStatus::Revoked;
                expected.push((replacement.id, KeyOrigin::Recovered, KeyStatus::Active));
                check_keys(&client, &key, &expected).await;
                retry.check_secrets([&seed, &original.key, &key]);
                restart.stop().check_secrets([&seed, &original.key, &key]);
                continue;
            }
            FaultPoint::AfterAdminKeyLink => {
                let key = file_key(&path);
                assert_file_key(&key, original);
                admin(&client, &key, original.id).await;
            }
            _ => unreachable!(),
        }
        restart.stop().check_secrets([&seed, &original.key]);
    }
}

fn assert_file_key(key: &Secret, expected: &Told) {
    assert!(
        key.expose() == expected.key.expose(),
        "admin.key holds the exact key"
    );
}
