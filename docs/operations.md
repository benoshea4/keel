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
| `--retain-terminal-hours` | 0 (keep forever) | GC completed/failed workflows (journal, events, snapshots, kv included) after this many hours. |
| `--backup-dir` + `--backup-interval-secs` + `--backup-keep` | off / 300 / 24 | Periodic online snapshots (below). |
| `--secrets-file` | unset | KEY=VALUE file backing the `secret` host call (below). |
| `--provider name=path.wasm` | none | Register a capability provider (repeatable) — see [PROVIDERS.md](../PROVIDERS.md). Compiled + type-checked at boot; a bad provider fails the start. |

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
# retain_terminal_hours, backup_dir, backup_interval_secs, backup_keep,
# secrets_file (per-tenant secrets — cells never share one),
# providers = ["name=path.wasm", ...] (per-tenant capability providers)

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

## Monitoring

`GET /metrics` (Prometheus text, behind the same token):
`keel_workflows{status=...}` gauges and `keel_worker_threads`. Engine logs go
to stdout (`tracing`, INFO).

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
