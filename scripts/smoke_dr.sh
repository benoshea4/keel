#!/usr/bin/env bash
# v2 DR smoke test: periodic online backups + full disaster-recovery restore.
#
# The scenario with teeth: a workflow parks waiting for an event, the interval
# backup snapshots the LIVE database, then the "disaster" — engine killed and
# every database file deleted. Restore = copy the newest snapshot over the db
# path and start the engine: the workflow must come back still parked, take
# its event, and complete. Also checks the one-shot `keel backup` subcommand
# and snapshot pruning (--backup-keep).
set -euo pipefail
DB=smoke-dr.db; BK=smoke-dr-backups
rm -f $DB $DB-shm $DB-wal; rm -rf $BK

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
(cd guests/approval && cargo component build --release --target wasm32-unknown-unknown)

# 2s interval, keep 3 — fast snapshots, exercises pruning
./target/release/keel serve --db $DB --backup-dir $BK --backup-interval-secs 2 --backup-keep 3 \
  > engine.log 2>&1 & ENG=$!
wait_ready

H=$(curl -s -X POST --data-binary @guests/approval/target/wasm32-unknown-unknown/release/approval.wasm \
  "localhost:8080/api/modules?name=approval" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$H\",\"input\":{}}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
status() { curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

for i in $(seq 1 15); do ST=$(status); [ "$ST" = "waiting_event" ] && break; sleep 1; done
[ "$ST" = "waiting_event" ] || { echo "FAIL: expected waiting_event, got $ST"; exit 1; }

# one-shot subcommand works against the LIVE db
./target/release/keel backup --db $DB --to smoke-dr-oneshot.db
N=$(sqlite3 smoke-dr-oneshot.db "SELECT COUNT(*) FROM workflows WHERE id='$WF'")
[ "$N" = "1" ] || { echo "FAIL: one-shot backup missing the workflow"; exit 1; }
rm -f smoke-dr-oneshot.db

# let the interval loop produce snapshots PAST the parked state, and prune
sleep 7
SNAPS=$(ls $BK/keel-*.db | wc -l | tr -d ' ')
[ "$SNAPS" -le 3 ] || { echo "FAIL: pruning kept $SNAPS snapshots, want <= 3"; exit 1; }
[ "$SNAPS" -ge 1 ] || { echo "FAIL: no snapshots written"; exit 1; }
NEWEST=$(ls $BK/keel-*.db | sort | tail -1)
N=$(sqlite3 "$NEWEST" "SELECT COUNT(*) FROM workflows WHERE id='$WF' AND status='waiting_event'")
[ "$N" = "1" ] || { echo "FAIL: newest snapshot doesn't hold the parked workflow"; exit 1; }

# --- the disaster: engine dead, database GONE
kill -9 $ENG; sleep 1
rm -f $DB $DB-shm $DB-wal

# --- the restore: copy the snapshot back, start the engine
cp "$NEWEST" $DB
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!
wait_ready
for i in $(seq 1 15); do ST=$(status); [ "$ST" = "waiting_event" ] && break; sleep 1; done
[ "$ST" = "waiting_event" ] || { echo "FAIL: restored workflow is $ST, want waiting_event"; exit 1; }

# the restored workflow is fully alive: deliver the event, watch it finish
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/workflows/$WF/events \
  -H 'content-type: application/json' -d '{"name":"approve","payload":{"by":"dr-test"}}')
[ "$CODE" = "202" ] || { echo "FAIL: event POST returned $CODE"; exit 1; }
# approval sleeps 60s after the event; allow 90
for i in $(seq 1 90); do ST=$(status); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST after restore+event"; exit 1; }
sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'" | grep -q "dr-test" \
  || { echo "FAIL: output missing the delivered payload"; exit 1; }

kill $ENG || true
rm -rf $BK
echo "DR SMOKE PASS"
