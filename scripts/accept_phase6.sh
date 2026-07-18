#!/usr/bin/env bash
# Micro-cloud phase 6 acceptance (ext spec Task 6.4): hosted full-stack apps.
# THE FLAGSHIP SLICE: a Rust→WASM (Leptos) frontend served from SQLite, a
# backend function, and a durable workflow — one binary, one file, end to end.
#
# Proves:
#   * a zip of Trunk output uploads into the assets table (stored >= 3);
#   * the app root serves HTML that loads the frontend's .wasm, and the
#     .wasm asset comes back as application/wasm (browsers refuse otherwise);
#   * the vertical slice via the same endpoints the frontend button calls:
#     app api -> backend function -> durable workflow -> polled to completion;
#   * zip-slip entries are rejected and NOTHING from the bundle lands;
#   * the human check is printed, not automated (headless browsers are out
#     of scope) — open the URL, click the button, watch a workflow tick.
#
# Needs trunk (cargo install --locked trunk) + the wasm32-unknown-unknown
# target — CI installs trunk via taiki-e/install-action.
set -euo pipefail
DB=accept6.db; rm -f $DB $DB-shm $DB-wal

ENG=""
MAIN=apps/hello/src/main.rs
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  # Restore the sed-injected MODULE_HASH placeholder — the repo stays clean.
  if [ -f "$MAIN.orig" ]; then mv "$MAIN.orig" "$MAIN"; fi
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

# --- 1. build: engine, backend + workflow guests, then the frontend ---------
cargo build --release -p keel-engine
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/starter-fn && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB --listen 127.0.0.1:8080 > engine.log 2>&1 &
ENG=$!
wait_ready

hash_of() {
  curl -s -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])'
}
H_CNT=$(hash_of guests/counter/target/wasm32-unknown-unknown/release/counter.wasm counter)
H_START=$(hash_of guests/starter-fn/target/wasm32-unknown-unknown/release/starter_fn.wasm starter-fn)

# The frontend bakes the workflow-module hash in (crude, honest, effective —
# ext spec Task 6.2); build AFTER the engine is up so the hash exists first.
cp "$MAIN" "$MAIN.orig"
sed -i.bak "s/MODULE_HASH_PLACEHOLDER/$H_CNT/" "$MAIN" && rm -f "$MAIN.bak"
(cd apps/hello && trunk build --release)
rm -f hello.zip
(cd apps/hello/dist && zip -qr ../../../hello.zip .)
mv "$MAIN.orig" "$MAIN"

# --- 2. create the app + upload the bundle ----------------------------------
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/apps \
  -H 'content-type: application/json' \
  -d "{\"name\":\"hello\",\"backend_hash\":\"$H_START\"}")
[ "$code" = "201" ] || { echo "FAIL: create app -> $code"; exit 1; }
stored=$(curl -s -X POST --data-binary @hello.zip localhost:8080/api/apps/hello/assets \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["stored"])')
[ "$stored" -ge 3 ] || { echo "FAIL: stored $stored assets (want >= 3)"; exit 1; }

# --- 3. the app root serves the frontend ------------------------------------
root=$(curl -s localhost:8080/apps/hello/)
echo "$root" | grep -q "<script" || { echo "FAIL: app root has no <script"; exit 1; }
echo "$root" | grep -q "\.wasm" || { echo "FAIL: app root references no .wasm"; exit 1; }
# Regression guard (found by the human check): asset URLs must be RELATIVE —
# absolute /hello-*.js resolves against the site root under /apps/<name>/ and
# the page renders blank. Trunk.toml pins public_url = "./".
echo "$root" | grep -q 'href="/hello' && { echo "FAIL: root-absolute asset URLs — Trunk public_url must be ./"; exit 1; }
JS_REF=$(echo "$root" | grep -o 'hello-[a-z0-9]*\.js' | head -1)
code=$(curl -s -o /dev/null -w "%{http_code}" "localhost:8080/apps/hello/$JS_REF")
[ "$code" = "200" ] || { echo "FAIL: app JS '$JS_REF' not fetchable under the app path -> $code"; exit 1; }

# --- 4. the .wasm asset: right type, real size ------------------------------
WASM_PATH=$(sqlite3 $DB "SELECT path FROM assets WHERE app='hello' AND path LIKE '%.wasm'")
[ -n "$WASM_PATH" ] || { echo "FAIL: no .wasm asset stored"; exit 1; }
hdr=$(curl -sI "localhost:8080/apps/hello/$WASM_PATH")
echo "$hdr" | grep -q "200" || { echo "FAIL: wasm asset status: $hdr"; exit 1; }
echo "$hdr" | grep -qi "content-type: application/wasm" \
  || { echo "FAIL: wasm content type: $hdr"; exit 1; }
size=$(curl -s "localhost:8080/apps/hello/$WASM_PATH" | wc -c | tr -d ' ')
[ "$size" -gt 100000 ] || { echo "FAIL: wasm asset only $size bytes"; exit 1; }

# --- 5. THE VERTICAL SLICE via the frontend's own endpoints -----------------
WFID=$(curl -s -X POST localhost:8080/apps/hello/api/start \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$H_CNT\",\"input\":{\"target\":3}}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["workflow_id"])')
[ -n "$WFID" ] || { echo "FAIL: no workflow_id from app backend"; exit 1; }
deadline=$((SECONDS + 60))
status_json=""
while [ $SECONDS -lt $deadline ]; do
  status_json=$(curl -s "localhost:8080/apps/hello/api/status?id=$WFID")
  echo "$status_json" | grep -q '"status":"completed"' && break
  sleep 1
done
echo "$status_json" | grep -q '"status":"completed"' \
  || { echo "FAIL: workflow never completed via the app: $status_json"; exit 1; }
echo "$status_json" | grep -q '\\"count\\":3' \
  || { echo "FAIL: app-relayed output missing count:3: $status_json"; exit 1; }

# --- 6. zip-slip ------------------------------------------------------------
python3 - << 'PYEOF'
import zipfile
z = zipfile.ZipFile('evil.zip', 'w')
z.writestr('../evil.txt', 'gotcha')
z.close()
PYEOF
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST --data-binary @evil.zip \
  localhost:8080/api/apps/hello/assets)
[ "$code" = "400" ] || { echo "FAIL: zip-slip upload -> $code (want 400)"; exit 1; }
n=$(sqlite3 $DB "SELECT COUNT(*) FROM assets WHERE path LIKE '%evil%'")
[ "$n" = "0" ] || { echo "FAIL: $n evil assets stored"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal hello.zip evil.zip
echo "HUMAN CHECK (one time, not automated): run the engine, re-upload the"
echo "hello app, open http://127.0.0.1:8080/apps/hello/ and click 'Start job' —"
echo "a durable workflow ticks to completion inside a Rust-compiled-to-WASM"
echo "page served by the same binary."
echo "PHASE 6 PASS"
