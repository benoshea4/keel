// function.rs — micro-cloud phase 4: stateless serverless functions.
//
// A function is a component implementing `world handler` (../wit, 0.7.0):
// fresh sandboxed instance per request, fuel/epoch/memory quotas from the
// bound route, NO durability — no journal, no replay, direct now/random (the
// ext spec's §E1: never let journal code leak into this path). Durability is
// one door over: a function that needs reliability calls `start-workflow`,
// which starts a real journaled workflow.
//
// Bindgen #3 (after host.rs `workflow` and provider.rs's two provider worlds)
// — its own module, so the handler world's inline http-request/http-response
// records can't collide with host-api's types.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use wasmtime::component::{bindgen, HasSelf, Linker};
use wasmtime::Store;

use crate::db;
use crate::runner::EngineShared;
use crate::sandbox::{classify, MemLimiter, Outcome, Quota};

bindgen!({
    path: "../wit",
    world: "handler",
});

use keel::workflow::platform_api;

/// Amendment 1 (A2) — host-side caps on captured guest logs. The guest is
/// untrusted: it can call `log` in a loop with 10 MiB strings; what lands in
/// fn_logs is bounded regardless.
const MAX_LOG_LINES: usize = 256;
const MAX_LINE_BYTES: usize = 2048;

/// Amendment 2 (A7) — the kv caps, public-plane discipline: a tokenless
/// caller must not be able to grow the database. Named in every err.
/// v4.0 (E5): wasi:keyvalue shares this exact enforcement — one wall.
const KV_KEY_MAX: usize = 256;
const KV_VALUE_MAX: usize = 64 * 1024;
const KV_MAX_KEYS: i64 = 1024;
const KV_MAX_BYTES: i64 = 8 * 1024 * 1024;

/// The ONE bounded kv write path (A7 + E5): door checks, then cap check +
/// upsert under an IMMEDIATE txn (concurrent writers to a ref serialize, so
/// the per-ref totals can't be over-admitted by a race). Commit is the
/// durability point — the row is on disk before this returns.
pub(crate) fn kv_set_bounded(
    db: &mut rusqlite::Connection,
    kind: &str,
    refname: &str,
    key: &str,
    value: &[u8],
) -> Result<(), String> {
    if key.len() > KV_KEY_MAX {
        return Err(format!("key exceeds {KV_KEY_MAX} bytes"));
    }
    if value.len() > KV_VALUE_MAX {
        return Err(format!("value exceeds {KV_VALUE_MAX} bytes"));
    }
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("engine error: {e:#}"))?;
    let (keys, bytes) =
        db::kv_usage(&tx, kind, refname).map_err(|e| format!("engine error: {e:#}"))?;
    // Overwrites of an existing key must not trip the key-count cap; the byte
    // cap counts the old value until the upsert lands — safely conservative.
    let existing =
        db::kv_get(&tx, kind, refname, key).map_err(|e| format!("engine error: {e:#}"))?;
    if existing.is_none() && keys >= KV_MAX_KEYS {
        return Err(format!("ref already holds {KV_MAX_KEYS} keys"));
    }
    if bytes + value.len() as i64 > KV_MAX_BYTES {
        return Err(format!("ref would exceed {KV_MAX_BYTES} total value bytes"));
    }
    db::kv_set(&tx, kind, refname, key, value).map_err(|e| format!("engine error: {e:#}"))?;
    tx.commit().map_err(|e| format!("engine error: {e:#}"))
}

/// Truncate to at most `max` bytes without splitting a UTF-8 char.
fn truncate_line(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

/// Store data for one function invocation. One connection per invocation
/// (db::open_conn, never shared across threads — ext spec §E1); the limiter
/// is both the cap and the meter.
pub struct FnCtx {
    pub db: rusqlite::Connection,
    pub shared: Arc<EngineShared>,
    pub mem_limiter: MemLimiter,
    /// A2 — `log` lines collected during the run, batch-written after it
    /// (with the invocation's ledger rowid) so log I/O never slows the guest.
    pub logs: Vec<String>,
    /// Lines past MAX_LOG_LINES: counted, dropped, one marker line at the end.
    pub logs_dropped: u64,
    /// Amendment 2 — the invocation's ref identity ('function'|'app' + the
    /// route prefix or app name): config-get and the kv calls scope to it.
    pub kind: String,
    pub refname: String,
}

impl platform_api::Host for FnCtx {
    fn log(&mut self, msg: String) {
        tracing::info!("fn: {msg}");
        if self.logs.len() < MAX_LOG_LINES {
            self.logs.push(truncate_line(msg, MAX_LINE_BYTES));
        } else {
            self.logs_dropped += 1;
        }
    }

    fn now_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn random_u64(&mut self) -> u64 {
        // Same no-new-deps trick as host.rs: a v4 uuid is 122 random bits;
        // unlike the workflow version there's no journal/textual-compare
        // constraint here, so the full 64 bits pass through.
        u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap())
    }

    fn start_workflow(&mut self, module_hash: String, input: String) -> Result<String, String> {
        let known = db::module_exists(&self.db, &module_hash)
            .map_err(|e| format!("engine error: {e:#}"))?;
        if !known {
            return Err(format!("unknown module hash {module_hash}"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        // Same crash-safe order as Engine::start_workflow: the row COMMITS
        // before the spawn (die between the two → recovery picks it up).
        db::create_workflow(&self.db, &id, &module_hash, &input)
            .map_err(|e| format!("engine error: {e:#}"))?;
        // sanctioned spawn call-site 5 of 5 (§0, amended by ext spec Task 4.2)
        crate::runner::spawn(self.shared.clone(), id.clone());
        Ok(id)
    }

    fn get_workflow(&mut self, id: String) -> Result<String, String> {
        match db::get_workflow(&self.db, &id) {
            Ok(Some(wf)) => Ok(db::workflow_json(&wf).to_string()),
            Ok(None) => Err(format!("unknown workflow id {id}")),
            Err(e) => Err(format!("engine error: {e:#}")),
        }
    }

    // --- Amendment 2 (v3.5): config + durable kv, all scoped (kind, ref) ----

    fn config_get(&mut self, name: String) -> Option<String> {
        // A db error here degrades to `none` — the guest surface has no error
        // case by design (A6); the operator's log gets the truth.
        db::get_config(&self.db, &self.kind, &self.refname, &name).unwrap_or_else(|e| {
            tracing::error!("config-get({name}) for {} {}: {e:#}", self.kind, self.refname);
            None
        })
    }

    fn kv_get(&mut self, key: String) -> Option<Vec<u8>> {
        db::kv_get(&self.db, &self.kind, &self.refname, &key).unwrap_or_else(|e| {
            tracing::error!("kv-get({key}) for {} {}: {e:#}", self.kind, self.refname);
            None
        })
    }

    fn kv_set(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        let (kind, refname) = (self.kind.clone(), self.refname.clone());
        kv_set_bounded(&mut self.db, &kind, &refname, &key, &value)
    }

    fn kv_delete(&mut self, key: String) {
        if let Err(e) = db::kv_delete(&self.db, &self.kind, &self.refname, &key) {
            tracing::error!("kv-delete({key}) for {} {}: {e:#}", self.kind, self.refname);
        }
    }
}

/// v4.0 — a world-agnostic HTTP response (the proxy path has no bindgen
/// HttpResponse; the dispatcher translates either shape identically).
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Everything the ledger + HTTP layer need to know about one invocation.
/// Exactly one of `response` (handler world) / `raw_response` (proxy world)
/// is Some, and only for Outcome::Ok.
pub struct Invocation {
    pub outcome: Outcome,
    pub response: Option<HttpResponse>,
    pub raw_response: Option<RawResponse>,
    pub fuel_used: u64,
    pub peak_mem: usize,
    pub duration_ms: u64,
}

/// A2, shared by both worlds (v4.0): apply the host-side caps and batch-write
/// captured lines against the ledger row. Best-effort by design — a failed
/// log write must never convert a finished invocation into a 500.
pub(crate) fn capture_lines(
    conn: &rusqlite::Connection,
    kind: &str,
    refname: &str,
    invocation_id: i64,
    lines: Vec<String>,
) {
    if lines.is_empty() {
        return;
    }
    let dropped = lines.len().saturating_sub(MAX_LOG_LINES);
    let mut capped: Vec<String> = lines
        .into_iter()
        .take(MAX_LOG_LINES)
        .map(|l| truncate_line(l, MAX_LINE_BYTES))
        .collect();
    if dropped > 0 {
        capped.push(format!(
            "({dropped} lines dropped — {MAX_LOG_LINES}/invocation cap)"
        ));
    }
    if let Err(e) = db::insert_fn_logs(conn, kind, refname, invocation_id, &capped) {
        tracing::error!("fn_logs write failed for {kind} {refname}: {e:#}");
    }
}

// --- Amendment 1 (A1): rate-limit admission, off the ledger ------------------

/// The rolling admission window. rate_limit = max admitted runs per this.
pub const RATE_WINDOW_MS: i64 = 60_000;

/// Result of an admission check: run it, or tell the caller when to retry.
pub enum Admission {
    Admitted(AdmitGuard),
    Limited { retry_after_ms: i64 },
}

/// Holds one in-flight admission slot; Drop releases it, so an engine fault
/// (or a panic unwinding through the blocking closure) can never leak one.
pub struct AdmitGuard {
    shared: Arc<EngineShared>,
    /// None = the ref is unlimited and no slot was taken.
    key: Option<String>,
}

impl Drop for AdmitGuard {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            // Poison-tolerant, matching Permit::drop: a release path must NEVER
            // panic — a panic in Drop during unwind aborts the whole process,
            // taking every workflow thread with it.
            let mut m = self
                .shared
                .fn_inflight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(n) = m.get_mut(key) {
                *n -= 1;
                if *n == 0 {
                    m.remove(key); // map hygiene: idle refs hold no entry
                }
            }
        }
    }
}

/// Admit or reject one request against `rate_limit` (None = unlimited).
///
/// The DURABLE window state is the invocations ledger itself (the amendment's
/// "off the ledger" — restart-safe and observable by construction); the
/// in-memory inflight term closes the admission-to-row gap so a concurrent
/// burst of N can't oversubscribe. Count-and-increment happens under ONE
/// lock; the ledger COUNT under it is a μs-scale indexed read that only
/// rate-limited refs ever pay.
pub fn admit(
    shared: &Arc<EngineShared>,
    conn: &rusqlite::Connection,
    kind: &str,
    refname: &str,
    rate_limit: Option<i64>,
) -> Result<Admission> {
    let Some(limit) = rate_limit else {
        return Ok(Admission::Admitted(AdmitGuard {
            shared: shared.clone(),
            key: None,
        }));
    };
    // '\0' can't appear in a route prefix or app name — collision-proof key.
    let key = format!("{kind}\0{refname}");
    let now = crate::journal::now_ms();
    let since = now - RATE_WINDOW_MS;
    let mut m = shared
        .fn_inflight
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inflight = *m.get(&key).unwrap_or(&0) as i64;
    let recent = db::recent_invocation_count(conn, kind, refname, since)?;
    if inflight + recent >= limit {
        drop(m);
        shared
            .fn_rate_limited
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The honest Retry-After: when the oldest in-window row ages out. A
        // window that is pure in-flight runs frees in however long those runs
        // take — "shortly" (1s) beats a flat 60.
        let retry_after_ms = match db::oldest_invocation_since(conn, kind, refname, since)? {
            Some(oldest) => (oldest + RATE_WINDOW_MS - now).max(1),
            None => 1000,
        };
        return Ok(Admission::Limited { retry_after_ms });
    }
    *m.entry(key.clone()).or_insert(0) += 1;
    drop(m);
    Ok(Admission::Admitted(AdmitGuard {
        shared: shared.clone(),
        key: Some(key),
    }))
}

/// The dispatcher core (ext spec Tasks 4.3/6.1 share it): run ONE request
/// through a handler component under the given quotas, classify the outcome,
/// and ALWAYS write the `invocations` ledger row — metering that only counts
/// successes is fiction, so the row is written on every path, failure
/// included. `kind` is 'function'|'app'; `refname` is the route prefix or app
/// name. Err = the ENGINE failed (db/compile trouble), not the guest.
pub fn invoke_handler(
    shared: &Arc<EngineShared>,
    kind: &str,
    refname: &str,
    module_hash: &str,
    quota: Quota,
    req: HttpRequest,
) -> Result<Invocation> {
    let conn = db::open_conn(&shared.db_path)?;
    // v3.3 (P-FIX-3): hash-first — the module BLOB is only read on a compile
    // miss, not copied out of SQLite on every request.
    let component = shared.component_cached(module_hash, || {
        db::get_module_wasm(&conn, module_hash)?
            .with_context(|| format!("module {module_hash} not found"))
    })?;

    let ctx = FnCtx {
        db: conn,
        shared: shared.clone(),
        mem_limiter: MemLimiter {
            limit: quota.mem,
            peak: 0,
            denied: false,
        },
        logs: Vec::new(),
        logs_dropped: 0,
        kind: kind.to_string(),
        refname: refname.to_string(),
    };
    let mut store = Store::new(&shared.engine, ctx);
    store.limiter(|c| &mut c.mem_limiter);
    // The FUNCTION limit profile (ext spec §E1): real quotas on both
    // dimensions — fuel for compute, epoch deadline for wall time (deadline
    // expiry TRAPS: no callback, unlike workflow/provider stores).
    store.set_fuel(quota.fuel)?;
    store.set_epoch_deadline(quota.time_ms.div_ceil(100).max(1));
    store.epoch_deadline_trap();

    let started = std::time::Instant::now();
    let result = (|| -> Result<HttpResponse, wasmtime::Error> {
        let mut linker: Linker<FnCtx> = Linker::new(&shared.engine);
        Handler::add_to_linker::<_, HasSelf<FnCtx>>(&mut linker, |c| c)?;
        let h = Handler::instantiate(&mut store, &component, &linker)?;
        h.call_handle(&mut store, &req)
    })();
    let duration_ms = started.elapsed().as_millis() as u64;

    let fuel_used = quota.fuel - store.get_fuel().unwrap_or(0);
    let ctx = store.into_data();
    let outcome = classify(&result, &ctx.mem_limiter);
    let invocation_id = db::insert_invocation(
        &ctx.db,
        kind,
        refname,
        module_hash,
        outcome.as_str(),
        Some(fuel_used as i64),
        Some(ctx.mem_limiter.peak as i64),
        duration_ms as i64,
    )
    .context("recording invocation")?;
    // A2 — captured log lines land AFTER the ledger row, tagged with its id.
    // Best-effort by design: a failed log write must not convert a finished
    // invocation into a 500 (the ledger row is already committed — metering
    // outranks observability).
    if !ctx.logs.is_empty() || ctx.logs_dropped > 0 {
        let mut lines = ctx.logs;
        if ctx.logs_dropped > 0 {
            lines.push(format!(
                "({} lines dropped — {MAX_LOG_LINES}/invocation cap)",
                ctx.logs_dropped
            ));
        }
        if let Err(e) = db::insert_fn_logs(&ctx.db, kind, refname, invocation_id, &lines) {
            tracing::error!("fn_logs write failed for {kind} {refname}: {e:#}");
        }
    }
    Ok(Invocation {
        outcome,
        response: result.ok(),
        raw_response: None,
        fuel_used,
        peak_mem: ctx.mem_limiter.peak,
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{admit, truncate_line, Admission, RATE_WINDOW_MS};
    use crate::db;

    /// v3.4 (R.4): admit()'s ledger+inflight interplay, unit-level at last —
    /// the §P hedge ("if EngineShared is too heavy…") died with v3.3's
    /// testutil. Rows are seeded through the real insert path and backdated
    /// with direct SQL, exactly how the operate gate manipulates the window.
    fn seeded(name: &str) -> (Arc<crate::runner::EngineShared>, rusqlite::Connection) {
        let shared = Arc::new(crate::runner::testutil::shared(name, 8));
        let conn = db::open_conn(&shared.db_path).expect("open scratch db");
        (shared, conn)
    }

    fn ledger_row(conn: &rusqlite::Connection, kind: &str, refname: &str) {
        db::insert_invocation(conn, kind, refname, "hash", "ok", Some(1), Some(1), 1)
            .expect("seed invocation");
    }

    #[test]
    fn admit_unlimited_takes_no_slot() {
        let (shared, conn) = seeded("admit-unlimited");
        for _ in 0..3 {
            match admit(&shared, &conn, "function", "/fn/u", None).unwrap() {
                Admission::Admitted(_g) => {}
                Admission::Limited { .. } => panic!("unlimited ref was limited"),
            }
        }
        assert!(shared.fn_inflight.lock().unwrap().is_empty(), "no slots for unlimited refs");
    }

    #[test]
    fn admit_counts_ledger_plus_inflight_and_guard_drop_frees() {
        let (shared, conn) = seeded("admit-count");
        ledger_row(&conn, "function", "/fn/c"); // recent = 1
        let g1 = match admit(&shared, &conn, "function", "/fn/c", Some(2)).unwrap() {
            Admission::Admitted(g) => g, // 1 recent + 0 inflight < 2
            Admission::Limited { .. } => panic!("first admission must pass"),
        };
        match admit(&shared, &conn, "function", "/fn/c", Some(2)).unwrap() {
            Admission::Limited { retry_after_ms } => {
                assert!((1..=RATE_WINDOW_MS).contains(&retry_after_ms), "{retry_after_ms}");
            }
            Admission::Admitted(_) => panic!("1 recent + 1 inflight must hit limit 2"),
        }
        drop(g1); // the guard IS the release path (engine faults, panics)
        assert!(shared.fn_inflight.lock().unwrap().is_empty(), "drop must clear the slot");
        match admit(&shared, &conn, "function", "/fn/c", Some(2)).unwrap() {
            Admission::Admitted(_g) => {}
            Admission::Limited { .. } => panic!("slot freed — must admit again"),
        }
    }

    #[test]
    fn admit_inflight_only_window_says_retry_shortly() {
        let (shared, conn) = seeded("admit-inflight");
        let _g = match admit(&shared, &conn, "function", "/fn/i", Some(1)).unwrap() {
            Admission::Admitted(g) => g,
            Admission::Limited { .. } => panic!("empty window must admit"),
        };
        match admit(&shared, &conn, "function", "/fn/i", Some(1)).unwrap() {
            // No ledger row to age out — the honest answer is the 1s floor.
            Admission::Limited { retry_after_ms } => assert_eq!(retry_after_ms, 1000),
            Admission::Admitted(_) => panic!("inflight slot must count toward the limit"),
        }
    }

    #[test]
    fn admit_frees_when_the_window_ages_out() {
        let (shared, conn) = seeded("admit-age");
        ledger_row(&conn, "function", "/fn/a");
        match admit(&shared, &conn, "function", "/fn/a", Some(1)).unwrap() {
            Admission::Limited { retry_after_ms } => {
                assert!((1..=RATE_WINDOW_MS).contains(&retry_after_ms));
            }
            Admission::Admitted(_) => panic!("full window must limit"),
        }
        conn.execute(
            "UPDATE invocations SET created_at = created_at - ?1",
            [RATE_WINDOW_MS + 1000],
        )
        .unwrap();
        match admit(&shared, &conn, "function", "/fn/a", Some(1)).unwrap() {
            Admission::Admitted(_g) => {}
            Admission::Limited { .. } => panic!("aged-out window must admit"),
        }
    }

    /// v3.5 (A6/A7): the host surface itself, driven as the guest would.
    fn fn_ctx(name: &str, kind: &str, refname: &str) -> super::FnCtx {
        let shared = Arc::new(crate::runner::testutil::shared(name, 8));
        let conn = db::open_conn(&shared.db_path).expect("open scratch db");
        super::FnCtx {
            db: conn,
            shared,
            mem_limiter: crate::sandbox::MemLimiter {
                limit: 1024 * 1024,
                peak: 0,
                denied: false,
            },
            logs: Vec::new(),
            logs_dropped: 0,
            kind: kind.to_string(),
            refname: refname.to_string(),
        }
    }

    #[test]
    fn kv_roundtrip_delete_and_value_cap() {
        use super::platform_api::Host as _;
        let mut ctx = fn_ctx("kv-host", "function", "/fn/t");
        assert_eq!(ctx.kv_get("k".into()), None);
        ctx.kv_set("k".into(), b"v1".to_vec()).unwrap();
        assert_eq!(ctx.kv_get("k".into()), Some(b"v1".to_vec()));
        ctx.kv_set("k".into(), b"v2".to_vec()).unwrap(); // overwrite, not a new key
        assert_eq!(ctx.kv_get("k".into()), Some(b"v2".to_vec()));
        let err = ctx
            .kv_set("big".into(), vec![b'x'; super::KV_VALUE_MAX + 1])
            .unwrap_err();
        assert!(err.contains(&super::KV_VALUE_MAX.to_string()), "{err}");
        let err = ctx
            .kv_set("x".repeat(super::KV_KEY_MAX + 1), b"v".to_vec())
            .unwrap_err();
        assert!(err.contains(&super::KV_KEY_MAX.to_string()), "{err}");
        ctx.kv_delete("k".into());
        assert_eq!(ctx.kv_get("k".into()), None);
        ctx.kv_delete("k".into()); // idempotent
    }

    #[test]
    fn config_get_is_ref_scoped() {
        use super::platform_api::Host as _;
        let mut ctx = fn_ctx("cfg-host", "function", "/fn/a");
        db::upsert_config(&ctx.db, "function", "/fn/a", "API_KEY", "sk-123").unwrap();
        db::upsert_config(&ctx.db, "function", "/fn/OTHER", "API_KEY", "sk-999").unwrap();
        assert_eq!(ctx.config_get("API_KEY".into()), Some("sk-123".into()));
        assert_eq!(ctx.config_get("MISSING".into()), None);
        let mut other = fn_ctx("cfg-host2", "app", "a");
        assert_eq!(
            other.config_get("API_KEY".into()),
            None,
            "kind 'app' must not see kind 'function' config"
        );
    }

    #[test]
    fn truncate_line_respects_utf8_boundaries() {
        assert_eq!(truncate_line("short".into(), 2048), "short");
        assert_eq!(truncate_line("abcdef".into(), 4), "abcd");
        // 'é' is 2 bytes; a cut landing mid-char must back up, not panic.
        let s = "aé".repeat(1000); // 3000 bytes
        let t = truncate_line(s, 4);
        assert_eq!(t, "aéa"); // byte 4 splits the second 'é' → backs up to 4-1
        let exact = truncate_line("éé".into(), 4);
        assert_eq!(exact, "éé");
    }
}
