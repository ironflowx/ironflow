//! Foundation's `tenancy.feature`, but for what is `@local`, against the binary.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use cucumber::writer::Stats as _;
use cucumber::{World, given, then, when};
use ironflowx_cell::{HostClock, Ledger, ManualClock, new_env_ref};
use ironflowx_core::{Clock as _, EnvRef, ErrorKind, KeyOrigin, Scope, Secret, Timestamp};
use ironflowx_operations::{
    Engine, EngineError, Opened, RecoverError, SeedState, recover_admin_key,
};
use ironflowx_proto::{
    Call, CreateKeyRequest, EmitBatchRequest, EmitRequest, Environment, GetEventRequest, KeyInfo,
    KeyPage, KeyStatus, ListEnvironmentsRequest, ListEventsRequest, ListKeysRequest, OpError,
    Operations, RevokeKeyRequest, Whoami, WhoamiRequest,
};
use tempfile::TempDir;

use super::adapter::{Adapter, BINARIES, runs_remotely};

/// How many scenarios run against the binary, an outline once per example, so no example goes
/// unnoticed.
pub const REMOTE_RUNS: usize = 41;

const HOUR: i64 = 3_600_000;

#[derive(World)]
#[world(init = Self::new)]
pub struct Tenancy {
    /// Whether the engine is the binary's.
    remote: bool,
    // Before `dir`: the engine must close before its directory goes.
    engine: Option<Adapter>,
    /// A second engine, with its own directory (ACC-009).
    other: Option<(Engine, TempDir)>,
    dir: TempDir,
    clock: ManualClock,
    /// The bootstrap key's text, as the first open returned it.
    bootstrap_key: Option<String>,
    /// What the last open returned in `minted`.
    returned: Option<bool>,
    whoami: Option<Result<Whoami, OpError>>,
    /// The environment before a reopen (ACC-003).
    env_before: Option<EnvRef>,
    /// The second engine's environment, as its own key lists it (ACC-009).
    other_env: Option<Environment>,
    /// The second engine's bootstrap key's text (ACC-024).
    other_key: Option<String>,
    /// The key text the last refused call presented, if any.
    presented: Option<String>,
    /// How the last call ended.
    outcome: Option<Result<(), OpError>>,
    /// The client's key text, if not the bootstrap key's, and its expiry.
    client: Option<String>,
    client_expiry: Option<Timestamp>,
    /// Another client's key text (ACC-023).
    second: Option<String>,
    /// The last event the client emitted, or the last batch's.
    emitted: Vec<String>,
    /// The time a key expires at (ACC-053).
    t: Option<Timestamp>,
    listed: Option<Vec<Environment>>,
    /// What the last open reported about its seed.
    reported: Option<SeedState>,
    /// Why the last reopen was refused, if it was.
    refused: Option<EngineError>,
    /// Why the recovery was refused, if it was.
    not_recovered: Option<RecoverError>,
    /// The text of the seeds and keys a scenario names.
    texts: BTreeMap<String, String>,
    /// Keys a scenario names (A, B, C, K), by key ID.
    named: BTreeMap<String, String>,
    /// The secrets of those keys (ACC-044).
    secrets: Vec<String>,
    /// The key the client created last: its text and what was said about it.
    created: Option<(String, KeyInfo)>,
    /// Pages of keys, as listed.
    pages: Vec<KeyPage>,
    /// The number of keys before a refused creation.
    keys_before: Option<usize>,
    /// Every path in the directory before a refused open, with a file's bytes.
    contents: Option<BTreeMap<PathBuf, Option<Vec<u8>>>>,
}

impl fmt::Debug for Tenancy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tenancy")
            .field("returned", &self.returned)
            .field("whoami", &self.whoami)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

fn clock() -> ManualClock {
    ManualClock::new(Timestamp::from_millis(1_800_000_000_000))
}

/// A call presenting `key`; tests copy the text, since a `Secret` cannot be cloned.
fn call(key: &str) -> Call {
    Call::with_key(Secret::new(key.to_owned()))
}

fn scopes(text: &str) -> BTreeSet<Scope> {
    text.split(['"', ',', ' '])
        .filter(|s| !s.is_empty() && *s != "and")
        .map(|s| s.parse().expect("a scope"))
        .collect()
}

impl Tenancy {
    fn new() -> Self {
        Self {
            remote: false,
            engine: None,
            other: None,
            dir: tempfile::tempdir().unwrap(),
            clock: clock(),
            bootstrap_key: None,
            returned: None,
            whoami: None,
            env_before: None,
            other_env: None,
            other_key: None,
            presented: None,
            outcome: None,
            client: None,
            client_expiry: None,
            second: None,
            emitted: Vec::new(),
            t: None,
            listed: None,
            reported: None,
            refused: None,
            not_recovered: None,
            texts: BTreeMap::new(),
            named: BTreeMap::new(),
            secrets: Vec::new(),
            created: None,
            pages: Vec::new(),
            keys_before: None,
            contents: None,
        }
    }

    /// Every path in the directory, with a file's bytes.
    fn read_directory(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![self.dir.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                    out.insert(path, None);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    out.insert(path, Some(bytes));
                }
            }
        }
        out
    }

    async fn open(&mut self) {
        if self.remote {
            let (served, key) = Adapter::served(&self.dir.path().join("data")).await;
            self.bootstrap_key = Some(key);
            self.engine = Some(served);
            return;
        }
        self.open_with(None).await.expect("the engine opens");
    }

    /// Opens the engine on the scenario's directory with `seed`, a key's text.
    async fn open_with(&mut self, seed: Option<&str>) -> Result<(), EngineError> {
        let seed = seed.map(|s| Secret::new(s.to_owned()));
        let opened = Engine::open(self.dir.path(), self.clock.clone(), seed).await?;
        self.returned = Some(opened.minted.is_some());
        self.reported = opened.seed;
        if let Some(key) = opened.minted {
            self.bootstrap_key = Some(key.expose().to_owned());
        }
        self.engine = Some(Adapter::Engine(opened.engine));
        Ok(())
    }

    /// Closes the engine, if it is open, and opens it again, keeping how the open ended.
    async fn reopen_with(&mut self, seed: Option<&str>) {
        if let Some(engine) = self.engine.take() {
            engine.close().await;
        }
        self.returned = None;
        self.reported = None;
        self.refused = self.open_with(seed).await.err();
    }

    /// The text of a key the scenario names: a seed (A, B) or a created key (K).
    fn text(&self, name: &str) -> &str {
        self.texts.get(name).expect("a key the scenario named")
    }

    async fn whoami_with(&self, name: &str) -> Result<Whoami, OpError> {
        let cx = call(self.text(name));
        self.engine().whoami(&cx, WhoamiRequest {}).await
    }

    fn engine(&self) -> &Adapter {
        self.engine.as_ref().expect("an open engine")
    }

    /// The client's key: the bootstrap key unless a step gave the client another.
    fn key(&self) -> Call {
        let key = self.client.as_deref().or(self.bootstrap_key.as_deref());
        call(key.expect("a key"))
    }

    fn bootstrap(&self) -> Call {
        call(self.bootstrap_key.as_deref().expect("the bootstrap key"))
    }

    /// The client's key ID: its text between `ixk_` and the secret.
    fn key_id(&self) -> String {
        let key = self.client.as_deref().or(self.bootstrap_key.as_deref());
        key.expect("a key")[4..40].to_owned()
    }

    /// Creates a key with the bootstrap key, opening the engine if no step has; its text.
    async fn create(
        &mut self,
        scopes: BTreeSet<Scope>,
        expires_at: Option<Timestamp>,
        label: Option<String>,
    ) -> String {
        if self.engine.is_none() {
            self.open().await;
        }
        let req = CreateKeyRequest {
            scopes,
            expires_at,
            label,
        };
        let created = self.engine().create_key(&self.bootstrap(), req).await;
        created
            .expect("the bootstrap key creates a key")
            .key
            .expose()
            .to_owned()
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn keep<T>(&mut self, answer: Result<T, OpError>) {
        self.outcome = Some(answer.map(drop));
    }

    async fn whoami(&self) -> Result<Whoami, OpError> {
        self.engine().whoami(&self.key(), WhoamiRequest {}).await
    }

    /// Lists events on the first engine, opening it if no step has, and keeps the outcome.
    async fn list_events(&mut self, cx: Call) {
        if self.engine.is_none() {
            self.open().await;
        }
        let answer = self.engine().list_events(&cx, first_page()).await;
        self.keep(answer);
    }

    fn refusal(&self) -> &OpError {
        match &self.outcome {
            Some(Err(err)) => err,
            other => panic!("a refused call, not {other:?}"),
        }
    }

    async fn emit(&mut self, req: EmitRequest) {
        let answer = self.engine().emit(&self.key(), req).await;
        if let Ok(r) = &answer {
            self.emitted = vec![r.event_id.to_string()];
        }
        self.keep(answer);
    }

    /// Every event, as the bootstrap key lists them.
    async fn events(&self) -> Vec<ironflowx_proto::Event> {
        let page = self
            .engine()
            .list_events(&self.bootstrap(), first_page())
            .await;
        page.expect("the bootstrap key lists events").events
    }

    /// Every key, as the bootstrap key lists them in one page.
    async fn all_keys(&self) -> Vec<KeyInfo> {
        let req = ListKeysRequest {
            limit: std::num::NonZeroU32::new(1_000),
            cursor: None,
        };
        let page = self.engine().list_keys(&self.bootstrap(), req).await;
        page.expect("the bootstrap key lists keys").keys
    }

    /// The client creates a key, keeping the outcome and, on success, the key.
    async fn client_creates_key(&mut self, req: CreateKeyRequest) {
        self.keys_before = Some(self.all_keys().await.len());
        let answer = self.engine().create_key(&self.key(), req).await;
        if let Ok(created) = &answer {
            self.created = Some((created.key.expose().to_owned(), created.info.clone()));
        }
        self.keep(answer);
    }

    async fn revoke(&mut self, cx: &Call, key_id: String) {
        let answer = self
            .engine()
            .revoke_key(cx, RevokeKeyRequest { key_id })
            .await;
        self.keep(answer);
    }

    async fn environments(&self) -> Vec<Environment> {
        self.engine()
            .list_environments(&self.key(), ListEnvironmentsRequest {})
            .await
            .expect("the key lists environments")
    }
}

#[given("a fresh engine directory")]
fn fresh(_: &mut Tenancy) {}

#[when("the engine is opened")]
#[given("the engine was opened and returned its bootstrap key")]
async fn opened(w: &mut Tenancy) {
    w.open().await;
    assert_eq!(
        w.returned,
        Some(true),
        "the first open returns the bootstrap key"
    );
}

#[given("a client with the bootstrap key")]
async fn client(w: &mut Tenancy) {
    w.open().await;
}

#[then("exactly one key is returned")]
fn one_key(w: &mut Tenancy) {
    assert_eq!(w.returned, Some(true));
    let key = w.bootstrap_key.as_deref().unwrap();
    assert!(key.starts_with("ixk_") && key.len() == 84, "{key}");
}

#[then("no key is returned")]
fn no_key(w: &mut Tenancy) {
    assert_eq!(w.returned, Some(false));
}

#[then(regex = r#"^with it, whoami names scopes (.+) and environment "([^"]+)"$"#)]
async fn whoami_names(w: &mut Tenancy, names: String, address: String) {
    let me = w.whoami().await.expect("the bootstrap key authenticates");
    assert_eq!(me.scopes, scopes(&names));
    assert_eq!(me.environment.to_string(), address);
}

#[when("the client asks whoami")]
async fn asks_whoami(w: &mut Tenancy) {
    w.whoami = Some(w.whoami().await);
}

#[then(
    regex = r#"^the answer names the key's ID, scopes (.+), environment "([^"]+)" and no expiry$"#
)]
fn answer_names(w: &mut Tenancy, names: String, address: String) {
    let me = w
        .whoami
        .as_ref()
        .unwrap()
        .as_ref()
        .expect("whoami succeeds");
    let key = w.bootstrap_key.as_deref().unwrap();
    // The key's ID is its text between `ixk_` and the secret.
    assert_eq!(me.key_id.to_string(), key[4..40]);
    assert_eq!(me.scopes, scopes(&names));
    assert_eq!(me.environment.to_string(), address);
    assert_eq!(me.expires_at, None);
}

#[then(regex = r#"^listing environments shows one environment, named "([^"]+)"$"#)]
async fn lists_one(w: &mut Tenancy, name: String) {
    let envs = w.environments().await;
    assert_eq!(envs.len(), 1, "{envs:?}");
    assert_eq!(envs[0].name, name);
}

#[when("the engine is closed and opened again")]
async fn reopened(w: &mut Tenancy) {
    // With the bootstrap key: the client's may be refused by now (ACC-054).
    let envs = w
        .engine()
        .list_environments(&w.bootstrap(), ListEnvironmentsRequest {})
        .await
        .expect("the bootstrap key lists environments");
    w.env_before = Some(envs[0].env);
    w.engine.take().unwrap().close().await;
    w.open().await;
}

#[then("the bootstrap key still authenticates")]
async fn still_authenticates(w: &mut Tenancy) {
    w.whoami().await.expect("the bootstrap key authenticates");
}

#[then("listing environments still shows one environment, with the same ID")]
async fn same_environment(w: &mut Tenancy) {
    let envs = w.environments().await;
    assert_eq!(envs.len(), 1, "{envs:?}");
    assert_eq!(Some(envs[0].env), w.env_before);
}

#[when("two engines are opened in two fresh directories")]
async fn two_engines(w: &mut Tenancy) {
    w.open().await;
    let dir = tempfile::tempdir().unwrap();
    let Opened { engine, minted, .. } = Engine::open(dir.path(), clock(), None).await.unwrap();
    let key = call(minted.expect("the second engine's bootstrap key").expose());
    let mut envs = engine
        .list_environments(&key, ListEnvironmentsRequest {})
        .await
        .unwrap();
    assert_eq!(envs.len(), 1, "{envs:?}");
    w.other_env = envs.pop();
    w.other = Some((engine, dir));
}

#[then("their environments have different IDs and different project IDs")]
async fn different_ids(w: &mut Tenancy) {
    let first = w.environments().await.remove(0);
    let second = w.other_env.as_ref().unwrap();
    assert_ne!(first.env.id(), second.env.id());
    assert_ne!(first.project_id, second.project_id);
}

#[then(regex = r#"^both environments have the address "([^"]+)"$"#)]
async fn both_addresses(w: &mut Tenancy, address: String) {
    assert_eq!(w.environments().await[0].address.to_string(), address);
    assert_eq!(w.other_env.as_ref().unwrap().address.to_string(), address);
}

#[when(regex = r"^a client with (no key|an empty key) lists events$")]
async fn lists_without_a_key(w: &mut Tenancy, key: String) {
    let cx = if key == "no key" {
        Call::anonymous()
    } else {
        call("")
    };
    w.list_events(cx).await;
}

#[when(regex = r"^a client presenting (.+) lists events$")]
async fn lists_presenting(w: &mut Tenancy, key: String) {
    w.open().await;
    let bootstrap = w.bootstrap_key.clone().unwrap();
    let text = match key.as_str() {
        // Use nonzero random secret bytes so SQLite padding is not a TEN-3 match.
        "a well-formed key no engine minted" => {
            ironflowx_operations::generate_seed().1.expose().to_owned()
        }
        "the bootstrap key's ID with another secret" => {
            // The secret's first character: every bit of it is data, unlike the last one's.
            let other = if &bootstrap[41..42] == "A" { "B" } else { "A" };
            format!("{}{other}{}", &bootstrap[..41], &bootstrap[42..])
        }
        r#""bad-key""# => "bad-key".to_owned(),
        r#"the bootstrap key with a "Bearer " prefix"# => format!("Bearer {bootstrap}"),
        "the bootstrap key with one character removed" => bootstrap[..83].to_owned(),
        other => panic!("no such key: {other}"),
    };
    w.presented = Some(text.clone());
    w.list_events(call(&text)).await;
}

#[given("a second engine opened in another fresh directory, and its bootstrap key")]
async fn second_engine(w: &mut Tenancy) {
    w.open().await;
    let dir = tempfile::tempdir().unwrap();
    let Opened { engine, minted, .. } = Engine::open(dir.path(), clock(), None).await.unwrap();
    w.other_key = Some(minted.unwrap().expose().to_owned());
    w.other = Some((engine, dir));
}

#[when("a client presenting that key lists events on the first engine")]
async fn lists_with_the_other_key(w: &mut Tenancy) {
    let key = w.other_key.clone().unwrap();
    w.list_events(call(&key)).await;
}

#[then(regex = r#"^the call is refused as unauthenticated with "([^"]+)"$"#)]
fn refused_as(w: &mut Tenancy, message: String) {
    let err = w.refusal();
    assert_eq!(
        (err.kind, err.message.as_str()),
        (ErrorKind::Unauthenticated, message.as_str())
    );
}

#[then("the error is not retryable")]
fn not_retryable(w: &mut Tenancy) {
    assert!(!w.refusal().kind.retryable());
}

#[then("the error has no hint")]
fn no_hint(w: &mut Tenancy) {
    assert_eq!(w.refusal().hint, None);
}

#[then("the hint names the key format")]
fn hint_names_format(w: &mut Tenancy) {
    let hint = w.refusal().hint.as_deref().expect("a hint");
    assert!(hint.contains("ixk_<key-id>_<secret>"), "{hint}");
}

#[then("the error does not contain the presented key")]
fn no_echo(w: &mut Tenancy) {
    let bootstrap = w.bootstrap_key.as_deref().unwrap();
    let shown = format!("{:?}", w.refusal());
    // Neither the text presented, nor the key inside it, nor its secret alone.
    for part in [w.presented.as_deref().unwrap(), bootstrap, &bootstrap[41..]] {
        assert!(!shown.contains(part), "{shown}");
    }
}

const fn first_page() -> ListEventsRequest {
    ListEventsRequest {
        limit: None,
        cursor: None,
    }
}

fn named(name: &str) -> EmitRequest {
    EmitRequest {
        name: name.into(),
        data: None,
        metadata: None,
        version: None,
        idempotency_key: None,
    }
}

#[given(
    regex = r#"^(a|another) client with an? ((?:"\w+"(?: and )?)+) key( K)?( expiring in one hour| created with label "[^"]+" and an expiry| expiring at time T| that has listed events)?$"#
)]
async fn client_with_key(w: &mut Tenancy, who: String, names: String, k: String, rest: String) {
    let hour = Timestamp::from_millis(w.now().as_millis() + HOUR);
    let (expires_at, label) = match rest.as_str() {
        "" | " that has listed events" => (None, None),
        " expiring in one hour" => (Some(hour), None),
        " expiring at time T" => {
            w.t = Some(hour);
            (Some(hour), None)
        }
        labelled => (Some(hour), labelled.split('"').nth(1).map(str::to_owned)),
    };
    let key = w.create(scopes(&names), expires_at, label).await;
    if !k.is_empty() {
        w.named.insert("K".into(), key[4..40].to_owned());
    }
    if who == "another" {
        w.second = Some(key);
    } else {
        w.client = Some(key);
        w.client_expiry = expires_at;
    }
    if rest == " that has listed events" {
        w.list_events(w.key()).await;
        assert_eq!(w.outcome, Some(Ok(())));
    }
}

#[when("the client lists environments")]
async fn lists_environments(w: &mut Tenancy) {
    let answer = w
        .engine()
        .list_environments(&w.key(), ListEnvironmentsRequest {})
        .await;
    w.listed = Some(answer.expect("the key lists environments"));
}

#[then(
    regex = r#"^the list holds one environment, named "([^"]+)", with its ID, project ID, address "([^"]+)" and creation time$"#
)]
async fn the_list_holds(w: &mut Tenancy, name: String, address: String) {
    let listed = w.listed.clone().unwrap();
    // The same environment as the bootstrap key's, created when the engine first opened.
    let req = ListEnvironmentsRequest {};
    let bootstrap = w.engine().list_environments(&w.bootstrap(), req).await;
    assert_eq!(listed, bootstrap.expect("the bootstrap key lists them"));
    assert_eq!(listed.len(), 1, "{listed:?}");
    let env = &listed[0];
    assert_eq!(
        (env.name.as_str(), env.address.to_string()),
        (name.as_str(), address)
    );
    if w.remote {
        let now = HostClock.now();
        assert!(w.now() <= env.created_at && env.created_at <= now);
    } else {
        assert_eq!(env.created_at, clock().now());
    }
}

#[when(regex = r#"^the (?:first )?client emits event "([^"]+)"$"#)]
async fn client_emits(w: &mut Tenancy, name: String) {
    w.emit(named(&name)).await;
}

#[when("the second client reads that event by ID")]
async fn second_reads(w: &mut Tenancy) {
    let req = GetEventRequest {
        event_id: w.emitted[0].clone(),
    };
    let answer = w
        .engine()
        .get_event(&call(w.second.as_deref().unwrap()), req)
        .await;
    w.keep(answer);
}

#[then("the call succeeds")]
fn succeeds(w: &mut Tenancy) {
    assert_eq!(w.outcome, Some(Ok(())));
}

#[then(regex = r#"^whoami with either key names environment "([^"]+)"$"#)]
async fn whoami_either(w: &mut Tenancy, address: String) {
    for key in [w.client.clone().unwrap(), w.second.clone().unwrap()] {
        let me = w
            .engine()
            .whoami(&call(&key), WhoamiRequest {})
            .await
            .unwrap();
        assert_eq!(me.environment.to_string(), address);
    }
}

#[then("the call is refused as permission denied")]
fn permission_denied(w: &mut Tenancy) {
    assert_eq!(w.refusal().kind, ErrorKind::PermissionDenied);
}

#[then(regex = r#"^the hint says a key with scope "(\w+)" is needed$"#)]
fn hint_names_scope(w: &mut Tenancy, scope: String) {
    let err = w.refusal();
    assert_eq!(err.message, format!("missing scope: {scope}"));
    let hint = err.hint.as_deref().expect("a hint");
    assert!(hint.contains(&format!("scope {scope}")), "{hint}");
}

#[then("no event is stored")]
async fn no_event(w: &mut Tenancy) {
    assert_eq!(w.events().await, vec![]);
}

#[then(
    regex = r#"^the answer names the key's ID, scope "(\w+)", environment "([^"]+)" and the expiry$"#
)]
fn answer_names_expiry(w: &mut Tenancy, scope: String, address: String) {
    let me = w
        .whoami
        .as_ref()
        .unwrap()
        .as_ref()
        .expect("whoami succeeds");
    assert_eq!(me.key_id.to_string(), w.key_id());
    assert_eq!(me.scopes, scopes(&scope));
    assert_eq!(me.environment.to_string(), address);
    assert_eq!(me.expires_at, w.client_expiry);
    assert!(me.expires_at.is_some());
}

#[when(regex = r#"^the client creates a key with scope "(\w+)" and (.+)$"#)]
async fn client_creates(w: &mut Tenancy, scope: String, expiry: String) {
    let creator = w
        .client_expiry
        .expect("a creator with an expiry")
        .as_millis();
    let expires_at = match expiry.as_str() {
        "no expiry" => None,
        "an expiry one millisecond after the creator's" => Some(creator + 1),
        "an expiry equal to the creator's" => Some(creator),
        "an expiry in 30 minutes" => Some(w.now().as_millis() + HOUR / 2),
        other => panic!("no such expiry: {other}"),
    };
    let req = CreateKeyRequest {
        scopes: scopes(&scope),
        expires_at: expires_at.map(Timestamp::from_millis),
        label: None,
    };
    let answer = w.engine().create_key(&w.key(), req).await;
    w.keep(answer);
}

#[then("the call is refused as invalid")]
fn refused_invalid(w: &mut Tenancy) {
    assert_eq!(w.refusal().kind, ErrorKind::Invalid);
}

#[when(regex = r"^the clock reads (one millisecond before )?T$")]
fn clock_reads(w: &mut Tenancy, before: String) {
    let t = w.t.expect("a time T").as_millis();
    let at = if before.is_empty() { t } else { t - 1 };
    w.clock.set(Timestamp::from_millis(at));
}

#[when("the client lists events")]
async fn client_lists(w: &mut Tenancy) {
    w.list_events(w.key()).await;
}

#[when(regex = r"^the client emits a batch of (\d+) events$")]
async fn client_emits_batch(w: &mut Tenancy, n: usize) {
    let items = (0..n).map(|i| named(&format!("batch.item{i}"))).collect();
    let answer = w
        .engine()
        .emit_batch(&w.key(), EmitBatchRequest { items })
        .await;
    let batch = answer.expect("the batch is emitted");
    w.emitted = batch
        .items
        .into_iter()
        .map(|item| item.expect("each item is stored").event_id.to_string())
        .collect();
}

#[then(regex = r#"^reading the event shows emitted_by "key:" followed by K's ID$"#)]
async fn reads_emitter(w: &mut Tenancy) {
    let req = GetEventRequest {
        event_id: w.emitted[0].clone(),
    };
    let event = w.engine().get_event(&w.key(), req).await.unwrap();
    assert_eq!(event.emitted_by.to_string(), format!("key:{}", w.key_id()));
}

#[then(regex = r#"^each of the (\d+) events shows emitted_by "key:" followed by K's ID$"#)]
async fn each_shows_emitter(w: &mut Tenancy, n: usize) {
    assert_eq!(w.emitted.len(), n);
    let by = format!("key:{}", w.key_id());
    for id in w.emitted.clone() {
        let req = GetEventRequest { event_id: id };
        let event = w.engine().get_event(&w.key(), req).await.unwrap();
        assert_eq!(event.emitted_by.to_string(), by);
    }
}

#[given(regex = r#"^an event "([^"]+)" was emitted with the bootstrap key$"#)]
async fn emitted_with_bootstrap(w: &mut Tenancy, name: String) {
    w.open().await;
    let answer = w.engine().emit(&w.bootstrap(), named(&name)).await;
    w.emitted = vec![answer.expect("emitted").event_id.to_string()];
}

#[given(regex = r#"^a client with a key holding only "(\w+)"$"#)]
async fn client_holding_only(w: &mut Tenancy, scope: String) {
    let key = w.create(scopes(&scope), None, None).await;
    w.client = Some(key);
}

#[when(regex = r"^the client calls (.+)$")]
async fn client_calls(w: &mut Tenancy, operation: String) {
    let (engine, cx) = (w.engine(), w.key());
    let answer = match operation.as_str() {
        "list events" => engine.list_events(&cx, first_page()).await.map(drop),
        "get the event" => {
            let req = GetEventRequest {
                event_id: w.emitted[0].clone(),
            };
            engine.get_event(&cx, req).await.map(drop)
        }
        "list environments" => engine
            .list_environments(&cx, ListEnvironmentsRequest {})
            .await
            .map(drop),
        "emit" => engine.emit(&cx, named("order.shipped")).await.map(drop),
        "emit a batch" => {
            let items = vec![named("a.one"), named("a.two")];
            engine
                .emit_batch(&cx, EmitBatchRequest { items })
                .await
                .map(drop)
        }
        "create a key" => engine
            .create_key(&cx, CreateKeyRequest::default())
            .await
            .map(drop),
        "list keys" => engine
            .list_keys(&cx, ListKeysRequest::default())
            .await
            .map(drop),
        "revoke a key" => {
            let req = RevokeKeyRequest { key_id: w.key_id() };
            engine.revoke_key(&cx, req).await
        }
        other => panic!("no such operation: {other}"),
    };
    w.keep(answer);
}

#[then(regex = r#"^the call is refused as permission denied with "([^"]+)"$"#)]
fn denied_with(w: &mut Tenancy, message: String) {
    let err = w.refusal();
    assert_eq!(
        (err.kind, err.message.as_str()),
        (ErrorKind::PermissionDenied, &*message)
    );
}

#[when("the client creates a key without scopes")]
async fn creates_without_scopes(w: &mut Tenancy) {
    w.client_creates_key(CreateKeyRequest::default()).await;
}

#[then(regex = r#"^the answer holds the key and its scopes are (.+)$"#)]
fn answer_holds_key(w: &mut Tenancy, names: String) {
    let (key, info) = w.created.as_ref().expect("a created key");
    assert!(key.starts_with("ixk_") && key.len() == 84, "{key}");
    assert_eq!(info.scopes, scopes(&names));
}

#[then("listing keys shows the key without its secret")]
async fn listed_without_secret(w: &mut Tenancy) {
    let (key, info) = w.created.clone().unwrap();
    let keys = w.all_keys().await;
    let listed = keys
        .iter()
        .find(|k| k.key_id == info.key_id)
        .expect("listed");
    assert_eq!(listed, &info);
    // `KeyInfo` has no field for it; nor does any text a listing prints.
    assert!(!format!("{keys:?}").contains(&key[41..]));
}

#[when(
    regex = r#"^the client creates a key with scopes (.+), label "([^"]+)" and an expiry in one hour$"#
)]
async fn creates_with_scopes_label_expiry(w: &mut Tenancy, names: String, label: String) {
    let req = CreateKeyRequest {
        scopes: scopes(&names),
        expires_at: Some(Timestamp::from_millis(w.now().as_millis() + HOUR)),
        label: Some(label),
    };
    w.client_creates_key(req).await;
}

#[then("a client with the new key can emit and list events")]
async fn new_key_works(w: &mut Tenancy) {
    let cx = call(&w.created.as_ref().unwrap().0);
    w.engine()
        .emit(&cx, named("order.placed"))
        .await
        .expect("emit");
    let page = w
        .engine()
        .list_events(&cx, first_page())
        .await
        .expect("list");
    assert_eq!(page.events.len(), 1);
}

#[then(
    regex = r#"^listing keys shows it with label "([^"]+)", those scopes, the expiry, status "active", origin "created" and the bootstrap key as its creator$"#
)]
async fn listed_with(w: &mut Tenancy, label: String) {
    let info = w.created.clone().unwrap().1;
    let keys = w.all_keys().await;
    let listed = keys
        .iter()
        .find(|k| k.key_id == info.key_id)
        .expect("listed");
    let bootstrap_id = w.bootstrap_key.as_deref().unwrap()[4..40].parse().unwrap();
    assert_eq!(listed.label.as_deref(), Some(label.as_str()));
    assert_eq!(listed.scopes, BTreeSet::from([Scope::Read, Scope::Write]));
    assert_eq!(
        listed.expires_at,
        Some(Timestamp::from_millis(w.now().as_millis() + HOUR))
    );
    assert_eq!(
        (listed.status, listed.origin, listed.created_by),
        (KeyStatus::Active, KeyOrigin::Created, Some(bootstrap_id))
    );
}

#[when(regex = r#"^the client creates a key with scope "(\w+)"$"#)]
async fn creates_with_scope(w: &mut Tenancy, scope: String) {
    let req = CreateKeyRequest {
        scopes: scopes(&scope),
        ..CreateKeyRequest::default()
    };
    w.client_creates_key(req).await;
}

#[when(
    regex = r"^the client creates a key with (an expiry an hour ago|a label of 129 bytes|a label holding a line break)$"
)]
async fn creates_bad(w: &mut Tenancy, request: String) {
    let mut req = CreateKeyRequest::default();
    match request.as_str() {
        "an expiry an hour ago" => {
            req.expires_at = Some(Timestamp::from_millis(w.now().as_millis() - HOUR));
        }
        "a label of 129 bytes" => req.label = Some("x".repeat(129)),
        _ => req.label = Some("first\nsecond".into()),
    }
    w.client_creates_key(req).await;
}

#[then("no key is created")]
async fn no_key_created(w: &mut Tenancy) {
    assert_eq!(Some(w.all_keys().await.len()), w.keys_before);
}

#[given("a client with the bootstrap key and the clock reading T")]
async fn bootstrap_at_t(w: &mut Tenancy) {
    w.open().await;
    w.t = Some(w.now());
}

#[when("the client creates a key expiring at T")]
async fn creates_expiring_at_t(w: &mut Tenancy) {
    let req = CreateKeyRequest {
        expires_at: w.t,
        ..CreateKeyRequest::default()
    };
    w.client_creates_key(req).await;
}

#[given("a client with the bootstrap key created keys A, B and C, in that order")]
async fn created_abc(w: &mut Tenancy) {
    w.open().await;
    for name in ["A", "B", "C"] {
        let req = CreateKeyRequest {
            label: Some(name.into()),
            ..CreateKeyRequest::default()
        };
        let created = w.engine().create_key(&w.bootstrap(), req).await.unwrap();
        w.named.insert(name.into(), created.info.key_id.to_string());
    }
}

#[when(regex = r"^the client lists keys (\d+) at a time$")]
async fn lists_keys_by(w: &mut Tenancy, n: u32) {
    let mut cursor = None;
    loop {
        let req = ListKeysRequest {
            limit: std::num::NonZeroU32::new(n),
            cursor,
        };
        let page = w.engine().list_keys(&w.key(), req).await.unwrap();
        cursor = page.next.clone();
        w.pages.push(page);
        if cursor.is_none() {
            break;
        }
    }
}

/// The names of a page's keys: a scenario's own, or "the bootstrap key".
fn names(w: &Tenancy, page: &KeyPage) -> Vec<String> {
    let bootstrap = &w.bootstrap_key.as_deref().unwrap()[4..40];
    let name = |id: String| {
        if id == bootstrap {
            return "the bootstrap key".to_owned();
        }
        let named = w.named.iter().find(|(_, v)| **v == id);
        named.expect("a key the scenario named").0.clone()
    };
    page.keys
        .iter()
        .map(|k| name(k.key_id.to_string()))
        .collect()
}

#[then("the first page holds C and B and names a next page")]
fn first_page_holds(w: &mut Tenancy) {
    assert_eq!(names(w, &w.pages[0]), ["C", "B"]);
    assert!(w.pages[0].next.is_some());
}

#[then("the next page holds A and the bootstrap key, and names none")]
fn next_page_holds(w: &mut Tenancy) {
    assert_eq!(w.pages.len(), 2);
    assert_eq!(names(w, &w.pages[1]), ["A", "the bootstrap key"]);
    assert_eq!(w.pages[1].next, None);
}

#[when("a client with the bootstrap key revokes K")]
async fn bootstrap_revokes_k(w: &mut Tenancy) {
    let k = w.named["K"].clone();
    w.revoke(&w.bootstrap(), k).await;
    assert_eq!(w.outcome, Some(Ok(())));
}

#[when("the client with K lists events again")]
async fn k_lists_again(w: &mut Tenancy) {
    w.list_events(w.key()).await;
}

#[given("a client with the bootstrap key revoked key K")]
async fn bootstrap_revoked_k(w: &mut Tenancy) {
    w.open().await;
    let created = w
        .engine()
        .create_key(&w.bootstrap(), CreateKeyRequest::default())
        .await;
    let k = created.unwrap().info.key_id.to_string();
    w.named.insert("K".into(), k.clone());
    w.revoke(&w.bootstrap(), k).await;
    assert_eq!(w.outcome, Some(Ok(())));
}

#[when("the client revokes K again")]
async fn revokes_k_again(w: &mut Tenancy) {
    let k = w.named["K"].clone();
    w.revoke(&w.key(), k).await;
}

#[when("the client revokes a key ID no key has")]
async fn revokes_unknown(w: &mut Tenancy) {
    w.revoke(&w.key(), "019a9f3e-7c2b-7d41-8a3e-5f6b7c8d9e0f".into())
        .await;
}

#[then(regex = r#"^the call is refused as not found with "([^"]+)"$"#)]
fn not_found_with(w: &mut Tenancy, message: String) {
    let err = w.refusal();
    assert_eq!(
        (err.kind, err.message.as_str()),
        (ErrorKind::NotFound, &*message)
    );
}

#[when("the client revokes the bootstrap key")]
async fn revokes_bootstrap(w: &mut Tenancy) {
    let id = w.bootstrap_key.as_deref().unwrap()[4..40].to_owned();
    w.revoke(&w.key(), id).await;
}

#[then("the client's next call is refused as unauthenticated")]
async fn next_call_refused(w: &mut Tenancy) {
    let err = w
        .engine()
        .whoami(&w.key(), WhoamiRequest {})
        .await
        .expect_err("refused");
    assert_eq!(err.kind, ErrorKind::Unauthenticated);
}

#[then(regex = r#"^the client's next call is refused as unauthenticated with "([^"]+)"$"#)]
async fn next_call_refused_with(w: &mut Tenancy, message: String) {
    w.list_events(w.key()).await;
    refused_as(w, message);
}

#[given("the clock reads T and the key was refused")]
async fn refused_at_t(w: &mut Tenancy) {
    clock_reads(w, String::new());
    next_call_refused_with(w, "invalid API key".into()).await;
}

#[when("the clock steps back one hour")]
fn steps_back(w: &mut Tenancy) {
    let back = w.now().as_millis() - HOUR;
    w.clock.set(Timestamp::from_millis(back));
}

#[given("a client with the bootstrap key created keys A, B and C, with C expiring in one hour")]
async fn created_abc_expiring(w: &mut Tenancy) {
    w.open().await;
    let hour = Timestamp::from_millis(w.now().as_millis() + HOUR);
    for (name, expires_at) in [("A", None), ("B", None), ("C", Some(hour))] {
        let key = w.create(BTreeSet::new(), expires_at, None).await;
        w.named.insert(name.into(), key[4..40].to_owned());
        w.secrets.push(key[41..].to_owned());
    }
}

#[given("the client revoked key B")]
async fn client_revoked_b(w: &mut Tenancy) {
    let b = w.named["B"].clone();
    w.revoke(&w.key(), b).await;
    assert_eq!(w.outcome, Some(Ok(())));
}

#[given("two hours pass")]
fn two_hours_pass(w: &mut Tenancy) {
    let later = w.now().as_millis() + 2 * HOUR;
    w.clock.set(Timestamp::from_millis(later));
}

#[when("the client lists keys")]
async fn client_lists_keys(w: &mut Tenancy) {
    let page = w
        .engine()
        .list_keys(&w.key(), ListKeysRequest::default())
        .await;
    w.pages.push(page.expect("the client lists keys"));
}

#[then(r#"A is "active", B is "revoked" and C is "expired""#)]
fn statuses(w: &mut Tenancy) {
    let status = |name: &str| {
        let keys = &w.pages[0].keys;
        let key = keys.iter().find(|k| k.key_id.to_string() == w.named[name]);
        key.expect("listed").status
    };
    assert_eq!(
        (status("A"), status("B"), status("C")),
        (KeyStatus::Active, KeyStatus::Revoked, KeyStatus::Expired)
    );
}

#[then("no listed key holds a secret")]
fn no_listed_secret(w: &mut Tenancy) {
    // `KeyInfo` has no field for one; nor does any text a listing prints.
    let shown = format!("{:?}", w.pages);
    let bootstrap = &w.bootstrap_key.as_deref().unwrap()[41..];
    for secret in w.secrets.iter().map(String::as_str).chain([bootstrap]) {
        assert!(!shown.contains(secret), "{shown}");
    }
}

/// A seed's text: a well-formed key with a fresh key ID.
fn new_seed() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    // The secret is 43 characters, and the last carries only 4 bits, so it stays "A".
    format!("ixk_019a9f3e-7c2b-7d41-8a3e-{n:012x}_{}A", "a".repeat(42))
}

/// `key`'s text with another secret.
fn other_secret(key: &str) -> String {
    format!("{}{}A", &key[..41], "c".repeat(42))
}

#[when("the engine is opened with a seed key")]
#[given("the engine was opened with seed key A")]
async fn opened_with_a(w: &mut Tenancy) {
    let a = new_seed();
    w.texts.insert("A".into(), a.clone());
    w.open_with(Some(&a)).await.expect("the engine opens");
}

#[then("no key is returned and the open reports the seed as current")]
fn reported_current(w: &mut Tenancy) {
    assert_eq!(w.returned, Some(false));
    assert_eq!(w.reported, Some(SeedState::Current));
}

#[then(regex = r#"^the seed key authenticates with scopes (.+)$"#)]
async fn seed_authenticates(w: &mut Tenancy, names: String) {
    let me = w.whoami_with("A").await.expect("the seed authenticates");
    assert_eq!(me.scopes, scopes(&names));
}

#[when("the engine is reopened with seed key B, which has another key ID")]
#[given("the engine was reopened with seed key B")]
async fn reopened_with_b(w: &mut Tenancy) {
    let b = new_seed();
    w.texts.insert("B".into(), b.clone());
    w.reopen_with(Some(&b)).await;
    assert_eq!(w.reported, Some(SeedState::Current));
}

#[when(
    regex = r"^the engine is reopened with (seed key A|no seed|a seed with K's key ID|a seed with A's key ID and another secret)$"
)]
async fn reopened_with(w: &mut Tenancy, which: String) {
    let seed = match which.as_str() {
        // A seed added later (ACC-013) is first named here.
        "seed key A" => Some(w.texts.entry("A".into()).or_insert_with(new_seed).clone()),
        "no seed" => None,
        "a seed with K's key ID" => Some(other_secret(w.text("K"))),
        _ => Some(other_secret(w.text("A"))),
    };
    w.reopen_with(seed.as_deref()).await;
}

#[then(regex = r"^key (A|B) authenticates as an admin key$")]
async fn authenticates_as_admin(w: &mut Tenancy, name: String) {
    let me = w.whoami_with(&name).await.expect("an admin key");
    let every = [Scope::Read, Scope::Write, Scope::Admin, Scope::Execute];
    assert_eq!(me.scopes, BTreeSet::from(every));
    let req = ListKeysRequest::default();
    let listed = w.engine().list_keys(&call(w.text(&name)), req).await;
    listed.expect("an admin key lists keys");
}

#[then(regex = r"^key (A|B) is refused as an invalid key$")]
async fn refused_as_invalid_key(w: &mut Tenancy, name: String) {
    let err = w.whoami_with(&name).await.expect_err("refused");
    assert_eq!(
        (err.kind, err.message.as_str(), err.hint),
        (ErrorKind::Unauthenticated, "invalid API key", None)
    );
}

#[then(regex = r"^key (A|K) still authenticates$")]
async fn key_still_authenticates(w: &mut Tenancy, name: String) {
    assert!(w.refused.is_none(), "{:?}", w.refused);
    w.whoami_with(&name).await.expect("it authenticates");
}

#[when(regex = r"^the engine is opened with (.+) as the seed$")]
async fn opened_with_malformed(w: &mut Tenancy, which: String) {
    let good = new_seed();
    let text = match which.as_str() {
        r#""not-a-key""# => "not-a-key".to_owned(),
        "a valid key with one character removed" => good[..83].to_owned(),
        "a valid key whose ID is in uppercase" => good.replace("019a9f3e", "019A9F3E"),
        "a valid key whose ID is not a UUIDv7" => good.replace("-7d41-", "-4d41-"),
        other => panic!("no such seed: {other}"),
    };
    assert!(text != good);
    w.presented = Some(text.clone());
    w.reopen_with(Some(&text)).await;
}

#[then("the open is refused and names the key format")]
fn open_refused_naming_format(w: &mut Tenancy) {
    let err = w.refused.as_ref().expect("a refused open");
    assert!(matches!(err, EngineError::MalformedSeed), "{err:?}");
    assert!(err.to_string().contains("ixk_<key-id>_<secret>"), "{err}");
}

#[then("the error does not contain the seed")]
fn seed_not_echoed(w: &mut Tenancy) {
    let err = w.refused.as_ref().expect("a refused open");
    let shown = format!("{err} {err:?}");
    let seed = w.presented.as_deref().unwrap();
    // Neither the whole text nor, for a key, its secret alone.
    for part in [Some(seed), seed.get(41..)].into_iter().flatten() {
        assert!(!shown.contains(part), "{shown}");
    }
}

#[given(regex = r"^a client with key A created (a key|an admin key) K$")]
async fn a_created_k(w: &mut Tenancy, kind: String) {
    let scopes = match kind.as_str() {
        "a key" => BTreeSet::from([Scope::Read]),
        _ => BTreeSet::from([Scope::Admin]),
    };
    let req = CreateKeyRequest {
        scopes,
        ..CreateKeyRequest::default()
    };
    let created = w.engine().create_key(&call(w.text("A")), req).await;
    let key = created.expect("key A creates a key").key;
    w.texts.insert("K".into(), key.expose().to_owned());
}

#[given("nothing else happened")]
fn nothing_else(_: &mut Tenancy) {}

#[then("the open is refused as a seed conflict")]
fn seed_conflict(w: &mut Tenancy) {
    let err = w.refused.as_ref().expect("a refused open");
    assert!(matches!(err, EngineError::SeedConflict), "{err:?}");
}

#[given("a client with key K revoked key A")]
async fn k_revoked_a(w: &mut Tenancy) {
    let (k, a) = (call(w.text("K")), w.text("A")[4..40].to_owned());
    w.revoke(&k, a).await;
    assert_eq!(w.outcome, Some(Ok(())));
}

#[then("the open succeeds and reports the seed as revoked")]
fn reported_revoked(w: &mut Tenancy) {
    assert!(w.refused.is_none(), "{:?}", w.refused);
    assert_eq!(w.reported, Some(SeedState::Revoked));
}

#[given(
    regex = r#"^the engine was opened, a client emitted event "([^"]+)", and the engine was closed$"#
)]
async fn emitted_and_closed(w: &mut Tenancy, name: String) {
    w.open().await;
    w.emit(named(&name)).await;
    assert_eq!(w.outcome, Some(Ok(())));
    w.engine.take().unwrap().close().await;
}

#[when(
    regex = r"^the (control plane file is deleted|control plane file and the lock file are deleted|environment's ledger directory is deleted|environment's ledger directory is emptied)$"
)]
fn lost(w: &mut Tenancy, what: String) {
    let root = w.dir.path();
    let entries = std::fs::read_dir(root.join("ledgers")).unwrap();
    let ledgers: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
    let [ledger] = ledgers.as_slice() else {
        panic!("one ledger: {ledgers:?}");
    };
    match what.as_str() {
        "control plane file is deleted" => {
            std::fs::remove_file(root.join("control.db")).unwrap();
        }
        "control plane file and the lock file are deleted" => {
            std::fs::remove_file(root.join("control.db")).unwrap();
            std::fs::remove_file(root.join("engine.lock")).unwrap();
        }
        "environment's ledger directory is deleted" => {
            std::fs::remove_dir_all(ledger).unwrap();
        }
        _ => {
            std::fs::remove_dir_all(ledger).unwrap();
            std::fs::create_dir(ledger).unwrap();
        }
    }
    w.contents = Some(w.read_directory());
}

#[when("the engine is opened again")]
#[when("an engine is opened on that directory")]
async fn opened_again(w: &mut Tenancy) {
    w.reopen_with(None).await;
}

#[then(regex = r"^the open is refused as (orphaned|ledger missing|not an engine directory)$")]
fn layout_refused(w: &mut Tenancy, refusal: String) {
    let err = w.refused.as_ref().expect("a refused open");
    let matches = match refusal.as_str() {
        "orphaned" => matches!(err, EngineError::Orphaned),
        "ledger missing" => matches!(err, EngineError::LedgerMissing),
        _ => matches!(err, EngineError::NotAnEngineRoot),
    };
    assert!(matches, "{err:?}");
}

#[then("no key is returned, and nothing in the directory changed but the lock file")]
fn only_the_lock_changed(w: &mut Tenancy) {
    assert_eq!(w.returned, None);
    let lock = w.dir.path().join("engine.lock");
    let mut before = w.contents.take().unwrap();
    let mut after = w.read_directory();
    before.remove(&lock);
    after.remove(&lock);
    assert!(after == before, "{:?}", after.keys());
}

#[given("a ledger directory written by the ledger alone")]
async fn ledger_alone(w: &mut Tenancy) {
    let ledger = Ledger::open(w.dir.path(), new_env_ref(), w.clock.clone()).await;
    ledger.expect("a ledger opens").close().await;
    w.contents = Some(w.read_directory());
}

#[then("nothing in the directory changed")]
fn nothing_changed(w: &mut Tenancy) {
    let before = w.contents.take().unwrap();
    let after = w.read_directory();
    assert!(after == before, "{:?}", after.keys());
}

#[given("an engine is open on the directory with seed key A")]
async fn open_with_a(w: &mut Tenancy) {
    opened_with_a(w).await;
}

#[when("a second engine is opened on the same directory with seed key B")]
async fn second_opened_with_b(w: &mut Tenancy) {
    let b = new_seed();
    w.texts.insert("B".into(), b.clone());
    let second = Engine::open(w.dir.path(), w.clock.clone(), Some(Secret::new(b))).await;
    w.refused = second.err();
}

#[then("the second open is refused as locked")]
fn second_locked(w: &mut Tenancy) {
    let err = w.refused.take().expect("a refused open");
    assert!(matches!(err, EngineError::Locked), "{err:?}");
}

#[then("key A still authenticates on the first engine")]
async fn a_on_the_first(w: &mut Tenancy) {
    authenticates_as_admin(w, "A".into()).await;
}

#[then("key B is refused on the first engine")]
async fn b_on_the_first(w: &mut Tenancy) {
    refused_as_invalid_key(w, "B".into()).await;
}

#[given("a client with the bootstrap key revoked the bootstrap key")]
async fn bootstrap_revoked_itself(w: &mut Tenancy) {
    w.open().await;
    revokes_bootstrap(w).await;
    assert_eq!(w.outcome, Some(Ok(())));
}

#[when(regex = r"^an admin key is recovered from the (?:fresh )?engine directory$")]
fn recovers(w: &mut Tenancy) {
    match recover_admin_key(w.dir.path(), w.clock.clone()) {
        Ok(key) => {
            w.texts.insert("R".into(), key.expose().to_owned());
        }
        Err(err) => w.not_recovered = Some(err),
    }
}

#[then(regex = r"^the recovered key authenticates with scopes (.+)$")]
async fn recovered_authenticates(w: &mut Tenancy, names: String) {
    let me = w.whoami_with("R").await.expect("the key authenticates");
    assert_eq!(me.scopes, scopes(&names));
}

#[then(r#"listing keys shows it with origin "recovered" and label "recovered""#)]
async fn listed_recovered(w: &mut Tenancy) {
    let req = ListKeysRequest {
        limit: None,
        cursor: None,
    };
    let page = w.engine().list_keys(&call(w.text("R")), req).await;
    let id = &w.text("R")[4..40];
    let keys = page.expect("an admin key lists keys").keys;
    let key = keys.iter().find(|k| k.key_id.to_string() == id);
    let key = key.expect("the recovered key");
    assert_eq!(key.origin, KeyOrigin::Recovered);
    assert_eq!(key.label.as_deref(), Some("recovered"));
    assert_eq!(key.created_by, None);
}

#[then("the revoked bootstrap key is still refused")]
async fn revoked_bootstrap_refused(w: &mut Tenancy) {
    next_call_refused_with(w, "invalid API key".into()).await;
}

#[then("the recovery is refused as not ready")]
fn recovery_not_ready(w: &mut Tenancy) {
    let err = w.not_recovered.as_ref().expect("a refused recovery");
    assert!(matches!(err, RecoverError::NotReady), "{err:?}");
}

#[then("the directory is still empty")]
fn still_empty(w: &mut Tenancy) {
    assert_eq!(w.read_directory(), BTreeMap::new());
}

/// Runs every tenancy scenario, through the binary or on the engine: whether any failed, and how
/// many passed.
pub async fn run(parser: super::Unescaped, remote: bool) -> (bool, usize) {
    let writer = Tenancy::cucumber::<&str>()
        .with_parser(parser)
        .before(move |_, _, _, world| {
            world.remote = remote;
            if remote {
                // The machine's time, read once a scenario.
                world.clock = ManualClock::new(HostClock.now());
            }
            Box::pin(async {})
        })
        .after(move |_, _, _, _, world| {
            Box::pin(async move {
                if let Some(w) = world {
                    if let Some(engine) = w.engine.take() {
                        assert_eq!(matches!(engine, Adapter::Binary(..)), remote, "the adapter");
                        engine.close().await;
                    }
                    if let Some((engine, _dir)) = w.other.take() {
                        engine.close().await;
                    }
                }
            })
        })
        .max_concurrent_scenarios(if remote { BINARIES } else { 64 })
        .fail_on_skipped()
        .filter_run(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../vendor/foundation/docs/behaviors/tenancy.feature"
            ),
            move |_, _, scenario| !remote || runs_remotely(scenario),
        )
        .await;
    let passed = writer.scenarios_stats().passed;
    (writer.execution_has_failed(), passed)
}
