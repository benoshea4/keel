#!/usr/bin/env bash
# v2.6 smoke test: the content-addressed provider registry, end to end.
#
# Proves:
#   * providers upload via the API (per-tier pre-flight at the door: junk and
#     tier-violating components are 400s, never workflow failures);
#   * the swap is LIVE: re-uploading a name changes the NEXT call without a
#     restart, while a killed workflow's REPLAY still returns its recorded
#     (old-version) response — the journal wins, the registry doesn't;
#   * registry providers PERSIST across restarts (engine restarted flagless);
#   * rebind by hash = rollback without re-shipping bytes;
#   * DELETE unbinds: later calls err as unregistered (data, workflow fails
#     with the guest's error, engine unharmed);
#   * boot flags upsert into the same registry;
#   * effectful uploads serve effectful calls (journaled internals present);
#   * the /providers UI page lists the registry.
set -euo pipefail
DB=smoke-reg.db; rm -f $DB $DB-shm $DB-wal

ENG=""; STUB=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  if [ -n "${STUB:-}" ]; then kill -9 "$STUB" 2>/dev/null || true; fi
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
(cd providers/greet && cargo component build --release --target wasm32-unknown-unknown)
GREET_V1=greet-v1.wasm
cp providers/greet/target/wasm32-unknown-unknown/release/greet.wasm $GREET_V1
(cd providers/greet && cargo component build --release --target wasm32-unknown-unknown --features v2)
GREET_V2=providers/greet/target/wasm32-unknown-unknown/release/greet.wasm
(cd providers/relay && cargo component build --release --target wasm32-unknown-unknown)
RELAY=providers/relay/target/wasm32-unknown-unknown/release/relay.wasm
(cd guests/providerdemo && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/relay && cargo component build --release --target wasm32-unknown-unknown)

python3 scripts/stub_server.py 18081 & STUB=$!
sleep 0.5
./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!   # NO provider flags
wait_ready

upload_mod() { curl -s -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])'; }
start_wf() { curl -s -X POST localhost:8080/api/workflows -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$1\",\"input\":$2}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'; }
status() { curl -s localhost:8080/api/workflows/$1 \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }
outp() { sqlite3 $DB "SELECT output FROM workflows WHERE id='$1'"; }
wait_status() { # id want tries
  for i in $(seq 1 $3); do ST=$(status $1); [ "$ST" = "$2" ] && return 0; sleep 1; done
  echo "FAIL: workflow $1 status=$ST, wanted $2"; exit 1
}

# 1. upload greet v1 (pure) through the API; tier is required and validated
H1=$(curl -s -X POST --data-binary @$GREET_V1 "localhost:8080/api/providers?name=greet&tier=pure" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
[ -n "$H1" ] || { echo "FAIL: no hash from provider upload"; exit 1; }
RC=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary @$GREET_V1 "localhost:8080/api/providers?name=greet")
[ "$RC" = "400" ] || { echo "FAIL: missing tier accepted ($RC)"; exit 1; }
RC=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary "junk" "localhost:8080/api/providers?name=j&tier=pure")
[ "$RC" = "400" ] || { echo "FAIL: junk accepted ($RC)"; exit 1; }
RC=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary @$RELAY "localhost:8080/api/providers?name=sneaky&tier=pure")
[ "$RC" = "400" ] || { echo "FAIL: effectful component accepted under tier=pure ($RC)"; exit 1; }

# 2. live roll + replay-vs-registry: start providerdemo (greet call 1 journaled
# with v1), re-upload v2 while it sleeps, kill -9, restart FLAGLESS.
HP=$(upload_mod guests/providerdemo/target/wasm32-unknown-unknown/release/providerdemo.wasm providerdemo)
WFA=$(start_wf $HP '{}')
wait_status $WFA sleeping 15
H2=$(curl -s -X POST --data-binary @$GREET_V2 "localhost:8080/api/providers?name=greet&tier=pure" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
[ "$H2" != "$H1" ] || { echo "FAIL: v2 hashed identical to v1"; exit 1; }
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!   # still NO flags
wait_ready
wait_status $WFA completed 30
outp $WFA | grep -q 'hello keel' \
  || { echo "FAIL: replay did not return the RECORDED v1 greeting — $(outp $WFA)"; exit 1; }
outp $WFA | grep -q 'hello v2' && { echo "FAIL: replay used the re-bound v2 provider"; exit 1; }
WFB=$(start_wf $HP '{}')
wait_status $WFB completed 30
outp $WFB | grep -q 'hello v2 keel' \
  || { echo "FAIL: new workflow did not get the rolled v2 provider — $(outp $WFB)"; exit 1; }

# 3. rebind by hash (rollback, no bytes)
RC=$(curl -s -o /dev/null -w "%{http_code}" -X POST "localhost:8080/api/providers?name=greet&tier=pure&hash=$H1")
[ "$RC" = "200" ] || { echo "FAIL: rebind to $H1 returned $RC"; exit 1; }
RC=$(curl -s -o /dev/null -w "%{http_code}" -X POST "localhost:8080/api/providers?name=greet&tier=pure&hash=deadbeef")
[ "$RC" = "404" ] || { echo "FAIL: rebind to unknown hash returned $RC"; exit 1; }
WFC=$(start_wf $HP '{}')
wait_status $WFC completed 30
outp $WFC | grep -q 'hello keel' \
  || { echo "FAIL: rollback rebind did not restore v1 — $(outp $WFC)"; exit 1; }

# 4. effectful via the registry
curl -s -X POST --data-binary @$RELAY "localhost:8080/api/providers?name=relay&tier=effectful" > /dev/null
HR=$(upload_mod guests/relay/target/wasm32-unknown-unknown/release/relay_guest.wasm relay-guest)
WFR=$(start_wf $HR '{"first_url":"http://127.0.0.1:18081/hook/r1","second_url":"http://127.0.0.1:18081/hook/r2"}')
wait_status $WFR completed 30
IN=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WFR' AND kind='provider-http:relay'")
[ "$IN" = "2" ] || { echo "FAIL: expected 2 relay internals via registry, got $IN"; exit 1; }

# 5. list + UI page
curl -s localhost:8080/api/providers | grep -q '"name":"greet"' \
  || { echo "FAIL: greet missing from GET /api/providers"; exit 1; }
curl -s localhost:8080/providers | grep -q 'greet' \
  || { echo "FAIL: /providers UI page does not list greet"; exit 1; }

# 6. DELETE unbinds: later calls err as data (workflow fails, engine fine)
RC=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE localhost:8080/api/providers/greet)
[ "$RC" = "200" ] || { echo "FAIL: delete returned $RC"; exit 1; }
WFD=$(start_wf $HP '{}')
wait_status $WFD failed 30
outp $WFD | grep -q "no provider 'greet'" \
  || { echo "FAIL: post-delete workflow error wrong — $(outp $WFD)"; exit 1; }

# 7. boot flags upsert into the registry (and coexist with API entries)
kill -9 $ENG; sleep 1
./target/release/keel serve --db $DB --provider greet=$GREET_V1 >> engine.log 2>&1 & ENG=$!
wait_ready
GOT=$(curl -s localhost:8080/api/providers \
  | python3 -c 'import sys,json;ps={p["name"]:p["tier"]+":"+p["hash"] for p in json.load(sys.stdin)};print(ps.get("greet",""))')
[ "$GOT" = "pure:$H1" ] || { echo "FAIL: flag boot did not upsert greet@v1 (got '$GOT')"; exit 1; }
WFE=$(start_wf $HP '{}')
wait_status $WFE completed 30
outp $WFE | grep -q 'hello keel' || { echo "FAIL: flag-registered greet broken — $(outp $WFE)"; exit 1; }

kill $ENG || true; ENG=""
rm -f $GREET_V1
echo "PROVIDER REGISTRY SMOKE PASS"
