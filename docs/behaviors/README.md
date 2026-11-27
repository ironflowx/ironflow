# Behaviors

What a client of the binary observes. Invariants that no single example can show are in [`../invariants.md`](../invariants.md); the rules these scenarios sample are in [`../architecture.md`](../architecture.md).

## Files

| File | Scenarios | IDs |
|---|---|---|
| [`cli.feature`](cli.feature) | 14 | Calling CLI-001–002, CLI-013–014 · keys CLI-006–007 · errors CLI-003–004, CLI-015 · describe CLI-008–010 · seeds, version CLI-012, CLI-016 |
| [`serve.feature`](serve.feature) | 26 | First start and the bootstrap key HOST-001–005, HOST-017–019, HOST-030 · health HOST-006 · starting, stopping, refusing HOST-009–013, HOST-016 · settings HOST-014–015 · the wire HOST-021–027, HOST-029 |
| [`events.feature`](../../vendor/foundation/docs/behaviors/events.feature), foundation's | 41 | Emit EVT-001–009 · gates EVT-010–017, EVT-038–039 · batch EVT-020–025 · idempotency EVT-030–037 · reading EVT-040–055 · read your writes EVT-060–062 |
| [`tenancy.feature`](../../vendor/foundation/docs/behaviors/tenancy.feature), foundation's | 40 | Bootstrap and open ACC-001–018 · authentication ACC-020–024 · scopes ACC-030–032 · creating and listing keys ACC-040–047 · revoking and expiry ACC-050–054 · who emitted ACC-060–061 |

IDs missing from these ranges are unassigned (CLI-005 too), except EVT-005, HOST-028 and CLI-011, which are retired.

## Foundation's feature files

`events.feature` and `tenancy.feature` are foundation's spec of the engine: foundation runs them on the engine in process, and this repository runs them again against the binary. They are vendored in [`vendor/foundation/docs/behaviors/`](../../vendor/foundation/docs/behaviors/), at the commit the root `Cargo.toml` pins. `ci/vendored.sh`, the `vendored` CI job, checks that each file under `vendor/foundation/` matches foundation's at that commit, byte for byte, so a pin bump that changes them fails until the copies are refreshed. The same holds for the foundation test helpers the binary's tests include (the key-leak search, the five bad-key refusals, the recording door).

Vendoring was chosen over reading the files out of Cargo's git checkout of foundation, whose path Cargo does not give a test, and which a `[patch]` to a local checkout replaces. A copy is plain to read and review, and the check keeps it honest. `ci/conformance.sh` runs against a foundation checkout with these copies still at the pin.

## How they run

The steps call `Operations`, never a URL or a status code, but for `/health` and `/ready`, which are the interface. Those of `serve.feature` also start the binary, read its log and, where a scenario names a protocol, call with a client of that protocol generated from the proto, or, where it is about the JSON itself, read what that protocol sent; those of `cli.feature` start the binary and run commands of it, and read their stdout, stderr and exit code:

| Adapter | Drives | Runs |
|---|---|---|
| Proto client (`Client`) | The `ironflowx` binary, built once at the suite's start with `cargo build -p ironflowx-bin --locked` (Cargo gives a test a binary's path only within its own package), started by each scenario on a fresh data directory and `127.0.0.1:0` | `events.feature` and `tenancy.feature`, but for what is `@local`; `serve.feature`, but for the calls that name a protocol |
| The CLI | The same binary, each command a process of its own with the scenario's variables, calling an engine the scenario serves | `cli.feature` |

Against the binary, "a fresh environment" and "a fresh engine directory" are a binary started on a fresh data directory; "the bootstrap key" is the key in `admin.key`; and "now", "in one hour" and "an hour ago" are the machine's clock, read once per scenario, so ACC-010's creation time is checked to lie between that and the answer. "A client" and "another client" call through the scenario's one `Client`. `Client` refuses EVT-016's two rows that are not UTF-8 itself, and HOST-025 sends such bytes raw.

The runner is the `cucumber` crate, in an unpublished workspace crate (D20), `crates/ironflowx-behaviors`. It runs the files in place; they are not copied at run time. It fails unless the `.feature` files here and in `vendor/foundation/docs/behaviors/` are exactly those its `SCENARIOS` table names, each with that many scenario declarations (before outline expansion), so a new scenario updates its count and a new file is added there. Each file runs on its own World, a started binary each. The runner checks the number of passing cases, counting each outline example separately. Its step definitions are foundation's, the in-process branch included, so that the runner stays a copy of foundation's; here only the binary's half runs.

The behavior suites start `serve` at `--log-level trace`, including for remote calls. They capture stdout and stderr separately. Successful `keys create`, `recover-key --stdout` and `seed generate` each deliver a credential in the top-level JSON `key` field with its matching ID. The searches in [TEN-3](../invariants.md) cover the other output and the data directory before and after shutdown. Run the scenarios with `cargo test -p ironflowx-behaviors --locked --test behaviors`.

## Conventions

- Tags: a stable `@EVT-nnn`, `@ACC-nnn`, `@HOST-nnn` or `@CLI-nnn` ID and the release that added it, as `@release-0.1`; `@door:cli` on every scenario of `cli.feature`. IDs are never reused.
- A quoted table cell is a JSON string literal after Gherkin unescaping. A data cell is the JSON sent, byte for byte.
- A data or metadata cell that starts `built:` describes its input (`built: 128 nested arrays`, `built: a string holding the byte 0xFF`), and the step builds it: a feature file is UTF-8 and cannot hold an invalid byte itself. A description the step does not know fails the step; it is never sent as data.
- Lengths count UTF-8 bytes. Sizes are the bytes of the JSON as sent.
- `@local`: the scenario needs a fixture only foundation's in-process adapter has (the clock, a second incarnation, the data directory, what `open` returns), so it does not run here.
- In `serve.feature`, "served" is `ironflowx serve` on the scenario's data directory, on `127.0.0.1:0` unless an address is named; "becomes ready" is `/ready` answering 200. "Over Connect", "over gRPC" and so on use a client for that protocol generated from the proto, not `Client`. A data cell is the JSON itself, unquoted, or `built:` and a description.
- In `cli.feature`, an envelope is the one line of stderr, with nothing on stdout; "exits with code N" is the process's status; single quotes in a command are the shell's, and `sh` runs it, in the scenario's directory; a key with scopes is one the CLI creates with the bootstrap key; a printed cell is the JSON itself, unquoted.
