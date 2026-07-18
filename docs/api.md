# Keel HTTP API

Auth: with `--api-token`/`KEEL_API_TOKEN` set, every route below requires
`Authorization: Bearer <token>` (401 otherwise). Without a token the engine is
open (loopback use). `/assets/*`, `/login`, `/logout` never require auth.

Write endpoints marked *(also form)* additionally accept the UI's
urlencoded/multipart body shape.

## Modules

| Route | What |
|---|---|
| `POST /api/modules?name=<n>` *(also multipart: `file`, `name`)* | Body = raw component bytes. Validates the `\0asm` magic (400 otherwise). → `{"hash": "<sha256hex>"}`. Content-addressed: re-upload is a no-op (the first name wins). |
| `POST /api/providers?name=<n>&tier=pure\|effectful` *(also multipart: `file`, `name`, `tier`; v2.6)* | Body = provider component bytes, pre-flighted for the TIER at the door (wrong world/imports → 400, never a workflow failure) → `{"hash"}`. The swap is LIVE: the next `provider-call` uses it; recorded journal rows replay unchanged. With `&hash=<h>` and no body: REBIND the name to an already-stored blob (rollback; 404 if unknown hash). See [PROVIDERS.md](../PROVIDERS.md). |
| `GET /api/providers` | Registry bindings: `[{name, tier, hash, updated_at}]`. |
| `DELETE /api/providers/{name}` | Unbind (the blob stays for rebind). Later calls to the name err as unregistered — data, journaled. 404 if not bound. |

## Workflows

| Route | What |
|---|---|
| `POST /api/workflows` *(also form)* | `{"module_hash": "...", "input": <any json>}` → `{"id": "<uuid>"}`. 404 unknown hash. The workflow starts immediately. |
| `GET /api/workflows?status=&limit=&offset=` | Paged listing, newest first (metadata only). `limit` ≤ 500, default 100. |
| `GET /api/workflows/{id}` | `{id, status, output, module_hash, created_at, updated_at}`. `output` is a JSON *string* (or error text on `failed`). |
| `GET /api/workflows/{id}/journal` | The journal rows: `{seq, kind, request, response, created_at}` — request/response are the stored JSON strings. |
| `POST /api/workflows/{id}/events` *(also form)* | `{"name": "...", "payload": <any json>}` → 202. Queued until a matching `await-event`; FIFO per name. |
| `POST /api/workflows/{id}/upgrade` *(also form)* | `{"module_hash": "<new>"}` → move a **parked, checkpointed** workflow onto new code. Pre-flights the module (400 if it could never run), 409 if not parked / no checkpoint / operation already in flight. → `{id, module_hash, resumed_from_seq}`. |
| `POST /api/workflows/{id}/cancel` | → 200, workflow becomes `failed` with output `cancelled by operator`. Works parked or spinning; 409 if terminal, mid-host-call (retry), or another operation holds the claim. |

## Schedules (v1.2, cron + pause v2.1)

| Route | What |
|---|---|
| `POST /api/schedules` | `{"module_hash", "input", "interval_ms"}` (≥1000) **or** `{"module_hash", "input", "cron"}` — exactly one of the two → `{id, next_run_at}`. `cron` is 6 fields, seconds first (`sec min hour dom mon dow`), UTC, vixie semantics (`*` `a-b` `a,b` `/step`; dom/dow OR when both restricted). Missed windows (downtime) collapse into one firing either way. Bad expressions 400 here, never in the scheduler. |
| `GET /api/schedules` | All schedules with `cron`, `next_run_at`, `enabled`. |
| `PATCH /api/schedules/{id}` | `{"enabled": true\|false}` → pause/resume firing. A re-enabled interval schedule fires once for the paused gap; a cron schedule waits for its next match. |
| `DELETE /api/schedules/{id}` | → 204. Already-spawned workflows are untouched. |

## Operations

| Route | What |
|---|---|
| `GET /metrics` | Prometheus text: `keel_workflows{status=...}`, `keel_worker_threads`. |
| `GET /` `/modules` `/workflows/{id}` | The htmx UI (2s-polling). |
| `GET/POST /login`, `GET /logout` | UI session when a token is configured. |

## Status codes worth knowing

- `401` missing/invalid token (token mode only).
- `404` unknown workflow/module/schedule id.
- `409` operation conflicts: upgrade/cancel claim held, workflow not parked,
  no checkpoint, already terminal, busy in a host call.
- `400` malformed body, non-wasm upload, module that fails upgrade pre-flight.
