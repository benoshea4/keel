// client.rs — Amendment 1 (A4): the curl-free platform. Four verbs (deploy,
// bind, run, logs), all THIN clients of the existing HTTP API — no
// engine-side code paths, so anything the CLI can do, curl can do, and the
// API stays the single control plane.
//
// Errors: a non-2xx response surfaces its status and body VERBATIM (the
// server's error messages are already written for humans) and the process
// exits 1 via anyhow. Connection flags ride on every verb: --server /
// KEEL_SERVER, --token / KEEL_API_TOKEN — the same variable the server
// reads, so one exported var makes a shell both a server and a client.

use std::io::Write as _;

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};

use crate::ui::query_enc;

/// Shared connection arguments (clap-flattened into every client verb).
#[derive(clap::Args)]
pub struct Conn {
    /// Engine URL.
    #[arg(long, env = "KEEL_SERVER", default_value = "http://127.0.0.1:8080")]
    pub server: String,
    /// Bearer token — the same variable `keel serve` reads.
    #[arg(long, env = "KEEL_API_TOKEN")]
    pub token: Option<String>,
}

impl Conn {
    fn agent(&self) -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build()
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        let url = format!("{}{}", self.server.trim_end_matches('/'), path);
        let mut r = self.agent().request(method, &url);
        if let Some(t) = &self.token {
            r = r.set("authorization", &format!("Bearer {t}"));
        }
        r
    }

    /// One place turns ureq's three outcomes into ours: 2xx body as JSON,
    /// non-2xx as an error carrying the server's words, transport as itself.
    /// (Parsed by hand — ureq's own into_json sits behind a feature flag
    /// core doesn't enable, and two lines beat feature drift.)
    fn finish(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<Value> {
        match resp {
            Ok(r) => {
                let body = r.into_string().unwrap_or_default();
                Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!("server said {code}: {}", body.trim());
            }
            Err(e) => bail!("cannot reach the engine: {e}"),
        }
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        Self::finish(self.request("GET", path).call())
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        Self::finish(
            self.request("POST", path)
                .set("content-type", "application/json")
                .send_string(&body.to_string()),
        )
    }

    fn post_bytes(&self, path: &str, bytes: &[u8]) -> Result<Value> {
        Self::finish(self.request("POST", path).send_bytes(bytes))
    }

    /// v3.4 — DELETE; a 204 has no body, which finish() maps to Null.
    fn delete(&self, path: &str) -> Result<Value> {
        Self::finish(self.request("DELETE", path).call())
    }

    /// Upload a .wasm file to /api/modules → its content hash.
    fn upload_module(&self, path: &str, name: &str) -> Result<String> {
        let wasm = std::fs::read(path).with_context(|| format!("reading {path}"))?;
        let out = self.post_bytes(&format!("/api/modules?name={}", query_enc(name)), &wasm)?;
        out.get("hash")
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("module upload returned no hash: {out}"))
    }
}

/// A module argument is a .wasm path (uploaded first) or an already-known
/// content hash — a file wins if both readings are possible.
fn resolve_module(conn: &Conn, module: &str, name: Option<&str>) -> Result<String> {
    if std::path::Path::new(module).is_file() {
        let stem = std::path::Path::new(module)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("module");
        return conn.upload_module(module, name.unwrap_or(stem));
    }
    if module.len() == 64 && module.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(module.to_string());
    }
    bail!("'{module}' is neither a .wasm file on disk nor a 64-hex module hash");
}

/// `keel bind <prefix> <file.wasm|hash>` — upload + bind in one step.
#[allow(clippy::too_many_arguments)] // 1:1 with the route's knobs, nothing more
pub fn bind(
    conn: &Conn,
    prefix: &str,
    module: &str,
    fuel: Option<i64>,
    mem_mb: Option<i64>,
    time_ms: Option<i64>,
    rate: Option<i64>,
    name: Option<&str>,
) -> Result<()> {
    let hash = resolve_module(conn, module, name)?;
    let mut body = json!({"prefix": prefix, "module_hash": hash});
    if let Some(f) = fuel {
        body["fuel_limit"] = f.into();
    }
    if let Some(m) = mem_mb {
        body["mem_limit"] = (m * 1024 * 1024).into();
    }
    if let Some(t) = time_ms {
        body["time_limit_ms"] = t.into();
    }
    if let Some(r) = rate {
        body["rate_limit"] = r.into();
    }
    let out = conn.post_json("/api/routes", &body)?;
    println!(
        "bound {prefix} -> {} (fuel {}, mem {}, time {} ms, rate/min {})",
        &hash[..12.min(hash.len())],
        out["fuel_limit"],
        out["mem_limit"],
        out["time_limit_ms"],
        if out["rate_limit"].is_null() { "unlimited".to_string() } else { out["rate_limit"].to_string() },
    );
    println!("try it:  curl {}{prefix}", conn.server);
    Ok(())
}

/// `keel run <file.wasm|hash>` — start a durable workflow and watch it to a
/// terminal state (exit 0 completed / 1 failed); --detach prints the id only.
/// v3.4 (R.2): --timeout N stops WATCHING after N seconds with exit 2 — the
/// workflow keeps running (durable work outlives the shell; scripts need a
/// bound anyway).
pub fn run(conn: &Conn, module: &str, input: &str, detach: bool, timeout: Option<u64>) -> Result<()> {
    let parsed: Value =
        serde_json::from_str(input).with_context(|| format!("--input is not valid JSON: {input}"))?;
    let hash = resolve_module(conn, module, None)?;
    let out = conn.post_json("/api/workflows", &json!({"module_hash": hash, "input": parsed}))?;
    let id = out
        .get("id")
        .and_then(Value::as_str)
        .with_context(|| format!("workflow create returned no id: {out}"))?;
    if detach {
        println!("{id}");
        return Ok(());
    }
    eprintln!("workflow {id}");
    let deadline = timeout.map(|t| std::time::Instant::now() + std::time::Duration::from_secs(t));
    let mut last_status = String::new();
    loop {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            eprintln!(
                "timeout: workflow {id} still '{}' — it keeps running; watch again with \
                 curl {}/api/workflows/{id}",
                if last_status.is_empty() { "starting" } else { &last_status },
                conn.server
            );
            std::process::exit(2);
        }
        let wf = conn.get_json(&format!("/api/workflows/{id}"))?;
        let status = wf["status"].as_str().unwrap_or("").to_string();
        if status != last_status {
            eprintln!("  -> {status}");
            last_status = status.clone();
        }
        match status.as_str() {
            "completed" => {
                if let Some(o) = wf["output"].as_str() {
                    println!("{o}");
                }
                return Ok(());
            }
            "failed" => {
                bail!("workflow failed: {}", wf["output"].as_str().unwrap_or("(no output)"));
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
}

/// `keel logs <ref>` — tail captured platform-api log lines. Kind inferred:
/// a ref starting with '/' is a route prefix, otherwise an app name.
pub fn logs(conn: &Conn, refname: &str, kind: Option<&str>, follow: bool) -> Result<()> {
    let kind = kind.unwrap_or(if refname.starts_with('/') { "function" } else { "app" });
    let base = format!("/api/logs?kind={kind}&ref={}", query_enc(refname));
    let out = conn.get_json(&base)?;
    let mut last_id: i64 = 0;
    let empty = Vec::new();
    for l in out["lines"].as_array().unwrap_or(&empty) {
        println!("{}", l["line"].as_str().unwrap_or(""));
        last_id = l["id"].as_i64().unwrap_or(last_id);
    }
    if !follow {
        return Ok(());
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let out = conn.get_json(&format!("{base}&after={last_id}"))?;
        for l in out["lines"].as_array().unwrap_or(&empty) {
            println!("{}", l["line"].as_str().unwrap_or(""));
            last_id = l["id"].as_i64().unwrap_or(last_id);
        }
        std::io::stdout().flush().ok();
    }
}

/// `keel deploy <dir> --name <app>` — the flagship: directory → running app.
/// Zips <dir> in memory (dot-prefixed entries skipped — .DS_Store is not a
/// frontend), uploads the backend first if given, upserts the app, uploads
/// the bundle, prints the URL. Re-running re-deploys end to end.
pub fn deploy(
    conn: &Conn,
    dir: &str,
    name: &str,
    backend: Option<&str>,
    rate: Option<i64>,
) -> Result<()> {
    let root = std::path::Path::new(dir);
    if !root.is_dir() {
        bail!("'{dir}' is not a directory (point deploy at your built dist/)");
    }
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(root, root, &mut files)?;
    if files.is_empty() {
        bail!("'{dir}' has no files to deploy");
    }
    if !files.iter().any(|(p, _)| p == "index.html") {
        eprintln!("warning: no index.html at the top of '{dir}' — the app root will 404");
    }

    let backend_hash = match backend {
        Some(path) => Some(conn.upload_module(path, &format!("{name}-backend"))?),
        None => None,
    };

    let mut body = json!({"name": name});
    if let Some(h) = &backend_hash {
        body["backend_hash"] = h.clone().into();
    }
    if let Some(r) = rate {
        body["rate_limit"] = r.into();
    }
    conn.post_json("/api/apps", &body)?;

    let zip_bytes = zip_in_memory(&files)?;
    let out = conn.post_bytes(&format!("/api/apps/{}/assets", query_enc(name)), &zip_bytes)?;
    println!(
        "deployed '{name}': {} assets{} -> {}/apps/{name}/",
        out["stored"],
        match &backend_hash {
            Some(h) => format!(", backend {}", &h[..12.min(h.len())]),
            None => String::new(),
        },
        conn.server,
    );
    Ok(())
}

/// Walk `dir` collecting (bundle-relative forward-slash path, bytes).
/// Dot-prefixed files AND directories are skipped at every level.
fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        if file_name.to_string_lossy().starts_with('.') {
            continue;
        }
        // Symlinks are skipped outright: following them invites cycles
        // (infinite recursion), and a built dist/ has no business containing
        // any. file_type() does NOT follow links, unlike path.is_dir().
        if entry.file_type()?.is_symlink() {
            eprintln!("warning: skipping symlink {}", path.display());
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("walked path is under root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            out.push((rel, bytes));
        }
    }
    Ok(())
}

/// Deflate the collected files into one in-memory zip (what the assets
/// endpoint expects as a raw body).
fn zip_in_memory(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (path, bytes) in files {
        w.start_file(path.as_str(), opts)
            .with_context(|| format!("zipping {path}"))?;
        w.write_all(bytes)?;
    }
    Ok(w.finish()?.into_inner())
}

/// `keel ls` — v3.4 (R.2): one screen of what the engine is serving. Reads
/// the same three list endpoints curl would (GET /api/routes, /api/apps —
/// which this stage added — and /api/schedules).
pub fn ls(conn: &Conn) -> Result<()> {
    let routes = conn.get_json("/api/routes")?;
    let apps = conn.get_json("/api/apps")?;
    let schedules = conn.get_json("/api/schedules")?;
    let rate = |v: &Value| {
        if v["rate_limit"].is_null() { "∞".to_string() } else { v["rate_limit"].to_string() }
    };
    println!("routes:");
    for r in routes.as_array().into_iter().flatten() {
        println!(
            "  {:<24} {}  fuel {}  time {} ms  rate/min {}",
            r["prefix"].as_str().unwrap_or("?"),
            &r["module_hash"].as_str().unwrap_or("")[..12.min(r["module_hash"].as_str().unwrap_or("").len())],
            r["fuel_limit"],
            r["time_limit_ms"],
            rate(r),
        );
    }
    println!("apps:");
    for a in apps.as_array().into_iter().flatten() {
        let backend = a["backend_hash"].as_str().unwrap_or("");
        println!(
            "  {:<24} {} assets  backend {}  rate/min {}",
            a["name"].as_str().unwrap_or("?"),
            a["assets"],
            if backend.is_empty() { "-" } else { &backend[..12.min(backend.len())] },
            rate(a),
        );
    }
    println!("schedules:");
    for s in schedules.as_array().into_iter().flatten() {
        let when = s["cron"].as_str().map(str::to_string).unwrap_or_else(|| {
            format!("every {} ms", s["interval_ms"])
        });
        println!(
            "  {:<38} {}  enabled {}",
            s["id"].as_str().unwrap_or("?"),
            when,
            s["enabled"],
        );
    }
    Ok(())
}

/// `keel unbind /fn/x` — v3.4 (R.2): remove a route binding; the module blob
/// stays uploaded (content-addressed — rebind by hash any time).
pub fn unbind(conn: &Conn, prefix: &str) -> Result<()> {
    conn.delete(&format!("/api/routes{prefix}"))?;
    println!("unbound {prefix}");
    Ok(())
}

/// `keel apps rm <name>` — v3.4 (R.2): delete the app + its assets; ledger
/// history remains under --retain-ledger-hours.
pub fn apps_rm(conn: &Conn, name: &str) -> Result<()> {
    conn.delete(&format!("/api/apps/{name}"))?;
    println!("removed app '{name}'");
    Ok(())
}
