#!/usr/bin/env bash
# v2.2 crate-split gate: keel-core runs a workflow IN-PROCESS — no `keel`
# binary, no HTTP. Builds the counter guest, then runs the ignored
# integration test (core/tests/embedded.rs) against it. cargo test's exit
# code is the assertion; the example (core/examples/embedded.rs) compiles as
# part of the same invocation, keeping the docs honest.
set -euo pipefail

(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)
KEEL_EMBED_WASM="$PWD/guests/counter/target/wasm32-unknown-unknown/release/counter.wasm" \
  cargo test --release -p keel-core --test embedded -- --ignored

echo "EMBEDDED SMOKE PASS"
