#!/usr/bin/env bash
# Amendment 1 (SPEC-AMENDMENT-1.md §A5) — v3.2 acceptance: the client CLI.
# Drives the platform end to end through `keel` verbs alone (curl only to
# VERIFY results), against a TOKENED engine:
#
#   * auth: no token and a wrong token exit 1; the right token works via
#     BOTH --token and KEEL_API_TOKEN (the same variable the server reads);
#   * keel bind: upload+bind in one step, then a tokenless curl through the
#     public data plane answers;
#   * keel run: counter workflow watched to completion — right output on
#     stdout, exit code 0;
#   * keel deploy: a fabricated static+backend app dir (with a .DS_Store that
#     must NOT ship) → exactly the real files stored, served with correct
#     content types, backend roundtrip live;
#   * keel logs: returns the lines the bound function logged, for both
#     inferred kinds (/fn/... → function, bare name → app).
set -euo pipefail
DB=accept-cli.db; rm -f $DB $DB-shm $DB-wal
TOKEN=cli-gate-secret
KEEL=./target/release/keel

ENG=""
cleanup() {
  if [ -n "${ENG:-}" ]; then kill -9 "$ENG" 2>/dev/null || true; fi
  rm -rf cli-dist
}
trap cleanup EXIT

if curl -sf -o /dev/null --max-time 1 localhost:8080/; then
  echo "FAIL: something is already listening on :8080 — kill it first"; exit 1
fi
wait_ready() {
  for i in $(seq 1 50); do
    curl -s -o /dev/null localhost:8080/ && return 0
    sleep 0.2
  done
  echo "FAIL: engine did not answer on :8080 within 10s (see engine.log)"; exit 1
}

# --- build ------------------------------------------------------------------
cargo build --release -p keel-engine
(cd guests/echo-fn && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)
ECHO_WASM=guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm
CNT_WASM=guests/counter/target/wasm32-unknown-unknown/release/counter.wasm

KEEL_API_TOKEN=$TOKEN $KEEL serve --db $DB --listen 127.0.0.1:8080 > engine.log 2>&1 &
ENG=$!
wait_ready

# --- 1. auth: wrong/no token exit 1; right token works both ways -------------
if env -u KEEL_API_TOKEN $KEEL bind /fn/cli $ECHO_WASM > /dev/null 2>&1; then
  echo "FAIL: tokenless bind against a tokened server succeeded"; exit 1
fi
if env -u KEEL_API_TOKEN $KEEL bind /fn/cli $ECHO_WASM --token wrong > /dev/null 2>&1; then
  echo "FAIL: wrong-token bind succeeded"; exit 1
fi

# --- 2. keel bind (via --token), then the public plane answers ---------------
out=$(env -u KEEL_API_TOKEN $KEEL bind /fn/cli $ECHO_WASM --rate 100 --token $TOKEN)
echo "$out" | grep -q "bound /fn/cli" || { echo "FAIL: bind output: $out"; exit 1; }
resp=$(curl -s -X POST localhost:8080/fn/cli --data-binary 'from-the-cli')
echo "$resp" | grep -q '"body_len":12' || { echo "FAIL: /fn/cli roundtrip: $resp"; exit 1; }

# --- 3. keel run: watch a durable workflow to completion (env token) ---------
out=$(KEEL_API_TOKEN=$TOKEN $KEEL run $CNT_WASM --input '{"target":2}' 2>run.log) \
  || { echo "FAIL: keel run exited nonzero"; cat run.log; exit 1; }
echo "$out" | grep -q '"count":2' || { echo "FAIL: run output: $out"; exit 1; }
grep -q "completed" run.log || { echo "FAIL: run progress never showed completed"; cat run.log; exit 1; }
rm -f run.log

# --- 4. keel deploy: fabricated dir, dot-files skipped, served correctly -----
rm -rf cli-dist && mkdir -p cli-dist/sub
printf '<!doctype html><title>cli app</title>hello from the cli' > cli-dist/index.html
printf 'console.log("cli")' > cli-dist/app.js
printf 'body{}' > cli-dist/sub/nested.css
printf 'junk' > cli-dist/.DS_Store
out=$(KEEL_API_TOKEN=$TOKEN $KEEL deploy cli-dist --name cliapp --backend $ECHO_WASM --rate 50)
echo "$out" | grep -q "deployed 'cliapp': 3 assets" || { echo "FAIL: deploy output: $out"; exit 1; }
root=$(curl -s localhost:8080/apps/cliapp/)
echo "$root" | grep -q "hello from the cli" || { echo "FAIL: app root: $root"; exit 1; }
ct=$(curl -sI localhost:8080/apps/cliapp/sub/nested.css | grep -i content-type)
echo "$ct" | grep -qi "text/css" || { echo "FAIL: nested asset content type: $ct"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" localhost:8080/apps/cliapp/.DS_Store)
[ "$code" = "404" ] || { echo "FAIL: .DS_Store was deployed ($code)"; exit 1; }
resp=$(curl -s -X POST localhost:8080/apps/cliapp/api/z --data-binary 'app-line')
echo "$resp" | grep -q '"body_len":8' || { echo "FAIL: app backend roundtrip: $resp"; exit 1; }

# --- 5. keel logs: both inferred kinds return the guests' lines --------------
out=$(KEEL_API_TOKEN=$TOKEN $KEEL logs /fn/cli)
echo "$out" | grep -q "echo: from-the-cli" || { echo "FAIL: function logs: $out"; exit 1; }
out=$(KEEL_API_TOKEN=$TOKEN $KEEL logs cliapp)
echo "$out" | grep -q "echo: app-line" || { echo "FAIL: app logs: $out"; exit 1; }

# --- 6. re-deploy is an upsert end to end ------------------------------------
printf '<!doctype html><title>cli app</title>hello again' > cli-dist/index.html
out=$(KEEL_API_TOKEN=$TOKEN $KEEL deploy cli-dist --name cliapp --backend $ECHO_WASM)
echo "$out" | grep -q "3 assets" || { echo "FAIL: re-deploy output: $out"; exit 1; }
curl -s localhost:8080/apps/cliapp/ | grep -q "hello again" \
  || { echo "FAIL: re-deploy did not replace index.html"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal
rm -rf cli-dist
echo "CLI PASS"
