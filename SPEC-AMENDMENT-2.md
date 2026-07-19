# Amendment 2 to the micro-cloud extension — functions grow up: config + state

Status: AUTHORED IN-REPO (2026-07-19), per the same stretch-direction rule as
[Amendment 1](SPEC-AMENDMENT-1.md): *spec amendment first, code second.*
SPEC-MICROCLOUD.md stays a pristine copy; everything here is additive to it.
Approved sequence: status.md §R ("do 3.4 and 3.5 and beyond").

Ships as **v3.5**. Acceptance script `scripts/accept_functions2.sh` is the
definition of done — same immutability rules as every gate (SPEC.md §0).

**ONE WIT bump: `keel:workflow` 0.7.0 → 0.8.0** — the one-bump-per-stage rule
is why config and kv ship *together*, and why they ship *before* the wasi
compatibility work (Amendment 3): they are pure keel, zero new dependencies,
and Amendment 3's `wasi:keyvalue` will back onto the store built here.

Motivation: a function that cannot hold an API key cannot call anything real,
and a function that cannot remember anything across requests cannot *be*
anything real without reaching for a full workflow. `platform-api` gains
exactly four calls — `config-get`, `kv-get`, `kv-set`, `kv-delete` — and the
control plane gains the endpoints to manage what those calls see. Nothing
else.

---

## A6. Per-ref config — `config-get`

Operator-set string config attached to a **ref** — a route prefix
(kind `function`) or an app name (kind `app`) — exactly the identity the
ledger, logs, and rate limits already use. The app's backend reads the app's
config; two routes never share by accident.

Control plane (token-gated, same param style as `/api/logs`):

- `POST /api/config` `{"kind","ref","name","value"}` → 201 (upsert).
- `GET /api/config?kind=&ref=` → `{"names":[...]}` — **names only. Values are
  never echoed by any endpoint or page.** The engine-side guarantee is
  names-out-only; a guest that prints its own key into its own logs has made
  its own choice (same posture as a workflow logging a secret it read).
- `DELETE /api/config?kind=&ref=&name=` → 204 / 404.

Guest call (world `handler`, DIRECT like all of platform-api — functions
don't journal):

```wit
/// none = not set for this ref. Values are operator-owned strings.
config-get: func(name: string) -> option<string>;
```

Door checks: `name` is `[A-Za-z0-9_-]{1,64}`; `value` ≤ 4 KiB; ≤ 64 entries
per ref. Violations 400 at the API — the guest-visible surface has no error
case beyond `none`.

Storage: `fn_config (kind, ref, name, value, created_at, PK(kind,ref,name))`.
Deleting a route or app does NOT cascade config (re-binding sees it again —
config is operator intent, not serving state); `DELETE /api/config` removes
entries explicitly.

## A7. Per-ref durable KV — `kv-get` / `kv-set` / `kv-delete`

Durable state for functions **without** a workflow: sessions, counters,
caches. Scoped per ref like config — bind the same module at two prefixes and
they have two stores (the module is code; the ref is the tenant of record).

```wit
/// none = key absent. Bytes, not strings — sessions store binary.
kv-get: func(key: string) -> option<list<u8>>;
/// The call returns ONLY after the row is committed — durability is the
/// return, the same discipline as the workflow journal. err = a cap.
kv-set: func(key: string, value: list<u8>) -> result<_, string>;
/// Absent key is fine (idempotent).
kv-delete: func(key: string);
```

- **Durability contract:** `kv-set` commits before returning; kill -9 after a
  returned set never loses it. The acceptance gate proves this by restarting
  the engine mid-count.
- **Caps (public-plane discipline — a tokenless caller must not grow the
  db):** key ≤ 256 bytes, value ≤ 64 KiB, ≤ 1024 keys per ref, ≤ 8 MiB total
  value bytes per ref. Over-cap `kv-set` errs with the cap named; the
  request is otherwise unharmed (the guest decides what its error looks
  like).
- **Atomicity is per call, and that is ALL.** No transactions, no
  compare-and-swap in this amendment: two concurrent read-modify-writes to
  one key race, last write wins. A function needing atomic sequences starts
  a workflow — that is the platform's whole shape. (`kv-cas` is named as the
  future extension if demand shows up; not built now.)
- Lifecycle mirrors config: unbinding does not wipe; explicit wipe via the
  control plane. Retention flags do NOT touch kv (state, not history).

Control plane: `GET /api/kv?kind=&ref=` → `{"keys":[...]}` (keys only —
values are guest state, not operator browsing material);
`DELETE /api/kv?kind=&ref=` → 204, the whole ref's store (the "reset my
function" button).

Storage: `fn_kv (kind, ref, key, value BLOB, updated_at, PK(kind,ref,key))`.

## A8. The WIT bump — 0.8.0 is a rebuild event

`interface platform-api` gains the four calls above; nothing else in the
package changes textually. Per the package's own history (0.x minors are
blob-incompatible — the 0.4.0 comment in workflow.wit), **components compiled
against ≤ 0.7.0 will not instantiate on a 0.8.0 engine**: the import name
carries the version. Consequences, stated loudly:

- Every guest in this repo rebuilds in its gate — the suite passing IS the
  migration proof.
- An operator's previously-uploaded modules fail at instantiate after the
  engine upgrade: publicly a generic 500 (v3.3 posture), the real reason in
  the engine log. Fix = rebuild against 0.8.0, re-upload (new content hash),
  re-bind. docs/operations.md gets this as an upgrade note.
- `world workflow` and `world solver` are textually untouched; workflow
  guests rebuild without source changes, solvers are import-free and
  UNAFFECTED even as binaries (empty linker — old solver blobs still judge).

## A9. Acceptance — `scripts/accept_functions2.sh`

A new fixture guest `guests/kvcfg-fn` (world `handler`) routes on its path:
`/cfg` returns `config-get("API_KEY")` or `"none"`; `/count` increments key
`count` via get→set and returns the number; `/reset` kv-deletes it; `/big`
attempts an over-cap `kv-set` and returns the error string. The gate asserts,
on a TOKENED engine:

1. config set via API → `/cfg` returns the value; unset name → `"none"`;
   `GET /api/config` lists the NAME and the response body does NOT contain
   the value; door checks 400 (bad name, oversize value).
2. `/count` → 1, 2, 3 sequentially; **engine kill -9 + restart**; `/count` →
   4 (the durability contract). `/reset` → subsequent `/count` → 1.
3. The same module bound at a second prefix counts independently (ref-scoped,
   not module-scoped) — and an app with this backend counts under kind
   `app`, independent again.
4. `/big` response contains the cap error (value cap enforced end to end).
5. `GET /api/kv` lists `count` (keys only); `DELETE /api/kv` resets the
   counter to 1 on next call.
6. Existing-suite regression: every prior gate rebuilds its guests against
   0.8.0 and passes — the A8 migration claim, proven by the suite itself.
