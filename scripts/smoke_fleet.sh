#!/usr/bin/env bash
# v2 fleet smoke test: cell tenancy — one keel process/db/token per tenant.
#
# Proves: both tenants come up from one config; tokens do NOT cross cells
# (tenant A's token is a 401 on tenant B); workflows run per-cell and are
# invisible to the other cell; a hard-killed tenant is respawned by the
# supervisor with its state intact; per-tenant providers (v2.2) register on
# their cell only — the other cell gets the guest-visible "no provider" err.
set -euo pipefail
rm -f tenant-a.db* tenant-b.db* keel-tenant-a.log keel-tenant-b.log fleet-smoke.toml fleet.log

FLEET=""
cleanup() {
  if [ -n "${FLEET:-}" ]; then kill -9 "$FLEET" 2>/dev/null || true; fi
  # fleet's children are not in its process group — sweep them by db path
  pkill -f "keel serve --db tenant-a.db" 2>/dev/null || true
  pkill -f "keel serve --db tenant-b.db" 2>/dev/null || true
}
trap cleanup EXIT

for p in 9101 9102; do
  if curl -sf -o /dev/null --max-time 1 localhost:$p/login; then
    echo "FAIL: something is already listening on :$p — kill it first"; exit 1
  fi
done
wait_port() { # $1 = port
  for i in $(seq 1 75); do
    curl -sf -o /dev/null localhost:$1/login && return 0
    sleep 0.2
  done
  echo "FAIL: :$1 did not answer within 15s (see keel-tenant-*.log)"; exit 1
}

cargo build --release -p keel-engine
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/providerdemo && cargo component build --release --target wasm32-unknown-unknown)
(cd providers/greet && cargo component build --release --target wasm32-unknown-unknown)

cat > fleet-smoke.toml <<'EOF'
[[tenants]]
name = "tenant-a"
port = 9101
db = "tenant-a.db"
api_token = "token-a"
providers = ["greet=providers/greet/target/wasm32-unknown-unknown/release/greet.wasm"]

[[tenants]]
name = "tenant-b"
port = 9102
db = "tenant-b.db"
api_token = "token-b"
EOF

./target/release/keel fleet --config fleet-smoke.toml > fleet.log 2>&1 & FLEET=$!
wait_port 9101
wait_port 9102

code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

# tokens are per-cell: A's token opens A and is a 401 on B
[ "$(code -H "Authorization: Bearer token-a" localhost:9101/api/workflows)" = "200" ] \
  || { echo "FAIL: tenant A token rejected by A"; exit 1; }
[ "$(code -H "Authorization: Bearer token-a" localhost:9102/api/workflows)" = "401" ] \
  || { echo "FAIL: tenant A token accepted by B — cells are leaking"; exit 1; }
[ "$(code localhost:9101/api/workflows)" = "401" ] || { echo "FAIL: tokenless not 401"; exit 1; }

# run a workflow in A; B must not see it
H=$(curl -s -H "Authorization: Bearer token-a" -X POST \
  --data-binary @guests/counter/target/wasm32-unknown-unknown/release/counter.wasm \
  "localhost:9101/api/modules?name=counter" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -H "Authorization: Bearer token-a" -X POST localhost:9101/api/workflows \
  -H 'content-type: application/json' -d "{\"module_hash\":\"$H\",\"input\":{\"target\":0}}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
for i in $(seq 1 10); do
  ST=$(curl -s -H "Authorization: Bearer token-a" localhost:9101/api/workflows/$WF \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$ST" = "completed" ] && break; sleep 1
done
[ "$ST" = "completed" ] || { echo "FAIL: tenant A workflow is $ST"; exit 1; }
NB=$(curl -s -H "Authorization: Bearer token-b" localhost:9102/api/workflows \
  | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')
[ "$NB" = "0" ] || { echo "FAIL: tenant B sees $NB workflows — cells are leaking"; exit 1; }

# v2.2: providers are per-cell. The provider workflow succeeds on A (greet is
# registered there) and on B the greet call comes back as the "no provider"
# guest error (providerdemo's `?` turns that into a failed workflow).
run_pd() { # $1=port $2=token -> workflow id
  local h wf
  h=$(curl -s -H "Authorization: Bearer $2" -X POST \
    --data-binary @guests/providerdemo/target/wasm32-unknown-unknown/release/providerdemo.wasm \
    "localhost:$1/api/modules?name=providerdemo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
  curl -s -H "Authorization: Bearer $2" -X POST localhost:$1/api/workflows \
    -H 'content-type: application/json' -d "{\"module_hash\":\"$h\",\"input\":{}}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}
pd_status() { # $1=port $2=token $3=id
  curl -s -H "Authorization: Bearer $2" localhost:$1/api/workflows/$3 \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'
}
WFA=$(run_pd 9101 token-a)
WFB=$(run_pd 9102 token-b)
for i in $(seq 1 20); do
  SA=$(pd_status 9101 token-a $WFA); SB=$(pd_status 9102 token-b $WFB)
  [ "$SA" = "completed" ] && [ "$SB" = "failed" ] && break; sleep 1
done
[ "$SA" = "completed" ] || { echo "FAIL: provider workflow on A is $SA (want completed)"; exit 1; }
[ "$SB" = "failed" ] || { echo "FAIL: provider workflow on B is $SB (want failed — no provider there)"; exit 1; }
OB=$(sqlite3 tenant-b.db "SELECT output FROM workflows WHERE id='$WFB'")
echo "$OB" | grep -q "no provider 'greet'" \
  || { echo "FAIL: B's failure is not the no-provider err — got $OB"; exit 1; }

# supervision: hard-kill tenant A's process; the fleet must respawn it with
# its database intact
APID=$(pgrep -f "keel serve --db tenant-a.db")
kill -9 $APID
UP=""
for i in $(seq 1 20); do
  sleep 1
  if curl -sf -o /dev/null localhost:9101/login; then UP=1; break; fi
done
[ -n "$UP" ] || { echo "FAIL: tenant A not respawned within 20s"; exit 1; }
ST=$(curl -s -H "Authorization: Bearer token-a" localhost:9101/api/workflows/$WF \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
[ "$ST" = "completed" ] || { echo "FAIL: respawned tenant A lost the workflow (got $ST)"; exit 1; }
grep -q "restarting in 1s" fleet.log || { echo "FAIL: fleet.log has no restart line"; exit 1; }

kill $FLEET || true; sleep 1
pkill -f "keel serve --db tenant-a.db" 2>/dev/null || true
pkill -f "keel serve --db tenant-b.db" 2>/dev/null || true
rm -f tenant-a.db* tenant-b.db* keel-tenant-a.log keel-tenant-b.log fleet-smoke.toml fleet.log
echo "FLEET SMOKE PASS"
