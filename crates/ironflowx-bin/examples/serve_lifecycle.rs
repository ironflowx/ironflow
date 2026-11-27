//! Runs the host in child processes with local filesystem and fault-hook fixtures.

use std::cell::RefCell;
use std::io::{self, BufRead as _, BufReader, Read as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use connectrpc::{ConnectError, ErrorCode, Protocol};
use hyper::Request;
use hyper::client::conn::http1::{SendRequest, handshake};
use hyper_util::rt::TokioIo;
use ironflowx_core::{ErrorKind, FaultPoint, Scope, Secret, set_fault_hook};
use ironflowx_proto::{
    Call, Client, CreateKeyRequest, EmitRequest, ListEnvironmentsRequest, ListEventsRequest,
    ListKeysRequest, OpError, Operations as _, WhoamiRequest, wire,
};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const LATE: Duration = Duration::from_secs(20);

static FIXTURE: OnceLock<(String, PathBuf)> = OnceLock::new();

fn fault(point: FaultPoint) {
    let (mode, root) = FIXTURE.get().unwrap();
    match (mode.as_str(), point) {
        ("open", FaultPoint::BeforeRename)
        | ("idle-fault" | "late-fault", FaultPoint::AfterAck)
        | ("late-fault" | "timeout", FaultPoint::BeforeCreateCommit)
        | ("timeout", FaultPoint::AfterFsync) => {
            tracing::info!(point = point.name(), "barrier");
            let deadline = Instant::now() + LATE;
            while !root.join(point.name()).exists() {
                assert!(
                    Instant::now() < deadline,
                    "fixture did not release {point:?}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        ("timeout", FaultPoint::AfterCreateCommit) => {
            tracing::info!(point = point.name(), "barrier");
        }
        ("writer-panic", FaultPoint::BeforeAppend) => panic!("fixture panic before append"),
        _ => {}
    }
}

fn host(mode: String, root: PathBuf) -> ExitCode {
    // A failed assertion aborts the fixture driver, so its child must also watch the pipe.
    std::thread::spawn(|| {
        let _ = io::stdin().read_exact(&mut [0]);
        std::process::exit(1);
    });
    tracing_subscriber::fmt()
        .json()
        .with_writer(io::stderr)
        .init();
    if mode == "handler-panic" {
        ironflowx_bin::panic_in_handler();
    }
    FIXTURE.set((mode, root.clone())).unwrap();
    set_fault_hook(fault);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(ironflowx_bin::serve(
        &root.join("data"),
        "127.0.0.1:0".parse().unwrap(),
    ));
    runtime.shutdown_background();
    if let Err(error) = result {
        tracing::error!(exit_code = error.exit_code(), "serve failed");
        return error.exit_code().into();
    }
    ExitCode::SUCCESS
}

struct Host {
    child: Child,
    log: Receiver<String>,
    history: RefCell<Vec<String>>,
    addr: SocketAddr,
    root: PathBuf,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Host {
    fn start(root: &Path, mode: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg(mode)
            .arg(root)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (record, log) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if record.send(line).is_err() {
                    break;
                }
            }
        });
        let mut host = Host {
            child,
            log,
            history: RefCell::default(),
            addr: "127.0.0.1:0".parse().unwrap(),
            root: root.to_owned(),
        };
        let line = host.until(|fields| fields["message"] == "listening");
        host.addr = line["url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap();
        host
    }

    fn until(&self, found: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + LATE;
        loop {
            let line = self
                .log
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("expected host log");
            let parsed = serde_json::from_str::<Value>(&line);
            self.history.borrow_mut().push(line);
            if let Ok(line) = parsed
                && found(&line["fields"])
            {
                return line["fields"].clone();
            }
        }
    }

    fn barrier(&self, point: FaultPoint) {
        self.until(|fields| fields["message"] == "barrier" && fields["point"] == point.name());
    }

    fn release(&self, point: FaultPoint) {
        std::fs::write(self.root.join(point.name()), []).unwrap();
    }

    fn signal(&self, signal: Signal) {
        kill_process(Pid::from_child(&self.child), signal).unwrap();
    }

    fn exit(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            assert!(Instant::now() < deadline, "host did not exit in {limit:?}");
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn key_file(&self) -> PathBuf {
        self.root.join("data/admin.key")
    }

    async fn ready(&self) {
        let deadline = Instant::now() + LATE;
        loop {
            if Connection::to(self.addr).await.get("/ready").await.0 == 200 {
                return;
            }
            assert!(Instant::now() < deadline, "host did not become ready");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn client(&self) -> (Client, Call) {
        let client = Client::new(&format!("http://{}", self.addr)).unwrap();
        let key = std::fs::read_to_string(self.key_file()).unwrap();
        (
            client,
            Call::with_key(Secret::new(key.trim_end().to_owned())),
        )
    }

    fn lines(&self) -> Vec<String> {
        self.history
            .borrow()
            .iter()
            .cloned()
            .chain(self.log.iter())
            .collect()
    }
}

/// Keeps a single transport, so a refusal proves that a live connection is still served.
struct Connection(SendRequest<Body>);

impl Connection {
    async fn to(addr: SocketAddr) -> Self {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (send, connection) = handshake(TokioIo::new(stream)).await.unwrap();
        tokio::spawn(connection);
        Self(send)
    }

    async fn get(&mut self, path: &str) -> (u16, String) {
        let request = Request::get(path)
            .header("host", "fixture")
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    async fn whoami(&mut self, call: &Call) -> (u16, String) {
        let request = Request::post("/ironflowx.v1.IdentityService/Whoami")
            .header("host", "fixture")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", call.key().unwrap().expose()),
            )
            .body(Body::from("{}"))
            .unwrap();
        self.send(request).await
    }

    async fn send(&mut self, request: Request<Body>) -> (u16, String) {
        tokio::time::timeout(LATE, async {
            self.0.ready().await.unwrap();
            let response = self.0.send_request(request).await.unwrap();
            let status = response.status().as_u16();
            let body = to_bytes(Body::new(response.into_body()), 4096)
                .await
                .unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        })
        .await
        .expect("fixture HTTP response")
    }
}

/// A request is in the door once Hyper asks for the body. Leave the body unsent.
async fn held_body(addr: SocketAddr) -> tokio::net::TcpStream {
    tokio::time::timeout(LATE, async {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"POST /ironflowx.v1.IdentityService/Whoami HTTP/1.1\r\nHost: fixture\r\n\
              Content-Type: application/json\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n",
            )
            .await
            .unwrap();
        let interim = b"HTTP/1.1 100 Continue\r\n\r\n";
        let mut bytes = [0; 25];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, interim);
        stream
    })
    .await
    .expect("the request reached the door")
}

async fn body_cancelled(mut stream: tokio::net::TcpStream) -> Instant {
    let mut byte = [0];
    assert_eq!(
        stream.read(&mut byte).await.unwrap(),
        0,
        "the held body got a response"
    );
    Instant::now()
}

const fn latest() -> ListEventsRequest {
    ListEventsRequest {
        limit: None,
        cursor: None,
    }
}

fn event() -> EmitRequest {
    EmitRequest {
        name: "order.placed".into(),
        data: Some(br#"{"fixture":true}"#.to_vec()),
        metadata: None,
        version: None,
        idempotency_key: None,
    }
}

fn key_request() -> CreateKeyRequest {
    CreateKeyRequest {
        scopes: [Scope::Read].into(),
        expires_at: None,
        label: Some("held transaction".into()),
    }
}

fn emit(
    host: &Host,
) -> tokio::task::JoinHandle<Result<ironflowx_proto::EmitResponse, ironflowx_proto::OpError>> {
    let (client, call) = host.client();
    tokio::spawn(async move { client.emit(&call, event()).await })
}

async fn timed<T>(answer: impl Future<Output = T>) -> (Instant, T) {
    let answer = answer.await;
    (Instant::now(), answer)
}

async fn hold_key(
    host: &Host,
) -> tokio::task::JoinHandle<(Instant, Result<ironflowx_proto::CreatedKey, OpError>)> {
    let (client, call) = host.client();
    let request = tokio::spawn(timed(async move {
        client.create_key(&call, key_request()).await
    }));
    host.barrier(FaultPoint::BeforeCreateCommit);
    request
}

async fn during_open() {
    for twice in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut host = Host::start(root.path(), "open");
        host.barrier(FaultPoint::BeforeRename);
        let mut connection = Connection::to(host.addr).await;
        assert_eq!(connection.get("/health").await.0, 200);
        let body = if twice {
            None
        } else {
            Some(held_body(host.addr).await)
        };
        let start = Instant::now();
        host.signal(Signal::TERM);
        host.until(|fields| fields["message"] == "stopping");
        assert_eq!(host.child.try_wait().unwrap(), None);
        assert!(!host.key_file().exists());
        if twice {
            host.signal(Signal::INT);
            assert_eq!(host.exit(Duration::from_secs(2)).code(), Some(1));
            assert!(!host.key_file().exists());
        } else {
            let deadline = tokio::time::Instant::from_std(start) + Duration::from_secs(12);
            let ended = tokio::time::timeout_at(deadline, body_cancelled(body.unwrap()))
                .await
                .expect("transport drained while open was held");
            assert!(ended.duration_since(start) >= Duration::from_secs(10));
            assert_eq!(host.child.try_wait().unwrap(), None);
            assert!(!host.key_file().exists());
            host.release(FaultPoint::BeforeRename);
            assert_eq!(host.exit(LATE).code(), Some(0));
            let published = std::fs::read_to_string(host.key_file())
                .expect("the stopping host published its minted key");
            let restarted = Host::start(root.path(), "normal");
            restarted.ready().await;
            let (client, _) = restarted.client();
            let call = Call::with_key(Secret::new(published.trim_end().to_owned()));
            client.whoami(&call, WhoamiRequest {}).await.unwrap();
        }
    }
}

async fn read_db(host: &Host) -> rusqlite::Connection {
    let (client, call) = host.client();
    let env = client
        .list_environments(&call, ListEnvironmentsRequest {})
        .await
        .unwrap()[0]
        .env;
    rusqlite::Connection::open(
        host.root
            .join("data/ledgers")
            .join(env.id().to_string())
            .join("read.db"),
    )
    .unwrap()
}

async fn ledger_fault(late: bool) {
    let root = tempfile::tempdir().unwrap();
    let mut host = Host::start(root.path(), if late { "late-fault" } else { "idle-fault" });
    host.ready().await;
    let db = read_db(&host).await;
    db.execute_batch(
        "CREATE TRIGGER fixture BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'fixture'); END",
    )
    .unwrap();
    let emitted = emit(&host);
    host.barrier(FaultPoint::AfterAck);
    let event_id = emitted.await.unwrap().unwrap().event_id;
    if late {
        let _key = hold_key(&host).await;
        host.signal(Signal::TERM);
        host.until(|fields| fields["message"] == "stopping");
        host.release(FaultPoint::AfterAck);
        // The held key call keeps the host from closing while the fold fails.
        host.until(|fields| fields["message"] == "ledger fault during drain");
        host.release(FaultPoint::BeforeCreateCommit);
    } else {
        host.release(FaultPoint::AfterAck);
    }
    assert_eq!(host.exit(LATE).code(), Some(8));
    db.execute_batch("DROP TRIGGER fixture").unwrap();
    drop(db);
    let restarted = Host::start(root.path(), "normal");
    restarted.ready().await;
    let (client, call) = restarted.client();
    let page = client.list_events(&call, latest()).await.unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.event_id)
            .collect::<Vec<_>>(),
        [event_id]
    );
}

async fn drain_at_limit() {
    let root = tempfile::tempdir().unwrap();
    let mut host = Host::start(root.path(), "timeout");
    host.ready().await;
    let (_, call) = host.client();
    let mut connection = Connection::to(host.addr).await;
    assert_eq!(connection.get("/health").await.0, 200);
    let grpc = wire::EventServiceClient::new(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new(format!("http://{}", host.addr).parse().unwrap())
            .with_protocol(Protocol::Grpc),
    );
    let options = CallOptions::default().with_header(
        "authorization",
        format!("Bearer {}", call.key().unwrap().expose()),
    );
    let emitted = tokio::spawn(timed(async move {
        grpc.emit_with_options(wire::EmitRequest::try_from(event()).unwrap(), options)
            .await
    }));
    host.barrier(FaultPoint::AfterFsync);
    let created = hold_key(&host).await;
    let pending_body = tokio::spawn(body_cancelled(held_body(host.addr).await));
    let start = Instant::now();
    host.signal(Signal::TERM);
    host.until(|fields| fields["message"] == "stopping");
    assert!(TcpStream::connect(host.addr).is_err());
    let (status, body) = connection.whoami(&call).await;
    assert_eq!(status, 503);
    let body: Value = serde_json::from_str(&body).unwrap();
    let mut error = ConnectError::new(ErrorCode::Unavailable, body["message"].as_str().unwrap());
    assert_eq!(body["code"], "unavailable");
    error.details = serde_json::from_value(body["details"].clone()).unwrap();
    let refused = ironflowx_proto::wire::op_error(&error).unwrap();
    assert_eq!(refused.kind, ErrorKind::Unavailable);
    assert_eq!(refused.hint.as_deref(), Some("the engine is stopping"));
    assert_eq!(
        connection.get("/ready").await,
        (503, r#"{"ready":false,"waiting_on":"open"}"#.into()),
    );
    let line = host.until(|fields| fields["operation"] == "ironflowx.v1.IdentityService/Whoami");
    assert_eq!(line["outcome"], "unavailable");
    assert!(!line.as_object().unwrap().contains_key("key_id"));
    assert!(start.elapsed() < Duration::from_secs(10));
    let upper = tokio::time::Instant::from_std(start) + Duration::from_secs(12);
    let (emit_ended, emitted) = tokio::time::timeout_at(upper, emitted)
        .await
        .unwrap()
        .unwrap();
    let Err(error) = emitted else {
        panic!("the HTTP/2 stream was not aborted");
    };
    assert_eq!(wire::op_error(&error), None);
    let (create_ended, created) = tokio::time::timeout_at(upper, created)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(created, Err(e) if e.kind == ErrorKind::Unknown));
    let ended = tokio::time::timeout_at(upper, pending_body)
        .await
        .expect("the held body was aborted")
        .unwrap();
    for (call, ended) in [
        ("HTTP/2 emit", emit_ended),
        ("create key", create_ended),
        ("request body", ended),
    ] {
        let elapsed = ended.duration_since(start);
        assert!(
            (Duration::from_secs(10)..Duration::from_secs(12)).contains(&elapsed),
            "{call} ended after {elapsed:?}",
        );
    }
    host.release(FaultPoint::BeforeCreateCommit);
    host.barrier(FaultPoint::AfterCreateCommit);
    host.release(FaultPoint::AfterFsync);
    assert_eq!(host.exit(LATE).code(), Some(0));
    let records: Vec<Value> = host
        .lines()
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|line| line["fields"].clone())
        .filter(|fields| fields["message"] == "call")
        .collect();
    for (operation, outcome) in [
        ("ironflowx.v1.EventService/Emit", &["cancelled"][..]),
        ("ironflowx.v1.KeyService/CreateKey", &["cancelled"][..]),
        (
            "ironflowx.v1.IdentityService/Whoami",
            &["unavailable", "cancelled"][..],
        ),
    ] {
        let actual = records
            .iter()
            .filter(|fields| fields["operation"] == operation)
            .map(|fields| fields["outcome"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, outcome, "{operation}");
    }
    let restarted = Host::start(root.path(), "normal");
    restarted.ready().await;
    let (client, call) = restarted.client();
    let page = client.list_events(&call, latest()).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].data, event().data.unwrap());
    let page = client
        .list_keys(&call, ListKeysRequest::default())
        .await
        .unwrap();
    assert_eq!(page.keys.len(), 2);
    let created = page
        .keys
        .iter()
        .find(|key| key.label.as_deref() == Some("held transaction"))
        .unwrap();
    assert_eq!(created.scopes, [Scope::Read].into());
}

async fn control_failure() {
    let root = tempfile::tempdir().unwrap();
    let mut host = Host::start(root.path(), "normal");
    host.ready().await;
    let control = rusqlite::Connection::open(root.path().join("data/control.db")).unwrap();
    control
        .execute_batch("ALTER TABLE environments RENAME TO fixture")
        .unwrap();
    let mut connection = Connection::to(host.addr).await;
    assert_eq!(
        connection.get("/ready").await,
        (
            503,
            r#"{"ready":false,"waiting_on":"control_plane"}"#.into()
        )
    );
    assert_eq!(connection.get("/health").await.0, 200);
    let (client, call) = host.client();
    let failure = client.whoami(&call, WhoamiRequest {}).await.unwrap_err();
    assert_eq!(failure.kind, ErrorKind::Unavailable);
    control
        .execute_batch("ALTER TABLE fixture RENAME TO environments")
        .unwrap();
    client.whoami(&call, WhoamiRequest {}).await.unwrap();
    assert_eq!(
        connection.get("/ready").await,
        (200, r#"{"ready":true}"#.into())
    );
    host.signal(Signal::INT);
    assert_eq!(host.exit(LATE).code(), Some(0));
}

async fn aborts_on_panic(mode: &str) {
    let root = tempfile::tempdir().unwrap();
    let mut host = Host::start(root.path(), mode);
    host.ready().await;
    let (client, call) = host.client();
    let panic = match mode {
        "handler-panic" => {
            client.whoami(&call, WhoamiRequest {}).await.unwrap_err();
            "fixture panic in whoami handler"
        }
        "writer-panic" => {
            client.emit(&call, event()).await.unwrap_err();
            "fixture panic before append"
        }
        "damaged-read" => {
            client.emit(&call, event()).await.unwrap();
            client.list_events(&call, latest()).await.unwrap();
            let db = read_db(&host).await;
            assert_eq!(
                db.execute("UPDATE events SET data = x'FFFE'", []).unwrap(),
                1
            );
            client.list_events(&call, latest()).await.unwrap_err();
            "a stored event is not UTF-8 (LED-5)"
        }
        _ => unreachable!(),
    };
    assert_eq!(host.exit(LATE).signal(), Some(Signal::ABORT.as_raw()));
    let logs = host.lines().join("\n");
    assert!(logs.contains(panic), "{logs}");
    assert!(!logs.contains("serve failed"), "{logs}");
    if mode == "damaged-read" {
        let mut restarted = Host::start(root.path(), "normal");
        restarted.ready().await;
        let (client, call) = restarted.client();
        client.list_events(&call, latest()).await.unwrap_err();
        assert_eq!(restarted.exit(LATE).signal(), Some(Signal::ABORT.as_raw()));
        assert!(restarted.lines().iter().any(|line| line.contains(panic)));
        // Repair the fixture row to confirm the rest of the data directory opens cleanly.
        read_db_path(&root)
            .execute("UPDATE events SET data = ?1", [event().data.unwrap()])
            .unwrap();
    }
    let restarted = Host::start(root.path(), "normal");
    restarted.ready().await;
    let (client, call) = restarted.client();
    client.whoami(&call, WhoamiRequest {}).await.unwrap();
    let page = client.list_events(&call, latest()).await.unwrap();
    assert_eq!(page.events.len(), usize::from(mode == "damaged-read"));
    println!("{mode}: host aborted and reopened");
}

fn read_db_path(root: &tempfile::TempDir) -> rusqlite::Connection {
    let ledgers = root.path().join("data/ledgers");
    let path = std::fs::read_dir(ledgers)
        .unwrap()
        .map(|e| e.unwrap().path().join("read.db"))
        .find(|p| p.is_file())
        .unwrap();
    rusqlite::Connection::open(path).unwrap()
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if let (Some(mode), Some(root)) = (args.next(), args.next()) {
        return host(mode.into_string().unwrap(), root.into());
    }
    if cfg!(panic = "unwind") {
        eprintln!("serve fixture must use the abort panic strategy");
        return ExitCode::FAILURE;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        during_open().await;
        ledger_fault(false).await;
        ledger_fault(true).await;
        drain_at_limit().await;
        control_failure().await;
        for mode in ["handler-panic", "writer-panic", "damaged-read"] {
            aborts_on_panic(mode).await;
        }
    });
    ExitCode::SUCCESS
}
