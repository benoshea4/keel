#!/usr/bin/env bash
# v4.0 acceptance (SPEC-AMENDMENT-3.md E7) — the ecosystem release: PURE
# wasi:http/proxy components (zero keel WIT) served by the keel dispatcher
# behind the same walls as keel-world functions.
#
# Proves, on a TOKENED engine, all offline (stub on :18080):
#   * a proxy-world component roundtrips (status + body from the guest), its
#     ledger row lands with outcome ok, its wasi:cli stdout line arrives in
#     /api/logs — while a keel-WIT route on the SAME engine still serves
#     (two worlds, one dispatcher);
#   * its wasi:keyvalue counter counts 1,2,3 → engine kill -9 → 4 (A7's
#     durability through the wasi surface) and /api/kv lists the key;
#   * outbound HTTP WITHOUT the grant is a clean in-band error (never a
#     hang); with allow_outbound=true the stub's answer relays and
#     keel_fn_outbound_total counts it;
#   * quotas hold for the proxy world: a starvation fuel re-bind → oof in
#     the ledger and {"outcome":"oof"} on the wire;
#   * a response past the 10 MiB cap is guest_error, not an engine wedge.
set -euo pipefail
DB=accept-eco.db; rm -f $DB $DB-shm $DB-wal
export KEEL_API_TOKEN=eco-secret

ENG=""; STUB=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  if [ -n "${STUB:-}" ]; then kill -9 "$STUB" 2>/dev/null || true; fi
}
trap cleanup EXIT

for port in 8080 18080; do
  if curl -sf -o /dev/null --max-time 1 localhost:$port/; then
    echo "FAIL: something is already listening on :$port — kill it first"; exit 1
  fi
done
python3 -m http.server 18080 --bind 127.0.0.1 --directory scripts/stub > /dev/null 2>&1 & STUB=$!
for i in $(seq 1 25); do curl -sf -o /dev/null localhost:18080/ping && break; sleep 0.2; done

wait_ready() {
  for i in $(seq 1 50); do
    curl -sf -o /dev/null localhost:8080/favicon.ico && return 0
    sleep 0.2
  done
  echo "FAIL: engine did not answer on :8080 within 10s (see engine.log)"; exit 1
}
SQL() { sqlite3 -cmd ".timeout 5000" $DB "$1"; }
AUTH=(-H "authorization: Bearer $KEEL_API_TOKEN")

# --- build ------------------------------------------------------------------
cargo build --release -p keel-engine
(cd guests/proxy-echo && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/proxy-out && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/echo-fn && cargo component build --release --target wasm32-unknown-unknown)

start_engine() {
  ./target/release/keel serve --db $DB --listen 127.0.0.1:8080 >> engine.log 2>&1 &
  ENG=$!
  wait_ready
}
start_engine

hash_of() {
  curl -s "${AUTH[@]}" -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])'
}
H_PX=$(hash_of guests/proxy-echo/target/wasm32-unknown-unknown/release/proxy_echo.wasm proxy-echo)
H_OUT=$(hash_of guests/proxy-out/target/wasm32-unknown-unknown/release/proxy_out.wasm proxy-out)
H_ECHO=$(hash_of guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm echo-fn)

bind() { # prefix hash extra-json
  local code
  code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/routes \
    -H 'content-type: application/json' \
    -d "{\"prefix\":\"$1\",\"module_hash\":\"$2\"$3}")
  [ "$code" = "201" ] || { echo "FAIL: bind $1 -> $code"; exit 1; }
}
bind /fn/px "$H_PX" ""
bind /fn/out "$H_OUT" ""
bind /fn/e "$H_ECHO" ""

# --- 1. two worlds, one dispatcher ------------------------------------------
resp=$(curl -s "localhost:8080/fn/px/hello?x=1")
[ "$resp" = "GET /hello?x=1" ] || { echo "FAIL: proxy roundtrip -> '$resp'"; exit 1; }
n=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/px' AND outcome='ok'")
[ "$n" = "1" ] || { echo "FAIL: proxy ledger rows: $n"; exit 1; }
logs=$(curl -s "${AUTH[@]}" "localhost:8080/api/logs?kind=function&ref=%2Ffn%2Fpx")
echo "$logs" | grep -q "proxy-echo: /hello" \
  || { echo "FAIL: wasi stdout line missing from /api/logs: $logs"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/e -d keel)
[ "$code" = "200" ] || { echo "FAIL: keel-world route beside proxy -> $code"; exit 1; }

# --- 2. wasi:keyvalue durability through kill -9 ----------------------------
for want in 1 2 3; do
  got=$(curl -s "localhost:8080/fn/px/count")
  [ "$got" = "$want" ] || { echo "FAIL: wasi kv count -> '$got' (want $want)"; exit 1; }
done
kill -9 "$ENG" 2>/dev/null || true; ENG=""
start_engine
got=$(curl -s "localhost:8080/fn/px/count")
[ "$got" = "4" ] || { echo "FAIL: count after kill -9 -> '$got' (want 4)"; exit 1; }
keys=$(curl -s "${AUTH[@]}" "localhost:8080/api/kv?kind=function&ref=%2Ffn%2Fpx")
echo "$keys" | grep -q '"count"' || { echo "FAIL: /api/kv missing 'count': $keys"; exit 1; }

# --- 3. outbound: denied by default, granted by the operator ----------------
resp=$(curl -s "localhost:8080/fn/out/")
case "$resp" in
  *[Dd]enied*) ;;
  *) echo "FAIL: ungranted outbound must surface a denial, got: '$resp'"; exit 1;;
esac
bind /fn/out "$H_OUT" ',"allow_outbound":true'
resp=$(curl -s "localhost:8080/fn/out/")
[ "$resp" = "upstream 200: pong" ] || { echo "FAIL: granted outbound -> '$resp'"; exit 1; }
metric=$(curl -s "${AUTH[@]}" localhost:8080/metrics | awk '/^keel_fn_outbound_total/ {print $2}')
[ "$metric" = "1" ] || { echo "FAIL: keel_fn_outbound_total=$metric (want 1)"; exit 1; }

# --- 4. quotas hold for the proxy world -------------------------------------
bind /fn/px "$H_PX" ',"fuel_limit":1000'
resp=$(curl -s -w "\n%{http_code}" "localhost:8080/fn/px/hello")
code=$(echo "$resp" | tail -1); body=$(echo "$resp" | head -1)
[ "$code" = "500" ] || { echo "FAIL: starved proxy -> $code (want 500)"; exit 1; }
echo "$body" | grep -q '"outcome":"oof"' || { echo "FAIL: starved proxy body: $body"; exit 1; }
n=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/px' AND outcome='oof'")
[ "$n" = "1" ] || { echo "FAIL: oof ledger rows: $n"; exit 1; }
bind /fn/px "$H_PX" ""   # restore

# --- 5. the 10 MiB response cap is a guest error, not a wedge ---------------
resp=$(curl -s -w "\n%{http_code}" --max-time 30 "localhost:8080/fn/px/big")
code=$(echo "$resp" | tail -1); body=$(echo "$resp" | head -1)
[ "$code" = "500" ] || { echo "FAIL: oversized response -> $code (want 500)"; exit 1; }
echo "$body" | grep -q '"outcome":"guest_error"' \
  || { echo "FAIL: oversized response body: $body"; exit 1; }
got=$(curl -s "localhost:8080/fn/px/count")
[ "$got" = "5" ] || { echo "FAIL: engine wedged after /big? count -> '$got' (want 5)"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
kill -9 "$STUB" 2>/dev/null || true; STUB=""
rm -f $DB $DB-shm $DB-wal
echo "ECOSYSTEM PASS"
