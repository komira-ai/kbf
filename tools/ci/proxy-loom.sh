#!/usr/bin/env bash
# Runs the loom models of kbf-proxy's slot and gate (docs/design/kbf-proxy-api.md, section 5).
#
# The models are `loom_*` unit tests of `crates/kbf-proxy/src/slot/loom_tests.rs`, built only
# under `--cfg kbf_loom` (the cfg swaps the atomics `slot.rs` uses for loom's). The cfg is
# not `loom`: tokio reads that one too. A separate target directory keeps the cfg from
# rebuilding the normal one. On the dev box run it through kbf-cargo's budget:
#   tools/ci/proxy-loom.sh            (this script calls cargo, so give it the lease first)
# In CI it is one step: `run: tools/ci/proxy-loom.sh`.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
export RUSTFLAGS="${RUSTFLAGS:-} --cfg kbf_loom"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/loom}"
# Release: the models explore many interleavings and run an order of magnitude faster.
exec cargo test -p kbf-proxy --lib --release --locked -- loom_
