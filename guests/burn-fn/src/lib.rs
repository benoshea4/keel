// burn-fn — v3.3 acceptance function (see Cargo.toml header): spin until a
// quota ends the run. The phase-5 lesson applies — black_box BOTH sides of
// the work, or LLVM deletes the "unoptimizable" loop and the fixture returns
// instantly instead of burning.

#[allow(warnings)]
mod bindings;

use bindings::{HttpRequest, HttpResponse};

struct Component;

impl bindings::Guest for Component {
    fn handle(_req: HttpRequest) -> HttpResponse {
        let mut i: u64 = 0;
        while std::hint::black_box(i) != u64::MAX {
            i = std::hint::black_box(i.wrapping_add(1));
        }
        // Reachable in principle (so the compiler keeps the loop's result
        // live), never reached under any real quota.
        HttpResponse {
            status: 200,
            headers: vec![],
            body: i.to_le_bytes().to_vec(),
        }
    }
}

bindings::export!(Component with_types_in bindings);
