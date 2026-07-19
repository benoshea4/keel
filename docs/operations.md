# Operating Keel

Everything here assumes the single binary (`target/release/keel` or a release
tarball). All state is one SQLite file per engine.

## Run

```bash
keel serve --db keel.db --listen 127.0.0.1:8080
```

Flags that matter in production:

| Flag | Default | What |
|---|---|---|
| `--api-token` / `KEEL_API_TOKEN` | unset (open mode) | Operator bearer token. Set it for anything beyond loopback; prefer the env var (argv is world-readable). |
| `--max-running` | 256 | Worker-thread cap. Parked workflows hold a slot — size generously. |
| `--max-guest-memory-mb` | 256 | Per-workflow linear-memory cap. |
| `--wf-fuel-limit` | 10^13 | Micro-cloud phase 4 — per-run/resume workflow fuel budget (a runaway kill-switch, not a quota). An infinite loop fails with `runaway guest: exhausted compute budget`; parked workflows spend zero; replay resets to the full budget. Raise it only if a legitimate very-long replay ever trips it. |
| `--retain-terminal-hours` | 0 (keep forever) | GC completed/failed workflows (journal, events, snapshots, kv included) after this many hours. |
| `--retain-ledger-hours` | 0 (keep forever) | Amendment 1: GC invocations-ledger rows and captured function logs after this many hours. Rate limits read a 60-second window, so hours-scale retention can't interact with them. |
| `--max-fn-concurrent` | 64 | v3.3: max concurrently executing function/app-backend sandboxes, process-wide. Each run parks one blocking thread for its full quota; beyond the cap the data plane answers `503` + `Retry-After: 1` (`keel_fn_over_capacity_total`). Per-route rate limits bound admission per ref — this bounds the process. |
| `--max-compiled-modules` | 64 | v3.3: compiled components held in memory (LRU beyond it; `keel_compiled_cache_size`). A JIT image is MBs — size to your hot module count; eviction is transparent (recompile on next use). |
| `--data-timeout-secs` | 30 | v3.3: whole-request deadline on `/fn/*` and `/apps/*` (`408` beyond it) — a slow-drip body can no longer hold a connection forever. Covers upload AND execution: keep it above your largest route `time_limit_ms`. Control plane and UI are unaffected. |
| `--backup-dir` + `--backup-interval-secs` + `--backup-keep` | off / 300 / 24 | Periodic online snapshots (below). |
| `--secrets-file` | unset | KEY=VALUE file backing the `secret` host call (below). |
| `--provider name=path.wasm` | none | Register a PURE capability provider (repeatable) — see [PROVIDERS.md](../PROVIDERS.md). Import-free, enforced. Compiled + type-checked at boot; a bad provider fails the start. v2.6: flags UPSERT into the provider REGISTRY — the provider persists across restarts; remove with `DELETE /api/providers/{name}`, and providers can also be uploaded/rolled live via `POST /api/providers` (no restart). |
| `--provider-effectful name=path.wasm` | none | v2.5 — register an EFFECTFUL provider (repeatable): may import `keel:provider/host-http` and make real HTTP calls, each journaled individually. The grant means the provider can reach any URL this host can — an operator decision, same trust class as the guest `http-get` surface. |

## Exposing it

1. Set a token: `KEEL_API_TOKEN=$(openssl rand -hex 24)`.
2. Terminate TLS in front — Caddy is three lines:
   ```
   keel.example.com {
       reverse_proxy 127.0.0.1:8080
   }
   ```
3. Treat the token as root: it uploads and executes arbitrary WASM, and guest
   `http-request` reaches anything the host can reach.

API clients send `Authorization: Bearer <token>`; the UI logs in at `/login`
(the cookie stores a digest, never the token; `/logout` clears it).

Micro-cloud surfaces split into two planes: the CONTROL plane (`/api/*`, the
UI) honors the token like everything else, but the DATA plane — `/fn/*`
function calls and phase-6 `/apps/*` serving — is deliberately public even
with a token set: a browser-served app must reach its own backend tokenless.
Don't bind a function you wouldn't expose to whoever can reach the listener.

Since Amendment 1, the public plane can be *bounded*: give each route (and
each app's backend) a `rate_limit` — max admitted runs per rolling 60 s,
counted off the invocations ledger, so limits are exact under bursts and
survive restarts. Over the limit → 429 with an honest `Retry-After`; watch
`keel_fn_rate_limited_total` in `/metrics`. Pair with
`--retain-ledger-hours` so the ledger and captured function logs can't grow
a public listener's disk without bound.

**Upgrading to v3.5 (WIT 0.8.0) is a guest rebuild event.** The wit package
version is part of a component's import names, so handler/workflow modules
compiled against ≤ 0.7.0 fail to instantiate on a 0.8.0 engine (publicly a
generic 500; the real reason in the engine log). Rebuild with the 0.8.0 wit,
re-upload (new content hash), re-bind. Solver binaries are unaffected — the
judge's world is import-free.

v3.3 closes the remaining unbounded dimensions. `--max-fn-concurrent` caps
concurrently *executing* sandboxes process-wide (503 beyond it — honest
backpressure; asset serving takes no execution slot, so a hosted app's
frontend stays up even when its functions are saturated), judge runs
serialize instead of stacking blocking threads, and `--data-timeout-secs`
puts a whole-request deadline on the data plane. Engine faults on the public
plane answer a generic `{"error":"internal error"}` — the full error chain
lands in the engine log (grep `public-plane`), because tokenless callers
don't get module hashes, database paths, or compiler output.

## Outbound HTTP (proxy-world grants)

v4.0 lets a `wasi:http/proxy` route or app make real outgoing HTTP, but only
when bound with `allow_outbound: true` — the default is a clean in-band denial
(SSRF posture mirrors effectful providers). The grant is visible in every read
path: `GET /api/routes` and `GET /api/apps` both echo `allow_outbound`, and
`keel ls` shows it — so a capability inventory of a fleet never misses an
outbound-capable ref.

An outbound call cannot outlive the invocation's own wall-clock budget. The
proxy runs synchronously on a blocking thread, so a guest parked in an
outbound to a slow or hung upstream would otherwise pin its `--max-fn-concurrent`
permit far past the route's `time_limit_ms` (wasi-http's per-phase timeouts
default to 600 s and the guest can raise them). The engine bounds the permit
hold to O(`time_limit_ms`) three ways: each connect / first-byte / between-bytes
phase is clamped DOWN to the route's `time_limit_ms`; a new outbound is refused
once the invocation's wall-clock budget is spent (no chaining sub-budget calls);
and the store's epoch deadline traps the guest at its next wasm boundary once
wall time exceeds the budget. Net effect: a granted outbound to a dead upstream
returns in ~`time_limit_ms`, the permit frees, and a flood of hung outbounds at
the cap clears without wedging the data plane. Size `time_limit_ms` for the
slowest upstream a route legitimately calls.

## Secrets

```bash
install -m 600 /dev/null secrets.env
echo 'stripe-key=sk_live_...' >> secrets.env
keel serve --db keel.db --secrets-file secrets.env
```

Format: `KEY=VALUE` per line, `#` comments, keys trimmed, values verbatim
after the first `=`. Duplicate keys and `=`-less lines fail startup (fail
fast beats a wrong secret at 3am). The engine warns unless the file is
mode 600.

What guests see and what lands on disk: `secret(name)` returns the live
value; the journal records only the name and a **salted sha256**; values a
workflow has read are redacted (`{{secret:name}}`) from its journaled
HTTP requests. The database and its backups never contain secret bytes —
the secrets file is the one thing you must protect (and it deliberately
does NOT ride along in `--backup-dir` snapshots: restore = db snapshot +
your secrets file).

**Rotation:** editing the file takes effect immediately for *new* reads.
An in-flight workflow that already read the old value keeps working —
UNLESS it crashes/restarts and replays the read, which then fails loudly
("changed mid-workflow"): restore the old value, let the workflow finish,
rotate again; or cancel the workflow. Never rotate-and-restart as one move
unless failing those workflows is what you want.

## Backups and disaster recovery

**Continuous restore points (built in):**

```bash
keel serve --db keel.db --backup-dir backups/ --backup-interval-secs 300 --backup-keep 24
```

Each snapshot (`backups/keel-<millis>.db`) is a consistent online copy taken
with SQLite's backup API — safe while workflows are writing, fully
checkpointed, no `-wal`/`-shm` needed. One-shot equivalent (live db is fine):

```bash
keel backup --db keel.db --to /somewhere/keel-snapshot.db
```

**Restore runbook (this exact sequence is CI-tested by `scripts/smoke_dr.sh`):**

1. Stop the engine (hard kill is fine — it always is).
2. `rm -f keel.db keel.db-wal keel.db-shm` (if anything is left).
3. `cp backups/keel-<newest>.db keel.db`
4. Start the engine. Recovery replays every non-terminal workflow from its
   journal/checkpoint; work that happened *after* the snapshot re-executes
   live (the at-least-once window, now as wide as your backup interval —
   size `--backup-interval-secs` accordingly).

**Off-box / continuous replication:** run [Litestream](https://litestream.io)
against the db file (`litestream replicate keel.db s3://bucket/keel`) — WAL
streaming to object storage is exactly its job, and Keel is a standard SQLite
WAL database. Point-in-time restore then comes from Litestream, and the runbook
above starts at step 3.

## Multi-tenant: the fleet

One tenant = one process + one database + one token (see VISION.md for why
there is no shared-db mode). Config:

```toml
# fleet.toml
[[tenants]]
name = "acme"          # [a-z0-9-], unique
port = 9101            # 127.0.0.1 port, unique
db = "acme.db"
api_token = "..."      # per-tenant root credential
# optional per-tenant: max_running, max_guest_memory_mb,
# retain_terminal_hours, retain_ledger_hours, backup_dir, backup_interval_secs, backup_keep,
# max_fn_concurrent, max_compiled_modules, data_timeout_secs (v3.3 — a tenant's flood stays its own problem),
# secrets_file (per-tenant secrets — cells never share one),
# providers = ["name=path.wasm", ...] (per-tenant capability providers)
# providers_effectful = ["name=path.wasm", ...] (v2.5 — per-tenant effectful grants)

[[tenants]]
name = "globex"
port = 9102
db = "globex.db"
api_token = "..."
```

```bash
keel fleet --config fleet.toml
```

The supervisor spawns each tenant (`keel-<name>.log` per tenant), polls every
second, and restarts dead cells after 1s — children are hard-killed on ctrl-c
and never asked nicely, because kill -9 is a supported shutdown. Routing/TLS is
the proxy's job — one Caddy host per tenant port:

```
acme.keel.example.com   { reverse_proxy 127.0.0.1:9101 }
globex.keel.example.com { reverse_proxy 127.0.0.1:9102 }
```

Run the fleet itself under systemd (`Restart=always`); if the supervisor dies,
orphaned cells keep serving until systemd restarts the tree.

## Deploying

Copy-paste recipes in [`docs/deploy/`](deploy/): a hardened systemd unit
([`keel.service`](deploy/keel.service) — token via EnvironmentFile,
SIGKILL stop because hard kills are supported), a two-stage
[`Dockerfile`](deploy/Dockerfile) + [`compose.yml`](deploy/compose.yml), and
a single-replica [`k8s.yaml`](deploy/k8s.yaml) (**replicas must stay 1 per
database** — one writer per journal; scale by adding tenant cells, not
replicas; `strategy: Recreate` keeps two writers from overlapping).

## Monitoring

`GET /metrics` (Prometheus text, behind the same token):
`keel_workflows{status=...}` gauges, `keel_worker_threads` (live worker
threads, parked included) and `keel_active_permits` (threads actually holding
a `--max-running` slot — this is the one that shows saturation). Engine logs
go to stdout (`tracing`, INFO).

**Traces (optional):** build with `cargo build --release -p keel-engine
--features otel` and set `OTEL_EXPORTER_OTLP_ENDPOINT` (default
`http://localhost:4318`) — one span per workflow execution with a child span
per journaled host call, exported via OTLP/http. The default binary carries
none of the OTel dependency tree. Traces are best-effort: kill -9 (a
supported shutdown) drops unexported spans.

## Upgrading the engine binary

Stop, swap the binary, start — recovery handles the rest. One caveat that
bites: WIT `0.x` versions are blob-incompatible. If the new engine carries a
new WIT version (see the release notes), *stored module blobs keep running
only when their interface still instantiates* — rebuild and re-upload guests,
then `POST /api/workflows/{id}/upgrade` checkpointed workflows onto the new
hashes. Terminal workflows never care.

## Stuck things

- Workflow you want gone: `POST /api/workflows/{id}/cancel` (parked or
  spinning; 409 mid-host-call — retry after it returns).
- Full crib: the "Debugging crib" in [status.md](../status.md).
