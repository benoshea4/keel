#!/usr/bin/env bash
# v2.3 smoke test: kv versioning × live upgrade — the closed caveat.
#
# Proves:
#   * kv-set appends VERSIONS (two rows for one key after write-checkpoint-write);
#   * upgrading a parked workflow discards kv versions written by the journal
#     tail (seq > checkpoint) together with the tail itself;
#   * the upgraded module's resume() reads the pre-tail value — under v2.2
#     semantics it read the tail's value, which is exactly the bug this closes.
# (v2.3's other half — wire-only idempotency keys — is asserted in
# smoke_effects.sh, where http-request journal rows actually exist.)
set -euo pipefail
DB=smoke-kvup.db; rm -f $DB $DB-shm $DB-wal

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

cargo build --release -p keel-engine
(cd guests/kvup && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/kvup2 && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!
wait_ready

H1=$(curl -s -X POST --data-binary @guests/kvup/target/wasm32-unknown-unknown/release/kvup.wasm \
  "localhost:8080/api/modules?name=kvup" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
H2=$(curl -s -X POST --data-binary @guests/kvup2/target/wasm32-unknown-unknown/release/kvup2.wasm \
  "localhost:8080/api/modules?name=kvup2" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$H1\",\"input\":{}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
status() { curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

# 1. the guest writes below + above its checkpoint, then parks
for i in $(seq 1 10); do ST=$(status); [ "$ST" = "waiting_event" ] && break; sleep 1; done
[ "$ST" = "waiting_event" ] || { echo "FAIL: expected waiting_event, got $ST"; exit 1; }
NV=$(sqlite3 $DB "SELECT COUNT(*) FROM kv WHERE workflow_id='$WF' AND key='k'")
[ "$NV" = "2" ] || { echo "FAIL: expected 2 kv versions before upgrade, got $NV"; exit 1; }
TOP=$(sqlite3 $DB "SELECT value FROM kv WHERE workflow_id='$WF' AND key='k' ORDER BY seq DESC LIMIT 1")
[ "$TOP" = "above" ] || { echo "FAIL: latest pre-upgrade version is '$TOP', want 'above'"; exit 1; }

# 2. upgrade to kvup2 — the tail (and its kv version) must be discarded
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:8080/api/workflows/$WF/upgrade \
  -H 'content-type: application/json' -d "{\"module_hash\":\"$H2\"}")
[ "$CODE" = "200" ] || { echo "FAIL: upgrade returned $CODE"; exit 1; }
for i in $(seq 1 15); do ST=$(status); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST after upgrade"; exit 1; }

# 3. the upgraded module saw the PRE-TAIL value
OUT=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'")
echo "$OUT" | grep -q '"path":"v2-resume"' || { echo "FAIL: not resumed under v2 — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"k":"below"' \
  || { echo "FAIL: upgraded module read '$OUT' — tail kv version leaked through"; exit 1; }
NV=$(sqlite3 $DB "SELECT COUNT(*) FROM kv WHERE workflow_id='$WF' AND key='k'")
[ "$NV" = "1" ] || { echo "FAIL: expected 1 kv version after upgrade, got $NV"; exit 1; }
V=$(sqlite3 $DB "SELECT value FROM kv WHERE workflow_id='$WF' AND key='k'")
[ "$V" = "below" ] || { echo "FAIL: surviving version is '$V', want 'below'"; exit 1; }

kill $ENG || true; ENG=""
echo "KV-UPGRADE SMOKE PASS"
