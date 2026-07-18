#!/usr/bin/env bash
# v2.4 load gate: hundreds of concurrent workflows through a small worker cap.
#
# Proves, with N=200 workflows against --max-running 8:
#   * every workflow completes (no losses, no stuck permits);
#   * the cap is RESPECTED: keel_active_permits, sampled throughout the drain,
#     never exceeds 8 — and actually reaches the cap (the test really queued);
#   * journal integrity holds under concurrency: per workflow, seqs are dense
#     from 0 (COUNT == MAX+1; none of these workflows checkpoint).
N=200
MAXR=8
set -euo pipefail
DB=load-test.db; rm -f $DB $DB-shm $DB-wal

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
(cd guests/loadgen && cargo component build --release --target wasm32-unknown-unknown)

if curl -sf -o /dev/null --max-time 1 localhost:18081/count; then
  echo "FAIL: something is already listening on :18081 — kill it first"; exit 1
fi
python3 scripts/stub_server.py 18081 & STUB=$!
for i in $(seq 1 50); do
  curl -sf -o /dev/null localhost:18081/count && break; sleep 0.2
done

./target/release/keel serve --db $DB --max-running $MAXR > engine.log 2>&1 & ENG=$!
wait_ready

H=$(curl -s -X POST --data-binary @guests/loadgen/target/wasm32-unknown-unknown/release/loadgen.wasm \
  "localhost:8080/api/modules?name=loadgen" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')

# each workflow: one journaled http-get taking ~300ms at the stub, so the
# drain (~N*0.3/MAXR ≈ 8s) is long enough to sample the permit gauge hard
echo "starting $N workflows (cap $MAXR)..."
seq 1 $N | xargs -P 16 -I{} curl -s -o /dev/null -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$H\",\"input\":{\"url\":\"http://127.0.0.1:18081/slow?ms=300\"}}"

# drain, sampling the permit gauge every 200ms
PEAK=0
for i in $(seq 1 300); do
  # a raced/failed scrape yields empty — skip the sample, never trip set -e
  P=$(curl -s localhost:8080/metrics | awk '/^keel_active_permits/ {print $2}') || true
  if [ -n "$P" ]; then
    [ "$P" -gt "$PEAK" ] && PEAK=$P || true
    if [ "$P" -gt "$MAXR" ]; then
      echo "FAIL: keel_active_permits=$P exceeds --max-running $MAXR"; exit 1
    fi
  fi
  DONE=$(sqlite3 -cmd ".timeout 5000" $DB "SELECT COUNT(*) FROM workflows WHERE status='completed'")
  [ "$DONE" = "$N" ] && break
  sleep 0.2
done

DONE=$(sqlite3 -cmd ".timeout 5000" $DB "SELECT COUNT(*) FROM workflows WHERE status='completed'")
FAILED=$(sqlite3 -cmd ".timeout 5000" $DB "SELECT COUNT(*) FROM workflows WHERE status='failed'")
[ "$DONE" = "$N" ] || { echo "FAIL: $DONE/$N completed ($FAILED failed) after 60s"; exit 1; }
[ "$PEAK" -eq "$MAXR" ] || { echo "FAIL: permit peak was $PEAK, never reached the cap $MAXR — no real contention"; exit 1; }

# journal integrity: dense seqs from 0 for every workflow (these never
# checkpoint, so COUNT == MAX+1 exactly), and exactly one http-get row each
BAD=$(sqlite3 -cmd ".timeout 5000" $DB "SELECT COUNT(*) FROM (
  SELECT workflow_id FROM journal GROUP BY workflow_id
  HAVING COUNT(*) != MAX(seq) + 1)")
[ "$BAD" = "0" ] || { echo "FAIL: $BAD workflows have gapped journals"; exit 1; }
NJ=$(sqlite3 -cmd ".timeout 5000" $DB "SELECT COUNT(*) FROM journal WHERE kind='http-get'")
[ "$NJ" = "$N" ] || { echo "FAIL: expected $N http-get rows, got $NJ"; exit 1; }

kill $ENG || true; ENG=""
echo "LOAD TEST PASS ($N workflows, cap $MAXR, permit peak $PEAK)"
