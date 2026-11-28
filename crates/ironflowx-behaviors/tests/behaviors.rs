//! The behavior scenarios in `docs/behaviors/`, and foundation's `events.feature` and
//! `tenancy.feature`, vendored, each file on its own World: the `ironflowx` binary on a temporary
//! directory (D20). Steps call `Operations` or the CLI, never a URL, but for the binary's `/health`
//! and `/ready`.

#[path = "behaviors/adapter.rs"]
mod adapter;
#[path = "behaviors/cli.rs"]
mod cli;
#[path = "behaviors/output.rs"]
mod output;
#[path = "behaviors/serve.rs"]
mod serve;
#[path = "behaviors/tenancy.rs"]
mod tenancy;

use std::collections::BTreeSet;
use std::fmt;

use adapter::{Adapter, BINARIES, runs_remotely};
use cucumber::feature::Ext as _;
use cucumber::gherkin::{Feature, GherkinEnv};
use cucumber::writer::Stats as _;
use cucumber::{World, given, then, when};
use ironflowx_cell::{Ledger, ManualClock, new_env_ref};
use ironflowx_core::{EnvRef, ErrorKind, EventId, Scope, Secret, SubmitAs, Timestamp};
use ironflowx_events::{Emit, Submission};
use ironflowx_operations::Engine;
use ironflowx_proto::{
    Call, CreateKeyRequest, EmitBatchRequest, EmitBatchResponse, EmitRequest, EmitResponse, Event,
    EventPage, GetEventRequest, ListEnvironmentsRequest, ListEventsRequest, OpError, Operations,
    WhoamiRequest,
};
use tempfile::TempDir;

const DAY: i64 = 86_400_000;

#[derive(World)]
#[world(init = Self::new)]
struct Env {
    /// Whether the environment is the binary's.
    remote: bool,
    // Before `dir`: fields drop in order, and the engine must go before its directory.
    ops: Option<Adapter>,
    /// The bootstrap key, as the fresh environment's open returned it, or as `admin.key` holds it.
    bootstrap: Option<Secret>,
    /// The client's call, holding the bootstrap key.
    client: Option<Call>,
    /// Another client's call, holding a key of its own.
    other: Option<Call>,
    dir: TempDir,
    clock: ManualClock,
    emits: Vec<Result<EmitResponse, OpError>>,
    /// The data bytes of the last emit.
    sent: Option<Vec<u8>>,
    read: Option<Result<Event, OpError>>,
    batch: Option<Result<EmitBatchResponse, OpError>>,
    page: Option<Result<EventPage, OpError>>,
    /// Events emitted by a "N events" step, oldest first, and the size of the first page.
    originals: Vec<EventId>,
    first_page: usize,
    cursor: Option<String>,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("emits", &self.emits)
            .field("batch", &self.batch)
            .field("page", &self.page)
            .finish_non_exhaustive()
    }
}

fn request(name: impl Into<String>) -> EmitRequest {
    EmitRequest {
        name: name.into(),
        data: None,
        metadata: None,
        version: None,
        idempotency_key: None,
    }
}

fn all_scopes() -> BTreeSet<Scope> {
    BTreeSet::from([Scope::Read, Scope::Write, Scope::Admin, Scope::Execute])
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            clock: ManualClock::new(Timestamp::from_millis(1_800_000_000_000)),
            remote: false,
            ops: None,
            bootstrap: None,
            client: None,
            other: None,
            emits: Vec::new(),
            sent: None,
            read: None,
            batch: None,
            page: None,
            originals: Vec::new(),
            first_page: 0,
            cursor: None,
        }
    }

    fn ops(&self) -> &Adapter {
        self.ops.as_ref().expect("a fresh environment")
    }

    fn client(&self) -> &Call {
        self.client.as_ref().expect("a client")
    }

    /// Whoever a step names: the other client, which holds another key with all four scopes,
    /// or the client.
    async fn reader(&self, other: &str) -> &Call {
        if other.is_empty() {
            return self.client();
        }
        let cx = self.other.as_ref().expect("another client");
        let mine = self.ops().whoami(self.client(), WhoamiRequest {}).await;
        let theirs = self.ops().whoami(cx, WhoamiRequest {}).await.unwrap();
        assert_ne!(
            theirs.key_id,
            mine.unwrap().key_id,
            "the other client holds a key of its own"
        );
        assert_eq!(theirs.scopes, all_scopes());
        cx
    }

    async fn emit(&mut self, req: EmitRequest) -> Result<EmitResponse, OpError> {
        self.sent = req.data.clone();
        let r = self.ops().emit(self.client(), req).await;
        self.emits.push(r.clone());
        r
    }

    async fn emit_n(&mut self, n: usize) -> Vec<EventId> {
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(self.emit(request(format!("e.{i}"))).await.unwrap().event_id);
        }
        ids
    }

    async fn batch(&mut self, items: Vec<EmitRequest>) {
        let r = self
            .ops()
            .emit_batch(self.client(), EmitBatchRequest { items })
            .await;
        self.batch = Some(r);
    }

    async fn list(&self, limit: Option<u32>, cursor: Option<String>) -> Result<EventPage, OpError> {
        self.list_as(self.client(), limit, cursor).await
    }

    async fn list_as(
        &self,
        cx: &Call,
        limit: Option<u32>,
        cursor: Option<String>,
    ) -> Result<EventPage, OpError> {
        let limit = limit.map(|l| l.try_into().unwrap());
        let req = ListEventsRequest { limit, cursor };
        self.ops().list_events(cx, req).await
    }

    async fn get(&self, id: String) -> Result<Event, OpError> {
        self.get_as(self.client(), id).await
    }

    async fn get_as(&self, cx: &Call, event_id: String) -> Result<Event, OpError> {
        self.ops().get_event(cx, GetEventRequest { event_id }).await
    }

    /// Every stored event, newest first.
    async fn stored(&self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.list(Some(200), cursor).await.unwrap();
            events.extend(page.events);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return events,
            }
        }
    }

    async fn stored_names(&self) -> Vec<String> {
        self.stored().await.into_iter().map(|e| e.name).collect()
    }

    fn last_emit(&self) -> &Result<EmitResponse, OpError> {
        self.emits.last().expect("an emit")
    }

    fn last_id(&self) -> EventId {
        self.last_emit()
            .as_ref()
            .expect("the emit succeeded")
            .event_id
    }

    fn page(&self) -> &EventPage {
        self.page
            .as_ref()
            .expect("a list")
            .as_ref()
            .expect("the list succeeded")
    }

    fn batch_items(&self) -> &[Result<EmitResponse, OpError>] {
        &self
            .batch
            .as_ref()
            .expect("a batch")
            .as_ref()
            .expect("the batch succeeded")
            .items
    }
}

// ---- Cells ----

fn bytes_count(s: &str) -> usize {
    s.replace(',', "").parse().unwrap()
}

/// A quoted cell is a JSON string literal; a described one is built.
fn text(cell: &str) -> String {
    if cell.starts_with('"') {
        return serde_json::from_str(cell).unwrap_or_else(|e| panic!("{cell}: {e}"));
    }
    let words: Vec<_> = cell.split(' ').collect();
    match words.as_slice() {
        ["a", "name" | "key", "of", n, "ASCII", "letters"] => "a".repeat(n.parse().unwrap()),
        ["a", "name", "of", n, quoted] => text(quoted).repeat(n.parse().unwrap()),
        _ => panic!("unknown text: {cell}"),
    }
}

/// A data cell is the JSON sent, byte for byte, unless it starts `built:`.
fn data(cell: &str) -> Vec<u8> {
    let Some(built) = cell.strip_prefix("built: ") else {
        return cell.as_bytes().to_vec();
    };
    if let Some(n) = built.strip_suffix(" nested arrays") {
        let n = n.parse().unwrap();
        return ["[".repeat(n), "]".repeat(n)].concat().into_bytes();
    }
    match built {
        "a string holding the byte 0xFF" => b"\"\xff\"".to_vec(),
        r#"an object whose "t" is a string holding the byte 0xFF"# => b"{\"t\": \"\xff\"}".to_vec(),
        _ => panic!("unknown built data: {cell}"),
    }
}

/// Fills `<ASCII a repeated>` in `template` so that it is exactly `size` bytes.
fn sized(template: &str, size: usize) -> Vec<u8> {
    let hole = "<ASCII a repeated>";
    let fill = size - (template.len() - hole.len());
    template.replace(hole, &"a".repeat(fill)).into_bytes()
}

/// Splits a leading JSON string literal off `s`.
fn quoted(s: &str) -> (String, &str) {
    let mut stream = serde_json::Deserializer::from_str(s).into_iter::<String>();
    let value = stream.next().unwrap().unwrap();
    (value, &s[stream.byte_offset()..])
}

/// Parses the phrase after "the client emits event ": a name and what follows it. Returns the
/// request and how many times to send it.
fn emit_phrase(phrase: &str) -> (EmitRequest, usize) {
    if !phrase.starts_with('"') {
        return (request(text(phrase)), 1);
    }
    let (name, rest) = quoted(phrase);
    let mut req = request(name);
    let mut rest = rest.strip_suffix(" again").unwrap_or(rest);
    if let Some(i) = rest
        .rfind(" with idempotency key ")
        .or(rest.rfind(" and idempotency key "))
    {
        let key = rest[i..].split_once(" key ").unwrap().1;
        req.idempotency_key = Some(text(key));
        rest = &rest[..i];
    }
    if rest == " twice without an idempotency key" {
        return (req, 2);
    }
    if let Some((part, cell)) = rest
        .strip_prefix(" whose ")
        .and_then(|r| r.split_once(" is "))
    {
        match part {
            "data" => req.data = Some(data(cell)),
            "metadata" => req.metadata = Some(data(cell)),
            "idempotency key" => req.idempotency_key = Some(text(cell)),
            _ => panic!("unknown part: {part}"),
        }
    } else if rest == " with no data" || rest.is_empty() {
    } else if let Some(v) = rest.strip_prefix(" with version ") {
        req.version = Some(v.parse().unwrap());
    } else if let Some(v) = rest.strip_prefix(" with data value ") {
        req.data = Some(data(v));
    } else if let Some(r) = rest.strip_prefix(" with metadata ") {
        let (template, size) = r.split_once(" sent as exactly ").unwrap();
        req.metadata = Some(sized(
            template,
            bytes_count(size.trim_end_matches(" bytes")),
        ));
    } else if let Some(r) = rest.strip_prefix(" with data ") {
        req.data = Some(
            if let Some((template, size)) = r.split_once(" sent as exactly ") {
                sized(template, bytes_count(size.trim_end_matches(" bytes")))
            } else if let Some((template, size)) = r.split_once(" padded with spaces to exactly ") {
                let size = bytes_count(size.trim_end_matches(" bytes"));
                let mut d = sized(template, size - 16);
                d.extend_from_slice(&[b' '; 16]);
                d
            } else if let Some((d, metadata)) = r.split_once(", metadata ") {
                req.metadata = Some(data(metadata));
                data(d)
            } else {
                data(r)
            },
        );
    } else {
        panic!("unknown emit: {rest}");
    }
    (req, 1)
}

fn quoted_list(s: &str) -> Vec<String> {
    s.split(['"'])
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

fn refused(r: &Result<impl fmt::Debug, OpError>, kind: ErrorKind) -> &str {
    match r {
        Err(e) if e.kind == kind => {
            assert_eq!(e.hint, None, "a rule's refusal carries no hint");
            &e.message
        }
        other => panic!("expected {kind:?}, got {other:?}"),
    }
}

// ---- Background ----

#[given("a fresh environment")]
async fn fresh(w: &mut Env) {
    if w.remote {
        let (served, key) = Adapter::served(&w.dir.path().join("data")).await;
        w.bootstrap = Some(Secret::new(key));
        w.ops = Some(served);
        return;
    }
    let opened = Engine::open(w.dir.path(), w.clock.clone(), None).await;
    let opened = opened.unwrap();
    w.bootstrap = opened.minted;
    w.ops = Some(Adapter::Engine(opened.engine));
}

#[given("a client")]
fn client(w: &mut Env) {
    let key = w.bootstrap.take().expect("the bootstrap key");
    w.client = Some(Call::with_key(key));
}

#[given("another client")]
async fn another_client(w: &mut Env) {
    let req = CreateKeyRequest {
        scopes: all_scopes(),
        ..CreateKeyRequest::default()
    };
    let created = w.ops().create_key(w.client(), req).await.unwrap();
    w.other = Some(Call::with_key(created.key));
}

// ---- Emit ----

#[when(regex = r"^the client emits (event .*|an event with no name)$")]
async fn emits(w: &mut Env, phrase: String) {
    emitted(w, phrase, String::new()).await;
}

#[given(regex = r"^the client emitted (event .*?)(?: (\d+) days ago)?$")]
async fn emitted(w: &mut Env, phrase: String, days: String) {
    let (req, times) = match phrase.strip_prefix("event ") {
        Some(p) => emit_phrase(p),
        None => (request(""), 1),
    };
    for _ in 0..times {
        let _ = w.emit(req.clone()).await;
    }
    if let Ok(days) = days.parse::<i64>() {
        w.clock.step(days * DAY);
    }
}

#[given(regex = r#"^the client emitted events (.*) in that order$"#)]
async fn emitted_names(w: &mut Env, names: String) {
    for name in quoted_list(&names) {
        w.emit(request(name)).await.unwrap();
    }
}

#[given(regex = r"^the client emitted (\d+) events$")]
async fn emitted_n(w: &mut Env, n: usize) {
    w.originals = w.emit_n(n).await;
}

#[then("the emit succeeds with a UUID event ID")]
fn succeeds_with_uuid(w: &mut Env) {
    let id = w.last_id().to_string();
    assert_eq!((id.len(), id.matches('-').count()), (36, 4), "{id}");
}

#[then("the emit succeeds")]
fn succeeds(w: &mut Env) {
    w.last_id();
}

#[then(
    regex = r#"^the emit is refused as invalid(?: with (".*")|, and the message contains (".*"))?$"#
)]
fn emit_refused(w: &mut Env, exact: String, contains: String) {
    let message = refused(w.last_emit(), ErrorKind::Invalid);
    if !exact.is_empty() {
        assert_eq!(message, text(&exact));
    }
    assert!(message.contains(&text_or_empty(&contains)), "{message}");
}

fn text_or_empty(cell: &str) -> String {
    if cell.is_empty() {
        String::new()
    } else {
        text(cell)
    }
}

#[then(regex = r"^(no event is|one event is|two events are) stored$")]
async fn count_stored(w: &mut Env, count: String) {
    let n = match count.as_str() {
        "no event is" => 0,
        "one event is" => 1,
        _ => 2,
    };
    assert_eq!(w.stored().await.len(), n);
}

#[when("the client reads that event by ID")]
async fn reads_that(w: &mut Env) {
    w.read = Some(w.get(w.last_id().to_string()).await);
}

#[when(regex = r#"^the client reads event (".*")$"#)]
async fn reads(w: &mut Env, id: String) {
    w.read = Some(w.get(text(&id)).await);
}

fn read(w: &Env) -> &Event {
    w.read
        .as_ref()
        .expect("a read")
        .as_ref()
        .expect("the read succeeded")
}

#[then(
    regex = r#"^it has name (".*"), data (.*), metadata (.*), version (\d+) and idempotency key (".*")$"#
)]
fn has_all(w: &mut Env, name: String, d: String, metadata: String, version: i64, key: String) {
    let e = read(w);
    assert_eq!(e.name, text(&name));
    assert_eq!(e.data, data(&d));
    assert_eq!(e.metadata, Some(data(&metadata)));
    assert_eq!(e.version, version);
    assert_eq!(e.idempotency_key, Some(text(&key)));
}

#[then(regex = r"^it has version (\d+)$")]
fn has_version(w: &mut Env, version: i64) {
    assert_eq!(read(w).version, version);
}

#[then(regex = r"^its data is (.*)$")]
fn its_data(w: &mut Env, value: String) {
    let expected = match value.as_str() {
        "exactly the bytes sent" => w.sent.clone().unwrap(),
        v => data(v),
    };
    assert_eq!(read(w).data, expected);
}

#[then(regex = r#"^the read is refused as not found with (".*")$"#)]
fn read_not_found(w: &mut Env, message: String) {
    assert_eq!(
        refused(w.read.as_ref().unwrap(), ErrorKind::NotFound),
        text(&message)
    );
}

// ---- Batch ----

#[when(regex = r"^the client emits a batch of (.*)$")]
async fn emits_batch(w: &mut Env, what: String) {
    let keyed = |name: &str, key: &str| EmitRequest {
        idempotency_key: Some(key.into()),
        ..request(name)
    };
    let items = match what.as_str() {
        r#""a.ok", an event with no name, and "b.ok""# => {
            vec![request("a.ok"), request(""), request("b.ok")]
        }
        r#""a.ok" with idempotency key "k-1" and "b.ok" with idempotency key "k-1""# => {
            vec![keyed("a.ok", "k-1"), keyed("b.ok", "k-1")]
        }
        "no events" => vec![],
        w if w.starts_with('"') => quoted_list(w).into_iter().map(request).collect(),
        w => match w.split_once(" events, each with data of ") {
            Some((n, size)) => {
                let size = bytes_count(size.trim_end_matches(" bytes"));
                let d = format!("\"{}\"", "a".repeat(size - 2)).into_bytes();
                let item = EmitRequest {
                    data: Some(d),
                    ..request("blob.saved")
                };
                vec![item; n.parse().unwrap()]
            }
            None => {
                let n: usize = w.strip_suffix(" events").unwrap().parse().unwrap();
                (0..n).map(|i| request(format!("e.{i}"))).collect()
            }
        },
    };
    w.batch(items).await;
}

#[then(regex = r"^the batch call succeeds( with no results)?$")]
fn batch_succeeds(w: &mut Env, none: String) {
    let items = w.batch_items();
    if !none.is_empty() {
        assert!(items.is_empty());
    }
}

#[then(regex = r#"^the batch is refused as invalid(?: with (".*"))?$"#)]
fn batch_refused(w: &mut Env, message: String) {
    let got = refused(w.batch.as_ref().unwrap(), ErrorKind::Invalid);
    if !message.is_empty() {
        assert_eq!(got, text(&message));
    }
}

#[then("the second item is refused as invalid and has no event ID")]
fn second_refused(w: &mut Env) {
    refused(&w.batch_items()[1], ErrorKind::Invalid);
}

#[then(regex = r#"^events (".*") are stored$"#)]
async fn these_stored(w: &mut Env, names: String) {
    let stored: BTreeSet<_> = w.stored_names().await.into_iter().collect();
    assert_eq!(stored, quoted_list(&names).into_iter().collect());
}

#[then(regex = r#"^only (".*") is stored$"#)]
async fn only_stored(w: &mut Env, name: String) {
    assert_eq!(w.stored_names().await, [text(&name)]);
}

#[then("both items return the same event ID")]
fn same_items(w: &mut Env) {
    let items = w.batch_items();
    assert_eq!(
        items[0].as_ref().unwrap().event_id,
        items[1].as_ref().unwrap().event_id
    );
}

#[then("the second item is a duplicate")]
fn second_duplicate(w: &mut Env) {
    assert!(w.batch_items()[1].as_ref().unwrap().duplicate);
}

// ---- Idempotency ----

fn emit_id(w: &Env, i: usize) -> EventId {
    w.emits[i].as_ref().expect("the emit succeeded").event_id
}

#[then(
    regex = r"^(?:both emits return the same event ID|the second emit returns the first event's ID|the emit returns the original event ID)$"
)]
fn same_emits(w: &mut Env) {
    let n = w.emits.len();
    assert_eq!(emit_id(w, 0), emit_id(w, n - 1));
    assert!(w.emits[n - 1].as_ref().unwrap().duplicate);
}

#[then("the emits return different event IDs")]
fn different_emits(w: &mut Env) {
    assert_ne!(emit_id(w, 0), emit_id(w, 1));
}

// ---- Reading ----

#[when(regex = r"^the client lists events(?: with (no limit|limit \d+|a limit of \d+))?$")]
async fn lists(w: &mut Env, limit: String) {
    let limit = limit.rsplit(' ').next().and_then(|l| l.parse().ok());
    w.page = Some(w.list(limit, None).await);
}

#[when(regex = r"^the client follows the next cursor with a limit of (\d+)$")]
async fn follows(w: &mut Env, limit: u32) {
    let next = w.page().next.clone().expect("a next cursor");
    w.page = Some(w.list(Some(limit), Some(next)).await);
}

#[then(regex = r"^the page holds (\d+) events(?: and has (a|no) next page)?$")]
fn page_holds(w: &mut Env, n: usize, next: String) {
    let page = w.page();
    assert_eq!(page.events.len(), n);
    match next.as_str() {
        "a" => assert!(page.next.is_some()),
        "no" => assert!(page.next.is_none()),
        _ => {}
    }
}

#[then(regex = r"^they come back as (.*)$")]
fn come_back_as(w: &mut Env, names: String) {
    let got: Vec<_> = w.page().events.iter().map(|e| e.name.clone()).collect();
    assert_eq!(got, quoted_list(&names));
}

#[given(regex = r"^the client emitted (\d+) events and listed the first page of (\d+)$")]
async fn emitted_and_listed(w: &mut Env, n: usize, limit: u32) {
    w.originals = w.emit_n(n).await;
    w.first_page = limit as usize;
    w.page = Some(w.list(Some(limit), None).await);
}

#[when(regex = r"^the client emits (\d+) more events and follows the next cursor$")]
async fn emits_more_and_follows(w: &mut Env, n: usize) {
    let next = w.page().next.clone().expect("a next cursor");
    w.emit_n(n).await;
    w.page = Some(w.list(None, Some(next)).await);
}

#[then(regex = r"^the page holds the next (\d+) of the original events$")]
fn next_originals(w: &mut Env, n: usize) {
    let expected: Vec<_> = w
        .originals
        .iter()
        .rev()
        .skip(w.first_page)
        .take(n)
        .copied()
        .collect();
    let got: Vec<_> = w.page().events.iter().map(|e| e.event_id).collect();
    assert_eq!(got, expected);
}

#[when(regex = r"^the client lists events with (a cursor that is not a token|that cursor)$")]
async fn lists_with_cursor(w: &mut Env, which: String) {
    let cursor = match which.as_str() {
        "that cursor" => w.cursor.clone().expect("a cursor"),
        _ => "not-a-token".to_owned(),
    };
    w.page = Some(w.list(None, Some(cursor)).await);
}

#[then("the list is refused as invalid")]
fn list_refused(w: &mut Env) {
    refused(w.page.as_ref().unwrap(), ErrorKind::Invalid);
}

#[given("a cursor issued by an environment with the same ID and another incarnation token")]
async fn foreign_cursor(w: &mut Env) {
    let mine = w
        .ops()
        .list_environments(w.client(), ListEnvironmentsRequest {})
        .await
        .unwrap();
    let other: EnvRef = format!("{}.{}", mine[0].env.id(), new_env_ref().token())
        .parse()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path(), other, w.clock.clone())
        .await
        .unwrap();
    // A direct ledger caller, which the ledger trusts (D36).
    let by = SubmitAs::embedder("another incarnation").unwrap();
    for name in ["a", "b"] {
        let emit = Emit {
            name: name.into(),
            ..Emit::default()
        };
        ledger
            .submit(Submission::Emit(emit), by.clone())
            .await
            .unwrap();
    }
    let page = ledger.list_events(1.try_into().ok(), None).await.unwrap();
    w.cursor = page.next;
    ledger.close().await;
}

// ---- Read your writes ----

#[then(regex = r"^(the other client )?reading that event by ID right away finds it$")]
async fn finds_it(w: &mut Env, other: String) {
    let id = w.last_id();
    let event = w.get_as(w.reader(&other).await, id.to_string()).await;
    assert_eq!(event.unwrap().event_id, id);
}

#[then(regex = r"^(the other client )?listing events right away includes it$")]
async fn lists_it(w: &mut Env, other: String) {
    let id = w.last_id();
    let page = w.list_as(w.reader(&other).await, None, None).await.unwrap();
    assert!(page.events.iter().any(|e| e.event_id == id));
}

#[then("listing events returns none")]
async fn lists_none(w: &mut Env) {
    assert!(w.list(None, None).await.unwrap().events.is_empty());
}

/// Reads the feature file in place. gherkin-rs refuses a table cell escape other than `\n`,
/// `\|` and `\\`, where Gherkin keeps the backslash: `"\u0000"` is a JSON escape in a cell. So
/// this doubles such a backslash before parsing, which gives the cell Gherkin's meaning.
struct Unescaped;

impl cucumber::Parser<&'static str> for Unescaped {
    type Cli = cucumber::cli::Empty;
    type Output = futures::stream::Iter<std::vec::IntoIter<cucumber::parser::Result<Feature>>>;

    fn parse(self, path: &'static str, _: Self::Cli) -> Self::Output {
        let text = std::fs::read_to_string(path).unwrap();
        let text: String = text.lines().map(|l| cells(l) + "\n").collect();
        let mut feature =
            Feature::parse(text, GherkinEnv::default()).unwrap_or_else(|e| panic!("{path}: {e}"));
        feature.path = Some(path.into());
        // Each file's scenario declarations, before outline expansion, so a scenario cannot be
        // added or dropped unnoticed.
        let expected = SCENARIOS
            .iter()
            .find(|(name, _)| path.ends_with(name))
            .map(|(_, count)| *count);
        assert_eq!(
            Some(feature.scenarios.len()),
            expected,
            "{path}: scenario count"
        );
        let feature = feature.expand_examples().expect("examples expand");
        futures::stream::iter(vec![Ok(feature)])
    }
}

fn cells(line: &str) -> String {
    if !line.trim_start().starts_with('|') {
        return line.to_owned();
    }
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '\\' {
            match chars.next() {
                Some(next @ ('n' | '|' | '\\')) => out.push(next),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => {}
            }
        }
    }
    out
}

/// How many scenarios of `events.feature` run against the binary, an outline once per example, so
/// no example goes unnoticed.
const EVENT_REMOTE_RUNS: usize = 93;

/// Every feature file in `docs/behaviors/` and in `FOUNDATION`, and its scenario declarations.
const SCENARIOS: &[(&str, usize)] = &[
    ("cli.feature", 14),
    ("events.feature", 41),
    ("serve.feature", 26),
    ("tenancy.feature", 40),
];

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/behaviors");

/// Foundation's feature files at the pinned commit, vendored: `ci/vendored.sh` checks the copies.
const FOUNDATION: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../vendor/foundation/docs/behaviors"
);

/// The feature files on disk must be exactly the ones `SCENARIOS` names.
fn discover() {
    let mut found: Vec<String> = [DIR, FOUNDATION]
        .into_iter()
        .flat_map(|dir| std::fs::read_dir(dir).unwrap())
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".feature"))
        .collect();
    found.sort();
    let mut known: Vec<String> = SCENARIOS.iter().map(|(n, _)| (*n).to_owned()).collect();
    known.sort();
    assert_eq!(found, known, "feature files on disk vs SCENARIOS");
}

/// Runs `events.feature`, through the binary or on the engine: whether a scenario failed, and how
/// many passed.
async fn events(remote: bool) -> (bool, usize) {
    let writer = Env::cucumber::<&str>()
        .with_parser(Unescaped)
        .before(move |_, _, _, world| {
            world.remote = remote;
            Box::pin(async {})
        })
        .after(move |_, _, _, _, world| {
            Box::pin(async move {
                if let Some(ops) = world.and_then(|w| w.ops.take()) {
                    assert_eq!(matches!(ops, Adapter::Binary(..)), remote, "the adapter");
                    ops.close().await;
                }
            })
        })
        .max_concurrent_scenarios(if remote { BINARIES } else { 64 })
        .fail_on_skipped()
        .filter_run(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../vendor/foundation/docs/behaviors/events.feature"
            ),
            move |_, _, scenario| !remote || runs_remotely(scenario),
        )
        .await;
    let passed = writer.scenarios_stats().passed;
    (writer.execution_has_failed(), passed)
}

#[tokio::main]
async fn main() {
    output::fixtures();
    discover();
    serve::build();
    // Foundation runs these two files on the engine; this repository, against the binary. The
    // steps keep foundation's engine branch, so the runner stays a copy of foundation's.
    let mut runs = vec![
        (
            "events.feature".to_owned(),
            events(true).await,
            EVENT_REMOTE_RUNS,
        ),
        (
            "tenancy.feature".to_owned(),
            tenancy::run(Unescaped, true).await,
            tenancy::REMOTE_RUNS,
        ),
    ];
    let serve = serve::run(Unescaped).await;
    runs.push(("serve.feature".to_owned(), serve, serve::RUNS));
    let cli = cli::run(Unescaped).await;
    runs.push(("cli.feature".to_owned(), cli, cli::RUNS));
    for (file, (failed, passed), expected) in runs {
        assert!(!failed, "{file} failed");
        assert_eq!(passed, expected, "{file}: scenarios run");
    }
}
