# Amendment 4 — the ports & adapters program (hexagonal, made explicit)

Status: AUTHORED IN-REPO (2026-07-19), same discipline as Amendments
[1](SPEC-AMENDMENT-1.md)/[2](SPEC-AMENDMENT-2.md)/[3](SPEC-AMENDMENT-3.md):
spec first, code second. Origin: status.md §S "HEXAGONAL / PORTS & ADAPTERS"
shelf; user go-ahead "commit and start hexagonal".

Ships incrementally on the v4.x line — this is architecture, not a WIT bump,
so it ships ONE port at a time, each behind a gate, never as a big-bang
refactor. The first port (**secret-store**) is specified in full below and is
the definition-of-done for the first slice; the rest are named and deferred to
demand.

---

## H0. The thesis — Keel already won the hard half

Hexagonal architecture (ports & adapters) says: the application core depends on
ABSTRACTIONS (ports), and concrete infrastructure plugs in as ADAPTERS, so the
core is testable and the infrastructure is swappable without touching the core.

Keel already lives this on the GUEST side, and nobody says so out loud:

- A guest (the application core) has **zero ambient capability** — it reaches
  the world ONLY through host-defined ports (the WIT interfaces `host-api` /
  `platform-api` / `wasi:*`). That is the port boundary, enforced by the
  component model, not by convention.
- The **journal sits on that port**: every effect is a host call, so the port
  is exactly where durability, redaction, and metering attach.
- The **capability providers** (v2.2–v2.6) are already first-class,
  content-addressed, hot-swappable, dependency-inverted ADAPTERS plugged into
  `keel:provider`. That is the adapter SPI, shipped and gate-proven.

This amendment turns the SAME pattern INWARD: the engine's own infrastructure
dependencies (where secrets/blobs/rows/time/outbound-bytes come from) become
named **driven ports** with swappable adapters, so "one binary, one SQLite
file" is the DEFAULT adapter set, not a hardcoded fate.

## H1. The ports (named; most are deferred)

Driven ports (the engine depends on these; adapters supply them):

| Port | Today (the default adapter) | Second adapter that earns the port |
|---|---|---|
| **secret-store** | `--secrets-file` (KEY=VALUE file) | environment variables (12-factor / k8s) — **THIS SLICE** |
| durable-Store | rusqlite/SQLite (`db.rs`) | Postgres cell / libSQL replica / `:memory:` |
| outbound-http | ureq (`host.rs` do_http_request) + wasmtime-wasi-http (proxy) | egress allowlist (the refuted-SSRF hardening) / record-replay mock |
| blob-store | SQLite BLOBs (modules, assets) | S3 / filesystem / OCI (content-addressed — the hash is the key) |
| clock | `SystemTime`/`Instant` | a fake clock for deterministic tests |

Driving ports (these drive the engine; already plural, already proof the shape
works): the HTTP control plane, the CLI (`client.rs`), the **embedded library**
(the v2.2 `Engine` façade), cron, and (shelf) M-2 topics.

## H2. The contract-as-invariant rule (why this is safe)

A port's TRAIT CONTRACT is where the platform invariants live, so every adapter
inherits them or fails to compile/pass the gate. This is the discipline that
lets a cell swap SQLite for Postgres WITHOUT betraying the vision:

- secret-store: `get(name)` is re-read LIVE on every call (rotation detection —
  §2.1 salted-hash-on-replay — depends on it); the value NEVER touches the
  journal (the engine journals only name→salted-hash and redacts values from
  wire logs — that stays engine-side, above the port).
- durable-Store (when built): "one writer, journal-commit-before-return" is the
  contract; a Postgres adapter still takes one writer per workflow.
- outbound-http (when built): the egress policy is an adapter, applied to ALL
  paths, not a per-call flag.

## H3. The guardrail — extract at the SECOND adapter, never speculatively

The failure mode of hexagonal is a trait explosion for single-use code
(violates SPEC.md §0 simplicity). The rule, enforced for every port here:

> Extract a port ONLY when a second REAL adapter is demanded. Until then the
> concrete code stays concrete. Define the port at the moment of the second
> adapter.

secret-store qualifies NOW: environment-variable secrets are the 12-factor
standard and the deploy recipes (`docs/deploy/`: docker, k8s) make file-mounting
extra friction — a real, demanded second adapter. The other ports stay
concrete until their second adapter is demanded.

---

## The first slice — the secret-store port (definition of done)

### A. The port

`core/src/secrets.rs`, a new module:

```rust
/// The secret-store PORT. Adapters supply raw secret VALUES by name; all
/// journaling / salted-hash-on-replay / wire-redaction stays ABOVE this line
/// (host.rs), so no adapter can weaken the security posture.
///
/// CONTRACT: get() is re-read LIVE on every call — rotation detection depends
/// on it (§2.1). Ok(value) or Err(guest-visible reason, journaled as {"err"}).
pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Result<String, String>;
    /// One-line human description for the startup log / diagnostics.
    fn describe(&self) -> String;
}
```

### B. The adapters

- `FileSecretStore { path }` — the DEFAULT, refactored verbatim from today's
  `lookup_secret`/`load_secrets` (strict KEY=VALUE, re-read per call, dup/`=`
  errors preserved).
- `EnvSecretStore { prefix }` — the SECOND adapter: `get("API_KEY")` reads
  `std::env::var(format!("{prefix}API_KEY"))`. Default prefix `KEEL_SECRET_`.
  Re-read per call (rotation via a redeploy that changes the env). A name that
  is not a valid env-var tail simply does not resolve (an `Err`, i.e. DATA).
- `LayeredSecretStore(Vec<Arc<dyn SecretStore>>)` — composition: the first
  adapter that RESOLVES wins; errors fall through; the last error is returned
  if none resolve. File-before-env when both are configured (an explicit file
  overrides an ambient env var — the least-surprising precedence).
- `NoSecretStore` — nothing configured; `get` errs "no secret store configured
  on this engine" (the `secret()` call is guest-visible DATA either way).

### C. Wiring (surgical, behavior-preserving)

- `EngineShared.secrets_path: Option<String>` → `EngineShared.secrets:
  Arc<dyn SecretStore>`, built in `EngineShared::new` from the options.
- `EngineOptions` gains `secrets_env_prefix: Option<String>` beside the existing
  `secrets_path`. Construction: file? env? both→Layered; neither→NoSecretStore.
- `Ctx.secrets_path: Option<String>` → `Ctx.secrets: Arc<dyn SecretStore>`
  (cloned from shared in `runner.rs`); the `secret()` host call swaps
  `lookup_secret(self.secrets_path…)` for `self.secrets.get(&name)`. Everything
  downstream (salted hash, journal row, redaction, replay verify, "changed
  mid-workflow") is UNTOUCHED — the value's SOURCE is the only thing abstracted.
- `main.rs`: new flag `--secrets-env-prefix <PREFIX>` (env `KEEL_SECRETS_ENV_PREFIX`);
  keep `--secrets-file`. Startup still validates the file (fail fast) and now
  logs the resolved store's `describe()`.

### D. Acceptance

Extend `scripts/smoke_secrets.sh` (it must stay green verbatim for the file
adapter) with an env-adapter leg: start an engine with `--secrets-env-prefix`,
`KEEL_SECRET_<NAME>` in the environment, a guest reads the secret, the value is
redacted from journaled requests exactly as with the file adapter, and the
journal still stores name→salted-hash only (no raw value in the db). A layered
leg proves file-over-env precedence. No WIT change; existing guests untouched.

### E. Non-goals for this slice

Vault/AWS-Secrets-Manager/SOPS adapters (later, same trait), fleet per-tenant
env prefixes (file per-tenant already works), and the OTHER ports (Store,
outbound, blob, clock) — each waits for its own second adapter per H3.
