// provider.rs — capability providers (PROVIDERS.md is the design doc).
// v2.2: pure tier. v2.5: effectful tier (host-http import, per-effect journaling).
//
// A provider is a wasm component implementing one of the two `keel:provider`
// worlds (../provider-wit):
//
//   * `provider` (pure, --provider): NO imports — no clocks, no sockets, no
//     host-api. Whatever it returns is journaled by the calling workflow's
//     provider-call row, so replay never re-invokes it.
//   * `provider-effectful` (--provider-effectful): may import host-http and
//     make real HTTP calls. Each wire call is journaled INDIVIDUALLY (kind
//     `provider-http:<name>`) through a NESTED JournalCtx that shares the
//     calling workflow's journal and dense seq: the provider scope occupies
//     seqs N..N+k-1 and host.rs writes the `custom:<name>:<kind>` terminal
//     row at N+k afterwards. A crash mid-provider re-invokes the provider on
//     recovery, but its already-committed wire calls replay from the journal
//     instead of re-firing — providers become nested durable functions.
//
// Both tiers: registered at startup, compiled once (fail fast), instantiated
// per call (stateless by construction).

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use wasmtime::component::{bindgen, Component, HasSelf, Linker};
use wasmtime::Store;

use crate::journal::JournalCtx;

bindgen!({
    path: "../provider-wit", // keel/provider-wit/provider.wit, relative to core/
    world: "provider",
});

bindgen!({
    // Same WIT package, second world. Host fns return wasmtime::Result<_> so a
    // journal failure inside the provider can TRAP it (integrity failures must
    // fail the workflow, never launder into a journaled guest-visible err).
    path: "../provider-wit",
    world: "provider-effectful",
    imports: { default: trappable },
});

/// A registered provider: the compiled component plus the tier the OPERATOR
/// granted it at registration (the flag used, not anything the component
/// claims). host.rs dispatches provider-call on `effectful`.
pub struct ProviderEntry {
    pub component: Component,
    pub effectful: bool,
}

/// Per-call store data for the PURE tier: just the memory cap (same figure as
/// guests).
struct ProviderCtx {
    limits: wasmtime::StoreLimits,
}

/// Per-call store data for the EFFECTFUL tier. Fully OWNED (no borrows into
/// the guest's Ctx — wasmtime stores want 'static data): the nested journal
/// scope gets its own Connection to the same db, and host.rs copies the seq
/// cursor in and back out around the call. Safe because the guest is
/// suspended inside provider-call for the whole duration — the two
/// connections never write concurrently.
struct EffCtx {
    limits: wasmtime::StoreLimits,
    j: JournalCtx,
    provider_name: String,
    /// The calling execution's redaction set (Ctx.read_secrets, cloned): a
    /// secret the guest passed into the provider's request must not show up
    /// raw in the provider's journaled wire calls either.
    secrets: Vec<(String, String)>,
    agent: ureq::Agent,
    /// A journal-integrity failure (nondeterministic provider replay, SQLite
    /// error) inside an import. The trap that takes the provider down is just
    /// the vehicle; THIS is the real error, and call_effectful() propagates
    /// it as workflow-fatal instead of a journaled guest-visible err.
    fatal: Option<anyhow::Error>,
    /// Epoch bookkeeping — see the deadline callback in call_effectful().
    returned_from_host: bool,
    ticks_used: u64,
}

impl keel::provider::host_http::Host for EffCtx {
    fn http_request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<String>,
        retry_attempts: u32,
        timeout_ms: u32,
    ) -> wasmtime::Result<Result<keel::provider::host_http::HttpResponse, String>> {
        // Mirrors host.rs http_request field-for-field: redacted journaled
        // request, wire-only idempotency key at THIS row's own seq (so the
        // key is stable across replay and crash-resend, per wire call).
        #[derive(Serialize)]
        struct Req {
            method: String,
            url: String,
            headers: Vec<(String, String)>,
            body: Option<String>,
            retry_attempts: u32,
            timeout_ms: u32,
        }
        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum Resp {
            Ok {
                status: u16,
                headers: Vec<(String, String)>,
                body: String,
            },
            Err {
                err: String,
            },
        }
        let jkind = format!("provider-http:{}", self.provider_name);
        let req = Req {
            method: method.clone(),
            url: crate::host::redact(&url, &self.secrets),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), crate::host::redact(v, &self.secrets)))
                .collect(),
            body: body.as_deref().map(|b| crate::host::redact(b, &self.secrets)),
            retry_attempts,
            timeout_ms,
        };
        let wire_headers =
            crate::host::inject_idempotency_key(headers, &self.j.workflow_id, self.j.next_seq);
        let agent = self.agent.clone();
        let r = self.j.journaled(&jkind, &req, move || {
            Ok(
                match crate::host::do_http_request(
                    &agent,
                    &method,
                    &url,
                    &wire_headers,
                    body.as_deref(),
                    retry_attempts,
                    timeout_ms,
                ) {
                    Ok((status, headers, body)) => Resp::Ok {
                        status,
                        headers,
                        body,
                    },
                    Err(e) => Resp::Err { err: e },
                },
            )
        });
        self.returned_from_host = true;
        match r {
            Ok(Resp::Ok {
                status,
                headers,
                body,
            }) => Ok(Ok(keel::provider::host_http::HttpResponse {
                status,
                headers,
                body,
            })),
            Ok(Resp::Err { err }) => Ok(Err(err)),
            Err(e) => {
                let msg = format!(
                    "journal failure inside provider '{}': {e:#}",
                    self.provider_name
                );
                self.fatal =
                    Some(e.context(format!("inside provider '{}'", self.provider_name)));
                Err(wasmtime::Error::msg(msg))
            }
        }
    }
}

fn magic_and_compile(engine: &wasmtime::Engine, wasm: &[u8]) -> Result<Component> {
    anyhow::ensure!(
        wasm.starts_with(b"\0asm"),
        "not a WebAssembly binary (missing \\0asm magic)"
    );
    Component::new(engine, wasm)
        .map_err(anyhow::Error::from)
        .context("compiling provider component")
}

/// Compile + type-check PURE-tier provider bytes WITHOUT running them — the
/// same fail-fast the upgrade pre-flight gives modules: the import surface
/// must be EMPTY (instantiate_pre on an import-free linker) and `handle` must
/// exist with the right type. EngineShared::new runs this for every provider.
pub fn preflight(engine: &wasmtime::Engine, wasm: &[u8]) -> Result<Component> {
    let component = magic_and_compile(engine, wasm)?;
    let linker: Linker<ProviderCtx> = Linker::new(engine);
    let pre = linker
        .instantiate_pre(&component)
        .map_err(anyhow::Error::from)
        .context("provider imports something — pure (--provider) providers must be import-free; effectful components need --provider-effectful")?;
    ProviderPre::<ProviderCtx>::new(pre)
        .map_err(anyhow::Error::from)
        .context("provider does not export handle(kind, request) -> result<string, string>")?;
    Ok(component)
}

/// EFFECTFUL-tier pre-flight: imports must be a SUBSET of keel:provider/host-http
/// (an import-free pure component passes too — the operator merely granted
/// more than it uses), and `handle` must exist with the right type.
pub fn preflight_effectful(engine: &wasmtime::Engine, wasm: &[u8]) -> Result<Component> {
    let component = magic_and_compile(engine, wasm)?;
    let mut linker: Linker<EffCtx> = Linker::new(engine);
    ProviderEffectful::add_to_linker::<_, HasSelf<EffCtx>>(&mut linker, |c| c)
        .map_err(anyhow::Error::from)?;
    let pre = linker
        .instantiate_pre(&component)
        .map_err(anyhow::Error::from)
        .context("provider imports something beyond keel:provider/host-http")?;
    ProviderEffectfulPre::<EffCtx>::new(pre)
        .map_err(anyhow::Error::from)
        .context("provider does not export handle(kind, request) -> result<string, string>")?;
    Ok(component)
}

/// One PURE provider call — LIVE path only (replay returns the journal row
/// without coming here). Every failure is a guest-visible Err(String): unknown
/// kind (the provider's own err), a trap, a blown memory cap, or a blown epoch
/// budget — data, like an http transport error, journaled and replayed as-is.
pub fn call(
    engine: &wasmtime::Engine,
    component: &Component,
    max_memory: usize,
    kind: &str,
    request: &str,
) -> Result<String, String> {
    let ctx = ProviderCtx {
        limits: wasmtime::StoreLimitsBuilder::new()
            .memory_size(max_memory)
            .build(),
    };
    let mut store = Store::new(engine, ctx);
    store.limiter(|c| &mut c.limits);
    // Epoch budget: ~10 ticks of the engine's 1s ticker. A provider that
    // spins traps into Err instead of pinning a worker thread forever —
    // providers never get the guests' escape hatch (there is no park loop to
    // cancel them at), so the budget is the whole containment story.
    store.set_epoch_deadline(10);
    let linker: Linker<ProviderCtx> = Linker::new(engine);
    let p = match Provider::instantiate(&mut store, component, &linker) {
        Ok(p) => p,
        Err(e) => return Err(format!("provider instantiation failed: {e}")),
    };
    match p.call_handle(&mut store, kind, request) {
        Ok(inner) => inner,
        Err(trap) => Err(format!("provider trapped: {trap}")),
    }
}

/// One EFFECTFUL provider call, live or mid-replay (host.rs only comes here
/// when the terminal row is absent; already-recorded wire calls replay inside
/// journaled() without re-firing). Takes the nested JournalCtx by VALUE and
/// returns the advanced cursor even on failure — internal rows may have
/// committed and the caller's cursor must move past them.
///
/// Outcome layers: outer Err = workflow-fatal (journal integrity — the
/// nondeterminism bail or a db failure inside the provider); Ok(Err) =
/// provider-level failure, DATA (trap, budget, instantiation, its own err),
/// journaled in the terminal row exactly like the pure tier.
#[allow(clippy::too_many_arguments)] // 1:1 with what the nested scope needs, nothing more
pub fn call_effectful(
    engine: &wasmtime::Engine,
    component: &Component,
    max_memory: usize,
    provider_name: &str,
    kind: &str,
    request: &str,
    j: JournalCtx,
    secrets: Vec<(String, String)>,
    agent: ureq::Agent,
) -> (i64, Result<Result<String, String>>) {
    let ctx = EffCtx {
        limits: wasmtime::StoreLimitsBuilder::new()
            .memory_size(max_memory)
            .build(),
        j,
        provider_name: provider_name.to_string(),
        secrets,
        agent,
        fatal: None,
        returned_from_host: false,
        ticks_used: 0,
    };
    let mut store = Store::new(engine, ctx);
    store.limiter(|c| &mut c.limits);
    // Budget: ~10 ticks of PURE-WASM time, like the pure tier — but an http
    // wait must not count against it (the epoch keeps advancing during a slow
    // wire call, and the deadline check fires on the first wasm instruction
    // AFTER the host call returns). The callback excuses exactly one firing
    // per completed host call; consecutive firings mean the provider is
    // spinning in wasm and get counted.
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|mut cx| {
        let d = cx.data_mut();
        if d.returned_from_host {
            d.returned_from_host = false;
            Ok(wasmtime::UpdateDeadline::Continue(1))
        } else if d.ticks_used < 10 {
            d.ticks_used += 1;
            Ok(wasmtime::UpdateDeadline::Continue(1))
        } else {
            Err(wasmtime::Error::msg(
                "provider exceeded its compute budget (~10s of wasm time)",
            ))
        }
    });
    let mut linker: Linker<EffCtx> = Linker::new(engine);
    if let Err(e) = ProviderEffectful::add_to_linker::<_, HasSelf<EffCtx>>(&mut linker, |c| c) {
        let seq = store.data().j.next_seq;
        return (seq, Ok(Err(format!("provider instantiation failed: {e}"))));
    }
    let p = match ProviderEffectful::instantiate(&mut store, component, &linker) {
        Ok(p) => p,
        Err(e) => {
            let seq = store.data().j.next_seq;
            return (seq, Ok(Err(format!("provider instantiation failed: {e}"))));
        }
    };
    let out = p.call_handle(&mut store, kind, request);
    let fatal = store.data_mut().fatal.take();
    let seq = store.data().j.next_seq;
    match (out, fatal) {
        (_, Some(e)) => (seq, Err(e)),
        (Ok(inner), None) => (seq, Ok(inner)),
        (Err(trap), None) => (seq, Ok(Err(format!("provider trapped: {trap}")))),
    }
}
