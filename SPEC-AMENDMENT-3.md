# Amendment 3 to the micro-cloud extension — the ecosystem release

Status: AUTHORED IN-REPO (2026-07-19), same discipline as Amendments
[1](SPEC-AMENDMENT-1.md) and [2](SPEC-AMENDMENT-2.md): spec first, code
second. Approved sequence: status.md §R ("do 3.4 and 3.5 and beyond").

Ships as **v4.0** — phase-sized, the largest stage since the micro-cloud
extension itself. Acceptance script `scripts/accept_ecosystem.sh` is the
definition of done.

Motivation: today only keel-WIT guests run. The component ecosystem —
Spin apps, componentize-js/JCO output, anything targeting `wasi:http` —
compiles to a world keel cannot drive. One dispatcher change makes the
ENTIRE ecosystem deployable on a one-binary cloud. That is the single
biggest adoption unlock on the board, and it deserves its decisions made
here, not mid-code.

---

## E1. The dependency decision — wasmtime-wasi(-http), adopted deliberately

Keel's host surface has been hand-rolled since phase 1: zero wasi
dependencies, every host call ours. That ends here, deliberately and only
for the compatibility world: **`wasmtime-wasi` + `wasmtime-wasi-http`
(the wasmtime 43 family) become dependencies of keel-core.** They are the
reference implementation of `wasi:io/streams`, `wasi:clocks`,
`wasi:random`, `wasi:cli` stdio and the `wasi:http` types; hand-rolling
streams is a losing game and a security surface we don't want to own.
The keel-WIT worlds (`workflow`, `handler`, `solver`, providers) remain
hand-rolled and untouched — the new crates are linked ONLY into the
proxy path.

**Sync is a requirement, not a preference.** Keel's engine is synchronous
(`async_support` off; fuel + epoch on ONE engine buys one component cache
across everything — the phase-4 invariant). The proxy host must use the
sync bindings (`add_to_linker_sync` / the sync `Proxy` bindings) on the
EXISTING engine. If the adopted crate version cannot provide a sync path,
the fallback is a second, async-configured `wasmtime::Engine` reserved for
proxy components — an explicit bend of the one-engine invariant that must
be recorded in status.md with its cost (a second component cache) if
taken. The builder must try sync first and document the outcome.

## E2. World detection — lazy, at invoke, never at bind

`POST /api/routes` keeps its existence-only check. This is pinned by an
immutable gate: accept_harden.sh BINDS a non-compiling module (201) and
asserts the failure surfaces at request time as a generic 500. Therefore:

- Detection happens at dispatch, from the COMPILED component's export
  surface: an export of `wasi:http/incoming-handler@0.2.x` → the proxy
  path; a keel `handle` export → the phase-4 path; neither → engine fault
  (generic 500, detail logged — v3.3 posture).
- The result is cached in memory keyed by module hash (alongside the
  compiled-component LRU; eviction re-detects — detection is a type-level
  inspection, microseconds).
- The routes table does NOT gain a world column. The module IS the truth;
  a re-upload under the same route follows its bytes.
- App backends get the same detection for free (same dispatch core).

## E3. The proxy invocation — same walls, same ledger

A proxy-world request is still a function invocation in every way that
matters:

- **Quotas**: the bound route's fuel / memory / time quotas apply
  unchanged (set_fuel, MemLimiter, epoch deadline on the same store).
- **Admission**: rate limits and the global --max-fn-concurrent permit
  apply before the sandbox spins up, identically.
- **Ledger**: one invocations row per request, same outcome alphabet.
  A guest that returns without setting a response is `guest_error`.
- **Body caps**: request 10 MiB (existing); response capped at 10 MiB —
  symmetric — a streaming guest that exceeds it is `guest_error` with the
  cap named in the log (the public wire gets the v3.3 generic).
- **Stdio**: `wasi:cli` stdout/stderr are captured into fn_logs under the
  A2 caps (256 lines / 2 KiB / newest 2000 per ref) — ecosystem guests
  log with `println!`, and it lands where keel logs land.
- **Clocks/random**: real (functions are non-durable by design).
- **Filesystem**: not part of the proxy world; nothing is preopened.

## E4. Outbound HTTP — an operator grant, never a default

`wasi:http/proxy` imports `outgoing-handler`: the guest can originate
requests. On a tokenless-invocable public plane that is an SSRF machine,
so the posture mirrors effectful providers (v2.5 — capability = operator
grant):

- Default: outgoing-handler is linked to a stub that answers every
  request with an error-code — the guest sees a clean "outbound not
  granted" failure, never a hang.
- `POST /api/routes` (and apps) gain `"allow_outbound": true` — a
  per-ref grant, additive column via ensure_column, echoed by GET,
  settable only through the token-gated control plane.
- Granted refs use wasmtime-wasi-http's real outgoing implementation
  with a 30 s per-request cap (the engine's existing outbound norm).
  Outbound calls are NOT journaled (functions never journal) and NOT
  ledger rows (the ledger is sandbox outcomes); they are counted in a
  `keel_fn_outbound_total{ref=...}` metric.

## E5. wasi:keyvalue — the ecosystem half of Amendment 2

Proxy-world guests get `wasi:keyvalue/store` backed by the SAME fn_kv
table and caps as A7 (key 256 B, value 64 KiB, 1024 keys / 8 MiB per ref,
cap-check + write in one transaction, commit-before-return):

- Only the default bucket (`""`) exists in v4.0; `open` on any other name
  errs. Buckets are a namespace keel's per-ref scoping already provides —
  a second namespace layer is speculative until someone needs it.
- `get`/`set`/`delete`/`exists` map 1:1 onto the A7 store — a keel-WIT
  function and a wasi guest bound to the same ref see the SAME data.
  `list-keys` maps onto the keys listing with the same order.
- keel-WIT handler guests keep platform-api kv; wasi:keyvalue is linked
  only into the proxy world.

## E6. Fixtures — vendored wits, offline forever

Ecosystem fixtures must build OFFLINE (gate rule since phase 1), so the
`wasi:http`/`wasi:keyvalue` wit trees are VENDORED under the fixture
guests (`guests/proxy-*/wit/deps/...`), pinned to the 0.2.x revision
wasmtime 43 implements. Two fixtures:

- `guests/proxy-echo` (`wasi:http/proxy`): reflects method/path/body,
  prints one line to stdout (the fn_logs assertion), and exercises
  wasi:keyvalue with a counter key.
- `guests/proxy-out` (`wasi:http/proxy`): makes one outbound GET to the
  local stub (:18080) and relays the answer — the E4 fixture.

## E7. Acceptance — `scripts/accept_ecosystem.sh`

On a tokened engine, all offline:

1. Bind proxy-echo to `/fn/px` with no special flags → request roundtrips
   (status/body from the guest); the ledger row lands with outcome ok;
   stdout line appears in `/api/logs`; keel-WIT routes on the same engine
   still serve (both worlds, one dispatcher).
2. The wasi:keyvalue counter counts 1, 2, 3 → kill -9 → 4 (A7's
   durability through the wasi surface), and `GET /api/kv` lists the key.
3. proxy-out WITHOUT the grant → the guest's outbound errs (response
   surfaces the guest's own failure handling, not a hang); with
   `allow_outbound: true` re-bind → the stub's answer relays and
   `keel_fn_outbound_total` increments.
4. Quotas hold: proxy-echo bound with a starvation fuel limit → oof
   outcome in the ledger, 500 outcome body on the wire (the phase-5
   assertion, now for the proxy world).
5. A response larger than 10 MiB → guest_error, generic wire body.
6. Regression: the full suite stays green (no WIT change to keel worlds —
   guests do not even rebuild this time).
