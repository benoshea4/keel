# Capability providers

How Keel grows effects the way Envoy grows filters: a **provider** is a wasm
component that extends the engine's effect surface without touching engine
code. Shipped in v2.2; gate: `scripts/smoke_providers.sh`.

## The model

```
guest ── provider-call(name, kind, request) ──▶ engine ── journaled ──▶ provider.handle(kind, request)
                                                  │
                                             journal row
                                        custom:<name>:<kind>
```

- A provider implements the [`keel:provider` world](provider-wit/provider.wit):
  one export, `handle(kind, request-json) → result<json, error>`. One provider
  may serve many kinds.
- Registered at startup: `keel serve --provider name=path.wasm` (repeatable;
  per-tenant `providers = ["name=path.wasm"]` in fleet configs). Compiled and
  type-checked **at boot** — a component that imports anything or lacks
  `handle` fails the start, never a workflow.
- Guests call `provider-call(name, kind, request)` (WIT 0.6.0). The engine
  journals it as `custom:<name>:<kind>` with the same
  commit-before-return rule as every effect — **replay returns the recorded
  response without instantiating the provider at all** (proven by the gate:
  kill -9 between two calls, the first is never re-invoked).

## The rules

1. **No imports, no ambient capabilities.** Stricter than guests: not even
   host-api. A provider is a pure request→response sandbox — it cannot reach
   clocks, sockets, files, or the journal. This is enforced by pre-flight
   (`instantiate_pre` against an empty linker), not convention.
2. **Stateless per call.** The engine instantiates a fresh instance per call
   (compilation is cached). Nothing persists between calls except what the
   *workflow* chooses to journal/kv.
3. **Bounded.** Same linear-memory cap as guests
   (`--max-guest-memory-mb`) and an epoch budget of ~10s of CPU per call — a
   spinning provider becomes a guest-visible `err`, never a pinned worker.
4. **Failures are data.** Unknown provider name, unknown kind, a trap, a
   blown budget — all come back to the guest as `err(string)`, journaled and
   replayed identically. An unregistered name stays an err on replay even if
   the provider has since been registered: determinism beats convenience.
5. **Secrets are redacted from the journaled request** (`{{secret:name}}`),
   same as http-request. The provider *response* is journaled verbatim — do
   not build providers that echo secrets back.

## Determinism guidance

Provider results are journaled, so a nondeterministic provider still replays
consistently (the recorded answer wins). But a provider that returns garbage
under memory pressure or wall-clock-dependent output makes workflows that are
*correct by accident*. Write providers as pure functions of (kind, request).

## Writing one

Copy `providers/greet/` (Cargo.toml carries the `[package.metadata.component]`
block pointing at `provider-wit/`):

```bash
cd providers/greet
cargo component build --release --target wasm32-unknown-unknown
keel serve --db keel.db --provider greet=target/wasm32-unknown-unknown/release/greet.wasm
```

Guest side:

```rust
let resp = host::provider_call("greet", "greet", r#"{"name":"keel"}"#)?;
```

## Future (deliberately not in v2.2)

- **Effectful providers**: an optional imported subset of host-api (http,
  kv) whose calls the engine journals *individually* under the provider's
  span. That turns providers into full connectors (Stripe, S3, SMTP) while
  keeping every wire effect replayed exactly once. Needs a design pass on
  seq allocation before it exists.
- **Content-addressed provider registry**: upload providers like modules
  instead of pointing at paths, so fleets can roll them without restarts.
