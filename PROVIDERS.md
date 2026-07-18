# Capability providers

How Keel grows effects the way Envoy grows filters: a **provider** is a wasm
component that extends the engine's effect surface without touching engine
code. Pure tier shipped in v2.2 (`scripts/smoke_providers.sh`); effectful tier
in v2.5 (`scripts/smoke_providers_effectful.sh`).

## The model

```
guest ── provider-call(name, kind, request) ──▶ engine ──▶ provider.handle(kind, request)
                                                  │              │ (effectful only)
                                                  │              └─ host-http ──▶ wire
                                                  │                   │
                                                  │              journal row per wire call
                                                  │              provider-http:<name>
                                             terminal row
                                          custom:<name>:<kind>
```

- A provider implements one of the two [`keel:provider` worlds](provider-wit/provider.wit):
  one export, `handle(kind, request-json) → result<json, error>`. One provider
  may serve many kinds.
- The **tier is the operator's grant**, not the component's claim:
  `--provider name=path.wasm` registers the pure tier, `--provider-effectful
  name=path.wasm` the effectful one (both repeatable; per-tenant `providers =
  [...]` / `providers_effectful = [...]` in fleet configs; one shared name
  namespace). Compiled and type-checked **at boot** — a component whose
  imports exceed its grant fails the start, never a workflow.
- Guests call `provider-call(name, kind, request)` — unchanged since WIT
  0.6.0; the effectful tier required **no guest-side change at all**.

## The two tiers

1. **Pure (`--provider`, `world provider`).** No imports, no ambient
   capabilities — not even host-http: a pure request→response sandbox that
   cannot reach clocks, sockets, files, or the journal. Enforced by pre-flight
   (`instantiate_pre` against an empty linker), exactly as in v2.2; components
   built against `keel:provider@0.1.0` keep working without a rebuild. The
   whole call is one journal row (`custom:<name>:<kind>`), and **replay
   returns the recorded response without instantiating the provider at all.**
2. **Effectful (`--provider-effectful`, `world provider-effectful`).** May
   import `keel:provider/host-http` and make real HTTP calls — the connector
   tier (Stripe, S3, SMTP). Pre-flight admits imports **up to** host-http
   (a pure component under this grant is fine, it just journals no
   internals). Granting it means the provider can reach any URL the engine's
   host can reach: treat the grant like installing a plugin with network
   access, because that is what it is.

## Effectful semantics: nested durable functions

Every `host-http` call a provider makes is journaled **individually**, at its
own seq in the calling workflow's journal (kind `provider-http:<name>`),
inside the enclosing provider-call's scope; the `custom:<name>:<kind>`
terminal row commits last, when the provider returns. All the engine's
invariants apply per wire call — commit-before-return, redaction of the
journaled request (a secret the guest passed into the provider is redacted in
the provider's wire rows too), and the wire-only idempotency key
`<workflow-id>:<seq>` (each wire call has its own seq, so its own stable key).

What that buys, concretely (all gated):

- **Crash mid-provider** (engine killed between two wire calls): recovery
  re-invokes the provider, but its already-committed calls replay from their
  journal rows — the remote sees them **once**. Only the truly in-flight call
  re-sends, carrying the **same** idempotency key as the lost attempt, so a
  deduping remote collapses the at-least-once window entirely.
- **Crash after the provider returned**: replay fast-forwards the whole
  recorded scope — internals and terminal — without instantiating the
  provider at all, same as the pure tier.
- **Nondeterministic providers are caught**, same trap as guests: on a crash
  re-run the provider's calls are checked against the recorded rows
  (kind + request); divergence fails the workflow rather than corrupting it.

The provider's wasm re-runs from the top on a crash re-run (compute repeats;
wire effects do not) — so an effectful provider must be a deterministic
function of `(kind, request, responses-to-its-imports)`. Same rule as guests.

## The rules

1. **Stateless per call.** The engine instantiates a fresh instance per call
   (compilation is cached). Nothing persists between calls except what the
   *workflow* chooses to journal/kv.
2. **Bounded.** Same linear-memory cap as guests (`--max-guest-memory-mb`)
   and ~10s of **wasm** compute per call — for the effectful tier, time spent
   waiting on its wire calls is excused (each of those is bounded by the http
   timeout/retry caps instead). A spinning provider becomes a guest-visible
   `err`, never a pinned worker.
3. **Provider failures are data.** Unknown provider name, unknown kind, a
   trap, a blown budget, a transport error — all come back to the guest as
   `err(string)`, journaled and replayed identically. An unregistered name
   stays an err on replay even if the provider has since been registered.
   **The one exception:** a journal-integrity failure *inside* an effectful
   provider (nondeterministic replay, a database error) fails the workflow —
   corruption is never laundered into data.
4. **Don't change a provider's tier mid-workflow.** Recorded rows replay
   fine across a tier change only when the shape matches (a pure history
   under a new effectful grant is fine — the scan finds the terminal row
   immediately). An effectful history replayed under a pure grant trips the
   nondeterminism trap. Register the tier you mean to keep.
5. **Cancel latency.** A guest blocked in provider-call cannot be cancelled
   mid-call (same as any host call); an effectful provider's window is the sum
   of its wire calls' timeouts. Keep providers to a handful of calls.
6. **Secrets are redacted from journaled requests** (`{{secret:name}}`) —
   the provider-call request and the provider's wire rows both. Provider
   *responses* are journaled verbatim — do not build providers that echo
   secrets back.

## Writing one

Pure: copy `providers/greet/`. Effectful: copy `providers/relay/` (its
Cargo.toml targets `world = "provider-effectful"`):

```bash
cd providers/relay
cargo component build --release --target wasm32-unknown-unknown
keel serve --db keel.db \
  --provider-effectful relay=target/wasm32-unknown-unknown/release/relay.wasm
```

Provider side (generated bindings expose the import):

```rust
use bindings::keel::provider::host_http;
let resp = host_http::http_request("POST", &url, &[], Some(&body), 0, 0)?;
```

Guest side — unchanged:

```rust
let resp = host::provider_call("relay", "relay", r#"{"first_url": "..."}"#)?;
```

## The registry (v2.6)

Providers live in a **content-addressed registry** inside the database
(`scripts/smoke_provider_registry.sh` is the gate): blobs are immutable,
keyed by sha256; a *name* is a mutable pointer to `(tier, hash)`.

```bash
curl -X POST --data-binary @relay.wasm 'localhost:8080/api/providers?name=relay&tier=effectful'
curl 'localhost:8080/api/providers'                    # [{name, tier, hash, updated_at}]
curl -X POST 'localhost:8080/api/providers?name=relay&tier=effectful&hash=<old>'  # rollback, no bytes
curl -X DELETE 'localhost:8080/api/providers/relay'    # unbind (blob stays)
```

- **Pre-flight at the door**: the tier check runs at upload — a bad or
  tier-violating component is a 400, never a workflow failure.
- **The swap is live**: the next `provider-call` under the name uses the new
  component; no restart. A `/providers` UI page lists and manages bindings.
- **Replay beats the registry**: recorded journal rows are returned as-is, so
  rolling a provider never rewrites history — a recovering workflow replays
  the response the OLD version gave. (Corollary: a crash *mid-effectful-call*
  re-runs the provider on recovery — if you rolled it in between and the new
  version makes different wire calls, the nondeterminism trap fires. Roll
  when calls are not in flight, same discipline as module upgrades.)
- **Boot flags are an upload channel**: `--provider` / `--provider-effectful`
  validate eagerly (a bad flag still fails the boot) and then UPSERT into the
  registry — so since v2.6 flag-registered providers **persist across
  restarts**; removal is the DELETE endpoint, not dropping the flag. A stored
  blob that stops compiling under a future engine is logged and skipped at
  boot (calls err as unregistered) — never a bricked start.
- Fleets: each tenant has its own database, hence its own registry — roll a
  tenant's providers through that tenant's API, zero restarts.

## Future (deliberately not in v2.6)

- **host-kv for providers**: durable provider-scoped state. Needs a
  namespacing design (provider keys must not collide with the workflow's own
  kv) before it exists.
