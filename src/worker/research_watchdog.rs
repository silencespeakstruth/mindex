//! Cancels a research run that has outlived every deadline it has.
//!
//! A run is bounded twice already: `DeadlineToken` stops the investigation at
//! `budget.max_seconds`, and a second one stops the report phase at
//! `[research].report_timeout_ms`. Both are children of the job token, both reach
//! `chat_stream`'s `select!`s, and between them they cover the run's whole shape.
//! So why a third mechanism?
//!
//! Because a handful of awaits in the job are, by design or by accident, *not*
//! under a token: `effective_num_ctx`'s `/api/show` lookup, the `response.text()`
//! read on Ollama's error path, and — deliberately — the journal write, which uses
//! a fresh token so a finished run still gets its row. Each is individually
//! bounded, but each holds a `max_concurrent` permit while it runs, and nothing
//! stopped a future one from being added without a token at all. With
//! `max_concurrent = 1`, one wedged slot is a total outage of the feature, and the
//! only remedy used to be restarting the service.
//!
//! The rule is deliberately narrow: **a busy slot is not a defect**. This cancels
//! only a run past `max_seconds + report_timeout_ms + WEDGE_GRACE`, i.e. one that has
//! already outlived the longest it could legitimately take. A long run that is
//! merely long is never touched.
//!
//! It also keeps `research_inflight_oldest_age_seconds` current — the gauge that
//! distinguishes "the slot is busy" from "the slot has been busy for an hour".

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::backend::inflight::{ResearchRegistry, WEDGE_GRACE};
use crate::backend::metrics::Metrics;

/// How often the registry is swept.
///
/// Not configurable: this is a backstop, not a tuning knob. Sweeping faster would
/// cancel nothing sooner (the grace dominates), and slower would only widen the
/// window in which a wedged slot is still held.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

pub async fn run(registry: ResearchRegistry, metrics: Arc<Metrics>, token: CancellationToken) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!(
        sweep_interval_secs = SWEEP_INTERVAL.as_secs(),
        grace_secs = WEDGE_GRACE.as_secs(),
        "Research watchdog: started (cancels a run past its own worst case)."
    );

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Research watchdog: shutting down.");
                break;
            }
        }
        // The same grace `GET /health` degrades on, so the two never disagree about
        // which runs are wedged.
        sweep_once(&registry, &metrics, WEDGE_GRACE);
    }
}

/// One sweep. Split out of the loop so it is testable without a clock — the
/// `gc::collect` / `collect_once` precedent. `grace` is a parameter rather than
/// the const so a test can make a run overdue without sleeping through it.
pub(crate) fn sweep_once(registry: &ResearchRegistry, metrics: &Metrics, grace: Duration) {
    metrics
        .state
        .research_inflight_oldest_age_seconds
        .set((registry.oldest_age_ms().unwrap_or(0) / 1000) as i64);

    for run in registry.cancel_overdue(grace) {
        warn!(
            run_id = %run.run_id,
            project_guid = %run.project_guid,
            model = %run.model,
            effort = run.effort,
            age_ms = run.age_ms(),
            worst_case_ms = run.worst_case_ms,
            "A research run outlived its investigation deadline and its report \
             window; cancelling it to free the slot. Sysadmin: this should not \
             happen — check the Ollama logs for a wedged request, and mindex's \
             for the run's last step."
        );
        metrics.research.watchdog_cancels.inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry holding one run of the given worst case, aged by `ran_for`.
    fn registry_with(
        worst_case_ms: u64,
        ran_for: Duration,
        token: CancellationToken,
    ) -> ResearchRegistry {
        let reg = ResearchRegistry::new();
        // Leaked deliberately: the guard's `Drop` unregisters, and this test wants
        // the entry to outlive the call the way a live job's would.
        std::mem::forget(reg.register(
            "run".to_string(),
            "guid".to_string(),
            "question",
            "model".to_string(),
            "high",
            worst_case_ms / 1000,
            worst_case_ms,
            token,
        ));
        reg.backdate("run", ran_for);
        reg
    }

    /// The whole point of the narrow rule: a run inside its budget is left alone,
    /// however long that budget is. This is the regression that matters — a
    /// watchdog that cancels healthy long runs is worse than none.
    #[test]
    fn a_run_inside_its_worst_case_is_not_cancelled() {
        let metrics = Metrics::new();
        let token = CancellationToken::new();
        // An hour granted, fifty minutes spent: long, and entirely legitimate.
        let reg = registry_with(3_600_000, Duration::from_secs(3000), token.clone());

        sweep_once(&reg, &metrics, Duration::ZERO);

        assert!(!token.is_cancelled());
        assert_eq!(metrics.research.watchdog_cancels.get(), 0);
        assert_eq!(reg.snapshot().len(), 1);
    }

    #[test]
    fn a_run_past_its_worst_case_is_cancelled_and_counted() {
        let metrics = Metrics::new();
        let token = CancellationToken::new();
        // Five minutes granted, ten spent — past every deadline it has.
        let reg = registry_with(300_000, Duration::from_secs(600), token.clone());

        // Still covered by a generous grace.
        sweep_once(&reg, &metrics, Duration::from_secs(3600));
        assert!(!token.is_cancelled());
        assert_eq!(metrics.research.watchdog_cancels.get(), 0);

        // With none, it is overdue.
        sweep_once(&reg, &metrics, Duration::ZERO);
        assert!(token.is_cancelled());
        assert_eq!(metrics.research.watchdog_cancels.get(), 1);
    }

    /// The gauge is the difference between "a slot is busy" and "a slot has been
    /// busy for an hour", so it must be written on every sweep — including the
    /// sweep that finds nothing, which is what resets it to zero.
    #[test]
    fn the_oldest_age_gauge_is_written_even_when_nothing_is_running() {
        let metrics = Metrics::new();
        metrics.state.research_inflight_oldest_age_seconds.set(42);

        sweep_once(&ResearchRegistry::new(), &metrics, Duration::ZERO);

        assert_eq!(metrics.state.research_inflight_oldest_age_seconds.get(), 0);
    }
}
