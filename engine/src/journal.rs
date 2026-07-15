// journal.rs — the journaled() wrapper, heart of the engine (SPEC.md §6, "implement
// exactly"). This is the ONE file allowed to contain SQL outside db.rs, because the
// spec fixes its contents verbatim.
//
// THE architectural invariants (SPEC.md §0, rules 2 + 3):
//
//   * The journal row is COMMITTED to SQLite BEFORE the result is returned to the
//     guest. Never reorder this — the ordering is the entire correctness story.
//
//   * There is NO "replay mode". Fresh execution and crash recovery run the SAME
//     code path: if a row for (workflow_id, seq) already exists, return the recorded
//     response; otherwise run the effect live and record it. If you ever feel the
//     need to write `if replaying { ... }`, stop and re-read SPEC.md §4.

use anyhow::{bail, Context, Result};
use rusqlite::OptionalExtension;
use serde::{de::DeserializeOwned, Serialize};

pub struct JournalCtx {
    pub workflow_id: String,
    /// This workflow thread's PRIVATE connection (one per thread, opened via
    /// db::open_conn — never shared, never opened any other way).
    pub db: rusqlite::Connection,
    /// Dense per-workflow sequence counter. ALWAYS starts at 0, even on recovery
    /// (recovery is not special). Phase 3's snapshot resume is the only thing that
    /// ever starts it elsewhere (snapshot.journal_seq + 1).
    pub next_seq: i64,
}

impl JournalCtx {
    /// Wrap every effectful host call in this. `exec` runs ONLY on the live path.
    ///
    /// At-least-once caveat (accepted for phases 1–3): a crash after `exec` succeeds
    /// but before the INSERT commits means the effect re-runs on recovery.
    /// Mitigation (intent records + idempotency keys) is out of scope.
    pub fn journaled<Req, Resp>(
        &mut self,
        kind: &str,
        req: &Req,
        exec: impl FnOnce() -> Result<Resp>, // captures what it needs; clone cheap
                                             // handles (ureq::Agent is an Arc) into it
    ) -> Result<Resp>
    where
        Req: Serialize,
        Resp: Serialize + DeserializeOwned,
    {
        let seq = self.next_seq;
        self.next_seq += 1;
        let req_json = serde_json::to_string(req)?;

        let recorded: Option<(String, String, String)> = self
            .db
            .query_row(
                "SELECT kind, request, response FROM journal
                 WHERE workflow_id = ?1 AND seq = ?2",
                rusqlite::params![self.workflow_id, seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        if let Some((rkind, rreq, rresp)) = recorded {
            if rkind != kind || rreq != req_json {
                bail!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced ({kind}, {req_json}). The workflow code has \
                     diverged from its journal."
                );
            }
            return serde_json::from_str(&rresp).context("corrupt journal response");
        }

        let resp = exec()?; // real side effect happens here
        let resp_json = serde_json::to_string(&resp)?;
        self.db.execute(
            "INSERT INTO journal (workflow_id, seq, kind, request, response, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![self.workflow_id, seq, kind, req_json, resp_json, now_ms()],
        )?; // committed BEFORE returning
        Ok(resp)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
