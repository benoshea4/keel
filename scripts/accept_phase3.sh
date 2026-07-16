#!/usr/bin/env bash
# Phase 3 acceptance (SPEC.md Task 3.8) — fresh DB; engine output collects in
# engine.log (>> on restarts); if it fails, fix the ENGINE, not this script.
# Definition of done: prints "PHASE 3 PASS" twice in a row, fresh DB each time.
# Hardened post-review: trap cleanup (no leaked server on failure), port
# preflight, readiness polling after every engine launch.
#
# What it proves: checkpoint prunes the journal (row count stays tiny across
# ticks), recovery goes through resume() from the snapshot (engine.log says
# "resuming", not a from-zero replay), a parked+checkpointed workflow live-
# upgrades v1→v2 (200) with its state migrated (output carries note:"upgraded"
# and the tick count continues to 8), and a terminal workflow refuses upgrade
# (409).
set -euo pipefail
DB=accept3.db; rm -f $DB $DB-shm $DB-wal

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
# one crate, two artifacts: same output filename — copy each aside (Task 3.7)
(cd guests/counter \
  && cargo component build --release --target wasm32-unknown-unknown \
  && cp target/wasm32-unknown-unknown/release/counter.wasm counter-v1.wasm \
  && cargo component build --release --target wasm32-unknown-unknown --features v2 \
  && cp target/wasm32-unknown-unknown/release/counter.wasm counter-v2.wasm)

./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!
wait_ready

H1=$(curl -s -X POST --data-binary @guests/counter/counter-v1.wasm \
  "localhost:8080/api/modules?name=counter-v1" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
H2=$(curl -s -X POST --data-binary @guests/counter/counter-v2.wasm \
  "localhost:8080/api/modules?name=counter-v2" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
[ "$H1" != "$H2" ] || { echo "FAIL: v1 and v2 hashes identical"; exit 1; }

WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$H1\",\"input\":{\"target\":8}}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

status() { curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

# 2. pruning: by ~12s the guest has checkpointed at least once (ticks at 5s/10s);
# without pruning the journal would grow ~2 rows per tick forever
sleep 12
SNAPS=$(sqlite3 $DB "SELECT COUNT(*) FROM snapshots WHERE workflow_id='$WF'")
[ "$SNAPS" = "1" ] || { echo "FAIL: expected a snapshots row, got $SNAPS"; exit 1; }
NJ=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF'")
[ "$NJ" -le 4 ] || { echo "FAIL: journal has $NJ rows — pruning is not working"; exit 1; }

# 3. kill -9 mid-sleep; recovery must go through resume(), not a from-zero replay
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!
wait_ready
grep -q "resuming" engine.log || { echo "FAIL: engine.log has no 'resuming' line"; exit 1; }

# 4. upgrade to v2 while parked. The workflow is sleeping ~99% of the time; if
# the POST lands exactly in the ~50ms tick window the engine rightly answers 409
# ("not parked") — re-poll and retry, the 200 assertion stands.
CODE=""
for attempt in 1 2 3; do
  for i in $(seq 1 15); do ST=$(status); [ "$ST" = "sleeping" ] && break; sleep 1; done
  [ "$ST" = "sleeping" ] || { echo "FAIL: expected sleeping before upgrade, got $ST"; exit 1; }
  CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/workflows/$WF/upgrade \
    -H 'content-type: application/json' -d "{\"module_hash\":\"$H2\"}")
  [ "$CODE" = "200" ] && break
done
[ "$CODE" = "200" ] || { echo "FAIL: upgrade returned $CODE"; exit 1; }

# 5. completion under v2 with migrated state (target 8, 5s ticks → well under 120s)
for i in $(seq 1 120); do ST=$(status); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST after 120s"; exit 1; }
OUT=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'")
echo "$OUT" | grep -q '"note":"upgraded"' || { echo "FAIL: output missing \"note\":\"upgraded\" — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"total":8' || { echo "FAIL: output missing \"total\":8 — got $OUT"; exit 1; }
MH=$(sqlite3 $DB "SELECT module_hash FROM workflows WHERE id='$WF'")
[ "$MH" = "$H2" ] || { echo "FAIL: workflows.module_hash is not the v2 hash"; exit 1; }

# 6. negative: a terminal workflow refuses upgrade
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/workflows/$WF/upgrade \
  -H 'content-type: application/json' -d "{\"module_hash\":\"$H1\"}")
[ "$CODE" = "409" ] || { echo "FAIL: upgrading a completed workflow returned $CODE, want 409"; exit 1; }

kill $ENG || true
echo "PHASE 3 PASS"
