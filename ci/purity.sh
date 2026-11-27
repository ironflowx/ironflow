#!/usr/bin/env bash
# Usage: ci/purity.sh
# The binary's and the CLI's dependency boundaries: the CLI links no engine crate (SRV-1), the host
# reaches the engine's parts only through it; the crash test's fault points in no release build
# (D23); a clap that prints arguments, refused (TEN-3); ci/must-reject.sh; and each
# check rejecting its fixture.
set -euo pipefail
cd "$(dirname "$0")/.."

ci/no-engine-deps.sh crates/ironflowx-cli/Cargo.toml
# A dev-dependency does not ship.
ci/no-engine-deps.sh ci/fixtures/engine-dep/allowed/Cargo.toml
ci/no-host-engine-deps.sh crates/ironflowx-bin/Cargo.toml
# A build-dependency is outside the host check's normal dependency boundary.
ci/no-host-engine-deps.sh ci/fixtures/engine-dep/at-build/Cargo.toml
ci/no-release-fault-points.sh
ci/no-release-fault-points.sh ci/fixtures/release-fault-points/allowed.yml
ci/must-reject.sh "release workflow names __fault-points" \
  "release workflow enables --all-features" -- \
  ci/no-release-fault-points.sh ci/fixtures/release-fault-points/release.yml
ci/must-reject.sh "escaped_feature: release workflow names __fault-points" \
  "continued_feature: release workflow names __fault-points" -- \
  ci/no-release-fault-points.sh ci/fixtures/release-fault-points/escaped-feature.yml
ci/must-reject.sh "cargo_global: release Cargo build must select only ironflowx-bin" \
  "comment_scope: release Cargo build must select only ironflowx-bin" \
  "cross_build: release Cargo build must select only ironflowx-bin" \
  "zigbuild: release Cargo build must select only ironflowx-bin" \
  "locked_global: release Cargo build must select only ironflowx-bin" \
  "implicit_run: release Cargo build must select only ironflowx-bin" \
  "subshell_group: release Cargo build must select only ironflowx-bin" \
  "command_subst: release Cargo build must select only ironflowx-bin" \
  "test_no_run: release Cargo build must select only ironflowx-bin" \
  "test_integration: release Cargo build must select only ironflowx-bin" \
  "time_flag: release Cargo build must select only ironflowx-bin" \
  "leading_redir: release Cargo build must select only ironflowx-bin" \
  "devnull_redir: release Cargo build must select only ironflowx-bin" \
  "command_double_dash: release Cargo build must select only ironflowx-bin" \
  "time_o_flag: release Cargo build must select only ironflowx-bin" \
  "heredoc_bash: release Cargo build must select only ironflowx-bin" \
  "arith_shift: release Cargo build must select only ironflowx-bin" \
  "proc_subst: release Cargo build must select only ironflowx-bin" \
  "brace_fd: release Cargo build must select only ironflowx-bin" \
  "exec_combined: release Cargo build must select only ironflowx-bin" \
  "dollar_heredoc: release Cargo build must select only ironflowx-bin" \
  "subshell_printf: release Cargo build must select only ironflowx-bin" \
  "brace_script: release Cargo build must select only ironflowx-bin" \
  "cargo_alias: release Cargo build must select only ironflowx-bin" \
  "case_script: release Cargo build must select only ironflowx-bin" \
  "nested_if_script: release Cargo build must select only ironflowx-bin" \
  "then_for_script: release Cargo build must select only ironflowx-bin" \
  "alias_override: release Cargo build must select only ironflowx-bin" \
  "job_env_alias: release Cargo build must select only ironflowx-bin" \
  "shorthand_b: release Cargo build must select only ironflowx-bin" \
  "pkg_zig: release Cargo build must select only ironflowx-bin" \
  "batched_env_alias: release Cargo build must select only ironflowx-bin" \
  "inline_alias_config: release Cargo build must select only ironflowx-bin" -- \
  ci/no-release-fault-points.sh ci/fixtures/release-fault-points/workspace.yml
ci/must-reject.sh "ironflowx-bin: ordinary release graph enables __fault-points" -- \
  ci/no-release-fault-points.sh .github/workflows/release.yml \
  ci/fixtures/release-fault-points/default/Cargo.toml
ci/must-reject.sh "ironflowx-bin: ordinary release graph enables __fault-points" -- \
  ci/no-release-fault-points.sh .github/workflows/release.yml \
  ci/fixtures/release-fault-points/opt-in/Cargo.toml

ci/must-reject.sh \
  "engine-dep/direct/Cargo.toml: engine crate: 'ironflowx-operations'" \
  "engine-dep/renamed/Cargo.toml: engine crate: 'ironflowx-cell'" \
  "engine-dep/optional/Cargo.toml: engine crate: 'ironflowx-store'" \
  "engine-dep/target-only/Cargo.toml: engine crate: 'ironflowx-authz'" \
  "engine-dep/at-build/Cargo.toml: engine crate: 'ironflowx-events'" \
  "engine-dep/through/Cargo.toml: engine crate: 'ironflowx-operations'" \
  "engine-dep/facade/Cargo.toml: engine crate: 'ironflowx'" \
  "engine-dep/generator/Cargo.toml: linked at run time: 'ironflowx-codegen'" -- \
  ci/no-engine-deps.sh ci/fixtures/engine-dep/{direct,renamed,optional,target-only,at-build,through,facade,generator}/Cargo.toml
ci/must-reject.sh \
  "host-engine-deps/Cargo.toml: direct engine dependency: 'ironflowx-authz'" \
  "host-engine-deps/Cargo.toml: direct engine dependency: 'ironflowx-cell'" \
  "host-engine-deps/Cargo.toml: direct engine dependency: 'ironflowx-events'" \
  "host-engine-deps/Cargo.toml: direct engine dependency: 'ironflowx-store'" -- \
  ci/no-host-engine-deps.sh ci/fixtures/host-engine-deps/Cargo.toml

# TEN-3: built with clap's tracing, the binary reads no argument.
CARGO_TARGET_DIR=target/clap-debug cargo build -q --locked -p ironflowx-bin --features clap/debug
ci/clap-trace.sh target/clap-debug/debug/ironflowx
for traces in stderr stdout code; do
  ci/must-reject.sh "clap-trace: ci/fixtures/clap-trace/$traces" -- \
    ci/clap-trace.sh "ci/fixtures/clap-trace/$traces"
done

# ci/must-reject.sh takes a pattern in one line of the output, no fewer, no more and no other's.
ci/must-reject.sh "with 'BadType' in 0 lines" -- \
  ci/must-reject.sh BadType -- sh -c 'echo BadOther; exit 1'
ci/must-reject.sh "with 'BadType' in 2 lines" -- \
  ci/must-reject.sh BadType -- sh -c 'echo BadType; echo BadTypeHolder; exit 1'
ci/must-reject.sh "with 'BadTypeHolder' only in another pattern's line" -- \
  ci/must-reject.sh BadType BadTypeHolder -- sh -c 'echo BadTypeHolder; exit 1'
ci/must-reject.sh "passed, but it must reject" -- ci/must-reject.sh BadType -- echo BadType
