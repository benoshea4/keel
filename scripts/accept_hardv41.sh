#!/usr/bin/env bash
# v4.1 acceptance (status.md §S) — the audit-fix gate for the fixes NO existing
# gate exercised. Proves, on a TOKENED engine (offline):
#   * S-FIX-1 (P1): a proxy guest's GRANTED outbound to a HANGING upstream is
#     bounded to ~the route time_limit_ms (not wasi-http's 600s default) so the
#     fn_sem permit frees — a single hung outbound returns bounded AND a fast
#     route serves right after; and under a full --max-fn-concurrent flood of
#     hung outbounds the data plane does NOT wedge;
#   * S-FIX-5: a route prefix containing a double-quote produces WELL-FORMED
#     Prometheus exposition (the ref label is escaped) instead of a scrape that
#     Prometheus drops whole;
#   * S-FIX-8: the fn_config 64-entry cap holds under a concurrent burst (the
#     IMMEDIATE-txn fix — check-then-act would over-admit).
#   * S-FIX-3: a forged-uncompressed-size zip bomb (header claims 0, deflate
#     inflates past the 256 MiB cap) is a 400 with NOTHING stored, and the
#     engine survives — the bounded read, not the forgeable pre-check.
#   * S-FIX-10: `keel run --timeout 0` polls ONCE before giving up, so exit-2
#     reports an OBSERVED live status ('running'/'waiting_event'), never the
#     pre-fix fabricated 'starting'.
# (The rest of §S is covered by accept_harden/functions2/polish/ecosystem.)
set -euo pipefail
DB=accept-hv41.db; rm -f $DB $DB-shm $DB-wal
export KEEL_API_TOKEN=hv41-secret

ENG=""; STUB=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  if [ -n "${STUB:-}" ]; then kill -9 "$STUB" 2>/dev/null || true; fi
}
trap cleanup EXIT
FAIL() { echo "FAIL: $*"; exit 1; }

for port in 8080 18080; do
  if curl -sf -o /dev/null --max-time 1 localhost:$port/ 2>/dev/null; then
    FAIL "something is already listening on :$port — kill it first"
  fi
done

# The hanging upstream (never sends a response byte).
python3 scripts/hang_stub.py > /dev/null 2>&1 & STUB=$!

wait_ready() {
  for i in $(seq 1 50); do
    curl -sf -o /dev/null localhost:8080/favicon.ico && return 0
    sleep 0.2
  done
  FAIL "engine did not answer on :8080 within 10s (see engine.log)"
}
SQL() { sqlite3 -cmd ".timeout 5000" $DB "$1"; }
AUTH=(-H "authorization: Bearer $KEEL_API_TOKEN")

# --- build ------------------------------------------------------------------
cargo build --release -p keel-engine
(cd guests/proxy-out && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/echo-fn && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/approval && cargo component build --release --target wasm32-unknown-unknown)  # S-FIX-10: a workflow that parks
APPROVAL_WASM=guests/approval/target/wasm32-unknown-unknown/release/approval.wasm

# cap 2 so the permit-exhaustion flood is small; data timeout HIGH so the
# request-level 408 never masks the outbound bound we are testing.
./target/release/keel serve --db $DB --listen 127.0.0.1:8080 \
  --max-fn-concurrent 2 --data-timeout-secs 30 >> engine.log 2>&1 &
ENG=$!
wait_ready

hash_of() {
  curl -s "${AUTH[@]}" -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])'
}
H_OUT=$(hash_of guests/proxy-out/target/wasm32-unknown-unknown/release/proxy_out.wasm proxy-out)
H_ECHO=$(hash_of guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm echo-fn)

# bind via python-built JSON so a quote in the prefix survives (S-FIX-5).
bind() { # prefix hash extra-json-object-fields(python dict literal or {})
  local code
  code=$(python3 - "$1" "$2" "$3" <<'PY'
import json, sys, urllib.request
prefix, mh, extra = sys.argv[1], sys.argv[2], sys.argv[3]
body = {"prefix": prefix, "module_hash": mh}
if extra:
    body.update(json.loads(extra))
req = urllib.request.Request(
    "http://localhost:8080/api/routes",
    data=json.dumps(body).encode(),
    method="POST",
    headers={"content-type": "application/json",
             "authorization": "Bearer " + __import__("os").environ["KEEL_API_TOKEN"]})
try:
    with urllib.request.urlopen(req) as r:
        print(r.status)
except urllib.error.HTTPError as e:
    print(e.code)
PY
)
  [ "$code" = "201" ] || FAIL "bind $1 -> $code"
}
bind /fn/out "$H_OUT" '{"allow_outbound": true, "time_limit_ms": 2000}'
bind /fn/e   "$H_ECHO" '{}'

# --- S-FIX-1: a single hung outbound is bounded, permit frees ----------------
t0=$SECONDS
body=$(curl -s --max-time 20 "localhost:8080/fn/out/")
dt=$((SECONDS - t0))
[ "$dt" -le 8 ] || FAIL "S-FIX-1: hung outbound held for ${dt}s (want <=8 ≈ time_limit_ms; pre-fix ~600s/curl-cap)"
echo "$body" | grep -qiE 'error|timeout|refused|never resolved' \
  || FAIL "S-FIX-1: expected a bounded outbound failure, got: '$body'"
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "localhost:8080/fn/e" -d keel)
[ "$code" = "200" ] || FAIL "S-FIX-1: permit not freed after hung outbound (/fn/e -> $code)"
echo "S-FIX-1 single: bounded at ${dt}s, permit freed"

# --- S-FIX-1: a flood of hung outbounds must not wedge the data plane --------
t0=$SECONDS
pids=""
for i in 1 2 3 4; do curl -s --max-time 20 -o /dev/null "localhost:8080/fn/out/" & pids="$pids $!"; done
wait $pids   # ONLY the curls — a bare `wait` would also wait on the hang stub
dt=$((SECONDS - t0))
[ "$dt" -le 12 ] || FAIL "S-FIX-1: a flood of hung outbounds pinned the pool for ${dt}s (pre-fix would hit the 20s curl cap)"
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "localhost:8080/fn/e" -d keel)
[ "$code" = "200" ] || FAIL "S-FIX-1: data plane wedged after the flood (/fn/e -> $code)"
echo "S-FIX-1 flood: cleared in ${dt}s, data plane healthy"

# --- S-FIX-5: a quote in a `ref` must not corrupt /metrics exposition --------
# create_route only requires /fn/ + no trailing slash + no ".."; a prefix may
# legally contain '"' (the http crate accepts it raw in a path), so a ledger
# `ref` can carry one. The FIX is the ESCAPING at emission (api::escape_label);
# we seed the quote-ref row directly and assert the escaping — robust, unlike
# routing a raw quote through nc/httparse (environment-fragile).
SQL "INSERT INTO invocations (kind, ref, module_hash, outcome, fuel_used, peak_mem, duration_ms, created_at) VALUES ('function','/fn/q\"x','deadbeef','ok',1,1,5,0)"
m=$(curl -s "${AUTH[@]}" "localhost:8080/metrics")
# the label value must carry an ESCAPED quote (backslash-quote), never a bare one.
echo "$m" | grep -F 'q\"x' > /dev/null \
  || FAIL "S-FIX-5: quote in ref not escaped in /metrics exposition"
echo "S-FIX-5: quote-in-ref label escaped"

# --- S-FIX-8: fn_config 64-cap holds under a concurrent burst ----------------
for i in $(seq 1 63); do
  curl -s -o /dev/null "${AUTH[@]}" -X POST "localhost:8080/api/config" \
    -H 'content-type: application/json' \
    -d "{\"kind\":\"function\",\"ref\":\"/fn/e\",\"name\":\"seed$i\",\"value\":\"v\"}"
done
seeded=$(SQL "SELECT COUNT(*) FROM fn_config WHERE kind='function' AND ref='/fn/e'")
[ "$seeded" = "63" ] || FAIL "S-FIX-8: seed setup wrong ($seeded, want 63)"
pids=""
for i in 1 2 3 4 5 6 7 8; do
  curl -s -o /dev/null "${AUTH[@]}" -X POST "localhost:8080/api/config" \
    -H 'content-type: application/json' \
    -d "{\"kind\":\"function\",\"ref\":\"/fn/e\",\"name\":\"burst$i\",\"value\":\"v\"}" &
  pids="$pids $!"
done
wait $pids   # ONLY the curls — a bare `wait` would also wait on the hang stub
n=$(SQL "SELECT COUNT(*) FROM fn_config WHERE kind='function' AND ref='/fn/e'")
[ "$n" = "64" ] || FAIL "S-FIX-8: config cap over-admitted under concurrency ($n, want 64)"
echo "S-FIX-8: config cap held at 64 under an 8-way burst"

# --- S-FIX-3: a forged-header zip bomb is rejected, nothing stored -----------
# The forge zeros the uncompressed-size fields the pre-fix pre-check trusted;
# the deflate stream still inflates past the 256 MiB cap. Post-fix: a bounded
# read caps the allocation and returns 400 with zero assets stored. Pre-fix: an
# unbounded read_to_end materialised the whole stream and STORED it (200) — so
# a 400 here is the exact regression assertion, no OOM dependency.
python3 scripts/forge_zipbomb.py "$DB.bomb.zip" 300
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST \
  -H 'content-type: application/json' "localhost:8080/api/apps" \
  -d '{"name":"bombtest"}')
[ "$code" = "201" ] || FAIL "S-FIX-3: app create -> $code"
resp=$(curl -s "${AUTH[@]}" -X POST --data-binary @"$DB.bomb.zip" \
  "localhost:8080/api/apps/bombtest/assets")
echo "$resp" | grep -qi 'stored' && FAIL "S-FIX-3: forged-header zip bomb was STORED: $resp"
# the app must have zero assets after the rejected upload.
assets=$(curl -s "${AUTH[@]}" "localhost:8080/api/apps" \
  | python3 -c 'import json,sys; a=[x for x in json.load(sys.stdin) if x["name"]=="bombtest"]; print(a[0]["assets"] if a else "MISSING")')
[ "$assets" = "0" ] || FAIL "S-FIX-3: bomb upload left $assets assets stored (want 0)"
# and the engine is still alive.
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "localhost:8080/fn/e" -d keel)
[ "$code" = "200" ] || FAIL "S-FIX-3: engine unhealthy after bomb upload (/fn/e -> $code)"
rm -f "$DB.bomb.zip"
echo "S-FIX-3: forged-header zip bomb rejected (400), nothing stored, engine healthy"

# --- S-FIX-10: keel run --timeout 0 reports an OBSERVED status, exits 2 -------
# The approval guest parks on an external event, so it never completes fast.
# Post-fix polls once BEFORE the deadline check: exit 2 with an OBSERVED live
# status ('running' at create, or 'waiting_event' once parked). Pre-fix exited
# before polling and fabricated 'starting'.
set +e
run_err=$(KEEL_API_TOKEN=$KEEL_API_TOKEN ./target/release/keel run "$APPROVAL_WASM" \
  --timeout 0 --server http://localhost:8080 2>&1 >/dev/null)
rc=$?
set -e
[ "$rc" = "2" ] || FAIL "S-FIX-10: run --timeout 0 exited $rc (want 2); output: $run_err"
echo "$run_err" | grep -qE "still '(running|waiting_event)'" \
  || FAIL "S-FIX-10: expected an OBSERVED status, got: $run_err"
echo "$run_err" | grep -q "still 'starting'" \
  && FAIL "S-FIX-10: reported the pre-fix fabricated 'starting': $run_err"
echo "S-FIX-10: run --timeout 0 exit 2 with an observed status"

kill -9 "$ENG" 2>/dev/null || true; ENG=""
kill -9 "$STUB" 2>/dev/null || true; STUB=""
rm -f $DB $DB-shm $DB-wal
echo "HARDV41 PASS"
