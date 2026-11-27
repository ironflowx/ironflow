//! TEN-3 checks for the binary's process output and data directory.

#[allow(dead_code)]
#[path = "../../../../vendor/foundation/crates/ironflowx-operations/tests/leak/mod.rs"]
pub mod leak;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Output;

use connectrpc::ConnectError;
use ironflowx_core::Secret;
use ironflowx_operations::key_id;
use ironflowx_proto::wire::op_error;
use ironflowx_proto::{EmitBatchResponse, OpError};
use serde_json::Value;

#[derive(Clone, Copy)]
pub enum Delivery {
    Create,
    SeedOrRecovery,
}

impl Delivery {
    fn id(self) -> &'static str {
        match self {
            Self::Create => "/info/key_id",
            Self::SeedOrRecovery => "/key_id",
        }
    }
}

#[derive(Default)]
pub struct Audit {
    keys: Vec<String>,
    malformed: Vec<String>,
    file_key: Option<String>,
    published: Vec<String>,
}

impl Audit {
    pub fn remember(&mut self, key: &str) {
        assert!(
            key_id(&Secret::new(key.to_owned())).is_some(),
            "fixture key format"
        );
        if !self.keys.iter().any(|known| known == key) {
            self.keys.push(key.to_owned());
        }
    }

    pub fn presented(&mut self, key: &Secret) {
        if key_id(key).is_some() {
            self.remember(key.expose());
        } else if !key.expose().is_empty() {
            let literal = leak::latin1(key.expose().as_bytes());
            if !self.malformed.contains(&literal) {
                self.malformed.push(literal);
            }
        }
    }

    pub fn file_key(&mut self, key: &str) {
        self.remember(key);
        self.file_key = Some(key.to_owned());
        self.published.push(key.to_owned());
    }

    pub fn remove_file_key(&mut self) {
        self.file_key = None;
    }

    fn leaked(&self, bytes: &[u8]) -> Option<&'static str> {
        self.leaked_text(&leak::latin1(bytes))
    }

    fn malformed_in(&self, text: &str) -> bool {
        self.malformed.iter().any(|literal| text.contains(literal))
    }

    fn leaked_text(&self, text: &str) -> Option<&'static str> {
        if self.malformed_in(text) {
            return Some("a malformed credential");
        }
        leak::leak(text, self.keys.iter().map(String::as_str))
    }

    pub fn check(&self, bytes: &[u8], where_: &str) {
        assert_eq!(self.leaked(bytes), None, "TEN-3: {where_}");
    }

    pub fn diagnostics(&self, bytes: &[u8], where_: &str) {
        self.check(bytes, where_);
        for line in bytes.split(|byte| *byte == b'\n') {
            if let Ok(value) = serde_json::from_slice::<Value>(line) {
                self.check(value.to_string().as_bytes(), where_);
            }
        }
    }

    pub fn op_error(&self, error: &OpError) {
        self.check(error.message.as_bytes(), "remote error message");
        if let Some(hint) = &error.hint {
            self.check(hint.as_bytes(), "remote error hint");
        }
    }

    pub fn batch(&self, batch: &EmitBatchResponse) {
        for error in batch.items.iter().filter_map(|item| item.as_ref().err()) {
            self.op_error(error);
        }
    }

    pub fn connect_error(&self, error: &ConnectError) {
        if let Some(message) = &error.message {
            self.check(message.as_bytes(), "door refusal message");
        }
        let wire = serde_json::to_vec(error).expect("serialize the Connect refusal");
        self.diagnostics(&wire, "door refusal");
        if let Some(error) = op_error(error) {
            self.op_error(&error);
        }
    }

    /// The key is allowed in one JSON field and nowhere else in the response or diagnostics.
    pub fn output(&mut self, out: &Output, delivery: Option<Delivery>) {
        if out.status.success() {
            if let Some(delivery) = delivery {
                assert!(
                    self.delivered(&out.stdout, delivery).is_ok(),
                    "TEN-3: wrong stdout delivery"
                );
            } else {
                self.check(&out.stdout, "stdout");
                if let Ok(value) = serde_json::from_slice::<Value>(&out.stdout) {
                    self.check(value.to_string().as_bytes(), "decoded stdout JSON");
                }
            }
        } else {
            assert!(out.stdout.is_empty(), "TEN-3: stdout before a refusal");
        }
        self.diagnostics(&out.stderr, "stderr");
    }

    fn delivered(&mut self, stdout: &[u8], delivery: Delivery) -> Result<(), &'static str> {
        let mut answer: Value = serde_json::from_slice(stdout).map_err(|_| "expected JSON")?;
        let key = answer
            .as_object_mut()
            .and_then(|object| object.remove("key"))
            .and_then(|key| key.as_str().map(str::to_owned))
            .ok_or("expected a top-level key string")?;
        let id = key_id(&Secret::new(key.clone())).ok_or("expected a valid key")?;
        if answer.pointer(delivery.id()).and_then(Value::as_str) != Some(&id.to_string()) {
            return Err("key ID does not agree");
        }
        if self.keys.contains(&key) {
            return Err("delivered an existing key");
        }
        self.remember(&key);
        let raw = std::str::from_utf8(stdout).map_err(|_| "expected UTF-8")?;
        if raw.matches(&key).count() != 1 {
            return Err("expected exactly one literal credential");
        }
        let rest = raw.replacen(&key, "", 1);
        if self.leaked(rest.as_bytes()).is_some()
            || self.leaked(answer.to_string().as_bytes()).is_some()
        {
            return Err("credential outside its delivery field");
        }
        Ok(())
    }

    /// File delivery permits only the expected key and its newline, and never a key in a path.
    pub fn directory(&self, root: &Path) {
        if !root.exists() {
            return;
        }
        for (path, bytes) in leak::files(root) {
            let mode = std::fs::symlink_metadata(root.join(&path))
                .ok()
                .filter(|meta| meta.is_file())
                .map(|meta| meta.permissions().mode() & 0o7777);
            assert_eq!(
                self.entry_leak(&path, &bytes, mode),
                None,
                "TEN-3: data directory"
            );
        }
    }

    fn entry_leak(&self, path: &Path, bytes: &str, mode: Option<u32>) -> Option<&'static str> {
        if leak::leak_in(
            &(path.to_owned(), String::new()),
            self.keys.iter().map(String::as_str),
        )
        .is_some()
            || self.malformed_in(&leak::latin1(path.as_os_str().as_encoded_bytes()))
        {
            return Some("credential in path");
        }
        let name = path.to_str().unwrap_or_default();
        let temporary = name
            .strip_prefix("admin.key.")
            .and_then(|name| name.strip_suffix(".tmp"))
            .is_some_and(|name| {
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric())
            });
        let content = bytes.strip_suffix('\n');
        let expected =
            name == "admin.key" && self.file_key.is_some() && content == self.file_key.as_deref()
                || temporary && self.published.iter().any(|key| content == Some(key));
        if expected && mode == Some(0o400) {
            return None;
        }
        self.leaked_text(bytes).map(|_| "credential in file")
    }
}

/// Synthetic output that must be rejected without displaying its contents.
pub fn fixtures() {
    let (id, key) = ironflowx_operations::generate_seed();
    let key = key.expose();
    let (other_id, other) = ironflowx_operations::generate_seed();
    let other = other.expose();
    let delivery = Delivery::SeedOrRecovery;
    let check = |answer: Value| {
        let mut audit = Audit::default();
        audit.remember(other);
        audit.delivered(answer.to_string().as_bytes(), delivery)
    };
    assert!(check(serde_json::json!({"key": key, "key_id": id.to_string()})).is_ok());
    assert_eq!(
        check(serde_json::json!({"key": key, "key_id": other_id.to_string()})),
        Err("key ID does not agree")
    );
    assert_eq!(
        check(serde_json::json!({"wrapped": {"key": key}, "key_id": id.to_string()})),
        Err("expected a top-level key string")
    );
    assert_eq!(
        check(serde_json::json!({"key": key, "key_id": id.to_string(), "copy": key})),
        Err("expected exactly one literal credential")
    );
    for copy in [&key[41..], &other[41..], other] {
        assert_eq!(
            check(serde_json::json!({"key": key, "key_id": id.to_string(), "copy": copy})),
            Err("credential outside its delivery field")
        );
    }

    let mut audit = Audit::default();
    audit.file_key(key);
    audit.remember(other);
    let expected = format!("{key}\n");
    let file = |path: &str, bytes: &str| audit.entry_leak(Path::new(path), bytes, Some(0o400));
    assert_eq!(file("admin.key", &expected), None);
    assert_eq!(file("admin.key.leftover.tmp", &expected), None);
    for path in ["admin.key", "admin.key.leftover.tmp"] {
        assert_eq!(
            audit.entry_leak(Path::new(path), &expected, Some(0o644)),
            Some("credential in file")
        );
    }
    assert_eq!(file("control.db", &expected), Some("credential in file"));
    assert_eq!(
        file("admin.key.leftover.tmp", &other[41..]),
        Some("credential in file")
    );
    assert_eq!(
        file(&format!("admin.key.{}.tmp", &key[41..]), &expected),
        Some("credential in path")
    );
    assert_eq!(
        file("admin.key.leftover.tmp", &format!("{other}\n")),
        Some("credential in file")
    );
    audit.file_key(other);
    assert_eq!(
        audit.entry_leak(Path::new("admin.key.leftover.tmp"), &expected, Some(0o400)),
        None
    );
    assert_eq!(
        audit.entry_leak(Path::new("admin.key"), &expected, Some(0o400)),
        Some("credential in file")
    );
    assert_eq!(
        Audit::default().entry_leak(Path::new("admin.key"), &expected, Some(0o400)),
        Some("credential in file")
    );

    // The secret is 32 copies of 0xe1. The path holds the first half as characters.
    let encoded = format!("{}4eE", "4eHh".repeat(10));
    audit.remember(&format!("ixk_{id}_{encoded}"));
    let displayed = "\u{e1}".repeat(16);
    assert_eq!(
        audit.entry_leak(Path::new(&displayed), "", None),
        Some("credential in path")
    );

    let malformed = "fixture-malformed-credential";
    audit.presented(&Secret::new(malformed.to_owned()));
    let log = serde_json::json!({"fields": {"message": malformed}});
    assert_eq!(
        audit.leaked(log.to_string().as_bytes()),
        Some("a malformed credential")
    );
    assert_eq!(
        audit.entry_leak(Path::new("control.db"), malformed, Some(0o600)),
        Some("credential in file")
    );
    assert_eq!(
        audit.entry_leak(Path::new(malformed), "", None),
        Some("credential in path")
    );
    audit.presented(&Secret::new(String::new()));
    assert_eq!(audit.leaked(b"clean diagnostic"), None);
}
