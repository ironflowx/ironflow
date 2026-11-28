//! The CLI, as a process, calling the HTTP door of a recording `Operations`.

#[path = "../../../vendor/foundation/crates/ironflowx-doors/tests/common/mod.rs"]
#[allow(dead_code)]
mod common;

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroU32;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use common::{CREATED, KEY, Recording, event, event_page, serving, whoami};
use ironflowx_core::{Address, ErrorKind, Scope, Timestamp};
use ironflowx_proto::{
    CreateKeyRequest, EmitBatchRequest, EmitRequest, Event, EventPage, GetEventRequest,
    ListEventsRequest, ListKeysRequest, OpError, RevokeKeyRequest, Whoami,
};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use serde_json::{Value, json};

/// The CLI, with no variable of ours set.
fn command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ironflowx"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("IRONFLOWX_") {
            command.env_remove(name);
        }
    }
    command.args(args).stdin(Stdio::null());
    command
}

fn stdout(out: &Output) -> &str {
    std::str::from_utf8(&out.stdout).unwrap()
}

/// The envelope of a failure: the one line of stderr, with nothing on stdout, and its exit code.
fn envelope(out: &Output) -> (Value, i32) {
    let stderr = std::str::from_utf8(&out.stderr).unwrap();
    assert_eq!(stdout(out), "", "{stderr}");
    let line = stderr.strip_suffix('\n').expect("a line");
    assert!(!line.contains('\n'), "{stderr}");
    let envelope = serde_json::from_str(line).expect("one JSON value");
    (envelope, out.status.code().expect("it exits"))
}

fn invalid(message: &str) -> (Value, i32) {
    let envelope = json!({"code": "invalid", "message": message, "retryable": false});
    (envelope, 2)
}

/// As `invalid`, of a command line that could not be read.
fn unread(message: &str) -> (Value, i32) {
    let (mut envelope, code) = invalid(message);
    envelope["hint"] = json!("--help says what a command takes");
    (envelope, code)
}

/// A server that answers `answer`, its URL, and the runtime it lives on.
struct Served {
    url: String,
    operations: Arc<Recording>,
    _runtime: tokio::runtime::Runtime,
}

impl Served {
    fn answering(answer: Result<Whoami, OpError>) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (addr, operations): (SocketAddr, _) = runtime.block_on(serving(answer));
        Self {
            url: format!("http://{addr}"),
            operations,
            _runtime: runtime,
        }
    }

    fn answers(&self, answer: Result<Whoami, OpError>) {
        *self.operations.answer.lock().unwrap() = answer;
    }

    /// What it answers of events: this one, and a page of it.
    fn holds(&self, event: Event, next: Option<&str>) {
        *self.operations.event.lock().unwrap() = Some(event.clone());
        *self.operations.page.lock().unwrap() = Some(EventPage {
            events: vec![event],
            next: next.map(str::to_owned),
        });
    }

    fn whoami(&self) -> Command {
        let mut command = command(&["whoami"]);
        command.env("IRONFLOWX_URL", &self.url);
        command
    }

    /// Runs `command`, which must succeed: the key it sent.
    fn key_sent(&self, command: &mut Command) -> Option<String> {
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let mut keys = self.operations.take();
        assert_eq!(keys.len(), 1);
        keys.remove(0)
    }
}

/// D65: no field an agent must guess at, and one line.
#[test]
fn every_field_that_is_not_optional_is_printed() {
    let served = Served::answering(Ok(whoami()));
    let out = served.whoami().output().unwrap();
    assert_eq!(
        stdout(&out),
        concat!(
            r#"{"environment":"default/main","expires_at":"2027-01-15T08:00:00.123Z","#,
            r#""key_id":"0190b5f0-0000-7000-8000-000000000001","scopes":["read","admin"]}"#,
            "\n"
        )
    );
    assert_eq!((out.status.code(), out.stderr), (Some(0), vec![]));
    served.answers(Ok(Whoami {
        scopes: BTreeSet::new(),
        expires_at: None,
        ..whoami()
    }));
    let out = served.whoami().output().unwrap();
    assert_eq!(
        stdout(&out),
        concat!(
            r#"{"environment":"default/main","#,
            r#""key_id":"0190b5f0-0000-7000-8000-000000000001","scopes":[]}"#,
            "\n"
        )
    );
    assert_eq!((out.status.code(), out.stderr), (Some(0), vec![]));
}

/// Each generated command: the answer on stdout, the new key of `keys create` there alone (TEN-3),
/// and the request no flag gives.
#[test]
fn each_command_prints_its_operations_answer() {
    let served = Served::answering(Ok(whoami()));
    let info = concat!(
        r#"{"created_at":"2023-11-14T22:13:20.001Z","#,
        r#""created_by":"0190b5f0-0000-7000-8000-000000000001","#,
        r#""expires_at":"2027-01-15T08:00:00.120Z","#,
        r#""key_id":"0190b5f0-0000-7000-8000-000000000002","label":"agent é","#,
        r#""origin":"created","scopes":["read","write"],"status":"active"}"#,
    );
    let bootstrap = concat!(
        r#"{"created_at":"1970-01-01T00:00:00Z","#,
        r#""key_id":"0190b5f0-0000-7000-8000-000000000001","#,
        r#""origin":"minted","scopes":["admin"],"status":"revoked"}"#,
    );
    let environment = concat!(
        r#"{"address":"default/main","created_at":"2023-11-14T22:13:20Z","#,
        r#""env":"0190b5f0-0000-7000-8000-000000000003.00112233445566778899aabbccddeeff","#,
        r#""name":"main","project_id":"0190b5f0-0000-7000-8000-000000000004"}"#,
    );
    let event = concat!(
        r#"{"data":{"b":1.50,"a":[9007199254740993,"\u00e9é\\\"",1e2]},"#,
        r#""emitted_by":"key:0190b5f0-0000-7000-8000-000000000001","#,
        r#""event_id":"0190b5f0-0000-7000-8000-000000000005","idempotency_key":"clé\u0001","#,
        "\"metadata\":{\"z\":\"\u{7f}\u{2028}😀\\ud83d\\ude00\\/\",\"a\":-0},",
        r#""name":"commande.passée","time":"2023-11-14T22:13:20.120Z","#,
        r#""version":"9223372036854775807"}"#,
    );
    let plain = concat!(
        r#"{"data":null,"emitted_by":"embedder:a b","#,
        r#""event_id":"0190b5f0-0000-7000-8000-000000000005","#,
        r#""name":"commande.passée","time":"2023-11-14T22:13:20.120Z","version":"1"}"#,
    );
    for (args, request, answer) in [
        (
            ["events", "emit"],
            r#"EmitRequest { name: "", data: None, metadata: None, version: None, idempotency_key: None }"#,
            r#"{"duplicate":false,"event_id":"0190b5f0-0000-7000-8000-000000000005"}"#.to_owned(),
        ),
        (
            ["events", "emit-batch"],
            "EmitBatchRequest { items: [] }",
            r#"{"items":[]}"#.to_owned(),
        ),
        (
            ["events", "get"],
            r#"GetEventRequest { event_id: "" }"#,
            event.to_owned(),
        ),
        (
            ["events", "list"],
            "ListEventsRequest { limit: None, cursor: None }",
            format!(r#"{{"events":[{event},{plain}],"next":"0123.7"}}"#),
        ),
        (
            ["environments", "list"],
            "ListEnvironmentsRequest",
            format!(r#"{{"environments":[{environment}]}}"#),
        ),
        (
            ["keys", "create"],
            "CreateKeyRequest { scopes: {}, expires_at: None, label: None }",
            format!(r#"{{"info":{info},"key":"{CREATED}"}}"#),
        ),
        (
            ["keys", "list"],
            "ListKeysRequest { limit: None, cursor: None }",
            format!(r#"{{"keys":[{info},{bootstrap}],"next":"0123.7"}}"#),
        ),
        (
            ["keys", "revoke"],
            r#"RevokeKeyRequest { key_id: "" }"#,
            "{}".to_owned(),
        ),
    ] {
        let mut command = command(&args);
        command.env("IRONFLOWX_URL", &served.url);
        let out = command.env("IRONFLOWX_KEY", KEY).output().unwrap();
        assert_eq!(stdout(&out), format!("{answer}\n"));
        assert_eq!((out.status.code(), out.stderr), (Some(0), vec![]));
        assert_eq!(served.operations.take_requests(), [request]);
        assert_eq!(served.operations.take(), [Some(KEY.to_owned())]);
    }
}

/// A request, from its flags and from its JSON alike, and where that is read from.
#[test]
fn a_request_is_what_its_flags_or_its_json_give() {
    let served = Served::answering(Ok(whoami()));
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, text: &str| {
        std::fs::write(dir.path().join(name), text).unwrap();
        format!("@{name}")
    };
    let (data, metadata) = ("\n{ \"id\" : 9007199254740993 }\r\n", "{ }\n");
    let (data_file, metadata_file) = (file("data", data), file("meta data", metadata));
    let emit = |name: &str, data: Option<&str>| EmitRequest {
        name: name.into(),
        data: data.map(Into::into),
        metadata: None,
        version: None,
        idempotency_key: None,
    };
    let whole = EmitRequest {
        metadata: Some(metadata.into()),
        version: Some(3),
        idempotency_key: Some("k".into()),
        ..emit("a.b", Some(data))
    };
    let whole_json = json!({
        "name": "a.b",
        "data": data,
        "metadata": metadata,
        "version": "3",
        "idempotency_key": "k",
    });
    // As a person would keep it.
    let whole_json = format!("\n{whole_json:#}\n");
    let whole_file = file("whole", &whole_json);
    let batch = EmitBatchRequest {
        items: vec![emit("a.one", Some("1")), emit("a.two", None)],
    };
    let batch_json = r#"{"items":[{"name":"a.one","data":"1"},{"name":"a.two"}]}"#;
    let created = CreateKeyRequest {
        scopes: [Scope::Read, Scope::Write].into(),
        expires_at: Some(Timestamp::from_millis(4_070_908_800_120)),
        label: Some("agent é".into()),
    };
    let created_json = r#"{"scopes":["write","read"],"expires_at":"2099-01-01T01:00:00.120+01:00","label":"agent é"}"#;
    let listed = ListEventsRequest {
        limit: NonZeroU32::new(4_294_967_295),
        cursor: Some("0123.7".into()),
    };
    let keys = ListKeysRequest {
        limit: NonZeroU32::new(7),
        cursor: Some(String::new()),
    };
    let id = "0190b5f0-0000-7000-8000-000000000005";
    let got = GetEventRequest {
        event_id: id.into(),
    };
    let revoked = RevokeKeyRequest { key_id: id.into() };
    for (args, stdin, request) in [
        (
            &[
                "events",
                "emit",
                "--name",
                "a.b",
                "--data",
                data,
                "--metadata",
                metadata,
            ][..],
            "",
            format!(
                "{:?}",
                EmitRequest {
                    version: None,
                    idempotency_key: None,
                    ..whole.clone()
                }
            ),
        ),
        (
            &[
                "events",
                "emit",
                "--idempotency-key",
                "k",
                "--version=3",
                "--metadata",
                &metadata_file,
                "--data",
                &data_file,
                "--name",
                "a.b",
            ],
            "",
            format!("{whole:?}"),
        ),
        (
            &["events", "emit", "--json", &whole_json],
            "",
            format!("{whole:?}"),
        ),
        (
            &["events", "emit", "--json", &whole_file],
            "",
            format!("{whole:?}"),
        ),
        (
            &["events", "emit", "--json", "-"],
            &whole_json,
            format!("{whole:?}"),
        ),
        // Text as it is, though a file or a flag has its name, and a number of either sign.
        (
            &["events", "emit", "--name", "@data"],
            "",
            format!("{:?}", emit("@data", None)),
        ),
        (
            &["events", "emit", "--name", "", "--data", "-1e-5"],
            "",
            format!("{:?}", emit("", Some("-1e-5"))),
        ),
        (
            &["events", "emit", "--data", "-"],
            "1",
            format!("{:?}", emit("", Some("-"))),
        ),
        (
            &["events", "emit", "--data", "--name"],
            "",
            format!("{:?}", emit("", Some("--name"))),
        ),
        // `null` is an absent field, as on the wire.
        (
            &[
                "events",
                "emit",
                "--json",
                r#"{"name":null,"version":null}"#,
            ],
            "",
            format!("{:?}", emit("", None)),
        ),
        (
            &["events", "emit-batch", "--json", batch_json],
            "",
            format!("{batch:?}"),
        ),
        (
            &["events", "emit-batch", "--json", "-"],
            batch_json,
            format!("{batch:?}"),
        ),
        (&["events", "get", "--event-id", id], "", format!("{got:?}")),
        (
            &[
                "events",
                "list",
                "--limit",
                "4294967295",
                "--cursor",
                "0123.7",
            ],
            "",
            format!("{listed:?}"),
        ),
        (
            &[
                "events",
                "list",
                "--json",
                r#"{"limit":4294967295,"cursor":"0123.7"}"#,
            ],
            "",
            format!("{listed:?}"),
        ),
        (
            &[
                "keys",
                "create",
                "--scopes",
                "write",
                "--label",
                "agent é",
                "--expires-at",
                "2099-01-01T00:00:00.12Z",
                "--scopes",
                "read",
            ],
            "",
            format!("{created:?}"),
        ),
        (
            &["keys", "create", "--json", created_json],
            "",
            format!("{created:?}"),
        ),
        (
            &["keys", "list", "--limit", "7", "--cursor", ""],
            "",
            format!("{keys:?}"),
        ),
        (
            &["keys", "revoke", "--key-id", id],
            "",
            format!("{revoked:?}"),
        ),
        (
            &["environments", "list", "--json", "{}"],
            "",
            "ListEnvironmentsRequest".into(),
        ),
        (&["whoami", "--json", " {}\n"], "", "WhoamiRequest".into()),
    ] {
        let mut command = command(args);
        command
            .env("IRONFLOWX_URL", &served.url)
            .env("IRONFLOWX_KEY", KEY)
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        // One that reads none of it can end first.
        let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
        let out = child.wait_with_output().unwrap();
        let taken = served.operations.take_requests();
        // A recording batch answers more results than these items, which the client refuses.
        if args[1] == "emit-batch" {
            let refused = json!({
                "code": "unknown",
                "hint": "retry unchanged only if every item had an idempotency key and keeps it; \
                         else check first what landed",
                "message": "the engine did not answer: unknown",
                "retryable": false,
            });
            assert_eq!(envelope(&out), (refused, 6), "{args:?}");
        } else {
            assert_eq!(
                (out.status.code(), &*out.stderr),
                (Some(0), &b""[..]),
                "{args:?}"
            );
        }
        assert_eq!(taken, [request], "{args:?}");
    }
}

/// What gives no request is a usage error, and nothing is sent.
#[test]
fn what_is_no_request_is_a_usage_error() {
    let served = Served::answering(Ok(whoami()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("latin"), b"\"\xe9\"").unwrap();
    let secret = &KEY[41..];
    let field = format!(r#"{{"{secret}":1}}"#);
    let version = format!(r#"{{"version":"{KEY}"}}"#);
    let emit = |rest: &[&'static str]| [&["events", "emit"], rest].concat();
    let json = |text: &'static str| emit(&["--json", text]);
    let hinted = |message: &str, hint: &str| {
        let (mut envelope, code) = invalid(message);
        envelope["hint"] = json!(hint);
        (envelope, code)
    };
    let text = "data is JSON text: pass a string";
    let decoding = |why: &str| invalid(&format!("the request does not decode: {why}"));
    let number = "expected an i64 as a quoted decimal string, integer, float, or null";
    for (args, expected) in [
        (
            emit(&["--version", "x"]),
            unread(&format!(
                "invalid value 'x' for '--version <VERSION>': invalid value: string \"x\", {number}"
            )),
        ),
        (
            emit(&["--version", "9223372036854775808"]),
            unread(&format!(
                "invalid value '9223372036854775808' for '--version <VERSION>': invalid value: \
                 string \"9223372036854775808\", {number}"
            )),
        ),
        (
            emit(&["--version", "-1"]),
            unread("unexpected argument '-1' found"),
        ),
        (
            vec!["events", "list", "--limit", "null"],
            unread(
                "invalid value 'null' for '--limit <LIMIT>': invalid value: string \"null\", \
                 expected a u32 as integer, string, float, or null",
            ),
        ),
        (
            vec!["events", "list", "--limit", "0"],
            invalid("a limit is at least 1"),
        ),
        (
            vec!["keys", "create", "--expires-at", "tomorrow"],
            unread(
                "invalid value 'tomorrow' for '--expires-at <EXPIRES_AT>': \
                 invalid RFC 3339 timestamp: tomorrow",
            ),
        ),
        (
            vec!["keys", "create", "--expires-at", KEY],
            unread(
                "invalid value <hidden> for '--expires-at <EXPIRES_AT>': \
                 invalid RFC 3339 timestamp: <hidden>",
            ),
        ),
        (
            vec![
                "keys",
                "create",
                "--expires-at",
                "2099-01-01T00:00:00.0001Z",
            ],
            invalid("a time is in whole milliseconds and in years 0001 to 9999"),
        ),
        (
            vec!["keys", "create", "--scopes", "read", "--scopes", "reed"],
            invalid("a scope is read, write, admin or execute"),
        ),
        (
            emit(&["--name", "a", "--name", "b"]),
            unread("the argument '--name <NAME>' cannot be used multiple times"),
        ),
        (
            emit(&["--data"]),
            unread("a value is required for '--data <DATA>' but none was supplied"),
        ),
        (
            emit(&["--data", "@none"]),
            unread(
                "invalid value '@none' for '--data <DATA>': No such file or directory (os error 2)",
            ),
        ),
        (
            emit(&["--metadata", "@latin"]),
            unread(
                "invalid value '@latin' for '--metadata <METADATA>': \
                 stream did not contain valid UTF-8",
            ),
        ),
        (
            emit(&["--name", "a", "--json", "{}"]),
            unread("the argument '--name <NAME>' cannot be used with '--json <JSON>'"),
        ),
        (
            emit(&["--json", "{}", "--data", "1", "--name", "a"]),
            unread("the argument '--json <JSON>' cannot be used with: --data <DATA> --name <NAME>"),
        ),
        (
            vec!["events", "emit-batch", "--items", "{}"],
            unread("unexpected argument '--items' found"),
        ),
        (
            json("@none"),
            invalid("cannot read what --json names: No such file or directory (os error 2)"),
        ),
        (
            json("@latin"),
            invalid("cannot read what --json names: stream did not contain valid UTF-8"),
        ),
        (
            vec!["whoami", "--json", "-"],
            decoding("EOF while parsing a value at line 1 column 0"),
        ),
        (
            json("{"),
            decoding("EOF while parsing an object at line 1 column 1"),
        ),
        (
            json("{} {}"),
            decoding("trailing characters at line 1 column 4"),
        ),
        (
            json("null"),
            decoding("invalid type: null, expected struct EmitRequest at line 1 column 4"),
        ),
        // What decoding would ignore, unread.
        (
            json(r#"{"name":"a.b","nope":1e400}"#),
            invalid("the request has no field 'nope'"),
        ),
        (
            json(r#"{"name":"a.b","data":1e400}"#),
            hinted("data is not a string", text),
        ),
        (
            json(r#"{"name":"a.b","data ":"1"}"#),
            invalid("the request has no field 'data '"),
        ),
        // Its label is no text; the caller's key is looked for in it all the same.
        (
            vec!["keys", "create", "--json", r#"{"label":"\ud800"}"#],
            decoding("unexpected end of hex escape at line 1 column 17"),
        ),
        // What an argument holds does not end the message, as what clap adds does.
        (
            emit(&["--version", "7\n\nUsage: x"]),
            unread(&format!(
                "invalid value '7 Usage: x' for '--version <VERSION>': invalid value: string \
                 \"7\\n\\nUsage: x\", {number}"
            )),
        ),
        (
            emit(&["--version", "7\n\nFor more information"]),
            unread(&format!(
                "invalid value '7 For more information' for '--version <VERSION>': invalid \
                 value: string \"7\\n\\nFor more information\", {number}"
            )),
        ),
        (
            emit(&["x\n\nUsage: y"]),
            unread("unexpected argument 'x Usage: y' found"),
        ),
        (
            emit(&["--version", "7\n\nx"]),
            unread(&format!(
                "invalid value '7 x' for '--version <VERSION>': invalid value: string \
                 \"7\\n\\nx\", {number}"
            )),
        ),
        // The decoder's own: not a later one of two; nor is the earlier one unread.
        (
            json(r#"{"idempotency_key":"a","idempotency_key":"b"}"#),
            decoding("duplicate field `idempotency_key` at line 1 column 40"),
        ),
        (
            vec![
                "events",
                "emit-batch",
                "--json",
                r#"{"items":[{"nope":1}],"items":[]}"#,
            ],
            invalid("the request has no field 'items[0].nope'"),
        ),
        (
            json(r#"{"version":true}"#),
            decoding(&format!(
                "invalid type: boolean `true`, {number} at line 1 column 15"
            )),
        ),
        (
            vec!["events", "emit", "--json", &version],
            decoding(&format!(
                "invalid value: string <hidden> {number} at line 1 column 97"
            )),
        ),
        (
            json(r#"{"name":"a.b","idempotncy_key":"k"}"#),
            invalid("the request has no field 'idempotncy_key'"),
        ),
        (
            json(r#"{"name":"a.b","idempotencyKey":"k"}"#),
            invalid("the request has no field 'idempotencyKey'"),
        ),
        (
            vec!["events", "emit", "--json", &field],
            invalid("the request has no field '<hidden>'"),
        ),
        (
            vec!["whoami", "--json", r#"{"a":1}"#],
            invalid("the request has no field 'a'"),
        ),
        (
            vec![
                "events",
                "emit-batch",
                "--json",
                r#"{"items":[{"name":"a"},{"name":"b","idempotncy_key":"k"}]}"#,
            ],
            invalid("the request has no field 'items[1].idempotncy_key'"),
        ),
        (
            json(r#"{"name":"a.b","data":{"id":1}}"#),
            hinted("data is not a string", text),
        ),
        (
            json(r#"{"data":null}"#),
            hinted("data is not a string", text),
        ),
        (
            json(r#"{"metadata":{}}"#),
            hinted(
                "metadata is not a string",
                "metadata is JSON text: pass a string",
            ),
        ),
        (
            vec![
                "events",
                "emit-batch",
                "--json",
                r#"{"items":[{"name":"a"},{"name":"b","data":7}]}"#,
            ],
            hinted("items[1].data is not a string", text),
        ),
        (
            vec!["keys", "create", "--json", r#"[["read"]]"#],
            decoding("invalid type: sequence, expected an object at line 1 column 0"),
        ),
        (
            vec!["events", "emit-batch", "--json", r#"{"items":[["a"]]}"#],
            decoding("invalid type: sequence, expected an object at line 1 column 10"),
        ),
    ] {
        let mut command = command(&args);
        command
            .env("IRONFLOWX_URL", &served.url)
            .env("IRONFLOWX_KEY", KEY)
            .current_dir(dir.path());
        let out = command.output().unwrap();
        assert_eq!(envelope(&out), expected, "{args:?}");
    }
    assert!(served.operations.take().is_empty());
}

/// TEN-3: what an agent stored can hold its own key, with reason; its answer is refused all the
/// same, and nothing stored is lost by it.
#[test]
fn stored_data_that_repeats_the_callers_key_is_not_printed() {
    let served = Served::answering(Ok(whoami()));
    let secret = &KEY[41..];
    let other = KEY.replace('A', "B");
    let call = |args: &[&str], key: &str| {
        let mut command = command(args);
        command.env("IRONFLOWX_URL", &served.url);
        command.env("IRONFLOWX_KEY", key).output().unwrap()
    };
    let message = "the answer is not printed, since it repeats the key the CLI was given";
    let refused = json!({"code": "internal", "message": message, "retryable": false});
    let plain = || event_page().events.remove(1);
    let printed = |data: &str, rest: &str| {
        format!(
            concat!(
                r#"{{"data":{},"emitted_by":"embedder:a b","#,
                r#""event_id":"0190b5f0-0000-7000-8000-000000000005",{}"#,
                r#""name":"commande.passée","time":"2023-11-14T22:13:20.120Z","version":"1"}}"#,
            ),
            data, rest
        )
    };
    for held in [KEY, secret] {
        let text = format!("{{ \"note\" : \"the key is {held}.\" }}");
        let compact = format!("{{\"note\":\"the key is {held}.\"}}");
        for (event, data, rest) in [
            (
                Event {
                    data: text.clone().into(),
                    ..plain()
                },
                &*compact,
                String::new(),
            ),
            (
                Event {
                    metadata: Some(text.clone().into()),
                    ..plain()
                },
                "null",
                format!(r#""metadata":{compact},"#),
            ),
            (
                Event {
                    idempotency_key: Some(held.to_owned()),
                    ..plain()
                },
                "null",
                format!(r#""idempotency_key":"{held}","#),
            ),
        ] {
            let printed = printed(data, &rest);
            served.holds(event, Some("0123.7"));
            for (args, printed) in [
                (["events", "get"], format!("{printed}\n")),
                (
                    ["events", "list"],
                    format!("{{\"events\":[{printed}],\"next\":\"0123.7\"}}\n"),
                ),
            ] {
                let out = call(&args, KEY);
                assert_eq!(envelope(&out), (refused.clone(), 1), "{args:?} {held}");
                // Another key's holder reads it.
                let out = call(&args, &other);
                assert_eq!(out.status.code(), Some(0), "{args:?} {held}");
                assert_eq!(stdout(&out), printed, "{args:?} {held}");
            }
        }
    }
    // A repeat that its JSON writes another way is not found.
    let data = format!("\"{}\\u0041{}\"", &KEY[41..60], &KEY[61..]);
    let event = Event {
        data: data.clone().into(),
        ..plain()
    };
    served.holds(event, None);
    let out = call(&["events", "get"], KEY);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), format!("{}\n", printed(&data, "")));
}

/// What answers may be no engine: JSON text that is none does not make its line JSON.
#[test]
fn json_text_that_is_no_json_is_not_printed() {
    let served = Served::answering(Ok(whoami()));
    for data in [
        "1 2",
        "",
        "1,\"name\":\"another\"",
        "{\"a\":1}\n{\"b\":2}",
        "\"\n\"",
        "'a'",
    ] {
        let event = Event {
            data: data.into(),
            ..event()
        };
        served.holds(event, None);
        for args in [["events", "get"], ["events", "list"]] {
            let mut command = command(&args);
            let out = command.env("IRONFLOWX_URL", &served.url).output().unwrap();
            let message = "the answer holds JSON text that is no JSON";
            let refused = json!({"code": "internal", "message": message, "retryable": false});
            assert_eq!(envelope(&out), (refused, 1), "{data}");
        }
    }
}

/// TEN-3: a label is in the answer, which would not be printed, and the new key in it lost.
#[test]
fn a_key_is_not_created_with_the_callers_in_its_label() {
    let served = Served::answering(Ok(whoami()));
    let own = &KEY[41..];
    let part = &KEY[42..];
    let other = KEY.replace('A', "B");
    let labelled = |label: &str| format!(r#"{{"label":"{label}"}}"#);
    let escaped = labelled(&format!("{}\\u0041{}", &KEY[41..60], &KEY[61..]));
    let message = "no key is created, since the request repeats the key the CLI was given";
    for (args, refused) in [
        (&["--label", KEY][..], true),
        (&["--label", &format!("x{own}")], true),
        (&["--json", &labelled(own)], true),
        (&["--json", &escaped], true),
        (&["--label", part], false),
        (&["--json", &labelled(part)], false),
        (&["--label", &other], false),
    ] {
        let mut command = command(&[&["keys", "create"], args].concat());
        command.env("IRONFLOWX_URL", &served.url);
        let out = command.env("IRONFLOWX_KEY", KEY).output().unwrap();
        let sent = served.operations.take_requests();
        if refused {
            assert_eq!(envelope(&out), invalid(message), "{args:?}");
            assert!(sent.is_empty(), "{args:?}");
        } else {
            assert_eq!(out.status.code(), Some(0), "{args:?}");
            let label = if args[1] == other { &*other } else { part };
            let request = CreateKeyRequest {
                label: Some(label.to_owned()),
                ..Default::default()
            };
            assert_eq!(sent, [format!("{request:?}")], "{args:?}");
        }
    }
}

/// SRV-11: what an agent branches on where no engine answers: whether the call may have run.
#[test]
fn a_state_change_no_engine_answers_is_unknown() {
    // Takes each connection and closes it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dropped = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || listener.incoming().for_each(drop));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    for (args, hint) in [
        (["events", "get"], None),
        (["events", "list"], None),
        (
            ["events", "emit"],
            Some("check first what landed: a retry may store a second event"),
        ),
        (
            ["events", "emit-batch"],
            Some(
                "retry unchanged only if every item had an idempotency key and keeps it; \
                 else check first what landed",
            ),
        ),
        (["environments", "list"], None),
        (["keys", "list"], None),
        (
            ["keys", "create"],
            Some(
                "the key may be stored: list the keys before the retry, and again after the \
                 engine reopens; revoke the one no holder has",
            ),
        ),
        (
            ["keys", "revoke"],
            Some("retry: revoking a revoked key succeeds, and one that revoked itself is refused"),
        ),
    ] {
        let call = |url: &str| {
            let mut command = command(&args);
            command.env("IRONFLOWX_URL", url);
            let out = command.env("IRONFLOWX_KEY", KEY).output().unwrap();
            let (envelope, exit) = envelope(&out);
            let code = envelope["code"].as_str().unwrap().to_owned();
            let retryable = envelope["retryable"].as_bool().unwrap();
            let hint = envelope.get("hint").map(|h| h.as_str().unwrap().to_owned());
            (code, retryable, hint, exit)
        };
        let unavailable = ("unavailable".to_owned(), true, None, 8);
        assert_eq!(call(&closed), unavailable, "{args:?}");
        let ended = match hint {
            Some(hint) => ("unknown".to_owned(), false, Some(hint.to_owned()), 6),
            None => unavailable,
        };
        assert_eq!(call(&dropped), ended, "{args:?}");
    }
}

/// A TLS proxy of `to`, for `localhost`, with the certificate in `tests/tls`: its port.
async fn tls_proxy(to: String) -> u16 {
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::crypto::aws_lc_rs::default_provider;
    use tokio_rustls::rustls::pki_types::pem::PemObject as _;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let cert = CertificateDer::from_pem_file(CERT).unwrap();
    let key = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tls/key.pem");
    let key = PrivateKeyDer::from_pem_file(key).unwrap();
    let config = ServerConfig::builder_with_provider(default_provider().into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // A client that does not trust it ends the handshake.
            if let Ok(mut stream) = tls.accept(stream).await {
                let mut engine = tokio::net::TcpStream::connect(&to).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut engine).await;
            }
        }
    });
    port
}

const CERT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tls/cert.pem");

/// SRV-4, over TLS. `SSL_CERT_FILE` stands for the system's store, in the child alone: the roots
/// are read from it in the store's place, on macOS too.
#[test]
fn a_server_the_systems_store_does_not_trust_is_unavailable() {
    let served = Served::answering(Ok(whoami()));
    let engine = served.url.strip_prefix("http://").unwrap().to_owned();
    let port = served._runtime.block_on(tls_proxy(engine.clone()));
    let url = format!("https://localhost:{port}");
    let call = |args: &[&str], url: &str, roots: Option<&str>| {
        let mut command = command(args);
        command
            .env_remove("SSL_CERT_FILE")
            .env_remove("SSL_CERT_DIR");
        if let Some(roots) = roots {
            command.env("SSL_CERT_FILE", roots);
        }
        command.args(["--url", url]).env("IRONFLOWX_KEY", KEY);
        command.output().unwrap()
    };
    for args in [
        ["events", "emit"],
        ["events", "emit-batch"],
        ["events", "list"],
        ["keys", "create"],
        ["keys", "revoke"],
    ] {
        let out = call(&args, &url, Some(CERT));
        assert_eq!(out.status.code(), Some(0), "{args:?} {out:?}");
        assert_eq!(served.operations.take(), [Some(KEY.to_owned())]);
        let other_name = format!("https://127.0.0.1:{port}");
        let plain = format!("https://{engine}");
        for (url, roots) in [
            (&url, None),
            (&url, Some("no such file")),
            (&other_name, Some(CERT)),
            (&plain, Some(CERT)),
        ] {
            let (envelope, exit) = envelope(&call(&args, url, roots));
            let code = envelope["code"].as_str();
            let retryable = envelope["retryable"].as_bool();
            let ended = (code, retryable, exit);
            let unavailable = (Some("unavailable"), Some(true), 8);
            assert_eq!(ended, unavailable, "{args:?} {url} {roots:?}");
            assert_eq!(envelope.get("hint"), None, "{args:?} {url} {roots:?}");
        }
        assert_eq!(served.operations.take(), []);
    }
}

/// What `command` prints on a terminal, which must succeed.
fn on_a_terminal(mut command: Command) -> String {
    let terminal = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&terminal).unwrap();
    unlockpt(&terminal).unwrap();
    let name = ptsname(&terminal, Vec::new()).unwrap();
    let end = File::options()
        .read(true)
        .write(true)
        .open(OsStr::from_bytes(name.as_bytes()))
        .unwrap();
    command.stdout(end).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    // Its copy of the terminal's end.
    drop(command);
    let mut terminal = File::from(terminal);
    let mut printed = Vec::new();
    let mut buffer = [0; 1024];
    // Linux fails the read once no one holds the other end.
    while let Ok(n @ 1..) = terminal.read(&mut buffer) {
        printed.extend_from_slice(&buffer[..n]);
    }
    let mut stderr = String::new();
    let mut piped = child.stderr.take().unwrap();
    piped.read_to_string(&mut stderr).unwrap();
    assert_eq!((child.wait().unwrap().code(), &*stderr), (Some(0), ""));
    // As a terminal's end takes a newline.
    let printed = String::from_utf8(printed).unwrap();
    assert_eq!(
        printed.matches('\n').count(),
        printed.matches("\r\n").count()
    );
    printed.replace("\r\n", "\n")
}

#[test]
fn a_terminal_gets_an_indented_seed() {
    let text = on_a_terminal(command(&["seed", "generate"]));
    let labels: Vec<_> = text
        .lines()
        .map(|line| line.split_once(": ").map_or(line, |p| p.0))
        .collect();
    assert_eq!(labels, ["{", r#"  "key""#, r#"  "key_id""#, "}"]);
    let seed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        seed["key"].as_str().unwrap().get(4..40),
        seed["key_id"].as_str()
    );
}

#[test]
fn a_terminal_gets_indented_output() {
    let served = Served::answering(Ok(whoami()));
    let expected = r#"{
  "environment": "default/main",
  "expires_at": "2027-01-15T08:00:00.123Z",
  "key_id": "0190b5f0-0000-7000-8000-000000000001",
  "scopes": [
    "read",
    "admin"
  ]
}
"#;
    assert_eq!(on_a_terminal(served.whoami()), expected);
    // What an event holds stays on its line.
    let mut get = command(&["events", "get"]);
    get.env("IRONFLOWX_URL", &served.url);
    let expected = r#"{
  "data": {"b":1.50,"a":[9007199254740993,"\u00e9é\\\"",1e2]},
  "emitted_by": "key:0190b5f0-0000-7000-8000-000000000001",
  "event_id": "0190b5f0-0000-7000-8000-000000000005",
  "idempotency_key": "clé\u0001",
  "metadata": {"z":"DEL SEP😀\ud83d\ude00\/","a":-0},
  "name": "commande.passée",
  "time": "2023-11-14T22:13:20.120Z",
  "version": "9223372036854775807"
}
"#;
    let expected = expected.replace("DEL SEP", "\u{7f}\u{2028}");
    assert_eq!(on_a_terminal(get), expected);
}

/// SRV-11, D64: the engine's error, its kind's code, `retryable` and exit code.
#[test]
fn every_kind_is_one_envelope_and_one_exit_code() {
    let served = Served::answering(Ok(whoami()));
    for (kind, code, retryable, exit) in [
        (ErrorKind::Internal, "internal", false, 1),
        (ErrorKind::Invalid, "invalid", false, 2),
        (ErrorKind::Unauthenticated, "unauthenticated", false, 3),
        (ErrorKind::PermissionDenied, "permission_denied", false, 4),
        (ErrorKind::NotFound, "not_found", false, 5),
        (ErrorKind::Unknown, "unknown", false, 6),
        (ErrorKind::ResourceExhausted, "resource_exhausted", false, 7),
        (ErrorKind::Unavailable, "unavailable", true, 8),
        (ErrorKind::Conflict, "conflict", false, 9),
    ] {
        for hint in [None, Some("do\nthis")] {
            served.answers(Err(OpError {
                kind,
                message: "two\n\"lines\"".into(),
                hint: hint.map(str::to_owned),
            }));
            let mut expected =
                json!({"code": code, "message": "two\n\"lines\"", "retryable": retryable});
            if let Some(hint) = hint {
                expected["hint"] = json!(hint);
            }
            let out = served.whoami().output().unwrap();
            assert_eq!(envelope(&out), (expected, exit));
        }
    }
}

/// TEN-3: what answers may be no engine, and repeat what it was sent.
#[test]
fn a_key_an_answer_repeats_is_not_printed() {
    let served = Served::answering(Ok(whoami()));
    let other = KEY.replace('A', "B");
    // As one in a file of variables can be.
    let quoted = format!("'{KEY}'");
    let (listed, joined, longer) = (
        format!("{other},{KEY}"),
        format!("{other}{KEY}"),
        format!("{KEY}x"),
    );
    for (sent, repeated, hidden) in [
        (KEY, KEY, true),
        // Its secret alone, all but the last of it, and one less than that.
        (KEY, &KEY[41..], true),
        (KEY, &KEY[..83], true),
        (KEY, &KEY[42..], false),
        // Its ID is no secret.
        (KEY, &KEY[4..40], false),
        (&*quoted, KEY, true),
        (&longer, &KEY[41..], true),
        (&listed, KEY, true),
        (&joined, &KEY[41..], true),
        (&joined, &other[41..], true),
        (&KEY[1..], &KEY[1..], true),
        (&KEY[..43], &KEY[..43], true),
        // Too short to hold a secret, so any message may hold it.
        (&KEY[..42], &KEY[..42], false),
        ("ixk_", "ixk_", false),
        ("e", "e", false),
        ("", "", false),
    ] {
        let mut command = served.whoami();
        command.env("IRONFLOWX_KEY", sent);
        let mut output = || command.output().unwrap();
        // A message or a hint, whichever repeats it.
        for (in_message, in_hint) in [(true, true), (true, false), (false, true)] {
            let text = |repeats| format!("no '{}'", if repeats { repeated } else { "key" });
            let printed = |repeats| match repeats && hidden {
                true => "not printed, since it repeats the key the CLI was given".to_owned(),
                false => text(repeats),
            };
            served.answers(Err(OpError {
                kind: ErrorKind::Unauthenticated,
                message: text(in_message),
                hint: Some(text(in_hint)),
            }));
            let expected = json!({
                "code": "unauthenticated",
                "message": printed(in_message),
                "hint": printed(in_hint),
                "retryable": false,
            });
            let given = format!("{sent} sent, {repeated} repeated");
            assert_eq!(envelope(&output()), (expected, 3), "{given}");
        }
        // Nor in an answer, whose one free text is the environment's name.
        served.answers(Ok(Whoami {
            environment: Address::new("default", &format!("a{repeated}")),
            ..whoami()
        }));
        let out = output();
        let given = format!("{sent} sent, {repeated} repeated");
        if hidden {
            let message = "the answer is not printed, since it repeats the key the CLI was given";
            let expected = json!({"code": "internal", "message": message, "retryable": false});
            assert_eq!(envelope(&out), (expected, 1), "{given}");
        } else {
            let answer: Value = serde_json::from_str(stdout(&out)).unwrap();
            let environment = format!("default/a{repeated}");
            assert_eq!(answer["environment"], environment, "{given}");
        }
    }
}

#[test]
fn a_usage_error_is_an_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let own = format!("'{}", &KEY[42..]);
    for (args, message) in [
        (
            &[][..],
            "'ironflowx' requires a subcommand but one was not provided \
             [subcommands: serve, recover-key, seed, version, describe, environments, events, whoami, keys]",
        ),
        (
            &["seed"],
            "'ironflowx seed' requires a subcommand but one was not provided [subcommands: generate]",
        ),
        (&["seed", "nope"], "unrecognized subcommand 'nope'"),
        (&["seed", "generate", "x"], "unexpected argument 'x' found"),
        (&["serve", "--key"], "unexpected argument '--key' found"),
        (
            &["serve", "--seed-file"],
            "a value is required for '--seed-file <PATH>' but none was supplied",
        ),
        (
            &["serve", "--log-level"],
            "a value is required for '--log-level <LEVEL>' but none was supplied \
             [possible values: off, error, warn, info, debug, trace]",
        ),
        (
            &["serve", "--log-format"],
            "a value is required for '--log-format <FORMAT>' but none was supplied \
             [possible values: json, text]",
        ),
        (
            &["serve", "--log-level", "bad"],
            "invalid value 'bad' for '--log-level <LEVEL>' \
             [possible values: off, error, warn, info, debug, trace]",
        ),
        (
            &["serve", "--log-format", "bad"],
            "invalid value 'bad' for '--log-format <FORMAT>' [possible values: json, text]",
        ),
        (
            &["serve", "--log-level", "info", "--log-level", "warn"],
            "the argument '--log-level <LEVEL>' cannot be used multiple times",
        ),
        (
            &["serve", "--log-format", "text", "--log-format", "json"],
            "the argument '--log-format <FORMAT>' cannot be used multiple times",
        ),
        (
            &["serve", "--seed-file", "first", "--seed-file", "second"],
            "the argument '--seed-file <PATH>' cannot be used multiple times",
        ),
        (&["nope"], "unrecognized subcommand 'nope'"),
        (&["describe", "health"], "unrecognized subcommand 'health'"),
        (&["describe", KEY], "unrecognized subcommand <hidden>"),
        (&["help"], "unrecognized subcommand 'help'"),
        (&["--version"], "unexpected argument '--version' found"),
        (
            &["whoami", "--no-such-flag"],
            "unexpected argument '--no-such-flag' found",
        ),
        (&["whoami", "x"], "unexpected argument 'x' found"),
        (
            &["whoami", "--url"],
            "a value is required for '--url <URL>' but none was supplied",
        ),
        (
            &["whoami", "--url", "http://h", "--url", "http://h"],
            "the argument '--url <URL>' cannot be used multiple times",
        ),
        (&["whoami", KEY], "unexpected argument <hidden> found"),
        (&[KEY], "unrecognized subcommand <hidden>"),
        (
            &["serve", "--listen", KEY],
            "invalid value <hidden> for '--listen <ADDRESS>': invalid socket address syntax",
        ),
        // What a key ends with: its secret, or that and the ID before it.
        (
            &["whoami", &KEY[41..]],
            "unexpected argument <hidden> found",
        ),
        (
            &["whoami", &KEY[42..]],
            "unexpected argument 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' found",
        ),
        (&["whoami", &KEY[4..]], "unexpected argument <hidden> found"),
        (&["whoami", "ixk_x"], "unexpected argument <hidden> found"),
        // Clap's quotes do not count; its own do.
        (&["whoami", &own], "unexpected argument <hidden> found"),
        // An ID is no secret.
        (
            &["whoami", &KEY[4..40]],
            "unexpected argument '0190b5f0-0000-7000-8000-000000000001' found",
        ),
        (
            &["whoami", "--key=ixk_x"],
            "unexpected argument '--key' found",
        ),
        (
            &["version", "--url", "http://h"],
            "unexpected argument '--url' found",
        ),
        (
            &["serve", "--listen", "x"],
            "invalid value 'x' for '--listen <ADDRESS>': invalid socket address syntax",
        ),
    ] {
        let mut command = command(args);
        command.current_dir(dir.path());
        assert_eq!(envelope(&command.output().unwrap()), unread(message));
    }
    assert!(!data_dir.exists() && !dir.path().join(".ironflowx").exists());
}

#[test]
fn help_is_text() {
    for (args, usage) in [
        (&["--help"][..], "Usage: ironflowx <COMMAND>"),
        (&["-h"], "Usage: ironflowx <COMMAND>"),
        (&["whoami", "--help"], "Usage: ironflowx whoami [OPTIONS]"),
        (
            &["events", "emit", "--help"],
            "--data <DATA>\n          JSON text, or @<file>\n",
        ),
        (
            &["keys", "create", "--help"],
            "--scopes <SCOPES>          May be repeated\n",
        ),
        (
            &["events", "emit-batch", "--help"],
            "The whole request, in place of its flags: JSON, @<file> or -, stdin\n",
        ),
        (&["version", "--help"], "Usage: ironflowx version"),
        (&["serve", "--help"], "Usage: ironflowx serve [OPTIONS]"),
    ] {
        let out = command(args).env("IRONFLOWX_KEY", KEY).output().unwrap();
        assert!(stdout(&out).contains(usage), "{}", stdout(&out));
        assert!(!stdout(&out).contains(KEY));
        assert_eq!((out.status.code(), out.stderr), (Some(0), vec![]));
    }
}

/// D61: `--key-file`, then `IRONFLOWX_KEY`, then none.
#[test]
fn the_key_is_the_one_given_and_no_other() {
    let served = Served::answering(Ok(whoami()));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("key");
    let other = KEY.replace('A', "B");
    let given = |file_holds: Option<&[u8]>, variable: Option<&[u8]>| {
        let mut command = served.whoami();
        command.current_dir(dir.path());
        if let Some(text) = file_holds {
            std::fs::write(&file, text).unwrap();
            command.arg("--key-file").arg(&file);
        }
        if let Some(text) = variable {
            command.env("IRONFLOWX_KEY", OsStr::from_bytes(text));
        }
        served.key_sent(&mut command)
    };
    let (key, spaced) = (KEY.as_bytes(), format!(" \t{KEY}\r\n\n"));
    let spaced = spaced.as_bytes();
    let of = |key: &str| Some(key.to_owned());
    assert_eq!(given(None, None), None);
    assert_eq!(given(None, Some(key)), of(KEY));
    assert_eq!(given(Some(key), None), of(KEY));
    assert_eq!(given(Some(key), Some(other.as_bytes())), of(KEY));
    assert_eq!(given(None, Some(spaced)), of(KEY));
    assert_eq!(given(Some(spaced), None), of(KEY));
    // An empty key is no key, and the flag still wins.
    assert_eq!(given(None, Some(b"")), None);
    assert_eq!(given(None, Some(b" \n")), None);
    assert_eq!(given(Some(b"\n"), Some(key)), None);
    // Not text: no well-formed key, which the engine says.
    let broken = [key, b"\xff"].concat();
    assert_eq!(given(None, Some(&broken)), of("malformed"));
    assert_eq!(given(Some(&broken), None), of("malformed"));
    // Inside, whitespace is part of it.
    assert_eq!(given(None, Some(b"a b")), of("malformed"));

    // Never unasked.
    std::fs::create_dir(dir.path().join(".ironflowx")).unwrap();
    for path in ["admin.key", ".ironflowx/admin.key"] {
        std::fs::write(dir.path().join(path), KEY).unwrap();
    }
    assert_eq!(given(None, None), None);
}

#[test]
fn a_key_file_that_cannot_be_read_is_a_usage_error() {
    let served = Served::answering(Ok(whoami()));
    let dir = tempfile::tempdir().unwrap();
    for (path, why) in [
        (dir.path(), "Is a directory (os error 21)"),
        // A key given where its file's path goes.
        (Path::new(KEY), "No such file or directory (os error 2)"),
    ] {
        let mut command = served.whoami();
        command
            .env("IRONFLOWX_KEY", KEY)
            .arg("--key-file")
            .arg(path);
        let message = format!("cannot read the file --key-file names: {why}");
        assert_eq!(envelope(&command.output().unwrap()), invalid(&message));
    }
    assert!(served.operations.take().is_empty());
}

#[test]
fn the_engine_is_the_one_the_flag_or_else_the_variable_names() {
    let served = Served::answering(Ok(whoami()));
    let mut by_flag = command(&["whoami", "--url", &served.url]);
    by_flag.env("IRONFLOWX_URL", "http://127.0.0.1:1");
    assert_eq!(served.key_sent(&mut by_flag), None);
    assert_eq!(served.key_sent(&mut served.whoami()), None);

    let form = "a URL of the form http[s]://host[:port][/path]";
    for given in [
        &b""[..],
        b"127.0.0.1:9310",
        b"http://u:pw@h",
        b"http://h/\xff",
    ] {
        let given = OsStr::from_bytes(given);
        let mut variable = command(&["whoami"]);
        variable.env("IRONFLOWX_URL", given);
        let message = format!("IRONFLOWX_URL is not {form}");
        assert_eq!(envelope(&variable.output().unwrap()), invalid(&message));
    }
    let mut flag = command(&["whoami", "--url", "http://u:pw@h"]);
    flag.env("IRONFLOWX_URL", &served.url);
    let message = format!("--url is not {form}");
    assert_eq!(envelope(&flag.output().unwrap()), invalid(&message));
    assert!(served.operations.take().is_empty());
}

/// The client's own tests show that it holds to the deadline it sends.
#[test]
fn a_call_is_given_30_seconds() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let head = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            if stream.read(&mut byte).unwrap() == 0 {
                break;
            }
            head.extend(byte);
        }
        let _ = stream.write_all(b"HTTP/1.1 503 x\r\n\r\n");
        String::from_utf8(head).unwrap().to_lowercase()
    });
    let out = command(&["whoami", "--url", &url]).output().unwrap();
    // If it never called, this ends the wait.
    drop(TcpStream::connect(addr));
    let head = head.join().unwrap();
    assert!(head.contains("\r\nconnect-timeout-ms: 30000\r\n"), "{head}");
    let (envelope, exit) = envelope(&out);
    assert_eq!((&envelope["code"], exit), (&json!("unavailable"), 8));
}

#[test]
fn an_answer_that_cannot_be_printed_is_a_failure() {
    let served = Served::answering(Ok(whoami()));
    let mut create = command(&["keys", "create"]);
    create.env("IRONFLOWX_URL", &served.url);
    let lost = "the key is stored and its secret lost: list the keys, revoke it";
    for (mut command, hint) in [
        (served.whoami(), None),
        (command(&["version"]), None),
        (command(&["--help"]), None),
        (command(&["seed", "generate"]), None),
        (create, Some(lost)),
    ] {
        // A pipe nothing reads.
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let child = command.stdout(writer).stderr(Stdio::piped()).spawn();
        drop(command);
        let (envelope, exit) = envelope(&child.unwrap().wait_with_output().unwrap());
        let message = "cannot write to stdout: Broken pipe (os error 32)";
        let mut expected = json!({"code": "internal", "message": message, "retryable": false});
        if let Some(hint) = hint {
            expected["hint"] = json!(hint);
        }
        assert_eq!((envelope, exit), (expected, 1));
    }
}

#[test]
fn a_short_write_leaves_only_the_response_prefix() {
    use ironflowx_core::KeyOrigin;
    use ironflowx_operations::{Engine, HostClock};
    use ironflowx_proto::{Call, ListKeysRequest, Operations as _};
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    use std::io::{Seek as _, SeekFrom};
    use std::os::unix::process::CommandExt as _;

    const CHILD: &str = "IRONFLOWX_TEST_SHORT_WRITE";
    const URL: &str = "IRONFLOWX_TEST_SHORT_WRITE_URL";
    const RECOVER: &str = "IRONFLOWX_TEST_SHORT_WRITE_RECOVER";
    const SEED_PREFIX: &[u8] = b"{\"key\":\"";
    // The recovery database must still be writable under the subprocess limit.
    const LIMIT: u64 = 16 * 1024 * 1024;
    const START: u64 = LIMIT - SEED_PREFIX.len() as u64;
    if let Some(path) = std::env::var_os(CHILD) {
        let mut stdout = File::options().write(true).open(path).unwrap();
        stdout.seek(SeekFrom::Start(START)).unwrap();
        setrlimit(
            Resource::Fsize,
            Rlimit {
                current: Some(LIMIT),
                maximum: getrlimit(Resource::Fsize).maximum,
            },
        )
        .unwrap();
        let mut cmd = if let Some(url) = std::env::var_os(URL) {
            let mut cmd = command(&["keys", "create"]);
            cmd.env("IRONFLOWX_URL", url);
            cmd
        } else if let Some(dir) = std::env::var_os(RECOVER) {
            let mut cmd = command(&["recover-key", "--stdout", "--data-dir"]);
            cmd.arg(dir);
            cmd
        } else {
            command(&["seed", "generate"])
        };
        let err = cmd.stdout(stdout).exec();
        panic!("cannot launch command under file limit: {err}");
    }

    let served = Served::answering(Ok(whoami()));
    let created = command(&["keys", "create"])
        .env("IRONFLOWX_URL", &served.url)
        .output()
        .unwrap();
    assert_eq!(created.status.code(), Some(0));
    assert!(created.stderr.is_empty());

    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let opened = runtime
        .block_on(Engine::open(dir.path(), HostClock, None))
        .unwrap();
    let call = Call::with_key(opened.minted.unwrap());
    for (name, prefix) in [
        ("seed", SEED_PREFIX),
        ("create", &created.stdout[..SEED_PREFIX.len()]),
        ("recover", SEED_PREFIX),
    ] {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut child = Command::new("/bin/sh");
        child
            .args(["-c", "trap '' XFSZ && exec \"$@\"", "short-write"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "a_short_write_leaves_only_the_response_prefix"])
            .env(CHILD, file.path())
            .env_remove(URL)
            .env_remove(RECOVER);
        match name {
            "create" => child.env(URL, &served.url),
            "recover" => child.env(RECOVER, dir.path()),
            _ => &mut child,
        };
        let out = child.output().unwrap();
        let mut capture = file.reopen().unwrap();
        capture.seek(SeekFrom::Start(START)).unwrap();
        let mut bytes = Vec::new();
        capture.read_to_end(&mut bytes).unwrap();
        assert!(
            bytes == prefix,
            "stdout must contain only the start of the response"
        );
        let error = std::io::Error::from(rustix::io::Errno::FBIG);
        let mut expected = json!({
            "code": "internal",
            "message": format!("cannot write to stdout: {error}"),
            "retryable": false,
        });
        if name == "create" {
            expected["hint"] =
                json!("the key is stored and its secret lost: list the keys, revoke it");
        }
        if name == "recover" {
            let page = runtime
                .block_on(opened.engine.list_keys(&call, ListKeysRequest::default()))
                .unwrap();
            assert_eq!(page.keys.len(), 2);
            let id = page
                .keys
                .iter()
                .find(|key| key.origin == KeyOrigin::Recovered)
                .expect("the key was committed")
                .key_id;
            expected["message"] = json!(format!(
                "cannot deliver recovered key {id} to stdout: {error}"
            ));
            expected["hint"] = json!(format!(
                "use a working admin key or recover one with ironflowx recover-key --stdout, then revoke {id}"
            ));
            assert!(!dir.path().join("admin.key").exists());
        }
        let actual: Value = serde_json::from_slice(&out.stderr).expect("stderr is an envelope");
        assert!(
            actual == expected,
            "stderr must contain only the write refusal"
        );
        assert_eq!(out.status.code(), Some(1));
    }
    runtime.block_on(opened.engine.close());
}

#[test]
fn version_names_the_commit_it_was_built_from() {
    let head = Command::new("git").args(["rev-parse", "HEAD"]).output();
    let head = head.ok().filter(|out| out.status.success());
    let head = head.map(|out| String::from_utf8(out.stdout).unwrap().trim().to_owned());
    let out = command(&["version"]).output().unwrap();
    let version: Value = serde_json::from_str(stdout(&out)).unwrap();
    let expected = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": head.unwrap_or_else(|| "unknown".to_owned()),
        "protocol": "ironflowx.v1",
    });
    assert_eq!(version, expected);
    assert_eq!((out.status.code(), out.stderr), (Some(0), vec![]));
}
