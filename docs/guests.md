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
| `http-request(method, url, headers, body, retry-attempts, timeout-ms)` | yes | The general HTTP call. Non-2xx is **data** (`http-response {status, headers, body}`); only transport failures are `err`. Retries are opt-in (transport + 5xx only, capped at 8 — loop with `sleep-ms` for more) — the engine won't re-POST for you. `timeout-ms` caps **each attempt** (0 = 30s default); a timeout is a transport failure. Body capped at 1 MiB, utf-8-lossy. |
| `secret(name)` | name + salted hash only | Reads the engine's `--secrets-file` live. The value NEVER touches the database: the journal gets `{name}` → `{salt, sha256}`, and replay re-verifies against the live file — a secret rotated under an in-flight replay **fails the workflow loudly** (restore the old value or cancel). Values you read are redacted (`{{secret:name}}`) from journaled `http-request`/`http-get` requests while the real bytes go on the wire. `err` if unconfigured/missing. |
| `http-get(url)` | yes | Legacy simple GET: non-2xx is `err`, 3 automatic attempts. Prefer `http-request`. |
| `sleep-ms(ms)` | yes | Durable: survives crashes with the original absolute deadline (remainder-sleep). |
| `now-ms()` / `random-u64()` | yes | Wall clock / randomness, recorded so replay sees the same values. |
| `await-event(name)` | yes | Parks until `POST /api/workflows/{id}/events` delivers a matching event; exactly-once. |
| `checkpoint(state)` | yes | Snapshots state, prunes the journal below it, enables upgrade + fast recovery. |
| `kv-set(key, value)` / `kv-get(key)` | yes | Durable per-workflow KV. Reads record what was seen; both are crash-atomic with their journal rows. Since v2.3 writes append *versions* tied to their journal seq, so an upgrade's tail-discard rolls values back with the tail; superseded versions are compacted at each checkpoint. |
| `provider-call(name, kind, request)` | yes (`custom:<name>:<kind>`) | Call a capability provider registered on the engine (see [PROVIDERS.md](../PROVIDERS.md)). Replay returns the recorded response without re-invoking. Unknown name/kind, traps and blown budgets are `err` (data). v2.5: EFFECTFUL providers (`--provider-effectful`) additionally journal each of their wire calls at its own seq (`provider-http:<name>`), so a crash mid-provider re-fires only the truly in-flight call — with the same `keel-idempotency-key`. No guest-side change. |
| `log(msg)` | **no** | Engine log line. Replays re-log — duplicates after recovery are normal. |

## Sharp edges

- **At-least-once effects:** a crash *between* an effect executing and its
  journal row committing re-runs the effect on recovery. Since v2.3 the
  engine sends `keel-idempotency-key: <workflow_id>:<seq>` on every
  `http-request` — stable across replay and across the crash-and-resend
  window, so a remote that stores processed keys (unique-index the column;
  return the stored response on conflict) collapses the window to
  exactly-once. Wire-only: it never appears in the journaled request. Send
  the header yourself to override the key, or with an empty value to send
  none. `http-get` does not carry one — prefer `http-request`.
- **KV vs. upgrade tail-discard — CLOSED in v2.3:** kv writes are versioned
  by journal seq, and an upgrade discards versions written by the discarded
  tail together with it, so the upgraded module reads the pre-tail values
  (`smoke_kv_upgrade.sh` is the proof). One migration note: kv rows written
  before v2.3 carry version 0, i.e. they are treated as pre-checkpoint state.
- **Secrets stay out of the journal only on the paths the engine controls.**
  Redaction covers journaled `http-request`/`http-get` *requests*. It cannot
  cover: `checkpoint` state (don't serialize secret values — re-read via
  `secret()` after resume), `kv-set` values, event payloads you're sent, or a
  *response body* in which the remote echoes your secret back. All of those
  are stored verbatim by design. Use high-entropy secrets.
- **1 MiB body cap is silent:** oversized HTTP responses arrive truncated with
  no marker. If you expect big payloads, check for your own terminator.
- **Guests that spin forever** (`loop {}`) burn a core until someone calls
  `POST .../cancel` — the epoch tick makes cancel work, not spinning cheap.
- **WIT versions:** rebuilding against a newer `workflow.wit` is usually just a
  rebuild (`bindings.rs` regenerates), but *already-uploaded blobs* keyed to an
  old 0.x interface won't instantiate on a newer engine — re-upload after
  engine upgrades that bump the WIT.
