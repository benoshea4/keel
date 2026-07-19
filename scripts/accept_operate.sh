#!/usr/bin/env bash
# Amendment 1 (SPEC-AMENDMENT-1.md §A5) — v3.1 acceptance: rate limits OFF THE
# LEDGER, captured function logs, ledger retention.
#
# Proves:
#   * echo-fn's platform-api log lines land in fn_logs with correct content,
#     and /api/logs honors the after= tail contract;
#   * a limit-3 route under 8 CONCURRENT requests admits EXACTLY 3 (the
#     in-flight term makes the limiter exact, not approximate): 3x 2xx,
#     5x 429 with Retry-After + retry_after_ms, ledger rows exactly 3,
#     keel_fn_rate_limited_total 5 — and a 429 writes NO ledger row;
#   * a limit-1 app 429s its second backend call;
#   * the window SURVIVES A RESTART (the ledger is the state — the same route
#     429s immediately after reboot);
#   * a backdated window frees the route with no sleeping (UPDATE created_at
#     is a legitimate control input precisely because the state is a table);
#   * --retain-ledger-hours sweeps backdated invocations AND fn_logs rows on
#     the restart's immediate GC pass, and touches nothing in-window.
set -euo pipefail
DB=accept-op.db; rm -f $DB $DB-shm $DB-wal

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
SQL() { sqlite3 -cmd ".timeout 5000" $DB "$1"; }

# --- build ------------------------------------------------------------------
cargo build --release -p keel-engine
(cd guests/echo-fn && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB --listen 127.0.0.1:8080 \
  --retain-ledger-hours 168 > engine.log 2>&1 &
ENG=$!
wait_ready

H_ECHO=$(curl -s -X POST --data-binary @guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm \
  "localhost:8080/api/modules?name=echo-fn" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])')

# --- 1. captured logs + the after= tail contract -----------------------------
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/routes \
  -H 'content-type: application/json' \
  -d "{\"prefix\":\"/fn/logs\",\"module_hash\":\"$H_ECHO\"}")
[ "$code" = "201" ] || { echo "FAIL: bind /fn/logs -> $code"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/logs \
  --data-binary $'alpha\nbeta')
[ "$code" = "200" ] || { echo "FAIL: /fn/logs call -> $code"; exit 1; }
logs=$(curl -s "localhost:8080/api/logs?kind=function&ref=%2Ffn%2Flogs")
echo "$logs" | python3 -c '
import json, sys
d = json.load(sys.stdin)["lines"]
texts = [l["line"] for l in d]
assert texts == ["echo: alpha", "echo: beta"], texts
assert all(l["invocation_id"] is not None for l in d), d
print(d[0]["id"])' > first_id.txt || { echo "FAIL: log lines: $logs"; exit 1; }
FIRST_ID=$(cat first_id.txt); rm -f first_id.txt
after=$(curl -s "localhost:8080/api/logs?kind=function&ref=%2Ffn%2Flogs&after=$FIRST_ID")
echo "$after" | python3 -c '
import json, sys
d = json.load(sys.stdin)["lines"]
assert [l["line"] for l in d] == ["echo: beta"], d
print("tail ok")' | grep -q "tail ok" || { echo "FAIL: after= tail: $after"; exit 1; }

# --- 2. the limiter is EXACT under concurrency -------------------------------
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/routes \
  -H 'content-type: application/json' \
  -d "{\"prefix\":\"/fn/rl\",\"module_hash\":\"$H_ECHO\",\"rate_limit\":3}")
[ "$code" = "201" ] || { echo "FAIL: bind /fn/rl -> $code"; exit 1; }
rm -f rl-*.code
# NOTE the \n in -w: without it the .code files concatenate into one giant
# line and the grep -c counts below silently read 0.
seq 1 8 | xargs -P 8 -I{} sh -c \
  'curl -s -o /dev/null -w "%{http_code}\n" -X POST localhost:8080/fn/rl -d x > rl-{}.code'
n200=$(cat rl-*.code | grep -c "^200$" || true)
n429=$(cat rl-*.code | grep -c "^429$" || true)
rm -f rl-*.code
[ "$n200" = "3" ] && [ "$n429" = "5" ] \
  || { echo "FAIL: burst of 8 at limit 3 -> $n200 x200 / $n429 x429 (want exactly 3/5)"; exit 1; }
resp=$(curl -s -D - -X POST localhost:8080/fn/rl -d x)
echo "$resp" | grep -qi "^retry-after: [0-9]" || { echo "FAIL: 429 without Retry-After: $resp"; exit 1; }
echo "$resp" | tail -1 | python3 -c '
import json, sys
d = json.loads(sys.stdin.read())
assert d["error"] == "rate limited", d
assert d["limit"] == 3 and d["window_ms"] == 60000, d
assert 1 <= d["retry_after_ms"] <= 60000, d
print("429 body ok")' | grep -q "429 body ok" || { echo "FAIL: 429 body: $resp"; exit 1; }
n=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/rl'")
[ "$n" = "3" ] || { echo "FAIL: /fn/rl ledger rows: $n (429s must not write rows)"; exit 1; }
metric=$(curl -s localhost:8080/metrics | awk '/^keel_fn_rate_limited_total/ {print $2}')
[ "$metric" = "6" ] || { echo "FAIL: keel_fn_rate_limited_total=$metric (want 6: 5 burst + 1 probe)"; exit 1; }

# --- 3. per-app rate limit ---------------------------------------------------
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/apps \
  -H 'content-type: application/json' \
  -d "{\"name\":\"rl-app\",\"backend_hash\":\"$H_ECHO\",\"rate_limit\":1}")
[ "$code" = "201" ] || { echo "FAIL: create rl-app -> $code"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/apps/rl-app/api/x -d y)
[ "$code" = "200" ] || { echo "FAIL: first app call -> $code"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/apps/rl-app/api/x -d y)
[ "$code" = "429" ] || { echo "FAIL: second app call at limit 1 -> $code (want 429)"; exit 1; }

# --- 4. the window survives a restart (off-the-ledger = restart-safe) --------
kill -9 "$ENG" 2>/dev/null || true; ENG=""
./target/release/keel serve --db $DB --listen 127.0.0.1:8080 \
  --retain-ledger-hours 168 > engine2.log 2>&1 &
ENG=$!
wait_ready
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/rl -d x)
[ "$code" = "429" ] || { echo "FAIL: /fn/rl after restart -> $code (window must survive)"; exit 1; }

# --- 5. a backdated window frees the route (no sleeps — the ledger IS the state)
SQL "UPDATE invocations SET created_at = created_at - 61000 WHERE ref='/fn/rl'"
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/rl -d x)
[ "$code" = "200" ] || { echo "FAIL: /fn/rl after window aged out -> $code"; exit 1; }

# --- 6. --retain-ledger-hours sweeps aged rows on the immediate GC pass ------
SQL "UPDATE invocations SET created_at = created_at - 700*3600000 WHERE ref='/fn/logs'"
SQL "UPDATE fn_logs SET created_at = created_at - 700*3600000 WHERE ref='/fn/logs'"
kill -9 "$ENG" 2>/dev/null || true; ENG=""
./target/release/keel serve --db $DB --listen 127.0.0.1:8080 \
  --retain-ledger-hours 168 > engine3.log 2>&1 &
ENG=$!
wait_ready
swept=""
for i in $(seq 1 20); do
  a=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/logs'")
  b=$(SQL "SELECT COUNT(*) FROM fn_logs WHERE ref='/fn/logs'")
  if [ "$a" = "0" ] && [ "$b" = "0" ]; then swept=yes; break; fi
  sleep 0.5
done
[ "$swept" = "yes" ] || { echo "FAIL: ledger GC left invocations=$a fn_logs=$b for /fn/logs after 10s"; exit 1; }
n=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/rl'")
[ "$n" = "4" ] || { echo "FAIL: in-window /fn/rl rows: $n (GC must not touch them)"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal
echo "OPERATE PASS"
