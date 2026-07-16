# Keel roadmap

Positioning and the reasoning behind this sequence live in [VISION.md](VISION.md).
Every shipped item below is gated by a script under `scripts/` (they are the
definition of done) and runs in CI.

## Shipped

| Version | What | Proof |
|---|---|---|
| v1.0 | Durable core: journal-before-return, replay recovery, durable timers, external events, checkpoints + pruning, live v1→v2 upgrade with pre-flight, cancel (parked + spinning guests via epoch interruption), htmx UI | `accept_phase{1,2,3}.sh`, `smoke_cancel.sh`, `cargo test` |
| v1.1 | Operator token auth (`--api-token`/`KEEL_API_TOKEN`, cookie-digest UI login), per-guest memory caps (`--max-guest-memory-mb`) | `smoke_auth.sh` |
| v1.2 | Effects: `http-request` (method/headers/body, non-2xx as data, opt-in retries), journaled per-workflow KV (`kv-set`/`kv-get`), interval schedules (`/api/schedules`) — WIT 0.4.0 | `smoke_effects.sh` |
| v1.3 | Operability: `GET /api/workflows` (paged, filtered), `/metrics` (Prometheus text), retention GC (`--retain-terminal-hours`), UI logout | `smoke_effects.sh` |
| v2 (slice) | **DR**: periodic online snapshots (`--backup-dir`/`--backup-interval-secs`/`--backup-keep`) + one-shot `keel backup`, restore-by-copy runbook. **Cell tenancy**: `keel fleet --config` — one process/db/token per tenant, supervised, crash-only restarts | `smoke_dr.sh`, `smoke_fleet.sh` |

## Answered design questions

- **Multi-tenancy** is cells (process + db + token per tenant), never tenant
  columns in a shared database — one writer lock and one blast radius per
  tenant, not per fleet. See VISION.md for the full argument.
- **RLS** (row-level security): unnecessary under cell tenancy — the process
  boundary is the row boundary. A tenant's SQLite file contains only that
  tenant. If in-db multi-tenancy is ever revisited (it shouldn't be), RLS-style
  scoping would be the minimum bar; cells make the whole class moot.
- **Replication**: restore points are built in (online snapshots above);
  continuous off-box replication is deliberately delegated to
  [Litestream](https://litestream.io) (`docs/operations.md`) rather than
  reimplemented — SQLite WAL streaming is its whole job. Native streaming
  replication only becomes Keel's problem if a managed cloud gets built.

## Next

- **v2.1 — extension surface:** capability providers via the WIT world (custom
  journaled effects, event-source connectors); library/binary crate split so
  Keel embeds in-process.
- **v2.2 — scheduling & config depth:** cron expressions on schedules (interval
  is the primitive today), per-schedule enable/disable API, `secret(name)` host
  call behind a written design (journals must not become a secrets dump).
- **v2.3 — journal semantics:** KV versioning across upgrade tail-discards
  (today a discarded tail's kv writes survive — documented caveat in
  docs/guests.md), idempotency keys for the at-least-once exec window.
- **v2.4 — surface polish:** schedules in the UI, OTel traces, linux-arm64
  release binaries, systemd/k8s deployment recipes, load/fuzz test suites.
- **Cloud (unversioned):** the hosted control plane on fleet cells — built if
  and when the open engine earns it (VISION.md).
