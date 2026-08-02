//! The in-process registry of research runs that are still running.
//!
//! `/research` had no such thing for its whole life, and the gap was not a missing
//! nicety: a run acquired a `max_concurrent` permit, spent up to
//! `max_seconds + report_timeout_ms` holding it, and was **invisible** for all of
//! it. `run_id` did not exist until the journal insert at the very end
//! (`db::research::insert_run`), a cancelled run is never journalled at all, and
//! the stored-research list only ever shows runs that finished. So an operator
//! whose only slot was occupied — the normal shape of this host, where
//! `max_concurrent = 1` — could see a 429, could see `mindex_research_active` sit
//! at 1 in `/metrics`, and could learn nothing else: not which project, not which
//! question, not how long, and above all had no way to end it short of restarting
//! the service.
//!
//! This registry is the identity half of that fix. It holds, for each live run,
//! what it is and the token that stops it, so `GET /research/active` can name it,
//! `DELETE /research/active/{run_id}` can cancel it, `GET /health` can say a slot
//! is busy, and the watchdog can cancel one that outlived its own worst case.
//!
//! **The entry and the permit are released by the same task.** The permit rides in
//! the spawned job (`post_research`), so the registry guard must too — held
//! anywhere else, the two would drift apart and the list would describe slots that
//! are free (or hide ones that are not). That is the same reasoning that puts the
//! SQLite connection's return inside its blocking task rather than in the awaiting
//! future.
//!
//! Cancellation itself is unchanged: it is still `token.cancel()`, and the
//! disconnect path (`SseEventStream`'s `Drop`) still reaches it. The registry only
//! adds a second hand on the same lever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// Characters of the question kept for the listing.
///
/// The list exists to let a human recognise their own run, which the opening
/// clause does; the whole question can be several kilobytes and this response is
/// read by an operator in a terminal. Cut on a **character** boundary — a question
/// is arbitrary UTF-8, and this is the same rule the report parser follows.
const QUESTION_PREVIEW_CHARS: usize = 160;

/// Slack over a run's own worst case before it counts as wedged.
///
/// Shared by the two things that must agree on the word: `GET /health`, which
/// degrades the verdict, and `worker::research_watchdog`, which cancels. Two
/// separate numbers would let health call a run healthy while the watchdog killed
/// it, which is the worst of both. A minute is far more than a run's deadlines
/// need to unwind (its journal write, its last events) and far less than a human
/// waits before suspecting a wedge.
pub const WEDGE_GRACE: Duration = Duration::from_secs(60);

/// One research run that is still running.
#[derive(Debug, Clone)]
pub struct InflightRun {
    pub run_id: String,
    /// Simple (hyphen-less) form, as everywhere else in the schema.
    pub project_guid: String,
    /// Truncated to [`QUESTION_PREVIEW_CHARS`] at registration.
    pub question: String,
    pub model: String,
    pub effort: &'static str,
    /// Monotonic start, for the age. `Instant` rather than a wall clock because
    /// this is the number the watchdog compares against a duration.
    pub started: Instant,
    /// Wall-clock start, for the wire. Both are kept because neither answers the
    /// other's question.
    pub started_at: i64,
    /// What this run was granted, so the reader can see an age against its bound.
    pub granted_seconds: u64,
    /// `granted_seconds * 1000 + report_timeout_ms` — the longest this run may
    /// legitimately take. Past it the run has outlived every deadline it has, which
    /// is a defect rather than a queue, and the watchdog acts on exactly that.
    pub worst_case_ms: u64,
    token: CancellationToken,
}

impl InflightRun {
    pub fn age_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// True once the run has outlived its investigation deadline *and* its report
    /// window, plus whatever grace the caller allows.
    pub fn is_overdue(&self, grace: Duration) -> bool {
        self.age_ms() > self.worst_case_ms.saturating_add(grace.as_millis() as u64)
    }
}

/// The process-wide table of live runs. Cheap to clone; one shared map.
#[derive(Clone, Default)]
pub struct ResearchRegistry {
    runs: Arc<Mutex<HashMap<String, InflightRun>>>,
}

impl ResearchRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a run and hand back the guard that unregisters it.
    ///
    /// The guard must be moved into the same future that holds the semaphore
    /// permit — see the module comment.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        run_id: String,
        project_guid: String,
        question: &str,
        model: String,
        effort: &'static str,
        granted_seconds: u64,
        worst_case_ms: u64,
        token: CancellationToken,
    ) -> InflightGuard {
        let run = InflightRun {
            run_id: run_id.clone(),
            project_guid,
            question: preview(question),
            model,
            effort,
            started: Instant::now(),
            started_at: unix_now(),
            granted_seconds,
            worst_case_ms,
            token,
        };
        self.lock().insert(run_id.clone(), run);
        InflightGuard {
            runs: Arc::clone(&self.runs),
            run_id,
        }
    }

    /// Every live run, oldest first — the order an operator hunting a stuck slot
    /// wants, and the order the watchdog would act in.
    pub fn snapshot(&self) -> Vec<InflightRun> {
        let mut runs: Vec<InflightRun> = self.lock().values().cloned().collect();
        runs.sort_by_key(|r| std::cmp::Reverse(r.age_ms()));
        runs
    }

    /// Cancel one run by id. `false` if no such run is live — which is also what a
    /// run that finished a moment ago looks like, so callers treat this as a 404
    /// rather than an error.
    ///
    /// The entry is **not** removed here: removal belongs to the guard in the job,
    /// so a cancelled-but-still-unwinding run keeps showing up (correctly) as
    /// holding its slot until it actually lets go.
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.lock().get(run_id) {
            Some(run) => {
                run.token.cancel();
                true
            }
            None => false,
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Exists because [`Self::len`] does: clippy's `len_without_is_empty` is right
    /// that a length-bearing type should answer both questions, and the tests below
    /// are its only production-shaped caller today.
    #[allow(dead_code, reason = "the pair clippy requires beside `len`")]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Age of the oldest live run, or `None` when nothing is running. The one
    /// number that distinguishes a busy slot from a wedged one.
    pub fn oldest_age_ms(&self) -> Option<u64> {
        self.lock().values().map(|r| r.age_ms()).max()
    }

    /// Runs past their own worst case — i.e. holding a slot that no deadline of
    /// theirs is going to free. Read-only; `GET /health` uses this to decide
    /// `degraded` while the watchdog uses [`Self::cancel_overdue`] to act on the
    /// same set, so the two can never disagree about the word "wedged".
    pub fn wedged(&self) -> Vec<InflightRun> {
        self.overdue(WEDGE_GRACE)
    }

    fn overdue(&self, grace: Duration) -> Vec<InflightRun> {
        self.lock()
            .values()
            .filter(|r| r.is_overdue(grace))
            .cloned()
            .collect()
    }

    /// Cancel every run past its worst case, returning **only the ones this call
    /// actually cancelled** so the caller can log and count them.
    ///
    /// The already-cancelled filter is what makes the counter mean something.
    /// Cancelling a token is idempotent, but a run parked in an await the token cannot
    /// reach does not leave the registry when it is cancelled — so without the filter
    /// the same wedged run was re-cancelled, re-warned and re-counted on every sweep,
    /// every 30 seconds, for as long as the process lived. It inflated the one counter
    /// documented to stay at zero into an unbounded number describing a single event,
    /// and filled the log with the same line. A run that stays here after being
    /// cancelled is still visible: `oldest_inflight_age_ms` keeps climbing and
    /// `GET /health` keeps saying `unhealthy`.
    pub fn cancel_overdue(&self, grace: Duration) -> Vec<InflightRun> {
        self.overdue(grace)
            .into_iter()
            .filter(|run| {
                let fresh = !run.token.is_cancelled();
                if fresh {
                    run.token.cancel();
                }
                fresh
            })
            .collect()
    }

    /// Move a run's start backwards, so a test can age it without sleeping.
    ///
    /// The alternative — a run whose worst case is zero, judged at age zero —
    /// tests a boundary that cannot occur (`max_seconds` is at least 1) and made
    /// the outcome depend on how many microseconds the test itself took.
    #[cfg(test)]
    pub fn backdate(&self, run_id: &str, by: Duration) {
        if let Some(run) = self.lock().get_mut(run_id) {
            run.started -= by;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, InflightRun>> {
        // A poisoned lock breaks no invariant here — the map is a plain table —
        // so recover rather than panic, as `IndexClaim` does.
        self.runs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Removes its run from the registry on drop.
pub struct InflightGuard {
    runs: Arc<Mutex<HashMap<String, InflightRun>>>,
    run_id: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut map = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&self.run_id);
    }
}

fn preview(question: &str) -> String {
    let trimmed = question.trim();
    match trimmed.char_indices().nth(QUESTION_PREVIEW_CHARS) {
        Some((byte, _)) => format!("{}…", &trimmed[..byte]),
        None => trimmed.to_string(),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(reg: &ResearchRegistry, id: &str, token: CancellationToken) -> InflightGuard {
        reg.register(
            id.to_string(),
            "guid".to_string(),
            "why is it built this way",
            "model".to_string(),
            "high",
            300,
            600_000,
            token,
        )
    }

    #[test]
    fn a_registered_run_is_listed_until_its_guard_drops() {
        let reg = ResearchRegistry::new();
        let guard = register(&reg, "a", CancellationToken::new());
        assert_eq!(reg.snapshot().len(), 1);
        assert_eq!(reg.snapshot()[0].run_id, "a");
        drop(guard);
        assert!(reg.is_empty());
        assert_eq!(reg.oldest_age_ms(), None);
    }

    /// Cancelling must not remove the entry: the run still holds its permit while
    /// it unwinds, and a list that hid it would report a free slot that is not.
    #[test]
    fn cancelling_stops_the_run_but_leaves_it_listed() {
        let reg = ResearchRegistry::new();
        let token = CancellationToken::new();
        let _guard = register(&reg, "a", token.clone());

        assert!(reg.cancel("a"));
        assert!(token.is_cancelled());
        assert_eq!(reg.snapshot().len(), 1);
    }

    #[test]
    fn cancelling_an_unknown_run_is_false_not_an_error() {
        let reg = ResearchRegistry::new();
        assert!(!reg.cancel("nope"));
    }

    /// The watchdog's rule: only a run past its own worst case is touched, so a
    /// long-but-legitimate run is never cancelled out from under a client.
    #[test]
    fn only_a_run_past_its_worst_case_is_overdue() {
        let reg = ResearchRegistry::new();
        let fresh = CancellationToken::new();
        let _g = register(&reg, "fresh", fresh.clone());
        assert!(reg.cancel_overdue(Duration::ZERO).is_empty());
        assert!(!fresh.is_cancelled());

        // A second run, aged past the 600 s worst case `register` gives it.
        let stale = CancellationToken::new();
        let _g2 = register(&reg, "stale", stale.clone());
        reg.backdate("stale", Duration::from_secs(700));

        let cancelled = reg.cancel_overdue(Duration::ZERO);
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].run_id, "stale");
        assert!(stale.is_cancelled());
        assert!(!fresh.is_cancelled());
    }

    /// A run wedged in an await its token cannot reach stays registered after being
    /// cancelled, so the sweep keeps finding it. It was re-cancelled, re-warned and
    /// re-counted every 30 seconds for the life of the process — turning
    /// `research_watchdog_cancels_total`, the one counter documented to stay at zero,
    /// into an unbounded number describing a single event, and filling the log with
    /// the same line. The run stays visible through `oldest_age_ms` and `wedged`.
    #[test]
    fn a_wedged_run_is_only_cancelled_once() {
        let reg = ResearchRegistry::new();
        let token = CancellationToken::new();
        let _g = register(&reg, "wedged", token.clone());
        reg.backdate("wedged", Duration::from_secs(700));

        assert_eq!(reg.cancel_overdue(Duration::ZERO).len(), 1);
        assert!(token.is_cancelled());
        assert!(
            reg.cancel_overdue(Duration::ZERO).is_empty(),
            "a second sweep must report nothing to count"
        );
        assert_eq!(
            reg.wedged().len(),
            1,
            "and the run is still visible to /health, which is what says it is stuck"
        );
    }

    /// A question is arbitrary UTF-8; the preview must never split a character.
    #[test]
    fn the_question_preview_cuts_on_a_character_boundary() {
        let reg = ResearchRegistry::new();
        let question = "щ".repeat(QUESTION_PREVIEW_CHARS * 2);
        let _g = reg.register(
            "a".to_string(),
            "guid".to_string(),
            &question,
            "model".to_string(),
            "low",
            1,
            1,
            CancellationToken::new(),
        );
        let shown = &reg.snapshot()[0].question;
        assert!(shown.ends_with('…'));
        assert_eq!(shown.chars().count(), QUESTION_PREVIEW_CHARS + 1);
    }
}
