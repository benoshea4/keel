// The v2.2 crate-split gate: a workflow runs IN-PROCESS through the Engine
// façade — no HTTP, no `keel` binary. Ignored by default because it needs a
// built guest component; scripts/smoke_embedded.sh builds one and runs:
//
//   KEEL_EMBED_WASM=.../counter.wasm cargo test -p keel-core --test embedded -- --ignored

use std::time::{Duration, Instant};

#[test]
#[ignore = "needs KEEL_EMBED_WASM=<built workflow component> — run scripts/smoke_embedded.sh"]
fn embedded_workflow_completes() {
    let wasm_path = std::env::var("KEEL_EMBED_WASM").expect("KEEL_EMBED_WASM not set");
    let wasm = std::fs::read(&wasm_path).expect("reading guest component");

    let dir = std::env::temp_dir().join(format!("keel-embed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("embed.db");
    let _ = std::fs::remove_file(&db);
    let db = db.to_string_lossy().into_owned();

    // Open (creates + migrates + recovers — nothing to recover here), store a
    // module, start a workflow, poll its row. That's the whole embedded API.
    let engine = keel_core::Engine::open(keel_core::EngineOptions::new(&db)).unwrap();
    let hash = engine.upload_module("embedded-counter", &wasm).unwrap();
    let id = engine.start_workflow(&hash, r#"{"target":0}"#).unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let wf = engine.workflow(&id).unwrap().expect("workflow row exists");
        match wf.status.as_str() {
            "completed" => break,
            "failed" => panic!("workflow failed: {:?}", wf.output),
            other if Instant::now() > deadline => panic!("timed out in status {other}"),
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    // Unknown module hash is an embedder error, not a panic.
    assert!(engine.start_workflow("no-such-hash", "{}").is_err());
    // Junk bytes never earn a content hash.
    assert!(engine.upload_module("junk", b"not wasm").is_err());
}
