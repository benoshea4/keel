#!/usr/bin/env bash
# v2.1 smoke test: the secret host call, end to end.
#
# Proves:
#   * secret() hands the guest the live value while the JOURNAL records only
#     {name} -> sha256(value) — the raw value appears NOWHERE in the database;
#   * the journaled http-request carries a {{secret:...}} placeholder while
#     the stub receives the real value on the wire;
#   * kill -9 mid-workflow -> replay verifies the hash against the LIVE file
#     and completes without re-POSTing (stub counter stays put);
#   * rotating the secret under a crashed workflow makes recovery fail LOUDLY
#     ("changed mid-workflow"), never silently resume with the new value;
#   * Amendment 4 — the ENV secret-store adapter gives the SAME guarantees
#     (value on the wire, {{secret:...}} in the journal, hash-only rows, no
#     raw leak) from `--secrets-env-prefix` instead of a file.
set -euo pipefail
DB=smoke-secrets.db; rm -f $DB $DB-shm $DB-wal
SECRETS=smoke-secrets.env
V1="sk-live-Zr9tQx7788"
V2="sk-live-ROTATED-1234"

ENG=""; STUB=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  if [ -n "${STUB:-}" ]; then kill -9 "$STUB" 2>/dev/null || true; fi
  rm -f $SECRETS
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
(cd guests/secrets && cargo component build --release --target wasm32-unknown-unknown)

# stub on :18081 (records the authorization header POSTs carry)
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

printf 'stub-token=%s\n' "$V1" > $SECRETS
chmod 600 $SECRETS

./target/release/keel serve --db $DB --secrets-file $SECRETS > engine.log 2>&1 & ENG=$!
wait_ready

HS=$(curl -s -X POST --data-binary @guests/secrets/target/wasm32-unknown-unknown/release/secrets.wasm \
  "localhost:8080/api/modules?name=secrets" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
new_wf() {
  curl -s -X POST localhost:8080/api/workflows \
    -H 'content-type: application/json' \
    -d "{\"module_hash\":\"$HS\",\"input\":{\"base\":\"http://127.0.0.1:18081\"}}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}
status() { curl -s localhost:8080/api/workflows/$1 | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

# 1. happy path: read secret, POST it, crash in the 6s sleep, recover
WF=$(new_wf)
for i in $(seq 1 10); do ST=$(status $WF); [ "$ST" = "sleeping" ] && break; sleep 1; done
[ "$ST" = "sleeping" ] || { echo "FAIL: expected sleeping before crash, got $ST"; exit 1; }
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB --secrets-file $SECRETS >> engine.log 2>&1 & ENG=$!
wait_ready
for i in $(seq 1 30); do ST=$(status $WF); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST after recovery"; exit 1; }

OUT=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'")
echo "$OUT" | grep -q '"status":200'  || { echo "FAIL: POST status missing — got $OUT"; exit 1; }
echo "$OUT" | grep -q '"stable":true' || { echo "FAIL: post-recovery secret read differs — got $OUT"; exit 1; }

# 2. replay proof + the wire carried the REAL value
COUNT=$(curl -s localhost:18081/count)
POSTS=$(echo "$COUNT" | python3 -c 'import sys,json;print(json.load(sys.stdin)["posts"])')
AUTH=$(echo "$COUNT"  | python3 -c 'import sys,json;print(json.load(sys.stdin)["auth"])')
[ "$POSTS" = "1" ] || { echo "FAIL: stub saw $POSTS POSTs — the http-request re-executed on replay"; exit 1; }
[ "$AUTH" = "Bearer $V1" ] || { echo "FAIL: stub saw authorization '$AUTH', want the real secret"; exit 1; }

# 3. journal shape: placeholder in the request, hash-only secret rows
JR=$(sqlite3 $DB "SELECT request FROM journal WHERE workflow_id='$WF' AND kind='http-request'")
echo "$JR" | grep -q '{{secret:stub-token}}' \
  || { echo "FAIL: journaled http-request lacks the placeholder — got $JR"; exit 1; }
if echo "$JR" | grep -q "$V1"; then
  echo "FAIL: journaled http-request contains the RAW secret: $JR"; exit 1
fi
NSEC=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF' AND kind='secret'")
[ "$NSEC" = "2" ] || { echo "FAIL: expected 2 secret journal rows, got $NSEC"; exit 1; }
sqlite3 $DB "SELECT response FROM journal WHERE workflow_id='$WF' AND kind='secret'" | grep -q '"sha256"' \
  || { echo "FAIL: secret journal rows are not hash-shaped"; exit 1; }

# 4. rotation fails replay LOUDLY: crash a second workflow in its sleep,
# rotate the secret, recover — the workflow must fail with the message
WF2=$(new_wf)
for i in $(seq 1 10); do ST=$(status $WF2); [ "$ST" = "sleeping" ] && break; sleep 1; done
[ "$ST" = "sleeping" ] || { echo "FAIL: expected wf2 sleeping, got $ST"; exit 1; }
kill -9 $ENG; sleep 1
printf 'stub-token=%s\n' "$V2" > $SECRETS
chmod 600 $SECRETS
./target/release/keel serve --db $DB --secrets-file $SECRETS >> engine.log 2>&1 & ENG=$!
wait_ready
for i in $(seq 1 30); do ST=$(status $WF2); [ "$ST" = "failed" ] && break; sleep 1; done
[ "$ST" = "failed" ] || { echo "FAIL: rotated secret should fail replay, wf2 is $ST"; exit 1; }
OUT2=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF2'")
echo "$OUT2" | grep -q "changed mid-workflow" \
  || { echo "FAIL: rotation failure lacks the loud message — got $OUT2"; exit 1; }
POSTS=$(curl -s localhost:18081/count | python3 -c 'import sys,json;print(json.load(sys.stdin)["posts"])')
[ "$POSTS" = "2" ] || { echo "FAIL: stub saw $POSTS POSTs after rotation recovery, want 2 (no re-POST)"; exit 1; }

# 5. the raw values appear NOWHERE in the database files or the engine log
kill -9 $ENG; ENG=""
if grep -aq -e "$V1" -e "$V2" $DB $DB-wal $DB-shm engine.log 2>/dev/null; then
  echo "FAIL: a raw secret value leaked into the database files or engine.log"; exit 1
fi

# 6. Amendment 4 — the ENV secret-store adapter: same guarantees, no file.
V3="sk-live-ENV-adapter-9999"
DB2=smoke-secrets-env.db; rm -f $DB2 $DB2-shm $DB2-wal
# The secret name 'stub-token' has a hyphen, so the env var is set via `env`
# (bash `export` rejects hyphens; execve/`env` and std::env::var do not).
env "KEEL_SECRET_stub-token=$V3" ./target/release/keel serve --db $DB2 \
  --secrets-env-prefix KEEL_SECRET_ >> engine.log 2>&1 & ENG=$!
wait_ready
HS2=$(curl -s -X POST --data-binary @guests/secrets/target/wasm32-unknown-unknown/release/secrets.wasm \
  "localhost:8080/api/modules?name=secrets" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF3=$(curl -s -X POST localhost:8080/api/workflows -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HS2\",\"input\":{\"base\":\"http://127.0.0.1:18081\"}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
for i in $(seq 1 30); do ST=$(status $WF3); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: env-adapter workflow status=$ST"; exit 1; }
# the wire carried the REAL env value...
AUTH3=$(curl -s localhost:18081/count | python3 -c 'import sys,json;print(json.load(sys.stdin)["auth"])')
[ "$AUTH3" = "Bearer $V3" ] || { echo "FAIL: env-adapter wire auth '$AUTH3', want 'Bearer $V3'"; exit 1; }
# ...the journal redacted it and stored a hash only...
JR3=$(sqlite3 $DB2 "SELECT request FROM journal WHERE workflow_id='$WF3' AND kind='http-request'")
echo "$JR3" | grep -q '{{secret:stub-token}}' \
  || { echo "FAIL: env-adapter journal lacks the placeholder — got $JR3"; exit 1; }
sqlite3 $DB2 "SELECT response FROM journal WHERE workflow_id='$WF3' AND kind='secret'" | grep -q '"sha256"' \
  || { echo "FAIL: env-adapter secret rows are not hash-shaped"; exit 1; }
# ...and the raw env value leaked NOWHERE.
kill -9 $ENG; ENG=""
if grep -aq "$V3" $DB2 $DB2-wal $DB2-shm engine.log 2>/dev/null; then
  echo "FAIL: the env secret value leaked into the database or engine.log"; exit 1
fi
rm -f $DB2 $DB2-shm $DB2-wal
echo "  env adapter: value on the wire, redacted in the journal, hash-only, no leak"

echo "SECRETS SMOKE PASS"
