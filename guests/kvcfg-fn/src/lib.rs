// kvcfg-fn — v3.5 acceptance function (see Cargo.toml header). One handler,
// four behaviors keyed on the path — each new platform-api call gets a probe
// the gate can assert through plain HTTP.

#[allow(warnings)]
mod bindings;

use bindings::keel::workflow::platform_api;
use bindings::{HttpRequest, HttpResponse};

struct Component;

fn text(status: u16, body: String) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: body.into_bytes(),
    }
}

impl bindings::Guest for Component {
    fn handle(req: HttpRequest) -> HttpResponse {
        match req.path.as_str() {
            "/cfg" => text(
                200,
                platform_api::config_get("API_KEY").unwrap_or_else(|| "none".to_string()),
            ),
            "/count" => {
                let n = platform_api::kv_get("count")
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0)
                    + 1;
                match platform_api::kv_set("count", n.to_string().as_bytes()) {
                    Ok(()) => text(200, n.to_string()),
                    Err(e) => text(500, e),
                }
            }
            "/reset" => {
                platform_api::kv_delete("count");
                text(200, "0".to_string())
            }
            "/big" => {
                // 100 000 B > the 64 KiB value cap: the err string IS the body,
                // so the gate can assert the cap end to end.
                let big = vec![b'x'; 100_000];
                match platform_api::kv_set("big", &big) {
                    Ok(()) => text(200, "stored?!".to_string()),
                    Err(e) => text(200, e),
                }
            }
            other => text(404, format!("no probe at {other}")),
        }
    }
}

bindings::export!(Component with_types_in bindings);
