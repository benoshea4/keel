#!/usr/bin/env bash
# v3.4 acceptance (status.md §R, R.1–R.5) — polish: conditional GETs, API/CLI
# symmetry, favicon, latency percentiles.
#
# Proves (on a TOKENED engine — the favicon allowlist claim needs one):
#   * R.3: /favicon.ico answers 200 image/x-icon TOKENLESS while the control
#     plane still 401s tokenless;
#   * R.1: a hash-named asset serves with an ETag and an immutable year-long
#     Cache-Control; If-None-Match returns 304 with an empty body; a plain
#     asset revalidates (no-cache + ETag); index.html stays no-store with no
#     ETag; re-uploading changed bytes moves the ETag and defeats the old one;
#   * R.2: GET /api/apps lists, DELETE /api/apps/{name} cascades assets (404
#     on a second delete), `keel ls` shows routes+apps+schedules, `keel
#     unbind` removes a binding, `keel apps rm` removes an app, and `keel run
#     --timeout` exits 2 on a parked workflow (which keeps running);
#   * R.5: keel_fn_duration_ms{...,quantile="0.5"|"0.99"} appears in /metrics
#     after traffic, and /partials/usage renders the Latency-by-ref table.
#   (R.4 is unit-level: the admit() tests in core/src/function.rs.)
set -euo pipefail
DB=accept-pol.db; rm -f $DB $DB-shm $DB-wal pol.zip pol2.zip
export KEEL_API_TOKEN=polish-secret

ENG=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
}
trap cleanup EXIT

if curl -sf -o /dev/null --max-time 1 localhost:8080/favicon.ico; then
  echo "FAIL: something is already listening on :8080 — kill it first"; exit 1
fi
wait_ready() {
  for i in $(seq 1 50); do
    curl -sf -o /dev/null localhost:8080/favicon.ico && return 0
    sleep 0.2
  done
  echo "FAIL: engine did not answer on :8080 within 10s (see engine.log)"; exit 1
}
SQL() { sqlite3 -cmd ".timeout 5000" $DB "$1"; }
AUTH=(-H "authorization: Bearer $KEEL_API_TOKEN")

# --- build ------------------------------------------------------------------
cargo build --release -p keel-engine
(cd guests/echo-fn && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/approval && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB --listen 127.0.0.1:8080 > engine.log 2>&1 &
ENG=$!
wait_ready

# --- 1. R.3: favicon tokenless, control plane still guarded ------------------
resp=$(curl -s -D - -o /dev/null localhost:8080/favicon.ico)
echo "$resp" | head -1 | grep -q " 200" || { echo "FAIL: favicon -> $resp"; exit 1; }
echo "$resp" | grep -qi "^content-type: image/x-icon" \
  || { echo "FAIL: favicon content-type: $resp"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" localhost:8080/api/routes)
[ "$code" = "401" ] || { echo "FAIL: tokenless control plane -> $code (want 401)"; exit 1; }

# --- fixtures: a route with traffic + an app with three asset classes --------
H_ECHO=$(curl -s "${AUTH[@]}" -X POST --data-binary @guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm \
  "localhost:8080/api/modules?name=echo-fn" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])')
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/routes \
  -H 'content-type: application/json' \
  -d "{\"prefix\":\"/fn/e\",\"module_hash\":\"$H_ECHO\"}")
[ "$code" = "201" ] || { echo "FAIL: bind /fn/e -> $code"; exit 1; }
for i in $(seq 1 5); do
  curl -s -o /dev/null -X POST localhost:8080/fn/e -d "ping$i"
done

code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/apps \
  -H 'content-type: application/json' -d '{"name":"p"}')
[ "$code" = "201" ] || { echo "FAIL: create app p -> $code"; exit 1; }
python3 - <<'PY'
import zipfile
z = zipfile.ZipFile('pol.zip', 'w')
z.writestr('index.html', '<h1>polish</h1>')
z.writestr('app-0123456789abcdef.js', 'console.log(1)')   # trunk-style hash name
z.writestr('plain.css', 'body{}')
z.close()
z = zipfile.ZipFile('pol2.zip', 'w')                      # the re-deploy: same names, new bytes
z.writestr('index.html', '<h1>polish v2</h1>')
z.writestr('app-0123456789abcdef.js', 'console.log(2)')
z.writestr('plain.css', 'body{margin:0}')
z.close()
PY
stored=$(curl -s "${AUTH[@]}" -X POST --data-binary @pol.zip localhost:8080/api/apps/p/assets \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["stored"])')
[ "$stored" = "3" ] || { echo "FAIL: stored $stored assets (want 3)"; exit 1; }

# --- 2. R.1: the conditional-GET story --------------------------------------
hdr=$(curl -s -D - -o /dev/null localhost:8080/apps/p/app-0123456789abcdef.js)
echo "$hdr" | grep -qi '^etag: "' || { echo "FAIL: hash-named asset has no ETag: $hdr"; exit 1; }
echo "$hdr" | grep -qi "^cache-control: public, max-age=31536000, immutable" \
  || { echo "FAIL: hash-named cache-control: $hdr"; exit 1; }
ETAG=$(echo "$hdr" | grep -i '^etag:' | tr -d '\r' | awk '{print $2}')
code=$(curl -s -o body.out -w "%{http_code}" -H "if-none-match: $ETAG" \
  localhost:8080/apps/p/app-0123456789abcdef.js)
[ "$code" = "304" ] || { echo "FAIL: conditional GET -> $code (want 304)"; exit 1; }
[ ! -s body.out ] || { echo "FAIL: 304 carried a body"; exit 1; }
rm -f body.out
hdr=$(curl -s -D - -o /dev/null localhost:8080/apps/p/plain.css)
echo "$hdr" | grep -qi "^cache-control: no-cache" \
  || { echo "FAIL: plain asset cache-control: $hdr"; exit 1; }
echo "$hdr" | grep -qi '^etag: "' || { echo "FAIL: plain asset has no ETag: $hdr"; exit 1; }
hdr=$(curl -s -D - -o /dev/null localhost:8080/apps/p/)
echo "$hdr" | grep -qi "^cache-control: no-store" \
  || { echo "FAIL: index.html cache-control: $hdr"; exit 1; }
echo "$hdr" | grep -qi '^etag:' && { echo "FAIL: index.html must carry no ETag: $hdr"; exit 1; }
# Re-deploy with changed bytes: the OLD etag must stop matching.
curl -s -o /dev/null "${AUTH[@]}" -X POST --data-binary @pol2.zip localhost:8080/api/apps/p/assets
resp=$(curl -s -D - -H "if-none-match: $ETAG" localhost:8080/apps/p/app-0123456789abcdef.js)
echo "$resp" | head -1 | grep -q " 200" \
  || { echo "FAIL: stale etag must fetch fresh bytes: $resp"; exit 1; }
echo "$resp" | grep -qi '^etag: "' && NEW=$(echo "$resp" | grep -i '^etag:' | tr -d '\r' | awk '{print $2}')
[ "$NEW" != "$ETAG" ] || { echo "FAIL: etag did not move on re-upload"; exit 1; }
grep -q "polish v2" < <(curl -s localhost:8080/apps/p/) \
  || { echo "FAIL: re-deployed index.html not served"; exit 1; }

# --- 3. R.2: list/delete symmetry, CLI verbs --------------------------------
napps=$(curl -s "${AUTH[@]}" localhost:8080/api/apps \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(len([a for a in d if a["name"]=="p" and a["assets"]==3]))')
[ "$napps" = "1" ] || { echo "FAIL: GET /api/apps missing app p with 3 assets"; exit 1; }
LS=$(./target/release/keel ls)
echo "$LS" | grep -q "/fn/e" || { echo "FAIL: keel ls missing route: $LS"; exit 1; }
echo "$LS" | grep -q "p .*3 assets\|p  *3 assets" || echo "$LS" | grep -q "3 assets" \
  || { echo "FAIL: keel ls missing app p: $LS"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X DELETE localhost:8080/api/apps/missing)
[ "$code" = "404" ] || { echo "FAIL: DELETE missing app -> $code"; exit 1; }

rc=0; ./target/release/keel run \
  guests/approval/target/wasm32-unknown-unknown/release/approval.wasm \
  --timeout 2 >/dev/null 2>run.err || rc=$?
[ "$rc" = "2" ] || { echo "FAIL: run --timeout exit $rc (want 2): $(cat run.err)"; exit 1; }
grep -q "timeout: workflow" run.err || { echo "FAIL: run.err: $(cat run.err)"; exit 1; }
WFID=$(grep -o "workflow [0-9a-f-]*" run.err | head -1 | awk '{print $2}')
status=$(curl -s "${AUTH[@]}" localhost:8080/api/workflows/$WFID \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')
case "$status" in running|waiting_event|sleeping) ;; *)
  echo "FAIL: workflow after CLI timeout is '$status' (must keep running)"; exit 1;; esac
rm -f run.err

./target/release/keel unbind /fn/e | grep -q "unbound /fn/e" || { echo "FAIL: unbind output"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/fn/e -d x)
[ "$code" = "404" ] || { echo "FAIL: /fn/e after unbind -> $code (want 404)"; exit 1; }
./target/release/keel apps rm p | grep -q "removed app 'p'" || { echo "FAIL: apps rm output"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" localhost:8080/apps/p/)
[ "$code" = "404" ] || { echo "FAIL: /apps/p/ after rm -> $code"; exit 1; }
n=$(SQL "SELECT COUNT(*) FROM assets WHERE app='p'")
[ "$n" = "0" ] || { echo "FAIL: $n orphaned assets after apps rm"; exit 1; }
rc=0; ./target/release/keel apps rm p >/dev/null 2>&1 || rc=$?
[ "$rc" = "1" ] || { echo "FAIL: second apps rm exit $rc (want 1)"; exit 1; }

# --- 4. R.5: percentiles in /metrics and on /usage --------------------------
metrics=$(curl -s "${AUTH[@]}" localhost:8080/metrics)
echo "$metrics" | grep -q 'keel_fn_duration_ms{kind="function",ref="/fn/e",quantile="0.5"}' \
  || { echo "FAIL: no p50 metric for /fn/e"; exit 1; }
echo "$metrics" | grep -q 'keel_fn_duration_ms{kind="function",ref="/fn/e",quantile="0.99"}' \
  || { echo "FAIL: no p99 metric for /fn/e"; exit 1; }
usage=$(curl -s "${AUTH[@]}" localhost:8080/partials/usage)
echo "$usage" | grep -q "Latency by ref" || { echo "FAIL: usage partial lacks latency table"; exit 1; }
echo "$usage" | grep -q "/fn/e" || { echo "FAIL: usage latency table lacks /fn/e"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal pol.zip pol2.zip
echo "POLISH PASS"
