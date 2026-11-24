#!/bin/bash
# ci/cargo.sh ARGS... - run cargo in a pinned Rust container, capped, with
# core dumps off.
#
# Why: the build server is shared. This wrapper
#   - waits for room and runs at low priority through /usr/local/bin/loadwait
#     (skipped when NO_LOADWAIT=1, as in CI where the runner is the limit);
#   - caps the container's CPUs (HS_CPUS, default 4), memory (HS_MEM,
#     default 8g) and cargo's parallel jobs (HS_JOBS, default 4);
#   - uses an image whose compiler is the one rust-toolchain.toml pins
#     (HS_IMAGE; the default carries cargo 1.100.0 with rustfmt and clippy);
#   - keeps the registry and the target directory in named volumes
#     (hearth-sync-cargo, and hearth-sync-target or HS_TARGET for a second
#     checkout), so nothing is written into the checkout as root;
#   - turns core dumps off (--ulimit core=0 and ulimit -c 0): a crashed
#     test once left two 13 MB dumps in crates/core, which got committed.
#
# Examples:
#   ci/cargo.sh test -p hearth-sync-sim --locked
#   HS_TARGET=hearth-sync-target-sim ci/cargo.sh clippy --workspace --all-targets -- -D warnings
#   ci/cargo.sh run -p hearth-sync-sim --release -- quick
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
image="${HS_IMAGE:-hearth-mail-builder:3}"
run=(docker run --rm --init
  --name "hearth-sync-cargo-$$"
  --memory "${HS_MEM:-8g}" --memory-swap "${HS_MEM:-8g}"
  --cpus "${HS_CPUS:-4}"
  --ulimit core=0
  -e CARGO_BUILD_JOBS="${HS_JOBS:-4}"
  -e CARGO_TARGET_DIR=/target
  -e CARGO_INCREMENTAL=0
  -e TMPDIR=/target/tmp
  -v hearth-sync-cargo:/usr/local/cargo/registry
  -v "${HS_TARGET:-hearth-sync-target}:/target"
  -v "$root:/src"
  -w /src)
# Pass the test runners' settings through when they are set.
for v in PROPTEST_CASES STORE_SEEDS SIM_SEEDS SIM_FIRST_SEED SIM_BUDGET RUST_BACKTRACE; do
  [ -n "${!v:-}" ] && run+=(-e "$v=${!v}")
done
run+=("$image" bash -c 'ulimit -c 0 && mkdir -p /target/tmp && exec cargo "$@"' cargo "$@")

if [ "${NO_LOADWAIT:-}" = 1 ]; then exec "${run[@]}"; fi
exec /usr/local/bin/loadwait "${run[@]}"
