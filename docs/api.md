# Keel HTTP API

Auth: with `--api-token`/`KEEL_API_TOKEN` set, every route below requires
`Authorization: Bearer <token>` (401 otherwise). Without a token the engine is
open (loopback use). `/assets/*`, `/login`, `/logout` never require auth.

Write endpoints marked *(also form)* additionally accept the UI's
urlencoded/multipart body shape.

The CLI verbs (`keel deploy` / `bind` / `run` / `logs`, Amendment 1 A4) are
thin clients of exactly these endpoints — anything they do, curl can do.

## Modules

| Route | What |
|---|---|
| `POST /api/modules?name=<n>` *(also multipart: `file`, `name`)* | Body = raw component bytes. Validates the `\0asm` magic (400 otherwise). → `{"hash": "<sha256hex>"}`. Content-addressed: re-upload is a no-op (the first name wins). |
| `POST /api/providers?name=<n>&tier=pure\|effectful` *(also multipart: `file`, `name`, `tier`; v2.6)* | Body = provider component bytes, pre-flighted for the TIER at the door (wrong world/imports → 400, never a workflow failure) → `{"hash"}`. The swap is LIVE: the next `provider-call` uses it; recorded journal rows replay unchanged. With `&hash=<h>` and no body: REBIND the name to an already-stored blob (rollback; 404 if unknown hash). See [PROVIDERS.md](../PROVIDERS.md). |
| `GET /api/providers` | Registry bindings: `[{name, tier, hash, updated_at}]`. |
| `DELETE /api/providers/{name}` | Unbind (the blob stays for rebind). Later calls to the name err as unregistered — data, journaled. 404 if not bound. |
| `POST /api/routes` *(micro-cloud phase 4)* | `{"prefix":"/fn/echo","module_hash":"...", "fuel_limit"?, "mem_limit"?, "time_limit_ms"?, "rate_limit"?, "allow_outbound"?}` → 201. v4.0 ([Amendment 3](SPEC-AMENDMENT-3.md)): the module may be a **`wasi:http/proxy` component** (Spin/JCO/ecosystem output) — the dispatcher detects the world from the export surface at request time; same quotas, ledger, and limits either way. `allow_outbound: true` grants a proxy-world guest real outgoing HTTP (default: a clean in-band denial — SSRF posture mirrors effectful providers). Binds a URL prefix to a `handler`-world FUNCTION component (stateless, sandboxed per request). Re-POSTing a prefix re-binds it. Prefix rules: starts `/fn/`, no trailing slash, no `..`. Defaults: fuel 5×10⁸, mem 64 MiB, time 5000 ms. `rate_limit` (Amendment 1): max admitted runs per rolling 60 s, absent = unlimited; over it → 429 with `Retry-After` and `{"error":"rate limited",...,"retry_after_ms"}`. The window state is the invocations ledger itself, so limits survive restarts; 429s write no ledger row (see `keel_fn_rate_limited_total` in /metrics). |
| `GET /api/routes` | All bindings with quotas: `[{prefix, module_hash, fuel_limit, mem_limit, time_limit_ms, created_at, rate_limit, allow_outbound}]`. |
| `DELETE /api/routes/{prefix}` | Unbind → 204 (404 if not bound). |
| `GET /api/logs?kind=function\|app&ref=<prefix-or-name>[&after=<id>][&limit=<n>]` *(Amendment 1)* | Captured platform-api `log` lines: `{"lines":[{id, invocation_id, line, created_at}]}`. Without `after`: newest `limit` (≤1000, default 100), oldest-first. With `after`: lines with id > after — the tail-following contract (`keel logs --follow`, the /logs page). Caps: 256 lines/invocation, 2 KiB/line, newest 2000 kept per ref. |
| `ANY /fn/*` *(PUBLIC — no token)* | The function data plane: longest-prefix match against bound routes, fresh sandboxed instance per request, body capped at 10 MiB (413). Outcome ≠ ok → 500 `{"outcome":"tle"\|"mle"\|"oof"\|"trap"}`. Every invocation (failures included) writes a `invocations` usage-ledger row. |
| `POST /api/problems` *(phase 5)* | Idempotent operator seeding: `{"slug","title","statement","cases":[{"input","expected"},…]}` — the case list is replaced atomically. |
| `POST /api/submissions` | Judge a solver against a problem. JSON `{"problem","module_hash"}`, or multipart `problem`+`file` (stores the module first). → 202 `{"id"}`; judging runs off-thread. Per-case quotas: 10⁹ fuel, 256 MiB, 2000 ms. Verdicts: AC · WA · TLE · MLE · OOF · RE; stops at first non-AC; each case writes a `solve` ledger row. |
| `GET /api/submissions/{id}` | `{"verdict"}` is null while judging; `detail` = per-case JSON (verdict, fuel, peak_mem, ms). |
| `POST /api/apps` *(phase 6)* | `{"name","backend_hash"?, "rate_limit"?, "allow_outbound"?}` → 201 (`rate_limit`? caps the app's api/* backend calls per rolling 60 s, like routes; `allow_outbound`? grants a proxy-world backend real outgoing HTTP, default deny — see [operations.md](operations.md#outbound-http-proxy-world-grants)). Name `[a-z0-9-]{1,32}`; no backend = static-only; re-POST re-binds. |
| `POST /api/config` *(v3.5, [Amendment 2](SPEC-AMENDMENT-2.md))* | `{"kind":"function"\|"app","ref","name","value"}` → 201 upsert. Per-ref operator config for `platform-api.config-get`. Door checks: name `[A-Za-z0-9_-]{1,64}`, value ≤ 4 KiB, ≤ 64 entries/ref. |
| `GET /api/config?kind=&ref=` | `{"names":[...]}` — **names only; no endpoint ever echoes a value.** |
| `DELETE /api/config?kind=&ref=&name=` | 204 / 404. |
| `GET /api/kv?kind=&ref=` *(v3.5)* | `{"keys":[...]}` — keys only (values are guest state). |
| `DELETE /api/kv?kind=&ref=` | 204 — wipes the ref's whole kv store ("reset my function"). |
| `GET /api/apps` *(v3.4)* | `[{name, backend_hash, assets, created_at, rate_limit, allow_outbound}]` — the listing `keel ls` reads. `allow_outbound` (v4.1) mirrors the routes listing so an outbound-capability inventory covers apps too. |
| `DELETE /api/apps/{name}` *(v3.4)* | App + its stored assets, one transaction → 204 (404 if absent). Ledger rows and captured logs remain — history is `--retain-ledger-hours`'s job. |
| `POST /api/apps/{name}/assets` | Body = a zip of the app's `dist/` (64 MiB cap). All-or-nothing: zip-slip entries (`..`, absolute) → 400 with nothing stored; decompressed total capped at 256 MiB (zip bombs → 400). `.wasm`/`.js` content types forced. → `{"stored": N}`. v3.4: each asset stores its sha256 as an `ETag`; serving honors `If-None-Match` → 304, hash-named files (`-<12+ hex>.ext`) get `max-age=31536000, immutable`, `index.html` stays `no-store`. |
| `ANY /apps/{name}/*` *(PUBLIC — no token)* | App serving: `/` → index.html · exact asset (stored content type, `cache-control: no-store`) · `api/*` → the backend function (guest sees the path after `api`) · extensionless → index.html (SPA fallback) · else 404. Bare `/apps/{name}` 301-redirects to `…/` so relative asset URLs resolve. |

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
