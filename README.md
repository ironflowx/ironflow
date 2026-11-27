# ironflow

[![CI](https://github.com/ironflowx/ironflow/actions/workflows/ci.yml/badge.svg?branch=main&event=push)](https://github.com/ironflowx/ironflow/actions/workflows/ci.yml?query=branch%3Amain)

The `ironflowx` binary: `ironflowx serve`, which opens an engine in a data directory and serves it over Connect, gRPC and gRPC-Web, and the CLI, in the same binary, which calls one. MIT-licensed, 0.x, with no stability promise.

The engine itself, with its ledger, credentials, control plane, wire, HTTP door and CLI generator, is a set of library crates in [ironflowx/foundation](https://github.com/ironflowx/foundation). Embed them from there; this repository is the app built on top of them, and it ships as release archives, not crates.

## What it does

- The first `serve` bootstraps the engine and writes the admin key to `admin.key`, or takes the key you supply as a seed.
- Create keys scoped `read`, `write` or `admin`, read-only unless asked otherwise, with an optional expiry; list them, and revoke one.
- Emit an event, or a batch of up to 100, with data, metadata, a version and an idempotency key; each records the key that emitted it.
- Read an event by ID, or list events newest first, as soon as any emit returns.
- The CLI prints JSON and exit codes an agent branches on, and `describe` gives each operation's input and output schemas and a runnable example, offline.
- `ironflowx recover-key` recovers an admin key from the data directory when the last one is lost; `ironflowx seed generate` makes a seed.

## Run it

Choose the archive and its `.sha256` file for your platform from the [GitHub releases](https://github.com/ironflowx/ironflow/releases). The Linux archives use static musl for x86-64 and AArch64. The macOS archive is for Apple silicon. Run `shasum -a 256 -c <archive>.sha256` beside the archive, then extract it with `tar -xzf <archive>`. Releases up to 0.3.0 are in [foundation's releases](https://github.com/ironflowx/foundation/releases).

Start the host in one terminal.

```sh
./ironflowx serve
```

Once it is ready, use the bootstrap key in another terminal.

```sh
./ironflowx whoami --url http://127.0.0.1:9310 --key-file .ironflowx/admin.key
./ironflowx describe events emit
```

From a checkout, `cargo run --locked -p ironflowx-bin -- serve` does the same.

## Crates

| Crate | Contents |
| --- | --- |
| `ironflowx-bin` | the `ironflowx` binary and its host: `serve`, `recover-key`, `seed generate` |
| `ironflowx-cli` | the CLI: the commands generated from foundation's proto, `describe` and `version` |
| `ironflowx-behaviors` | the behavior scenarios, run against the built binary |

All three are unpublished. The design is in [`docs/architecture.md`](docs/architecture.md), the decisions behind it in [`docs/decisions.md`](docs/decisions.md), and what the tests prove in [`docs/invariants.md`](docs/invariants.md).

## How it depends on foundation

Foundation is a git dependency pinned to one commit in the root `Cargo.toml`, and CI tests that pin. When foundation 0.4.0 is on crates.io, with `ironflowx-doors` and `ironflowx-codegen` published for the first time, a release here depends on crates.io versions only. The binary carries foundation's release number until 1.0.0; the next release here is `v0.4.0`, built on foundation 0.4.0.

A change that touches both repositories lands as two PRs: first foundation, then the pin bump here. To bump the pin, replace the commit in the root `Cargo.toml` and in `ci/fixtures/engine-dep/Cargo.toml`, and run `cargo update -p ironflowx-core` (any one foundation crate; they move together) at the root and in `ci/fixtures/engine-dep/`. Refresh `vendor/foundation/` from foundation at the new commit, and run `ci/vendored.sh`. Leave the links in `docs/` alone: they name the commit the docs were copied from.

### Working on foundation too

To build against a sibling checkout of foundation, create `.cargo/config.toml`, which `.gitignore` keeps out of commits:

```toml
[patch."https://github.com/ironflowx/foundation"]
ironflowx-authz = { path = "../foundation/crates/ironflowx-authz" }
ironflowx-cell = { path = "../foundation/crates/ironflowx-cell" }
ironflowx-codegen = { path = "../foundation/crates/ironflowx-codegen" }
ironflowx-core = { path = "../foundation/crates/ironflowx-core" }
ironflowx-doors = { path = "../foundation/crates/ironflowx-doors" }
ironflowx-events = { path = "../foundation/crates/ironflowx-events" }
ironflowx-operations = { path = "../foundation/crates/ironflowx-operations" }
ironflowx-proto = { path = "../foundation/crates/ironflowx-proto" }
ironflowx-store = { path = "../foundation/crates/ironflowx-store" }
```

Cargo then rewrites `Cargo.lock` for the patched sources, so `--locked` refuses until you run one command without it; do not commit that `Cargo.lock`. The patch also reaches the fixture workspace `ci/purity.sh` runs Cargo in, so move the file aside before running that script, or `ci/fixtures/engine-dep/Cargo.lock` changes too.

`ci/conformance.sh <path-to-foundation>` runs this repository's suite against a foundation checkout through the same `[patch]`, written for the run and removed after it, and restores `Cargo.lock`. Foundation's CI runs it for each foundation PR.

## Checks

The CI jobs and how to run each locally are in [`ci/README.md`](ci/README.md). Before a PR: `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, `ci/serve-lifecycle.sh` and `ci/server-crash.sh`.
