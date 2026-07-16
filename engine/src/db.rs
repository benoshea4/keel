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
";

pub fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch(MIGRATION)?;
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
/// matching event is queued (caller parks and retries).
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
    tx.commit()?;
    Ok(Some(payload))
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
