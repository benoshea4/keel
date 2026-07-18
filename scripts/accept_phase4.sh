#!/usr/bin/env bash
# Micro-cloud phase 4 acceptance (ext spec Task 4.6): stateless serverless
# functions + the function->workflow bridge.
#
# Proves:
#   * a handler-world component bound to /fn/<name> answers HTTP with the
#     dispatcher's path/query/body plumbing intact (longest-prefix match,
#     guest path is the part AFTER the prefix);
#   * EVERY invocation writes a usage-ledger row with real fuel numbers;
#   * a function can start a durable workflow and relay its status —
#     Lambda + Step Functions in one process, polled THROUGH the function;
#   * unbound paths 404.
set -euo pipefail
DB=accept4.db; rm -f $DB $DB-shm $DB-wal

ENG=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
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

# --- 1. build everything -----------------------------------------------------
cargo build --release -p keel-engine
(cd guests/echo-fn && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/starter-fn && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB --listen 127.0.0.1:8080 > engine.log 2>&1 &
ENG=$!
wait_ready

# --- 2. upload + bind --------------------------------------------------------
hash_of() {
  curl -s -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])'
}
H_ECHO=$(hash_of guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm echo-fn)
H_START=$(hash_of guests/starter-fn/target/wasm32-unknown-unknown/release/starter_fn.wasm starter-fn)
H_CNT=$(hash_of guests/counter/target/wasm32-unknown-unknown/release/counter.wasm counter)

code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/routes \
  -H 'content-type: application/json' \
  -d "{\"prefix\":\"/fn/echo\",\"module_hash\":\"$H_ECHO\"}")
[ "$code" = "201" ] || { echo "FAIL: bind /fn/echo -> $code"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/routes \
  -H 'content-type: application/json' \
  -d "{\"prefix\":\"/fn/jobs\",\"module_hash\":\"$H_START\"}")
[ "$code" = "201" ] || { echo "FAIL: bind /fn/jobs -> $code"; exit 1; }

# --- 3. the echo function ----------------------------------------------------
resp=$(curl -s -w "\n%{http_code}" -X POST "localhost:8080/fn/echo/deep/path?x=1" -d 'ping')
code=$(echo "$resp" | tail -1)
body=$(echo "$resp" | head -1)
[ "$code" = "200" ] || { echo "FAIL: /fn/echo -> $code"; exit 1; }
echo "$body" | grep -q '"path":"/deep/path"' || { echo "FAIL: echo body path: $body"; exit 1; }
echo "$body" | grep -q '"body_len":4' || { echo "FAIL: echo body_len: $body"; exit 1; }

# --- 4. the ledger meters it -------------------------------------------------
row=$(sqlite3 $DB "SELECT outcome, fuel_used > 0 FROM invocations WHERE ref='/fn/echo'")
[ "$row" = "ok|1" ] || { echo "FAIL: echo ledger row: '$row'"; exit 1; }

# --- 5. the function->workflow bridge, polled THROUGH the function -----------
WFID=$(curl -s -X POST localhost:8080/fn/jobs/start -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$H_CNT\",\"input\":{\"target\":3}}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["workflow_id"])')
[ -n "$WFID" ] || { echo "FAIL: no workflow_id from /fn/jobs/start"; exit 1; }

deadline=$((SECONDS + 60))
status_json=""
while [ $SECONDS -lt $deadline ]; do
  status_json=$(curl -s "localhost:8080/fn/jobs/status?id=$WFID")
  echo "$status_json" | grep -q '"status":"completed"' && break
  sleep 1
done
echo "$status_json" | grep -q '"status":"completed"' \
  || { echo "FAIL: workflow never completed via /fn/jobs/status: $status_json"; exit 1; }
echo "$status_json" | grep -q '\\"count\\":3' \
  || { echo "FAIL: bridge output missing count:3: $status_json"; exit 1; }

# --- 6. unbound path ---------------------------------------------------------
code=$(curl -s -o /dev/null -w "%{http_code}" localhost:8080/fn/nope)
[ "$code" = "404" ] || { echo "FAIL: /fn/nope -> $code (want 404)"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal
echo "PHASE 4 PASS"
