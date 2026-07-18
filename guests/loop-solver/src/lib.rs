// loop-solver — phase 5 TLE fodder (see Cargo.toml header).
//
// A bare `loop {}` would exhaust the 10^9-instruction FUEL budget in well
// under the 2s deadline — that's OOF fodder, not TLE. To time out, spin on
// bulk `memory.copy`: one wasm instruction (≈1 fuel) that moves 32 MiB of
// real wall-clock time per call. Milliseconds per fuel unit — the epoch
// deadline fires ages before the fuel meter notices.

#[allow(warnings)]
mod bindings;

struct Component;

impl bindings::Guest for Component {
    fn solve(_input: String) -> Result<String, String> {
        let mut buf = vec![1u8; 64 * 1024 * 1024]; // well under the 256 MiB cap
        let half = buf.len() / 2;
        loop {
            buf.copy_within(0..half, half); // memory.copy: 32 MiB, ~1 fuel
            if buf[half] == 0 {
                // Unreachable (the buffer is all 1s) — but the compiler can't
                // prove it, so the copy above can't be optimized away.
                return Ok(buf.len().to_string());
            }
        }
    }
}

bindings::export!(Component with_types_in bindings);
