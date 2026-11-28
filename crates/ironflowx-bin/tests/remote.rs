//! An engine behind the HTTP door, called over the network.

#[allow(dead_code)]
#[path = "../../../vendor/foundation/crates/ironflowx-operations/tests/refusals/mod.rs"]
mod refusals;

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::process::Command;
use std::sync::Arc;

use connectrpc::Protocol;
use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use ironflowx_doors::{Readiness, Slot, WaitingOn, router};
use ironflowx_operations::{Engine, Opened};
use ironflowx_proto::wire::{self, EventServiceClient};
use ironflowx_proto::{
    Client, EmitBatchRequest, EmitRequest, Event, EventPage, GetEventRequest, ListEventsRequest,
    Operations as _,
};
use refusals::{clock, key};

struct Ready;

impl Readiness for Ready {
    async fn ready(&self) -> Result<(), WaitingOn> {
        Ok(())
    }
}

/// Serves `engine` on a port of its own.
async fn serve(engine: Arc<Engine>) -> SocketAddr {
    let slot = Arc::new(Slot::starting());
    slot.open(engine, Ready);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(slot)).await });
    addr
}

/// TEN-7, through the client.
#[tokio::test(flavor = "multi_thread")]
async fn the_five_refusals_are_one_answer_through_the_client() {
    refusals::five(async |engine| {
        let addr = serve(engine.clone()).await;
        Arc::new(Client::new(&format!("http://{addr}")).unwrap())
    })
    .await;
}

/// Writes JSON text at random, the same each time (xorshift), with whitespace between its tokens or
/// without, which changes nothing else.
struct Json(u64, bool);

impl Json {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    fn any<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[self.below(of.len() as u64) as usize]
    }

    fn space(&mut self, out: &mut String) {
        for _ in 0..self.below(3) {
            let space = self.any(&["", " ", "\n", "\r\n", "\t", "  "]);
            out.push_str(if self.1 { space } else { "" });
        }
    }

    fn string(&mut self, out: &mut String) {
        out.push('"');
        for _ in 0..self.below(8) {
            out.push_str(self.any(&[
                "a",
                "Z",
                " ",
                " : ",
                "é",
                "e\u{301}",
                "\u{7f}",
                "\u{2028}",
                "漢",
                "😀",
                "/",
                "'",
                "<",
                "&",
                "\u{feff}",
                "\\\"",
                "\\\\",
                "\\/",
                "\\b",
                "\\f",
                "\\n",
                "\\r",
                "\\t",
                "\\u0001",
                "\\u001F",
                "\\u00e9",
                "\\u00E9",
                "\\ud83d\\ude00",
                "\\uFFFF",
                "\u{ffff}",
            ]));
        }
        out.push('"');
    }

    fn number(&mut self, out: &mut String) {
        out.push_str(self.any(&["", "-"]));
        match self.below(4) {
            0 => out.push_str(&(u64::MAX - self.below(1 << 40)).to_string()),
            1 => out.push_str(&((1 << 53) + self.below(1 << 20)).to_string()),
            // At most 30 digits, the first no zero.
            2 => {
                out.push('9');
                out.push_str(&"00719925474099310071992547409"[self.below(29) as usize..]);
            }
            _ => out.push_str(self.any(&["0", "0.0", "1.50", "1e2", "1E+2", "2.5e-3", "1e-400"])),
        }
    }

    fn object(&mut self, depth: u64, out: &mut String) {
        out.push('{');
        let keys = self.below(5);
        for i in 0..keys {
            self.space(out);
            let mut key = String::new();
            self.string(&mut key);
            // No key twice, in an order of its own.
            let at = self.below(90);
            key.insert_str(1, &format!("{}{}", at * 7 + i, keys - i));
            out.push_str(&key);
            self.space(out);
            out.push(':');
            self.value(depth, out);
            if i + 1 < keys {
                out.push(',');
            }
        }
        self.space(out);
        out.push('}');
    }

    fn value(&mut self, depth: u64, out: &mut String) {
        self.space(out);
        match self.below(if depth == 0 { 4 } else { 7 }) {
            0 => out.push_str(self.any(&["null", "true", "false"])),
            1 => self.number(out),
            2 | 3 => self.string(out),
            4 => {
                out.push('[');
                let items = self.below(5);
                for i in 0..items {
                    self.value(depth - 1, out);
                    out.push_str(if i + 1 < items { "," } else { "" });
                }
                self.space(out);
                out.push(']');
            }
            _ => self.object(depth - 1, out),
        }
        self.space(out);
    }
}

/// 100 events' data and metadata, with whitespace between their tokens or without.
fn documents(spaced: bool) -> Vec<(String, String)> {
    let mut json = Json(0x9e37_79b9_7f4a_7c15, spaced);
    let document = |_| {
        let (mut data, mut metadata) = (String::new(), String::new());
        json.value(4, &mut data);
        json.space(&mut metadata);
        json.object(2, &mut metadata);
        json.space(&mut metadata);
        (data, metadata)
    };
    (0..100).map(document).collect()
}

const PROTOCOLS: [&str; 4] = ["Connect JSON", "Connect binary", "gRPC", "gRPC-Web"];

/// What a client of `protocol` is made of.
fn transport(protocol: &str, addr: SocketAddr) -> (HttpClient, ClientConfig) {
    let config = ClientConfig::new(format!("http://{addr}").parse().unwrap());
    let http = match protocol {
        "gRPC" => HttpClient::plaintext_http2_only(),
        _ => HttpClient::plaintext(),
    };
    let config = match protocol {
        "Connect JSON" => config.json(),
        "Connect binary" => config,
        "gRPC" => config.with_protocol(Protocol::Grpc),
        "gRPC-Web" => config.with_protocol(Protocol::GrpcWeb),
        _ => unreachable!(),
    };
    (http, config)
}

fn client(protocol: &str, addr: SocketAddr) -> EventServiceClient<HttpClient> {
    let (http, config) = transport(protocol, addr);
    EventServiceClient::new(http, config)
}

/// SRV-5: whatever JSON an event holds, sent over each protocol, the engine stores those bytes and
/// each protocol reads them back.
#[tokio::test(flavor = "multi_thread")]
async fn event_data_crosses_every_protocol_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), clock(), None).await;
    let Opened { engine, minted, .. } = opened.unwrap();
    let admin = minted.unwrap();
    let bearer = || {
        let value = format!("Bearer {}", admin.expose());
        CallOptions::default().with_header("authorization", value)
    };
    let engine = Arc::new(engine);
    let addr = serve(engine.clone()).await;

    let documents = documents(true);
    // What a decoder that is not a copy would change is there.
    for held in [
        "\r\n",
        "\\ud83d",
        "\u{2028}",
        "-0",
        "90071992547",
        "184467",
        "e\u{301}",
    ] {
        let holds = |(d, m): &(String, String)| d.contains(held) || m.contains(held);
        assert!(documents.iter().any(holds), "{held:?}");
    }
    let mut stored = Vec::new();
    for sent_over in PROTOCOLS {
        let items = documents.iter().map(|(data, metadata)| wire::EmitRequest {
            name: "a".into(),
            data: Some(data.clone()),
            metadata: Some(metadata.clone()),
            ..Default::default()
        });
        let items: Vec<_> = items.collect();
        let sender = client(sent_over, addr);
        let first = sender
            .emit_with_options(items[0].clone(), bearer())
            .await
            .unwrap()
            .into_owned();
        stored.push(first.event_id.clone());
        let request = wire::EmitBatchRequest {
            items,
            ..Default::default()
        };
        let batch = sender.emit_batch_with_options(request, bearer()).await;
        let batch = ironflowx_proto::EmitBatchResponse::try_from(batch.unwrap().into_owned());
        let batch = batch.unwrap().items;
        assert_eq!(batch.len(), documents.len());
        for (item, (data, metadata)) in batch.iter().zip(&documents) {
            assert!(item.is_ok(), "{item:?}: {data:?} {metadata:?}");
            stored.push(item.as_ref().unwrap().event_id.to_string());
        }
        let expected = |page: EventPage| {
            // Newest first.
            let got: Vec<_> = page
                .events
                .into_iter()
                .rev()
                .map(|e| {
                    let text = |bytes| String::from_utf8(bytes).unwrap();
                    (text(e.data), text(e.metadata.unwrap()))
                })
                .collect();
            assert_eq!(got, documents, "sent over {sent_over}");
        };
        let cx = key(admin.expose());
        let newest = ListEventsRequest {
            limit: NonZeroU32::new(100),
            cursor: None,
        };
        expected(engine.list_events(&cx, newest.clone()).await.unwrap());
        let remote = Client::new(&format!("http://{addr}")).unwrap();
        expected(remote.list_events(&cx, newest).await.unwrap());
        for read_over in PROTOCOLS {
            let reader = client(read_over, addr);
            let request = wire::ListEventsRequest {
                limit: Some(100),
                ..Default::default()
            };
            let page = reader.list_events_with_options(request, bearer()).await;
            expected(page.unwrap().into_owned().try_into().unwrap());
            let request = wire::GetEventRequest {
                event_id: first.event_id.clone(),
                ..Default::default()
            };
            let one = reader.get_event_with_options(request, bearer()).await;
            let one = one.unwrap().into_owned();
            let (data, metadata) = &documents[0];
            assert_eq!(one.data.as_ref(), Some(data), "{read_over}");
            assert_eq!(one.metadata.as_ref(), Some(metadata), "{read_over}");
        }
    }
    // Those, each under the ID its emit was answered, and no other: newest first.
    let mut listed = Vec::new();
    let mut cursor = None;
    loop {
        let request = ListEventsRequest {
            limit: NonZeroU32::new(200),
            cursor,
        };
        let page = engine.list_events(&key(admin.expose()), request).await;
        let page = page.unwrap();
        listed.extend(page.events.iter().map(|e| e.event_id.to_string()));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    stored.reverse();
    assert_eq!(listed, stored);
}

/// SRV-5: the CLI sends JSON from its arguments and from files as it is there, and prints the same
/// values, key order and number text, on one line.
#[tokio::test(flavor = "multi_thread")]
async fn the_cli_prints_event_data_as_it_was_sent() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), clock(), None).await;
    let Opened { engine, minted, .. } = opened.unwrap();
    let admin = minted.unwrap();
    let engine = Arc::new(engine);
    let addr = serve(engine.clone()).await;
    let files = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ironflowx"));
        command
            .args(args)
            .current_dir(files.path())
            .env("IRONFLOWX_URL", format!("http://{addr}"))
            .env("IRONFLOWX_KEY", admin.expose());
        let out = tokio::task::block_in_place(|| command.output()).unwrap();
        assert_eq!(
            (out.status.code(), &*out.stderr),
            (Some(0), &b""[..]),
            "{args:?}"
        );
        let line = String::from_utf8(out.stdout).unwrap();
        let line = line.strip_suffix('\n').expect("a line").to_owned();
        assert!(!line.contains(['\n', '\r']), "{line}");
        line
    };
    let write = |name: &str, text: &str| std::fs::write(files.path().join(name), text).unwrap();

    let (sent, compact) = (documents(true), documents(false));
    // What a filter that does not know a string would change is there.
    for held in [" : ", "\\\" ", "\\\\\"", "\r\n", "-0", "90071992547"] {
        let holds = |(d, m): &(String, String)| d.contains(held) || m.contains(held);
        assert!(sent.iter().any(holds), "{held:?}");
    }
    // All at once, in a file as a person would keep it; then some again, each from files of its
    // own or as it is.
    let items = sent.iter().map(
        |(data, metadata)| serde_json::json!({"name": "a", "data": data, "metadata": metadata}),
    );
    let items: Vec<_> = items.collect();
    write(
        "batch",
        &format!("{:#}\n", serde_json::json!({ "items": items })),
    );
    run(&["events", "emit-batch", "--json", "@batch"]);
    for (i, (data, metadata)) in sent.iter().enumerate().take(40) {
        let mut args = vec!["events", "emit", "--name", "a", "--data", data];
        args.extend(["--metadata", metadata]);
        if i % 2 == 0 {
            write("data", data);
            write("metadata", metadata);
            (args[5], args[7]) = ("@data", "@metadata");
        }
        run(&args);
    }

    let request = ListEventsRequest {
        limit: NonZeroU32::new(200),
        cursor: None,
    };
    let page = engine.list_events(&key(admin.expose()), request).await;
    let mut stored = page.unwrap().events;
    // Oldest first.
    stored.reverse();
    let expected = sent.iter().chain(&sent[..40]);
    let got = stored.iter().map(|e| (&*e.data, e.metadata.as_deref()));
    let as_sent = expected.map(|(d, m)| (d.as_bytes(), Some(m.as_bytes())));
    assert!(got.eq(as_sent), "the engine stores the bytes");

    let printed = stored.iter().zip(compact.iter().chain(&compact[..40]));
    let printed = printed.map(|(event, (data, metadata))| {
        format!(
            r#"{{"data":{data},"emitted_by":"{}","event_id":"{}","metadata":{metadata},"name":"a","time":"2027-01-15T08:00:00Z","version":"1"}}"#,
            event.emitted_by, event.event_id
        )
    });
    let mut printed: Vec<String> = printed.collect();
    let newest = printed.len() - 1;
    let id = stored[newest].event_id.to_string();
    assert_eq!(run(&["events", "get", "--event-id", &id]), printed[newest]);
    printed.reverse();
    let page = format!(r#"{{"events":[{}]}}"#, printed.join(","));
    assert_eq!(run(&["events", "list", "--limit", "200"]), page);
}

/// D73: the largest page an engine answers, every event of it at every limit, is whole on each
/// protocol; a byte more and it is another page.
#[tokio::test(flavor = "multi_thread")]
async fn the_largest_page_of_events_fits() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), clock(), None).await;
    let Opened { engine, minted, .. } = opened.unwrap();
    let admin = minted.unwrap();
    let engine = Arc::new(engine);
    let addr = serve(engine.clone()).await;
    let client_of_ours = Client::new(&format!("http://{addr}")).unwrap();
    let cx = key(admin.expose());
    let item = |i: usize, size: usize| EmitRequest {
        name: "\"".repeat(256),
        // A JSON string of backslashes, where escaping doubles every byte.
        data: Some({
            let odd = size % 2;
            let (a, b) = ("a".repeat(odd), "\\".repeat(size - 12 - odd));
            format!("\"{a}{b}\"").into_bytes()
        }),
        metadata: Some(br#"{"a":"\\"}"#.to_vec()),
        version: Some(i32::MAX.into()),
        idempotency_key: Some(format!("{}{i:03}", "\u{1}".repeat(253))),
    };
    let emit = async |items: Vec<EmitRequest>| {
        let request = EmitBatchRequest { items };
        let answer = client_of_ours.emit_batch(&cx, request).await.unwrap();
        let ids = answer.items.into_iter().map(|item| item.unwrap().event_id);
        ids.collect::<Vec<_>>()
    };
    // 3 MiB of data and metadata in 200 events.
    let size = |i| if i == 0 { 15_856 } else { 15_728 };
    emit((0..100).map(|i| item(i, size(i))).collect()).await;
    emit((100..200).map(|i| item(i, size(i))).collect()).await;
    let request = || ListEventsRequest {
        limit: NonZeroU32::new(200),
        cursor: None,
    };
    let page = engine.list_events(&cx, request()).await.unwrap();
    let held = |e: &Event| e.data.len() + e.metadata.as_ref().unwrap().len();
    let bytes: usize = page.events.iter().map(held).sum();
    assert_eq!((page.events.len(), bytes), (200, 3 * 1024 * 1024));
    assert_eq!(page.next, None);
    let got = client_of_ours.list_events(&cx, request()).await;
    assert!(got == Ok(page.clone()));
    for protocol in PROTOCOLS {
        let (http, config) = transport(protocol, addr);
        // What the docs tell a client of JSON to take; the others take any page as they are.
        let config = match protocol {
            "Connect JSON" => config.with_default_max_message_size(wire::MAX_MESSAGE),
            _ => config,
        };
        let value = format!("Bearer {}", admin.expose());
        let options = CallOptions::default().with_header("authorization", value);
        let request = wire::ListEventsRequest::try_from(request()).unwrap();
        let client = EventServiceClient::new(http, config);
        let answer = client.list_events_with_options(request, options).await;
        let answer = EventPage::try_from(answer.unwrap().into_owned());
        assert!(answer == Ok(page.clone()), "{protocol}");
    }
    // The newest event is a byte longer than the oldest, which leaves, so the next oldest does not
    // fit: those two are the next page.
    let newest = emit(vec![item(200, size(0) + 1)]).await[0].to_string();
    let newest = GetEventRequest { event_id: newest };
    let newest = engine.get_event(&cx, newest).await.unwrap();
    let longer = engine.list_events(&cx, request()).await.unwrap();
    assert_eq!(longer.events.len(), 199);
    assert_eq!(longer.events[0], newest);
    assert_eq!(longer.events[1..], page.events[..198]);
    let rest = ListEventsRequest {
        cursor: longer.next,
        ..request()
    };
    let rest = client_of_ours.list_events(&cx, rest).await.unwrap();
    assert_eq!((&rest.events[..], rest.next), (&page.events[198..], None));
}

/// D107, through the CLI: `keys create --scopes execute` makes a key that holds `execute` alone;
/// `whoami` shows it; an emit and a read are refused, each hint naming the scope they need.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_made_with_execute_holds_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let opened = Engine::open(dir.path(), clock(), None).await;
    let Opened { engine, minted, .. } = opened.unwrap();
    let admin = minted.unwrap().expose().to_owned();
    let addr = serve(Arc::new(engine)).await;
    let run = |key: &str, args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ironflowx"));
        command
            .args(args)
            .env("IRONFLOWX_URL", format!("http://{addr}"))
            .env("IRONFLOWX_KEY", key);
        tokio::task::block_in_place(|| command.output()).unwrap()
    };
    let json = |bytes: &[u8]| serde_json::from_slice::<serde_json::Value>(bytes).unwrap();
    let out = run(&admin, &["keys", "create", "--scopes", "execute"]);
    assert_eq!((out.status.code(), &*out.stderr), (Some(0), &b""[..]));
    let created = json(&out.stdout);
    assert_eq!(created["info"]["scopes"], serde_json::json!(["execute"]));
    let key = created["key"].as_str().expect("the new key");
    let out = run(key, &["whoami"]);
    assert_eq!((out.status.code(), &*out.stderr), (Some(0), &b""[..]));
    assert_eq!(json(&out.stdout)["scopes"], serde_json::json!(["execute"]));
    for (args, scope) in [
        (&["events", "emit", "--name", "order.placed"][..], "write"),
        (&["events", "list"][..], "read"),
    ] {
        let out = run(key, args);
        let expected = serde_json::json!({
            "code": "permission_denied",
            "message": format!("missing scope: {scope}"),
            "retryable": false,
            "hint": format!("create a key with scope {scope}, or use one that has it"),
        });
        assert_eq!(
            (out.status.code(), &*out.stdout, json(&out.stderr)),
            (Some(4), &b""[..], expected),
            "{args:?}"
        );
    }
}
