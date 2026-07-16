#!/usr/bin/env bash
# v2.2 smoke test: capability providers, end to end.
#
# Proves:
#   * a provider component registers via --provider and serves provider-call;
#   * kill -9 between two provider calls -> the first REPLAYS from the journal
#     (the engine's "invoking provider" log line appears exactly once for it);
#   * an unregistered provider name is a guest-visible err (data, not a trap);
#   * journal kinds are custom:<name>:<kind>;
#   * a --provider that is not a valid provider component FAILS THE BOOT.
set -euo pipefail
DB=smoke-providers.db; rm -f $DB $DB-shm $DB-wal

ENG=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  rm -f smoke-providers-bad.db*
}
trap cleanup EXIT

if curl -sf -o /dev/null --max-time 1 localhost:8080/; then
  echo "FAIL: something is already listening on :8080 — kill it first"; exit 1
fi
wait_ready() {
  for i in $(seq 1 50); do
    curl -sf -o /dev/null localhost:8080/ && return 0
    sleep 0.2
  done
  echo "FAIL: engine did not answer on :8080 within 10s (see engine.log)"; exit 1
}

cargo build --release -p keel-engine
(cd providers/greet && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/providerdemo && cargo component build --release --target wasm32-unknown-unknown)
GREET=providers/greet/target/wasm32-unknown-unknown/release/greet.wasm

# 0. a junk provider must fail the boot (fail fast, never at call time).
# No `timeout` here — macOS lacks it; poll for exit instead.
./target/release/keel serve --db smoke-providers-bad.db \
  --provider bad=README.md > provider-bad.log 2>&1 & BAD=$!
RC=""
for i in $(seq 1 20); do
  if ! kill -0 $BAD 2>/dev/null; then
    set +e; wait $BAD; RC=$?; set -e
    break
  fi
  sleep 0.5
done
if [ -z "$RC" ]; then
  kill -9 $BAD 2>/dev/null || true
  echo "FAIL: engine kept running with a junk provider"; exit 1
fi
if [ "$RC" = "0" ]; then
  echo "FAIL: engine accepted a non-wasm provider (exited 0)"; cat provider-bad.log; exit 1
fi
grep -q "provider 'bad'" provider-bad.log \
  || { echo "FAIL: boot refusal does not name the provider"; cat provider-bad.log; exit 1; }
rm -f provider-bad.log

./target/release/keel serve --db $DB --provider greet=$GREET > engine.log 2>&1 & ENG=$!
wait_ready

HP=$(curl -s -X POST --data-binary @guests/providerdemo/target/wasm32-unknown-unknown/release/providerdemo.wasm \
  "localhost:8080/api/modules?name=providerdemo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HP\",\"input\":{}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
status() { curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

# 1. first provider call done, guest parked in its 5s sleep — kill it there
for i in $(seq 1 10); do ST=$(status); [ "$ST" = "sleeping" ] && break; sleep 1; done
[ "$ST" = "sleeping" ] || { echo "FAIL: expected sleeping before crash, got $ST"; exit 1; }
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB --provider greet=$GREET >> engine.log 2>&1 & ENG=$!
wait_ready
for i in $(seq 1 30); do ST=$(status); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST after recovery"; exit 1; }

# 2. output shape: both calls answered, unregistered provider erred as data
OUT=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'")
echo "$OUT" | grep -q 'hello keel'        || { echo "FAIL: greet response missing — got $OUT"; exit 1; }
echo "$OUT" | grep -Fq '\"HI\"'           || { echo "FAIL: shout response missing — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"missing_err":true' || { echo "FAIL: unregistered provider did not err — got $OUT"; exit 1; }

# 3. journal shape: custom:<name>:<kind>, one row each
for want in "custom:greet:greet" "custom:greet:shout" "custom:nope:x"; do
  GOT=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF' AND kind='$want'")
  [ "$GOT" = "1" ] || { echo "FAIL: expected 1 '$want' row, got $GOT"; exit 1; }
done

# 4. replay proof: the pre-crash call was NOT re-invoked on recovery — the
# live-path log line appears exactly once per executed call across BOTH runs
N1=$(grep -c "invoking provider greet kind greet" engine.log || true)
N2=$(grep -c "invoking provider greet kind shout" engine.log || true)
[ "$N1" = "1" ] || { echo "FAIL: greet invoked $N1 times — replay re-ran the provider"; exit 1; }
[ "$N2" = "1" ] || { echo "FAIL: shout invoked $N2 times"; exit 1; }

kill $ENG || true; ENG=""
echo "PROVIDERS SMOKE PASS"
