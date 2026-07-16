#!/usr/bin/env bash
# v1.2 + v1.3 smoke test: the new effect surface and the operability endpoints.
#
# Proves:
#   * http-request POSTs with headers/body, returns non-2xx as data (404);
#   * v2.1 per-call timeout-ms: a 3s endpoint with a 300ms budget errs as data;
#   * kv-set/kv-get are durable and journaled;
#   * kill -9 mid-workflow REPLAYS the new calls instead of re-executing them
#     (the stub's POST counter stays at 1 across the crash);
#   * interval schedules fire repeatedly and stop when deleted;
#   * v2.1 cron schedules fire by expression and PATCH enabled=false pauses;
#   * GET /api/workflows paging filter, /metrics, and retention GC work.
set -euo pipefail
DB=smoke-effects.db; rm -f $DB $DB-shm $DB-wal

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
(cd guests/effects && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)

# effects stub on :18081
if curl -sf -o /dev/null --max-time 1 localhost:18081/count; then
  echo "FAIL: something is already listening on :18081 — kill it first"; exit 1
fi
python3 scripts/stub_server.py 18081 & STUB=$!
STUB_UP=""
for i in $(seq 1 50); do
  if curl -sf -o /dev/null localhost:18081/count; then STUB_UP=1; break; fi
  sleep 0.2
done
[ -n "$STUB_UP" ] || { echo "FAIL: stub server did not start on :18081"; exit 1; }

./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!
wait_ready

HE=$(curl -s -X POST --data-binary @guests/effects/target/wasm32-unknown-unknown/release/effects.wasm \
  "localhost:8080/api/modules?name=effects" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HE\",\"input\":{\"base\":\"http://127.0.0.1:18081\"}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
status() { curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

# 1. the effects run fast, then the guest parks in its 8s sleep — kill it there
for i in $(seq 1 10); do ST=$(status); [ "$ST" = "sleeping" ] && break; sleep 1; done
[ "$ST" = "sleeping" ] || { echo "FAIL: expected sleeping before crash, got $ST"; exit 1; }
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!
wait_ready
for i in $(seq 1 30); do ST=$(status); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST after recovery"; exit 1; }

# 2. replay proof: recovery must NOT have re-POSTed. And the wire carried the
# v2.3 idempotency key for exactly this call: workflow_id:seq (the POST is the
# guest's second host call → seq 1) — the remote's handle for deduping the
# at-least-once window.
COUNT=$(curl -s localhost:18081/count)
POSTS=$(echo "$COUNT" | python3 -c 'import sys,json;print(json.load(sys.stdin)["posts"])')
KEY=$(echo "$COUNT" | python3 -c 'import sys,json;print(json.load(sys.stdin)["key"])')
[ "$POSTS" = "1" ] || { echo "FAIL: stub saw $POSTS POSTs — http-request re-executed on replay"; exit 1; }
[ "$KEY" = "$WF:1" ] || { echo "FAIL: idempotency key was '$KEY', want '$WF:1'"; exit 1; }
# ...and the key is WIRE-ONLY: journaled requests keep exactly what the guest
# asked (this is what keeps pre-v2.3 journals replayable).
if sqlite3 $DB "SELECT request FROM journal WHERE kind='http-request'" | grep -q "keel-idempotency-key"; then
  echo "FAIL: keel-idempotency-key leaked into the journal (must be wire-only)"; exit 1
fi

# 3. output + journal shape
OUT=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'")
echo "$OUT" | grep -q '"echo_status":200'  || { echo "FAIL: echo_status missing — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"miss_status":404'  || { echo "FAIL: 404-as-data missing — got $OUT"; exit 1; }
echo "$OUT" | grep -q 'hello keel'         || { echo "FAIL: echoed body missing — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"slow_timed_out":true' || { echo "FAIL: 300ms timeout did not fire — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"p1":true'          || { echo "FAIL: first kv-get wrong — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"phase":"two"'      || { echo "FAIL: post-crash kv-get wrong — got $OUT"; exit 1; }
for want in "http-request 3" "kv-set 2" "kv-get 2" "sleep-ms 1"; do
  KIND=${want% *}; N=${want#* }
  GOT=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF' AND kind='$KIND'")
  [ "$GOT" = "$N" ] || { echo "FAIL: expected $N $KIND rows, got $GOT"; exit 1; }
done
V=$(sqlite3 $DB "SELECT value FROM kv WHERE workflow_id='$WF' AND key='phase' ORDER BY seq DESC LIMIT 1")
[ "$V" = "two" ] || { echo "FAIL: kv latest version has phase='$V', want 'two'"; exit 1; }
NV=$(sqlite3 $DB "SELECT COUNT(*) FROM kv WHERE workflow_id='$WF' AND key='phase'")
[ "$NV" = "2" ] || { echo "FAIL: expected 2 kv versions of 'phase' (v2.3 append-only), got $NV"; exit 1; }

# 4. schedules: counter target=0 completes instantly; a 2s interval must fire
# at least twice in ~5.5s, and deleting the schedule stops it
HC=$(curl -s -X POST --data-binary @guests/counter/target/wasm32-unknown-unknown/release/counter.wasm \
  "localhost:8080/api/modules?name=counter" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
SCHED=$(curl -s -X POST localhost:8080/api/schedules \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HC\",\"input\":{\"target\":0},\"interval_ms\":2000}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
sleep 6
N1=$(sqlite3 $DB "SELECT COUNT(*) FROM workflows WHERE module_hash='$HC'")
[ "$N1" -ge 2 ] || { echo "FAIL: schedule fired $N1 times in 6s, want >= 2"; exit 1; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE localhost:8080/api/schedules/$SCHED)
[ "$CODE" = "204" ] || { echo "FAIL: schedule delete returned $CODE"; exit 1; }
sleep 3
N2=$(sqlite3 $DB "SELECT COUNT(*) FROM workflows WHERE module_hash='$HC'")
[ "$N2" -le $((N1 + 1)) ] || { echo "FAIL: schedule kept firing after delete ($N1 -> $N2)"; exit 1; }

# 4b. v2.1 cron schedules: every-2s expression fires, PATCH pauses, bad input 400s
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/schedules \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HC\",\"input\":{},\"interval_ms\":2000,\"cron\":\"* * * * * *\"}")
[ "$CODE" = "400" ] || { echo "FAIL: interval_ms+cron together returned $CODE, want 400"; exit 1; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/schedules \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HC\",\"input\":{},\"cron\":\"not a cron\"}")
[ "$CODE" = "400" ] || { echo "FAIL: junk cron returned $CODE, want 400"; exit 1; }
CRON=$(curl -s -X POST localhost:8080/api/schedules \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HC\",\"input\":{\"target\":0},\"cron\":\"*/2 * * * * *\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
curl -s localhost:8080/api/schedules | grep -Fq '"cron":"*/2 * * * * *"' \
  || { echo "FAIL: /api/schedules does not surface the cron field"; exit 1; }
C0=$(sqlite3 $DB "SELECT COUNT(*) FROM workflows WHERE module_hash='$HC'")
sleep 6
C1=$(sqlite3 $DB "SELECT COUNT(*) FROM workflows WHERE module_hash='$HC'")
[ $((C1 - C0)) -ge 2 ] || { echo "FAIL: cron fired $((C1 - C0)) times in 6s, want >= 2"; exit 1; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH localhost:8080/api/schedules/$CRON \
  -H 'content-type: application/json' -d '{"enabled":false}')
[ "$CODE" = "200" ] || { echo "FAIL: PATCH enabled=false returned $CODE"; exit 1; }
sleep 3
C2=$(sqlite3 $DB "SELECT COUNT(*) FROM workflows WHERE module_hash='$HC'")
[ "$C2" -le $((C1 + 1)) ] || { echo "FAIL: cron kept firing while disabled ($C1 -> $C2)"; exit 1; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH localhost:8080/api/schedules/no-such \
  -H 'content-type: application/json' -d '{"enabled":true}')
[ "$CODE" = "404" ] || { echo "FAIL: PATCH unknown schedule returned $CODE, want 404"; exit 1; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE localhost:8080/api/schedules/$CRON)
[ "$CODE" = "204" ] || { echo "FAIL: cron schedule delete returned $CODE"; exit 1; }

# 4c. v2.4 schedules UI: the page renders and lists the schedule we created
CRON2=$(curl -s -X POST localhost:8080/api/schedules \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HC\",\"input\":{\"target\":0},\"cron\":\"0 0 12 * * *\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
PAGE=$(curl -s localhost:8080/schedules)
echo "$PAGE" | grep -q "Create schedule" || { echo "FAIL: /schedules page missing the create form"; exit 1; }
echo "$PAGE" | grep -q "${CRON2:0:8}"    || { echo "FAIL: /schedules page does not list the schedule"; exit 1; }
echo "$PAGE" | grep -q "cron 0 0 12"     || { echo "FAIL: /schedules page does not show the cron expression"; exit 1; }
# form-shape create (the page's POST) + form-shape PATCH
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/schedules \
  --data-urlencode "module_hash=$HC" --data-urlencode 'input={"target":0}' \
  --data-urlencode "interval_ms=" --data-urlencode "cron=0 30 9 * * 1-5")
[ "$CODE" = "200" ] || { echo "FAIL: form-shape schedule create returned $CODE"; exit 1; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH localhost:8080/api/schedules/$CRON2 \
  --data-urlencode "enabled=false")
[ "$CODE" = "200" ] || { echo "FAIL: form-shape PATCH returned $CODE"; exit 1; }
sqlite3 $DB "DELETE FROM schedules" # stop everything before the GC section

# 4d. v2.4 kv on the workflow page (latest version per key)
WPAGE=$(curl -s localhost:8080/workflows/$WF)
echo "$WPAGE" | grep -q "Durable KV" || { echo "FAIL: workflow page missing the KV section"; exit 1; }
echo "$WPAGE" | grep -q "phase"      || { echo "FAIL: workflow page KV section missing the key"; exit 1; }

# 5. list API + metrics
LC=$(curl -s "localhost:8080/api/workflows?status=completed&limit=2" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')
[ "$LC" = "2" ] || { echo "FAIL: list limit=2 returned $LC rows"; exit 1; }
curl -s localhost:8080/metrics | grep -q 'keel_workflows{status="completed"}' \
  || { echo "FAIL: /metrics missing keel_workflows"; exit 1; }

# 6. retention GC: age every terminal row 2h into the past, restart with a 1h
# window (first sweep runs at startup), and they must be gone
kill -9 $ENG; sleep 1
sqlite3 $DB "UPDATE workflows SET updated_at = updated_at - 7200000 WHERE status IN ('completed','failed')"
./target/release/keel serve --db $DB --retain-terminal-hours 1 >> engine.log 2>&1 & ENG=$!
wait_ready
sleep 2
LEFT=$(sqlite3 $DB "SELECT COUNT(*) FROM workflows WHERE status IN ('completed','failed')")
[ "$LEFT" = "0" ] || { echo "FAIL: GC left $LEFT terminal workflows"; exit 1; }
LEFTJ=$(sqlite3 $DB "SELECT COUNT(*) FROM journal")
[ "$LEFTJ" = "0" ] || { echo "FAIL: GC left $LEFTJ journal rows"; exit 1; }

kill $ENG || true
echo "EFFECTS SMOKE PASS"
