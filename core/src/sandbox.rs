// sandbox.rs — micro-cloud phases 4/5: the pieces every UNTRUSTED-code store
// shares (functions and judge solvers — NOT workflow stores, whose limits are
// StoreLimits + fuel in runner.rs).
//
// MemLimiter is the ext spec's Task 5.1 struct verbatim: it both ENFORCES the
// per-invocation memory cap and RECORDS peak usage for the ledger, and its
// `denied` flag is how classify() distinguishes "grew past the limit" (mle)
// from an ordinary trap — wasmtime reports a refused grow as an allocation
// trap, so the flag is the only truthful signal.
//
// classify() is Task 4.3's outcome classification, normative ORDER included:
// the judge (5.2) reuses it verbatim, so it lives here, written once.

/// One invocation's limit triple — a route's stored limits, the judge's
/// per-case constants, or an app backend's defaults (ext spec Task 6.1:
/// dispatch is "a function taking (module_hash, limits, request)").
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    pub fuel: u64,
    pub mem: usize,
    pub time_ms: u64,
}

/// Per-invocation memory limiter + meter for function/solver stores.
pub struct MemLimiter {
    pub limit: usize,
    pub peak: usize,
    pub denied: bool,
}

impl wasmtime::ResourceLimiter for MemLimiter {
    fn memory_growing(
        &mut self,
        _cur: usize,
        desired: usize,
        _max: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.peak = self.peak.max(desired);
        if desired > self.limit {
            self.denied = true;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn table_growing(&mut self, _c: usize, d: usize, _m: Option<usize>) -> wasmtime::Result<bool> {
        Ok(d <= 1_000_000)
    }
}

/// Ledger outcomes (`invocations.outcome`). `GuestError` is never produced by
/// classify() — it's the caller's mapping for a solver that RETURNS Err
/// (world solver's result<..>): the sandbox held, the guest's own logic failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    GuestError,
    Tle,
    Mle,
    Oof,
    Trap,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::GuestError => "guest_error",
            Outcome::Tle => "tle",
            Outcome::Mle => "mle",
            Outcome::Oof => "oof",
            Outcome::Trap => "trap",
        }
    }
}

/// Task 4.3's outcome classification — THIS ORDER IS NORMATIVE (the ext spec's
/// troubleshooting table: a refused memory grow surfaces as a trap, so the
/// limiter's `denied` flag must be checked FIRST or every MLE reads as RE).
pub fn classify<T>(result: &Result<T, wasmtime::Error>, limiter: &MemLimiter) -> Outcome {
    if limiter.denied {
        return Outcome::Mle;
    }
    match result {
        Ok(_) => Outcome::Ok,
        Err(e) => match e.downcast_ref::<wasmtime::Trap>() {
            Some(wasmtime::Trap::OutOfFuel) => Outcome::Oof,
            Some(wasmtime::Trap::Interrupt) => Outcome::Tle,
            _ => Outcome::Trap,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim(denied: bool) -> MemLimiter {
        MemLimiter {
            limit: 1024,
            peak: 0,
            denied,
        }
    }

    #[test]
    fn classify_order_is_normative() {
        // denied wins over everything, even a fuel trap in hand.
        let fuel_err: Result<(), wasmtime::Error> =
            Err(wasmtime::Trap::OutOfFuel.into());
        assert_eq!(classify(&fuel_err, &lim(true)), Outcome::Mle);
        assert_eq!(classify(&fuel_err, &lim(false)), Outcome::Oof);

        let epoch_err: Result<(), wasmtime::Error> =
            Err(wasmtime::Trap::Interrupt.into());
        assert_eq!(classify(&epoch_err, &lim(false)), Outcome::Tle);

        let other: Result<(), wasmtime::Error> = Err(wasmtime::Error::msg("boom"));
        assert_eq!(classify(&other, &lim(false)), Outcome::Trap);

        let ok: Result<(), wasmtime::Error> = Ok(());
        assert_eq!(classify(&ok, &lim(false)), Outcome::Ok);
    }

    #[test]
    fn limiter_records_peak_and_denies_past_limit() {
        use wasmtime::ResourceLimiter as _;
        let mut m = lim(false);
        assert!(m.memory_growing(0, 512, None).unwrap());
        assert_eq!(m.peak, 512);
        assert!(!m.memory_growing(512, 4096, None).unwrap());
        assert!(m.denied);
        assert_eq!(m.peak, 4096); // peak records the ATTEMPT — that's the point
    }
}
