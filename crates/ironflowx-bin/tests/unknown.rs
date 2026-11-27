//! A state change that may have run, and did, through the proto client against a served engine:
//! never `Unavailable`, and no success (SRV-4). Its own test binary, since the fault hook is
//! process-wide.

use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironflowx_core::{ErrorKind, FaultPoint, Secret, set_fault_hook};
use ironflowx_doors::{Readiness, Slot, WaitingOn, router};
use ironflowx_operations::{Engine, HostClock, Opened};
use ironflowx_proto::{
    Call, Client, CreateKeyRequest, EmitBatchRequest, EmitRequest, KeyStatus, ListEventsRequest,
    ListKeysRequest, OpError, Operations as _, RevokeKeyRequest, WhoamiRequest,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// One test at a time reaches the fault points.
static ALONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Where the next call is held: it says so, then waits to be let go, or panics if it is never.
static BARRIER: Mutex<Option<Barrier>> = Mutex::new(None);

type Barrier = (FaultPoint, Sender<()>, Receiver<()>);

fn hold(point: FaultPoint) {
    let here = BARRIER.lock().unwrap().take_if(|(at, ..)| *at == point);
    if let Some((_, reached, go)) = here {
        reached.send(()).unwrap();
        go.recv().expect("let go");
    }
}

fn barrier(point: FaultPoint) -> (Receiver<()>, Sender<()>) {
    set_fault_hook(hold);
    let ((tell, reached), (go, gone)) = (channel(), channel());
    *BARRIER.lock().unwrap() = Some((point, tell, gone));
    (reached, go)
}

struct Ready;

impl Readiness for Ready {
    async fn ready(&self) -> Result<(), WaitingOn> {
        Ok(())
    }
}

/// An engine, its address, and its admin key.
struct Served {
    engine: Arc<Engine>,
    addr: SocketAddr,
    admin: Call,
    _dir: tempfile::TempDir,
}

async fn serve() -> Served {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), HostClock, None).await;
    let Opened { engine, minted, .. } = opened.unwrap();
    let engine = Arc::new(engine);
    let slot = Arc::new(Slot::starting());
    slot.open(engine.clone(), Ready);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(slot)).await });
    Served {
        engine,
        addr,
        admin: Call::with_key(Secret::new(minted.unwrap().expose().to_owned())),
        _dir: dir,
    }
}

fn client(addr: SocketAddr) -> Client {
    Client::new(&format!("http://{addr}")).unwrap()
}

fn event(name: &str, key: Option<&str>) -> EmitRequest {
    EmitRequest {
        name: name.into(),
        data: None,
        metadata: None,
        version: None,
        idempotency_key: key.map(str::to_owned),
    }
}

/// A state change, by what it leaves: events of a name, a key of a label, or that key revoked.
#[derive(Debug, Clone, Copy)]
enum Change {
    EmitKeyed,
    Emit,
    Batch,
    CreateKey,
    RevokeKey,
}

const CHANGES: [Change; 5] = [
    Change::EmitKeyed,
    Change::Emit,
    Change::Batch,
    Change::CreateKey,
    Change::RevokeKey,
];

impl Change {
    /// What the client's own `Unknown` hints.
    fn hint(self) -> &'static str {
        match self {
            Self::EmitKeyed => "retry with the same idempotency key",
            Self::Emit => "check first what landed: a retry may store a second event",
            Self::Batch => {
                "retry unchanged only if every item had an idempotency key and keeps it; \
                 else check first what landed"
            }
            Self::CreateKey => {
                "the key may be stored: list the keys before the retry, and again after the \
                 engine reopens; revoke the one no holder has"
            }
            Self::RevokeKey => {
                "retry: revoking a revoked key succeeds, and one that revoked itself is refused"
            }
        }
    }

    /// Asks for it, of `name`, through `client`.
    async fn ask(self, served: &Served, client: &Client, name: &str) -> Result<(), OpError> {
        let cx = &served.admin;
        match self {
            Self::EmitKeyed => client.emit(cx, event(name, Some(name))).await.map(drop),
            Self::Emit => client.emit(cx, event(name, None)).await.map(drop),
            Self::Batch => {
                let items = vec![event(name, None)];
                let batch = EmitBatchRequest { items };
                client.emit_batch(cx, batch).await.map(drop)
            }
            Self::CreateKey => client.create_key(cx, labelled(name)).await.map(drop),
            Self::RevokeKey => {
                let key = served.engine.create_key(cx, labelled(name)).await;
                let key_id = key.unwrap().info.key_id.to_string();
                client.revoke_key(cx, RevokeKeyRequest { key_id }).await
            }
        }
    }

    /// How often it happened.
    async fn landed(self, served: &Served, name: &str) -> usize {
        let cx = &served.admin;
        match self {
            Self::EmitKeyed | Self::Emit | Self::Batch => {
                let all = ListEventsRequest {
                    limit: None,
                    cursor: None,
                };
                let page = served.engine.list_events(cx, all).await.unwrap();
                page.events.iter().filter(|e| e.name == name).count()
            }
            Self::CreateKey | Self::RevokeKey => {
                let page = served.engine.list_keys(cx, ListKeysRequest::default());
                let keys = page.await.unwrap().keys.into_iter();
                let status = match self {
                    Self::RevokeKey => KeyStatus::Revoked,
                    _ => KeyStatus::Active,
                };
                keys.filter(|k| k.label.as_deref() == Some(name) && k.status == status)
                    .count()
            }
        }
    }
}

fn labelled(name: &str) -> CreateKeyRequest {
    CreateKeyRequest {
        label: Some(name.into()),
        ..CreateKeyRequest::default()
    }
}

fn unknown(message: &str, hint: &str) -> OpError {
    OpError {
        kind: ErrorKind::Unknown,
        message: message.into(),
        hint: Some(hint.into()),
    }
}

/// What `stream` sends until its HTTP/1.1 head is whole, and where that ends.
async fn head(stream: &mut TcpStream) -> (Vec<u8>, usize) {
    let mut read = Vec::new();
    loop {
        if let Some(end) = read.windows(4).position(|w| w == b"\r\n\r\n") {
            return (read, end + 4);
        }
        assert_ne!(stream.read_buf(&mut read).await.unwrap(), 0);
    }
}

/// The whole of a request that says how long its body is, or that has none.
async fn request(stream: &mut TcpStream) -> Vec<u8> {
    let (mut read, end) = head(stream).await;
    let text = String::from_utf8_lossy(&read[..end]).to_lowercase();
    let length = text.split("\r\ncontent-length: ").nth(1);
    let length: usize = length.map_or(0, |l| l.lines().next().unwrap().parse().unwrap());
    while read.len() < end + length {
        assert_ne!(stream.read_buf(&mut read).await.unwrap(), 0);
    }
    read
}

/// A proxy that takes each call to `engine`, which answers it, and answers `instead`.
async fn proxy(engine: SocketAddr, instead: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut caller, _)) = listener.accept().await {
            let request = request(&mut caller).await;
            let mut engine = TcpStream::connect(engine).await.unwrap();
            engine.write_all(&request).await.unwrap();
            let (answer, _) = head(&mut engine).await;
            assert!(answer.starts_with(b"HTTP/1.1 200 "));
            caller.write_all(instead).await.unwrap();
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_whose_answer_a_proxy_loses_is_unknown() {
    let _alone = ALONE.lock().await;
    let served = serve().await;
    let html = b"HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/html\r\n\
                 content-length: 6\r\n\r\n<html>";
    let json = b"HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\n\
                 content-length: 22\r\n\r\n{\"code\":\"unavailable\"}";
    let refusal = b"HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\n\
                    content-length: 30\r\n\r\n{\"code\":\"failed_precondition\"}";
    for (i, (instead, why)) in [
        (
            html.as_slice(),
            "the engine did not answer: an HTTP 503 that is not Connect's",
        ),
        (json, "the engine did not answer: unavailable"),
        // A refusal's code, which a server of Connect's does not send so.
        (
            refusal,
            "the engine did not answer: an HTTP 503 that is not Connect's",
        ),
        // Dropped after the request.
        (b"", "the engine did not answer: unavailable"),
    ]
    .into_iter()
    .enumerate()
    {
        let lost = client(proxy(served.addr, instead).await);
        for change in CHANGES {
            let name = format!("{change:?}-{i}");
            let e = change.ask(&served, &lost, &name).await.unwrap_err();
            let ended = (e.kind, e.hint.as_deref());
            let expected = (ErrorKind::Unknown, Some(change.hint()));
            assert_eq!(ended, expected, "{name}");
            assert_eq!(e.message, why);
            assert_eq!(change.landed(&served, &name).await, 1, "{name}");
        }
        let e = lost.whoami(&served.admin, WhoamiRequest {}).await;
        let e = e.unwrap_err();
        assert_eq!((e.kind, e.hint), (ErrorKind::Unavailable, None));
    }
}

/// The commit is held past the deadline the engine and the client both have, then goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_that_outlives_its_calls_deadline_is_unknown() {
    let _alone = ALONE.lock().await;
    let served = serve().await;
    let hasty = client(served.addr).with_timeout(Duration::from_secs(2));
    for change in CHANGES {
        let name = format!("{change:?}");
        let (reached, go) = barrier(match change {
            Change::EmitKeyed | Change::Emit | Change::Batch => FaultPoint::BeforeAppend,
            Change::CreateKey => FaultPoint::BeforeCreateCommit,
            Change::RevokeKey => FaultPoint::BeforeRevokeCommit,
        });
        let e = change.ask(&served, &hasty, &name).await.unwrap_err();
        reached.try_recv().expect("the commit is held");
        let ended = (e.kind, e.hint.as_deref());
        assert_eq!(ended, (ErrorKind::Unknown, Some(change.hint())), "{name}");
        // A read waits for the control plane, which the key change holds.
        if matches!(change, Change::CreateKey | Change::RevokeKey) {
            let e = hasty.whoami(&served.admin, WhoamiRequest {}).await;
            let e = e.unwrap_err();
            assert_eq!((e.kind, e.hint), (ErrorKind::Unavailable, None));
        }
        go.send(()).unwrap();
        for tries in 0.. {
            match change.landed(&served, &name).await {
                0 if tries < 1_000 => tokio::time::sleep(Duration::from_millis(10)).await,
                landed => {
                    assert_eq!(landed, 1, "{name}");
                    break;
                }
            }
        }
    }
}

/// The engine's own `Unknown`: its message and its hint cross the wire as they are.
#[tokio::test(flavor = "multi_thread")]
async fn the_engines_unknown_crosses_the_wire_as_it_is() {
    let _alone = ALONE.lock().await;
    for (change, hint) in [
        (
            Change::EmitKeyed,
            "reopen the engine, then retry with the same idempotency key",
        ),
        (
            Change::Emit,
            "reopen the engine, then check first what landed: a retry may store a second event",
        ),
        (
            Change::Batch,
            "reopen the engine, then retry unchanged only if every item had an idempotency key \
             and keeps it; else check first what landed",
        ),
    ] {
        let served = serve().await;
        let (_reached, go) = barrier(FaultPoint::AfterFsync);
        // Never let go: the writer panics at the point, and the append's outcome is lost.
        drop(go);
        let e = change.ask(&served, &client(served.addr), "a").await;
        assert_eq!(e, Err(unknown("the outcome is unknown", hint)));
    }
}
