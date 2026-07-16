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

// --- Unit tests (post-review hardening) ------------------------------------------
// The replay check and nondeterminism bail above are the engine's core safety
// property; they must not depend on the acceptance scripts alone. Recovery is
// simulated the way the engine really does it: same code path, next_seq reset.

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    struct Req {
        x: u32,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Resp {
        y: u32,
    }

    fn ctx() -> JournalCtx {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&db).unwrap();
        crate::db::insert_module(&db, "h", "m", b"\0asm").unwrap();
        crate::db::create_workflow(&db, "w", "h", "{}").unwrap();
        JournalCtx {
            workflow_id: "w".to_string(),
            db,
            next_seq: 0,
        }
    }

    #[test]
    fn live_records_then_replay_returns_without_rerunning_exec() {
        let mut j = ctx();
        let mut execs = 0;
        let r = j
            .journaled("t", &Req { x: 1 }, || {
                execs += 1;
                Ok(Resp { y: 7 })
            })
            .unwrap();
        assert_eq!(r, Resp { y: 7 });
        assert_eq!((execs, j.next_seq), (1, 1));

        j.next_seq = 0; // what recovery does: same code path, counter back at 0
        let r = j
            .journaled("t", &Req { x: 1 }, || {
                execs += 1;
                Ok(Resp { y: 999 })
            })
            .unwrap();
        assert_eq!(r, Resp { y: 7 }, "replay returns the RECORDED response");
        assert_eq!(execs, 1, "exec must not re-run on replay");
        assert_eq!(j.next_seq, 1);
    }

    #[test]
    fn replay_with_changed_kind_is_nondeterminism() {
        let mut j = ctx();
        j.journaled("t", &Req { x: 1 }, || Ok(Resp { y: 7 })).unwrap();
        j.next_seq = 0;
        let mut ran = false;
        let err = j
            .journaled("other", &Req { x: 1 }, || {
                ran = true;
                Ok(Resp { y: 0 })
            })
            .unwrap_err();
        assert!(err.to_string().contains("nondeterministic replay"), "got: {err}");
        assert!(!ran, "exec must never run once the journal disagrees");
    }

    #[test]
    fn replay_with_changed_request_is_nondeterminism() {
        let mut j = ctx();
        j.journaled("t", &Req { x: 1 }, || Ok(Resp { y: 7 })).unwrap();
        j.next_seq = 0;
        let mut ran = false;
        let err = j
            .journaled("t", &Req { x: 2 }, || {
                ran = true;
                Ok(Resp { y: 0 })
            })
            .unwrap_err();
        assert!(err.to_string().contains("nondeterministic replay"), "got: {err}");
        assert!(!ran);
    }

    #[test]
    fn exec_error_journals_nothing_so_recovery_retries() {
        let mut j = ctx();
        let err = j
            .journaled("t", &Req { x: 1 }, || Err::<Resp, _>(anyhow::anyhow!("boom")))
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert!(
            crate::db::get_journal_row(&j.db, "w", 0).unwrap().is_none(),
            "a failed exec must not journal"
        );
        j.next_seq = 0; // restart retries the same seq — and this time records it
        let r = j.journaled("t", &Req { x: 1 }, || Ok(Resp { y: 5 })).unwrap();
        assert_eq!(r, Resp { y: 5 });
    }
}
