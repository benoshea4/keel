# Keel

A single-binary durable execution engine in Rust. It embeds [wasmtime], runs
user-supplied WASM workflow *components* (component model, WIT-typed), journals every
side effect to SQLite, and recovers workflows after any crash — including `kill -9`
mid-run — by deterministic replay.

Built in three phases from [SPEC.md](SPEC.md). **Current build progress, decisions,
and hand-off notes live in [status.md](status.md) — read that first if you are
continuing the build.**

## How it works (one paragraph)

Guests have zero ambient capabilities (no clocks, no random, no sockets, no
filesystem); the only doors to the outside world are the host functions in
[`wit/workflow.wit`](wit/workflow.wit). Every effectful host call claims a per-workflow
sequence number and is journaled: the result row is committed to SQLite *before* the
result is returned to the guest. Recovery is simply "run the workflow again from the
beginning" — recorded rows are returned instead of re-executing effects, so execution
fast-forwards deterministically to where it died. There is no separate replay mode.

## Quick start

Prebuilt engine binaries (Linux x86_64, macOS arm64) are attached to each
[GitHub release](https://github.com/benoshea4/keel/releases) with sha256
checksums — or build from source:

```bash
# build engine + demo guest (needs: rust, cargo-component, wasm32-unknown-unknown target)
cargo build --release -p keel-engine
(cd guests/demo && cargo component build --release --target wasm32-unknown-unknown)

# run
./target/release/keel serve --db keel.db --listen 127.0.0.1:8080

# upload the demo guest, start a workflow, watch it
curl -s -X POST --data-binary @guests/demo/target/wasm32-unknown-unknown/release/demo.wasm \
  'localhost:8080/api/modules?name=demo'                     # -> {"hash":"..."}
curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d '{"module_hash":"<hash>","input":{}}'                   # -> {"id":"..."}
curl -s localhost:8080/api/workflows/<id>                    # status/output
curl -s localhost:8080/api/workflows/<id>/journal            # the journal itself
```

Prove durability to yourself: start the demo workflow, `kill -9` the engine during its
15-second sleep, start it again — the workflow completes without re-running the
already-journaled HTTP call, and the sleep resumes from its original deadline instead
of restarting. `scripts/accept_phase1.sh` does exactly this, with assertions.

## UI and events (phase 2)

The same binary serves a small htmx UI: `http://127.0.0.1:8080/` lists workflows
(2s live refresh), `/modules` uploads components and starts workflows, and each
workflow page has a journal view plus a "Send event" form. Workflows can park on
external events (`await-event` in the WIT contract); deliver one with:

```bash
curl -s -X POST localhost:8080/api/workflows/<id>/events \
  -H 'content-type: application/json' \
  -d '{"name":"approve","payload":{"by":"alice"}}'          # -> 202
```

`scripts/accept_phase2.sh` proves the phase-2 story: kill -9 while parked on an
event, kill -9 again mid-sleep, and the workflow still completes exactly once with
the delivered payload.

## Checkpoints and live code upgrade (phase 3)

A guest can call `checkpoint(state)` at a safe point: the engine snapshots the
state blob and prunes all older journal rows, so long-running workflows restart
from the checkpoint (`resume(state)`) instead of replaying from the beginning. A
workflow that is parked (sleeping / waiting for an event) *and* has a checkpoint
can be moved onto new code without losing its state:

```bash
curl -s -X POST localhost:8080/api/workflows/<id>/upgrade \
  -H 'content-type: application/json' \
  -d '{"module_hash":"<new module hash>"}'
```

The engine aborts the parked worker at its park point, discards the journal tail
beyond the checkpoint (re-queueing any events that tail had consumed), points the
workflow at the new module, and resumes it from the checkpoint state. The upgrade
pre-flights the new module (compile + world check) before touching anything, so a
bad hash can never brick a workflow. One documented wrinkle: an in-flight sleep
restarts from `resume` with a fresh full duration after an upgrade, because its
timer and journal tail were discarded. `scripts/accept_phase3.sh` proves pruning,
resume-based recovery, and a v1→v2 upgrade mid-workflow.

## Cancelling workflows

Any non-terminal workflow can be cancelled; it lands in `failed` with output
`cancelled by operator`:

```bash
curl -s -X POST localhost:8080/api/workflows/<id>/cancel     # -> 200
```

Parked workflows abort immediately at their park point. Guests spinning in pure
wasm (`loop {}`) are stopped too: the engine runs wasmtime epoch interruption
with a 1-second tick, and an abort flag traps the guest at the next tick. The one
gap: a guest blocked inside a long host call (an in-flight HTTP GET) can't be
interrupted mid-call — cancel answers 409, retry once the call returns.
`scripts/smoke_cancel.sh` proves both paths against `guests/counter` (parked)
and `guests/spin` (spinning).

## Tests

`cargo test -p keel-engine` runs unit tests for the journal replay/nondeterminism
core and the multi-statement transactions (event delivery, upgrade tail-discard,
sleep wake, cancel) against in-memory SQLite. The four scripts under `scripts/`
are the end-to-end gates; all of it runs in CI on every push
(`.github/workflows/ci.yml`) — fully offline, phase 1 fetches a local stub.

## Scope and security posture

Keel binds `127.0.0.1` by default and has **no authentication**: anyone who can
reach the port can upload and execute arbitrary WASM, and the guest `http-get`
capability will fetch any URL the engine's host can reach (including internal
addresses). Keep it on loopback, or put an authenticating proxy in front of it
before choosing `--listen` on a wider interface.

Other non-goals (all phases): multi-node clustering, authentication, TLS, HTTP methods
other than GET in the guest API, streaming bodies, physical linear-memory snapshots,
WASI 0.3 async, metrics/OTel.

[wasmtime]: https://wasmtime.dev
