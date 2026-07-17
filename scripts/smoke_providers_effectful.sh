#!/usr/bin/env bash
# v2.5 smoke test: EFFECTFUL capability providers, end to end.
#
# Proves:
#   * an effectful component under the PURE flag (--provider) FAILS THE BOOT
#     (the v2.2 import-free guarantee is intact);
#   * a pure component under --provider-effectful still works (the grant is a
#     superset), journaling exactly one terminal row, no internals;
#   * each provider wire call is journaled INDIVIDUALLY: kill -9 while the
#     provider is mid-second-POST -> recovery re-invokes the provider, but the
#     FIRST post replays from its own journal row (the stub sees it ONCE), and
#     the second re-sends carrying the SAME keel-idempotency-key (wfid:seq) —
#     the at-least-once window collapsed to one wire call, deduplicable;
#   * the re-sent 11s call survives the provider epoch budget (host-call time
#     is excused; only pure-wasm spin counts);
#   * a COMPLETED provider scope fast-forwards on replay without instantiating
#     the provider at all ("invoking provider" log line count);
#   * journal shape: provider-http:<name> internals + custom:<name>:<kind>
#     terminal, dense seq.
set -euo pipefail
DB=smoke-eff.db; rm -f $DB $DB-shm $DB-wal

ENG=""; STUB=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  if [ -n "${STUB:-}" ]; then kill -9 "$STUB" 2>/dev/null || true; fi
  rm -f smoke-eff-bad.db* eff-bad.log
}
trap cleanup EXIT

for port in 8080 18081; do
  if curl -sf -o /dev/null --max-time 1 localhost:$port/; then
    echo "FAIL: something is already listening on :$port — kill it first"; exit 1
  fi
done
wait_ready() {
  for i in $(seq 1 50); do
    curl -sf -o /dev/null localhost:8080/ && return 0
    sleep 0.2
  done
  echo "FAIL: engine did not answer on :8080 within 10s (see engine.log)"; exit 1
}

cargo build --release -p keel-engine
(cd providers/relay && cargo component build --release --target wasm32-unknown-unknown)
(cd providers/greet && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/relay && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/providerdemo && cargo component build --release --target wasm32-unknown-unknown)
RELAY=providers/relay/target/wasm32-unknown-unknown/release/relay.wasm
GREET=providers/greet/target/wasm32-unknown-unknown/release/greet.wasm

# 0. tier enforcement: the effectful relay component under the PURE flag must
# fail the boot. No `timeout` on macOS — poll for exit.
./target/release/keel serve --db smoke-eff-bad.db \
  --provider relay=$RELAY > eff-bad.log 2>&1 & BAD=$!
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
  echo "FAIL: engine kept running with an effectful component under --provider"; exit 1
fi
[ "$RC" != "0" ] || { echo "FAIL: pure tier accepted an importing provider"; cat eff-bad.log; exit 1; }
grep -q "provider 'relay'" eff-bad.log \
  || { echo "FAIL: boot refusal does not name the provider"; cat eff-bad.log; exit 1; }
grep -q "provider-effectful" eff-bad.log \
  || { echo "FAIL: boot refusal does not point at --provider-effectful"; cat eff-bad.log; exit 1; }

python3 scripts/stub_server.py 18081 & STUB=$!
sleep 0.5

start_engine() {
  ./target/release/keel serve --db $DB \
    --provider-effectful relay=$RELAY --provider-effectful greet=$GREET \
    >> engine.log 2>&1 & ENG=$!
  wait_ready
}
: > engine.log
start_engine

upload() { curl -s -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])'; }
start_wf() { curl -s -X POST localhost:8080/api/workflows -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$1\",\"input\":$2}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'; }
status() { curl -s localhost:8080/api/workflows/$1 \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }
hits() { curl -s localhost:18081/hits \
  | python3 -c "import sys,json;print(json.load(sys.stdin)['hits'].get('$1',0))"; }
keys() { curl -s localhost:18081/hits \
  | python3 -c "import sys,json;print(','.join(json.load(sys.stdin)['keys'].get('$1',[])))"; }

HR=$(upload guests/relay/target/wasm32-unknown-unknown/release/relay_guest.wasm relay-guest)
HP=$(upload guests/providerdemo/target/wasm32-unknown-unknown/release/providerdemo.wasm providerdemo)

# 1. crash MID-PROVIDER, between its two wire calls: first POST is instant,
# second parks the provider inside an 11s stub sleep — kill the engine there.
WF1=$(start_wf $HR '{"first_url":"http://127.0.0.1:18081/hook/a","second_url":"http://127.0.0.1:18081/hook/b?ms=11000"}')
for i in $(seq 1 40); do [ "$(hits /hook/b)" -ge 1 ] && break; sleep 0.3; done
[ "$(hits /hook/b)" -ge 1 ] || { echo "FAIL: second POST never reached the stub"; exit 1; }
kill -9 $ENG; sleep 1
start_engine
for i in $(seq 1 45); do ST=$(status $WF1); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: wf1 status=$ST after mid-provider crash recovery"; exit 1; }

# 1a. the committed first call was NOT re-fired; the in-flight second was
# re-sent once, with the SAME idempotency key both times (wfid:seq).
[ "$(hits /hook/a)" = "1" ] || { echo "FAIL: /hook/a hit $(hits /hook/a) times — committed wire call re-fired"; exit 1; }
[ "$(hits /hook/b)" = "2" ] || { echo "FAIL: /hook/b hit $(hits /hook/b) times — expected exactly 2 (crash resend)"; exit 1; }
[ "$(keys /hook/a)" = "$WF1:0" ] || { echo "FAIL: /hook/a key '$(keys /hook/a)' != $WF1:0"; exit 1; }
[ "$(keys /hook/b)" = "$WF1:1,$WF1:1" ] || { echo "FAIL: /hook/b keys '$(keys /hook/b)' != same key twice"; exit 1; }

# 1b. journal shape: two internals + one terminal + the sleep, dense.
IN1=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF1' AND kind='provider-http:relay'")
[ "$IN1" = "2" ] || { echo "FAIL: expected 2 provider-http:relay rows, got $IN1"; exit 1; }
T1=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF1' AND kind='custom:relay:relay'")
[ "$T1" = "1" ] || { echo "FAIL: expected 1 custom:relay:relay row, got $T1"; exit 1; }
ROWS=$(sqlite3 $DB "SELECT COUNT(*), MAX(seq)+1 FROM journal WHERE workflow_id='$WF1'")
[ "${ROWS%|*}" = "${ROWS#*|}" ] || { echo "FAIL: wf1 journal not dense: $ROWS"; exit 1; }
OUT1=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF1'")
echo "$OUT1" | grep -q 'first_status\\\":200' || { echo "FAIL: wf1 output missing first_status — $OUT1"; exit 1; }
echo "$OUT1" | grep -q 'second_status\\\":200' || { echo "FAIL: wf1 output missing second_status — $OUT1"; exit 1; }

# 1c. per-effect journaling means the provider itself IS re-invoked after a
# mid-scope crash (its committed calls replay; boundary journaling would show 1)
NI=$(grep -c "invoking provider relay kind relay" engine.log || true)
[ "$NI" = "2" ] || { echo "FAIL: relay invoked $NI times for wf1 — expected 2 (live + crash re-run)"; exit 1; }

# 2. crash AFTER the provider scope completed (in the guest's 5s sleep):
# recovery must fast-forward the recorded scope without instantiating relay.
WF2=$(start_wf $HR '{"first_url":"http://127.0.0.1:18081/hook/c","second_url":"http://127.0.0.1:18081/hook/d"}')
for i in $(seq 1 20); do ST=$(status $WF2); [ "$ST" = "sleeping" ] && break; sleep 0.5; done
[ "$ST" = "sleeping" ] || { echo "FAIL: wf2 expected sleeping before crash, got $ST"; exit 1; }
kill -9 $ENG; sleep 1
start_engine
for i in $(seq 1 30); do ST=$(status $WF2); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: wf2 status=$ST after post-scope crash recovery"; exit 1; }
[ "$(hits /hook/c)" = "1" ] || { echo "FAIL: /hook/c hit $(hits /hook/c) times"; exit 1; }
[ "$(hits /hook/d)" = "1" ] || { echo "FAIL: /hook/d hit $(hits /hook/d) times"; exit 1; }
NI=$(grep -c "invoking provider relay kind relay" engine.log || true)
[ "$NI" = "3" ] || { echo "FAIL: relay invocations $NI — wf2 recovery re-instantiated a completed scope"; exit 1; }

# 3. a PURE component under the effectful grant: works, and journals exactly
# one terminal row per call — no internals.
WF3=$(start_wf $HP '{}')
for i in $(seq 1 30); do ST=$(status $WF3); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: providerdemo under effectful grant: status=$ST"; exit 1; }
OUT3=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF3'")
echo "$OUT3" | grep -q 'hello keel' || { echo "FAIL: greet response missing — $OUT3"; exit 1; }
echo "$OUT3" | grep -q '"missing_err":true' || { echo "FAIL: unregistered provider did not err — $OUT3"; exit 1; }
GI=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF3' AND kind='provider-http:greet'")
[ "$GI" = "0" ] || { echo "FAIL: pure component journaled $GI internals"; exit 1; }
GT=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF3' AND kind='custom:greet:greet'")
[ "$GT" = "1" ] || { echo "FAIL: expected 1 custom:greet:greet row, got $GT"; exit 1; }

kill $ENG || true; ENG=""
echo "EFFECTFUL PROVIDERS SMOKE PASS"
