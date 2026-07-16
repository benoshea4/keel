//! Keel as a library — run a durable workflow in-process, no server.
//!
//! ```bash
//! (cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)
//! cargo run -p keel-core --example embedded -- \
//!     guests/counter/target/wasm32-unknown-unknown/release/counter.wasm '{"target":2}'
//! ```
//!
//! Kill it mid-run and run it again with the same db: the workflow recovers
//! and completes — Engine::open replays every non-terminal workflow. That is
//! the entire durability story, embedded.

use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let wasm_path = args.next().expect("usage: embedded <component.wasm> [input-json]");
    let input = args.next().unwrap_or_else(|| "{}".to_string());
    let wasm = std::fs::read(&wasm_path)?;

    let engine = keel_core::Engine::open(keel_core::EngineOptions::new("embedded.db"))?;
    let hash = engine.upload_module("embedded-example", &wasm)?;
    let id = engine.start_workflow(&hash, &input)?;
    println!("started workflow {id} (db: embedded.db)");

    loop {
        let wf = engine.workflow(&id)?.expect("workflow row exists");
        println!("  status: {}", wf.status);
        if wf.status == "completed" || wf.status == "failed" {
            println!("output: {}", wf.output.as_deref().unwrap_or("<none>"));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
