//! `serve.feature` on the `ironflowx` binary, started by each scenario.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use connectrpc::client::{
    BoxFuture, CallOptions, ClientBody, ClientConfig, ClientTransport, HttpClient, full_body,
};
use connectrpc::{ConnectError, ErrorCode, Protocol};
use cucumber::writer::Stats as _;
use cucumber::{World, given, then, when};
use ironflowx_core::{ErrorKind, KeyOrigin, Scope, Secret};
use ironflowx_proto::wire::{
    CreateKeyRequest, CreateKeyResponse, EmitBatchRequest, EmitRequest, Event, EventServiceClient,
    GetEventRequest, IdentityServiceClient, KeyServiceClient, WhoamiRequest, WhoamiResponse,
    op_error,
};
use ironflowx_proto::{
    Call, Client, EmitBatchResponse, EmitResponse, ListEventsRequest, ListKeysRequest, OpError,
    Operation, Operations as _,
};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::Value;
use tempfile::TempDir;

use super::output::{Audit, Delivery};

/// How many scenarios run, an outline once per example, so no example goes unnoticed.
pub const RUNS: usize = 47;

const LATE: Duration = Duration::from_secs(30);

static BINARY: OnceLock<PathBuf> = OnceLock::new();

/// Builds the binary, as an operator would: Cargo gives a test a binary's path only within its own
/// package.
pub fn build() {
    let out = Command::new(env!("CARGO"))
        .args(["build", "-p", "ironflowx-bin", "--locked"])
        .arg("--message-format=json")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(out.status.success(), "ironflowx-bin builds");
    let binary = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|m| m["target"]["name"] == "ironflowx" && m["target"]["kind"][0] == "bin")
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .expect("cargo names the binary");
    BINARY.set(binary).expect("built once");
}

/// The binary, with no variable of ours set.
pub fn command() -> Command {
    let mut command = Command::new(BINARY.get().expect("the binary is built"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("IRONFLOWX_") {
            command.env_remove(name);
        }
    }
    command
}

/// A started `ironflowx serve`, killed when dropped, and what it has logged.
pub struct Served {
    child: Child,
    stdout: File,
    log: Arc<Mutex<Vec<String>>>,
    /// Ends once the whole log is in `log`.
    reader: Option<JoinHandle<()>>,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Served {
    /// Scans the live engine, then stops it with SIGTERM and checks the entire output and disk.
    pub fn stop(mut self, data_dir: &Path, audit: &Audit) -> String {
        self.ready();
        audit.directory(data_dir);
        kill_process(Pid::from_child(&self.child), Signal::TERM).unwrap();
        let status = until("stopped", || self.child.try_wait().unwrap());
        assert_eq!(status.code(), Some(0));
        self.finished(data_dir, audit)
    }

    fn finished(mut self, data_dir: &Path, audit: &Audit) -> String {
        self.reader.take().unwrap().join().unwrap();
        self.stdout.rewind().unwrap();
        let mut stdout = Vec::new();
        self.stdout.read_to_end(&mut stdout).unwrap();
        assert!(stdout.is_empty(), "TEN-3: serve stdout");
        let log = self.log();
        audit.diagnostics(log.as_bytes(), "serve stderr");
        audit.directory(data_dir);
        log
    }

    pub fn start(data_dir: &Path, listen: &str) -> Self {
        Self::start_command(&mut serve_command(data_dir, listen))
    }

    pub fn start_command(command: &mut Command) -> Self {
        let stdout = tempfile::tempfile().unwrap();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let log = Arc::<Mutex<Vec<String>>>::default();
        let kept = log.clone();
        let reader = std::thread::spawn(move || {
            for line in stderr.lines() {
                let line = line.expect("serve stderr is UTF-8");
                kept.lock().unwrap().push(line);
            }
        });
        Self {
            child,
            stdout,
            log,
            reader: Some(reader),
        }
    }

    pub fn log(&self) -> String {
        self.log.lock().unwrap().join("\n")
    }

    /// The URLs of the `listening` lines so far.
    fn listening(&self) -> Vec<String> {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["fields"]["message"] == "listening")
            .map(|line| line["fields"]["url"].as_str().expect("a URL").to_owned())
            .collect()
    }

    pub fn url(&self) -> String {
        until("listening", || self.listening().into_iter().next())
    }

    /// Waits until `/ready` answers 200.
    pub fn ready(&self) {
        let url = self.url();
        until("ready", || (get(&url, "/ready") == 200).then_some(()));
    }
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(started.elapsed() < LATE, "not {what} in time");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The status `GET <path>` answers, with no key.
fn get(url: &str, path: &str) -> u16 {
    let addr = url.strip_prefix("http://").expect("an http URL");
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(stream, "GET {path} HTTP/1.0\r\n\r\n").unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let status = answer.split(' ').nth(1).expect("a status");
    status.parse().unwrap()
}

/// What the RPC answers, as the JSON it sends.
fn post_json(url: &str, rpc: &str, key: &str, body: &str) -> Value {
    let addr = url.strip_prefix("http://").expect("an http URL");
    let mut stream = TcpStream::connect(addr).unwrap();
    let length = body.len();
    write!(
        stream,
        "POST /{rpc} HTTP/1.0\r\nauthorization: Bearer {key}\r\n\
         content-type: application/json\r\ncontent-length: {length}\r\n\r\n{body}"
    )
    .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").expect("an answer");
    assert!(head.starts_with("HTTP/1.0 200 "), "{head}");
    serde_json::from_str(body).unwrap()
}

#[derive(World)]
#[world(init = Self::new)]
pub struct Serve {
    served: Option<Served>,
    dir: TempDir,
    refused_start: Option<Output>,
    recovery: Option<Output>,
    taken: Option<TcpListener>,
    seed: Option<String>,
    control_before: Option<Vec<u8>>,
    start_environment: BTreeMap<&'static str, PathBuf>,
    /// The admin key's text.
    key: Option<String>,
    answer: Option<Result<WhoamiResponse, ConnectError>>,
    created: Option<Result<CreateKeyResponse, ConnectError>>,
    /// The client's key, where it is not the admin key, and the observer's.
    client_key: Option<String>,
    observer_key: Option<String>,
    emitted: Option<Result<EmitResponse, ConnectError>>,
    batch: Option<Result<EmitBatchResponse, ConnectError>>,
    event: Option<Event>,
    acknowledged: Vec<ironflowx_core::EventId>,
    signalled: Option<Instant>,
    audit: Audit,
}

impl fmt::Debug for Serve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Serve").finish_non_exhaustive()
    }
}

/// Sends every request with `query` in its URL.
#[derive(Clone)]
struct WithQuery(HttpClient, String);

impl ClientTransport for WithQuery {
    type ResponseBody = <HttpClient as ClientTransport>::ResponseBody;
    type Error = <HttpClient as ClientTransport>::Error;

    fn send(
        &self,
        mut request: http::Request<ClientBody>,
    ) -> BoxFuture<'static, Result<http::Response<Self::ResponseBody>, Self::Error>> {
        let uri = format!("{}?{}", request.uri(), self.1);
        *request.uri_mut() = uri.parse().unwrap();
        self.0.send(request)
    }
}

/// Sends every request with this body in place of its own.
#[derive(Clone)]
struct WithBody(HttpClient, Vec<u8>);

impl ClientTransport for WithBody {
    type ResponseBody = <HttpClient as ClientTransport>::ResponseBody;
    type Error = <HttpClient as ClientTransport>::Error;

    fn send(
        &self,
        request: http::Request<ClientBody>,
    ) -> BoxFuture<'static, Result<http::Response<Self::ResponseBody>, Self::Error>> {
        let body = full_body(self.1.clone().into());
        self.0.send(request.map(|_| body))
    }
}

/// Calls that present `key`.
fn bearer(key: &str) -> CallOptions {
    CallOptions::default().with_header("authorization", format!("Bearer {key}"))
}

impl Serve {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Not a setgid bit, which a directory made in it takes on Linux, as this one may have.
        std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o700)).unwrap();
        Self {
            served: None,
            refused_start: None,
            recovery: None,
            taken: None,
            seed: None,
            control_before: None,
            start_environment: BTreeMap::new(),
            dir,
            key: None,
            answer: None,
            created: None,
            client_key: None,
            observer_key: None,
            emitted: None,
            batch: None,
            event: None,
            acknowledged: Vec::new(),
            signalled: None,
            audit: Audit::default(),
        }
    }

    /// Not there until the host creates it.
    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn serve(&mut self, listen: &str) {
        self.start_unseeded(&mut serve_command(&self.data_dir(), listen));
    }

    fn start_unseeded(&mut self, cmd: &mut Command) {
        let publishes = !self.data_dir().join("control.db").exists();
        self.served = Some(Served::start_command(cmd));
        if publishes {
            self.served().ready();
            self.read_published_key();
        }
    }

    fn refused(&mut self, cmd: &mut Command) {
        let out = cmd.output().unwrap();
        assert!(!out.status.success(), "serve must refuse");
        self.audit.output(&out, None);
        self.audit.directory(&self.data_dir());
        self.refused_start = Some(out);
    }

    fn start_error(&self) -> (i32, Value) {
        let out = self.refused_start.as_ref().expect("a refusal");
        let log = std::str::from_utf8(&out.stderr).unwrap();
        let envelope = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        (out.status.code().unwrap(), envelope)
    }

    fn seed_file(&self) -> PathBuf {
        let file = self.dir.path().join("seed");
        std::fs::write(&file, format!(" {}\n", self.seed.as_deref().unwrap())).unwrap();
        file
    }

    fn served(&self) -> &Served {
        self.served.as_ref().expect("a served engine")
    }

    fn key(&self) -> &str {
        self.key.as_deref().expect("an admin key")
    }

    /// The key in `admin.key`.
    fn read_key(&mut self) {
        let text = std::fs::read_to_string(self.data_dir().join("admin.key")).unwrap();
        let key = text.strip_suffix('\n').expect("a newline").to_owned();
        assert_eq!(key.len(), 84, "one whole key");
        self.key = Some(key);
    }

    fn read_published_key(&mut self) {
        self.read_key();
        self.audit.file_key(self.key.as_deref().unwrap());
    }

    fn stop(&mut self) -> String {
        let data_dir = self.data_dir();
        self.served.take().unwrap().stop(&data_dir, &self.audit)
    }

    fn config(&self) -> ClientConfig {
        ClientConfig::new(self.served().url().parse().unwrap())
    }

    /// What a client of `protocol` is made of.
    fn transport(&self, protocol: &str) -> (HttpClient, ClientConfig) {
        match protocol {
            "Connect JSON" => (HttpClient::plaintext(), self.config().json()),
            "Connect binary" => (HttpClient::plaintext(), self.config().proto()),
            "gRPC" => (
                HttpClient::plaintext_http2_only(),
                self.config().with_protocol(Protocol::Grpc),
            ),
            "gRPC-Web" => (
                HttpClient::plaintext(),
                self.config().with_protocol(Protocol::GrpcWeb),
            ),
            _ => unreachable!(),
        }
    }

    fn events(&self, protocol: &str) -> EventServiceClient<HttpClient> {
        let (http, config) = self.transport(protocol);
        EventServiceClient::new(http, config)
    }

    fn client_key(&self) -> &str {
        self.client_key.as_deref().expect("a client")
    }

    /// Creates a key with the scopes `names` quotes, with the admin key; its text.
    async fn create(&mut self, names: &str) -> String {
        let client = Client::new(&self.served().url()).unwrap();
        let cx = Call::with_key(Secret::new(self.key().to_owned()));
        let scopes = names.split('"').skip(1).step_by(2);
        let request = ironflowx_proto::CreateKeyRequest {
            scopes: scopes.map(|s| s.parse().unwrap()).collect(),
            ..Default::default()
        };
        let created = client.create_key(&cx, request).await.unwrap();
        self.audit.remember(created.key.expose());
        created.key.expose().to_owned()
    }

    /// Why the client's call was refused.
    fn refusal(&self) -> &ConnectError {
        let emitted = self.emitted.as_ref().and_then(|a| a.as_ref().err());
        let batch = self.batch.as_ref().and_then(|a| a.as_ref().err());
        let created = self.created.as_ref().and_then(|a| a.as_ref().err());
        let error = emitted.or(batch).or(created).expect("a refused call");
        self.audit.connect_error(error);
        error
    }

    async fn whoami<T>(&mut self, client: IdentityServiceClient<T>, options: CallOptions)
    where
        T: ClientTransport,
        <T::ResponseBody as connectrpc::http_body::Body>::Error: fmt::Display,
    {
        let request = WhoamiRequest::default();
        let answer = client.whoami_with_options(request, options).await;
        if let Err(error) = &answer {
            self.audit.connect_error(error);
        }
        self.answer = Some(answer.map(connectrpc::UnaryResponse::into_owned));
    }

    fn answered(&self) -> &Result<WhoamiResponse, ConnectError> {
        self.answer.as_ref().expect("a call")
    }
}

#[given("a fresh data directory")]
fn fresh(w: &mut Serve) {
    assert!(!w.data_dir().exists());
}

/// Every start drops them.
#[given("no IRONFLOWX_ variable is set except those the scenario names")]
fn no_variables(_: &mut Serve) {}

#[when("the engine is served on the data directory")]
fn served(w: &mut Serve) {
    w.serve("127.0.0.1:0");
}

#[when("the engine is served on it")]
fn refused_start(w: &mut Serve) {
    w.refused(&mut serve_command(&w.data_dir(), "127.0.0.1:0"));
}

#[when(expr = "the engine is served on {string}")]
fn served_on(w: &mut Serve, listen: String) {
    w.serve(&listen);
}

#[given(r#"the engine is served and "admin.key" is still there"#)]
#[given("the engine is served and an admin key is known")]
#[given("the engine is served and a client holds the bootstrap key")]
fn served_with_key(w: &mut Serve) {
    w.serve("127.0.0.1:0");
    w.served().ready();
    w.read_key();
}

#[when("the engine is served and becomes ready")]
fn starts_and_becomes_ready(w: &mut Serve) {
    served(w);
    becomes_ready(w);
}

#[then(expr = "GET {string} answers 200 with no key")]
fn public_endpoint(w: &mut Serve, path: String) {
    assert_eq!(get(&w.served().url(), &path), 200);
}

#[then(regex = r#"^"ironflowx describe" lists neither$"#)]
fn no_health_operations(w: &mut Serve) {
    w.read_key();
    let out = command().arg("describe").output().unwrap();
    w.audit.output(&out, None);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let answer: Value = serde_json::from_slice(&out.stdout).unwrap();
    for op in answer["operations"].as_array().unwrap() {
        for field in ["command", "rpc"] {
            let name = op[field].as_str().unwrap().to_ascii_lowercase();
            assert!(
                !name.contains("health") && !name.contains("ready"),
                "{name}"
            );
        }
    }
}

#[then("the engine becomes ready")]
fn becomes_ready(w: &mut Serve) {
    w.served().ready();
}

fn mode(path: &Path) -> String {
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    format!("{:04o}", mode & 0o7777)
}

#[then(expr = "the data directory has mode {string}")]
fn directory_mode(w: &mut Serve, expected: String) {
    assert_eq!(mode(&w.data_dir()), expected);
}

#[then(expr = "{string} exists in the data directory with mode {string}")]
fn file_mode(w: &mut Serve, name: String, expected: String) {
    assert_eq!(mode(&w.data_dir().join(name)), expected);
}

#[then(expr = "the log names the path of {string} and does not contain the key")]
fn log_names(w: &mut Serve, name: String) {
    w.read_key();
    let path = w.data_dir().join(name);
    // As a log line holds it: a JSON string, its quotes included.
    let path = serde_json::to_string(&path.to_string_lossy()).unwrap();
    until("logged", || w.served().log().contains(&path).then_some(()));
    let log = w.served().log();
    w.audit.check(log.as_bytes(), "bootstrap log");
}

#[then(
    regex = r#"^with the key in "admin.key", whoami names scopes "read", "write", "admin" and "execute"$"#
)]
async fn key_scopes(w: &mut Serve) {
    w.read_key();
    let client = Client::new(&w.served().url()).unwrap();
    let cx = Call::with_key(Secret::new(w.key().to_owned()));
    let me = client.whoami(&cx, ironflowx_proto::WhoamiRequest {}).await;
    let me = me.unwrap();
    assert_eq!(me.key_id.to_string(), w.key()[4..40]);
    let all = [Scope::Read, Scope::Write, Scope::Admin, Scope::Execute];
    assert_eq!(me.scopes, all.into());
    assert_eq!(me.expires_at, None);
    // The log, once it is whole.
    w.stop();
}

#[then(regex = r#"^the log has one "listening" line whose URL answers GET "/health" with 200$"#)]
fn one_listening_line(w: &mut Serve) {
    w.served().ready();
    let urls = w.served().listening();
    assert_eq!(urls.len(), 1, "{}", w.served().log());
    assert!(!urls[0].ends_with(":0"), "{urls:?}");
    assert_eq!(get(&urls[0], "/health"), 200);
}

#[when(regex = "^whoami is called over Connect with (.+)$")]
async fn over_connect(w: &mut Serve, credential: String) {
    let key = w.key().to_owned();
    let header = |name: &str, value: String| CallOptions::default().with_header(name, value);
    let auth = |value: String| header("authorization", value);
    let options = match credential.as_str() {
        r#""Authorization: Bearer " and the admin key"# => auth(format!("Bearer {key}")),
        r#""Authorization: bearer " and the admin key"# => auth(format!("bearer {key}")),
        r#""Authorization: Basic " and the admin key"# => auth(format!("Basic {key}")),
        r#""Authorization: " and the admin key, no scheme"# => auth(key),
        r#""Authorization: Bearer" and nothing else"# => auth("Bearer".to_owned()),
        "no header" => CallOptions::default(),
        r#"the admin key in a header "X-Test-Credential""# => header("x-test-credential", key),
        r#"the admin key in a query parameter "token""# => {
            let transport = WithQuery(HttpClient::plaintext(), format!("token={key}"));
            let client = IdentityServiceClient::new(transport, w.config().json());
            return w.whoami(client, CallOptions::default()).await;
        }
        _ => panic!("unknown credential: {credential}"),
    };
    let client = IdentityServiceClient::new(HttpClient::plaintext(), w.config().json());
    w.whoami(client, options).await;
}

#[when(
    regex = "^whoami is called over (Connect JSON|Connect binary|gRPC|gRPC-Web) with the admin key$"
)]
async fn over_protocol(w: &mut Serve, protocol: String) {
    let (http, config) = w.transport(&protocol);
    let options = bearer(w.key());
    w.whoami(IdentityServiceClient::new(http, config), options)
        .await;
}

#[then("the answer names the admin key's ID")]
#[then("the call succeeds and names the admin key's ID")]
fn names_the_key(w: &mut Serve) {
    let answer = w.answered().as_ref().expect("the call succeeds");
    assert_eq!(answer.key_id, w.key()[4..40]);
}

#[then(expr = "the call is refused as unauthenticated with {string}")]
fn refused(w: &mut Serve, message: String) {
    let e = w.answered().as_ref().expect_err("the call is refused");
    assert_eq!(e.code, ErrorCode::Unauthenticated);
    let OpError {
        kind,
        message: said,
        ..
    } = op_error(e).expect("the engine's own error");
    assert_eq!((kind, said), (ErrorKind::Unauthenticated, message));
}

#[when(
    "the client creates a key over Connect whose expiry has 1 nanosecond past a whole millisecond"
)]
async fn too_fine(w: &mut Serve) {
    let mut request = CreateKeyRequest::default();
    let expiry = request.expires_at.get_or_insert_default();
    // Ahead of now by a day, so that nothing but its fraction is wrong.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    expiry.seconds = i64::try_from(now.as_secs()).unwrap() + 86_400;
    expiry.nanos = 1_000_001;
    let options = bearer(w.key());
    let client = KeyServiceClient::new(HttpClient::plaintext(), w.config().json());
    let answer = client.create_key_with_options(request, options).await;
    w.created = Some(answer.map(connectrpc::UnaryResponse::into_owned));
}

#[then("the call is refused as invalid")]
fn refused_as_invalid(w: &mut Serve) {
    let e = w.refusal();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    if w.created.is_some() {
        let e = op_error(e).expect("the door's own error");
        let message = "a time is in whole milliseconds and in years 0001 to 9999";
        assert_eq!((e.kind, &*e.message), (ErrorKind::Invalid, message));
    } else {
        // It did not decode, so the library refused it.
        assert_eq!(op_error(e), None);
        let message = "failed to decode proto request: invalid UTF-8 in string field";
        assert_eq!(e.message.as_deref(), Some(message));
    }
}

#[given(
    regex = r#"^the engine is served(?: and|,) a client holds a ("write"|"read"|"write" and "read") key(?: and an observer holds a ("read") key)?$"#
)]
async fn served_with_client(w: &mut Serve, client: String, observer: String) {
    served_with_key(w);
    w.client_key = Some(w.create(&client).await);
    if !observer.is_empty() {
        w.observer_key = Some(w.create(&observer).await);
    }
}

fn named(name: &str) -> EmitRequest {
    EmitRequest {
        name: name.to_owned(),
        ..EmitRequest::default()
    }
}

#[when("the client emits an event over Connect")]
async fn emits(w: &mut Serve) {
    let client = w.events("Connect JSON");
    let answer = client
        .emit_with_options(named("order.placed"), bearer(w.client_key()))
        .await;
    w.emitted = Some(answer.map(|a| a.into_owned().try_into().expect("an answer")));
}

#[then(regex = r#"^the call is refused as permission denied with "([^"]+)"$"#)]
fn permission_denied(w: &mut Serve, message: String) {
    let e = w.refusal();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    let e = op_error(e).expect("the engine's own error");
    assert_eq!((e.kind, e.message), (ErrorKind::PermissionDenied, message));
}

#[then(regex = r#"^the hint says a key with scope "(\w+)" is needed$"#)]
fn hint_names_scope(w: &mut Serve, scope: String) {
    let e = op_error(w.refusal()).expect("the engine's own error");
    let hint = format!("create a key with scope {scope}, or use one that has it");
    assert_eq!(e.hint, Some(hint));
}

#[then("the error is not retryable")]
fn not_retryable(w: &mut Serve) {
    let e = op_error(w.refusal()).expect("the engine's own error");
    assert!(!e.kind.retryable());
}

/// A data cell is the JSON itself, unless it starts `built:`.
fn data(cell: &str) -> &str {
    match cell {
        "built: an object spread over two lines" => "{\"b\": 1,\n \"a\": 2}",
        cell => {
            assert!(!cell.starts_with("built:"), "{cell}");
            cell
        }
    }
}

#[when(
    regex = r"^the client emits an event with data (.+) over (Connect JSON|Connect binary|gRPC|gRPC-Web)$"
)]
async fn emits_data(w: &mut Serve, cell: String, codec: String) {
    let request = EmitRequest {
        data: Some(data(&cell).to_owned()),
        ..named("order.placed")
    };
    let client = w.events(&codec);
    let answer = client
        .emit_with_options(request, bearer(w.client_key()))
        .await;
    w.emitted = Some(answer.map(|a| a.into_owned().try_into().expect("an answer")));
}

/// The emitted event, read over `codec`.
async fn read(w: &Serve, codec: &str) -> Event {
    let emitted = w.emitted.as_ref().expect("an emit");
    let request = GetEventRequest {
        event_id: emitted.as_ref().expect("an event").event_id.to_string(),
        ..GetEventRequest::default()
    };
    let client = w.events(codec);
    let answer = client
        .get_event_with_options(request, bearer(w.client_key()))
        .await;
    answer.expect("the event").into_owned()
}

#[when(regex = r"^reads it back over (Connect JSON|Connect binary|gRPC|gRPC-Web)$")]
async fn reads_back(w: &mut Serve, codec: String) {
    w.event = Some(read(w, &codec).await);
}

#[then(regex = r"^the data is byte for byte (.+)$")]
fn the_data_is(w: &mut Serve, cell: String) {
    let event = w.event.as_ref().expect("an event");
    assert_eq!(event.data.as_deref(), Some(data(&cell)));
}

#[then("over JSON the event's time is RFC 3339 in UTC, with no fraction finer than a millisecond")]
async fn the_time_is(w: &mut Serve) {
    // As it is sent: a client would read any spelling of the time.
    let emitted = w.emitted.as_ref().expect("an emit");
    let id = emitted.as_ref().expect("an event").event_id;
    let body = format!(r#"{{"event_id":"{id}"}}"#);
    let rpc = Operation::GetEvent.rpc();
    let event = post_json(&w.served().url(), rpc, w.client_key(), &body);
    let time = event["time"].as_str().expect("a time").to_owned();
    // 2027-01-15T08:00:00Z or 2027-01-15T08:00:00.123Z
    let shape: String = time
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    let whole = "0000-00-00T00:00:00Z";
    let millis = "0000-00-00T00:00:00.000Z";
    assert!([whole, millis].contains(&&*shape), "{time}");
}

#[when("the client sends an emit batch whose request body is 17 MiB")]
async fn sends_17_mib(w: &mut Serve) {
    let size = 17 * 1024 * 1024;
    // An event the engine would store, and spaces.
    let mut body = br#"{"items":[{"name":"a.one"}]}"#.to_vec();
    body.resize(size, b' ');
    // HTTP/1.1 closes the connection on a body it refuses, and can lose the answer with it.
    let http = HttpClient::plaintext_http2_only();
    let client = EventServiceClient::new(WithBody(http, body), w.config().json());
    let answer = client
        .emit_batch_with_options(EmitBatchRequest::default(), bearer(w.client_key()))
        .await;
    w.batch = Some(answer.map(|a| a.into_owned().try_into().expect("an answer")));
}

#[then("the call is refused as resource exhausted")]
fn resource_exhausted(w: &mut Serve) {
    let e = w.refusal();
    assert_eq!(e.code, ErrorCode::ResourceExhausted);
    // By the library, before the engine.
    assert_eq!(op_error(e), None);
    let message = e.message.as_deref();
    assert_eq!(message, Some("request body size exceeds limit 16777216"));
}

#[then("the observer lists no events")]
async fn observer_lists_none(w: &mut Serve) {
    let client = Client::new(&w.served().url()).unwrap();
    let key = w.observer_key.clone().expect("an observer");
    let request = ListEventsRequest {
        limit: None,
        cursor: None,
    };
    let page = client
        .list_events(&Call::with_key(Secret::new(key)), request)
        .await
        .unwrap();
    assert_eq!((page.events, page.next), (vec![], None));
}

#[when(
    regex = r"^the client sends an emit over Connect binary whose (name|idempotency key|data|metadata) holds the bytes FF FE$"
)]
async fn sends_bytes(w: &mut Serve, field: String) {
    let number = match field.as_str() {
        "name" => 1,
        "data" => 2,
        "metadata" => 3,
        _ => 5,
    };
    // A name, then the field, each a tag and a length before its bytes.
    let body = [b"\x0a\x05a.one", &[number << 3 | 2, 2, 0xff, 0xfe][..]].concat();
    let (http, config) = w.transport("Connect binary");
    let client = EventServiceClient::new(WithBody(http, body), config);
    let answer = client
        .emit_with_options(EmitRequest::default(), bearer(w.client_key()))
        .await;
    w.emitted = Some(answer.map(|a| a.into_owned().try_into().expect("an answer")));
}

#[when(
    regex = r#"^the client emits a batch over Connect of an event named "([^"]+)" and an event with no name$"#
)]
async fn emits_batch(w: &mut Serve, name: String) {
    let request = EmitBatchRequest {
        items: vec![named(&name), named("")],
        ..EmitBatchRequest::default()
    };
    let client = w.events("Connect JSON");
    let answer = client
        .emit_batch_with_options(request, bearer(w.client_key()))
        .await;
    w.batch = Some(answer.map(|a| a.into_owned().try_into().expect("an answer")));
}

fn results(w: &Serve) -> &[Result<EmitResponse, OpError>] {
    let batch = w.batch.as_ref().expect("a batch").as_ref();
    let items = &batch.expect("the batch succeeds").items;
    assert_eq!(items.len(), 2, "{items:?}");
    items
}

#[then("the first result is an event")]
fn first_is_event(w: &mut Serve) {
    let first = results(w)[0].as_ref().expect("an event");
    assert!(!first.duplicate);
}

#[then(regex = r#"^the second result is an error of kind invalid with message "([^"]+)"$"#)]
fn second_is_error(w: &mut Serve, message: String) {
    let refused = OpError {
        kind: ErrorKind::Invalid,
        message,
        hint: None,
    };
    assert_eq!(results(w)[1], Err(refused));
}

#[then(r#"with the key in "admin.key", listing keys shows no key with origin "recovered""#)]
#[then("listing keys shows only the bootstrap key")]
async fn only_the_bootstrap_key(w: &mut Serve) {
    let client = Client::new(&w.served().url()).unwrap();
    let cx = Call::with_key(Secret::new(w.key().to_owned()));
    let page = client.list_keys(&cx, ListKeysRequest::default()).await;
    let page = page.unwrap();
    let keys: Vec<_> = page.keys.iter().map(|k| (k.key_id, k.origin)).collect();
    let bootstrap = w.key()[4..40].parse().unwrap();
    assert_eq!(keys, [(bootstrap, KeyOrigin::Minted)]);
    assert_eq!(page.next, None);
}

pub fn serve_command(data_dir: &Path, listen: &str) -> Command {
    let mut cmd = command();
    cmd.args([
        "serve",
        "--log-level",
        "trace",
        "--listen",
        listen,
        "--data-dir",
    ])
    .arg(data_dir);
    cmd
}

fn new_seed(audit: &mut Audit) -> String {
    let out = command().args(["seed", "generate"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    audit.output(&out, Some(Delivery::SeedOrRecovery));
    assert!(out.stderr.is_empty(), "seed generation wrote stderr");
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    json["key"].as_str().unwrap().to_owned()
}

#[given("the engine was served once and stopped")]
fn was_served(w: &mut Serve) {
    served_with_key(w);
    w.stop();
}

#[given(r#""admin.key" was deleted"#)]
#[given(r#"the operator kept the key from "admin.key" and deleted the file"#)]
fn deleted_key(w: &mut Serve) {
    assert_eq!(w.key().len(), 84);
    std::fs::remove_file(w.data_dir().join("admin.key")).unwrap();
    w.audit.remove_file_key();
}

#[when("the engine is served again")]
fn served_again(w: &mut Serve) {
    w.serve("127.0.0.1:0");
}

#[then(r#""admin.key" does not exist"#)]
fn no_key_file(w: &mut Serve) {
    w.served().ready();
    assert!(!w.data_dir().join("admin.key").exists());
}

#[then("the kept key still authenticates")]
#[then(r#"the seed key authenticates with scopes "read", "write", "admin" and "execute""#)]
async fn authenticates(w: &mut Serve) {
    let client = Client::new(&w.served().url()).unwrap();
    let key = w.seed.as_deref().or(w.key.as_deref()).unwrap();
    let cx = Call::with_key(Secret::new(key.to_owned()));
    let me = client
        .whoami(&cx, ironflowx_proto::WhoamiRequest {})
        .await
        .unwrap();
    assert_eq!(me.key_id.to_string(), key[4..40]);
    assert_eq!(
        me.scopes,
        [Scope::Read, Scope::Write, Scope::Admin, Scope::Execute].into()
    );
}

#[given("the engine is served and its bootstrap key revoked itself")]
async fn revoked_bootstrap(w: &mut Serve) {
    served_with_key(w);
    let client = Client::new(&w.served().url()).unwrap();
    let cx = Call::with_key(Secret::new(w.key().to_owned()));
    let key_id = w.key()[4..40].to_owned();
    client
        .revoke_key(&cx, ironflowx_proto::RevokeKeyRequest { key_id })
        .await
        .unwrap();
}

#[when(r#""ironflowx recover-key" runs on the data directory"#)]
#[when(r#""ironflowx recover-key" runs on the data directory and no other flag"#)]
fn recover_to_file(w: &mut Serve) {
    recover(w, false);
}

#[when(r#""ironflowx recover-key" runs on the data directory and --stdout"#)]
fn recover_to_stdout(w: &mut Serve) {
    recover(w, true);
}

fn recover(w: &mut Serve, stdout: bool) {
    let mut cmd = command();
    cmd.args(["recover-key", "--data-dir"]).arg(w.data_dir());
    if stdout {
        cmd.arg("--stdout");
    }
    let out = cmd.output().unwrap();
    if out.status.success() && !stdout {
        w.read_published_key();
    }
    w.audit
        .output(&out, stdout.then_some(Delivery::SeedOrRecovery));
    w.recovery = Some(out);
}

#[then(r#""admin.key", with mode "0400", holds a key that authenticates with scopes "read", "write", "admin" and "execute""#)]
async fn recovered_file(w: &mut Serve) {
    check_recovery(w, false).await;
}

#[then(r#"the "key" field of its JSON output holds a key that authenticates with scopes "read", "write", "admin" and "execute""#)]
async fn recovered_stdout(w: &mut Serve) {
    check_recovery(w, true).await;
}

async fn check_recovery(w: &mut Serve, stdout: bool) {
    let out = w.recovery.take().unwrap();
    assert_eq!(out.status.code(), Some(0));
    if stdout {
        assert!(!w.data_dir().join("admin.key").exists());
        let json: Value = serde_json::from_slice(&out.stdout).expect("a JSON key object");
        let key = json["key"].as_str().expect("a key");
        assert_eq!(key.len(), 84);
        assert_eq!(json["key_id"], &key[4..40]);
        w.key = Some(key.to_owned());
    } else {
        assert!(out.stdout.is_empty());
        file_mode(w, "admin.key".into(), "0400".into());
    }
    authenticates(w).await;
}

#[then(r#"listing keys shows it with origin "recovered""#)]
async fn recovered_origin(w: &mut Serve) {
    let client = Client::new(&w.served().url()).unwrap();
    let cx = Call::with_key(Secret::new(w.key().to_owned()));
    let page = client
        .list_keys(&cx, ListKeysRequest::default())
        .await
        .unwrap();
    let keys: Vec<_> = page.keys.iter().map(|k| (k.key_id, k.origin)).collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(
        keys[0],
        (w.key()[4..40].parse().unwrap(), KeyOrigin::Recovered)
    );
    assert_eq!(keys[1].1, KeyOrigin::Minted);
    assert_eq!(page.next, None);
    w.stop();
}

#[then("it exits with code 2 and an envelope whose hint names --stdout")]
fn existing_recovery_file(w: &mut Serve) {
    let out = w.recovery.as_ref().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let json: Value = serde_json::from_slice(&out.stderr).expect("a stderr envelope");
    assert_eq!(json["code"], "invalid");
    assert_eq!(json["retryable"], false);
    assert!(json["hint"].as_str().unwrap().contains("--stdout"));
}

#[then(r#""admin.key" is unchanged"#)]
fn unchanged_key(w: &mut Serve) {
    let path = w.data_dir().join("admin.key");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.strip_suffix('\n') == Some(w.key()));
    assert_eq!(mode(&path), "0400");
}

#[given(r#"a seed key made with "ironflowx seed generate""#)]
#[given("a seed key A in a file named by --seed-file")]
fn generated_seed(w: &mut Serve) {
    w.seed = Some(new_seed(&mut w.audit));
}

#[when(regex = "^the engine is served with the seed given (.*)$")]
fn serve_seed(w: &mut Serve, source: String) {
    let mut cmd = serve_command(&w.data_dir(), "127.0.0.1:0");
    match source.as_str() {
        "in a file named by --seed-file" => {
            cmd.arg("--seed-file").arg(w.seed_file());
        }
        "in a file named by IRONFLOWX_SEED_FILE" => {
            cmd.env("IRONFLOWX_SEED_FILE", w.seed_file());
        }
        "as the value of IRONFLOWX_SEED" => {
            cmd.env("IRONFLOWX_SEED", w.seed.as_ref().unwrap());
        }
        _ => unreachable!(),
    }
    w.served = Some(Served::start_command(&mut cmd));
}

#[given("a seed key B as the value of IRONFLOWX_SEED")]
fn another_seed(w: &mut Serve) {
    w.client_key = Some(new_seed(&mut w.audit));
}

#[given(r#""not-a-key-but-secret" as the value of IRONFLOWX_SEED"#)]
fn malformed_seed(w: &mut Serve) {
    const SECRET: &str = "but-secret";
    let malformed = format!("not-a-key-{SECRET}");
    w.audit.presented(&Secret::new(malformed.clone()));
    w.audit.presented(&Secret::new(SECRET.to_owned()));
    w.client_key = Some(malformed);
}

#[when("the engine is served")]
fn start_with_inputs(w: &mut Serve) {
    let mut cmd = serve_command(&w.data_dir(), "127.0.0.1:0");
    if let Some(key) = &w.client_key {
        cmd.env("IRONFLOWX_SEED", key);
        if w.seed.is_some() {
            cmd.arg("--seed-file").arg(w.seed_file());
        }
        w.refused(&mut cmd);
    } else if w.taken.is_some() {
        let listen = w.taken.as_ref().unwrap().local_addr().unwrap().to_string();
        w.refused(&mut serve_command(&w.data_dir(), &listen));
    } else {
        cmd.envs(&w.start_environment);
        w.start_unseeded(&mut cmd);
    }
}

#[then(expr = "serve exits with code {int} and an envelope with code {string}")]
fn exit_kind(w: &mut Serve, code: i32, kind: String) {
    let (actual, envelope) = w.start_error();
    assert_eq!(actual, code);
    assert_eq!(envelope["code"], kind);
}

#[then(r#"serve exits with code 8 and an envelope with code "unavailable" and retryable true"#)]
fn unavailable(w: &mut Serve) {
    exit_kind(w, 8, "unavailable".into());
    assert_eq!(w.start_error().1["retryable"], true);
}

#[then("the data directory does not exist")]
fn no_directory(w: &mut Serve) {
    assert!(!w.data_dir().exists());
}

#[then("serve exits with code 2 and an envelope that names the key format")]
fn seed_format(w: &mut Serve) {
    exit_kind(w, 2, "invalid".into());
    assert!(
        w.start_error().1["message"]
            .as_str()
            .unwrap()
            .contains("ixk_<key-id>_<secret>")
    );
}

#[then(expr = "neither stderr nor stdout contains {string}")]
fn secret_is_absent(w: &mut Serve, secret: String) {
    let out = w.refused_start.as_ref().unwrap();
    for bytes in [&out.stderr, &out.stdout] {
        assert!(
            !std::str::from_utf8(bytes).unwrap().contains(&secret),
            "TEN-3: malformed seed in diagnostic"
        );
    }
}

#[given("another process listens on the address the engine is given")]
fn address_taken(w: &mut Serve) {
    w.taken = Some(TcpListener::bind("127.0.0.1:0").unwrap());
}

#[given("the engine is served on the data directory")]
fn first_engine(w: &mut Serve) {
    served_with_key(w);
}

#[when("a second engine is served on the same data directory and another address")]
fn second_engine(w: &mut Serve) {
    w.refused(&mut serve_command(&w.data_dir(), "127.0.0.1:0"));
}

#[then(r#"the second exits with code 8 and an envelope with code "unavailable""#)]
fn second_unavailable(w: &mut Serve) {
    exit_kind(w, 8, "unavailable".into());
}

#[then("the first still answers calls")]
async fn first_still_answers(w: &mut Serve) {
    authenticates(w).await;
}

#[given("a data directory whose control plane has another format version")]
async fn different_format(w: &mut Serve) {
    use ironflowx_operations::{Engine, HostClock};
    let dir = w.data_dir();
    let opened = Engine::open(&dir, HostClock, None).await.unwrap();
    opened.engine.close().await;
    std::fs::remove_dir_all(dir.join("ledgers")).unwrap();
    std::fs::create_dir(dir.join("ledgers")).unwrap();
    let control = dir.join("control.db");
    let db = rusqlite::Connection::open(&control).unwrap();
    db.execute_batch("UPDATE meta SET format = 1").unwrap();
    drop(db);
    w.control_before = Some(std::fs::read(control).unwrap());
}

#[then(
    r#"serve exits with code 1 and an envelope with code "internal" that names the format version"#
)]
fn format_refusal(w: &mut Serve) {
    exit_kind(w, 1, "internal".into());
    assert_eq!(
        w.start_error().1["message"],
        "the control plane is in another format version"
    );
}

#[then(r#""control.db" and "ledgers/" are unchanged"#)]
fn still_other_format(w: &mut Serve) {
    assert_eq!(
        std::fs::read(w.data_dir().join("control.db")).unwrap(),
        *w.control_before.as_ref().unwrap()
    );
    assert_eq!(
        std::fs::read_dir(w.data_dir().join("ledgers"))
            .unwrap()
            .count(),
        0
    );
}

#[then(regex = "^the log (does|does not) warn that keys travel in clear without a TLS proxy$")]
fn cleartext_warning(w: &mut Serve, expected: String) {
    w.served().ready();
    assert_eq!(
        w.served()
            .log()
            .contains("keys travel in clear without a TLS proxy"),
        expected == "does"
    );
}

#[given(r#"IRONFLOWX_LISEN is set to "127.0.0.1:0""#)]
fn unknown_setting(w: &mut Serve) {
    w.start_environment
        .insert("IRONFLOWX_LISEN", "127.0.0.1:0".into());
}

#[then("the log warns once that IRONFLOWX_LISEN is not a setting")]
fn setting_warning(w: &mut Serve) {
    w.served().ready();
    let log = w.stop();
    assert_eq!(log.matches("IRONFLOWX_LISEN is not a setting").count(), 1);
}

#[given("IRONFLOWX_DATA_DIR names directory A")]
fn directory_a(w: &mut Serve) {
    w.start_environment
        .insert("IRONFLOWX_DATA_DIR", w.dir.path().join("a"));
}

#[when("the engine is served with --data-dir naming directory B")]
fn directory_b(w: &mut Serve) {
    let mut cmd = serve_command(&w.data_dir(), "127.0.0.1:0");
    cmd.envs(&w.start_environment);
    w.start_unseeded(&mut cmd);
}

#[then("directory B holds the engine and directory A does not exist")]
fn only_directory_b(w: &mut Serve) {
    w.served().ready();
    assert!(w.data_dir().join("control.db").is_file());
    assert!(!w.dir.path().join("a").exists());
}

#[given(r#"a data directory holding only an "admin.key" file"#)]
fn stale_key(w: &mut Serve) {
    std::fs::create_dir(w.data_dir()).unwrap();
    std::fs::write(w.data_dir().join("admin.key"), b"previous installation").unwrap();
}

#[then(r#"serve exits with code 2 and an envelope that names "admin.key""#)]
fn stale_exit(w: &mut Serve) {
    exit_kind(w, 2, "invalid".into());
    assert!(
        w.start_error().1["message"]
            .as_str()
            .unwrap()
            .contains("admin.key")
    );
}

#[then(r#"the data directory holds only that "admin.key", unchanged"#)]
fn stale_kept(w: &mut Serve) {
    let data_dir = w.data_dir();
    assert_eq!(
        std::fs::read(data_dir.join("admin.key")).unwrap(),
        b"previous installation"
    );
    assert_eq!(std::fs::read_dir(data_dir).unwrap().count(), 1);
}

#[given(
    "the engine was served with seed key A, which created an admin key K and was revoked by K, and stopped"
)]
async fn revoked_seed(w: &mut Serve) {
    generated_seed(w);
    serve_seed(w, "as the value of IRONFLOWX_SEED".into());
    w.served().ready();
    let client = Client::new(&w.served().url()).unwrap();
    let seed = w.seed.as_ref().unwrap();
    let a = Call::with_key(Secret::new(seed.to_owned()));
    let k = client
        .create_key(
            &a,
            ironflowx_proto::CreateKeyRequest {
                scopes: [Scope::Admin].into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    w.audit.remember(k.key.expose());
    let k = Call::with_key(k.key);
    client
        .revoke_key(
            &k,
            ironflowx_proto::RevokeKeyRequest {
                key_id: seed[4..40].into(),
            },
        )
        .await
        .unwrap();
    w.stop();
}

#[when("the engine is served with seed key A")]
fn restart_with_seed(w: &mut Serve) {
    serve_seed(w, "as the value of IRONFLOWX_SEED".into());
}

#[then("the log warns that the seed is revoked")]
fn seed_warning(w: &mut Serve) {
    until("seed revoked", || {
        w.served()
            .log()
            .contains("the seed is revoked")
            .then_some(())
    });
}

#[then("key A is refused as an invalid key")]
async fn old_seed_refused(w: &mut Serve) {
    let client = Client::new(&w.served().url()).unwrap();
    let cx = Call::with_key(Secret::new(w.seed.as_ref().unwrap().to_owned()));
    let error = client
        .whoami(&cx, ironflowx_proto::WhoamiRequest {})
        .await
        .unwrap_err();
    w.audit.op_error(&error);
    assert_eq!(
        (error.kind, error.message.as_str()),
        (ErrorKind::Unauthenticated, "invalid API key")
    );
}

pub async fn run(parser: super::Unescaped) -> (bool, usize) {
    let writer = Serve::cucumber::<&str>()
        .with_parser(parser)
        .after(|_, _, _, _, world| {
            if let Some(world) = world
                && world.served.is_some()
            {
                world.stop();
            }
            Box::pin(async {})
        })
        .fail_on_skipped()
        .run(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/behaviors/serve.feature"
        ))
        .await;
    let passed = writer.scenarios_stats().passed;
    (writer.execution_has_failed(), passed)
}

#[given("the engine is served and a client emitted 100 events and saw each acknowledged")]
async fn acknowledged_events(w: &mut Serve) {
    served_with_key(w);
    let client = Client::new(&w.served().url()).unwrap();
    let call = Call::with_key(Secret::new(w.key().to_owned()));
    for _ in 0..100 {
        let event = client
            .emit(
                &call,
                ironflowx_proto::EmitRequest {
                    name: "order.placed".into(),
                    data: None,
                    metadata: None,
                    version: None,
                    idempotency_key: None,
                },
            )
            .await
            .unwrap();
        w.acknowledged.push(event.event_id);
    }
}

#[when("serve receives SIGTERM")]
fn signal(w: &mut Serve) {
    w.audit.directory(&w.data_dir());
    w.signalled = Some(Instant::now());
    kill_process(Pid::from_child(&w.served().child), Signal::TERM).unwrap();
}

#[then("serve exits with code 0 within 11 seconds")]
async fn stopped(w: &mut Serve) {
    let deadline = w.signalled.unwrap() + Duration::from_secs(11);
    let mut served = w.served.take().unwrap();
    loop {
        assert!(
            Instant::now() <= deadline,
            "serve did not stop within 11 seconds"
        );
        if let Some(status) = served.child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(0));
            served.finished(&w.data_dir(), &w.audit);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[then("after the engine is served again, all 100 events read back")]
async fn still_there(w: &mut Serve) {
    served_again(w);
    w.served().ready();
    let client = Client::new(&w.served().url()).unwrap();
    let call = Call::with_key(Secret::new(w.key().to_owned()));
    let page = client
        .list_events(
            &call,
            ListEventsRequest {
                limit: std::num::NonZeroU32::new(200),
                cursor: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.next, None);
    assert_eq!(
        page.events
            .into_iter()
            .map(|event| event.event_id)
            .collect::<Vec<_>>(),
        w.acknowledged.iter().rev().copied().collect::<Vec<_>>()
    );
}
