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

## Known, accepted limitation: runaway guests

Runaway-guest protection is a non-goal: a guest that spins in pure compute without
host calls pins its thread forever and even upgrade cannot abort it, since aborts are
only observed at park points and host calls — this is a KNOWN, ACCEPTED limitation;
the production fix is wasmtime epoch interruption, explicitly deferred.

Other non-goals (all phases): multi-node clustering, authentication, TLS, HTTP methods
other than GET in the guest API, streaming bodies, physical linear-memory snapshots,
WASI 0.3 async, metrics/OTel.

[wasmtime]: https://wasmtime.dev
