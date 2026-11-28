//! `cli.feature` on the `ironflowx` binary: a served engine, and each command a process of its own.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use cucumber::gherkin::Step;
use cucumber::writer::Stats as _;
use cucumber::{World, given, then, when};
use ironflowx_core::{Scope, Secret};
use ironflowx_proto::wire::FILE_DESCRIPTOR_SET;
use ironflowx_proto::{Call, Client, Operations as _, WhoamiRequest};
use prost_reflect::DescriptorPool;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::output::{Audit, Delivery};
use super::serve::{Served, command, serve_command};

/// How many scenarios run, an outline once per example, so no example goes unnoticed.
pub const RUNS: usize = 22;

#[derive(World)]
#[world(init = Self::new)]
pub struct Cli {
    served: Option<Served>,
    /// The working directory, which holds the data directory.
    dir: TempDir,
    variables: BTreeMap<&'static str, String>,
    ran: Option<Output>,
    /// A key with the `read` scope alone.
    read: Option<String>,
    audit: Audit,
}

/// The default: where a CLI that looked for an admin key would look.
const DATA_DIR: &str = ".ironflowx";

impl fmt::Debug for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cli")
            .field("status", &self.ran.as_ref().map(|out| out.status))
            .finish_non_exhaustive()
    }
}

impl Cli {
    fn new() -> Self {
        Self {
            served: None,
            dir: tempfile::tempdir().unwrap(),
            variables: BTreeMap::new(),
            ran: None,
            read: None,
            audit: Audit::default(),
        }
    }

    fn ran(&self) -> &Output {
        self.ran.as_ref().expect("a command ran")
    }

    fn stdout(&self) -> &str {
        std::str::from_utf8(&self.ran().stdout).unwrap()
    }

    fn stderr(&self) -> &str {
        std::str::from_utf8(&self.ran().stderr).unwrap()
    }

    /// The bootstrap key, in `admin.key`.
    fn key(&self) -> String {
        let path = self.dir.path().join(DATA_DIR).join("admin.key");
        let text = std::fs::read_to_string(path).unwrap();
        text.strip_suffix('\n').expect("a newline").to_owned()
    }

    /// Runs `line`, a command and its arguments as a shell reads them.
    fn run(&mut self, line: &str, stdin: &str) {
        let args = line.strip_prefix("ironflowx ").expect("a command of it");
        let binary = command();
        let mut shell = Command::new("sh");
        shell
            .args(["-c", &format!("exec \"$0\" {args}")])
            .arg(binary.get_program());
        for (name, value) in binary.get_envs() {
            assert_eq!(value, None, "it only drops variables");
            shell.env_remove(name);
        }
        let mut child = shell
            .current_dir(self.dir.path())
            .envs(&self.variables)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // One that reads none of it can end first.
        let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
        let out = child.wait_with_output().unwrap();
        let mut words = args.split_whitespace();
        let delivery = match (words.next(), words.next()) {
            (Some("keys"), Some("create")) => Some(Delivery::Create),
            (Some("seed"), Some("generate")) => Some(Delivery::SeedOrRecovery),
            _ => None,
        };
        self.audit.output(&out, delivery);
        self.ran = Some(out);
    }

    /// A key with these scopes, made from the CLI with the bootstrap key.
    fn create(&mut self, scopes: &[&str]) -> String {
        let given = self.variables.insert("IRONFLOWX_KEY", self.key());
        let scopes: String = scopes.iter().map(|s| format!(" --scopes {s}")).collect();
        self.run(&format!("ironflowx keys create{scopes}"), "");
        let key = self.succeeded()["key"].as_str().expect("a key").to_owned();
        match given {
            Some(key) => self.variables.insert("IRONFLOWX_KEY", key),
            None => self.variables.remove("IRONFLOWX_KEY"),
        };
        key
    }

    /// The answer of a command that must succeed.
    fn succeeded(&self) -> Value {
        assert_eq!(self.ran().status.code(), Some(0));
        assert_eq!(self.stderr(), "");
        self.answer()
    }

    /// Stdout, as the one JSON line it must be.
    fn answer(&self) -> Value {
        let line = self.stdout().strip_suffix('\n').expect("a line");
        assert!(!line.contains('\n'), "expected one line of JSON");
        serde_json::from_str(line).expect("one JSON value")
    }

    /// The envelope of the command that failed, with nothing on stdout.
    fn envelope(&self) -> Value {
        assert!(self.stdout().is_empty(), "expected no stdout");
        let line = self.stderr().strip_suffix('\n').expect("a line");
        assert!(!line.contains('\n'), "{line}");
        serde_json::from_str(line).expect("one JSON value")
    }

    fn finish(&mut self) {
        if let Some(server) = self.served.take() {
            server.stop(&self.dir.path().join(DATA_DIR), &self.audit);
        }
    }
}

#[given("the engine is served on a fresh data directory")]
fn served(w: &mut Cli) {
    let served = Served::start(&w.dir.path().join(DATA_DIR), "127.0.0.1:0");
    served.ready();
    w.audit.file_key(&w.key());
    w.served = Some(served);
}

#[given("the CLI is pointed at it with IRONFLOWX_URL")]
fn pointed(w: &mut Cli) {
    let url = w.served.as_ref().expect("a served engine").url();
    w.variables.insert("IRONFLOWX_URL", url);
}

/// Every command drops them.
#[given("no other IRONFLOWX_ variable is set except those the scenario names")]
fn no_variables(_: &mut Cli) {}

#[given("IRONFLOWX_KEY holds the bootstrap key")]
fn bootstrap_key(w: &mut Cli) {
    w.variables.insert("IRONFLOWX_KEY", w.key());
}

#[given("no key is given")]
fn no_key(w: &mut Cli) {
    assert!(!w.variables.contains_key("IRONFLOWX_KEY"));
}

#[given("IRONFLOWX_URL names an address where nothing listens")]
fn nothing_listens(w: &mut Cli) {
    // A port the system gives to no one who asks for any.
    let url = "http://127.0.0.1:1".to_owned();
    w.variables.insert("IRONFLOWX_URL", url);
}

#[given(
    regex = r#"^the working directory is the one holding the data directory, with "admin.key" in it$"#
)]
fn beside_the_key(w: &mut Cli) {
    // In either one.
    std::fs::write(w.dir.path().join("admin.key"), w.key()).unwrap();
}

#[given(regex = r#"^IRONFLOWX_KEY holds a "(read|write)"(?: and "(read)")? key$"#)]
fn holds_a_key(w: &mut Cli, one: String, two: String) {
    let scopes = [&*one, &*two];
    let scopes = scopes.iter().filter(|s| !s.is_empty()).copied();
    let key = w.create(&scopes.collect::<Vec<_>>());
    w.variables.insert("IRONFLOWX_KEY", key);
}

#[given(regex = r#"^a "read" key R and the bootstrap key A$"#)]
fn two_keys(w: &mut Cli) {
    w.read = Some(w.create(&["read"]));
}

/// `(?s)`: an argument can hold a newline.
#[when(regex = r#"(?s)^"(ironflowx .*)" runs(?: with stdout piped)?$"#)]
fn runs(w: &mut Cli, line: String) {
    let Some((line, held)) = line.split_once(", a file holding ") else {
        return w.run(&line, "");
    };
    let (_, file) = line.rsplit_once('@').expect("the file's name");
    let held = held.strip_prefix('\'').and_then(|h| h.strip_suffix('\''));
    std::fs::write(w.dir.path().join(file), held.expect("quotes")).unwrap();
    w.run(line, "");
}

#[when(regex = r#"^"(ironflowx events get --event-id)" runs with that ID and stdout piped$"#)]
fn gets_that_event(w: &mut Cli, line: String) {
    let id = w.answer()["event_id"].as_str().expect("an ID").to_owned();
    w.run(&format!("{line} {id}"), "");
}

#[when(regex = r#"^"([^"]*)" runs with stdin holding two items named "([^"]*)" and "([^"]*)"$"#)]
fn runs_with_two_items(w: &mut Cli, line: String, one: String, two: String) {
    let items = json!({"items": [{"name": one}, {"name": two}]});
    w.run(&line, &format!("{items:#}\n"));
}

#[when(
    regex = r#"^"([^"]*)" runs with (?:IRONFLOWX_KEY holding (A|R))?(?: and )?(?:--key-file (naming a file that holds R|holding R))?$"#
)]
fn runs_with_keys(w: &mut Cli, mut line: String, variable: String, file: String) {
    let read = w.read.clone().expect("a read key");
    let key = match &*variable {
        "A" => Some(w.key()),
        "R" => Some(read.clone()),
        _ => None,
    };
    if let Some(key) = key {
        w.variables.insert("IRONFLOWX_KEY", key);
    }
    if !file.is_empty() {
        std::fs::write(w.dir.path().join("r.key"), format!("{read}\n")).unwrap();
        line += " --key-file r.key";
    }
    assert!(!variable.is_empty() || !file.is_empty());
    w.run(&line, "");
}

#[then(expr = "stdout names key R")]
fn names_the_read_key(w: &mut Cli) {
    let read = w.read.as_ref().expect("a read key");
    let expected = json!({
        "key_id": read[4..40],
        "scopes": ["read"],
        "environment": "default/default",
    });
    assert_eq!(w.succeeded(), expected);
}

#[then(
    regex = r#"^it exits with code 0 and stdout is one line holding the new "event_id" and "duplicate" false$"#
)]
fn emitted(w: &mut Cli) {
    let answer = w.succeeded();
    let id = answer["event_id"].as_str().expect("an ID");
    assert_eq!(id.len(), 36, "{id}");
    assert_eq!(answer, json!({"event_id": id, "duplicate": false}));
}

#[then(regex = r#"^stdout is one line whose "data" is (.*)$"#)]
fn printed_data(w: &mut Cli, printed: String) {
    w.succeeded();
    // The text, not what it reads as.
    let data = format!("{{\"data\":{printed},\"emitted_by\":");
    assert!(w.stdout().starts_with(&data), "{}", w.stdout());
}

#[then(regex = r#"^whose "emitted_by" names the key$"#)]
fn emitted_by_the_key(w: &mut Cli) {
    let id = &w.variables["IRONFLOWX_KEY"][4..40];
    assert_eq!(w.answer()["emitted_by"], format!("key:{id}"));
}

#[then("it exits with code 0")]
fn succeeds(w: &mut Cli) {
    w.succeeded();
}

#[then(regex = r#"^stdout holds two results, each with an "event_id", in input order$"#)]
fn two_results(w: &mut Cli) {
    let answer = w.succeeded();
    let items = answer["items"].as_array().expect("results");
    assert_eq!(items.len(), 2);
    for (item, name) in items.iter().zip(["a.one", "a.two"]) {
        let id = item["ok"]["event_id"].as_str().expect("an ID");
        assert_eq!(*item, json!({"ok": {"event_id": id, "duplicate": false}}));
        w.run(&format!("ironflowx events get --event-id {id}"), "");
        assert_eq!(w.succeeded()["name"], name);
    }
}

#[then(regex = r#"^stdout holds the new key once, with scopes "read" only$"#)]
fn a_read_only_key(w: &mut Cli) {
    let answer = w.succeeded();
    let key = answer["key"].as_str().expect("a key");
    assert_eq!((key.len(), &key[..4]), (84, "ixk_"));
    assert_eq!(w.stdout().matches(key).count(), 1);
    assert_eq!(w.stdout().matches(&key[41..]).count(), 1);
    assert_eq!(answer["info"]["key_id"], key[4..40]);
    assert_eq!(answer["info"]["scopes"], json!(["read"]));
}

#[then(
    regex = r#"^"(ironflowx keys list)" shows the key with label "([^"]*)" and without its secret$"#
)]
fn lists_the_key(w: &mut Cli, line: String, label: String) {
    let created = w.answer();
    let key = created["key"].as_str().expect("a key");
    assert_eq!(created["info"]["label"], label);
    w.run(&line, "");
    let listed = w.succeeded();
    // Newest first, and the bootstrap key the other.
    assert_eq!(listed["keys"][0], created["info"]);
    assert_eq!(listed["keys"].as_array().map(Vec::len), Some(2));
    assert!(!w.stdout().contains(&key[41..]));
}

#[then(
    regex = r#"^stderr is one JSON object with code "([^"]*)", a message, retryable (true|false) and a hint or none$"#
)]
fn one_envelope(w: &mut Cli, code: String, retryable: bool) {
    let envelope = w.envelope();
    let message = envelope["message"].as_str().expect("a message");
    assert!(!message.is_empty());
    let mut expected = json!({"code": code, "message": message, "retryable": retryable});
    if let Some(hint) = envelope.get("hint") {
        assert!(!hint.as_str().expect("text").is_empty());
        expected["hint"] = hint.clone();
    }
    assert_eq!(envelope, expected);
}

/// Of a failure, which printed an envelope.
#[then(regex = r"^it exits with code ([1-9])$")]
fn exits(w: &mut Cli, code: i32) {
    assert_eq!(w.ran().status.code(), Some(code));
}

#[then("stdout is empty")]
fn no_stdout(w: &mut Cli) {
    assert!(w.stdout().is_empty(), "expected no stdout");
}

#[then(
    expr = "it exits with code {int} and an envelope with code {string} whose message names {string}"
)]
fn names(w: &mut Cli, exit: i32, code: String, named: String) {
    let envelope = w.envelope();
    let message = envelope["message"].as_str().expect("a message");
    assert!(message.contains(&named), "{message}");
    assert_eq!(
        (&envelope["code"], w.ran().status.code()),
        (&json!(code), Some(exit))
    );
}

#[then("stdout is exactly one line, a JSON object naming the key's ID and scopes")]
fn names_the_key(w: &mut Cli) {
    let expected = json!({
        "key_id": w.key()[4..40],
        "scopes": ["read", "write", "admin", "execute"],
        "environment": "default/default",
    });
    assert_eq!(w.answer(), expected);
    assert_eq!(w.ran().status.code(), Some(0));
}

#[then("stderr is empty")]
fn no_stderr(w: &mut Cli) {
    assert_eq!(w.stderr(), "");
}

#[then(expr = "it exits with code {int} and an envelope with code {string} and retryable true")]
fn retryable(w: &mut Cli, exit: i32, code: String) {
    let envelope = w.envelope();
    assert_eq!(
        (&envelope["code"], &envelope["retryable"]),
        (&json!(code), &json!(true))
    );
    assert_eq!(w.ran().status.code(), Some(exit));
}

#[then(expr = "it exits with code {int} and an envelope with code {string}")]
fn fails(w: &mut Cli, exit: i32, code: String) {
    assert_eq!(w.envelope()["code"], code);
    assert_eq!(w.ran().status.code(), Some(exit));
}

#[then(
    regex = r#"^stdout is a JSON object with "version", "commit" and "protocol" "ironflowx.v1"$"#
)]
fn version(w: &mut Cli) {
    let head = Command::new("git").args(["rev-parse", "HEAD"]).output();
    let head = head.ok().filter(|out| out.status.success());
    let commit = head.map_or_else(
        || "unknown".into(),
        |out| String::from_utf8(out.stdout).unwrap(),
    );
    let expected = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": commit.trim(),
        "protocol": "ironflowx.v1",
    });
    assert_eq!(w.answer(), expected);
    assert_eq!(w.ran().status.code(), Some(0));
}

#[then("its operations are exactly:")]
fn operations(w: &mut Cli, step: &Step) {
    let answer = w.succeeded();
    let pool = DescriptorPool::decode(FILE_DESCRIPTOR_SET).unwrap();
    let cli = pool.get_extension_by_name("ironflowx.v1.cli").unwrap();
    let summary = pool.get_extension_by_name("ironflowx.v1.summary").unwrap();
    let proto: BTreeMap<_, _> = pool
        .services()
        .flat_map(|service| service.methods().collect::<Vec<_>>())
        .map(|method| {
            let options = method.options();
            let path = options.get_extension(&cli).as_str().unwrap().to_owned();
            let text = options.get_extension(&summary).as_str().unwrap().to_owned();
            let rpc = format!("{}/{}", method.parent_service().full_name(), method.name());
            (path, (rpc, text))
        })
        .collect();
    let expected: BTreeMap<_, _> = step.table().unwrap().rows[1..]
        .iter()
        .map(|r| {
            let path = r[0].as_str();
            let (rpc, summary) = &proto[path];
            let changes_state = r[2].parse::<bool>().unwrap();
            (
                path,
                json!({
                    "command": path, "rpc": rpc, "summary": summary,
                    "scope": r[1], "changes_state": changes_state,
                }),
            )
        })
        .collect();
    let operations = answer["operations"].as_array().unwrap();
    let actual: BTreeMap<_, _> = operations
        .iter()
        .map(|op| (op["command"].as_str().unwrap(), op.clone()))
        .collect();
    assert_eq!(operations.len(), expected.len());
    assert_eq!(actual, expected);
    w.run("ironflowx version", "");
    let version = w.succeeded();
    for field in ["version", "protocol"] {
        assert_eq!(answer.get(field), version.get(field), "catalog {field}");
    }
}

#[then("its exit codes are exactly:")]
fn exit_codes(w: &mut Cli, step: &Step) {
    let expected: Vec<_> = step.table().unwrap().rows[1..]
        .iter()
        .map(|r| json!({"code": r[0].parse::<u8>().unwrap(), "kind": r[1], "retryable": r[2].parse::<bool>().unwrap()}))
        .collect();
    assert_eq!(w.succeeded()["exit_codes"], json!(expected));
}

fn schema(value: &Value) -> jsonschema::Validator {
    assert_eq!(
        value["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(value["additionalProperties"], false);
    jsonschema::options()
        .should_validate_formats(true)
        .build(value)
        .unwrap()
}

#[then("stdout holds an input schema, an output schema and an example")]
fn describes_an_operation(w: &mut Cli) {
    let answer = w.succeeded();
    schema(&answer["input"]);
    schema(&answer["output"]);
    assert!(answer["example"].is_object());
}

#[then("the example is valid against the input schema")]
fn valid_example(w: &mut Cli) {
    let answer = w.succeeded();
    assert!(schema(&answer["input"]).is_valid(&answer["example"]));
}

#[then(
    regex = r#"^with IRONFLOWX_KEY holding a "write" key, "(ironflowx events emit --json)" given the example exits with code 0$"#
)]
fn write_example(w: &mut Cli, line: String) {
    let described = w.succeeded();
    holds_a_key(w, "write".to_owned(), String::new());
    w.run(&format!("{line} -"), &described["example"].to_string());
    assert!(schema(&described["output"]).is_valid(&w.succeeded()));
}

/// A schema must also reject a real answer with one of its guaranteed fields removed.
fn requires(output: &jsonschema::Validator, answer: &Value, parent: &str, fields: &[&str]) {
    for field in fields {
        let mut incomplete = answer.clone();
        let object = incomplete
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap();
        assert!(
            object.remove(*field).is_some(),
            "{parent}/{field} was not printed"
        );
        assert!(
            !output.is_valid(&incomplete),
            "{parent}/{field} is not required"
        );
    }
}

/// SRV-12: what is advertised is what a real engine accepts and the CLI prints.
#[then("every described example works against a fresh engine with the bootstrap key")]
fn every_example(_: &mut Cli) {
    let mut w = Cli::new();
    w.run("ironflowx describe", "");
    let overview = w.succeeded();
    for op in overview["operations"].as_array().unwrap() {
        let path = op["command"].as_str().unwrap();
        let mut w = Cli::new();
        w.run(&format!("ironflowx describe {path}"), "");
        let mut described = w.succeeded();
        let input = schema(&described.as_object_mut().unwrap().remove("input").unwrap());
        let output = schema(&described.as_object_mut().unwrap().remove("output").unwrap());
        let example = described
            .as_object_mut()
            .unwrap()
            .remove("example")
            .unwrap();
        assert_eq!(&described, op);
        assert!(input.is_valid(&example), "{path}: {example}");
        served(&mut w);
        pointed(&mut w);
        bootstrap_key(&mut w);
        w.run(&format!("ironflowx {path} --json -"), &example.to_string());
        if matches!(path, "events get" | "keys revoke") {
            assert_eq!(w.ran().status.code(), Some(5), "{path}");
            assert_eq!(w.envelope()["code"], "not_found", "{path}");
            if path == "keys revoke" {
                w.finish();
                continue;
            }
            // The advertised ID is absent. Read a real event to exercise the answer schema.
            w.run("ironflowx events emit --name describe.probe", "");
            let emitted = w.succeeded();
            let id = emitted["event_id"].as_str().unwrap();
            w.run(&format!("ironflowx events get --event-id {id}"), "");
        }
        let answer = w.succeeded();
        assert!(output.is_valid(&answer), "{path}: output fails its schema");
        match path {
            "events get" => requires(&output, &answer, "", &["data", "time"]),
            "keys create" => {
                requires(&output, &answer, "", &["info"]);
                requires(&output, &answer, "/info", &["created_at"]);
            }
            "environments list" => requires(&output, &answer, "/environments/0", &["created_at"]),
            "events emit-batch" => {
                let items = answer["items"].as_array().unwrap();
                assert_eq!(items.len(), example["items"].as_array().unwrap().len());
                for (i, item) in items.iter().enumerate() {
                    assert!(item.get("ok").is_some(), "{path}: item {i} was refused");
                }
            }
            _ => {}
        }
        w.finish();
    }
}

#[then("the published inputs enforce the wire limits, scopes and IDs")]
fn input_edges(_: &mut Cli) {
    const ID: &str = "0190b5f0-0000-7000-8000-000000000001";
    let pages = [json!({"limit": 1}), json!({"limit": u32::MAX})];
    let bad_pages = [
        json!({"limit": 0}),
        json!({"limit": u64::from(u32::MAX) + 1}),
    ];
    let mut w = Cli::new();
    for (path, accepted, refused) in [
        ("events list", pages.clone(), bad_pages.clone()),
        ("keys list", pages, bad_pages),
        (
            "keys create",
            [
                json!({"scopes": []}),
                json!({"scopes": ["read", "write", "admin", "execute"]}),
            ],
            [json!({"scopes": ["owner"]}), json!({"scopes": ["READ"]})],
        ),
        (
            "events get",
            [json!({}), json!({"event_id": ID})],
            [
                json!({"event_id": "bad"}),
                json!({"event_id": ID.to_uppercase()}),
            ],
        ),
        (
            "keys revoke",
            [json!({}), json!({"key_id": ID})],
            [
                json!({"key_id": "bad"}),
                json!({"key_id": ID.to_uppercase()}),
            ],
        ),
    ] {
        w.run(&format!("ironflowx describe {path}"), "");
        let input = schema(&w.succeeded()["input"]);
        for value in accepted {
            assert!(input.is_valid(&value), "{path} input refused {value}");
        }
        for value in refused {
            assert!(!input.is_valid(&value), "{path} input accepted {value}");
        }
    }
}

#[then("the batch output schema requires exactly one result per item")]
fn batch_output_alternatives(_: &mut Cli) {
    let mut w = Cli::new();
    w.run("ironflowx describe events emit-batch", "");
    let output = schema(&w.succeeded()["output"]);
    let ok = json!({"event_id": "0190b5f0-0000-7000-8000-000000000001", "duplicate": false});
    let error = json!({"kind": "invalid", "message": "event name is required"});
    for (item, expected) in [
        (json!({"ok": ok}), true),
        (json!({"error": error}), true),
        (json!({"ok": ok, "error": error}), false),
        (json!({}), false),
    ] {
        let answer = json!({"items": [item]});
        assert_eq!(output.is_valid(&answer), expected, "batch output: {answer}");
    }
}

#[then(r#"stdout holds a "key" and its "key_id""#)]
fn printed_seed(w: &mut Cli) {
    let seed = w.succeeded();
    assert_eq!(
        seed["key"].as_str().unwrap().get(4..40),
        seed["key_id"].as_str()
    );
}

#[then(
    "an engine served on another fresh data directory with that key as IRONFLOWX_SEED authenticates it as admin"
)]
async fn use_seed(w: &mut Cli) {
    let seed = w.succeeded();
    let key = seed["key"].as_str().unwrap();
    let dir = w.dir.path().join("seeded");
    let mut cmd = serve_command(&dir, "127.0.0.1:0");
    let served = Served::start_command(cmd.env("IRONFLOWX_SEED", key));
    served.ready();
    let client = Client::new(&served.url()).unwrap();
    let cx = Call::with_key(Secret::new(key.to_owned()));
    let me = client.whoami(&cx, WhoamiRequest {}).await.unwrap();
    assert_eq!(me.key_id.unwrap().to_string(), seed["key_id"]);
    assert!(me.scopes.contains(&Scope::Admin));
    served.stop(&dir, &w.audit);
}

pub async fn run(parser: super::Unescaped) -> (bool, usize) {
    let writer = Cli::cucumber::<&str>()
        .with_parser(parser)
        .after(|_, _, _, _, world| {
            if let Some(world) = world {
                world.finish();
            }
            Box::pin(async {})
        })
        .fail_on_skipped()
        .run(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/behaviors/cli.feature"
        ))
        .await;
    let passed = writer.scenarios_stats().passed;
    (writer.execution_has_failed(), passed)
}
