#!/usr/bin/env bash
# v1.1 smoke test: bearer/cookie auth + per-guest memory limits.
#
# Auth model under test: with KEEL_API_TOKEN set, every route except /assets/*,
# /login and /logout requires Authorization: Bearer <token> (API) or the login
# cookie (UI); without a token the engine is open (v1.0 behavior). Memory model:
# --max-guest-memory-mb caps guest linear memory; a guest that can't even
# instantiate under the cap fails instead of eating the host.
set -euo pipefail
DB=smoke-auth.db; rm -f $DB $DB-shm $DB-wal
TOK="s3cret-operator-token"

ENG=""
cleanup() { if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi; }
trap cleanup EXIT

if curl -sf -o /dev/null --max-time 1 localhost:8080/login; then
  echo "FAIL: something is already listening on :8080 — kill it first"; exit 1
fi
# NOTE: readiness probes /login — it is reachable without auth by design.
wait_ready() {
  for i in $(seq 1 50); do
    curl -sf -o /dev/null localhost:8080/login && return 0
    sleep 0.2
  done
  echo "FAIL: engine did not answer on :8080 within 10s (see engine.log)"; exit 1
}
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

cargo build --release -p keel-engine
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)

# --- 1. auth enforced when a token is configured (via env, proving env wiring)
KEEL_API_TOKEN=$TOK ./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!
wait_ready

[ "$(code localhost:8080/api/workflows/nope)" = "401" ] || { echo "FAIL: API without token not 401"; exit 1; }
[ "$(code -H "Authorization: Bearer wrong" localhost:8080/api/workflows/nope)" = "401" ] \
  || { echo "FAIL: API with wrong token not 401"; exit 1; }
# UI redirects to the login page instead of a bare 401
[ "$(code localhost:8080/)" = "303" ] || [ "$(code localhost:8080/)" = "302" ] \
  || { echo "FAIL: UI without token did not redirect (got $(code localhost:8080/))"; exit 1; }
[ "$(code localhost:8080/login)" = "200" ] || { echo "FAIL: /login not reachable logged-out"; exit 1; }
[ "$(code localhost:8080/assets/style.css)" = "200" ] || { echo "FAIL: assets must not need auth"; exit 1; }

# correct bearer: full workflow lifecycle works
H=$(curl -s -H "Authorization: Bearer $TOK" -X POST \
  --data-binary @guests/counter/target/wasm32-unknown-unknown/release/counter.wasm \
  "localhost:8080/api/modules?name=counter" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -H "Authorization: Bearer $TOK" -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' -d "{\"module_hash\":\"$H\",\"input\":{\"target\":0}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
status() { curl -s -H "Authorization: Bearer $TOK" localhost:8080/api/workflows/$WF \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }
for i in $(seq 1 10); do ST=$(status); [ "$ST" = "completed" ] && break; sleep 1; done
[ "$ST" = "completed" ] || { echo "FAIL: target=0 counter not completed with bearer, got $ST"; exit 1; }

# cookie login flow: bad token 401, good token sets cookie that opens the UI
JAR=$(mktemp)
[ "$(code -X POST localhost:8080/login -d 'token=wrong')" = "401" ] \
  || { echo "FAIL: bad login not 401"; exit 1; }
curl -s -c "$JAR" -o /dev/null -X POST localhost:8080/login -d "token=$TOK"
grep -q "keel_token" "$JAR" || { echo "FAIL: login did not set keel_token cookie"; exit 1; }
grep -q "$TOK" "$JAR" && { echo "FAIL: cookie contains the RAW token (must be a digest)"; exit 1; }
[ "$(code -b "$JAR" localhost:8080/)" = "200" ] || { echo "FAIL: cookie did not open the dashboard"; exit 1; }
rm -f "$JAR"

kill -9 $ENG; sleep 1

# --- 2. open mode without a token (v1.0 behavior preserved)
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!
wait_ready
[ "$(code localhost:8080/)" = "200" ] || { echo "FAIL: open mode dashboard not 200"; exit 1; }
[ "$(code "localhost:8080/api/workflows/$WF")" = "200" ] || { echo "FAIL: open mode API not 200"; exit 1; }
kill -9 $ENG; sleep 1

# --- 3. memory cap: 1 MiB is below the counter guest's baseline → failed;
# the default cap runs the same module to completion (proven in step 1).
./target/release/keel serve --db $DB --max-guest-memory-mb 1 >> engine.log 2>&1 & ENG=$!
wait_ready
W2=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' -d "{\"module_hash\":\"$H\",\"input\":{\"target\":0}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
for i in $(seq 1 15); do
  ST=$(curl -s localhost:8080/api/workflows/$W2 | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$ST" = "failed" ] && break; sleep 1
done
[ "$ST" = "failed" ] || { echo "FAIL: guest under 1MiB cap ended as $ST, want failed"; exit 1; }

kill $ENG || true
echo "AUTH+LIMITS SMOKE PASS"
