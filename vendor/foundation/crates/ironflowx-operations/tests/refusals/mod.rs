//! The five refusals of a bad key, as the test that includes this reaches an engine: `admission.rs`
//! in process, `ironflowx-bin`'s `remote.rs` through the proto client.

use std::sync::Arc;

use ironflowx_authz::mint;
use ironflowx_cell::ManualClock;
use ironflowx_core::{Clock, ErrorKind, Secret, Timestamp};
use ironflowx_operations::{Engine, Opened};
use ironflowx_proto::{
    Call, CreateKeyRequest, OpError, Operations, RevokeKeyRequest, WhoamiRequest,
};

pub fn clock() -> ManualClock {
    ManualClock::new(Timestamp::from_millis(1_800_000_000_000))
}

pub fn key(text: &str) -> Call {
    Call::with_key(Secret::new(text.to_owned()))
}

pub async fn refusal<O: Operations>(ops: &O, cx: &Call) -> OpError {
    let err = ops.whoami(cx, WhoamiRequest {}).await.expect_err("refused");
    assert_eq!(err.kind, ErrorKind::Unauthenticated, "{err:?}");
    assert!(!err.kind.retryable());
    err
}

/// The five kinds of bad key get one answer, byte for byte, and the same one when recording an
/// expiry fails; a wrong-secret or revoked key that has also expired is no different (TEN-7).
pub async fn five<O: Operations>(reach: impl AsyncFnOnce(&Arc<Engine>) -> Arc<O>) {
    const HOUR: i64 = 3_600_000;
    let (dir, other_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let clock = clock();
    let t = clock.now().as_millis();
    let opened = Engine::open(dir.path(), clock.clone(), None).await.unwrap();
    let Opened { engine, minted, .. } = opened;
    let engine = Arc::new(engine);
    let ops = reach(&engine).await;
    let other = Engine::open(other_dir.path(), clock.clone(), None)
        .await
        .unwrap();
    let admin = key(minted.as_ref().unwrap().expose());
    let mut made = Vec::new();
    for _ in 0..4 {
        let req = CreateKeyRequest {
            expires_at: Some(Timestamp::from_millis(t + HOUR)),
            ..CreateKeyRequest::default()
        };
        made.push(ops.create_key(&admin, req).await.unwrap());
    }
    let revoke = async |id: String| {
        let req = RevokeKeyRequest { key_id: id };
        ops.revoke_key(&admin, req).await.unwrap();
    };
    revoke(made[2].info.key_id.to_string()).await;
    let wrong = mint(made[3].info.key_id, [2; 32]).0;
    let unknown = mint(
        "019a9f3e-7c2b-7d41-8a3e-5f6b7c8d9e0f".parse().unwrap(),
        [1; 32],
    )
    .0;
    let recorded = || -> Vec<String> {
        let db = rusqlite::Connection::open(dir.path().join("control.db")).unwrap();
        let sql = "SELECT id FROM keys WHERE expiry_recorded";
        let mut stmt = db.prepare(sql).unwrap();
        let ids = stmt.query_map([], |r| r.get(0)).unwrap();
        ids.map(Result::unwrap).collect()
    };
    let expected = OpError {
        kind: ErrorKind::Unauthenticated,
        message: "invalid API key".into(),
        hint: None,
    };

    clock.set(Timestamp::from_millis(t + HOUR));
    let bad = [
        ("expired", made[0].key.expose()),
        ("unknown", unknown.expose()),
        ("wrong secret", wrong.expose()),
        ("revoked", made[2].key.expose()),
        ("foreign", other.minted.as_ref().unwrap().expose()),
    ];
    let db = rusqlite::Connection::open(dir.path().join("control.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail BEFORE UPDATE ON keys BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    for (case, text) in bad {
        let got = refusal(&*ops, &key(text)).await;
        assert_eq!(got, expected, "{case}, recording failing");
    }
    assert!(recorded().is_empty(), "the recording did fail");
    db.execute_batch("DROP TRIGGER fail").unwrap();
    // The expired key twice: newly expired, then recorded.
    for (case, text) in bad.into_iter().chain([bad[0]]) {
        assert_eq!(refusal(&*ops, &key(text)).await, expected, "{case}");
    }
    assert_eq!(recorded(), [made[0].info.key_id.to_string()]);
    drop(ops);
    // A door that still serves it drops it with the runtime.
    if let Some(engine) = Arc::into_inner(engine) {
        engine.close().await;
    }
    other.engine.close().await;
}
