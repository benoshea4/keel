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

- **v1.2 — real-world effects:** full `http-request` (verbs, headers, body) as a
  journaled call; journaled per-workflow KV; cron/scheduled workflow starts;
  per-call retry policy.
- **v1.3 — operability:** metrics/OTel, workflow list API with pagination,
  retention/GC for terminal workflows, backup guidance (litestream).
- **v2 — a platform, not just an engine:** the WIT world becomes the extension
  surface — capability providers (event-source connectors, custom journaled
  effects) so Keel grows effects the way Envoy grows filters.

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
