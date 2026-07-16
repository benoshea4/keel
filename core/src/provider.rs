// provider.rs — v2.2 capability providers (PROVIDERS.md is the design doc).
//
// A provider is a wasm component implementing the `keel:provider` world
// (../provider-wit): one export, handle(kind, request-json) → result<json>.
// Providers have NO imports — no clocks, no sockets, no host-api: whatever a
// provider returns is journaled by the calling workflow's provider-call row,
// so replay never re-invokes it. Registered at startup (--provider
// name=path.wasm → EngineOptions.providers), compiled once (fail fast),
// instantiated per call (stateless by construction).

use anyhow::{Context as _, Result};
use wasmtime::component::{bindgen, Component, Linker};
use wasmtime::Store;

bindgen!({
    path: "../provider-wit", // keel/provider-wit/provider.wit, relative to core/
    world: "provider",
});

/// Per-call store data: just the memory cap (same figure as guests).
struct ProviderCtx {
    limits: wasmtime::StoreLimits,
}

/// Compile + type-check provider bytes WITHOUT running them — the same
/// fail-fast the upgrade pre-flight gives modules: the import surface must be
/// EMPTY (instantiate_pre on an import-free linker) and `handle` must exist
/// with the right type. EngineShared::new runs this for every provider.
pub fn preflight(engine: &wasmtime::Engine, wasm: &[u8]) -> Result<Component> {
    anyhow::ensure!(
        wasm.starts_with(b"\0asm"),
        "not a WebAssembly binary (missing \\0asm magic)"
    );
    let component = Component::new(engine, wasm)
        .map_err(anyhow::Error::from)
        .context("compiling provider component")?;
    let linker: Linker<ProviderCtx> = Linker::new(engine);
    let pre = linker
        .instantiate_pre(&component)
        .map_err(anyhow::Error::from)
        .context("provider imports something — providers must be import-free")?;
    ProviderPre::<ProviderCtx>::new(pre)
        .map_err(anyhow::Error::from)
        .context("provider does not export handle(kind, request) -> result<string, string>")?;
    Ok(component)
}

/// One provider call — LIVE path only (replay returns the journal row without
/// coming here). Every failure is a guest-visible Err(String): unknown kind
/// (the provider's own err), a trap, a blown memory cap, or a blown epoch
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
