#!/usr/bin/env bash
# Cancel smoke test (post-review hardening) — NOT one of the three phase gates,
# but the regression test for POST /api/workflows/{id}/cancel. Proves BOTH
# cancel paths:
#   * a PARKED workflow (counter, sleeping between ticks) — the park loop sees
#     the abort flag the instant set_abort's notify lands;
#   * a workflow SPINNING IN PURE WASM (guests/spin, `loop {}`) — the epoch-
#     deadline callback traps it at the next 1s tick. A park-loop flag alone can
#     NEVER catch this one; before the epoch work its only off switch was
#     hand-editing the database.
# Also: cancel is terminal (re-cancel → 409) and cleans the parked timer row.
set -euo pipefail
DB=smoke-cancel.db; rm -f $DB $DB-shm $DB-wal

ENG=""
cleanup() { if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi; }
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
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/spin && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!
wait_ready

up() { curl -s -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])'; }
HC=$(up guests/counter/target/wasm32-unknown-unknown/release/counter.wasm counter)
HS=$(up guests/spin/target/wasm32-unknown-unknown/release/spin.wasm spin)

start() { curl -s -X POST localhost:8080/api/workflows -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$1\",\"input\":$2}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'; }
W1=$(start "$HC" '{"target":1000}')   # ticks every 5s → parked (sleeping) almost always
W2=$(start "$HS" '{}')                # burns a core in pure wasm, never parks

status() { curl -s localhost:8080/api/workflows/$1 | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

for i in $(seq 1 15); do ST=$(status $W1); [ "$ST" = "sleeping" ] && break; sleep 1; done
[ "$ST" = "sleeping" ] || { echo "FAIL: counter not sleeping before cancel, got $ST"; exit 1; }
[ "$(status $W2)" = "running" ] || { echo "FAIL: spin guest not running, got $(status $W2)"; exit 1; }

# invalid magic is refused at upload (post-review hardening)
CODE=$(printf 'not wasm at all' | curl -s -o /dev/null -w '%{http_code}' -X POST \
  --data-binary @- "localhost:8080/api/modules?name=junk")
[ "$CODE" = "400" ] || { echo "FAIL: junk upload returned $CODE, want 400"; exit 1; }

for W in $W1 $W2; do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/workflows/$W/cancel)
  [ "$CODE" = "200" ] || { echo "FAIL: cancel of $W returned $CODE"; exit 1; }
  ST=$(status $W)
  [ "$ST" = "failed" ] || { echo "FAIL: $W is $ST after cancel, want failed"; exit 1; }
  OUT=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$W'")
  [ "$OUT" = "cancelled by operator" ] || { echo "FAIL: output of $W is '$OUT'"; exit 1; }
done

# cancel is terminal — a second cancel refuses
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/workflows/$W1/cancel)
[ "$CODE" = "409" ] || { echo "FAIL: re-cancel returned $CODE, want 409"; exit 1; }
# the parked counter's timer row was cleaned up in the cancel txn
N=$(sqlite3 $DB "SELECT COUNT(*) FROM timers")
[ "$N" = "0" ] || { echo "FAIL: $N timer rows left after cancel"; exit 1; }

kill $ENG || true
echo "CANCEL SMOKE PASS"
