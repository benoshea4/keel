#!/usr/bin/env bash
# v3.3 acceptance (status.md §P, P-FIX-1..5) — hardening the public plane.
#
# Proves:
#   * P-FIX-1: a provoked ENGINE fault on the public plane (a module that
#     passes the \0asm door check but does not compile) answers exactly
#     {"error":"internal error"} — no hash, no wasmtime chain — while the
#     real chain lands in engine.log ("public-plane"), and engine faults
#     still write NO ledger row;
#   * P-FIX-2 (functions): with --max-fn-concurrent 2, 6 concurrent slow
#     requests admit EXACTLY 2 (the rest 503 "engine at capacity" with
#     Retry-After), keel_fn_over_capacity_total counts every rejection,
#     503s write NO ledger row, permits free when runs end — and an app's
#     ASSETS still serve while function capacity is saturated (the asset
#     branch takes no permit);
#   * P-FIX-2 (judge): two back-to-back submissions both reach a verdict —
#     the 1-permit judge QUEUES, it never rejects or deadlocks;
#   * P-FIX-3/4: after three distinct modules have executed with
#     --max-compiled-modules 2, keel_compiled_cache_size is exactly 2 and
#     an evicted module still serves (transparent recompile);
#   * P-FIX-5: a slow-drip body on the data plane answers 408 at
#     --data-timeout-secs, and the happy path is untouched.
set -euo pipefail
DB=accept-hard.db; rm -f $DB $DB-shm $DB-wal hard-*.code hard.zip bad.wasm

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
(cd guests/burn-fn && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/sum-solver && cargo component build --release --target wasm32-unknown-unknown)

# Small caps on purpose: the bounds must be observable, not theoretical.
# data-timeout 4s clears the burn route's 2000ms time quota with margin.
./target/release/keel serve --db $DB --listen 127.0.0.1:8080 \
  --max-fn-concurrent 2 --max-compiled-modules 2 --data-timeout-secs 4 \
  > engine.log 2>&1 &
ENG=$!
wait_ready

hash_of() {
  curl -s -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])'
}
H_ECHO=$(hash_of guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm echo-fn)
H_BURN=$(hash_of guests/burn-fn/target/wasm32-unknown-unknown/release/burn_fn.wasm burn-fn)
H_SUM=$(hash_of guests/sum-solver/target/wasm32-unknown-unknown/release/sum_solver.wasm sum-solver)
# Passes the \0asm door check, fails compilation — the engine-fault fixture.
printf '\0asmJUNKJUNKJUNK' > bad.wasm
H_BAD=$(hash_of bad.wasm bad)
rm -f bad.wasm

bind() { # prefix hash extra-json-fields
  local code
  code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/routes \
    -H 'content-type: application/json' \
    -d "{\"prefix\":\"$1\",\"module_hash\":\"$2\"$3}")
  [ "$code" = "201" ] || { echo "FAIL: bind $1 -> $code"; exit 1; }
}
bind /fn/e "$H_ECHO" ""
# Huge fuel so the TIME quota (2000ms) is what ends a burn — each request
# deterministically holds an execution slot for ~2s.
bind /fn/burn "$H_BURN" ',"fuel_limit":50000000000,"time_limit_ms":2000'
bind /fn/broken "$H_BAD" ""

# An asset-only app: must keep serving while function capacity is saturated.
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/apps \
  -H 'content-type: application/json' -d '{"name":"h"}')
[ "$code" = "201" ] || { echo "FAIL: create app h -> $code"; exit 1; }
python3 - <<'PY'
import zipfile
z = zipfile.ZipFile('hard.zip', 'w')
z.writestr('index.html', '<h1>up</h1>')
z.close()
PY
curl -s -o /dev/null -X POST --data-binary @hard.zip localhost:8080/api/apps/h/assets
rm -f hard.zip

# --- 1. P-FIX-1: engine faults answer generic, log verbose ------------------
body=$(curl -s -X POST localhost:8080/fn/broken -d x)
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/broken -d x)
[ "$code" = "500" ] || { echo "FAIL: /fn/broken -> $code (want 500)"; exit 1; }
[ "$body" = '{"error":"internal error"}' ] \
  || { echo "FAIL: public fault body leaked: $body"; exit 1; }
case "$body" in *"$H_BAD"*|*wasm*|*compil*) echo "FAIL: internals in body: $body"; exit 1;; esac
grep -q "public-plane" engine.log \
  || { echo "FAIL: engine.log has no public-plane detail line"; exit 1; }
n=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/broken'")
[ "$n" = "0" ] || { echo "FAIL: engine faults wrote $n ledger rows"; exit 1; }

# --- 2. P-FIX-2: the 503's anatomy + assets stay up under saturation --------
curl -s -o /dev/null -X POST localhost:8080/fn/burn -d x &
B1=$!
curl -s -o /dev/null -X POST localhost:8080/fn/burn -d x &
B2=$!
sleep 0.6   # both burns hold their permits now (each runs ~2s)
resp=$(curl -s -D - -X POST localhost:8080/fn/burn -d x)
echo "$resp" | head -1 | grep -q " 503" || { echo "FAIL: probe under saturation: $resp"; exit 1; }
echo "$resp" | grep -qi "^retry-after: 1" || { echo "FAIL: 503 without Retry-After: 1: $resp"; exit 1; }
echo "$resp" | tail -1 | grep -q '"engine at capacity"' \
  || { echo "FAIL: 503 body: $resp"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" localhost:8080/apps/h/)
[ "$code" = "200" ] \
  || { echo "FAIL: asset serve under fn saturation -> $code (assets must not take permits)"; exit 1; }
wait $B1 $B2

# --- 3. P-FIX-2: the cap is EXACT under a concurrent burst ------------------
rm -f hard-*.code
# \n in -w (the accept_operate lesson): without it the .code files
# concatenate into one line and the greps below silently count 0.
seq 1 6 | xargs -P 6 -I{} sh -c \
  'curl -s -o /dev/null -w "%{http_code}\n" -X POST localhost:8080/fn/burn -d x > hard-{}.code'
n503=$(cat hard-*.code | grep -c "^503$" || true)
n500=$(cat hard-*.code | grep -c "^500$" || true)
rm -f hard-*.code
[ "$n503" = "4" ] && [ "$n500" = "2" ] \
  || { echo "FAIL: burst of 6 at cap 2 -> ${n500}x500 / ${n503}x503 (want exactly 2/4)"; exit 1; }
metric=$(curl -s localhost:8080/metrics | awk '/^keel_fn_over_capacity_total/ {print $2}')
[ "$metric" = "5" ] \
  || { echo "FAIL: keel_fn_over_capacity_total=$metric (want 5: 1 anatomy probe + 4 burst)"; exit 1; }
rows=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/burn'")
[ "$rows" = "4" ] || { echo "FAIL: /fn/burn ledger rows: $rows (503s must not write rows)"; exit 1; }
tle=$(SQL "SELECT COUNT(*) FROM invocations WHERE ref='/fn/burn' AND outcome='tle'")
[ "$tle" = "4" ] || { echo "FAIL: burn outcomes not all tle: $tle/4"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/e -d probe)
[ "$code" = "200" ] || { echo "FAIL: /fn/e after burst -> $code (permits must free)"; exit 1; }

# --- 4. P-FIX-2: the judge queues at 1 permit, never rejects ----------------
out=$(curl -s -X POST localhost:8080/api/problems -H 'content-type: application/json' \
  -d '{"slug":"hsum","title":"Sum","statement":"sum ints","cases":[{"input":"2\n1 2","expected":"3"}]}')
echo "$out" | grep -q '"cases":1' || { echo "FAIL: seeding problem: $out"; exit 1; }
sid1=$(curl -s -X POST localhost:8080/api/submissions -H 'content-type: application/json' \
  -d "{\"problem\":\"hsum\",\"module_hash\":\"$H_SUM\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
sid2=$(curl -s -X POST localhost:8080/api/submissions -H 'content-type: application/json' \
  -d "{\"problem\":\"hsum\",\"module_hash\":\"$H_SUM\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
verdict_of() {
  local v
  for i in $(seq 1 60); do
    v=$(curl -s localhost:8080/api/submissions/$1 \
      | python3 -c 'import json,sys; v=json.load(sys.stdin)["verdict"]; print(v or "")')
    if [ -n "$v" ]; then echo "$v"; return 0; fi
    sleep 0.5
  done
  echo "TIMEOUT"
}
v1=$(verdict_of "$sid1"); v2=$(verdict_of "$sid2")
[ "$v1" = "AC" ] && [ "$v2" = "AC" ] \
  || { echo "FAIL: queued submissions -> $v1/$v2 (want AC/AC — the queue must drain)"; exit 1; }

# --- 5. P-FIX-3/4: the compile cache is bounded and eviction is transparent -
# Three distinct modules have executed (echo, burn, sum); the bad one never
# compiled. With --max-compiled-modules 2 the gauge must sit AT the cap.
gauge=$(curl -s localhost:8080/metrics | awk '/^keel_compiled_cache_size/ {print $2}')
[ "$gauge" = "2" ] || { echo "FAIL: keel_compiled_cache_size=$gauge (want exactly 2)"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/e -d again)
[ "$code" = "200" ] || { echo "FAIL: /fn/e post-eviction -> $code (recompile must be transparent)"; exit 1; }

# --- 6. P-FIX-5: slow-drip bodies die at the deadline; happy path unaffected -
python3 - <<'PY' || { echo "FAIL: slow-drip did not answer 408"; exit 1; }
import socket
s = socket.create_connection(("127.0.0.1", 8080), timeout=15)
s.sendall(b"POST /fn/e HTTP/1.1\r\nHost: localhost\r\n"
          b"Content-Type: text/plain\r\nContent-Length: 100000\r\n\r\n")
s.sendall(b"drip")  # ...and stall: the engine must not wait past the deadline
s.settimeout(15)
first = s.recv(4096).split(b"\r\n")[0].decode()
assert " 408 " in first, f"want 408, got: {first}"
print("drip ok")
PY
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/e -d hi)
[ "$code" = "200" ] || { echo "FAIL: happy path after drip -> $code"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal
echo "HARDEN PASS"
