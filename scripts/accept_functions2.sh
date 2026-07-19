#!/usr/bin/env bash
# v3.5 acceptance (SPEC-AMENDMENT-2.md A9) — functions grow up: per-ref
# config (config-get) + durable per-ref KV (kv-get/set/delete), WIT 0.8.0.
#
# Proves, on a TOKENED engine, driven through guests/kvcfg-fn:
#   * config set via the API is visible to the guest at ITS ref only; the
#     listing returns NAMES and never the value; door checks 400 (bad name,
#     oversize value);
#   * the kv counter counts 1,2,3 → engine kill -9 + restart → 4 (A7's
#     durability contract: a returned kv-set survives anything);
#   * the SAME module at a second prefix counts independently, and an app
#     backend counts independently under kind 'app' (ref-scoped, not
#     module-scoped);
#   * kv-delete resets; the over-cap write errs with the cap named;
#   * GET /api/kv lists keys only; DELETE /api/kv wipes the ref's store;
#   * config survives the restart too (it is rows, like everything here).
set -euo pipefail
DB=accept-fn2.db; rm -f $DB $DB-shm $DB-wal
export KEEL_API_TOKEN=fn2-secret

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
AUTH=(-H "authorization: Bearer $KEEL_API_TOKEN")

# --- build ------------------------------------------------------------------
cargo build --release -p keel-engine
(cd guests/kvcfg-fn && cargo component build --release --target wasm32-unknown-unknown)

start_engine() {
  ./target/release/keel serve --db $DB --listen 127.0.0.1:8080 >> engine.log 2>&1 &
  ENG=$!
  wait_ready
}
start_engine

H=$(curl -s "${AUTH[@]}" -X POST --data-binary @guests/kvcfg-fn/target/wasm32-unknown-unknown/release/kvcfg_fn.wasm \
  "localhost:8080/api/modules?name=kvcfg-fn" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])')
for p in /fn/kc /fn/kc2; do
  code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/routes \
    -H 'content-type: application/json' -d "{\"prefix\":\"$p\",\"module_hash\":\"$H\"}")
  [ "$code" = "201" ] || { echo "FAIL: bind $p -> $code"; exit 1; }
done
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/apps \
  -H 'content-type: application/json' -d "{\"name\":\"ka\",\"backend_hash\":\"$H\"}")
[ "$code" = "201" ] || { echo "FAIL: create app ka -> $code"; exit 1; }

# --- 1. A6: config — ref-scoped, names-only listing, door checks -------------
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/config \
  -H 'content-type: application/json' \
  -d '{"kind":"function","ref":"/fn/kc","name":"API_KEY","value":"s3cret-value-42"}')
[ "$code" = "201" ] || { echo "FAIL: set config -> $code"; exit 1; }
v=$(curl -s localhost:8080/fn/kc/cfg)
[ "$v" = "s3cret-value-42" ] || { echo "FAIL: guest config-get -> '$v'"; exit 1; }
v=$(curl -s localhost:8080/fn/kc2/cfg)
[ "$v" = "none" ] || { echo "FAIL: /fn/kc2 must NOT see /fn/kc's config: '$v'"; exit 1; }
listing=$(curl -s "${AUTH[@]}" "localhost:8080/api/config?kind=function&ref=%2Ffn%2Fkc")
echo "$listing" | grep -q '"API_KEY"' || { echo "FAIL: listing lacks the name: $listing"; exit 1; }
case "$listing" in *s3cret-value-42*) echo "FAIL: listing leaked the VALUE: $listing"; exit 1;; esac
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/config \
  -H 'content-type: application/json' \
  -d '{"kind":"function","ref":"/fn/kc","name":"bad name!","value":"x"}')
[ "$code" = "400" ] || { echo "FAIL: bad config name -> $code (want 400)"; exit 1; }
big=$(python3 -c 'print("v"*5000)')
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X POST localhost:8080/api/config \
  -H 'content-type: application/json' \
  -d "{\"kind\":\"function\",\"ref\":\"/fn/kc\",\"name\":\"BIG\",\"value\":\"$big\"}")
[ "$code" = "400" ] || { echo "FAIL: oversize config value -> $code (want 400)"; exit 1; }

# --- 2. A7: the counter counts, then SURVIVES kill -9 ------------------------
for want in 1 2 3; do
  got=$(curl -s -X POST localhost:8080/fn/kc/count)
  [ "$got" = "$want" ] || { echo "FAIL: count -> '$got' (want $want)"; exit 1; }
done
kill -9 "$ENG" 2>/dev/null || true; ENG=""
start_engine
got=$(curl -s -X POST localhost:8080/fn/kc/count)
[ "$got" = "4" ] || { echo "FAIL: count after kill -9 + restart -> '$got' (want 4 — A7 durability)"; exit 1; }
v=$(curl -s localhost:8080/fn/kc/cfg)
[ "$v" = "s3cret-value-42" ] || { echo "FAIL: config after restart -> '$v'"; exit 1; }

# --- 3. ref-scoped, not module-scoped ---------------------------------------
got=$(curl -s -X POST localhost:8080/fn/kc2/count)
[ "$got" = "1" ] || { echo "FAIL: /fn/kc2 count -> '$got' (want 1 — stores are per REF)"; exit 1; }
got=$(curl -s -X POST localhost:8080/apps/ka/api/count)
[ "$got" = "1" ] || { echo "FAIL: app backend count -> '$got' (want 1 — kind 'app' is its own store)"; exit 1; }

# --- 4. kv-delete + the value cap -------------------------------------------
got=$(curl -s -X POST localhost:8080/fn/kc/reset)
[ "$got" = "0" ] || { echo "FAIL: reset -> '$got'"; exit 1; }
got=$(curl -s -X POST localhost:8080/fn/kc/count)
[ "$got" = "1" ] || { echo "FAIL: count after reset -> '$got' (want 1)"; exit 1; }
caperr=$(curl -s -X POST localhost:8080/fn/kc/big)
echo "$caperr" | grep -q "65536" || { echo "FAIL: over-cap kv-set must name the cap: '$caperr'"; exit 1; }

# --- 5. the kv control plane: keys-only listing + wipe -----------------------
keys=$(curl -s "${AUTH[@]}" "localhost:8080/api/kv?kind=function&ref=%2Ffn%2Fkc")
echo "$keys" | grep -q '"count"' || { echo "FAIL: /api/kv lacks 'count': $keys"; exit 1; }
code=$(curl -s -o /dev/null -w "%{http_code}" "${AUTH[@]}" -X DELETE \
  "localhost:8080/api/kv?kind=function&ref=%2Ffn%2Fkc")
[ "$code" = "204" ] || { echo "FAIL: kv wipe -> $code"; exit 1; }
got=$(curl -s -X POST localhost:8080/fn/kc/count)
[ "$got" = "1" ] || { echo "FAIL: count after wipe -> '$got' (want 1)"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal
echo "FUNCTIONS2 PASS"
