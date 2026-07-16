# Keel — vision

## Positioning: the SQLite of workflow engines

Every serious durable-execution system today — Temporal, Restate, Inngest,
Cloudflare Workflows — is a *cluster you talk to*: a server fleet, client SDKs,
an ops team. Keel is the other point in the design space: **durable execution
you just run**. One self-contained binary, one SQLite file, workflows as
sandboxed WASM components. No cluster, no sidecar, no SDK handshake — `keel
serve` and you have journaled, crash-proof, resumable, live-upgradable
workflows on a laptop, a $50 edge box, or a single VM.

The bet: an enormous amount of durable-execution demand looks like "inside my
app / on one machine / at one site," and nobody serves it — the same demand
SQLite found under the client-server databases.

What the WASM component model buys (and why guests aren't native plugins):
determinism enough to replay, a capability boundary the journal can sit on
(guests have *zero* ambient capabilities — every effect goes through the
journaled host API), language neutrality, and safe live code upgrade of a
running workflow's module.

## What Keel is not

Not a cluster (single writer, single node — by design). Not a general serverless
platform. Not an OS. Multi-node distribution is explicitly out; if you need a
fleet, you run many independent keels (see "edge" below).

## Roadmap

Done (v1.0–v1.1): the durable core — journal-before-return, replay recovery,
durable timers, external events, checkpoints + pruning, live v1→v2 upgrade,
cancel (park-loop + epoch interruption), htmx UI, operator token auth, guest
memory caps, offline acceptance gates, CI, release binaries.

- **v1.2 — real-world effects** (the adoption gate: today guests can only GET,
  sleep and wait — nobody can build a real workflow yet):
  `http-request` (method, headers, body, per-call timeout/retry) as a journaled
  call; journaled per-workflow KV (`kv-set`/`kv-get` — durable state without
  checkpoint gymnastics); cron/scheduled workflow starts. Stretch: a `secret`
  host call, once the journals-contain-what-guests-read tension has a written
  design.
- **v1.3 — operability:** workflow list API with pagination, metrics/OTel,
  retention/GC for terminal workflows, backup guidance (litestream), UI logout.
- **v2 — a platform, not just an engine:** split into library + binary so Keel
  embeds in-process (the positioning, literally); the WIT world becomes the
  extension surface — capability providers (event-source connectors, custom
  journaled effects) so Keel grows effects the way Envoy grows filters.

**On multi-tenancy** (asked often enough to answer here): never tenant columns
in one shared DB — that couples every tenant to a single SQLite writer lock and
one blast radius, and rewrites auth for a cloud product with no users yet.
Tenancy is **cells**: one keel process + one DB + one token per tenant, which
v1.1 already shapes (per-process token, per-guest caps, 8 MB binary). The thin
`keel fleet` supervisor that spawns/routes cells is v2 work, after the engine
is worth hosting.

## The two ideas we deliberately parked

**Edge:** not an "edge OS" — durable journals want one consistent writer and
distribution fights that. But keel binaries are ~8 MB and self-contained, so
*fleets of independent keels* (one per site/tenant) are the natural deployment
story, and that needs no new architecture.

**Cloud:** a hosted "serverless durable WASM" service is the eventual business
model, not the product. Keel's shape makes the credible architecture cheap:
one keel process + one SQLite file per tenant — cell isolation with nothing
shared. That gets built if and when the open engine earns adoption; the engine
stays MIT either way.
