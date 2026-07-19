// proxy-echo — v4.0 acceptance fixture (see Cargo.toml header): a pure
// wasi:http/proxy component, no keel WIT anywhere. Routes on the request
// path:
//   /count  -> wasi:keyvalue counter (get -> +1 -> set), body = the number
//   *       -> echo "method path" back
// Every request writes one line to wasi:cli stdout (the stdio→fn_logs
// assertion). NOTE: println! is a no-op on wasm32-unknown-unknown — stdout
// must be reached through the wasi:cli stream API explicitly.

#[allow(warnings)]
mod bindings;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::keyvalue::store;

struct Component;

fn stdout_line(msg: &str) {
    let out = bindings::wasi::cli::stdout::get_stdout();
    let _ = out.blocking_write_and_flush(format!("{msg}\n").as_bytes());
}

fn respond(response_out: ResponseOutparam, status: u16, payload: &[u8]) {
    let resp = OutgoingResponse::new(Fields::new());
    let _ = resp.set_status_code(status);
    let body = resp.body().expect("outgoing body");
    ResponseOutparam::set(response_out, Ok(resp));
    {
        let stream = body.write().expect("body stream");
        let _ = stream.blocking_write_and_flush(payload);
    }
    let _ = OutgoingBody::finish(body, None);
}

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        stdout_line(&format!("proxy-echo: {path}"));
        if path.starts_with("/count") {
            let payload = match store::open("") {
                Ok(bucket) => {
                    let n = bucket
                        .get("count")
                        .ok()
                        .flatten()
                        .and_then(|b| String::from_utf8(b).ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0)
                        + 1;
                    match bucket.set("count", n.to_string().as_bytes()) {
                        Ok(()) => n.to_string(),
                        Err(e) => format!("kv error: {e:?}"),
                    }
                }
                Err(e) => format!("open error: {e:?}"),
            };
            respond(response_out, 200, payload.as_bytes());
            return;
        }
        if path.starts_with("/big") {
            // E3's response cap: write ~11 MiB so the host's 10 MiB collector
            // gives up — the write errors once the channel closes, and this
            // guest shrugging that off is exactly the guest_error the gate
            // asserts.
            let resp = OutgoingResponse::new(Fields::new());
            let _ = resp.set_status_code(200);
            let body = resp.body().expect("outgoing body");
            ResponseOutparam::set(response_out, Ok(resp));
            {
                // wasi:io contract: blocking-write-and-flush takes AT MOST
                // 4096 bytes per call — bigger buffers trap by design.
                let stream = body.write().expect("body stream");
                let chunk = vec![b'x'; 4096];
                for _ in 0..2816 {
                    if stream.blocking_write_and_flush(&chunk).is_err() {
                        break;
                    }
                }
            }
            let _ = OutgoingBody::finish(body, None);
            return;
        }
        let method = match request.method() {
            bindings::wasi::http::types::Method::Get => "GET".to_string(),
            bindings::wasi::http::types::Method::Post => "POST".to_string(),
            m => format!("{m:?}").to_uppercase(),
        };
        respond(response_out, 200, format!("{method} {path}").as_bytes());
    }
}

bindings::export!(Component with_types_in bindings);
