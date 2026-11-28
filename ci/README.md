# CI checks

`.github/workflows/ci.yml` runs these on every PR. Every check script of `fmt`, `vendored` and `purity` has a fixture it must reject, a dedicated file under `fixtures/` or a build configured to trigger it; `must-reject.sh` checks that each fixture fails for the expected reason. Foundation comes from the commit the root `Cargo.toml` pins.

| Job | Enforces | Run locally | When it fails |
|---|---|---|---|
| `fmt`, `clippy`, `clippy-macos` | Formatting, and comment lines of at most 100 columns in Rust and `ci/*.sh`, URLs exempt (`ci/comment-width.sh`: rustfmt does not wrap comments); lints with warnings denied, on Linux and macOS | `cargo fmt --check`, `ci/comment-width.sh crates ci/*.sh`, `cargo clippy --workspace --all-targets --locked -- -D warnings` | Fix the code |
| `test` | The tests and the behavior scenarios ([`docs/behaviors/`](../docs/behaviors/README.md)) on Linux and macOS; `ci/serve-lifecycle.sh`, the serve lifecycle fixture in dev and release (SRV-10, SRV-15); `ci/server-crash.sh`, the [server crash test](../docs/invariants.md#server-crash-test); the CLI's own tests again with `serde_json`'s `arbitrary_precision` | `cargo test --workspace --locked`, `ci/serve-lifecycle.sh`, `ci/server-crash.sh`, `cargo test -p ironflowx-cli --locked --features serde_json/arbitrary_precision` | Fix the code. For the crash test, replay the printed `SERVER_CRASH_SEED=<seed> ci/server-crash.sh seeded_random_kill` |
| `vendored` | Each file under `vendor/foundation/` matches foundation's at the pinned commit, byte for byte (`ci/vendored.sh`, which needs network) | `ci/vendored.sh` | Copy the file from foundation at the pinned commit |
| `deny` | Licences, advisories and sources, Linux and macOS graphs (`deny.toml`); foundation's git repository is the one git source allowed | `cargo deny check` | Replace the dependency, or extend `deny.toml` with a reason |
| `purity` | SRV-1: `ironflowx-cli` depends on no engine crate (`ci/no-engine-deps.sh`); the host has no direct engine-internal dependency (`ci/no-host-engine-deps.sh`); the crash test's fault points are in no release build (`ci/no-release-fault-points.sh`, D23); TEN-3: the binary built with clap's `debug` feature, which prints arguments, refuses to start (`ci/clap-trace.sh`); `ci/must-reject.sh` itself. Linux and macOS | `ci/purity.sh` (needs `jq` and `ruby`) | Call the engine through `Operations`; select only `ironflowx-bin`, without `__fault-points`, for an archive build |
| `maintainer-only` | No outside contributions: a PR must come from a branch of this repository, which only those with write access, and Dependabot, can push. A push to `main` passes it | None: it reads the pull request | Close the PR; a maintainer opens the change from a branch here |
| `lint-ci` | actionlint and shellcheck over the workflows and `ci/*.sh` | `actionlint && shellcheck -x ci/*.sh` | Fix the script |

`ci/conformance.sh <path-to-foundation>` runs the `test` job's steps against a foundation checkout, through a `[patch]` in a temporary `.cargo/config.toml`, and restores `Cargo.lock` after. Foundation's CI calls it for each foundation PR. Keep it and the `test` job alike.

## Host dependencies

`ci/no-host-engine-deps.sh crates/ironflowx-bin/Cargo.toml`: the binary reaches the ledger, control plane, event rules and credentials through the engine. The check reads the binary's direct normal dependencies from Cargo metadata, including renamed, optional and target-specific entries. Build, dev and transitive dependencies are outside the check. `ci/no-release-fault-points.sh` checks `release.yml` and the build graph of `ironflowx-bin` with its ordinary features on all targets, with fixtures for the explicit feature, all features, a workspace build and an enabled default or opt-in feature.

The fixtures in `fixtures/engine-dep/` depend on foundation at the pinned commit: a pin bump updates their `Cargo.toml` and `Cargo.lock` too.

## Releases

A `vX.Y.Z` tag, matching the root `Cargo.toml`'s version and with a `## X.Y.Z ` section in `CHANGELOG.md`, starts `.github/workflows/release.yml`. It builds the three archives and creates the GitHub release with their checksums and that version's changelog section. For a failed archive build, rerun the failed jobs in the tag's workflow run. To rehearse, `gh workflow run release.yml --ref <branch>`: a manual run uploads the archives and their SHA-256 files as workflow artifacts, and only a tag push creates a release.

The archive matrix builds on native Linux x86-64 and AArch64 runners with `musl-tools` and on a native Apple silicon runner. Each job checks the archive files and checksum, runs the extracted binary to verify its version, checkout commit and protocol, and calls a fresh engine over loopback HTTPS with a test trust root (`release-https.py`). Linux jobs also check the extracted binary for an ELF interpreter or dynamic library dependencies (D81). Each Linux job builds two rejecting ELF fixtures, one for each violation, and accepts a static ELF as a control. The Apple silicon job exercises rejecting fixtures for the shared checks: checksum corruption or the wrong digest algorithm, archive members, licence, version and HTTPS.

## Tools

`ci/tools.env` pins cargo-deny, actionlint and shellcheck; CI reads it. `jq`, `ruby` and `python3` are not pinned: they come with the GitHub runners and macOS.
