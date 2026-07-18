// hello — phase 6 flagship demo (see Cargo.toml header). One button starts a
// durable workflow THROUGH the app's backend function; a 1s poll renders its
// status until completion. Everything relative (./api/...) so the app is
// mount-point agnostic (Task 6.2). MODULE_HASH is sed-injected by
// accept_phase6.sh before the build — crude, honest, effective.

use leptos::prelude::*;

const MODULE_HASH: &str = "MODULE_HASH_PLACEHOLDER";

fn main() {
    leptos::mount::mount_to_body(App);
}

async fn start_job(set_status: WriteSignal<String>, set_output: WriteSignal<String>) {
    set_output.set(String::new());
    let body = serde_json::json!({"module_hash": MODULE_HASH, "input": {"target": 3}});
    let resp = gloo_net::http::Request::post("./api/start")
        .header("content-type", "application/json")
        .body(body.to_string())
        .expect("building request")
        .send()
        .await;
    let v: serde_json::Value = match resp {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(e) => {
            set_status.set(format!("start failed: {e}"));
            return;
        }
    };
    let Some(id) = v["workflow_id"].as_str().map(String::from) else {
        set_status.set(format!("start failed: {v}"));
        return;
    };
    set_status.set(format!("workflow {id}: starting"));
    loop {
        gloo_timers::future::TimeoutFuture::new(1_000).await;
        let Ok(r) = gloo_net::http::Request::get(&format!("./api/status?id={id}"))
            .send()
            .await
        else {
            continue; // transient fetch failure — keep polling
        };
        let s: serde_json::Value = r.json().await.unwrap_or_default();
        let status = s["status"].as_str().unwrap_or("?").to_string();
        set_status.set(format!("workflow {id}: {status}"));
        if status == "completed" || status == "failed" {
            set_output.set(s["output"].as_str().unwrap_or("").to_string());
            break;
        }
    }
}

#[component]
fn App() -> impl IntoView {
    let (status, set_status) = signal("idle — press the button".to_string());
    let (output, set_output) = signal(String::new());
    view! {
        <h1>"hello — a keel app"</h1>
        <p>"The button starts a DURABLE workflow through this app's backend \
            function. Kill the engine mid-run and restart it: the workflow \
            finishes anyway. This page is Rust compiled to WASM, served by \
            the same binary."</p>
        <button on:click=move |_| {
            set_status.set("starting…".to_string());
            leptos::task::spawn_local(start_job(set_status, set_output));
        }>"Start job"</button>
        <p>{status}</p>
        <pre>{output}</pre>
    }
}
