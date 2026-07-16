#!/usr/bin/env bash
# Phase 1 acceptance (SPEC.md Task 1.7) — the spec's script, hardened post-review:
#   * trap cleanup — a FAILING run must not leak a live server holding :8080
#     (the next run's curls would silently hit it, with a different database);
#   * readiness polling instead of sleep-and-hope after each engine launch;
#   * a LOCAL http stub instead of example.com — the definition of done must not
#     depend on DNS or someone else's uptime (the demo guest reads its fetch url
#     from the workflow input).
# Run from the repo root (keel/). Definition of done: prints "PHASE 1 PASS"
# twice in a row from a clean checkout. If it fails, fix the ENGINE, not this
# script.
set -euo pipefail
DB=accept1.db; rm -f $DB $DB-shm $DB-wal

ENG=""; STUB=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  if [ -n "${STUB:-}" ]; then kill -9 "$STUB" 2>/dev/null || true; fi
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
(cd guests/demo && cargo component build --release --target wasm32-unknown-unknown)

# Local stub on :18080 serving scripts/stub/ — a fixed body, deterministic runs.
if curl -sf -o /dev/null --max-time 1 localhost:18080/; then
  echo "FAIL: something is already listening on :18080 — kill it first"; exit 1
fi
python3 -m http.server 18080 --bind 127.0.0.1 --directory scripts/stub > /dev/null 2>&1 & STUB=$!
STUB_UP=""
for i in $(seq 1 50); do
  if curl -sf -o /dev/null localhost:18080/body.txt; then STUB_UP=1; break; fi
  sleep 0.2
done
[ -n "$STUB_UP" ] || { echo "FAIL: stub server did not start on :18080"; exit 1; }

./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!
wait_ready
# ALL acceptance scripts redirect engine output to engine.log (appending on restart:
# use >> for the second launch) — phase 3's script greps it for "resuming".

HASH=$(curl -s -X POST --data-binary @guests/demo/target/wasm32-unknown-unknown/release/demo.wasm \
  "localhost:8080/api/modules?name=demo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HASH\",\"input\":{\"url\":\"http://127.0.0.1:18080/body.txt\"}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

sleep 5                                   # inside the 15s guest sleep by now
kill -9 $ENG                              # ungraceful, mid-workflow
sleep 1
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!
wait_ready

for i in $(seq 1 40); do
  ST=$(curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$ST" = "completed" ] && break; sleep 1
done
kill $ENG || true
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST"; exit 1; }

# exactly 2 http-gets total: the pre-crash one was NOT re-executed
N=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF' AND kind='http-get'")
[ "$N" = "2" ] || { echo "FAIL: expected 2 http-get rows, got $N"; exit 1; }

# the replayed random matches the recorded one → output stamp == journal row.
# NOTE: check the DB's output column, NOT the API response — the API returns output as
# an escaped JSON string, so grepping the API body for "stamp": is dead on arrival.
STAMP=$(sqlite3 $DB "SELECT json_extract(response,'\$.v') FROM journal WHERE workflow_id='$WF' AND kind='random-u64'")
sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'" | grep -q "\"stamp\":$STAMP" \
  || { echo "FAIL: replayed random diverged"; exit 1; }
echo "PHASE 1 PASS"
