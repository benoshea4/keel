// loop-solver — phase 5 TLE fodder (see Cargo.toml header).
//
// A bare `loop {}` exhausts the 10^9-instruction FUEL budget in under a
// second — that's OOF fodder, not TLE. To time out instead, burn WALL TIME
// that is cheap in fuel: bulk `memory.copy` moves 8 MiB per single wasm
// instruction (~1 fuel), so the 2s epoch deadline fires while the fuel meter
// has barely moved. black_box on both sides keeps LLVM from proving the copy
// unobservable and deleting the whole loop (it did, the first time — the
// buffer never even allocated).

#[allow(warnings)]
mod bindings;

struct Component;

impl bindings::Guest for Component {
    fn solve(_input: String) -> Result<String, String> {
        let mut buf = vec![0u8; 16 * 1024 * 1024]; // well under the 256 MiB cap
        let half = buf.len() / 2;
        loop {
            core::hint::black_box(&mut buf); // contents "unknown": copy must run
            buf.copy_within(0..half, half); // memory.copy: 8 MiB, ~1 fuel
            if core::hint::black_box(&buf)[half] == 255 {
                // Never true (the buffer is all zeros) — but black_box hides
                // that, so the loop can't be reduced to `loop {}`.
                return Ok(buf.len().to_string());
            }
        }
    }
}

bindings::export!(Component with_types_in bindings);
