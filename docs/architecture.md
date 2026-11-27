# Architecture

The `ironflowx` binary: `ironflowx serve`, the first host, which serves a foundation engine over HTTP, and the CLI, in the same binary, which calls one. The engine, its ledger, credentials, control plane, wire and door are foundation's library crates, described in [foundation's architecture](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md). This file names foundation's decisions and invariants by their foundation numbers.

## Context

`ironflowx serve` is a host as a process. Outside it:

- a client calls the engine over HTTP, by Connect, gRPC or gRPC-Web, with a key, and reaches it no other way; the CLI, in the same binary, is one, run by an agent in a shell;
- an orchestrator watches `/health` and `/ready`, with no key;
- the operator takes the admin key from `admin.key` in the data directory.

## Crates

| Crate | Is | Depends on |
|---|---|---|
| `ironflowx-bin` | The `ironflowx` binary and its host | `ironflowx-cli`; from foundation `ironflowx-doors` (`http`), `ironflowx-operations`, `ironflowx-proto` and `ironflowx-core`. The host reaches the ledger, control plane, event rules and credentials through the engine (`ci/no-host-engine-deps.sh`) |
| `ironflowx-cli` | The CLI: the generated commands, `describe` and `version` | From foundation `ironflowx-proto` and `ironflowx-core`, and no engine crate (SRV-1, `ci/no-engine-deps.sh`); at build time, `ironflowx-codegen`, which generates the commands from the proto (D52) |
| `ironflowx-behaviors` | Runs the behavior scenarios against the binary (D20) | Dev-dependencies only |

All three are unpublished. Foundation comes in as a git dependency pinned to one commit in the root `Cargo.toml`, until its 0.4.0 is on crates.io. The binary carries foundation's release number until 1.0.0.

## Modules and seams

In front of the engine, in `ironflowx serve`, a call goes through:

1. the host's accept loop, which reads the request ([Wire](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md#wire), "Timeouts");
2. the HTTP door, which decodes it, takes its key from `Authorization`, and converts it to an `Operations` request;
3. the slot, which answers for the engine until it is open;
4. the engine, through `Operations`; the answer, or the error, goes back the same way.

A CLI command is a client's side of that:

1. the CLI reads the command line, the key it was given and the engine's URL;
2. the generated call of the command asks the proto client, through `Operations`, which sends it with the key and the deadline;
3. the answer, as wire JSON with its defaults and its JSON text as values, goes to stdout; a failure, as the envelope, to stderr, with its kind's exit code ([CLI](#cli)).

`Operations` is a real seam: the engine in process and the proto client over Connect are its two adapters.

## Host

`ironflowx serve` binds, opens the engine, and serves it. SIGINT, SIGTERM or a ledger fault
starts a drain. Every acknowledgment already followed `fsync`.

| Setting | Flag | Variable | Default |
|---|---|---|---|
| Data directory | `--data-dir` | `IRONFLOWX_DATA_DIR` | `./.ironflowx` |
| Listen IP address and port | `--listen` | `IRONFLOWX_LISTEN` | `127.0.0.1:9310` |
| Seed | `--seed-file` | `IRONFLOWX_SEED_FILE` or `IRONFLOWX_SEED` | None |
| Log level | `--log-level` | `IRONFLOWX_LOG_LEVEL` | `info` |
| Log format | `--log-format` | `IRONFLOWX_LOG_FORMAT` | `json` |

A flag beats its variable. A seed file and an inline seed are two sources and are refused together.
The host trims surrounding whitespace off a seed and validates it before binding. An empty variable
is a value and is refused. The host warns once for each unread `IRONFLOWX_*` variable, naming the
variable without its value. It hides names that could carry credentials or arbitrary bytes.
`IRONFLOWX_URL` and `IRONFLOWX_KEY` are known CLI variables. There is no
settings file.

**Start.** In order:

1. Read and validate settings and any seed. Bad settings, a malformed seed or an unreadable seed file exit 2.
2. If the data directory holds `admin.key` and `control.db` is confirmed absent, exit 2: a stale key file, from another install.
3. Bind the listen address. In use exits 8, any other bind error exits 2. No file has been written (D62).
4. Log `listening` with the bound URL, also when port 0 picked the address. An address that is not loopback also logs a warning that keys travel in clear without a TLS proxy (D59).
5. Serve `/health` and the door, the slot `Starting`.
6. Create the data directory, and what is missing above it, asking for mode `0700`. Warn once about each leftover `admin.key.*.tmp` and keep it. Only a confirmed unmarked bootstrap with an occupied `admin.key` is stale (exit 2). For a seedless start, confirm the unmarked state after seeing the name is occupied, since another host can mark and publish concurrently. A seeded start leaves a bootstrap read error for the open. A seedless start on an unmarked or unreadable bootstrap checks the destination with `symlink_metadata`: an occupied name, including a dangling link, yields the stale refusal or the original read error, respectively. A lookup error other than not found is a refusal. When the destination is absent, probe before opening. If a healthy marked root has an occupied key path and the first read fails transiently, retry the start after the failure clears.
7. Open with `Engine::open(data_dir, HostClock, seed)`. Warn if it revoked an earlier minted key whose secret was lost, or if the supplied seed is revoked. Write a minted key to `admin.key` and log its path. On failure log the minted key ID and the instruction to run `ironflowx recover-key` and revoke that ID, close the engine and exit 1. Set the slot `Open` on success.

**Pre-mint probe.** Write and sync dummy data in the data directory, hard-link it, then remove
both probe-created names and sync the directory. Attempt cleanup of each created name even after
an earlier failure, and keep any existing entry that the probe hit in a collision. If the OS refuses
removal, name each failed removal by an exact safe absolute path or an exact filename relative to
the configured data directory. Any probe failure exits 1 before opening the engine or recovering a key.

The host uses an absolute locator in these warnings and errors only when the configured directory
is absolute and the locator is unambiguous and safe to print. Otherwise it names the file relative
to the configured data directory. When an old temporary's name itself cannot be shown safely,
the warning says the name was withheld and directs the operator to inspect `admin.key.*.tmp`
in the configured data directory locally.

`is_bootstrapped` reads the control plane without creating a directory or database. It answers
`true` for a marked bootstrap and `false` for a missing control plane, a control plane that
has never been bootstrapped, or an unmarked bootstrap. The read takes no engine lock and does
not check the ledger. SQLite can rebuild `control.db-shm` from a WAL left by a crash.

**Writing a key file.** Write the key and a newline to a new file in the data directory, `admin.key.<random>.tmp`, mode `0400`, and sync it; hard-link it to `admin.key`, which fails if the name is taken, by a symbolic link to nothing too; remove the temporary name; sync the directory. A crash leaves either no `admin.key` or the whole key, and at most the temporary file. A crash after the open marked the bootstrap and before the link leaves no `admin.key` and no sign to the next open: recover an admin key and revoke the minted one.

**Health.** Plain HTTP `GET`, no key:

| Path | 200 when | Else |
|---|---|---|
| `/health` | The process serves | No answer |
| `/ready` | The slot is `Open` and `Engine::health()` reports both parts ok: `{"ready":true}` | 503 with `{"ready":false,"waiting_on":"open" \| "control_plane" \| "ledger"}`, the control plane first |

`/ready` asks the engine on each request and has no deadline of its own, so it can answer as late as `Engine::health` does ([Commit and read](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md#commit-and-read)): a prober sets its own.

**Stop.** SIGINT, SIGTERM or `Engine::faulted()` starts a drain (D63, D72).
The slot stops admitting and the listener closes. Existing connections can still receive
the stopping refusal while requests in flight drain. The host allows up to 10 seconds, including
time to flush responses and close connections, then drops outstanding request futures, including
HTTP/2 handlers. It marks the slot closed and awaits `Engine::close_with_state` before shutting
down the runtime. Closing joins the ledger writer, which finishes queued submissions and the fold; event
acknowledgments already followed `fsync`. A dropped read can still hold `read.db` after engine
close until its blocking query ends or the process exits. A detached control-plane write still
uses a single SQLite transaction (D48, TEN-11).

A signal during open drains the transport on the same deadline while open finishes. The host
publishes any minted key before it closes the engine.
A second signal received during shutdown exits 1 immediately. The operating system can coalesce
identical pending signals. A signal alone exits 0. A ledger fault present when close starts exits 8,
even when a signal started the drain. The ledger reads its state at entry to close using the
same progress gate that publishes faults. A control-plane failure makes `/ready` answer
503 `control_plane` and serving continues. Restart runs the ledger's usual open and replay.
A sealed ledger opens again and faults on any emit that appends (D19). Serve it read-only.

**Logs.** JSON lines on stderr by default, or text with `--log-format text`.
`--log-level` takes `off`, `error`, `warn`, `info`, `debug` or `trace`.
At `info`, the host logs one line per call, with the operation, duration, outcome kind and the
presented key's ID, if the key is well formed. The ID is recorded even for a key the engine refuses.
Before `Operations`, the door logs without a key ID. A dropped request awaiting admission has
outcome `cancelled`. A transport refusal before the door can determine a domain kind has outcome
`refused`. It labels an unknown RPC in
`ironflowx.v1` as `unknown` without recording its path. Logs carry no key, seed, event data or
metadata. In a diagnostic path or message, the host hides words that could be credentials and
escapes control characters. The refusal's envelope is the last stderr line, regardless of the log format or level.

### Exit codes

Host errors use the same table as the CLI, from `ErrorKind::exit_code()`.

| Exit | Kind | Host outcome |
|---|---|---|
| 0 | Success | Stopped by a signal |
| 1 | `internal` | Engine or ledger format, orphaned engine, missing ledger, wrong incarnation, corrupt ledger, data directory creation failure, failed pre-mint probe, key-file publication or recovery output failure, signal registration failure. A second signal also exits 1 immediately |
| 2 | `invalid` | Bad settings, malformed or unreadable seed, a directory belonging to a ledger, a seed conflict, stale `admin.key`, bind error other than address in use, recovery with a missing data directory, unfinished bootstrap or occupied file destination |
| 3–7 | [Call errors](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md#errors) | Calls only |
| 8 | `unavailable` | Address in use, engine or ledger lock or I/O error, control-plane fault during serve startup or recovery, destination lookup error, a ledger fault before close |

**Recovering an admin key.** Run `ironflowx recover-key` with access to the data directory. It uses
`--data-dir`, then `IRONFLOWX_DATA_DIR`, then `./.ironflowx`. It works with or without a running
engine, uses no URL or existing key, and never creates the data directory. File delivery refuses
an existing `admin.key`, including a dangling symlink, then runs the pre-mint probe before
calling `recover_admin_key(data_dir, HostClock)`. On success it writes the whole key and a newline
to `admin.key` using the same no-clobber writer as startup.

An explicit `--stdout` skips file preflight and prints a JSON object with `key` and `key_id`,
indented on a terminal. It never touches `admin.key`. If file publication or printing fails
after the engine recovered a key, the refusal reports the key ID and says to use a working admin
key, or recover one to stdout, then revoke that ID. If two recoveries pass preflight for the same
file, the first successful link wins. The loser exits 1 and leaves the winner's file intact.

**Seed generation.** `ironflowx seed generate` prints one JSON object with `key` and `key_id`.
It indents the object on a terminal. It opens no engine and writes no file.

## CLI

`ironflowx <command>` calls an engine and prints what it answers. An agent in a shell branches on the exit code and reads JSON (D64, D65).

| Part | Rule |
|---|---|
| Commands | One per method of the proto, generated (D52): `environments list`, `events emit`, `events emit-batch`, `events get`, `events list`, `keys create`, `keys list`, `keys revoke` and `whoami`. Hand-written: `describe` and `version` here, `serve`, `recover-key` and `seed generate` in the host |
| Engine | `--url`, else `IRONFLOWX_URL`, else `http://127.0.0.1:9310`. `https://` too, for an engine behind a TLS proxy ([Wire](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md#wire)) |
| Key | `--key-file <path>`, else `IRONFLOWX_KEY`, else none, with surrounding whitespace trimmed. Nothing else: the CLI never reads `admin.key` unasked (D61). No key, and an empty one, is a call with no key, which the engine refuses; a file or a variable that is there and empty is still the one that was given. Text that is not UTF-8 is a malformed key |
| Arguments | A flag for each scalar field of the request, its name in kebab case (`--event-id`); the flag of a repeated field may be repeated (`--scopes read --scopes write`), and a field that holds messages has none. Its value is what wire JSON reads from a string (text, a number, an RFC 3339 time) or, for a `bool`, `true` or `false`; one its field does not take is a usage error that names the flag, but for what the wire mapping or the engine refuses (a limit of 0, an unknown scope). A `json_text` field takes JSON text, also text that begins with `-`, or `@<file>`, the file's text, and sends it byte for byte. Any other value that begins with `-` is given after `=`: `--label=-x`. A field without its flag is absent |
| `--json` | The whole request, as wire JSON, in place of its flags, with one of which it is a usage error: the text itself, `@<file>`, or `-`, stdin. It is `Invalid`, and nothing is sent, if a message in it, at any depth, has a field that its type does not have, which the message names (`the request has no field 'items[1].idempotncy_key'`), where the door ignores it (D79); if a `json_text` field is anything but a string, `null` included, with the hint `data is JSON text: pass a string`; if a message is an array, which the door reads as its fields in order; or if it does not decode, as a field given twice does not. Any other field that is `null` is absent |
| Output | The answer as wire JSON on stdout: one line, or indented if stdout is a terminal. A `json_text` field is printed as the JSON value it holds, on one line, with no whitespace between its tokens: its values, the order of its keys and the text of its numbers and strings are as sent (D65). Every field that is not `optional` is printed, defaults included; an absent `optional` field stays absent. (A message field has no default: one that is not `optional` is always set, since the answer printed is mapped from the domain type, D53.) The wire is unchanged. `--help` prints text and exits 0: the one exception |
| Errors | One line on stderr, the envelope; its kind's [exit code](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md#errors); nothing on stdout, unless a write to it failed midway. What answered may be no engine, so a message or a hint that repeats 43 characters in a row of the key the CLI was given, as many as a secret has, is not printed, and the envelope says so in its place; nor is an answer that does: `Internal`, also one that holds the caller's own key with reason, as stored event data can. So `events get` of an event that holds the key, in any field, and `events list` of a page with one, print nothing, whole; the event stays stored, and a caller with another key reads it. A `keys create` whose request repeats the key is `Invalid` and not sent, since its answer holds the label and the new key's only copy. A repeat that was changed (its case, split, encoded or cut shorter) is not found. A message can repeat the URL's path, where no key goes (D55) |
| Usage errors | An argument that is missing, unknown or of the wrong form, a URL that is not `http[s]://host[:port][/path]`, a `--key-file`, or a file of `--json` or a `json_text` flag, that cannot be read, and what `--json` refuses, are `Invalid`, exit 2, and nothing is sent. The message is one line and names a bad argument, or a value in `--json`, its whitespace as single spaces, or an unknown field in `--json`, as it is, but `<hidden>` for a word or a field that holds a key's `ixk_` or is as long as a key's secret; it never holds what `--key-file`, `--url` or `IRONFLOWX_URL` was set to, nor the file `--json` names, which may be or hold a secret. A build with clap's `debug` feature, which prints each argument as it parses it, reads none: every start is `Invalid`, `clap is built with its debug feature` (TEN-3) |
| Deadline | 30 s a call, sent as `Connect-Timeout-Ms` and held to by the client too: after it, `Unavailable` for a read and `Unknown` for a state change |

**The envelope** of a failure: `{"code": …, "message": …, "retryable": …, "hint": …}`. `code` is the kind's name in snake_case, as in `ErrorInfo`; `retryable` is `ErrorKind::retryable()`; `hint` is absent when there is none.

**`describe`** works offline, without a key (D67). Its catalog contains operations only.
Their scope and whether they change state come from the same rules as the engine and the client.

| Call | Answers |
|---|---|
| `ironflowx describe` | `{version, protocol, operations: [{command, rpc, scope, summary, changes_state}], exit_codes: [{code, kind, retryable}]}` |
| `ironflowx describe events emit` | The named operation's entry plus `input`, `output` and `example` |

The schemas use JSON Schema 2020-12 and close each declared message object with
`additionalProperties: false`. `input` describes the canonical wire JSON for `--json`,
with `json_text` as strings and 64-bit integers as bounded decimal strings. Timestamp schemas
check the local year and whole-millisecond precision. Enable the validator's `format`
assertions to check calendar dates. The wire decoder also checks that
applying the UTC offset leaves the instant in years 0001 through 9999. Run the example with `--json @file` or pipe it to
`--json -`. The wire and engine also validate the request. `output` describes the CLI's
printed answer, requiring the fields it always prints and allowing arbitrary JSON values
inside `json_text` fields.

After `unknown`, follow the operation's summary and the [error's hint](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md#errors). In particular,
retry an emit only with the same idempotency key on every item, or check what landed first.
After `unknown` or `unavailable` from `keys create`, use `keys list` before retrying, and
again after an engine reopen. Revoke any key that landed whose secret was lost.

**`version`** prints `{"version": …, "commit": …, "protocol": "ironflowx.v1"}`, and asks no engine. `commit` is the one the binary was built from, or `unknown` if it was not built in a git checkout.

## Depends on

Foundation's crates and their dependencies are in [foundation's "Depends on"](https://github.com/ironflowx/foundation/blob/1fc3d0e38f0442151ef5701ed08e37435c27fff3/docs/architecture.md#depends-on). This repository adds, or uses directly:

| Crate | Version | Used | Relied on |
|---|---|---|---|
| `serde_json` | Pinned exactly, as foundation pins it (D26): Cargo resolves one version for both | `ironflowx-cli`: `--json` and the printed answer | |
| `serde_core` | A caret range | `ironflowx-cli`: the JSON visitor of `--json` (D12) | |
| `tokio` | A caret range (D26) | The host's runtime, listener and signals; the CLI's runtime | |
| `axum` | A caret range | `ironflowx-bin`: serves the door's router | |
| `hyper`, `hyper-util` | Caret ranges | `ironflowx-bin`: the accept loop. `hyper-util` also `ironflowx-proto`'s `client` | hyper times an HTTP/1.1 head with the `Timer` it is given. The error of a connection that never opened `is_connect`, and no other is; `connectrpc` keeps it as its error's source. With another `hyper-util` than `connectrpc`'s, rule 3 ([Wire](#wire)) would end as rule 2 |
| `clap` | Pinned exactly, in two unpublished crates: a later one could keep its tracing and drop the backtrace that shows it, silently | `ironflowx-cli`, `ironflowx-bin`: the command line | A usage error's message is its text before the usage, if its context has one, and the footer, each by its last `\n\nUsage: ` and `\n\nFor more information`. With its `debug` feature, and only then, an error made from no text prints more than `error: `: a backtrace, by which `parse` knows that clap would print the arguments (TEN-3) |
| `tracing`, `tracing-subscriber`, `tempfile` | Caret ranges | `ironflowx-bin`: the log, the temporary key file | |

The decisions behind this design: [`decisions.md`](decisions.md).
