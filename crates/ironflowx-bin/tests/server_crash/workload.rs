//! Replay chooses the workload length and the writer crossing. The OS schedules the callers.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ironflowx_core::{ErrorKind, FaultPoint};
use ironflowx_operations::generate_seed;
use ironflowx_proto::{
    Call, Client, EmitBatchRequest, EmitRequest, EmitResponse, Event, GetEventRequest,
    ListEventsRequest, Operations as _,
};
use rustix::process::Signal;
use serde_json::Value;
use tokio::io::AsyncReadExt as _;
use tokio::sync::Barrier;
use tokio::task::JoinSet;

use super::{Process, admin, call, expect_answer, process};

const CLIENTS: usize = 4;
const DATA: &[u8] = br#"{ "b": 1,  "a": 2 }"#;
const META: &[u8] = br#"{ "fixture": "server-crash" }"#;

struct Write {
    items: Vec<EmitRequest>,
}

impl Write {
    fn new(round: usize, caller: usize) -> Self {
        let count = if caller < CLIENTS / 2 { 1 } else { 2 };
        let items = (0..count)
            .map(|item| {
                let name = format!("crash.round{round}.client{caller}.item{item}");
                let keyed = (caller + item).is_multiple_of(2);
                let idempotency_key = keyed.then(|| name.clone());
                EmitRequest {
                    name,
                    data: Some(DATA.to_vec()),
                    metadata: Some(META.to_vec()),
                    version: Some(2),
                    idempotency_key,
                }
            })
            .collect();
        Self { items }
    }

    async fn send(&self, client: &Client, auth: &Call) -> Reply {
        let result = match self.items.as_slice() {
            [item] => client
                .emit(auth, item.clone())
                .await
                .map(|answer| vec![answer]),
            items => client
                .emit_batch(
                    auth,
                    EmitBatchRequest {
                        items: items.to_vec(),
                    },
                )
                .await
                .map(|batch| {
                    batch
                        .items
                        .into_iter()
                        .map(|item| expect_answer(item, "each valid batch item succeeds"))
                        .collect()
                }),
        };
        match result {
            Ok(answers) => {
                assert_eq!(
                    answers.len(),
                    self.items.len(),
                    "whole batch acknowledgment"
                );
                assert!(answers.iter().all(|a| !a.duplicate), "every marker is new");
                Reply::Acknowledged(answers)
            }
            Err(error) => match error.kind {
                ErrorKind::Unavailable => Reply::Unavailable,
                ErrorKind::Unknown => Reply::Unknown,
                kind => panic!("unexpected write outcome: {kind:?}"),
            },
        }
    }

    async fn check(
        &self,
        reply: &Reply,
        listed: &mut BTreeMap<String, Event>,
        client: &Client,
        auth: &Call,
        emitter: &str,
    ) -> bool {
        let mut found = 0;
        for (i, item) in self.items.iter().enumerate() {
            let event = listed.remove(&item.name);
            if let Reply::Acknowledged(answers) = reply {
                let id = answers[i].event_id;
                let got = client
                    .get_event(
                        auth,
                        GetEventRequest {
                            event_id: id.to_string(),
                        },
                    )
                    .await;
                let got = expect_answer(got, "acknowledged ID reads back");
                assert!(event.as_ref() == Some(&got), "get matches the listed event");
                assert_eq!(got.event_id, id, "the read returned the acknowledged ID");
            }
            if let Some(event) = event {
                found += 1;
                assert!(event.data == DATA, "{} data bytes", item.name);
                assert!(
                    event.metadata.as_deref() == Some(META),
                    "{} metadata",
                    item.name
                );
                assert!(
                    event.idempotency_key == item.idempotency_key,
                    "idempotency key"
                );
                assert_eq!(event.version, 2);
                assert!(
                    event.emitted_by.to_string() == emitter,
                    "emitter matches the caller"
                );
            }
        }
        match reply {
            Reply::Acknowledged(_) => assert_eq!(found, self.items.len(), "every ack is present"),
            Reply::Unknown => assert!(found == 0 || found == self.items.len(), "no partial write"),
            Reply::Unavailable => assert_eq!(found, 0, "an unavailable write stores nothing"),
        }
        found != 0
    }
}

enum Reply {
    Acknowledged(Vec<EmitResponse>),
    Unknown,
    Unavailable,
}

async fn start_round(url: &str, auth: &Arc<Call>, round: usize) -> JoinSet<(Write, Reply)> {
    let mut tasks = JoinSet::new();
    let start = Arc::new(Barrier::new(CLIENTS + 1));
    for caller in 0..CLIENTS {
        let write = Write::new(round, caller);
        let client = Client::new(url)
            .unwrap()
            .with_timeout(Duration::from_secs(15));
        let (auth, start) = (auth.clone(), start.clone());
        tasks.spawn(async move {
            start.wait().await;
            let reply = write.send(&client, &auth).await;
            (write, reply)
        });
    }
    start.wait().await;
    tasks
}

async fn join(
    mut tasks: JoinSet<(Write, Reply)>,
    mut answers: Vec<(Write, Reply)>,
) -> Vec<(Write, Reply)> {
    while let Some(answer) = tasks.join_next().await {
        answers.push(answer.expect("the client task completed"));
    }
    assert_eq!(answers.len(), CLIENTS);
    answers
}

async fn list(client: &Client, auth: &Call) -> BTreeMap<String, Event> {
    let mut events = BTreeMap::new();
    let mut cursor = None;
    for _ in 0..=5 {
        let page = client
            .list_events(
                auth,
                ListEventsRequest {
                    limit: NonZeroU32::new(7),
                    cursor,
                },
            )
            .await;
        let page = expect_answer(page, "events list");
        for event in page.events {
            assert!(
                events.insert(event.name.clone(), event).is_none(),
                "a marker occurs once"
            );
        }
        cursor = page.next;
        if cursor.is_none() {
            return events;
        }
    }
    panic!("at most 30 events fit in six pages");
}

async fn crash(point: FaultPoint, warmup: usize, crossing: u64, drain: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let seed_file = dir.path().join("seed");
    let (seed_id, seed) = generate_seed();
    process::write_seed(&seed_file, &seed);
    let auth = Arc::new(call(&seed));
    let emitter = format!("key:{seed_id}");
    let stop = (warmup * CLIENTS) as u64 + crossing;
    let mut host = Process::serve(&root, Some(&seed_file), Some((point, stop)));
    let observer = host.ready().await;
    let url = host.url();
    let mut answered = Vec::new();
    for round in 0..warmup {
        answered.extend(join(start_round(&url, &auth, round).await, Vec::new()).await);
    }
    // A different client's read arrives after the acknowledged writes, while the host is live.
    let mut visible = list(&observer, &auth).await;
    for (write, reply) in &answered {
        assert!(
            matches!(reply, Reply::Acknowledged(_)),
            "the warmup is acknowledged"
        );
        write
            .check(reply, &mut visible, &observer, &auth, &emitter)
            .await;
    }
    assert!(visible.is_empty(), "live reads have no extra events");

    let mut interrupted = start_round(&url, &auth, warmup).await;
    host.at(point);
    // Before publication of the committed head, the observer still sees just the earlier writes.
    if matches!(point, FaultPoint::BeforeAppend | FaultPoint::AfterFsync) {
        let mut visible = list(&observer, &auth).await;
        for (write, reply) in &answered {
            write
                .check(reply, &mut visible, &observer, &auth, &emitter)
                .await;
        }
        let mut complete = 0;
        for caller in 0..CLIENTS {
            complete += usize::from(
                Write::new(warmup, caller)
                    .check(&Reply::Unknown, &mut visible, &observer, &auth, &emitter)
                    .await,
            );
        }
        assert_eq!(
            complete,
            crossing as usize - 1,
            "the parked write is invisible"
        );
        assert!(visible.is_empty(), "live reads have no extra events");
    }
    let mut during_drain = Vec::new();
    let held = if drain {
        let mut held = host.hold_request_body().await;
        host.signal(Signal::TERM);
        host.log("stopping");
        host.release();
        let mut byte = [0];
        loop {
            tokio::select! {
                biased;
                _ = held.read(&mut byte) => panic!("drain closed before acknowledging a write"),
                next = interrupted.join_next() => {
                    let answer = next.expect("drain ended without acknowledging a write")
                        .expect("the client task completed");
                    let acknowledged = matches!(answer.1, Reply::Acknowledged(_));
                    during_drain.push(answer);
                    if acknowledged {
                        break;
                    }
                }
            }
        }
        Some(held)
    } else {
        None
    };
    host.kill().check_secrets([&seed]);
    drop(held);
    let interrupted = join(interrupted, during_drain).await;

    let mut restart = Process::serve(&root, Some(&seed_file), None);
    let observer = restart.ready().await;
    admin(&observer, &seed, seed_id).await;
    let mut listed = list(&observer, &auth).await;
    for (write, reply) in &answered {
        write
            .check(reply, &mut listed, &observer, &auth, &emitter)
            .await;
    }
    let mut landed = 0;
    for (write, reply) in &interrupted {
        landed += usize::from(
            write
                .check(reply, &mut listed, &observer, &auth, &emitter)
                .await,
        );
    }
    if !drain {
        let expected = crossing as usize - usize::from(point == FaultPoint::BeforeAppend);
        assert_eq!(
            landed,
            expected,
            "the exact prefix made durable at {}",
            point.name()
        );
    }

    assert!(listed.is_empty(), "restart has no extra events");

    let (_, Reply::Acknowledged(answers)) = &answered[0] else {
        unreachable!()
    };
    let id = answers[0].event_id.to_string();
    let mut cli = Process::command();
    cli.args(["events", "get", "--event-id", &id, "--url", &restart.url()]);
    cli.arg("--key-file").arg(&seed_file);
    let cli = Process::start(cli, None).success();
    cli.check_secrets([&seed]);
    let event = serde_json::from_slice::<Value>(&cli.stdout).expect("CLI returned one JSON event");
    assert!(
        event["event_id"] == id,
        "CLI reads the acknowledged ID after restart"
    );
    restart.stop().check_secrets([&seed]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeded_random_kill() {
    let seed = std::env::var("SERVER_CRASH_SEED").ok().map_or_else(
        || {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
        },
        |seed| seed.parse().expect("numeric SERVER_CRASH_SEED"),
    );
    // SplitMix64 also disperses adjacent seeds and accepts zero.
    let mixed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let mixed = mixed ^ (mixed >> 31);
    let point = [
        FaultPoint::BeforeAppend,
        FaultPoint::AfterFsync,
        FaultPoint::AfterAck,
    ][(mixed % 3) as usize];
    let warmup = 1 + ((mixed >> 8) % 4) as usize;
    let crossing = 1 + (mixed >> 16) % ((CLIENTS - 1) as u64);
    use std::io::Write as _;
    writeln!(
        std::io::stderr(),
        "SERVER_CRASH_SEED={seed} ci/server-crash.sh seeded_random_kill ({}, round {warmup}, crossing {crossing})",
        point.name()
    ).expect("print replay seed");
    crash(point, warmup, crossing, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_during_drain() {
    crash(FaultPoint::AfterFsync, 1, 1, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_read_waits_for_fold() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let seed_file = dir.path().join("seed");
    let (_, seed) = generate_seed();
    process::write_seed(&seed_file, &seed);
    let mut host = Process::serve(&root, Some(&seed_file), Some((FaultPoint::AfterAck, 1)));
    let observer = host.ready().await;
    let writer = Client::new(&host.url())
        .unwrap()
        .with_timeout(Duration::from_secs(15));
    let auth = call(&seed);
    let write = Write::new(0, 0);
    let Reply::Acknowledged(answers) = write.send(&writer, &auth).await else {
        panic!("writer acknowledges before it folds");
    };
    host.at(FaultPoint::AfterAck);
    let id = answers[0].event_id;
    let read = tokio::spawn(async move {
        observer
            .get_event(
                &auth,
                GetEventRequest {
                    event_id: id.to_string(),
                },
            )
            .await
    });
    // Only the observer's read can cross this point, and only with the correct committed head.
    host.at(FaultPoint::ReadWaitingForFold);
    host.release();
    let got = expect_answer(
        read.await.expect("observer task"),
        "observer reads the acknowledged ID",
    );
    assert_eq!(got.event_id, id);
    host.stop().check_secrets([&seed]);
}
