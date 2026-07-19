// host.rs — Task 1.2: wasmtime component bindings + the host-api implementation.
//
// Every effectful host function below goes through JournalCtx::journaled — that
// wrapper is what makes calls durable and replayable. `log` is the deliberate
// exception: it claims NO seq and writes NO journal row (SPEC.md §4.1); replay
// re-runs it, so duplicate log lines after a restart are expected and harmless.
//
// Journal payload JSON is fixed by SPEC.md §4.2 — the field names ("ok"/"err"/"ms"/
// "v") are part of the on-disk format; renaming them breaks replay of existing DBs.
//
// PHASE 2: Task 2.5 fills in the await_event body (park loop + single-txn event
// delivery); Task 2.6 adds retries inside do_http_get (live path only).
// PHASE 3: Task 3.3 adds checkpoint.

use std::io::Read;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmtime::component::bindgen;

use crate::db;
use crate::journal::{now_ms, JournalCtx};
use crate::notifier::Notifier;

bindgen!({
    path: "../wit",           // keel/wit/workflow.wit, relative to engine/
    world: "workflow",
    // Host fns return wasmtime::Result<_> so a journal/db error traps the guest
    // cleanly (workflow -> failed). NOTE: the spec-era option `trappable_imports:
    // true` was renamed in wasmtime 43 to this syntax (see status.md deviation 2).
    imports: { default: trappable },
});
// FALLBACK (SPEC.md Task 1.2): if bindgen! can't find ../wit, copy wit/ into
// engine/wit/ and use path: "wit" — keep the two copies in sync.

/// Per-workflow store data — everything a host call can touch.
pub struct Ctx {
    pub j: JournalCtx,
    pub http: ureq::Agent,
    /// Park-loop wake-ups + the phase-3 abort flag (Task 2.3).
    pub notifier: Arc<Notifier>,
    /// For host calls whose journaled() exec closure needs its own scoped
    /// Connection (checkpoint, Task 3.3) — exec can't borrow j.db.
    pub db_path: String,
    /// v1.1 — per-store resource caps (linear memory). runner.rs builds it and
    /// registers it via store.limiter().
    pub limits: wasmtime::StoreLimits,
    /// v2.1 / Amendment 4 — the secret-store PORT. `get` is re-read on EVERY
    /// secret() call (rotation must be visible; secret reads are rare). The
    /// adapter (file / env / layered / none) is chosen at engine boot.
    pub secrets: std::sync::Arc<dyn crate::secrets::SecretStore>,
    /// v2.1 — (name, value) of every secret THIS execution has read, in read
    /// order. The redaction set for journaled http requests: deterministic
    /// across replay because secret() traps on any value change. Never
    /// persisted anywhere.
    pub read_secrets: Vec<(String, String)>,
    /// v2.2 — the process-wide wasmtime Engine (Arc-backed clone): provider
    /// calls build their own short-lived Store on it.
    pub engine: wasmtime::Engine,
    /// v2.2 — compiled capability providers by name (see provider.rs);
    /// v2.5 — each entry carries the tier the operator granted it;
    /// v2.6 — a live registry (RwLock: the upload/rebind/delete API mutates it
    /// without a restart). provider_call snapshots the entry per call.
    pub providers:
        Arc<std::sync::RwLock<std::collections::HashMap<String, crate::provider::ProviderEntry>>>,
    /// v2.2 — provider calls reuse the per-guest memory cap.
    pub max_guest_memory: usize,
}

impl Ctx {
    /// v2.1 — add a successfully-read secret to this execution's redaction
    /// set. Every DISTINCT (name, value) pair is kept: a secret rotated
    /// between two reads (legal while no replay crosses the rotation) means
    /// two live values in play, and a request built with EITHER must redact.
    fn remember_secret(&mut self, name: &str, value: &str) {
        if !self
            .read_secrets
            .iter()
            .any(|(n, v)| n == name && v == value)
        {
            self.read_secrets.push((name.to_string(), value.to_string()));
        }
    }
}

/// Sentinel error a worker thread bails with when the notifier's abort flag is
/// set. Raised from three places: the two park loops below (sleep_ms,
/// await_event) and the epoch-deadline callback in runner.rs (which catches
/// guests spinning in pure wasm that never reach a park loop). Two callers set
/// the flag: the upgrade endpoint (Task 3.6) and the cancel endpoint
/// (post-review hardening). The runner's result-match downcasts to this type
/// (walking the anyhow chain) to exit the thread WITHOUT marking the workflow
/// failed — the endpoint that raised the flag owns what happens next.
#[derive(Debug)]
pub struct AbortForUpgrade;

impl std::fmt::Display for AbortForUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbortForUpgrade")
    }
}

impl std::error::Error for AbortForUpgrade {}

/// Serializes to `{}` — the §4.2 request/response shape for parameterless calls.
#[derive(Serialize, Deserialize)]
struct Empty {}

/// wasmtime 43 split its Error type from anyhow (the spec predates this): the
/// engine stays on anyhow internally (journal.rs etc.), and this is the one
/// conversion point where a journal/db failure becomes a guest trap.
/// wasmtime::Error -> anyhow::Error is automatic (`?`); this direction is not.
fn trap(e: anyhow::Error) -> wasmtime::Error {
    wasmtime::Error::from_anyhow(e)
}

impl keel::workflow::host_api::Host for Ctx {
    fn http_get(&mut self, url: String) -> wasmtime::Result<Result<String, String>> {
        #[derive(Serialize)]
        struct Req {
            url: String,
        }
        // Guest-visible errors (bad status, transport failure) are DATA, not traps:
        // they journal as {"err": ...} and replay identically. Untagged enum gives
        // the exact §4.2 JSON: {"ok": body} | {"err": message}.
        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum Resp {
            Ok { ok: String },
            Err { err: String },
        }

        let agent = self.http.clone(); // Agent is Arc-backed: cheap clone keeps the
        let u = url.clone();           // closure from borrowing self while j is &mut
        // v2.1 — the journaled request is REDACTED (secret values → placeholder);
        // the closure uses the real url. See http_request for the full story.
        let url = redact(&url, &self.read_secrets);
        let r = self.j.journaled("http-get", &Req { url }, move || {
            Ok(match do_http_get(&agent, &u) {
                Ok(body) => Resp::Ok { ok: body },
                Err(e) => Resp::Err { err: e },
            })
        }).map_err(trap)?;
        Ok(match r {
            Resp::Ok { ok } => Ok(ok),
            Resp::Err { err } => Err(err),
        })
    }

    fn http_request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<String>,
        retry_attempts: u32,
        timeout_ms: u32,
    ) -> wasmtime::Result<Result<keel::workflow::host_api::HttpResponse, String>> {
        // v1.2 — the general HTTP call. Everything that could vary is part of
        // the journaled request, so replay verifies the guest still asks for
        // the same call. Non-2xx is DATA (the response comes back as-is);
        // only transport failures are Err. Retries are strictly opt-in
        // (retry_attempts extra tries on transport/5xx) because auto-retrying
        // a POST is a caller decision, not the engine's.
        //
        // v2.1 — the JOURNALED request is redacted: any value the guest
        // obtained via secret() is replaced by {{secret:name}} before the row
        // is written or compared, while the wire request uses the real bytes.
        // This is what makes secret() mean anything — the only use of a
        // secret is to put it in a request, and the request is journaled.
        // Replay-stable because secret() guarantees (by hash check) that the
        // redaction set carries identical values on replay, or traps first.
        #[derive(Serialize)]
        struct Req {
            method: String,
            url: String,
            headers: Vec<(String, String)>,
            body: Option<String>,
            retry_attempts: u32,
            timeout_ms: u32,
        }
        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum Resp {
            Ok {
                status: u16,
                headers: Vec<(String, String)>,
                body: String,
            },
            Err {
                err: String,
            },
        }

        let agent = self.http.clone();
        let rs = &self.read_secrets;
        let req = Req {
            method: method.clone(),
            url: redact(&url, rs),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), redact(v, rs)))
                .collect(),
            body: body.as_deref().map(|b| redact(b, rs)),
            retry_attempts,
            timeout_ms,
        };
        // v2.3 — idempotency key, injected ON THE WIRE ONLY (the journaled
        // request keeps exactly what the guest asked, so pre-v2.3 journals
        // replay unchanged; the key is derivable anyway: it IS this row's
        // workflow_id:seq). Deterministic across replay and REUSED by a
        // recovery re-send — which is the point: the remote can collapse the
        // at-least-once window by deduping on it. Guests opt out by passing
        // the header with an empty value, or override it with their own.
        let wire_headers = inject_idempotency_key(headers, &self.j.workflow_id, self.j.next_seq);
        let r = self
            .j
            .journaled("http-request", &req, move || {
                Ok(
                    match do_http_request(
                        &agent,
                        &method,
                        &url,
                        &wire_headers,
                        body.as_deref(),
                        retry_attempts,
                        timeout_ms,
                    ) {
                        Ok((status, headers, body)) => Resp::Ok {
                            status,
                            headers,
                            body,
                        },
                        Err(e) => Resp::Err { err: e },
                    },
                )
            })
            .map_err(trap)?;
        Ok(match r {
            Resp::Ok {
                status,
                headers,
                body,
            } => Ok(keel::workflow::host_api::HttpResponse {
                status,
                headers,
                body,
            }),
            Resp::Err { err } => Err(err),
        })
    }

    fn kv_set(&mut self, key: String, value: String) -> wasmtime::Result<()> {
        // v1.2 — durable per-workflow KV. Hand-rolled like the park loops: the
        // kv upsert and the journal row are ONE transaction (db.rs), so replay
        // can safely skip the write — it provably committed with its row.
        #[derive(Serialize)]
        struct Req {
            key: String,
            value: String,
        }
        let seq = self.j.next_seq;
        self.j.next_seq += 1;
        let _span = tracing::info_span!("host_call", kind = "kv-set", seq).entered();
        let id = self.j.workflow_id.clone();
        let req_json = serde_json::to_string(&Req {
            key: key.clone(),
            value: value.clone(),
        })
        .map_err(|e| trap(e.into()))?;

        if let Some((rkind, rreq, _)) = db::get_journal_row(&self.j.db, &id, seq).map_err(trap)? {
            if rkind != "kv-set" || rreq != req_json {
                return Err(trap(anyhow::anyhow!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced (kv-set, {req_json})."
                )));
            }
            return Ok(());
        }
        db::kv_set_and_journal(&mut self.j.db, &id, seq, &key, &value, &req_json).map_err(trap)?;
        Ok(())
    }

    fn kv_get(&mut self, key: String) -> wasmtime::Result<Option<String>> {
        // v1.2 — journaled read: the value READ is recorded, so replay returns
        // what this execution saw even if the row has changed since.
        #[derive(Serialize)]
        struct Req {
            key: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            v: Option<String>,
        }
        let seq = self.j.next_seq;
        self.j.next_seq += 1;
        let _span = tracing::info_span!("host_call", kind = "kv-get", seq).entered();
        let id = self.j.workflow_id.clone();
        let req_json =
            serde_json::to_string(&Req { key: key.clone() }).map_err(|e| trap(e.into()))?;

        if let Some((rkind, rreq, rresp)) =
            db::get_journal_row(&self.j.db, &id, seq).map_err(trap)?
        {
            if rkind != "kv-get" || rreq != req_json {
                return Err(trap(anyhow::anyhow!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced (kv-get, {req_json})."
                )));
            }
            let r: Resp = serde_json::from_str(&rresp)
                .map_err(|e| trap(anyhow::Error::new(e).context("corrupt journal response")))?;
            return Ok(r.v);
        }
        db::kv_get_and_journal(&mut self.j.db, &id, seq, &key, &req_json).map_err(trap)
    }

    fn secret(&mut self, name: String) -> wasmtime::Result<Result<String, String>> {
        // v2.1 — the one host call whose RESULT must never be journaled.
        // Hand-rolled: the journal records {"name"} → {"salt", "sha256"} (or
        // the guest-visible {"err"}), and replay re-reads the LIVE secrets
        // file and verifies the salted hash. Same value → return the live bytes (the
        // guest can't tell replay from first run). Different value → trap
        // LOUDLY: the workflow's journaled requests were built with the old
        // value, so silently substituting the new one would diverge replay in
        // ways no one can debug. The operator restores the old value or
        // cancels the workflow. Rotation is safe once the workflow completes,
        // or for workflows that have not read the secret yet.
        #[derive(Serialize)]
        struct Req {
            name: String,
        }
        let seq = self.j.next_seq;
        self.j.next_seq += 1;
        let id = self.j.workflow_id.clone();
        let req_json =
            serde_json::to_string(&Req { name: name.clone() }).map_err(|e| trap(e.into()))?;
        let _span = tracing::info_span!("host_call", kind = "secret", seq).entered();

        let live = self.secrets.get(&name);

        if let Some((rkind, rreq, rresp)) =
            db::get_journal_row(&self.j.db, &id, seq).map_err(trap)?
        {
            if rkind != "secret" || rreq != req_json {
                return Err(trap(anyhow::anyhow!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced (secret, {req_json})."
                )));
            }
            let v: serde_json::Value = serde_json::from_str(&rresp)
                .map_err(|e| trap(anyhow::Error::new(e).context("corrupt journal response")))?;
            if let Some(err) = v.get("err").and_then(|e| e.as_str()) {
                // The original read failed; replay returns the same failure.
                return Ok(Err(err.to_string()));
            }
            let want = v
                .get("sha256")
                .and_then(|h| h.as_str())
                .ok_or_else(|| trap(anyhow::anyhow!("corrupt journal response for secret")))?;
            let salt = v
                .get("salt")
                .and_then(|s| s.as_str())
                .ok_or_else(|| trap(anyhow::anyhow!("corrupt journal response for secret")))?;
            return match live {
                Ok(val) if salted_sha256(salt, &val) == want => {
                    self.remember_secret(&name, &val);
                    Ok(Ok(val))
                }
                Ok(_) => Err(trap(anyhow::anyhow!(
                    "secret '{name}' changed mid-workflow: the live value no longer matches \
                     the hash journaled at seq {seq}. Restore the previous value in the \
                     secrets file, or cancel this workflow — resuming with a different \
                     secret would silently diverge from the journal."
                ))),
                Err(e) => Err(trap(anyhow::anyhow!(
                    "secret '{name}' was readable when journaled at seq {seq} but is now \
                     unavailable ({e}). Restore it, or cancel this workflow."
                ))),
            };
        }

        // Live path: journal name→salted hash (never the value), THEN return
        // the value. Salted so the journal (which travels into every backup)
        // is not an offline rainbow-table oracle for guessable secrets.
        match live {
            Ok(val) => {
                let salt = hex::encode(uuid::Uuid::new_v4().as_bytes());
                let h = salted_sha256(&salt, &val);
                let resp = format!("{{\"salt\":\"{salt}\",\"sha256\":\"{h}\"}}");
                db::insert_journal_row(&self.j.db, &id, seq, "secret", &req_json, &resp)
                    .map_err(trap)?;
                self.remember_secret(&name, &val);
                Ok(Ok(val))
            }
            Err(e) => {
                let resp = serde_json::to_string(&serde_json::json!({ "err": e }))
                    .map_err(|er| trap(er.into()))?;
                db::insert_journal_row(&self.j.db, &id, seq, "secret", &req_json, &resp)
                    .map_err(trap)?;
                Ok(Err(e))
            }
        }
    }

    fn provider_call(
        &mut self,
        name: String,
        kind: String,
        request: String,
    ) -> wasmtime::Result<Result<String, String>> {
        // v2.2 — capability providers (provider.rs, PROVIDERS.md). Journaled
        // like any effect: kind `custom:<name>:<kind>` (provider identity
        // lives in the KIND so replay verifies the guest still calls the same
        // provider), request redacted like http-request. Replay returns the
        // recorded response without instantiating the provider at all. An
        // unregistered name is a guest-visible err — journaled, so a replay
        // on an engine where the provider has since been registered STILL
        // returns the recorded err (determinism beats convenience).
        #[derive(Serialize)]
        struct Req {
            request: String,
        }
        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum Resp {
            Ok { ok: String },
            Err { err: String },
        }
        let jkind = format!("custom:{name}:{kind}");
        let jreq = Req {
            request: redact(&request, &self.read_secrets),
        };
        let engine = self.engine.clone();
        let providers = self.providers.clone();
        let max_mem = self.max_guest_memory;
        // v2.5 — dispatch on the registered TIER. The None and pure arms
        // journal exactly as v2.2 did (one row, same kind/request bytes), so
        // pre-v2.5 journals replay unchanged. Tier is looked up before the
        // journaled() call, but replay stays tier-independent: a recorded row
        // is returned without touching the provider in all three arms (the
        // effectful arm's scan consumes the whole recorded scope).
        // v2.6 — snapshot (tier, component) under a read lock, then run
        // without holding it: a registry mutation mid-call affects the NEXT
        // call; this one keeps the component it resolved. Replay stays
        // registry-independent — recorded rows win in all three arms.
        let resolved = providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&name)
            .map(|e| (e.effectful, e.component.clone()));
        match resolved {
            None => {
                let r = self
                    .j
                    .journaled(&jkind, &jreq, move || {
                        Ok(Resp::Err {
                            err: format!("no provider '{name}' registered on this engine"),
                        })
                    })
                    .map_err(trap)?;
                Ok(match r {
                    Resp::Ok { ok } => Ok(ok),
                    Resp::Err { err } => Err(err),
                })
            }
            Some((false, component)) => {
                let r = self
                    .j
                    .journaled(&jkind, &jreq, move || {
                        tracing::info!("invoking provider {name} kind {kind}");
                        Ok(
                            match crate::provider::call(&engine, &component, max_mem, &kind, &request)
                            {
                                Ok(ok) => Resp::Ok { ok },
                                Err(err) => Resp::Err { err },
                            },
                        )
                    })
                    .map_err(trap)?;
                Ok(match r {
                    Resp::Ok { ok } => Ok(ok),
                    Resp::Err { err } => Err(err),
                })
            }
            Some((true, component)) => {
                // Effectful: the provider scope may span several journal rows
                // (its wire calls at seqs N.., then the terminal custom: row).
                // First try to consume a COMPLETED recorded scope without
                // instantiating the provider at all.
                let jreq_json = serde_json::to_string(&jreq).map_err(|e| trap(e.into()))?;
                let inner_kind = format!("provider-http:{name}");
                match scan_provider_scope(
                    &self.j.db,
                    &self.j.workflow_id,
                    self.j.next_seq,
                    &jkind,
                    &inner_kind,
                    &jreq_json,
                )
                .map_err(trap)?
                {
                    ScanOutcome::Complete { next_seq, response } => {
                        self.j.next_seq = next_seq;
                        let r: Resp =
                            serde_json::from_str(&response).map_err(|e| trap(e.into()))?;
                        Ok(match r {
                            Resp::Ok { ok } => Ok(ok),
                            Resp::Err { err } => Err(err),
                        })
                    }
                    ScanOutcome::Live => {
                        // Fresh, or a crash left internal rows without a
                        // terminal: re-invoke live. Recorded wire calls replay
                        // inside the nested journaled() (never re-fired); new
                        // ones execute and commit at their own seqs.
                        tracing::info!("invoking provider {name} kind {kind}");
                        let nested = crate::journal::JournalCtx {
                            workflow_id: self.j.workflow_id.clone(),
                            db: db::open_conn(&self.db_path).map_err(trap)?,
                            next_seq: self.j.next_seq,
                        };
                        let (final_seq, outcome) = crate::provider::call_effectful(
                            &engine,
                            &component,
                            max_mem,
                            &name,
                            &kind,
                            &request,
                            nested,
                            self.read_secrets.clone(),
                            self.http.clone(),
                        );
                        self.j.next_seq = final_seq;
                        // Workflow-fatal (journal integrity inside the
                        // provider) propagates as a trap; provider-level
                        // failures are DATA, journaled in the terminal row.
                        let r = outcome.map_err(trap)?;
                        let resp = self
                            .j
                            .journaled(&jkind, &jreq, move || {
                                Ok(match r {
                                    Ok(ok) => Resp::Ok { ok },
                                    Err(err) => Resp::Err { err },
                                })
                            })
                            .map_err(trap)?;
                        Ok(match resp {
                            Resp::Ok { ok } => Ok(ok),
                            Resp::Err { err } => Err(err),
                        })
                    }
                }
            }
        }
    }

    fn sleep_ms(&mut self, ms: u64) -> wasmtime::Result<()> {
        // Task 2.4 — durable sleep. Hand-rolled instead of journaled() because a
        // park loop sits between the replay check and the journal commit; the §0
        // invariants are unchanged (replay check first, row commits before return).
        //
        // Durability: the FIRST arrival at this seq writes a timers row with an
        // ABSOLUTE wake_at, then parks. kill -9 mid-sleep → recovery replays to
        // this same seq, finds no journal row but an existing timers row, KEEPS its
        // wake_at, and parks only for the remainder (the phase-1 full-re-sleep wart
        // is gone). The wake-up (timer delete + journal insert + status flip) is
        // ONE transaction — kill -9 at any instant leaves either the parked state
        // (remainder honored on recovery) or the fully-committed wake.
        #[derive(Serialize)]
        struct Req {
            ms: u64,
        }
        let seq = self.j.next_seq;
        self.j.next_seq += 1;
        let _span = tracing::info_span!("host_call", kind = "sleep-ms", seq).entered();
        let id = self.j.workflow_id.clone();
        let req_json = serde_json::to_string(&Req { ms }).map_err(|e| trap(e.into()))?;

        // Replay path — the same verification journaled() performs.
        if let Some((rkind, rreq, _)) = db::get_journal_row(&self.j.db, &id, seq).map_err(trap)? {
            if rkind != "sleep-ms" || rreq != req_json {
                return Err(trap(anyhow::anyhow!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced (sleep-ms, {req_json}). The workflow code \
                     has diverged from its journal."
                )));
            }
            return Ok(()); // recorded response is {} — nothing to surface
        }

        // Live path: get-or-create the durable deadline, then park until it passes.
        let wake_at = match db::get_timer_wake_at(&self.j.db, &id).map_err(trap)? {
            Some(w) => w, // restart mid-sleep: keep the original deadline
            None => {
                let w = now_ms() + ms as i64;
                db::insert_timer(&self.j.db, &id, seq, w).map_err(trap)?;
                w
            }
        };
        db::set_status(&self.j.db, &id, "sleeping", None).map_err(trap)?;
        loop {
            if self.notifier.is_aborted(&id) {
                return Err(trap(anyhow::Error::new(AbortForUpgrade)));
            }
            let remaining = wake_at - now_ms();
            if remaining <= 0 {
                break;
            }
            // 1s cap keeps the DB (via now_ms drift) re-checked even if every
            // notify is lost — the Notifier is a latency optimization only.
            self.notifier
                .wait(&id, std::time::Duration::from_millis(remaining.min(1000) as u64));
        }
        // Final re-check: never commit completion after an abort (SPEC.md Task 2.4).
        if self.notifier.is_aborted(&id) {
            return Err(trap(anyhow::Error::new(AbortForUpgrade)));
        }
        db::finish_sleep(&mut self.j.db, &id, seq, &req_json).map_err(trap)?;
        Ok(())
    }

    fn now_ms(&mut self) -> wasmtime::Result<u64> {
        #[derive(Serialize, Deserialize)]
        struct Resp {
            ms: i64,
        }
        let r = self.j.journaled("now-ms", &Empty {}, || Ok(Resp { ms: now_ms() })).map_err(trap)?;
        Ok(r.ms as u64)
    }

    fn random_u64(&mut self) -> wasmtime::Result<u64> {
        #[derive(Serialize, Deserialize)]
        struct Resp {
            v: u64,
        }
        let r = self.j.journaled("random-u64", &Empty {}, || {
            // Random bits from uuid v4 (already a dependency; no `rand` needed),
            // masked to 63 bits ON PURPOSE: values above i64::MAX lose precision
            // through sqlite3's json_extract(), which the acceptance script uses to
            // compare this value textually against the guest's output. ~62 effective
            // random bits — plenty for journaled (non-cryptographic) randomness.
            let v = (uuid::Uuid::new_v4().as_u128() as u64) & (i64::MAX as u64);
            Ok(Resp { v })
        }).map_err(trap)?;
        Ok(r.v)
    }

    fn await_event(&mut self, name: String) -> wasmtime::Result<String> {
        // Task 2.5 — external events. Same shape as durable sleep above: hand-rolled
        // replay check + park loop. The delivery step is a SINGLE transaction
        // (db::deliver_event_and_journal) — consuming the event and journaling it
        // must be atomic, or a crash between them loses (or double-delivers) it.
        #[derive(Serialize)]
        struct Req {
            name: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            payload: String,
        }
        let seq = self.j.next_seq;
        self.j.next_seq += 1;
        let _span = tracing::info_span!("host_call", kind = "await-event", seq).entered();
        let id = self.j.workflow_id.clone();
        let req_json =
            serde_json::to_string(&Req { name: name.clone() }).map_err(|e| trap(e.into()))?;

        // Replay path — the same verification journaled() performs.
        if let Some((rkind, rreq, rresp)) =
            db::get_journal_row(&self.j.db, &id, seq).map_err(trap)?
        {
            if rkind != "await-event" || rreq != req_json {
                return Err(trap(anyhow::anyhow!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced (await-event, {req_json}). The workflow \
                     code has diverged from its journal."
                )));
            }
            let r: Resp = serde_json::from_str(&rresp)
                .map_err(|e| trap(anyhow::Error::new(e).context("corrupt journal response")))?;
            return Ok(r.payload);
        }

        // Live path: park until a matching event arrives.
        db::set_status(&self.j.db, &id, "waiting_event", None).map_err(trap)?;
        loop {
            // Abort check BEFORE the delivering txn: an aborted worker must never
            // consume an event (SPEC.md Task 2.5).
            if self.notifier.is_aborted(&id) {
                return Err(trap(anyhow::Error::new(AbortForUpgrade)));
            }
            // The delivery txn also flips status back to running (all-or-nothing
            // with the consume + journal — see db::deliver_event_and_journal).
            if let Some(payload) =
                db::deliver_event_and_journal(&mut self.j.db, &id, seq, &name, &req_json)
                    .map_err(trap)?
            {
                return Ok(payload);
            }
            self.notifier.wait(&id, std::time::Duration::from_millis(1000));
        }
    }

    fn checkpoint(&mut self, state: Vec<u8>) -> wasmtime::Result<()> {
        // Task 3.3 — logical checkpoint + journal pruning, via journaled() as the
        // spec directs. Two subtleties:
        //   * C is this call's OWN seq — read it before journaled() claims it.
        //   * exec cannot borrow self.j.db (journaled holds &mut self), so it
        //     opens a SCOPED second connection for the snapshot+prune transaction
        //     (the one-connection-per-thread rule bends here, deliberately —
        //     status.md deviation 13). That txn and the wrapper's row-C INSERT are
        //     two separate transactions BY DESIGN ("the wrapper then inserts
        //     journal row C as usual"): a crash between them leaves the snapshot
        //     at C with row C missing, which recovery tolerates — resume starts at
        //     next_seq = C+1 and never reads row C.
        #[derive(Serialize)]
        struct Req {
            len: usize,
        }
        let c_seq = self.j.next_seq;
        let id = self.j.workflow_id.clone();
        let db_path = self.db_path.clone();
        self.j
            .journaled("checkpoint", &Req { len: state.len() }, move || {
                let mut conn = db::open_conn(&db_path)?;
                db::snapshot_and_prune(&mut conn, &id, c_seq, &state)?;
                Ok(Empty {})
            })
            .map_err(trap)?;
        Ok(())
    }

    fn log(&mut self, msg: String) -> wasmtime::Result<()> {
        // NOT journaled: no seq claimed, no row written (SPEC.md §4.1).
        tracing::info!(workflow = %self.j.workflow_id, "guest: {msg}");
        Ok(())
    }
}

/// Live-path HTTP GET. 30s timeout is set on the Agent (runner.rs); the body read is
/// capped at exactly 1 MiB (deterministic truncation, then lossy utf-8 so a cut
/// mid-sequence still yields a string); non-2xx maps to Err("status NNN").
///
/// Task 2.6 — retries, live path only: up to 3 attempts for transport errors
/// (including a failed body read) and status ≥500; 4xx NEVER retries (the server
/// answered; asking again won't change its mind). This function runs inside the
/// journaled() closure, so however many attempts happen here, exactly ONE journal
/// row records the final outcome — replay sees a single result.
/// (status, headers, 1 MiB-capped utf-8 body) — http-request's live-path output.
pub(crate) type HttpOut = (u16, Vec<(String, String)>, String);

/// v1.2 — live path of http-request. Returns (status, headers, body) for ANY
/// HTTP status; only transport-level failures (and body reads that die) are
/// Err. `retry_attempts` EXTRA tries apply to transport errors and 5xx — a 5xx
/// on the final try comes back as data, since a response is a response.
/// v2.5 — what the effectful provider-call replay scan concluded about the
/// journal from the current cursor onward.
#[derive(Debug)]
pub(crate) enum ScanOutcome {
    /// A completed recorded scope: consume through the terminal row (cursor
    /// moves to `next_seq`) and return the terminal's recorded response —
    /// the provider is NOT instantiated.
    Complete { next_seq: i64, response: String },
    /// No terminal row (fresh call, or a crash mid-provider left only
    /// internal rows): invoke the provider live; recorded internals replay
    /// inside the nested journaled().
    Live,
}

/// Walk the journal from `start`: rows of `inner_kind` (this provider's wire
/// calls) belong to the scope; the first `jkind` row is the terminal and must
/// carry the same request the live guest just produced (the journaled()
/// nondeterminism rule, applied at scope granularity). ANY other kind means
/// the guest has diverged from its journal.
pub(crate) fn scan_provider_scope(
    db: &rusqlite::Connection,
    workflow_id: &str,
    start: i64,
    jkind: &str,
    inner_kind: &str,
    jreq_json: &str,
) -> anyhow::Result<ScanOutcome> {
    let mut s = start;
    loop {
        match db::get_journal_row(db, workflow_id, s)? {
            None => return Ok(ScanOutcome::Live),
            Some((rkind, rreq, rresp)) => {
                if rkind == jkind {
                    if rreq != jreq_json {
                        anyhow::bail!(
                            "nondeterministic replay at seq {s}: recorded ({rkind}, {rreq}) \
                             but live code produced ({jkind}, {jreq_json}). The workflow code \
                             has diverged from its journal."
                        );
                    }
                    return Ok(ScanOutcome::Complete {
                        next_seq: s + 1,
                        response: rresp,
                    });
                } else if rkind == inner_kind {
                    s += 1;
                } else {
                    anyhow::bail!(
                        "nondeterministic replay at seq {s}: recorded ({rkind}, {rreq}) inside \
                         the provider scope that started at seq {start}, but live code produced \
                         a provider-call ({jkind}, {jreq_json}). The workflow code has diverged \
                         from its journal."
                    );
                }
            }
        }
    }
}

pub(crate) fn do_http_request(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    retry_attempts: u32,
    timeout_ms: u32,
) -> Result<HttpOut, String> {
    const BACKOFF_MS: [u64; 3] = [500, 1000, 2000];
    // Cap: a guest asking for u32::MAX retries would pin its worker inside one
    // host call — which cancel cannot interrupt. 8 extra tries (~16s of gaps +
    // 9 × 30s worst-case timeouts) is the ceiling; guests wanting more should
    // loop with sleep-ms between calls, which IS a cancellable park point.
    let tries = (retry_attempts.min(8) as usize) + 1;
    let mut last_err = String::new();
    for attempt in 0..tries {
        let mut req = agent.request(method, url);
        // v2.1 — per-attempt timeout; 0 = the Agent default (30s, runner.rs).
        if timeout_ms > 0 {
            req = req.timeout(std::time::Duration::from_millis(timeout_ms as u64));
        }
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let result = match body {
            Some(b) => req.send_string(b),
            None => req.call(),
        };
        match result {
            Ok(resp) => match read_response(resp) {
                Ok(out) => return Ok(out),
                // Body read died mid-stream: transport-class, retryable.
                Err(e) => last_err = e,
            },
            // ureq folds non-2xx into Error::Status but hands the Response
            // over — for http-request a status is DATA, not an error.
            Err(ureq::Error::Status(code, resp)) => {
                if code >= 500 && attempt + 1 < tries {
                    last_err = format!("status {code}");
                } else {
                    return read_response(resp);
                }
            }
            Err(e) => last_err = format!("transport: {e}"),
        }
        if attempt + 1 < tries {
            std::thread::sleep(std::time::Duration::from_millis(
                BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)],
            ));
        }
    }
    Err(last_err)
}

/// Status, headers (first value per name), and the 1 MiB-capped utf-8 body.
fn read_response(resp: ureq::Response) -> Result<HttpOut, String> {
    let status = resp.status();
    let names = resp.headers_names();
    let mut headers = Vec::with_capacity(names.len());
    for n in names {
        if let Some(v) = resp.header(&n) {
            headers.push((n.clone(), v.to_string()));
        }
    }
    let mut buf = Vec::new();
    resp.into_reader()
        .take(1024 * 1024)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read error: {e}"))?;
    Ok((status, headers, String::from_utf8_lossy(&buf).into_owned()))
}

/// v2.3 — the wire-side idempotency header for http-request. `seq` is the
/// journal seq this call is about to claim, so `<workflow_id>:<seq>` is
/// stable across replay AND across the crash-and-resend window (recovery
/// re-executes the same seq → same key → the remote can dedupe). The guest
/// passing the header itself wins; an EMPTY value means "send no key at all".
pub(crate) fn inject_idempotency_key(
    mut headers: Vec<(String, String)>,
    workflow_id: &str,
    seq: i64,
) -> Vec<(String, String)> {
    match headers
        .iter()
        .position(|(k, _)| k.eq_ignore_ascii_case("keel-idempotency-key"))
    {
        Some(i) if headers[i].1.is_empty() => {
            headers.remove(i); // explicit opt-out
        }
        Some(_) => {} // guest-supplied key wins
        None => headers.push((
            "keel-idempotency-key".to_string(),
            format!("{workflow_id}:{seq}"),
        )),
    }
    headers
}

// --- Secrets (v2.1) ---------------------------------------------------------

/// hex(sha256(salt-hex ‖ value)) — what the journal stores instead of a
/// secret. The salt (fresh per journal row) keeps a stolen journal/backup
/// from being an offline dictionary oracle for guessable secrets.
fn salted_sha256(salt: &str, value: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(value.as_bytes());
    hex::encode(h.finalize())
}

/// KEY=VALUE lines; blank lines and #-comments skipped; keys trimmed, values
/// taken VERBATIM after the first '=' (a secret may contain '=' or spaces).
/// A non-blank, non-comment line without '=' is an error, and so is a
/// duplicate key — silently skipping a typo or shadowing a value would turn
/// into a lying "secret not found" / wrong-secret later.
fn parse_secrets(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) => {
                let k = k.trim();
                if out.iter().any(|(n, _)| n == k) {
                    return Err(format!("secrets file line {}: duplicate key '{k}'", i + 1));
                }
                out.push((k.to_string(), v.to_string()));
            }
            None => return Err(format!("secrets file line {}: no '=' separator", i + 1)),
        }
    }
    Ok(out)
}

/// Read + parse a secrets file. pub: main.rs validates the file at startup
/// (fail fast on a config error) with the same code the host call uses.
pub fn load_secrets(path: &str) -> Result<Vec<(String, String)>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("secrets file {path}: {e}"))?;
    parse_secrets(&text)
}

/// Replace every read-secret VALUE occurring in `s` with {{secret:name}} —
/// applied to journaled request fields only, never to what goes on the wire.
/// Longest value first so a secret containing another is replaced whole;
/// empty values never match (they would "occur" everywhere).
pub(crate) fn redact(s: &str, secrets: &[(String, String)]) -> String {
    if secrets.is_empty() {
        return s.to_string();
    }
    let mut by_len: Vec<&(String, String)> =
        secrets.iter().filter(|(_, v)| !v.is_empty()).collect();
    by_len.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let mut out = s.to_string();
    for (name, val) in by_len {
        if out.contains(val.as_str()) {
            out = out.replace(val.as_str(), &format!("{{{{secret:{name}}}}}"));
        }
    }
    out
}

fn do_http_get(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    // Backoff schedule per SPEC.md Task 2.6. With 3 attempts only the first two
    // gaps are reachable; the 2s slot is the schedule's next step if attempts are
    // ever raised.
    const BACKOFF_MS: [u64; 3] = [500, 1000, 2000];
    let attempts = 3;
    let mut last_err = String::new();
    for (attempt, gap_ms) in BACKOFF_MS.iter().enumerate().take(attempts) {
        match agent.get(url).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                match resp.into_reader().take(1024 * 1024).read_to_end(&mut buf) {
                    Ok(_) => return Ok(String::from_utf8_lossy(&buf).into_owned()),
                    // Connection died mid-body: a transport failure — retryable.
                    Err(e) => last_err = format!("read error: {e}"),
                }
            }
            Err(ureq::Error::Status(code, _)) if code < 500 => return Err(format!("status {code}")),
            Err(ureq::Error::Status(code, _)) => last_err = format!("status {code}"),
            Err(e) => last_err = format!("transport: {e}"),
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_millis(*gap_ms));
        }
    }
    Err(last_err)
}

// --- Unit tests (v2.1) -------------------------------------------------------
// The secrets pure functions: parsing (strict) and redaction (the property the
// whole feature hangs on — secret bytes never reach a journaled request).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_secrets_skips_blanks_and_comments_keeps_values_verbatim() {
        let s = parse_secrets("# comment\n\nAPI_KEY=sk-live-123\nPW = p=a ss \n").unwrap();
        assert_eq!(
            s,
            vec![
                ("API_KEY".to_string(), "sk-live-123".to_string()),
                ("PW".to_string(), " p=a ss ".to_string()),
            ]
        );
    }

    #[test]
    fn parse_secrets_rejects_lines_without_equals() {
        let err = parse_secrets("GOOD=1\nnot a secret line\n").unwrap_err();
        assert!(err.contains("line 2"), "got: {err}");
    }

    #[test]
    fn parse_secrets_rejects_duplicate_keys() {
        let err = parse_secrets("A=1\nB=2\nA=3\n").unwrap_err();
        assert!(err.contains("duplicate key 'A'") && err.contains("line 3"), "got: {err}");
    }

    #[test]
    fn salted_hash_is_deterministic_and_salt_sensitive() {
        assert_eq!(salted_sha256("aa", "v"), salted_sha256("aa", "v"));
        assert_ne!(salted_sha256("aa", "v"), salted_sha256("bb", "v"));
        assert_ne!(salted_sha256("aa", "v"), salted_sha256("aa", "w"));
    }

    #[test]
    fn redact_replaces_values_with_named_placeholders() {
        let secrets = vec![("tok".to_string(), "sk-live-123".to_string())];
        assert_eq!(
            redact("Bearer sk-live-123", &secrets),
            "Bearer {{secret:tok}}"
        );
        assert_eq!(redact("no secrets here", &secrets), "no secrets here");
    }

    #[test]
    fn idempotency_key_injected_overridden_or_suppressed() {
        // No header → injected as workflow_id:seq.
        let h = inject_idempotency_key(vec![], "wf-1", 7);
        assert_eq!(h, vec![("keel-idempotency-key".to_string(), "wf-1:7".to_string())]);
        // Guest-supplied (any case) wins untouched.
        let h = inject_idempotency_key(
            vec![("Keel-Idempotency-Key".to_string(), "mine".to_string())],
            "wf-1",
            7,
        );
        assert_eq!(h, vec![("Keel-Idempotency-Key".to_string(), "mine".to_string())]);
        // Empty value = suppress entirely.
        let h = inject_idempotency_key(
            vec![("keel-idempotency-key".to_string(), String::new())],
            "wf-1",
            7,
        );
        assert!(h.is_empty());
    }

    #[test]
    fn redact_longest_value_first_and_ignores_empty() {
        let secrets = vec![
            ("short".to_string(), "abc".to_string()),
            ("long".to_string(), "abcdef".to_string()),
            ("empty".to_string(), String::new()),
        ];
        // The longer secret wins where they overlap; the empty one never fires.
        assert_eq!(redact("xx abcdef yy abc", &secrets), "xx {{secret:long}} yy {{secret:short}}");
    }

    // --- v2.5: the effectful provider-call replay scan -----------------------

    fn scan_db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&c).unwrap();
        crate::db::insert_module(&c, "h", "m", b"\0asm").unwrap();
        crate::db::create_workflow(&c, "w", "h", "{}").unwrap();
        c
    }

    fn put_row(c: &rusqlite::Connection, seq: i64, kind: &str, req: &str, resp: &str) {
        c.execute(
            "INSERT INTO journal (workflow_id, seq, kind, request, response, created_at)
             VALUES ('w', ?1, ?2, ?3, ?4, 0)",
            rusqlite::params![seq, kind, req, resp],
        )
        .unwrap();
    }

    const JK: &str = "custom:relay:relay";
    const IK: &str = "provider-http:relay";
    const RQ: &str = r#"{"request":"{}"}"#;

    #[test]
    fn scan_empty_and_internals_only_mean_live() {
        let c = scan_db();
        assert!(matches!(
            scan_provider_scope(&c, "w", 0, JK, IK, RQ).unwrap(),
            ScanOutcome::Live
        ));
        // A crash mid-provider leaves internals without a terminal.
        put_row(&c, 0, IK, "{}", "{}");
        put_row(&c, 1, IK, "{}", "{}");
        assert!(matches!(
            scan_provider_scope(&c, "w", 0, JK, IK, RQ).unwrap(),
            ScanOutcome::Live
        ));
    }

    #[test]
    fn scan_consumes_a_completed_scope_through_its_terminal() {
        let c = scan_db();
        put_row(&c, 0, IK, "{}", "{}");
        put_row(&c, 1, IK, "{}", "{}");
        put_row(&c, 2, JK, RQ, r#"{"ok":"done"}"#);
        match scan_provider_scope(&c, "w", 0, JK, IK, RQ).unwrap() {
            ScanOutcome::Complete { next_seq, response } => {
                assert_eq!(next_seq, 3);
                assert_eq!(response, r#"{"ok":"done"}"#);
            }
            ScanOutcome::Live => panic!("expected Complete"),
        }
        // Zero internals (a pure component under the effectful grant).
        put_row(&c, 3, JK, RQ, r#"{"ok":"pure"}"#);
        match scan_provider_scope(&c, "w", 3, JK, IK, RQ).unwrap() {
            ScanOutcome::Complete { next_seq, response } => {
                assert_eq!((next_seq, response.as_str()), (4, r#"{"ok":"pure"}"#));
            }
            ScanOutcome::Live => panic!("expected Complete"),
        }
    }

    #[test]
    fn scan_diverged_journal_is_nondeterminism() {
        let c = scan_db();
        // A foreign kind where the provider-call should be.
        put_row(&c, 0, "http-request", "{}", "{}");
        let e = scan_provider_scope(&c, "w", 0, JK, IK, RQ).unwrap_err();
        assert!(e.to_string().contains("nondeterministic replay"), "got: {e}");
        // A terminal whose request differs from what live code produced.
        let c = scan_db();
        put_row(&c, 0, JK, r#"{"request":"other"}"#, "{}");
        let e = scan_provider_scope(&c, "w", 0, JK, IK, RQ).unwrap_err();
        assert!(e.to_string().contains("nondeterministic replay"), "got: {e}");
        // A foreign kind after internals (guest or provider diverged mid-scope).
        let c = scan_db();
        put_row(&c, 0, IK, "{}", "{}");
        put_row(&c, 1, "sleep", "{}", "{}");
        let e = scan_provider_scope(&c, "w", 0, JK, IK, RQ).unwrap_err();
        assert!(e.to_string().contains("nondeterministic replay"), "got: {e}");
    }
}
