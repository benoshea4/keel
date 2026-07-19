// proxy-out — v4.0 acceptance fixture (see Cargo.toml header): one outbound
// GET to http://127.0.0.1:18080/ping, answer relayed. Denied outbound is
// DATA in the response body, which is exactly what the gate asserts.

#[allow(warnings)]
mod bindings;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::outgoing_handler;
use bindings::wasi::io::streams::StreamError;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingRequest, OutgoingResponse,
    ResponseOutparam, Scheme,
};

struct Component;

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
    fn handle(_request: IncomingRequest, response_out: ResponseOutparam) {
        let req = OutgoingRequest::new(Fields::new());
        let _ = req.set_method(&Method::Get);
        let _ = req.set_scheme(Some(&Scheme::Http));
        let _ = req.set_authority(Some("127.0.0.1:18080"));
        let _ = req.set_path_with_query(Some("/ping"));
        let payload = match outgoing_handler::handle(req, None) {
            Err(code) => format!("outbound refused: {code:?}"),
            Ok(fut) => {
                fut.subscribe().block();
                match fut.get() {
                    Some(Ok(Ok(resp))) => {
                        let status = resp.status();
                        let mut buf = Vec::new();
                        if let Ok(body) = resp.consume() {
                            if let Ok(stream) = body.stream() {
                                loop {
                                    match stream.blocking_read(64 * 1024) {
                                        Ok(chunk) => {
                                            if chunk.is_empty() {
                                                continue;
                                            }
                                            buf.extend_from_slice(&chunk);
                                        }
                                        Err(StreamError::Closed) => break,
                                        Err(_) => break,
                                    }
                                }
                            }
                        }
                        format!("upstream {status}: {}", String::from_utf8_lossy(&buf))
                    }
                    Some(Ok(Err(code))) => format!("outbound error: {code:?}"),
                    _ => "outbound future never resolved".to_string(),
                }
            }
        };
        respond(response_out, 200, payload.as_bytes());
    }
}

bindings::export!(Component with_types_in bindings);
