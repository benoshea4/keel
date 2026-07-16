// notifier.rs — Task 2.3: in-process wake-ups for parked workflow threads.
//
// A parked thread (durable sleep Task 2.4, await-event Task 2.5) NEVER depends on
// this for correctness: every park loop re-checks the database on a 1-second
// wait_timeout regardless. The Notifier only cuts that up-to-1s poll latency when
// the wake-up arrives while the engine is running. Losing a notify costs at most
// one poll interval; it can never lose data (SPEC.md Task 2.3).
//
// Both wait() and notify() get-or-insert the per-workflow entry: a notify that
// arrives before the first wait must land in a real map entry, not vanish into a
// missing key.
//
// The abort set is PHASE 3 machinery (live upgrade, Tasks 3.5/3.6): the park loops
// consult is_aborted() from day one so an upgrade can yank a parked workflow, but
// nothing calls set_abort until phase 3.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Per-workflow generation counter + condvar. The counter closes the classic race:
/// a notify() that lands between a waiter's condition re-check and its sleep bumps
/// the generation, so the wait returns immediately instead of blocking the full
/// timeout. Spurious wakes are allowed everywhere — callers loop.
type Entry = Arc<(Mutex<u64>, Condvar)>;

pub struct Notifier {
    entries: Mutex<HashMap<String, Entry>>,
    aborts: Mutex<HashSet<String>>,
}

impl Notifier {
    pub fn new() -> Self {
        Notifier {
            entries: Mutex::new(HashMap::new()),
            aborts: Mutex::new(HashSet::new()),
        }
    }

    fn entry(&self, id: &str) -> Entry {
        self.entries
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// Park for at most `timeout`, waking early if notify(id) arrives after this
    /// call has read the generation counter.
    pub fn wait(&self, id: &str, timeout: Duration) {
        let entry = self.entry(id);
        let (gen, cv) = &*entry;
        let guard = gen.lock().unwrap();
        let seen = *guard;
        let _ = cv
            .wait_timeout_while(guard, timeout, |g| *g == seen)
            .unwrap();
    }

    #[allow(dead_code)] // first caller lands in Task 2.5 (POST events) — remove then
    pub fn notify(&self, id: &str) {
        let entry = self.entry(id);
        let (gen, cv) = &*entry;
        *gen.lock().unwrap() += 1;
        cv.notify_all();
    }

    /// PHASE 3 (Task 3.6): flag a parked workflow so its park loop bails out with
    /// AbortForUpgrade at the next check; the notify makes "next check" be now.
    #[allow(dead_code)] // wired up by PHASE 3 Task 3.6 — remove this allow then
    pub fn set_abort(&self, id: &str) {
        self.aborts.lock().unwrap().insert(id.to_string());
        self.notify(id);
    }

    pub fn is_aborted(&self, id: &str) -> bool {
        self.aborts.lock().unwrap().contains(id)
    }

    /// PHASE 3: the upgrade handler MUST call this on every failure exit path, or
    /// the workflow zombifies at its next park (SPEC.md troubleshooting table).
    #[allow(dead_code)] // wired up by PHASE 3 Task 3.6 — remove this allow then
    pub fn clear_abort(&self, id: &str) {
        self.aborts.lock().unwrap().remove(id);
    }
}
