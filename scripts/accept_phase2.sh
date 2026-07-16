#!/usr/bin/env bash
# Phase 2 acceptance (SPEC.md Task 2.10) — same style as phase 1: fresh DB, engine
# output collects in engine.log (>> on restarts), and if it fails you fix the
# ENGINE, not this script. Definition of done: prints "PHASE 2 PASS" twice in a
# row, fresh DB each time.
#
# What it proves: await-event parks durably (kill -9 at the park point survives),
# event delivery is exactly-once (1 journal row), the durable sleep keeps its
# original wake_at across a crash (W1 == W2 — remaining-time sleep, not a
# restarted one), and the UI serves its pages + embedded assets.
set -euo pipefail
DB=accept2.db; rm -f $DB $DB-shm $DB-wal
cargo build --release -p keel-engine
(cd guests/approval && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!; sleep 1

HASH=$(curl -s -X POST --data-binary @guests/approval/target/wasm32-unknown-unknown/release/approval.wasm \
  "localhost:8080/api/modules?name=approval" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HASH\",\"input\":{}}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

status() { curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

# 2. reach the first park point, then kill -9 mid-wait
for i in $(seq 1 15); do ST=$(status); [ "$ST" = "waiting_event" ] && break; sleep 1; done
[ "$ST" = "waiting_event" ] || { echo "FAIL: expected waiting_event before crash, got $ST"; exit 1; }
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!
# recovery legitimately passes through 'running' while replaying — poll up to 15s,
# an immediate assert here is a flake, not a check (SPEC.md Task 2.10 step 2)
for i in $(seq 1 15); do ST=$(status); [ "$ST" = "waiting_event" ] && break; sleep 1; done
[ "$ST" = "waiting_event" ] || { echo "FAIL: expected waiting_event after restart, got $ST"; exit 1; }

# 3. deliver the approval; the guest moves on to its durable 60s sleep
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/workflows/$WF/events \
  -H 'content-type: application/json' -d '{"name":"approve","payload":{"by":"alice"}}')
[ "$CODE" = "202" ] || { echo "FAIL: event POST returned $CODE"; exit 1; }
for i in $(seq 1 15); do ST=$(status); [ "$ST" = "sleeping" ] && break; sleep 1; done
[ "$ST" = "sleeping" ] || { echo "FAIL: expected sleeping after event, got $ST"; exit 1; }
W1=$(sqlite3 $DB "SELECT wake_at FROM timers WHERE workflow_id='$WF'")
[ -n "$W1" ] || { echo "FAIL: no timers row while sleeping"; exit 1; }

# 4. kill -9 mid-sleep; the wake deadline must survive the crash UNCHANGED
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!; sleep 1
W2=$(sqlite3 $DB "SELECT wake_at FROM timers WHERE workflow_id='$WF'")
[ "$W1" = "$W2" ] || { echo "FAIL: wake_at changed across restart ($W1 -> $W2): sleep restarted instead of resuming"; exit 1; }

# 5. completion within the sleep's remainder (60s total; allow 90)
for i in $(seq 1 90); do ST=$(status); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST after 90s"; exit 1; }
# check the DB's output column, NOT the API response (same caveat as phase 1)
sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'" | grep -q "alice" \
  || { echo "FAIL: output does not contain alice"; exit 1; }
N=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF' AND kind='await-event'")
[ "$N" = "1" ] || { echo "FAIL: expected exactly 1 await-event row, got $N"; exit 1; }

# 6. UI smoke
curl -s localhost:8080/ | grep -q "Workflows" || { echo "FAIL: dashboard missing Workflows"; exit 1; }
curl -s localhost:8080/workflows/$WF | grep -q "$WF" || { echo "FAIL: workflow page missing the id"; exit 1; }
[ -n "$(curl -s localhost:8080/assets/htmx.min.js | head -c 100)" ] || { echo "FAIL: htmx asset empty"; exit 1; }

kill $ENG || true
echo "PHASE 2 PASS"
