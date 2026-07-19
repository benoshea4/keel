// db.rs — schema, migration, typed query helpers (SPEC.md §5).
//
// TWO HARD RULES (SPEC.md §0 cross-cutting + §5):
//
//   1. open_conn() is the ONLY way this codebase opens a SQLite connection.
//      busy_timeout and foreign_keys are PER-CONNECTION pragmas — an ad-hoc
//      rusqlite::Connection::open() elsewhere silently loses them and produces
//      "database is locked" bugs. Only WAL mode persists in the file itself.
//
//   2. set_status() is the ONLY statement that writes workflows.status/output
//      (create_workflow below is the one sanctioned INSERT). updated_at is NOT NULL
//      and scattered hand-written UPDATEs will forget it.
//
// Every SQL statement in the engine lives in this file so the schema story stays
// auditable in one place — with ONE deliberate exception: journal.rs, whose
// journaled() core the spec fixes verbatim (§6).
//
// The schema is complete for all 3 phases; future changes append, never ALTER.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::journal::now_ms;

pub fn open_conn(path: &str) -> Result<Connection> {
    let c = Connection::open(path)?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "busy_timeout", 5000)?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    Ok(c)
}

// §5 schema, verbatim. Idempotent; run once at startup.
const MIGRATION: &str = "
CREATE TABLE IF NOT EXISTS modules (
    hash        TEXT PRIMARY KEY,          -- lowercase hex sha256 of wasm bytes
    name        TEXT NOT NULL DEFAULT '',
    wasm        BLOB NOT NULL,
    created_at  INTEGER NOT NULL           -- unix millis
);

CREATE TABLE IF NOT EXISTS workflows (
    id          TEXT PRIMARY KEY,          -- uuid v4
    module_hash TEXT NOT NULL REFERENCES modules(hash),
    input       TEXT NOT NULL,             -- JSON
    status      TEXT NOT NULL,             -- 'running'|'sleeping'|'waiting_event'|'completed'|'failed'
    output      TEXT,                      -- JSON on completed; error string on failed
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS journal (
    workflow_id TEXT NOT NULL REFERENCES workflows(id),
    seq         INTEGER NOT NULL,          -- 0,1,2,... dense, per workflow
    kind        TEXT NOT NULL,
    request     TEXT NOT NULL,             -- JSON per SPEC.md §4.2
    response    TEXT NOT NULL,             -- JSON per SPEC.md §4.2
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (workflow_id, seq)
);

-- Task 2.2 additions (appended, never ALTERed).

CREATE TABLE IF NOT EXISTS timers (
    workflow_id TEXT PRIMARY KEY REFERENCES workflows(id),
    seq         INTEGER NOT NULL,
    wake_at     INTEGER NOT NULL            -- unix millis; FIXED once written
);

CREATE TABLE IF NOT EXISTS events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    workflow_id   TEXT NOT NULL REFERENCES workflows(id),
    name          TEXT NOT NULL,
    payload       TEXT NOT NULL,
    delivered     INTEGER NOT NULL DEFAULT 0,
    delivered_seq INTEGER,                  -- journal seq that consumed this event;
                                            -- REQUIRED so a phase-3 upgrade can
                                            -- un-deliver events whose journal rows
                                            -- fall in the discarded tail
    created_at    INTEGER NOT NULL
);

-- Task 3.2 addition (appended, never ALTERed).

CREATE TABLE IF NOT EXISTS snapshots (
    workflow_id TEXT PRIMARY KEY REFERENCES workflows(id),
    journal_seq INTEGER NOT NULL,   -- the checkpoint call's own seq (call it C)
    state       BLOB NOT NULL,
    module_hash TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

-- Post-review hardening: indexes for the two hot queries. Appended, never
-- ALTERed; IF NOT EXISTS retrofits them onto existing DBs at startup.

-- await-event's park loop runs this lookup every second per parked workflow;
-- rows within equal (workflow_id, name, delivered) sit in rowid order inside
-- the index, so the ORDER BY id LIMIT 1 needs no sort.
CREATE INDEX IF NOT EXISTS idx_events_undelivered
    ON events (workflow_id, name, delivered);

-- the dashboard sorts every workflow by recency on every 2s poll
CREATE INDEX IF NOT EXISTS idx_workflows_created_at
    ON workflows (created_at);

-- v1.2 additions (appended, never ALTERed).
-- v2.3 reshaped kv to APPEND-ONLY versions: one row per kv-set, seq = the
-- journal seq of the write. Reads resolve the highest seq; upgrade
-- tail-discard deletes rows with seq > C, which is what finally closes the
-- kv-vs-upgrade caveat (values roll back WITH the journal tail). Pre-v2.3
-- databases are reshaped in migrate() below.

CREATE TABLE IF NOT EXISTS kv (
    workflow_id TEXT NOT NULL REFERENCES workflows(id),
    key         TEXT NOT NULL,
    seq         INTEGER NOT NULL,           -- journal seq of the kv-set
    value       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (workflow_id, key, seq)
);

CREATE TABLE IF NOT EXISTS schedules (
    id           TEXT PRIMARY KEY,          -- uuid v4
    module_hash  TEXT NOT NULL REFERENCES modules(hash),
    input        TEXT NOT NULL,             -- JSON handed to every spawned workflow
    interval_ms  INTEGER NOT NULL,          -- 0 when cron-driven (v2.1)
    next_run_at  INTEGER NOT NULL,          -- unix millis
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    cron         TEXT                       -- v2.1: 6-field expr; NULL = interval
);

-- v2.6: the content-addressed provider registry (PROVIDERS.md). Blobs are
-- immutable and keyed by sha256; a name is a mutable pointer to (tier, hash).
-- Old blobs are kept on rebind/delete — rollback is a rebind, no re-upload.
CREATE TABLE IF NOT EXISTS provider_blobs (
    hash        TEXT PRIMARY KEY,           -- sha256 hex of wasm
    wasm        BLOB NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS providers (
    name        TEXT PRIMARY KEY,
    effectful   INTEGER NOT NULL,           -- the operator-granted tier
    hash        TEXT NOT NULL REFERENCES provider_blobs(hash),
    updated_at  INTEGER NOT NULL
);

-- Micro-cloud phases 4-6 (ext spec §E3). routes binds URL prefixes to
-- function components with per-route quotas; invocations is the usage ledger
-- (EVERY function/solver run writes a row, failures included); problems/
-- cases/submissions are the phase-5 judge; apps/assets are phase-6 hosted
-- full-stack apps.
CREATE TABLE IF NOT EXISTS routes (
    prefix       TEXT PRIMARY KEY,          -- e.g. '/fn/echo'; longest-prefix match wins
    module_hash  TEXT NOT NULL REFERENCES modules(hash),
    fuel_limit   INTEGER NOT NULL DEFAULT 500000000,
    mem_limit    INTEGER NOT NULL DEFAULT 67108864,
    time_limit_ms INTEGER NOT NULL DEFAULT 5000,
    created_at   INTEGER NOT NULL,
    rate_limit   INTEGER                    -- Amendment 1: max admitted runs per
                                            -- rolling 60s; NULL = unlimited
);
CREATE TABLE IF NOT EXISTS invocations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,              -- 'function' | 'app' | 'solve'
    ref         TEXT NOT NULL,              -- route prefix, app name, or submission id
    module_hash TEXT NOT NULL,
    outcome     TEXT NOT NULL,              -- 'ok'|'guest_error'|'tle'|'mle'|'oof'|'trap'
    fuel_used   INTEGER,
    peak_mem    INTEGER,
    duration_ms INTEGER NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS problems (
    slug        TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    statement   TEXT NOT NULL               -- markdown, rendered as <pre> (no md dep)
);
CREATE TABLE IF NOT EXISTS cases (
    problem     TEXT NOT NULL REFERENCES problems(slug),
    idx         INTEGER NOT NULL,
    input       TEXT NOT NULL,
    expected    TEXT NOT NULL,              -- exact string match after trim
    PRIMARY KEY (problem, idx)
);
CREATE TABLE IF NOT EXISTS submissions (
    id          TEXT PRIMARY KEY,           -- uuid
    problem     TEXT NOT NULL REFERENCES problems(slug),
    module_hash TEXT NOT NULL REFERENCES modules(hash),
    verdict     TEXT,                       -- NULL while judging; then AC|WA|TLE|MLE|RE|OOF
    detail      TEXT,                       -- JSON array of per-case results
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS apps (
    name         TEXT PRIMARY KEY,          -- [a-z0-9-]+, validated at the API
    backend_hash TEXT REFERENCES modules(hash),  -- nullable: static-only apps allowed
    created_at   INTEGER NOT NULL,
    rate_limit   INTEGER                    -- Amendment 1: like routes.rate_limit,
                                            -- for the app's api/* backend calls
);
CREATE TABLE IF NOT EXISTS assets (
    app          TEXT NOT NULL REFERENCES apps(name),
    path         TEXT NOT NULL,             -- 'index.html', 'pkg/app_bg.wasm', ...
    content_type TEXT NOT NULL,
    bytes        BLOB NOT NULL,
    etag         TEXT,                      -- v3.4 (R.1): sha256 hex of bytes, set at
                                            -- upsert; NULL only on pre-v3.4 rows
                                            -- (re-upload heals them)
    PRIMARY KEY (app, path)
);

-- Amendment 2 (v3.5): per-ref operator config (A6) and durable function KV
-- (A7). Both keyed by the (kind, ref) identity the ledger/logs/limits use.
-- Caps live in function.rs (the host impl); lifecycle: unbinding a ref does
-- NOT cascade either table — config is operator intent, kv is guest state;
-- explicit removal via DELETE /api/config and DELETE /api/kv.
CREATE TABLE IF NOT EXISTS fn_config (
    kind       TEXT NOT NULL,               -- 'function' | 'app'
    ref        TEXT NOT NULL,
    name       TEXT NOT NULL,
    value      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (kind, ref, name)
);
CREATE TABLE IF NOT EXISTS fn_kv (
    kind       TEXT NOT NULL,
    ref        TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (kind, ref, key)
);

-- Amendment 1 (A2): captured platform-api log lines from function/app runs,
-- batch-written after each run with the ledger rowid that produced them.
-- Host-side caps (256 lines/run, 2048 B/line, last 2000 rows per ref) live in
-- function.rs; time-based retention is --retain-ledger-hours.
CREATE TABLE IF NOT EXISTS fn_logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT NOT NULL,            -- 'function' | 'app'
    ref           TEXT NOT NULL,            -- route prefix or app name
    invocation_id INTEGER,                  -- invocations.id; NULL never happens
                                            -- today but the ledger row can be
                                            -- GC'd out from under old lines
    line          TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_fn_logs_ref ON fn_logs (kind, ref, id);

-- Amendment 1 (A1): the rate-limit admission COUNT runs per request on
-- limited refs — keep it off a table scan.
CREATE INDEX IF NOT EXISTS idx_invocations_admit ON invocations (kind, ref, created_at);
";

pub fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(MIGRATION)?;
    // v2.1 — the ONE sanctioned way an existing table evolves in place: an
    // additive, nullable column, retrofitted onto older databases here.
    ensure_column(c, "schedules", "cron", "TEXT")?;
    // Amendment 1 (A1) — rate limits; NULL = unlimited, so retrofitted
    // databases keep behaving exactly as before.
    ensure_column(c, "routes", "rate_limit", "INTEGER")?;
    ensure_column(c, "apps", "rate_limit", "INTEGER")?;
    // v3.4 (R.1) — conditional-GET support on stored assets.
    ensure_column(c, "assets", "etag", "TEXT")?;
    // v2.3 — kv went append-only (versioned). A pre-v2.3 kv table (no seq
    // column) is reshaped once: existing values become version 0, which any
    // later write out-versions. One transaction; idempotent by the seq check.
    if !has_column(c, "kv", "seq")? {
        c.execute_batch(
            "BEGIN;
             CREATE TABLE kv_v2 (
                 workflow_id TEXT NOT NULL REFERENCES workflows(id),
                 key         TEXT NOT NULL,
                 seq         INTEGER NOT NULL,
                 value       TEXT NOT NULL,
                 created_at  INTEGER NOT NULL,
                 PRIMARY KEY (workflow_id, key, seq)
             );
             INSERT INTO kv_v2 SELECT workflow_id, key, 0, value, updated_at FROM kv;
             DROP TABLE kv;
             ALTER TABLE kv_v2 RENAME TO kv;
             COMMIT;",
        )?;
    }
    Ok(())
}

fn has_column(c: &Connection, table: &str, col: &str) -> Result<bool> {
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let present = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|n| n == col);
    Ok(present)
}

/// ALTER TABLE ... ADD COLUMN, only if the column is missing — CREATE TABLE IF
/// NOT EXISTS won't touch tables that already exist, so additive columns need
/// this second pass on databases created before the column landed.
fn ensure_column(c: &Connection, table: &str, col: &str, decl: &str) -> Result<()> {
    if !has_column(c, table, col)? {
        c.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])?;
    }
    Ok(())
}

/// The single write path for workflows.status/output. Statuses:
/// 'running'|'sleeping'|'waiting_event'|'completed'|'failed'. Which transitions are
/// legal is runner.rs's business (§5) — this function just guarantees updated_at.
pub fn set_status(c: &Connection, id: &str, status: &str, output: Option<&str>) -> Result<()> {
    c.execute(
        "UPDATE workflows SET status = ?2, output = ?3, updated_at = ?4 WHERE id = ?1",
        rusqlite::params![id, status, output, now_ms()],
    )?;
    Ok(())
}

/// The one sanctioned INSERT into workflows. Rows are born 'running' BEFORE
/// runner::spawn is called: if the process dies between this INSERT and the spawn,
/// the startup recovery scan (main.rs) picks the workflow up. Crash-safe by ordering.
pub fn create_workflow(c: &Connection, id: &str, module_hash: &str, input_json: &str) -> Result<()> {
    let t = now_ms();
    c.execute(
        "INSERT INTO workflows (id, module_hash, input, status, output, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'running', NULL, ?4, ?4)",
        rusqlite::params![id, module_hash, input_json, t],
    )?;
    Ok(())
}

// --- v2.6: provider registry -------------------------------------------------

/// Upload path: store the (immutable, content-addressed) blob and point the
/// name at it, one transaction. INSERT OR IGNORE on the blob makes re-upload
/// of identical bytes a no-op, like modules.
pub fn upsert_provider(
    c: &Connection,
    name: &str,
    effectful: bool,
    hash: &str,
    wasm: &[u8],
) -> Result<()> {
    let t = now_ms();
    c.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> Result<()> {
        c.execute(
            "INSERT OR IGNORE INTO provider_blobs (hash, wasm, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![hash, wasm, t],
        )?;
        c.execute(
            "INSERT INTO providers (name, effectful, hash, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET effectful=?2, hash=?3, updated_at=?4",
            rusqlite::params![name, effectful as i64, hash, t],
        )?;
        Ok(())
    })();
    match r {
        Ok(()) => c.execute_batch("COMMIT")?,
        Err(_) => c.execute_batch("ROLLBACK")?,
    }
    r
}

/// Rollback path helper: fetch a stored blob by content address (None = no
/// such hash). The caller pre-flights it and then binds via upsert_provider —
/// pre-flight BEFORE bind, so a failed compile never moves the pointer.
pub fn get_provider_blob(c: &Connection, hash: &str) -> Result<Option<Vec<u8>>> {
    Ok(c.query_row(
        "SELECT wasm FROM provider_blobs WHERE hash = ?1",
        rusqlite::params![hash],
        |r| r.get(0),
    )
    .optional()?)
}

/// (name, effectful, hash, updated_at) for every binding, name order.
pub fn list_providers(c: &Connection) -> Result<Vec<(String, bool, String, i64)>> {
    let mut stmt =
        c.prepare("SELECT name, effectful, hash, updated_at FROM providers ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? != 0,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Unbind a name (the blob stays, for rebind). true = it existed.
pub fn delete_provider(c: &Connection, name: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM providers WHERE name = ?1", rusqlite::params![name])? > 0)
}

/// One boot-load row: (name, effectful, hash, wasm).
pub type ProviderRegistryRow = (String, bool, String, Vec<u8>);

/// Everything the boot needs to build the in-memory registry.
pub fn load_provider_registry(c: &Connection) -> Result<Vec<ProviderRegistryRow>> {
    let mut stmt = c.prepare(
        "SELECT p.name, p.effectful, p.hash, b.wasm FROM providers p
         JOIN provider_blobs b ON b.hash = p.hash ORDER BY p.name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? != 0,
            r.get::<_, String>(2)?,
            r.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Modules are content-addressed by sha256 → INSERT OR IGNORE makes re-upload a no-op
/// (and makes mutating an uploaded module impossible, which replay correctness needs).
pub fn insert_module(c: &Connection, hash: &str, name: &str, wasm: &[u8]) -> Result<()> {
    c.execute(
        "INSERT OR IGNORE INTO modules (hash, name, wasm, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![hash, name, wasm, now_ms()],
    )?;
    Ok(())
}

// --- UI list queries (Task 2.8) ------------------------------------------------

pub struct WorkflowListRow {
    pub id: String,
    pub module_name: String,
    pub module_hash: String,
    pub status: String,
    pub updated_at: i64,
}

/// Dashboard rows, newest first.
pub fn list_workflows(c: &Connection) -> Result<Vec<WorkflowListRow>> {
    let mut stmt = c.prepare(
        "SELECT w.id, m.name, w.module_hash, w.status, w.updated_at
         FROM workflows w JOIN modules m ON m.hash = w.module_hash
         ORDER BY w.created_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(WorkflowListRow {
                id: r.get(0)?,
                module_name: r.get(1)?,
                module_hash: r.get(2)?,
                status: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct ModuleRow {
    pub hash: String,
    pub name: String,
    pub created_at: i64,
}

pub fn list_modules(c: &Connection) -> Result<Vec<ModuleRow>> {
    let mut stmt =
        c.prepare("SELECT hash, name, created_at FROM modules ORDER BY created_at DESC")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ModuleRow {
                hash: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_module_name(c: &Connection, hash: &str) -> Result<Option<String>> {
    Ok(c
        .query_row("SELECT name FROM modules WHERE hash = ?1", [hash], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn module_exists(c: &Connection, hash: &str) -> Result<bool> {
    let row: Option<i64> = c
        .query_row("SELECT 1 FROM modules WHERE hash = ?1", [hash], |r| r.get(0))
        .optional()?;
    Ok(row.is_some())
}

pub fn get_module_wasm(c: &Connection, hash: &str) -> Result<Option<Vec<u8>>> {
    Ok(c.query_row("SELECT wasm FROM modules WHERE hash = ?1", [hash], |r| r.get(0))
        .optional()?)
}

pub struct WorkflowRow {
    pub id: String,
    pub module_hash: String,
    pub input: String,
    pub status: String,
    pub output: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The public JSON shape of a workflow — GET /api/workflows/:id AND the
/// platform-api `get-workflow` host call return exactly this (micro-cloud
/// Task 4.2: "share the serializer, don't duplicate it"). `output` stays a
/// JSON *string* (or null), never re-parsed — same contract as the API.
pub fn workflow_json(wf: &WorkflowRow) -> serde_json::Value {
    serde_json::json!({
        "id": wf.id,
        "status": wf.status,
        "output": wf.output,
        "module_hash": wf.module_hash,
        "created_at": wf.created_at,
        "updated_at": wf.updated_at,
    })
}

pub fn get_workflow(c: &Connection, id: &str) -> Result<Option<WorkflowRow>> {
    Ok(c
        .query_row(
            "SELECT id, module_hash, input, status, output, created_at, updated_at
             FROM workflows WHERE id = ?1",
            [id],
            |r| {
                Ok(WorkflowRow {
                    id: r.get(0)?,
                    module_hash: r.get(1)?,
                    input: r.get(2)?,
                    status: r.get(3)?,
                    output: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        )
        .optional()?)
}

pub struct JournalRow {
    pub seq: i64,
    pub kind: String,
    pub request: String,
    pub response: String,
    pub created_at: i64,
}

pub fn journal_rows(c: &Connection, workflow_id: &str) -> Result<Vec<JournalRow>> {
    let mut stmt = c.prepare(
        "SELECT seq, kind, request, response, created_at
         FROM journal WHERE workflow_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map([workflow_id], |r| {
            Ok(JournalRow {
                seq: r.get(0)?,
                kind: r.get(1)?,
                request: r.get(2)?,
                response: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- Raw journal access (Tasks 2.4/2.5) --------------------------------------
// journaled() (journal.rs) covers plain request→response effects. The park-loop
// host calls (durable sleep, await-event) need the same replay check and commit
// with WAITING in between, so they use these helpers instead of the closure
// wrapper. The invariant is unchanged: the row commits BEFORE the result reaches
// the guest, and the replay check happens before any effect.

/// (kind, request, response) recorded at (workflow_id, seq), if any.
pub fn get_journal_row(
    c: &Connection,
    workflow_id: &str,
    seq: i64,
) -> Result<Option<(String, String, String)>> {
    Ok(c
        .query_row(
            "SELECT kind, request, response FROM journal
             WHERE workflow_id = ?1 AND seq = ?2",
            rusqlite::params![workflow_id, seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

pub fn insert_journal_row(
    c: &Connection,
    workflow_id: &str,
    seq: i64,
    kind: &str,
    request: &str,
    response: &str,
) -> Result<()> {
    c.execute(
        "INSERT INTO journal (workflow_id, seq, kind, request, response, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![workflow_id, seq, kind, request, response, now_ms()],
    )?;
    Ok(())
}

// --- Timers (Task 2.4 durable sleep) ------------------------------------------

/// The workflow's durable wake deadline, if it is (or was, pre-crash) sleeping.
/// At most one row per workflow (PRIMARY KEY workflow_id).
pub fn get_timer_wake_at(c: &Connection, workflow_id: &str) -> Result<Option<i64>> {
    Ok(c
        .query_row(
            "SELECT wake_at FROM timers WHERE workflow_id = ?1",
            [workflow_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn insert_timer(c: &Connection, workflow_id: &str, seq: i64, wake_at: i64) -> Result<()> {
    c.execute(
        "INSERT INTO timers (workflow_id, seq, wake_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![workflow_id, seq, wake_at],
    )?;
    Ok(())
}

pub fn delete_timer(c: &Connection, workflow_id: &str) -> Result<()> {
    c.execute("DELETE FROM timers WHERE workflow_id = ?1", [workflow_id])?;
    Ok(())
}

/// The wake-up side of durable sleep (Task 2.4) in ONE transaction: drop the
/// timer, journal the sleep, flip status back to running. These were three
/// auto-commit statements once; a crash between the first two silently turned
/// the remainder-sleep guarantee into a full re-sleep. All three writes still
/// go through the single-purpose helpers — set_status stays the only status
/// writer, it just runs against the transaction.
pub fn finish_sleep(c: &mut Connection, workflow_id: &str, seq: i64, req_json: &str) -> Result<()> {
    let tx = c.transaction()?;
    delete_timer(&tx, workflow_id)?;
    insert_journal_row(&tx, workflow_id, seq, "sleep-ms", req_json, "{}")?;
    set_status(&tx, workflow_id, "running", None)?;
    tx.commit()?;
    Ok(())
}

/// The cancel endpoint's terminal write in ONE transaction: clear any parked
/// timer and mark the workflow failed with the cancellation note. Runs only
/// after the worker thread is gone (api.rs aborts and joins it first).
pub fn finish_cancel(c: &mut Connection, workflow_id: &str, note: &str) -> Result<()> {
    let tx = c.transaction()?;
    delete_timer(&tx, workflow_id)?;
    set_status(&tx, workflow_id, "failed", Some(note))?;
    tx.commit()?;
    Ok(())
}

// --- Events (Task 2.5 external events) -----------------------------------------

/// API side: queue an event for the workflow. It stays undelivered until an
/// await-event call with a matching name consumes it (FIFO by rowid).
pub fn insert_event(c: &Connection, workflow_id: &str, name: &str, payload: &str) -> Result<()> {
    c.execute(
        "INSERT INTO events (workflow_id, name, payload, delivered, delivered_seq, created_at)
         VALUES (?1, ?2, ?3, 0, NULL, ?4)",
        rusqlite::params![workflow_id, name, payload, now_ms()],
    )?;
    Ok(())
}

/// Host side of await-event: in ONE transaction, consume the oldest undelivered
/// matching event AND journal its delivery. The atomicity is MANDATORY (SPEC.md
/// Task 2.5): a crash between "delivered=1" and the journal INSERT silently loses
/// the event; the reverse order would deliver it twice. `req_json` is passed in
/// (not rebuilt here) so the journal row's request field is byte-identical to what
/// host.rs's replay check verifies against. Returns the payload, or None when no
/// matching event is queued (caller parks and retries). The flip back to running
/// rides in the same transaction (post-review hardening — it used to be a
/// separate write that a crash could skip, leaving a lying waiting_event).
pub fn deliver_event_and_journal(
    c: &mut Connection,
    workflow_id: &str,
    seq: i64,
    name: &str,
    req_json: &str,
) -> Result<Option<String>> {
    let tx = c.transaction()?;
    let found: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, payload FROM events
             WHERE workflow_id = ?1 AND name = ?2 AND delivered = 0
             ORDER BY id LIMIT 1",
            rusqlite::params![workflow_id, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((event_id, payload)) = found else {
        return Ok(None); // tx drops → rollback of nothing
    };
    tx.execute(
        "UPDATE events SET delivered = 1, delivered_seq = ?2 WHERE id = ?1",
        rusqlite::params![event_id, seq],
    )?;
    // §4.2 response shape: {"payload": "<the event's payload JSON text>"}.
    let resp_json = serde_json::to_string(&serde_json::json!({ "payload": payload }))?;
    tx.execute(
        "INSERT INTO journal (workflow_id, seq, kind, request, response, created_at)
         VALUES (?1, ?2, 'await-event', ?3, ?4, ?5)",
        rusqlite::params![workflow_id, seq, req_json, resp_json, now_ms()],
    )?;
    set_status(&tx, workflow_id, "running", None)?;
    tx.commit()?;
    Ok(Some(payload))
}

// --- KV (v1.2; append-only versions since v2.3) ------------------------------------
// Same discipline as event delivery: the state write and its journal row are ONE
// transaction, and the caller (host.rs) runs the replay check first.

/// kv-set's live path: APPEND a version row (seq = this call's journal seq)
/// and journal the call atomically. Never updates in place — versioning is
/// what lets an upgrade's tail-discard roll values back (v2.3).
pub fn kv_set_and_journal(
    c: &mut Connection,
    workflow_id: &str,
    seq: i64,
    key: &str,
    value: &str,
    req_json: &str,
) -> Result<()> {
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO kv (workflow_id, key, seq, value, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![workflow_id, key, seq, value, now_ms()],
    )?;
    insert_journal_row(&tx, workflow_id, seq, "kv-set", req_json, "{}")?;
    tx.commit()?;
    Ok(())
}

/// kv-get's live path: read the HIGHEST version of the key and journal what
/// was read atomically — replay then returns the recorded value even if more
/// versions land later. (Live reads only ever happen at the execution head,
/// so highest version ≡ current value.)
pub fn kv_get_and_journal(
    c: &mut Connection,
    workflow_id: &str,
    seq: i64,
    key: &str,
    req_json: &str,
) -> Result<Option<String>> {
    let tx = c.transaction()?;
    let v: Option<String> = tx
        .query_row(
            "SELECT value FROM kv WHERE workflow_id = ?1 AND key = ?2
             ORDER BY seq DESC LIMIT 1",
            rusqlite::params![workflow_id, key],
            |r| r.get(0),
        )
        .optional()?;
    let resp = serde_json::to_string(&serde_json::json!({ "v": v }))?;
    insert_journal_row(&tx, workflow_id, seq, "kv-get", req_json, &resp)?;
    tx.commit()?;
    Ok(v)
}

/// Latest value per key for a workflow (the UI/read-model view of kv).
pub fn kv_latest(c: &Connection, workflow_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = c.prepare(
        "SELECT key, value FROM kv k
         WHERE workflow_id = ?1
           AND seq = (SELECT MAX(seq) FROM kv k2
                      WHERE k2.workflow_id = k.workflow_id AND k2.key = k.key)
         ORDER BY key",
    )?;
    let rows = stmt
        .query_map([workflow_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- Schedules (v1.2) --------------------------------------------------------------

pub struct ScheduleRow {
    pub id: String,
    pub module_hash: String,
    pub input: String,
    pub interval_ms: i64,
    pub next_run_at: i64,
    pub enabled: bool,
    /// v2.1 — 6-field cron expression; None = plain interval schedule.
    pub cron: Option<String>,
}

const SCHEDULE_COLS: &str = "id, module_hash, input, interval_ms, next_run_at, enabled, cron";

fn schedule_from_row(r: &rusqlite::Row) -> rusqlite::Result<ScheduleRow> {
    Ok(ScheduleRow {
        id: r.get(0)?,
        module_hash: r.get(1)?,
        input: r.get(2)?,
        interval_ms: r.get(3)?,
        next_run_at: r.get(4)?,
        enabled: r.get::<_, i64>(5)? != 0,
        cron: r.get(6)?,
    })
}

pub fn insert_schedule(
    c: &Connection,
    id: &str,
    module_hash: &str,
    input_json: &str,
    interval_ms: i64,
    cron: Option<&str>,
    first_run_at: i64,
) -> Result<()> {
    c.execute(
        "INSERT INTO schedules (id, module_hash, input, interval_ms, next_run_at, enabled, created_at, cron)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
        rusqlite::params![id, module_hash, input_json, interval_ms, first_run_at, now_ms(), cron],
    )?;
    Ok(())
}

pub fn list_schedules(c: &Connection) -> Result<Vec<ScheduleRow>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {SCHEDULE_COLS} FROM schedules ORDER BY created_at DESC"
    ))?;
    let rows = stmt
        .query_map([], schedule_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete_schedule(c: &Connection, id: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM schedules WHERE id = ?1", [id])? > 0)
}

/// v2.1 — PATCH /api/schedules/{id}: pause/resume firing. Returns false for an
/// unknown id. Re-enabling leaves next_run_at alone: an interval schedule
/// fires once for the paused gap (the collapse math), a cron schedule at its
/// next expression match.
pub fn set_schedule_enabled(c: &Connection, id: &str, enabled: bool) -> Result<bool> {
    Ok(c.execute(
        "UPDATE schedules SET enabled = ?2 WHERE id = ?1",
        rusqlite::params![id, enabled as i64],
    )? > 0)
}

/// Enabled schedules whose next_run_at has passed.
pub fn due_schedules(c: &Connection, now: i64) -> Result<Vec<ScheduleRow>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {SCHEDULE_COLS} FROM schedules WHERE enabled = 1 AND next_run_at <= ?1"
    ))?;
    let rows = stmt
        .query_map([now], schedule_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Fire one due schedule: create its workflow AND move next_run_at to
/// `next_run_at` (computed by the caller — cron::next_run, one decision point
/// for both kinds) in ONE transaction. The workflow row doubles as the intent
/// record — a crash leaves either "not fired" (still due next pass) or "fired
/// and advanced" (recovery starts the created workflow). A create→advance gap
/// would double-fire.
pub fn fire_schedule(
    c: &mut Connection,
    s: &ScheduleRow,
    workflow_id: &str,
    next_run_at: i64,
) -> Result<()> {
    let tx = c.transaction()?;
    create_workflow(&tx, workflow_id, &s.module_hash, &s.input)?;
    tx.execute(
        "UPDATE schedules SET next_run_at = ?2 WHERE id = ?1",
        rusqlite::params![s.id, next_run_at],
    )?;
    tx.commit()?;
    Ok(())
}

// --- Listing + retention (v1.3) ----------------------------------------------------

/// Paged workflow listing for the API; optional status filter.
pub fn list_workflows_page(
    c: &Connection,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<WorkflowRow>> {
    let mut stmt = c.prepare(
        "SELECT id, module_hash, input, status, output, created_at, updated_at
         FROM workflows
         WHERE (?1 IS NULL OR status = ?1)
         ORDER BY created_at DESC LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![status, limit, offset], |r| {
            Ok(WorkflowRow {
                id: r.get(0)?,
                module_hash: r.get(1)?,
                input: r.get(2)?,
                status: r.get(3)?,
                output: r.get(4)?,
                created_at: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Retention GC: drop terminal (completed/failed) workflows untouched since
/// `cutoff_ms`, with every dependent row, in ONE transaction. Returns how many
/// workflows were removed. Live workflows are never eligible.
pub fn gc_terminal_workflows(c: &mut Connection, cutoff_ms: i64) -> Result<usize> {
    let tx = c.transaction()?;
    let ids: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM workflows
             WHERE status IN ('completed','failed') AND updated_at < ?1",
        )?;
        let ids = stmt
            .query_map([cutoff_ms], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids
    };
    for id in &ids {
        for table in ["journal", "timers", "events", "snapshots", "kv"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE workflow_id = ?1"),
                [id],
            )?;
        }
        tx.execute("DELETE FROM workflows WHERE id = ?1", [id])?;
    }
    tx.commit()?;
    Ok(ids.len())
}

/// Status counts for /metrics.
pub fn status_counts(c: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = c.prepare("SELECT status, COUNT(*) FROM workflows GROUP BY status")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- Snapshots (Task 3.3 checkpoint / Task 3.4 resume) ---------------------------

/// The exec side of the checkpoint host call, in ONE transaction: persist the
/// state blob (keyed by workflow, replacing any older snapshot) and prune every
/// journal row strictly below the checkpoint's own seq C. The snapshot copies the
/// workflow's CURRENT module_hash — Task 3.4 asserts it still matches at resume,
/// and the upgrade endpoint (3.6) rewrites both in step 4.
/// Invariant afterwards: journal = row C (inserted by the journaled() wrapper,
/// in a separate, later transaction) plus any rows > C.
pub fn snapshot_and_prune(
    c: &mut Connection,
    workflow_id: &str,
    c_seq: i64,
    state: &[u8],
) -> Result<()> {
    let tx = c.transaction()?;
    let n = tx.execute(
        "INSERT OR REPLACE INTO snapshots (workflow_id, journal_seq, state, module_hash, created_at)
         SELECT id, ?2, ?3, module_hash, ?4 FROM workflows WHERE id = ?1",
        rusqlite::params![workflow_id, c_seq, state, now_ms()],
    )?;
    anyhow::ensure!(n == 1, "workflow row missing during checkpoint");
    tx.execute(
        "DELETE FROM journal WHERE workflow_id = ?1 AND seq < ?2",
        rusqlite::params![workflow_id, c_seq],
    )?;
    // v2.3 — kv compaction rides the checkpoint: superseded versions can
    // never be read again (live reads take the highest version; an upgrade
    // discards only seq > C_snapshot, and the surviving latest version is
    // always ≤ the snapshot's C, because writes after it out-version it only
    // at seqs a later tail-discard would remove together with this row's
    // supersessors). Long-lived workflows stop accumulating dead versions.
    tx.execute(
        "DELETE FROM kv WHERE workflow_id = ?1 AND seq < (
             SELECT MAX(seq) FROM kv AS k2
             WHERE k2.workflow_id = ?1 AND k2.key = kv.key
         )",
        rusqlite::params![workflow_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub struct SnapshotRow {
    pub journal_seq: i64,
    pub state: Vec<u8>,
    pub module_hash: String,
}

/// Task 3.4 — the runner checks this before every start: a snapshot switches the
/// start path from call_run(input) at seq 0 to call_resume(state) at seq C+1.
pub fn get_snapshot(c: &Connection, workflow_id: &str) -> Result<Option<SnapshotRow>> {
    Ok(c
        .query_row(
            "SELECT journal_seq, state, module_hash FROM snapshots WHERE workflow_id = ?1",
            [workflow_id],
            |r| {
                Ok(SnapshotRow {
                    journal_seq: r.get(0)?,
                    state: r.get(1)?,
                    module_hash: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Task 3.6 step 4, in ONE transaction: discard everything the workflow executed
/// beyond its checkpoint C, then point workflow + snapshot at the new module.
/// Un-delivering events whose delivery row fell in the discarded tail
/// (delivered_seq > C) is what lets the re-executed await-event find them again —
/// without it the workflow would wait forever on an already-consumed event.
/// Undelivered events are untouched. The workflows UPDATE bypasses set_status on
/// purpose (it changes module_hash, not status, and MUST sit in this txn) but
/// still maintains updated_at.
pub fn upgrade_module_txn(
    c: &mut Connection,
    workflow_id: &str,
    c_seq: i64,
    new_hash: &str,
) -> Result<()> {
    let tx = c.transaction()?;
    tx.execute(
        "DELETE FROM journal WHERE workflow_id = ?1 AND seq > ?2",
        rusqlite::params![workflow_id, c_seq],
    )?;
    // v2.3 — kv versions written by the discarded tail roll back WITH it
    // (this line is what closed the old kv-vs-upgrade caveat in guests.md).
    tx.execute(
        "DELETE FROM kv WHERE workflow_id = ?1 AND seq > ?2",
        rusqlite::params![workflow_id, c_seq],
    )?;
    tx.execute("DELETE FROM timers WHERE workflow_id = ?1", [workflow_id])?;
    tx.execute(
        "UPDATE events SET delivered = 0, delivered_seq = NULL
         WHERE workflow_id = ?1 AND delivered_seq > ?2",
        rusqlite::params![workflow_id, c_seq],
    )?;
    tx.execute(
        "UPDATE workflows SET module_hash = ?2, updated_at = ?3 WHERE id = ?1",
        rusqlite::params![workflow_id, new_hash, now_ms()],
    )?;
    tx.execute(
        "UPDATE snapshots SET module_hash = ?2 WHERE workflow_id = ?1",
        rusqlite::params![workflow_id, new_hash],
    )?;
    tx.commit()?;
    Ok(())
}

/// v2 DR — one consistent online snapshot of the live database into
/// `dest_path` (SQLite backup API: safe while workflows are writing; the
/// result is a fully-checkpointed standalone .db, no -wal/-shm needed).
/// Restore = stop engine, copy the file over the db path, start engine —
/// recovery replays everything non-terminal as usual.
pub fn backup_to(src: &Connection, dest_path: &str) -> Result<()> {
    let mut dst = Connection::open(dest_path)?;
    let bk = rusqlite::backup::Backup::new(src, &mut dst)?;
    bk.run_to_completion(64, std::time::Duration::from_millis(10), None)?;
    Ok(())
}

/// Task 1.4 — this query IS the recovery implementation: every non-terminal workflow
/// simply gets started again from seq 0; the journal turns re-execution into replay.
pub fn resumable_ids(c: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        c.prepare("SELECT id FROM workflows WHERE status IN ('running','sleeping','waiting_event')")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

// --- Micro-cloud phases 4-6 (ext spec §E3 tables) --------------------------------

pub struct RouteRow {
    pub prefix: String,
    pub module_hash: String,
    pub fuel_limit: i64,
    pub mem_limit: i64,
    pub time_limit_ms: i64,
    pub created_at: i64,
    /// Amendment 1: max admitted runs per rolling 60s. None = unlimited.
    pub rate_limit: Option<i64>,
}

/// Bind (or re-bind — POST is how the 5.6 gate lowers /fn/echo's fuel) a URL
/// prefix to a function component with per-route quotas.
pub fn upsert_route(
    c: &Connection,
    prefix: &str,
    module_hash: &str,
    fuel_limit: i64,
    mem_limit: i64,
    time_limit_ms: i64,
    rate_limit: Option<i64>,
) -> Result<()> {
    c.execute(
        "INSERT INTO routes (prefix, module_hash, fuel_limit, mem_limit, time_limit_ms, created_at, rate_limit)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(prefix) DO UPDATE SET module_hash = excluded.module_hash,
             fuel_limit = excluded.fuel_limit, mem_limit = excluded.mem_limit,
             time_limit_ms = excluded.time_limit_ms, rate_limit = excluded.rate_limit",
        rusqlite::params![prefix, module_hash, fuel_limit, mem_limit, time_limit_ms, now_ms(), rate_limit],
    )?;
    Ok(())
}

pub fn list_routes(c: &Connection) -> Result<Vec<RouteRow>> {
    let mut stmt = c.prepare(
        "SELECT prefix, module_hash, fuel_limit, mem_limit, time_limit_ms, created_at, rate_limit
         FROM routes ORDER BY prefix",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(RouteRow {
                prefix: r.get(0)?,
                module_hash: r.get(1)?,
                fuel_limit: r.get(2)?,
                mem_limit: r.get(3)?,
                time_limit_ms: r.get(4)?,
                created_at: r.get(5)?,
                rate_limit: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete_route(c: &Connection, prefix: &str) -> Result<bool> {
    Ok(c.execute("DELETE FROM routes WHERE prefix = ?1", [prefix])? > 0)
}

/// Phase 5 — problems + cases arrive as one idempotent upsert (the operator
/// seeding endpoint): the problem row is replaced and its cases are wiped and
/// re-inserted in one transaction, so a re-seed can never leave a half-old
/// case list behind.
pub fn upsert_problem(
    c: &Connection,
    slug: &str,
    title: &str,
    statement: &str,
    cases: &[(String, String)],
) -> Result<()> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> Result<()> {
        c.execute(
            "INSERT INTO problems (slug, title, statement) VALUES (?1, ?2, ?3)
             ON CONFLICT(slug) DO UPDATE SET title = excluded.title, statement = excluded.statement",
            rusqlite::params![slug, title, statement],
        )?;
        c.execute("DELETE FROM cases WHERE problem = ?1", [slug])?;
        for (idx, (input, expected)) in cases.iter().enumerate() {
            c.execute(
                "INSERT INTO cases (problem, idx, input, expected) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![slug, idx as i64, input, expected],
            )?;
        }
        Ok(())
    })();
    match r {
        Ok(()) => c.execute_batch("COMMIT")?,
        Err(_) => c.execute_batch("ROLLBACK")?,
    }
    r
}

pub fn list_problems(c: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = c.prepare("SELECT slug, title FROM problems ORDER BY slug")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// (title, statement), or None.
pub fn get_problem(c: &Connection, slug: &str) -> Result<Option<(String, String)>> {
    Ok(c.query_row(
        "SELECT title, statement FROM problems WHERE slug = ?1",
        [slug],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?)
}

/// (idx, input, expected) in idx order — the judging order is the stored order.
pub fn list_cases(c: &Connection, slug: &str) -> Result<Vec<(i64, String, String)>> {
    let mut stmt =
        c.prepare("SELECT idx, input, expected FROM cases WHERE problem = ?1 ORDER BY idx")?;
    let rows = stmt
        .query_map([slug], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn insert_submission(c: &Connection, id: &str, problem: &str, module_hash: &str) -> Result<()> {
    c.execute(
        "INSERT INTO submissions (id, problem, module_hash, verdict, detail, created_at)
         VALUES (?1, ?2, ?3, NULL, NULL, ?4)",
        rusqlite::params![id, problem, module_hash, now_ms()],
    )?;
    Ok(())
}

pub struct SubmissionRow {
    pub id: String,
    pub problem: String,
    pub module_hash: String,
    pub verdict: Option<String>,
    pub detail: Option<String>,
    pub created_at: i64,
}

pub fn get_submission(c: &Connection, id: &str) -> Result<Option<SubmissionRow>> {
    Ok(c.query_row(
        "SELECT id, problem, module_hash, verdict, detail, created_at
         FROM submissions WHERE id = ?1",
        [id],
        |r| {
            Ok(SubmissionRow {
                id: r.get(0)?,
                problem: r.get(1)?,
                module_hash: r.get(2)?,
                verdict: r.get(3)?,
                detail: r.get(4)?,
                created_at: r.get(5)?,
            })
        },
    )
    .optional()?)
}

/// One UPDATE at the end of judging (ext spec Task 5.2 step 4).
pub fn set_submission_verdict(c: &Connection, id: &str, verdict: &str, detail: &str) -> Result<()> {
    c.execute(
        "UPDATE submissions SET verdict = ?2, detail = ?3 WHERE id = ?1",
        rusqlite::params![id, verdict, detail],
    )?;
    Ok(())
}

pub fn list_submissions(c: &Connection, problem: &str) -> Result<Vec<SubmissionRow>> {
    let mut stmt = c.prepare(
        "SELECT id, problem, module_hash, verdict, detail, created_at
         FROM submissions WHERE problem = ?1 ORDER BY created_at DESC LIMIT 50",
    )?;
    let rows = stmt
        .query_map([problem], |r| {
            Ok(SubmissionRow {
                id: r.get(0)?,
                problem: r.get(1)?,
                module_hash: r.get(2)?,
                verdict: r.get(3)?,
                detail: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct InvocationRow {
    pub kind: String,
    pub refname: String,
    pub module_hash: String,
    pub outcome: String,
    pub fuel_used: Option<i64>,
    pub peak_mem: Option<i64>,
    pub duration_ms: i64,
    pub created_at: i64,
}

/// Phase 5 — the /usage page: newest 100 ledger rows.
pub fn recent_invocations(c: &Connection) -> Result<Vec<InvocationRow>> {
    let mut stmt = c.prepare(
        "SELECT kind, ref, module_hash, outcome, fuel_used, peak_mem, duration_ms, created_at
         FROM invocations ORDER BY id DESC LIMIT 100",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(InvocationRow {
                kind: r.get(0)?,
                refname: r.get(1)?,
                module_hash: r.get(2)?,
                outcome: r.get(3)?,
                fuel_used: r.get(4)?,
                peak_mem: r.get(5)?,
                duration_ms: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Phase 5 — /usage totals: (module_hash, invocations, total fuel).
pub fn usage_totals(c: &Connection) -> Result<Vec<(String, i64, i64)>> {
    let mut stmt = c.prepare(
        "SELECT module_hash, COUNT(*), COALESCE(SUM(fuel_used), 0)
         FROM invocations GROUP BY module_hash ORDER BY 3 DESC",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Phase 6 — a hosted app: a name, an optional backend function, and a pile
/// of assets. Re-POSTing a name re-binds its backend (like routes).
pub fn upsert_app(
    c: &Connection,
    name: &str,
    backend_hash: Option<&str>,
    rate_limit: Option<i64>,
) -> Result<()> {
    c.execute(
        "INSERT INTO apps (name, backend_hash, created_at, rate_limit) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET backend_hash = excluded.backend_hash,
             rate_limit = excluded.rate_limit",
        rusqlite::params![name, backend_hash, now_ms(), rate_limit],
    )?;
    Ok(())
}

/// What the app-serving path needs to know about one app.
pub struct AppRec {
    /// None = static-only app.
    pub backend_hash: Option<String>,
    /// Amendment 1: like RouteRow.rate_limit, for the api/* backend calls.
    pub rate_limit: Option<i64>,
}

pub fn get_app(c: &Connection, name: &str) -> Result<Option<AppRec>> {
    Ok(c.query_row(
        "SELECT backend_hash, rate_limit FROM apps WHERE name = ?1",
        [name],
        |r| {
            Ok(AppRec {
                backend_hash: r.get(0)?,
                rate_limit: r.get(1)?,
            })
        },
    )
    .optional()?)
}

/// One /apps-page row: (name, backend_hash, asset_count, created_at, rate_limit).
pub type AppListRow = (String, Option<String>, i64, i64, Option<i64>);

pub fn list_apps(c: &Connection) -> Result<Vec<AppListRow>> {
    let mut stmt = c.prepare(
        "SELECT a.name, a.backend_hash,
                (SELECT COUNT(*) FROM assets WHERE app = a.name), a.created_at, a.rate_limit
         FROM apps a ORDER BY a.name",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn upsert_asset(
    c: &Connection,
    app: &str,
    path: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<()> {
    // v3.4 (R.1): the etag is computed HERE so no upload path can forget it —
    // sha256 of the bytes, which is also what makes re-uploads change it.
    let etag = {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(bytes))
    };
    c.execute(
        "INSERT INTO assets (app, path, content_type, bytes, etag) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(app, path) DO UPDATE SET content_type = excluded.content_type,
             bytes = excluded.bytes, etag = excluded.etag",
        rusqlite::params![app, path, content_type, bytes, etag],
    )?;
    Ok(())
}

/// (content_type, bytes, etag) — etag is NULL only on rows written before
/// v3.4 (serving degrades to unconditional 200s until re-upload).
pub type AssetRow = (String, Vec<u8>, Option<String>);

pub fn get_asset(c: &Connection, app: &str, path: &str) -> Result<Option<AssetRow>> {
    Ok(c.query_row(
        "SELECT content_type, bytes, etag FROM assets WHERE app = ?1 AND path = ?2",
        rusqlite::params![app, path],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?)
}

// --- Amendment 2 (v3.5): fn_config (A6) + fn_kv (A7) ------------------------

pub fn upsert_config(c: &Connection, kind: &str, refname: &str, name: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT INTO fn_config (kind, ref, name, value, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(kind, ref, name) DO UPDATE SET value = excluded.value",
        rusqlite::params![kind, refname, name, value, now_ms()],
    )?;
    Ok(())
}

pub fn get_config(c: &Connection, kind: &str, refname: &str, name: &str) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT value FROM fn_config WHERE kind = ?1 AND ref = ?2 AND name = ?3",
        rusqlite::params![kind, refname, name],
        |r| r.get(0),
    )
    .optional()?)
}

/// Names only — values never leave through a listing (A6).
pub fn list_config_names(c: &Connection, kind: &str, refname: &str) -> Result<Vec<String>> {
    let mut stmt = c.prepare(
        "SELECT name FROM fn_config WHERE kind = ?1 AND ref = ?2 ORDER BY name",
    )?;
    let rows = stmt.query_map(rusqlite::params![kind, refname], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn count_config(c: &Connection, kind: &str, refname: &str) -> Result<i64> {
    Ok(c.query_row(
        "SELECT COUNT(*) FROM fn_config WHERE kind = ?1 AND ref = ?2",
        rusqlite::params![kind, refname],
        |r| r.get(0),
    )?)
}

pub fn delete_config(c: &Connection, kind: &str, refname: &str, name: &str) -> Result<bool> {
    Ok(c.execute(
        "DELETE FROM fn_config WHERE kind = ?1 AND ref = ?2 AND name = ?3",
        rusqlite::params![kind, refname, name],
    )? > 0)
}

pub fn kv_get(c: &Connection, kind: &str, refname: &str, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(c.query_row(
        "SELECT value FROM fn_kv WHERE kind = ?1 AND ref = ?2 AND key = ?3",
        rusqlite::params![kind, refname, key],
        |r| r.get(0),
    )
    .optional()?)
}

/// (key count, total value bytes) for the ref — the A7 cap inputs. One
/// indexed aggregate; the PK prefix (kind, ref) serves it.
pub fn kv_usage(c: &Connection, kind: &str, refname: &str) -> Result<(i64, i64)> {
    Ok(c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(value)), 0) FROM fn_kv
         WHERE kind = ?1 AND ref = ?2",
        rusqlite::params![kind, refname],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// A7: the durability contract is THIS commit — autocommit means the row is
/// on disk before this function (and therefore the host call) returns.
pub fn kv_set(c: &Connection, kind: &str, refname: &str, key: &str, value: &[u8]) -> Result<()> {
    c.execute(
        "INSERT INTO fn_kv (kind, ref, key, value, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(kind, ref, key) DO UPDATE SET value = excluded.value,
             updated_at = excluded.updated_at",
        rusqlite::params![kind, refname, key, value, now_ms()],
    )?;
    Ok(())
}

pub fn kv_delete(c: &Connection, kind: &str, refname: &str, key: &str) -> Result<()> {
    c.execute(
        "DELETE FROM fn_kv WHERE kind = ?1 AND ref = ?2 AND key = ?3",
        rusqlite::params![kind, refname, key],
    )?;
    Ok(())
}

/// Keys only, same reasoning as config names (guest state isn't for browsing).
pub fn kv_keys(c: &Connection, kind: &str, refname: &str) -> Result<Vec<String>> {
    let mut stmt =
        c.prepare("SELECT key FROM fn_kv WHERE kind = ?1 AND ref = ?2 ORDER BY key")?;
    let rows = stmt.query_map(rusqlite::params![kind, refname], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The whole ref's store — the control plane's "reset my function" button.
pub fn kv_wipe(c: &Connection, kind: &str, refname: &str) -> Result<usize> {
    Ok(c.execute(
        "DELETE FROM fn_kv WHERE kind = ?1 AND ref = ?2",
        rusqlite::params![kind, refname],
    )?)
}

/// v3.4 (R.2) — delete an app AND its assets in one transaction (routes have
/// had DELETE since phase 4; apps never did). Ledger rows and captured logs
/// deliberately REMAIN — they are history, owned by --retain-ledger-hours.
pub fn delete_app(c: &mut Connection, name: &str) -> Result<bool> {
    let tx = c.transaction()?;
    tx.execute("DELETE FROM assets WHERE app = ?1", [name])?;
    let n = tx.execute("DELETE FROM apps WHERE name = ?1", [name])?;
    tx.commit()?;
    Ok(n > 0)
}

/// (kind, ref, p50, p95, p99) of duration_ms.
pub type LatencyRow = (String, String, i64, i64, i64);

/// v3.4 (R.5) — p50/p95/p99 of duration_ms per (kind, ref), nearest-rank on
/// the sorted set via OFFSET. Ref counts are small; three indexed lookups per
/// ref beat a hand-rolled histogram, and there is nothing to mis-bucket.
pub fn duration_percentiles(c: &Connection) -> Result<Vec<LatencyRow>> {
    let mut refs: Vec<(String, String, i64)> = Vec::new();
    {
        let mut stmt =
            c.prepare("SELECT kind, ref, COUNT(*) FROM invocations GROUP BY kind, ref ORDER BY kind, ref")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        for row in rows {
            refs.push(row?);
        }
    }
    let mut out = Vec::new();
    for (kind, refname, n) in refs {
        let q = |p: f64| -> Result<i64> {
            let off = ((p * (n - 1) as f64).round() as i64).clamp(0, n - 1);
            Ok(c.query_row(
                "SELECT duration_ms FROM invocations WHERE kind = ?1 AND ref = ?2
                 ORDER BY duration_ms LIMIT 1 OFFSET ?3",
                rusqlite::params![kind, refname, off],
                |r| r.get(0),
            )?)
        };
        out.push((kind.clone(), refname.clone(), q(0.50)?, q(0.95)?, q(0.99)?));
    }
    Ok(out)
}

/// Ledger rollup for the routes page: (ref, row count) for one kind.
pub fn invocation_counts(c: &Connection, kind: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt =
        c.prepare("SELECT ref, COUNT(*) FROM invocations WHERE kind = ?1 GROUP BY ref")?;
    let rows = stmt
        .query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The usage ledger (ext spec Task 4.3 step 6): one row per function/solver
/// invocation, written on EVERY outcome — metering that only counts successes
/// is fiction. fuel/peak are Options because a store that failed setup has
/// nothing truthful to report. Returns the new row's id (Amendment 1: fn_logs
/// lines are tagged with the invocation that produced them).
#[allow(clippy::too_many_arguments)] // 1:1 with the ledger columns, nothing more
pub fn insert_invocation(
    c: &Connection,
    kind: &str,
    refname: &str,
    module_hash: &str,
    outcome: &str,
    fuel_used: Option<i64>,
    peak_mem: Option<i64>,
    duration_ms: i64,
) -> Result<i64> {
    c.execute(
        "INSERT INTO invocations (kind, ref, module_hash, outcome, fuel_used, peak_mem, duration_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![kind, refname, module_hash, outcome, fuel_used, peak_mem, duration_ms, now_ms()],
    )?;
    Ok(c.last_insert_rowid())
}

// --- Amendment 1 (A1/A2/A3): rate-limit reads, fn_logs, ledger GC -----------

/// Ledger rows for one ref since a cutoff — the A1 admission window count.
/// Uses idx_invocations_admit; runs per-request on rate-limited refs only.
pub fn recent_invocation_count(c: &Connection, kind: &str, refname: &str, since_ms: i64) -> Result<i64> {
    Ok(c.query_row(
        "SELECT COUNT(*) FROM invocations WHERE kind = ?1 AND ref = ?2 AND created_at > ?3",
        rusqlite::params![kind, refname, since_ms],
        |r| r.get(0),
    )?)
}

/// The oldest in-window ledger row's created_at — when it ages out is the
/// honest Retry-After for a 429. None = the window is pure in-flight runs.
pub fn oldest_invocation_since(c: &Connection, kind: &str, refname: &str, since_ms: i64) -> Result<Option<i64>> {
    Ok(c.query_row(
        "SELECT MIN(created_at) FROM invocations WHERE kind = ?1 AND ref = ?2 AND created_at > ?3",
        rusqlite::params![kind, refname, since_ms],
        |r| r.get(0),
    )?)
}

/// How many rows each ref keeps in fn_logs — trimmed at insert time so a
/// chatty function can't grow the table without bound between GC passes.
pub const FN_LOGS_KEEP_PER_REF: i64 = 2000;

/// A2: batch-write one invocation's captured log lines (insert order = the
/// order the guest logged them; AUTOINCREMENT ids preserve it), then trim the
/// ref to its newest FN_LOGS_KEEP_PER_REF rows. One transaction.
pub fn insert_fn_logs(
    c: &Connection,
    kind: &str,
    refname: &str,
    invocation_id: i64,
    lines: &[String],
) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    c.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> Result<()> {
        let now = now_ms();
        {
            let mut stmt = c.prepare(
                "INSERT INTO fn_logs (kind, ref, invocation_id, line, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for line in lines {
                stmt.execute(rusqlite::params![kind, refname, invocation_id, line, now])?;
            }
        }
        // Keep the newest FN_LOGS_KEEP_PER_REF rows for this ref: the subquery
        // finds the oldest id still kept; with fewer rows it yields NULL and
        // `id < NULL` deletes nothing.
        c.execute(
            "DELETE FROM fn_logs WHERE kind = ?1 AND ref = ?2 AND id <
                 (SELECT id FROM fn_logs WHERE kind = ?1 AND ref = ?2
                  ORDER BY id DESC LIMIT 1 OFFSET ?3)",
            rusqlite::params![kind, refname, FN_LOGS_KEEP_PER_REF - 1],
        )?;
        Ok(())
    })();
    match r {
        Ok(()) => c.execute_batch("COMMIT")?,
        Err(_) => c.execute_batch("ROLLBACK")?,
    }
    r
}

pub struct FnLogRow {
    pub id: i64,
    pub invocation_id: Option<i64>,
    pub line: String,
    pub created_at: i64,
}

fn map_log_row(r: &rusqlite::Row) -> rusqlite::Result<FnLogRow> {
    Ok(FnLogRow {
        id: r.get(0)?,
        invocation_id: r.get(1)?,
        line: r.get(2)?,
        created_at: r.get(3)?,
    })
}

/// The newest `limit` lines for a ref, oldest-first (a log tail).
pub fn tail_fn_logs(c: &Connection, kind: &str, refname: &str, limit: i64) -> Result<Vec<FnLogRow>> {
    let mut stmt = c.prepare(
        "SELECT id, invocation_id, line, created_at FROM fn_logs
         WHERE kind = ?1 AND ref = ?2 ORDER BY id DESC LIMIT ?3",
    )?;
    let mut rows = stmt
        .query_map(rusqlite::params![kind, refname, limit], map_log_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.reverse();
    Ok(rows)
}

/// Lines with id > after, oldest-first — the tail-following contract
/// (/api/logs `after=`, the UI partial, `keel logs --follow`).
pub fn fn_logs_after(
    c: &Connection,
    kind: &str,
    refname: &str,
    after: i64,
    limit: i64,
) -> Result<Vec<FnLogRow>> {
    let mut stmt = c.prepare(
        "SELECT id, invocation_id, line, created_at FROM fn_logs
         WHERE kind = ?1 AND ref = ?2 AND id > ?3 ORDER BY id ASC LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![kind, refname, after, limit], map_log_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// A3: --retain-ledger-hours sweep — (invocations deleted, fn_logs deleted).
pub fn gc_ledger(c: &Connection, cutoff_ms: i64) -> Result<(usize, usize)> {
    let inv = c.execute("DELETE FROM invocations WHERE created_at < ?1", [cutoff_ms])?;
    let logs = c.execute("DELETE FROM fn_logs WHERE created_at < ?1", [cutoff_ms])?;
    Ok((inv, logs))
}

// --- Unit tests (post-review hardening) ------------------------------------------
// These run against in-memory SQLite with the real MIGRATION. The multi-statement
// transactions above (event delivery, upgrade tail-discard, sleep wake, cancel)
// must each execute under a test before they execute in anger.

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    fn wf(c: &Connection, id: &str, hash: &str) {
        insert_module(c, hash, "m", b"\0asm").unwrap();
        create_workflow(c, id, hash, "{}").unwrap();
    }

    fn status_of(c: &Connection, id: &str) -> String {
        get_workflow(c, id).unwrap().unwrap().status
    }

    fn event_row(c: &Connection, id: i64) -> (i64, Option<i64>) {
        c.query_row(
            "SELECT delivered, delivered_seq FROM events WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn deliver_event_consumes_oldest_journals_and_flips_status() {
        let mut c = mem();
        wf(&c, "w", "h");
        set_status(&c, "w", "waiting_event", None).unwrap();
        let none = deliver_event_and_journal(&mut c, "w", 0, "go", r#"{"name":"go"}"#).unwrap();
        assert_eq!(none, None, "no event queued yet");
        insert_event(&c, "w", "go", r#"{"n":1}"#).unwrap();
        insert_event(&c, "w", "go", r#"{"n":2}"#).unwrap();
        insert_event(&c, "w", "other", "{}").unwrap();

        let got = deliver_event_and_journal(&mut c, "w", 0, "go", r#"{"name":"go"}"#).unwrap();
        assert_eq!(got.as_deref(), Some(r#"{"n":1}"#), "oldest matching event wins");
        assert_eq!(event_row(&c, 1), (1, Some(0)), "consumed, tagged with its seq");
        assert_eq!(event_row(&c, 2), (0, None), "second event still queued");
        assert_eq!(event_row(&c, 3), (0, None), "name mismatch untouched");
        let (kind, req, resp) = get_journal_row(&c, "w", 0).unwrap().unwrap();
        assert_eq!(kind, "await-event");
        assert_eq!(req, r#"{"name":"go"}"#);
        assert_eq!(resp, r#"{"payload":"{\"n\":1}"}"#);
        assert_eq!(status_of(&c, "w"), "running", "flip rides in the delivery txn");
    }

    #[test]
    fn finish_sleep_is_one_atomic_wake() {
        let mut c = mem();
        wf(&c, "w", "h");
        insert_timer(&c, "w", 4, 12345).unwrap();
        set_status(&c, "w", "sleeping", None).unwrap();

        finish_sleep(&mut c, "w", 4, r#"{"ms":50}"#).unwrap();
        assert_eq!(get_timer_wake_at(&c, "w").unwrap(), None, "timer consumed");
        let (kind, req, resp) = get_journal_row(&c, "w", 4).unwrap().unwrap();
        assert_eq!(
            (kind.as_str(), req.as_str(), resp.as_str()),
            ("sleep-ms", r#"{"ms":50}"#, "{}")
        );
        assert_eq!(status_of(&c, "w"), "running");
    }

    #[test]
    fn upgrade_txn_discards_tail_undelivers_and_repoints() {
        let mut c = mem();
        wf(&c, "w", "v1");
        insert_module(&c, "v2", "m2", b"\0asm2").unwrap();
        for seq in 0..=5 {
            insert_journal_row(&c, "w", seq, "k", "{}", "{}").unwrap();
        }
        insert_timer(&c, "w", 6, 99999).unwrap();
        insert_event(&c, "w", "e", "1").unwrap(); // id 1: delivered pre-checkpoint
        insert_event(&c, "w", "e", "2").unwrap(); // id 2: delivered in the tail
        insert_event(&c, "w", "e", "3").unwrap(); // id 3: never delivered
        c.execute("UPDATE events SET delivered = 1, delivered_seq = 2 WHERE id = 1", [])
            .unwrap();
        c.execute("UPDATE events SET delivered = 1, delivered_seq = 5 WHERE id = 2", [])
            .unwrap();
        // checkpoint at C=3 prunes seq < 3 → journal is {3, 4, 5}
        snapshot_and_prune(&mut c, "w", 3, b"state").unwrap();
        let left: Vec<i64> = journal_rows(&c, "w").unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(left, vec![3, 4, 5], "prune keeps row C and the tail");

        upgrade_module_txn(&mut c, "w", 3, "v2").unwrap();

        let left: Vec<i64> = journal_rows(&c, "w").unwrap().iter().map(|r| r.seq).collect();
        assert_eq!(left, vec![3], "tail beyond C discarded");
        assert_eq!(get_timer_wake_at(&c, "w").unwrap(), None, "timers dropped");
        assert_eq!(event_row(&c, 1), (1, Some(2)), "pre-checkpoint delivery stands");
        assert_eq!(event_row(&c, 2), (0, None), "tail delivery UN-delivered");
        assert_eq!(event_row(&c, 3), (0, None), "queued event untouched");
        assert_eq!(get_workflow(&c, "w").unwrap().unwrap().module_hash, "v2");
        assert_eq!(get_snapshot(&c, "w").unwrap().unwrap().module_hash, "v2");
    }

    fn kv_all(c: &Connection, id: &str) -> Vec<(String, i64, String)> {
        let mut stmt = c
            .prepare("SELECT key, seq, value FROM kv WHERE workflow_id = ?1 ORDER BY key, seq")
            .unwrap();
        stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn kv_is_versioned_reads_latest_and_upgrade_discards_the_tail() {
        let mut c = mem();
        wf(&c, "w", "v1");
        insert_module(&c, "v2", "m2", b"\0asm2").unwrap();
        kv_set_and_journal(&mut c, "w", 0, "k", "a", "{}").unwrap();
        // checkpoint at C=1 (journal row C is the wrapper's business; the txn
        // here is what prunes + compacts)
        snapshot_and_prune(&mut c, "w", 1, b"s").unwrap();
        kv_set_and_journal(&mut c, "w", 2, "k", "b", "{}").unwrap();

        let got = kv_get_and_journal(&mut c, "w", 3, "k", "{}").unwrap();
        assert_eq!(got.as_deref(), Some("b"), "reads resolve the highest version");
        assert_eq!(
            kv_all(&c, "w"),
            vec![
                ("k".to_string(), 0, "a".to_string()),
                ("k".to_string(), 2, "b".to_string())
            ],
            "both versions live until compaction/upgrade"
        );

        upgrade_module_txn(&mut c, "w", 1, "v2").unwrap();
        assert_eq!(
            kv_all(&c, "w"),
            vec![("k".to_string(), 0, "a".to_string())],
            "tail versions (seq > C) roll back with the journal tail"
        );
        let got = kv_get_and_journal(&mut c, "w", 2, "k", "{}").unwrap();
        assert_eq!(got.as_deref(), Some("a"), "post-upgrade read sees the pre-tail value");
    }

    #[test]
    fn checkpoint_compacts_superseded_kv_versions() {
        let mut c = mem();
        wf(&c, "w", "h");
        kv_set_and_journal(&mut c, "w", 0, "k", "a", "{}").unwrap();
        kv_set_and_journal(&mut c, "w", 1, "k", "b", "{}").unwrap();
        kv_set_and_journal(&mut c, "w", 2, "other", "x", "{}").unwrap();
        snapshot_and_prune(&mut c, "w", 3, b"s").unwrap();
        assert_eq!(
            kv_all(&c, "w"),
            vec![
                ("k".to_string(), 1, "b".to_string()),
                ("other".to_string(), 2, "x".to_string())
            ],
            "only the latest version per key survives a checkpoint"
        );
    }

    #[test]
    fn migrate_reshapes_pre_v23_kv() {
        // A faithful pre-v2.3 database: valid module/workflow references and
        // the old single-row-per-key kv shape.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE modules (hash TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '',
                wasm BLOB NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE workflows (id TEXT PRIMARY KEY,
                module_hash TEXT NOT NULL REFERENCES modules(hash), input TEXT NOT NULL,
                status TEXT NOT NULL, output TEXT, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL);
             CREATE TABLE kv (workflow_id TEXT NOT NULL REFERENCES workflows(id),
                key TEXT NOT NULL, value TEXT NOT NULL, updated_at INTEGER NOT NULL,
                PRIMARY KEY (workflow_id, key));
             INSERT INTO modules VALUES ('h', 'm', x'00', 0);
             INSERT INTO workflows VALUES ('w', 'h', '{}', 'running', NULL, 0, 0);
             INSERT INTO kv VALUES ('w', 'k', 'old', 42);",
        )
        .unwrap();
        migrate(&c).unwrap();
        migrate(&c).unwrap(); // idempotent
        assert_eq!(
            kv_all(&c, "w"),
            vec![("k".to_string(), 0, "old".to_string())],
            "pre-v2.3 rows become version 0"
        );
    }

    #[test]
    fn fire_schedule_is_one_txn_and_enable_toggle_gates_due() {
        let mut c = mem();
        insert_module(&c, "h", "m", b"\0asm").unwrap();
        insert_schedule(&c, "s", "h", "{}", 2000, None, 1000).unwrap();
        insert_schedule(&c, "sc", "h", "{}", 0, Some("*/2 * * * * *"), 1000).unwrap();

        let due = due_schedules(&c, 1500).unwrap();
        assert_eq!(due.len(), 2);
        assert_eq!(due.iter().find(|s| s.id == "sc").unwrap().cron.as_deref(), Some("*/2 * * * * *"));

        fire_schedule(&mut c, &due[0], "w1", 3000).unwrap();
        assert!(get_workflow(&c, "w1").unwrap().is_some(), "workflow created in the txn");
        let s = list_schedules(&c).unwrap().into_iter().find(|s| s.id == due[0].id).unwrap();
        assert_eq!(s.next_run_at, 3000, "advanced to the caller's next");

        assert!(set_schedule_enabled(&c, "sc", false).unwrap());
        let due = due_schedules(&c, 1500).unwrap();
        assert!(due.iter().all(|s| s.id != "sc"), "disabled schedules are never due");
        assert!(!set_schedule_enabled(&c, "nope", true).unwrap(), "unknown id is false");
    }

    #[test]
    fn ensure_column_retrofits_pre_v21_schedules_table() {
        let c = Connection::open_in_memory().unwrap();
        // A pre-v2.1 schedules table (no cron column), then migrate() over it.
        c.execute_batch(
            "CREATE TABLE schedules (
                id TEXT PRIMARY KEY, module_hash TEXT NOT NULL, input TEXT NOT NULL,
                interval_ms INTEGER NOT NULL, next_run_at INTEGER NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL);",
        )
        .unwrap();
        migrate(&c).unwrap();
        migrate(&c).unwrap(); // idempotent: the second pass must not re-ALTER
        c.execute("INSERT INTO modules (hash, name, wasm, created_at) VALUES ('h','m',x'00',0)", [])
            .unwrap();
        insert_schedule(&c, "s", "h", "{}", 0, Some("0 * * * * *"), 1).unwrap();
        assert_eq!(
            list_schedules(&c).unwrap()[0].cron.as_deref(),
            Some("0 * * * * *")
        );
    }

    #[test]
    fn finish_cancel_clears_timer_and_fails_terminally() {
        let mut c = mem();
        wf(&c, "w", "h");
        insert_timer(&c, "w", 0, 99999).unwrap();
        set_status(&c, "w", "sleeping", None).unwrap();

        finish_cancel(&mut c, "w", "cancelled by operator").unwrap();
        assert_eq!(get_timer_wake_at(&c, "w").unwrap(), None);
        let row = get_workflow(&c, "w").unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(row.output.as_deref(), Some("cancelled by operator"));
    }

    #[test]
    fn provider_registry_upsert_rebind_delete_roundtrip() {
        let c = mem();
        // Upload: blob + binding, one txn; identical bytes dedupe by hash.
        upsert_provider(&c, "greet", false, "h1", b"\0asm-one").unwrap();
        upsert_provider(&c, "relay", true, "h2", b"\0asm-two").unwrap();
        upsert_provider(&c, "greet2", false, "h1", b"\0asm-one").unwrap();
        let blobs: i64 = c
            .query_row("SELECT COUNT(*) FROM provider_blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blobs, 2, "same bytes must share one blob");
        assert_eq!(
            list_providers(&c).unwrap(),
            vec![
                ("greet".into(), false, "h1".into(), list_providers(&c).unwrap()[0].3),
                ("greet2".into(), false, "h1".into(), list_providers(&c).unwrap()[1].3),
                ("relay".into(), true, "h2".into(), list_providers(&c).unwrap()[2].3),
            ]
        );
        // Re-upload under the same name moves the pointer (a roll).
        upsert_provider(&c, "greet", false, "h2", b"\0asm-two").unwrap();
        assert_eq!(list_providers(&c).unwrap()[0].2, "h2");
        // Rollback path: the old blob is still there by content address.
        assert_eq!(get_provider_blob(&c, "h1").unwrap().as_deref(), Some(&b"\0asm-one"[..]));
        assert_eq!(get_provider_blob(&c, "nope").unwrap(), None);
        // Unbind keeps the blob; load only returns bound names.
        assert!(delete_provider(&c, "greet2").unwrap());
        assert!(!delete_provider(&c, "greet2").unwrap());
        let loaded = load_provider_registry(&c).unwrap();
        assert_eq!(
            loaded.iter().map(|r| (r.0.as_str(), r.1, r.2.as_str())).collect::<Vec<_>>(),
            vec![("greet", false, "h2"), ("relay", true, "h2")]
        );
        assert_eq!(loaded[0].3, b"\0asm-two");
        assert!(get_provider_blob(&c, "h1").unwrap().is_some());
    }

    // --- Amendment 1 -----------------------------------------------------------

    #[test]
    fn rate_limit_column_retrofits_onto_old_databases() {
        // A pre-amendment routes/apps table (no rate_limit) must gain the
        // column via ensure_column, not fail or fork the schema.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE routes (
                 prefix TEXT PRIMARY KEY, module_hash TEXT NOT NULL,
                 fuel_limit INTEGER NOT NULL DEFAULT 500000000,
                 mem_limit INTEGER NOT NULL DEFAULT 67108864,
                 time_limit_ms INTEGER NOT NULL DEFAULT 5000,
                 created_at INTEGER NOT NULL);
             CREATE TABLE apps (
                 name TEXT PRIMARY KEY, backend_hash TEXT,
                 created_at INTEGER NOT NULL);
             INSERT INTO routes VALUES ('/fn/old', 'h', 1, 2, 3, 4);",
        )
        .unwrap();
        migrate(&c).unwrap();
        assert!(has_column(&c, "routes", "rate_limit").unwrap());
        assert!(has_column(&c, "apps", "rate_limit").unwrap());
        // The pre-existing binding reads back as unlimited.
        let rows = list_routes(&c).unwrap();
        assert_eq!(rows[0].rate_limit, None);
        // And the new column round-trips.
        upsert_route(&c, "/fn/old", "h", 1, 2, 3, Some(5)).unwrap();
        assert_eq!(list_routes(&c).unwrap()[0].rate_limit, Some(5));
    }

    #[test]
    fn admission_window_counts_only_recent_rows_for_the_ref() {
        let c = mem();
        for (kind, refname, age) in [
            ("function", "/fn/a", 0),      // in window
            ("function", "/fn/a", 0),      // in window
            ("function", "/fn/a", 70_000), // aged out
            ("function", "/fn/b", 0),      // other ref
            ("app", "/fn/a", 0),           // other kind, same ref string
        ] {
            let id = insert_invocation(&c, kind, refname, "h", "ok", None, None, 1).unwrap();
            if age > 0 {
                c.execute(
                    "UPDATE invocations SET created_at = created_at - ?1 WHERE id = ?2",
                    rusqlite::params![age, id],
                )
                .unwrap();
            }
        }
        let since = now_ms() - 60_000;
        assert_eq!(recent_invocation_count(&c, "function", "/fn/a", since).unwrap(), 2);
        assert_eq!(recent_invocation_count(&c, "function", "/fn/b", since).unwrap(), 1);
        assert_eq!(recent_invocation_count(&c, "function", "/fn/c", since).unwrap(), 0);
        assert!(oldest_invocation_since(&c, "function", "/fn/a", since).unwrap().is_some());
        assert_eq!(oldest_invocation_since(&c, "function", "/fn/c", since).unwrap(), None);
    }

    #[test]
    fn fn_logs_tail_after_and_per_ref_trim() {
        let c = mem();
        let inv = insert_invocation(&c, "function", "/fn/a", "h", "ok", None, None, 1).unwrap();
        insert_fn_logs(&c, "function", "/fn/a", inv, &["one".into(), "two".into()]).unwrap();
        insert_fn_logs(&c, "app", "hello", inv, &["other-ref".into()]).unwrap();
        let tail = tail_fn_logs(&c, "function", "/fn/a", 100).unwrap();
        assert_eq!(
            tail.iter().map(|l| l.line.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"],
            "tail is oldest-first and scoped to the ref"
        );
        assert_eq!(tail[0].invocation_id, Some(inv));
        // after= returns strictly newer lines only.
        let newer = fn_logs_after(&c, "function", "/fn/a", tail[0].id, 100).unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].line, "two");
        // Trim: pushing past FN_LOGS_KEEP_PER_REF keeps the newest rows and
        // never touches other refs.
        let batch: Vec<String> = (0..500).map(|i| format!("l{i}")).collect();
        for _ in 0..5 {
            insert_fn_logs(&c, "function", "/fn/a", inv, &batch).unwrap();
        }
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM fn_logs WHERE kind='function' AND ref='/fn/a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, FN_LOGS_KEEP_PER_REF);
        let kept = tail_fn_logs(&c, "function", "/fn/a", 1).unwrap();
        assert_eq!(kept[0].line, "l499", "the newest line survives the trim");
        assert_eq!(tail_fn_logs(&c, "app", "hello", 10).unwrap().len(), 1);
    }

    #[test]
    fn gc_ledger_sweeps_both_tables_past_the_cutoff() {
        let c = mem();
        let old = insert_invocation(&c, "function", "/fn/a", "h", "ok", None, None, 1).unwrap();
        insert_fn_logs(&c, "function", "/fn/a", old, &["old-line".into()]).unwrap();
        c.execute_batch(
            "UPDATE invocations SET created_at = created_at - 700000;
             UPDATE fn_logs SET created_at = created_at - 700000;",
        )
        .unwrap();
        let fresh = insert_invocation(&c, "function", "/fn/a", "h", "ok", None, None, 1).unwrap();
        insert_fn_logs(&c, "function", "/fn/a", fresh, &["fresh-line".into()]).unwrap();
        let (inv, logs) = gc_ledger(&c, now_ms() - 600_000).unwrap();
        assert_eq!((inv, logs), (1, 1));
        assert_eq!(recent_invocation_count(&c, "function", "/fn/a", 0).unwrap(), 1);
        let left = tail_fn_logs(&c, "function", "/fn/a", 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].line, "fresh-line");
    }

    #[test]
    fn duration_percentiles_nearest_rank() {
        // v3.4 (R.5): durations 10..=100 by tens → nearest-rank offsets are
        // knowable by hand: p50 = idx round(0.5*9)=5 (60ms wait: values
        // 10,20,...,100; idx 5 → 60? offset 5 → the 6th value = 60), p95 =
        // round(0.95*9)=9 → 100, p99 → 100.
        let c = mem();
        for d in (1..=10).map(|i| i * 10) {
            insert_invocation(&c, "function", "/fn/p", "h", "ok", None, None, d).unwrap();
        }
        insert_invocation(&c, "app", "solo", "h", "ok", None, None, 7).unwrap();
        let rows = duration_percentiles(&c).unwrap();
        assert_eq!(rows.len(), 2);
        let solo = rows.iter().find(|r| r.0 == "app").unwrap();
        assert_eq!((solo.2, solo.3, solo.4), (7, 7, 7), "n=1: every quantile IS the value");
        let p = rows.iter().find(|r| r.0 == "function").unwrap();
        assert_eq!((p.2, p.3, p.4), (60, 100, 100));
    }

    #[test]
    fn asset_etag_set_on_upsert_and_changes_with_bytes() {
        // v3.4 (R.1): the etag is the content hash — same bytes, same tag;
        // re-upload with new bytes MUST move it (that is the whole 304 story).
        let c = mem();
        c.execute(
            "INSERT INTO apps (name, created_at) VALUES ('a', 0)",
            [],
        )
        .unwrap();
        upsert_asset(&c, "a", "x.js", "text/javascript", b"one").unwrap();
        let (_, _, t1) = get_asset(&c, "a", "x.js").unwrap().unwrap();
        let t1 = t1.expect("etag set on insert");
        upsert_asset(&c, "a", "x.js", "text/javascript", b"one").unwrap();
        let (_, _, t2) = get_asset(&c, "a", "x.js").unwrap().unwrap();
        assert_eq!(Some(t1.clone()), t2, "same bytes, same etag");
        upsert_asset(&c, "a", "x.js", "text/javascript", b"two").unwrap();
        let (_, bytes, t3) = get_asset(&c, "a", "x.js").unwrap().unwrap();
        assert_eq!(bytes, b"two");
        assert_ne!(Some(t1), t3, "new bytes must move the etag");
    }

    #[test]
    fn delete_app_cascades_assets_in_one_txn() {
        // v3.4 (R.2)
        let mut c = mem();
        c.execute("INSERT INTO apps (name, created_at) VALUES ('gone', 0)", []).unwrap();
        upsert_asset(&c, "gone", "index.html", "text/html", b"<h1>").unwrap();
        assert!(delete_app(&mut c, "gone").unwrap());
        assert!(get_asset(&c, "gone", "index.html").unwrap().is_none());
        let n: i64 = c.query_row("SELECT COUNT(*) FROM apps", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        assert!(!delete_app(&mut c, "gone").unwrap(), "second delete reports missing");
    }
}
