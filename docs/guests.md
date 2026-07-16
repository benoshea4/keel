# Writing Keel guests

A guest is a WASM **component** (component model, `wasm32-unknown-unknown`)
implementing the `workflow` world in [`wit/workflow.wit`](../wit/workflow.wit).
Start by copying `guests/demo/` — its `Cargo.toml` carries the required
`[package.metadata.component]` block. Build with:

```bash
cargo component build --release --target wasm32-unknown-unknown
```

(`wasm32-unknown-unknown`, not wasip1 — zero ambient capabilities is the point:
a guest that can't reach the world except through the journaled host API is a
guest whose every effect replays.)

## The rules (why your workflow survives kill -9)

1. **No ambient anything.** No `std::time`, no `std::thread::sleep`, no
   `println!`, no `rand` — they panic or lie on this target. Use `host::*`.
2. **Determinism between host calls.** Given the same input and the same
   journaled results, your code must make the same next host call. Branch on
   host-call results all you like — they replay identically.
3. **Own your state for upgrades.** `checkpoint(state)` hands the engine a blob
   that must determine all *future* host calls; after a live upgrade the new
   module's `resume(state)` gets it back (migrate old shapes there — see
   `guests/counter` v1→v2).
4. **Export `resume`.** Guests that never checkpoint stub it:
   `Err("no checkpoints".to_string())`.

## Host API

| Call | Journaled | Semantics |
|---|---|---|
| `http-request(method, url, headers, body, retry-attempts)` | yes | The general HTTP call. Non-2xx is **data** (`http-response {status, headers, body}`); only transport failures are `err`. Retries are opt-in (transport + 5xx only, capped at 8 — loop with `sleep-ms` for more) — the engine won't re-POST for you. Body capped at 1 MiB, utf-8-lossy. |
| `http-get(url)` | yes | Legacy simple GET: non-2xx is `err`, 3 automatic attempts. Prefer `http-request`. |
| `sleep-ms(ms)` | yes | Durable: survives crashes with the original absolute deadline (remainder-sleep). |
| `now-ms()` / `random-u64()` | yes | Wall clock / randomness, recorded so replay sees the same values. |
| `await-event(name)` | yes | Parks until `POST /api/workflows/{id}/events` delivers a matching event; exactly-once. |
| `checkpoint(state)` | yes | Snapshots state, prunes the journal below it, enables upgrade + fast recovery. |
| `kv-set(key, value)` / `kv-get(key)` | yes | Durable per-workflow KV. Reads record what was seen; both are crash-atomic with their journal rows. |
| `log(msg)` | **no** | Engine log line. Replays re-log — duplicates after recovery are normal. |

## Sharp edges

- **At-least-once effects:** a crash *between* an effect executing and its
  journal row committing re-runs the effect on recovery. Make external calls
  idempotent where it matters (send an idempotency key header).
- **KV vs. upgrade tail-discard:** an upgrade discards journal rows past the
  checkpoint, but kv values written by that discarded tail *survive* (kv is
  state, not journal). The re-executed tail overwrites them deterministically
  in the common case; if you interleave kv writes and reads across a
  checkpoint boundary, checkpoint *after* the writes. (KV versioning is
  roadmapped — ROADMAP.md v2.3.)
- **1 MiB body cap is silent:** oversized HTTP responses arrive truncated with
  no marker. If you expect big payloads, check for your own terminator.
- **Guests that spin forever** (`loop {}`) burn a core until someone calls
  `POST .../cancel` — the epoch tick makes cancel work, not spinning cheap.
- **WIT versions:** rebuilding against a newer `workflow.wit` is usually just a
  rebuild (`bindings.rs` regenerates), but *already-uploaded blobs* keyed to an
  old 0.x interface won't instantiate on a newer engine — re-upload after
  engine upgrades that bump the WIT.
