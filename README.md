# Keel

**The SQLite of workflow engines**: durable execution you just run — no cluster,
no sidecar, no SDK handshake. A single-binary engine in Rust that embeds
[wasmtime], runs user-supplied WASM workflow *components* (component model,
WIT-typed), journals every side effect to SQLite, and recovers workflows after
any crash — including `kill -9` mid-run — by deterministic replay. Where this is
going: [VISION.md](VISION.md) · [ROADMAP.md](ROADMAP.md).

Docs: [operating Keel](docs/operations.md) (deploy, auth, secrets, backups/DR,
fleet tenancy) · [HTTP API](docs/api.md) · [writing guests](docs/guests.md) ·
[capability providers](PROVIDERS.md) (pure sandboxes, or — since v2.5 —
effectful connectors whose every wire call is journaled individually).

Since v2.7 Keel is growing into a **single-binary micro-cloud**
([SPEC-MICROCLOUD.md](SPEC-MICROCLOUD.md)): stateless serverless functions
bound to `/fn/<name>` prefixes — fresh sandboxed instance per request,
fuel/memory/time quotas, a usage ledger for every invocation — that can start
and query durable workflows through the same process. Lambda + Step
Functions, one binary, one SQLite file.

**Embeddable, literally** (since v2.2 the engine is a library, `keel-core`,
and the server is one consumer of it):

```rust
let engine = keel_core::Engine::open(keel_core::EngineOptions::new("app.db"))?;
let hash = engine.upload_module("demo", &wasm_bytes)?;
let id = engine.start_workflow(&hash, r#"{"target":2}"#)?;
// kill -9 the process here; Engine::open recovers it next start
```

Full example: [`core/examples/embedded.rs`](core/examples/embedded.rs).

Built in three phases from [SPEC.md](SPEC.md). **Engineering decisions and
hand-off notes live in [status.md](status.md) — read that first if you are
continuing the build.** MIT licensed.

## How it works (one paragraph)

Guests have zero ambient capabilities (no clocks, no random, no sockets, no
filesystem); the only doors to the outside world are the host functions in
[`wit/workflow.wit`](wit/workflow.wit). Every effectful host call claims a per-workflow
sequence number and is journaled: the result row is committed to SQLite *before* the
result is returned to the guest. Recovery is simply "run the workflow again from the
beginning" — recorded rows are returned instead of re-executing effects, so execution
fast-forwards deterministically to where it died. There is no separate replay mode.

## Quick start

Prebuilt engine binaries (Linux x86_64 + arm64, macOS arm64) are attached to
each [GitHub release](https://github.com/benoshea4/keel/releases) with sha256
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
with a 100ms tick, and an abort flag traps the guest at the next tick. The one
gap: a guest blocked inside a long host call (an in-flight HTTP GET) can't be
interrupted mid-call — cancel answers 409, retry once the call returns.
`scripts/smoke_cancel.sh` proves both paths against `guests/counter` (parked)
and `guests/spin` (spinning).

Runaway workflows also die on their own: every run/resume gets a fuel budget
(`--wf-fuel-limit`, default 10^13 instructions ≈ minutes of continuous
compute). An infinite loop exhausts it and fails with `runaway guest:
exhausted compute budget`; parked workflows spend zero, and replay resets to
the full budget so it can never trip a limit the original run survived.

## Tests

`cargo test -p keel-engine` runs unit tests for the journal replay/nondeterminism
core and the multi-statement transactions (event delivery, upgrade tail-discard,
sleep wake, cancel) against in-memory SQLite. The five scripts under `scripts/`
are the end-to-end gates (three phase suites, cancel, auth+limits); all of it
runs in CI on every push (`.github/workflows/ci.yml`) — fully offline, phase 1
fetches a local stub.

## Scope and security posture

Keel binds `127.0.0.1` by default and starts in **open mode** (no auth) for
frictionless local use. To expose it wider, set an operator token — then every
API call needs `Authorization: Bearer <token>` and the UI gets a login page:

```bash
KEEL_API_TOKEN=$(openssl rand -hex 24) ./target/release/keel serve --listen 0.0.0.0:8080
```

Remember the trust model even with a token: whoever holds it can upload and
execute arbitrary WASM, and the guest `http-get` capability fetches any URL the
engine's host can reach (including internal addresses). Terminate TLS in front
of it — e.g. Caddy: `your.domain { reverse_proxy 127.0.0.1:8080 }` — and treat
the token like a root credential. Guests are additionally capped per workflow
(`--max-guest-memory-mb`, default 256; plus the 1s epoch tick for cancel).

Other non-goals: multi-node clustering, in-process TLS (terminate it in front),
streaming request/response bodies, physical linear-memory snapshots, WASI 0.3
async. (Earlier-phase non-goals that have since shipped: token auth v1.1,
non-GET guest HTTP v1.2, metrics v1.2, OTel v2.4.)

[wasmtime]: https://wasmtime.dev
