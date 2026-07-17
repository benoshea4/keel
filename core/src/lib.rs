// keel-core — the embeddable durable-execution engine (v2.2).
//
// The `keel` binary is a thin consumer of this crate: HTTP API, UI, auth,
// fleet supervision and the scheduler/backup/GC loops live THERE; everything
// journal-shaped lives HERE. An embedder gets the same guarantees the binary
// gets — journal-before-return, replay recovery, epoch-interruptible guests —
// via the `Engine` façade below (examples/embedded.rs is the two-minute tour).
//
// What the façade deliberately does NOT cover (yet): live upgrade and cancel
// (they orchestrate abort/join/claim sequences that today live in the
// binary's api.rs) and the schedule/backup/GC background loops. Embedders who
// need those drive the public modules directly, the way api.rs does.

pub mod cron;
pub mod db;
pub mod host;
pub mod journal;
pub mod notifier;
pub mod provider;
pub mod runner;

// Consumers that type a Connection (the binary's ui.rs) use OUR rusqlite —
// re-exported so the workspace can never end up with two versions of it.
pub use rusqlite;

use std::sync::Arc;

use anyhow::{Context, Result};

/// Configuration for an embedded engine. `db_path` is the one required field;
/// the defaults mirror `keel serve`'s flag defaults.
pub struct EngineOptions {
    pub db_path: String,
    /// Worker-thread cap; parked workflows hold a slot (size generously).
    pub max_running: u32,
    /// Per-guest linear-memory cap in bytes.
    pub max_guest_memory: usize,
    /// Operator bearer token — used by the binary's HTTP layer; irrelevant to
    /// most embedders (None).
    pub api_token: Option<String>,
    /// KEY=VALUE file backing the `secret` host call.
    pub secrets_path: Option<String>,
    /// Capability providers: (name, component bytes) — see provider.rs.
    /// PURE tier: must be import-free (the v2.2 guarantee, still enforced).
    pub providers: Vec<(String, Vec<u8>)>,
    /// v2.5 — EFFECTFUL tier: may import keel:provider/host-http and make
    /// real HTTP calls, each journaled individually (PROVIDERS.md). Granting
    /// a provider this tier is an operator decision — names share one
    /// namespace with `providers`.
    pub providers_effectful: Vec<(String, Vec<u8>)>,
}

impl EngineOptions {
    pub fn new(db_path: impl Into<String>) -> Self {
        EngineOptions {
            db_path: db_path.into(),
            max_running: 256,
            max_guest_memory: 256 * 1024 * 1024,
            api_token: None,
            secrets_path: None,
            providers: Vec::new(),
            providers_effectful: Vec::new(),
        }
    }
}

/// An open engine: workflows recover on open, run on their own threads, and
/// survive the process dying at any instant (that is the whole point). Clone
/// the inner Arc via `shared()` for anything the façade doesn't cover.
pub struct Engine {
    shared: Arc<runner::EngineShared>,
}

impl Engine {
    /// Open (creating if missing) + migrate the database, then RECOVER: every
    /// non-terminal workflow is started again from its journal/checkpoint.
    /// Returns once recovery spawns are issued (not completed) — same startup
    /// order the binary uses: recover before accepting new work.
    pub fn open(opts: EngineOptions) -> Result<Engine> {
        let conn = db::open_conn(&opts.db_path)?;
        db::migrate(&conn)?;
        let shared = Arc::new(runner::EngineShared::new(opts)?);
        let resumable = db::resumable_ids(&conn)?;
        // Task 2.7 starvation warning: parked workflows hold permits, so a
        // recovery that nearly fills the cap starves workflow N+1 of a thread
        // until something finishes. (>80%, in integer math: n/max > 4/5.)
        if resumable.len() as u64 * 5 > shared.max_running().max(1) as u64 * 4 {
            tracing::warn!(
                "recovering {} workflows against max_running {}: parked workflows hold \
                 permits, so workflows beyond the cap will starve. Raise max_running \
                 well above your live workflow count.",
                resumable.len(),
                shared.max_running()
            );
        }
        for id in resumable {
            tracing::info!("recovering workflow {id}");
            runner::spawn(shared.clone(), id); // sanctioned spawn call-site 2 of 4 (§0)
        }
        Ok(Engine { shared })
    }

    /// Wrap an already-built EngineShared (the binary's serve() does this so
    /// its HTTP state and the façade are the same engine).
    pub fn from_shared(shared: Arc<runner::EngineShared>) -> Engine {
        Engine { shared }
    }

    /// The shared core, for everything the façade doesn't cover (metrics,
    /// notifier, upgrade/cancel orchestration à la api.rs).
    pub fn shared(&self) -> Arc<runner::EngineShared> {
        self.shared.clone()
    }

    /// Store a module (content-addressed; re-upload is a no-op) → sha256 hex.
    pub fn upload_module(&self, name: &str, wasm: &[u8]) -> Result<String> {
        anyhow::ensure!(
            wasm.starts_with(b"\0asm"),
            "not a WebAssembly binary (missing \\0asm magic)"
        );
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(wasm));
        let conn = db::open_conn(&self.shared.db_path)?;
        db::insert_module(&conn, &hash, name, wasm).context("storing module")?;
        Ok(hash)
    }

    /// Create + start a workflow → its id. `input_json` is opaque JSON text.
    /// The row is committed BEFORE the spawn (crash between the two = the
    /// next open()'s recovery picks it up).
    pub fn start_workflow(&self, module_hash: &str, input_json: &str) -> Result<String> {
        let conn = db::open_conn(&self.shared.db_path)?;
        anyhow::ensure!(
            db::module_exists(&conn, module_hash)?,
            "unknown module hash {module_hash}"
        );
        let id = uuid::Uuid::new_v4().to_string();
        db::create_workflow(&conn, &id, module_hash, input_json)?;
        runner::spawn(self.shared.clone(), id.clone()); // sanctioned spawn call-site 1 of 4 (§0)
        Ok(id)
    }

    /// A workflow's current row (status/output/timestamps), or None.
    pub fn workflow(&self, id: &str) -> Result<Option<db::WorkflowRow>> {
        let conn = db::open_conn(&self.shared.db_path)?;
        db::get_workflow(&conn, id)
    }
}
