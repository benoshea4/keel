// fleet.rs — v2 cell tenancy: one keel process + one database + one token per
// tenant, spawned and supervised from a TOML config. This is the WHOLE
// multi-tenancy model (VISION.md): isolation comes from the process boundary,
// not from tenant columns — there is no shared state to leak across, no shared
// SQLite writer lock to contend on, and a tenant's blast radius is itself.
//
// Supervision is crash-only: children are hard-killed (never asked nicely) and
// restarted 1s after they die, because kill -9 at any instant is a SUPPORTED
// shutdown of keel serve — the journal makes it safe. Fleet itself dying takes
// no children with it (no process groups here); in production run fleet under
// systemd and let it restart the whole tree. Routing/TLS is a reverse proxy's
// job (one Caddy host per tenant port — docs/operations.md).
//
// Config format (docs/operations.md):
//
//   [[tenants]]
//   name = "acme"            # [a-z0-9-], unique
//   port = 9101              # 127.0.0.1 port, unique
//   db = "acme.db"           # database path, unique
//   api_token = "..."        # passed via env, never argv
//   # optional: max_running, max_guest_memory_mb, retain_terminal_hours,
//   #           backup_dir, backup_interval_secs, backup_keep, secrets_file

use std::collections::HashSet;
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct FleetConfig {
    tenants: Vec<Tenant>,
}

#[derive(Deserialize, Clone)]
struct Tenant {
    name: String,
    port: u16,
    db: String,
    api_token: String,
    max_running: Option<u32>,
    max_guest_memory_mb: Option<usize>,
    retain_terminal_hours: Option<u64>,
    backup_dir: Option<String>,
    backup_interval_secs: Option<u64>,
    backup_keep: Option<usize>,
    secrets_file: Option<String>,
}

fn validate(cfg: &FleetConfig) -> Result<()> {
    if cfg.tenants.is_empty() {
        bail!("fleet config has no [[tenants]]");
    }
    let mut names = HashSet::new();
    let mut ports = HashSet::new();
    let mut dbs = HashSet::new();
    for t in &cfg.tenants {
        if t.name.is_empty()
            || !t
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!("tenant name '{}' must be non-empty [a-z0-9-]", t.name);
        }
        if !names.insert(&t.name) {
            bail!("duplicate tenant name '{}'", t.name);
        }
        if !ports.insert(t.port) {
            bail!("duplicate port {} (tenant '{}')", t.port, t.name);
        }
        if !dbs.insert(&t.db) {
            bail!("duplicate db path '{}' (tenant '{}')", t.db, t.name);
        }
        if t.api_token.is_empty() {
            bail!("tenant '{}' has an empty api_token — cells must be tokened", t.name);
        }
    }
    Ok(())
}

fn spawn_tenant(exe: &std::path::Path, t: &Tenant) -> Result<Child> {
    // Child output goes to keel-<name>.log (append across restarts) so tenants
    // don't interleave on fleet's stdout.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("keel-{}.log", t.name))
        .with_context(|| format!("opening keel-{}.log", t.name))?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--db")
        .arg(&t.db)
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", t.port))
        // The token travels via env, never argv — argv is world-readable in ps.
        .env("KEEL_API_TOKEN", &t.api_token)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if let Some(v) = t.max_running {
        cmd.arg("--max-running").arg(v.to_string());
    }
    if let Some(v) = t.max_guest_memory_mb {
        cmd.arg("--max-guest-memory-mb").arg(v.to_string());
    }
    if let Some(v) = t.retain_terminal_hours {
        cmd.arg("--retain-terminal-hours").arg(v.to_string());
    }
    if let Some(v) = &t.backup_dir {
        cmd.arg("--backup-dir").arg(v);
    }
    if let Some(v) = t.backup_interval_secs {
        cmd.arg("--backup-interval-secs").arg(v.to_string());
    }
    if let Some(v) = t.backup_keep {
        cmd.arg("--backup-keep").arg(v.to_string());
    }
    if let Some(v) = &t.secrets_file {
        cmd.arg("--secrets-file").arg(v);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawning tenant '{}'", t.name))?;
    tracing::info!(
        "tenant '{}' up: pid {} on 127.0.0.1:{} (db {}, log keel-{}.log)",
        t.name,
        child.id(),
        t.port,
        t.db,
        t.name
    );
    Ok(child)
}

pub async fn run(config_path: &str) -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading fleet config {config_path}"))?;
    let cfg: FleetConfig = toml::from_str(&raw).context("parsing fleet config")?;
    validate(&cfg)?;
    let exe = std::env::current_exe().context("resolving keel binary path")?;

    let mut cells: Vec<(Tenant, Child)> = Vec::new();
    for t in &cfg.tenants {
        let child = spawn_tenant(&exe, t)?;
        cells.push((t.clone(), child));
    }
    tracing::info!("fleet up: {} tenants (ctrl-c stops them all)", cells.len());

    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c: killing {} tenants (journal makes hard kills safe)", cells.len());
                for (t, child) in cells.iter_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::info!("tenant '{}' stopped", t.name);
                }
                return Ok(());
            }
            _ = tick.tick() => {
                for (t, child) in cells.iter_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        tracing::warn!("tenant '{}' exited ({status}) — restarting in 1s", t.name);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        match spawn_tenant(&exe, t) {
                            Ok(c) => *child = c,
                            Err(e) => tracing::error!("tenant '{}' respawn failed: {e:#}", t.name),
                        }
                    }
                }
            }
        }
    }
}
