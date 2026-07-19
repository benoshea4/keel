// proxy.rs — v4.0 (SPEC-AMENDMENT-3.md): the wasi:http/proxy compatibility
// runner. Ecosystem components (Spin, componentize-js/JCO, anything targeting
// wasi:http@0.2.x) run behind the SAME walls as keel-WIT functions: the bound
// route's quotas, the global permit, the admission window, one ledger row per
// request. The host surface here is the wasmtime-wasi(-http) reference
// implementation (E1 — the one deliberate wasi dependency family), linked
// SYNC on the one existing engine; wasi:keyvalue is keel's own five-function
// Host impl over the Amendment-2 fn_kv table (E5).
//
// The sync-embedding shape that matters: the guest writes its response body
// into a channel with a 1-chunk buffer, so a COLLECTOR task must consume
// concurrently on wasmtime-wasi's internal runtime while the guest runs on
// this blocking thread — collect-after-return would deadlock on bodies past
// one chunk. The collector caps the response at RESP_CAP (E3).

use std::sync::Arc;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http_body_util::BodyExt as _;
use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::Store;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::http::types::{ErrorCode, Scheme};
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::p2::{
    default_send_request, types as http_types, HttpResult, WasiHttpCtxView, WasiHttpHooks,
    WasiHttpView,
};
use wasmtime_wasi_http::WasiHttpCtx;

use crate::db;
use crate::function::{Invocation, RawResponse};
use crate::runner::EngineShared;
use crate::sandbox::{classify, MemLimiter, Outcome, Quota};

// Host-side bindgen for keel's own wasi:keyvalue implementation (E5). The
// wit under ../wit-wasi-keyvalue is the SAME vendored file the proxy
// fixtures build against.
mod kv_bindings {
    wasmtime::component::bindgen!({
        path: "../wit-wasi-keyvalue",
        world: "kv-bindings",
    });
}
use kv_bindings::wasi::keyvalue::store as kv;

/// E3 — symmetric with the 10 MiB request cap.
const RESP_CAP: usize = 10 * 1024 * 1024;
/// Stdout/stderr capture caps (feeds the A2 log pipeline, which caps again).
const STDIO_CAP: usize = 64 * 1024;

/// Which world a compiled component speaks (E2). Detected from the export
/// surface, never at bind time (accept_harden pins bind-as-existence-only).
#[derive(Clone, Copy, PartialEq)]
pub enum GuestWorld {
    Handler,
    Proxy,
}

/// E2 — a keel `handle` export means world handler; a wasi:http
/// incoming-handler instance export means the proxy world. Cached by hash in
/// EngineShared (a type walk is cheap; the cache makes it free).
pub fn world_of(shared: &EngineShared, hash: &str, component: &Component) -> Option<GuestWorld> {
    if let Some(w) = shared.guest_worlds.lock().unwrap().get(hash) {
        return Some(*w);
    }
    let ty = component.component_type();
    let mut found = None;
    for (name, _) in ty.exports(&shared.engine) {
        if name == "handle" {
            found = Some(GuestWorld::Handler);
            break;
        }
        if name.starts_with("wasi:http/incoming-handler@0.2") {
            found = Some(GuestWorld::Proxy);
            break;
        }
    }
    if let Some(w) = found {
        shared
            .guest_worlds
            .lock()
            .unwrap()
            .insert(hash.to_string(), w);
    }
    found
}

/// E4 — outbound HTTP is an operator grant, default deny. The hooks are the
/// interception point wasmtime-wasi-http provides; denial is DATA to the
/// guest (an error-code its own error handling sees), never a trap.
struct KeelHooks {
    allow_outbound: bool,
    outbound_made: u64,
}

impl WasiHttpHooks for KeelHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<HyperOutgoingBody>,
        config: http_types::OutgoingRequestConfig,
    ) -> HttpResult<http_types::HostFutureIncomingResponse> {
        if !self.allow_outbound {
            return Ok(http_types::HostFutureIncomingResponse::ready(Ok(Err(
                ErrorCode::HttpRequestDenied,
            ))));
        }
        self.outbound_made += 1;
        Ok(default_send_request(request, config))
    }
}

/// Store data for one proxy invocation — the wasi views plus keel's meters
/// and the kv identity.
struct ProxyCtx {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: KeelHooks,
    mem_limiter: MemLimiter,
    db: rusqlite::Connection,
    kind: String,
    refname: String,
}

impl WasiView for ProxyCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for ProxyCtx {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// E5 — wasi:keyvalue over fn_kv: the same table, the same caps, the same
/// (kind, ref) scoping as platform-api kv. Only the default bucket exists.
struct Bucket;

impl kv::Host for ProxyCtx {
    fn open(&mut self, identifier: String) -> Result<Resource<kv::Bucket>, kv::Error> {
        if !identifier.is_empty() && identifier != "default" {
            return Err(kv::Error::NoSuchStore);
        }
        self.table
            .push(Bucket)
            .map(|r| Resource::new_own(r.rep()))
            .map_err(|e| kv::Error::Other(format!("{e}")))
    }

}

impl kv::HostBucket for ProxyCtx {
    fn get(&mut self, _b: Resource<kv::Bucket>, key: String) -> Result<Option<Vec<u8>>, kv::Error> {
        db::kv_get(&self.db, &self.kind, &self.refname, &key)
            .map_err(|e| kv::Error::Other(format!("{e:#}")))
    }

    fn set(&mut self, _b: Resource<kv::Bucket>, key: String, value: Vec<u8>) -> Result<(), kv::Error> {
        let (kind, refname) = (self.kind.clone(), self.refname.clone());
        crate::function::kv_set_bounded(&mut self.db, &kind, &refname, &key, &value)
            .map_err(kv::Error::Other)
    }

    fn delete(&mut self, _b: Resource<kv::Bucket>, key: String) -> Result<(), kv::Error> {
        db::kv_delete(&self.db, &self.kind, &self.refname, &key)
            .map_err(|e| kv::Error::Other(format!("{e:#}")))
    }

    fn exists(&mut self, _b: Resource<kv::Bucket>, key: String) -> Result<bool, kv::Error> {
        db::kv_get(&self.db, &self.kind, &self.refname, &key)
            .map(|v| v.is_some())
            .map_err(|e| kv::Error::Other(format!("{e:#}")))
    }

    fn list_keys(
        &mut self,
        _b: Resource<kv::Bucket>,
        _cursor: Option<u64>,
    ) -> Result<kv::KeyResponse, kv::Error> {
        db::kv_keys(&self.db, &self.kind, &self.refname)
            .map(|keys| kv::KeyResponse { keys, cursor: None })
            .map_err(|e| kv::Error::Other(format!("{e:#}")))
    }

    fn drop(&mut self, b: Resource<kv::Bucket>) -> wasmtime::Result<()> {
        let _ = self.table.delete(Resource::<Bucket>::new_own(b.rep()));
        Ok(())
    }
}

/// Run ONE request through a wasi:http/proxy component under keel's walls.
/// Same contract as function::invoke_handler: ALWAYS writes the ledger row;
/// Err = the ENGINE failed. The caller (dispatch) has already admitted the
/// request and holds the global permit.
#[allow(clippy::too_many_arguments)] // 1:1 with the dispatch inputs, like run_function
pub fn invoke_proxy(
    shared: &Arc<EngineShared>,
    kind: &str,
    refname: &str,
    module_hash: &str,
    quota: Quota,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    allow_outbound: bool,
) -> Result<Invocation> {
    let conn = db::open_conn(&shared.db_path)?;
    let component = shared.component_cached(module_hash, || {
        db::get_module_wasm(&conn, module_hash)?
            .with_context(|| format!("module {module_hash} not found"))
    })?;

    // The stdio pipes are read back after the run (E3: ecosystem guests log
    // with stdout, and it lands where keel logs land).
    let stdout = MemoryOutputPipe::new(STDIO_CAP);
    let stderr = MemoryOutputPipe::new(STDIO_CAP);
    let wasi = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();

    let ctx = ProxyCtx {
        table: ResourceTable::new(),
        wasi,
        http: WasiHttpCtx::new(),
        hooks: KeelHooks {
            allow_outbound,
            outbound_made: 0,
        },
        mem_limiter: MemLimiter {
            limit: quota.mem,
            peak: 0,
            denied: false,
        },
        db: conn,
        kind: kind.to_string(),
        refname: refname.to_string(),
    };
    let mut store = Store::new(&shared.engine, ctx);
    store.limiter(|c| &mut c.mem_limiter);
    store.set_fuel(quota.fuel)?;
    store.set_epoch_deadline(quota.time_ms.div_ceil(100).max(1));
    store.epoch_deadline_trap();

    // Incoming request: keel already buffered the body (10 MiB cap at the
    // dispatcher door), so a Full body is exact.
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(if path_and_query.is_empty() { "/" } else { path_and_query });
    for (n, v) in headers {
        req = req.header(n.as_str(), v.as_str());
    }
    let req = req
        .body(
            http_body_util::Full::new(Bytes::from(body))
                .map_err(|_: std::convert::Infallible| ErrorCode::InternalError(None))
                .boxed_unsync(),
        )
        .context("assembling incoming request")?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    // The concurrent collector (see the module header): response head via the
    // oneshot, body frames drained under RESP_CAP while the guest still runs.
    let collector = wasmtime_wasi::runtime::spawn(async move {
        let resp: hyper::Response<HyperOutgoingBody> = match rx.await {
            Ok(Ok(r)) => r,
            Ok(Err(code)) => return CollectOut::GuestErr(format!("error-code: {code}")),
            Err(_) => return CollectOut::NoResponse,
        };
        let status = resp.status().as_u16();
        let hdrs: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(n, v)| Some((n.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let mut body = resp.into_body();
        let mut out: Vec<u8> = Vec::new();
        // Over the cap we keep DRAINING without storing: dropping the
        // receiver mid-write would trap the still-writing guest, and the trap
        // would mask the real verdict. The drain is bounded by the guest's
        // own fuel/time quotas.
        let mut over = false;
        loop {
            match body.frame().await {
                None => break,
                Some(Ok(f)) => {
                    if let Some(data) = f.data_ref() {
                        if over || out.len() + data.len() > RESP_CAP {
                            over = true;
                        } else {
                            out.extend_from_slice(data);
                        }
                    }
                }
                Some(Err(e)) => return CollectOut::GuestErr(format!("body stream: {e}")),
            }
        }
        if over {
            return CollectOut::GuestErr(format!("response body exceeds {RESP_CAP} bytes"));
        }
        CollectOut::Ok(RawResponse {
            status,
            headers: hdrs,
            body: out,
        })
    });

    let started = std::time::Instant::now();
    let call_result = (|| -> Result<(), wasmtime::Error> {
        let mut linker: Linker<ProxyCtx> = Linker::new(&shared.engine);
        wasmtime_wasi_http::p2::add_to_linker_sync(&mut linker)?;
        kv::add_to_linker::<_, wasmtime::component::HasSelf<ProxyCtx>>(&mut linker, |c| c)?;
        let proxy =
            wasmtime_wasi_http::p2::bindings::sync::Proxy::instantiate(&mut store, &component, &linker)?;
        let req_res = store.data_mut().http().new_incoming_request(Scheme::Http, req)?;
        let out_res = store.data_mut().http().new_response_outparam(tx)?;
        proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, req_res, out_res)
    })();
    let duration_ms = started.elapsed().as_millis() as u64;
    let fuel_used = quota.fuel - store.get_fuel().unwrap_or(0);

    // Recover the ctx BEFORE awaiting the collector: dropping the table (and
    // with it the guest's body-stream writer ends) is what lets the stream
    // report EOF instead of hanging the await.
    let ProxyCtx {
        table,
        mem_limiter,
        db: conn,
        hooks,
        ..
    } = store.into_data();
    drop(table);

    let collected = wasmtime_wasi::runtime::in_tokio(async {
        tokio::time::timeout(std::time::Duration::from_secs(5), collector).await
    });

    if hooks.outbound_made > 0 {
        shared
            .fn_outbound
            .fetch_add(hooks.outbound_made, std::sync::atomic::Ordering::Relaxed);
    }

    // Outcome: the wasmtime call classifies traps/fuel/epoch/memory exactly
    // like the handler path; a clean return still needs a RESPONSE to be ok.
    let unit_result: Result<(), wasmtime::Error> = call_result;
    let mut outcome = classify(&unit_result, &mem_limiter);
    let mut response: Option<RawResponse> = None;
    if outcome == Outcome::Ok {
        match collected {
            Ok(CollectOut::Ok(r)) => response = Some(r),
            Ok(CollectOut::GuestErr(e)) => {
                tracing::info!("proxy {kind} {refname}: {e}");
                outcome = Outcome::GuestError;
            }
            Ok(CollectOut::NoResponse) => {
                tracing::info!("proxy {kind} {refname}: guest returned without a response");
                outcome = Outcome::GuestError;
            }
            Err(_) => {
                tracing::error!("proxy {kind} {refname}: response body never completed");
                outcome = Outcome::GuestError;
            }
        }
    }

    let invocation_id = db::insert_invocation(
        &conn,
        kind,
        refname,
        module_hash,
        outcome.as_str(),
        Some(fuel_used as i64),
        Some(mem_limiter.peak as i64),
        duration_ms as i64,
    )
    .context("recording invocation")?;

    // Stdio → the A2 log pipeline (same caps, same tables, same tailing).
    let mut lines: Vec<String> = Vec::new();
    for pipe in [stdout, stderr] {
        let bytes = pipe.contents();
        for line in String::from_utf8_lossy(&bytes).lines() {
            if !line.is_empty() {
                lines.push(line.to_string());
            }
        }
    }
    crate::function::capture_lines(&conn, kind, refname, invocation_id, lines);

    Ok(Invocation {
        outcome,
        response: None,
        raw_response: response,
        fuel_used,
        peak_mem: mem_limiter.peak,
        duration_ms,
    })
}

enum CollectOut {
    Ok(RawResponse),
    GuestErr(String),
    NoResponse,
}
