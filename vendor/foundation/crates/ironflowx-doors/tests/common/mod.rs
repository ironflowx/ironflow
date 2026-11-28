//! A door in front of a recording `Operations`.

use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use ironflowx_core::{
    Address, ErrorKind, KeyId, KeyOrigin, Lease, RunId, Scope, Secret, Timestamp,
};
use ironflowx_doors::{Readiness, Slot, WaitingOn, router};
use ironflowx_proto::{
    BeginStepRequest, BeginStepResponse, Call, Claim, ClaimRunRequest, CompleteRunRequest,
    CompleteStepRequest, CompleteStepResponse, CreateKeyRequest, CreatedKey, EmitBatchRequest,
    EmitBatchResponse, EmitRequest, EmitResponse, Environment, Event, EventPage, Function,
    GetEventRequest, GetFunctionRequest, GetRunRequest, KeyInfo, KeyPage, KeyStatus,
    ListEnvironmentsRequest, ListEventsRequest, ListKeysRequest, OpError, Operations,
    PutFunctionRequest, PutFunctionResponse, RevokeKeyRequest, Run, RunEnd, RunStatus,
    StartRunRequest, StartRunResponse, Whoami, WhoamiRequest,
};
use tokio::net::TcpListener;

pub const KEY: &str =
    "ixk_0190b5f0-0000-7000-8000-000000000001_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

pub fn whoami() -> Whoami {
    Whoami {
        key_id: Some(KEY[4..40].parse().unwrap()),
        scopes: [Scope::Read, Scope::Admin].into(),
        environment: Address::new("default", "main"),
        expires_at: Some(Timestamp::from_millis(1_800_000_000_123)),
    }
}

/// Each kind once, in no order that matters.
pub const KINDS: [ErrorKind; 9] = [
    ErrorKind::Invalid,
    ErrorKind::Unauthenticated,
    ErrorKind::PermissionDenied,
    ErrorKind::NotFound,
    ErrorKind::Unknown,
    ErrorKind::ResourceExhausted,
    ErrorKind::Unavailable,
    ErrorKind::Internal,
    ErrorKind::Conflict,
];

const EVENT_ID: &str = "0190b5f0-0000-7000-8000-000000000005";

pub fn emitted() -> EmitResponse {
    EmitResponse {
        event_id: EVENT_ID.parse().unwrap(),
        duplicate: false,
    }
}

/// What a batch of any items answers: a new event, a duplicate, and each kind of error, with a hint
/// and without.
pub fn batch() -> EmitBatchResponse {
    let duplicate = EmitResponse {
        duplicate: true,
        ..emitted()
    };
    let mut items = vec![Ok(emitted()), Ok(duplicate)];
    for kind in KINDS {
        for hint in [None, Some(String::new()), Some("do\nthis".to_owned())] {
            items.push(Err(OpError {
                kind,
                message: "réfusé % \"x\"".into(),
                hint,
            }));
        }
    }
    EmitBatchResponse { items }
}

/// Data and metadata that any JSON encoder but a copy would change.
pub const DATA: &str =
    "\t{ \"b\": 1.50,\r\n \"a\": [9007199254740993, \"\\u00e9é\\\\\\\"\", 1e2] } ";
pub const METADATA: &str = "{\"z\":\"\u{7f}\u{2028}😀\\ud83d\\ude00\\/\" , \"a\":-0}";

pub fn event() -> Event {
    Event {
        event_id: EVENT_ID.parse().unwrap(),
        name: "commande.passée".into(),
        data: DATA.into(),
        metadata: Some(METADATA.into()),
        version: i64::MAX,
        idempotency_key: Some("clé\u{1}".into()),
        time: Timestamp::from_millis(1_700_000_000_120),
        emitted_by: format!("key:{}", &KEY[4..40]).parse().unwrap(),
    }
}

pub fn event_page() -> EventPage {
    let plain = Event {
        data: "null".into(),
        metadata: None,
        version: 1,
        idempotency_key: None,
        emitted_by: "embedder:a b".parse().unwrap(),
        ..event()
    };
    EventPage {
        events: vec![event(), plain],
        next: Some("0123.7".into()),
    }
}

/// The key a `create_key` answers.
pub const CREATED: &str =
    "ixk_0190b5f0-0000-7000-8000-000000000002_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";

pub fn environments() -> Vec<Environment> {
    vec![Environment {
        env: "0190b5f0-0000-7000-8000-000000000003.00112233445566778899aabbccddeeff"
            .parse()
            .unwrap(),
        name: "main".into(),
        address: Address::new("default", "main"),
        project_id: "0190b5f0-0000-7000-8000-000000000004".parse().unwrap(),
        created_at: Timestamp::from_millis(1_700_000_000_000),
    }]
}

pub fn created() -> CreatedKey {
    CreatedKey {
        key: Secret::new(CREATED.to_owned()),
        info: KeyInfo {
            key_id: CREATED[4..40].parse().unwrap(),
            label: Some("agent é".into()),
            scopes: [Scope::Read, Scope::Write].into(),
            origin: KeyOrigin::Created,
            created_by: Some(KEY[4..40].parse::<KeyId>().unwrap().into()),
            created_at: Timestamp::from_millis(1_700_000_000_001),
            expires_at: Some(Timestamp::from_millis(1_800_000_000_120)),
            status: KeyStatus::Active,
        },
    }
}

pub fn key_page() -> KeyPage {
    let bootstrap = KeyInfo {
        key_id: KEY[4..40].parse().unwrap(),
        label: None,
        scopes: [Scope::Admin].into(),
        origin: KeyOrigin::Minted,
        created_by: None,
        created_at: Timestamp::from_millis(0),
        expires_at: None,
        status: KeyStatus::Revoked,
    };
    KeyPage {
        keys: vec![created().info, bootstrap],
        next: Some("0123.7".into()),
    }
}

const RUN_ID: &str = "0190b5f0-0000-7000-8000-000000000006";

fn run_id() -> RunId {
    RUN_ID.parse().unwrap()
}

pub fn function() -> Function {
    Function {
        name: "charge".into(),
        code: "v1.2_b-3".into(),
        triggers: vec!["commande.passée".into(), "order.paid".into()],
        max_attempts: Some(u32::MAX),
        first_delay_seconds: None,
        max_delay_seconds: Some(0),
        put_at: Timestamp::from_millis(1_700_000_000_002),
        put_by: "embedder:a b".parse().unwrap(),
    }
}

pub fn put() -> PutFunctionResponse {
    PutFunctionResponse {
        function: function(),
        changed: true,
    }
}

pub fn started() -> StartRunResponse {
    StartRunResponse {
        run_id: run_id(),
        duplicate: false,
    }
}

pub fn run() -> Run {
    Run {
        run_id: run_id(),
        function: "charge".into(),
        code: "v1".into(),
        status: RunStatus::Completed,
        input: Some(DATA.into()),
        created_at: Timestamp::from_millis(1_700_000_000_003),
        created_by: format!("key:{}", &KEY[4..40]).parse().unwrap(),
        due_at: Some(Timestamp::from_millis(1_700_000_000_004)),
        execution: u32::MAX,
        lease_until: Some(Timestamp::from_millis(1_700_000_000_005)),
        steps: 1_000,
        step: Some("réserve\u{1}".into()),
        output: Some(METADATA.into()),
        ended_at: Some(Timestamp::from_millis(1_700_000_000_006)),
    }
}

pub fn claim() -> Option<Claim> {
    Some(Claim {
        lease: Lease {
            run: run_id(),
            execution: 7,
        },
        function: "charge".into(),
        code: "v1".into(),
        input: Some(DATA.into()),
        lease_until: Timestamp::from_millis(1_700_000_000_007),
        renew_after_seconds: 20,
    })
}

pub fn begun() -> BeginStepResponse {
    BeginStepResponse::Completed {
        output: DATA.into(),
    }
}

pub fn step_output() -> CompleteStepResponse {
    CompleteStepResponse::Completed {
        output: METADATA.into(),
    }
}

pub fn run_end() -> RunEnd {
    RunEnd {
        run_id: run_id(),
        status: RunStatus::Failed,
    }
}

/// Records each call's key and request, then answers the error it was set to, or what its
/// operation's function above does, a `whoami` what it was set to.
pub struct Recording {
    pub keys: Mutex<Vec<Option<String>>>,
    /// Each as it prints.
    pub requests: Mutex<Vec<String>>,
    pub answer: Mutex<Result<Whoami, OpError>>,
    /// What a `get_event` answers instead.
    pub event: Mutex<Option<Event>>,
    /// What a `list_events` answers instead.
    pub page: Mutex<Option<EventPage>>,
    /// Whether a call waits for good instead.
    pub hangs: AtomicBool,
}

impl Recording {
    pub fn answering(answer: Result<Whoami, OpError>) -> Arc<Self> {
        Arc::new(Self {
            keys: Mutex::default(),
            requests: Mutex::default(),
            answer: Mutex::new(answer),
            event: Mutex::default(),
            page: Mutex::default(),
            hangs: AtomicBool::new(false),
        })
    }

    /// The keys of the calls since the last time this was asked.
    pub fn take(&self) -> Vec<Option<String>> {
        std::mem::take(&mut self.keys.lock().unwrap())
    }

    /// Their requests, likewise.
    pub fn take_requests(&self) -> Vec<String> {
        std::mem::take(&mut self.requests.lock().unwrap())
    }

    async fn record(&self, cx: &Call, req: impl fmt::Debug) -> Result<Whoami, OpError> {
        let key = cx.key().map(|k| k.expose().to_owned());
        self.keys.lock().unwrap().push(key);
        self.requests.lock().unwrap().push(format!("{req:?}"));
        if self.hangs.load(Relaxed) {
            std::future::pending::<()>().await;
        }
        self.answer.lock().unwrap().clone()
    }
}

impl Operations for Recording {
    async fn whoami(&self, cx: &Call, req: WhoamiRequest) -> Result<Whoami, OpError> {
        self.record(cx, req).await
    }

    async fn list_environments(
        &self,
        cx: &Call,
        req: ListEnvironmentsRequest,
    ) -> Result<Vec<Environment>, OpError> {
        self.record(cx, req).await.map(|_| environments())
    }

    async fn create_key(&self, cx: &Call, req: CreateKeyRequest) -> Result<CreatedKey, OpError> {
        self.record(cx, req).await.map(|_| created())
    }

    async fn list_keys(&self, cx: &Call, req: ListKeysRequest) -> Result<KeyPage, OpError> {
        self.record(cx, req).await.map(|_| key_page())
    }

    async fn revoke_key(&self, cx: &Call, req: RevokeKeyRequest) -> Result<(), OpError> {
        self.record(cx, req).await.map(|_| ())
    }

    async fn emit(&self, cx: &Call, req: EmitRequest) -> Result<EmitResponse, OpError> {
        self.record(cx, req).await.map(|_| emitted())
    }

    async fn emit_batch(
        &self,
        cx: &Call,
        req: EmitBatchRequest,
    ) -> Result<EmitBatchResponse, OpError> {
        let none = req.items.is_empty();
        let answer = self.record(cx, req).await;
        answer.map(|_| match none {
            true => EmitBatchResponse { items: vec![] },
            false => batch(),
        })
    }

    async fn get_event(&self, cx: &Call, req: GetEventRequest) -> Result<Event, OpError> {
        let held = self.event.lock().unwrap().clone();
        let answer = self.record(cx, req).await;
        answer.map(|_| held.unwrap_or_else(event))
    }

    async fn list_events(&self, cx: &Call, req: ListEventsRequest) -> Result<EventPage, OpError> {
        let page = self.page.lock().unwrap().clone();
        let answer = self.record(cx, req).await;
        answer.map(|_| page.unwrap_or_else(event_page))
    }

    async fn put_function(
        &self,
        cx: &Call,
        req: PutFunctionRequest,
    ) -> Result<PutFunctionResponse, OpError> {
        self.record(cx, req).await.map(|_| put())
    }

    async fn get_function(&self, cx: &Call, req: GetFunctionRequest) -> Result<Function, OpError> {
        self.record(cx, req).await.map(|_| function())
    }

    async fn start_run(
        &self,
        cx: &Call,
        req: StartRunRequest,
    ) -> Result<StartRunResponse, OpError> {
        self.record(cx, req).await.map(|_| started())
    }

    async fn get_run(&self, cx: &Call, req: GetRunRequest) -> Result<Run, OpError> {
        self.record(cx, req).await.map(|_| run())
    }

    async fn claim_run(&self, cx: &Call, req: ClaimRunRequest) -> Result<Option<Claim>, OpError> {
        self.record(cx, req).await.map(|_| claim())
    }

    async fn begin_step(
        &self,
        cx: &Call,
        req: BeginStepRequest,
    ) -> Result<BeginStepResponse, OpError> {
        self.record(cx, req).await.map(|_| begun())
    }

    async fn complete_step(
        &self,
        cx: &Call,
        req: CompleteStepRequest,
    ) -> Result<CompleteStepResponse, OpError> {
        self.record(cx, req).await.map(|_| step_output())
    }

    async fn complete_run(&self, cx: &Call, req: CompleteRunRequest) -> Result<RunEnd, OpError> {
        self.record(cx, req).await.map(|_| run_end())
    }
}

pub struct Ready(pub Arc<Mutex<Result<(), WaitingOn>>>);

impl Readiness for Ready {
    async fn ready(&self) -> Result<(), WaitingOn> {
        *self.0.lock().unwrap()
    }
}

pub type TestSlot = Slot<Recording, Ready>;

/// Serves the door of `slot` on a port of its own.
pub async fn serve(slot: Arc<TestSlot>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(slot)).await });
    addr
}

/// Serves the door of an open slot that answers `answer`.
pub async fn serving(answer: Result<Whoami, OpError>) -> (SocketAddr, Arc<Recording>) {
    let operations = Recording::answering(answer);
    let slot = Arc::new(Slot::starting());
    slot.open(operations.clone(), Ready(Arc::new(Mutex::new(Ok(())))));
    (serve(slot).await, operations)
}
