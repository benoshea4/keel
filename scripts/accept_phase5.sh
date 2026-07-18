#!/usr/bin/env bash
# Micro-cloud phase 5 acceptance (ext spec Task 5.6): sandbox limits,
# metering, and the playground judge.
#
# Proves:
#   * the judge delivers the whole verdict alphabet — AC (with per-case
#     detail + fuel), WA, TLE (resolves in seconds, never hangs), MLE — and
#     every case writes a `solve` ledger row;
#   * function-side quotas bite: a fuel-starved route answers 500/oof and the
#     ledger says so;
#   * the WORKFLOW runaway watchdog: a spinning workflow guest under a
#     starvation --wf-fuel-limit fails with the §E1 message while the engine
#     stays healthy.
set -euo pipefail
DB=accept5.db; rm -f $DB $DB-shm $DB-wal

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

# --- build ------------------------------------------------------------------
cargo build --release -p keel-engine
(cd guests/echo-fn && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/spin && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/loop-solver && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/hog-solver && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/sum-solver && cargo component build --release --target wasm32-unknown-unknown --features wrong)
SUM_WRONG=sum-solver-wrong.wasm
cp guests/sum-solver/target/wasm32-unknown-unknown/release/sum_solver.wasm $SUM_WRONG
(cd guests/sum-solver && cargo component build --release --target wasm32-unknown-unknown)

# A starvation workflow-fuel budget, to prove the watchdog (10^7 is far below
# what spin needs, far above what the engine's own paths need).
./target/release/keel serve --db $DB --listen 127.0.0.1:8080 --wf-fuel-limit 10000000 > engine.log 2>&1 &
ENG=$!
wait_ready

hash_of() {
  curl -s -X POST --data-binary @"$1" "localhost:8080/api/modules?name=$2" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["hash"])'
}

# --- 1. seed the problem -----------------------------------------------------
out=$(curl -s -X POST localhost:8080/api/problems -H 'content-type: application/json' \
  -d '{"slug":"sum","title":"Sum the ints","statement":"Line 1: N. Line 2: N space-separated ints. Print their sum.","cases":[{"input":"2\n1 2","expected":"3"},{"input":"3\n10 20 30","expected":"60"}]}')
echo "$out" | grep -q '"cases":2' || { echo "FAIL: seeding problem: $out"; exit 1; }

# --- 2. upload solvers -------------------------------------------------------
H_SUM=$(hash_of guests/sum-solver/target/wasm32-unknown-unknown/release/sum_solver.wasm sum-solver)
H_WRONG=$(hash_of $SUM_WRONG sum-solver-wrong)
H_LOOP=$(hash_of guests/loop-solver/target/wasm32-unknown-unknown/release/loop_solver.wasm loop-solver)
H_HOG=$(hash_of guests/hog-solver/target/wasm32-unknown-unknown/release/hog_solver.wasm hog-solver)
H_ECHO=$(hash_of guests/echo-fn/target/wasm32-unknown-unknown/release/echo_fn.wasm echo-fn)
H_SPIN=$(hash_of guests/spin/target/wasm32-unknown-unknown/release/spin.wasm spin)

# submit <hash> -> echoes verdict; polls to resolution (30s cap = the "TLE
# must resolve, not hang" assertion for every submission).
submit() {
  local sid v
  sid=$(curl -s -X POST localhost:8080/api/submissions -H 'content-type: application/json' \
    -d "{\"problem\":\"sum\",\"module_hash\":\"$1\"}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
  [ -n "$sid" ] || { echo "FAIL: no submission id for $1" >&2; exit 1; }
  for i in $(seq 1 30); do
    v=$(curl -s localhost:8080/api/submissions/$sid \
      | python3 -c 'import json,sys; v=json.load(sys.stdin)["verdict"]; print(v or "")')
    [ -n "$v" ] && break
    sleep 1
  done
  [ -n "$v" ] || { echo "FAIL: submission $sid never resolved in 30s" >&2; exit 1; }
  echo "$sid $v"
}

# --- 3. AC with detail + ledger ---------------------------------------------
read SID_AC V_AC <<< "$(submit $H_SUM)"
[ "$V_AC" = "AC" ] || { echo "FAIL: sum-solver verdict $V_AC (want AC)"; exit 1; }
detail=$(curl -s localhost:8080/api/submissions/$SID_AC \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["detail"])')
echo "$detail" | python3 -c '
import json, sys
d = json.loads(sys.stdin.read())
assert len(d) == 2, f"want 2 case entries, got {len(d)}"
assert all(c["verdict"] == "AC" for c in d), d
assert all(c["fuel"] > 0 for c in d), d
print("detail ok")' | grep -q "detail ok" || { echo "FAIL: AC detail: $detail"; exit 1; }
n=$(sqlite3 $DB "SELECT COUNT(*) FROM invocations WHERE kind='solve' AND ref='$SID_AC'")
[ "$n" = "2" ] || { echo "FAIL: want 2 solve ledger rows for AC run, got $n"; exit 1; }

# --- 4. WA / TLE / MLE -------------------------------------------------------
read _ V <<< "$(submit $H_WRONG)"
[ "$V" = "WA" ] || { echo "FAIL: sum-solver-wrong verdict $V (want WA)"; exit 1; }
read _ V <<< "$(submit $H_LOOP)"
[ "$V" = "TLE" ] || { echo "FAIL: loop-solver verdict $V (want TLE)"; exit 1; }
read _ V <<< "$(submit $H_HOG)"
[ "$V" = "MLE" ] || { echo "FAIL: hog-solver verdict $V (want MLE)"; exit 1; }

# --- 5. function-side fuel limit --------------------------------------------
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST localhost:8080/api/routes \
  -H 'content-type: application/json' \
  -d "{\"prefix\":\"/fn/echo\",\"module_hash\":\"$H_ECHO\",\"fuel_limit\":1000}")
[ "$code" = "201" ] || { echo "FAIL: starved rebind -> $code"; exit 1; }
resp=$(curl -s -w "\n%{http_code}" -X POST localhost:8080/fn/echo -d 'x')
code=$(echo "$resp" | tail -1); body=$(echo "$resp" | head -1)
[ "$code" = "500" ] || { echo "FAIL: starved function -> $code (want 500)"; exit 1; }
echo "$body" | grep -q '"outcome":"oof"' || { echo "FAIL: starved body: $body"; exit 1; }
n=$(sqlite3 $DB "SELECT COUNT(*) FROM invocations WHERE ref='/fn/echo' AND outcome='oof'")
[ "$n" = "1" ] || { echo "FAIL: oof ledger rows: $n"; exit 1; }

# --- 6. the workflow runaway watchdog ---------------------------------------
WFID=$(curl -s -X POST localhost:8080/api/workflows -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$H_SPIN\",\"input\":{}}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
for i in $(seq 1 30); do
  st=$(sqlite3 $DB "SELECT status FROM workflows WHERE id='$WFID'")
  [ "$st" = "failed" ] && break
  sleep 1
done
[ "$st" = "failed" ] || { echo "FAIL: spin workflow status '$st' after 30s (want failed)"; exit 1; }
out=$(sqlite3 $DB "SELECT output FROM workflows WHERE id='$WFID'")
echo "$out" | grep -q "compute budget" || { echo "FAIL: watchdog output: $out"; exit 1; }
curl -sf -o /dev/null localhost:8080/ || { echo "FAIL: engine unhealthy after watchdog kill"; exit 1; }

kill -9 "$ENG" 2>/dev/null || true; ENG=""
rm -f $DB $DB-shm $DB-wal $SUM_WRONG
echo "PHASE 5 PASS"
