//! Keeps `GET /config`'s `research.observed` current — what a run at each effort
//! level has actually cost on this host.
//!
//! The ladder `GET /config` publishes says what a level *grants*: `high` allows
//! 3600 seconds. It never said what a level *takes*, and those are wildly different
//! numbers — measured here, `high` runs finish around 400 s against that 3600 s
//! grant. A caller choosing a level therefore had no way to price it, which is how
//! `effort: high` ends up on a question that reads one dictionary literal, and how
//! a caller plans a queue of investigations without knowing that each will take
//! seven minutes rather than the granted hour.
//!
//! The journal already holds the answer: `research_runs` records `model`, `effort`
//! and `elapsed_ms` for every finished run. This worker turns that into percentiles
//! on a tick, in the house shape (interval + `MissedTickBehavior::Skip` +
//! cancellation token, tick body split into a clock-free [`refresh_once`]), and
//! `get_config` reads the snapshot it leaves behind.
//!
//! **A failed tick keeps the previous snapshot**, for the reason the model catalog
//! does: a client's estimate going blank because one read failed is worse than one a
//! few minutes old, and `refreshed_at` is what tells "no runs yet" from "never
//! read".
//!
//! Deliberately per `(model, effort)` and not aggregated: a 31B model and a 3B model
//! at the same level are not the same wait, so a single number per level would be an
//! average of two distributions nobody runs.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};

/// How far back a percentile looks.
///
/// Not configurable: it trades sample size against staleness, and both ends are
/// bad in the same way — too short and a level with one run a week never has an
/// estimate, too long and last month's model is still quoted. A month is what the
/// default `retention_days` keeps anyway.
const OBSERVED_WINDOW_DAYS: i64 = 30;

/// Below this many runs a percentile is noise, so none is published.
///
/// Two runs would let a single cold-start outlier become "what high effort costs".
/// Publishing nothing is honest and the client falls back to the grant.
const MIN_RUNS_FOR_ESTIMATE: usize = 3;

/// What one `(model, effort)` pair has actually cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedEffort {
    pub model: String,
    pub effort: String,
    /// Runs the estimate is built from — the caller's basis for trusting it.
    pub runs: usize,
    pub p50_seconds: u64,
    pub p90_seconds: u64,
}

/// The published snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunStats {
    pub observed: Vec<ObservedEffort>,
    /// Unix seconds of the last **successful** read; `None` = never succeeded, which
    /// is a different statement from "no runs recorded".
    pub refreshed_at: Option<i64>,
}

/// One writer (this worker), many readers (`get_config`).
///
/// `tokio::sync::RwLock` for the reason the model catalog uses one: the reader is an
/// async handler that must not block, and neither side holds the guard across an
/// `.await`.
pub type SharedRunStats = Arc<tokio::sync::RwLock<RunStats>>;

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    stats: SharedRunStats,
    refresh_interval_seconds: u64,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(refresh_interval_seconds.max(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!(
        refresh_interval_seconds,
        window_days = OBSERVED_WINDOW_DAYS,
        "Research run statistics: started (the observed cost `GET /config` publishes)."
    );

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Research run statistics: shutting down.");
                break;
            }
        }

        match refresh_once(&db_pool, &token).await {
            Ok(observed) => {
                let mut guard = stats.write().await;
                *guard = RunStats {
                    observed,
                    refreshed_at: Some(unix_now()),
                };
            }
            Err(e) => {
                // Keep the previous snapshot, and do not re-stamp `refreshed_at`:
                // an estimate a few minutes old beats none, and the unmoved stamp
                // is the only thing that says the reads have stopped succeeding.
                warn!(
                    error = %e,
                    "Failed to read research run statistics; keeping the previous snapshot. \
                     Sysadmin: check the database is readable."
                );
            }
        }
    }
}

/// One read, turned into percentiles. Clock-free so it is testable — the
/// `refresh_once` / `collect_once` precedent.
pub(crate) async fn refresh_once(
    db_pool: &SQLite3Pool,
    token: &CancellationToken,
) -> Result<Vec<ObservedEffort>, SQLite3PoolError> {
    let cutoff = unix_now() - OBSERVED_WINDOW_DAYS * 86_400;
    let rows: Vec<(String, String, i64)> = db_pool
        .transaction(token.child_token(), move |tx| {
            // Every journalled run, whatever stopped it: a caller waiting does not
            // care *why* a run took what it took, and a cancelled run is never
            // journalled at all, so there is no abandonment to filter out.
            // Ordinary research only: `observed` is the promise `GET /config`
            // makes about `POST /research`, and a challenge's cost profile
            // differs (the whole subject report rides in its prompt).
            tx.prepare(
                "SELECT model, effort, elapsed_ms FROM research_runs \
                 WHERE created_at >= ?1 AND kind = 'research' \
                 ORDER BY model, effort, elapsed_ms",
            )?
            .query_map(rusqlite::params![cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(SQLite3PoolError::from)
        })
        .await?;

    Ok(summarize(rows))
}

/// Group the (already sorted) rows and take percentiles.
fn summarize(rows: Vec<(String, String, i64)>) -> Vec<ObservedEffort> {
    let mut out: Vec<ObservedEffort> = Vec::new();
    let mut current: Option<(String, String, Vec<i64>)> = None;

    let flush = |acc: Option<(String, String, Vec<i64>)>, out: &mut Vec<ObservedEffort>| {
        let Some((model, effort, durations)) = acc else {
            return;
        };
        if durations.len() < MIN_RUNS_FOR_ESTIMATE {
            return;
        }
        out.push(ObservedEffort {
            model,
            effort,
            runs: durations.len(),
            p50_seconds: percentile_seconds(&durations, 0.5),
            p90_seconds: percentile_seconds(&durations, 0.9),
        });
    };

    for (model, effort, elapsed_ms) in rows {
        match &mut current {
            Some((m, e, durations)) if *m == model && *e == effort => durations.push(elapsed_ms),
            _ => {
                flush(current.take(), &mut out);
                current = Some((model, effort, vec![elapsed_ms]));
            }
        }
    }
    flush(current.take(), &mut out);
    out
}

/// Nearest-rank percentile over an ascending slice, rounded to whole seconds.
///
/// Nearest-rank rather than interpolated: the samples are wall-clock durations of
/// real runs, and a caller wants "a run like this took that long", not a synthetic
/// value between two runs.
fn percentile_seconds(sorted_ms: &[i64], q: f64) -> u64 {
    if sorted_ms.is_empty() {
        return 0;
    }
    let rank = (q * sorted_ms.len() as f64).ceil() as usize;
    let idx = rank.clamp(1, sorted_ms.len()) - 1;
    (sorted_ms[idx].max(0) as u64).div_ceil(1000)
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

    fn rows(spec: &[(&str, &str, i64)]) -> Vec<(String, String, i64)> {
        spec.iter()
            .map(|(m, e, ms)| ((*m).to_string(), (*e).to_string(), *ms))
            .collect()
    }

    #[test]
    fn percentiles_come_from_the_runs_themselves() {
        let observed = summarize(rows(&[
            ("gemma4:31b", "high", 300_000),
            ("gemma4:31b", "high", 400_000),
            ("gemma4:31b", "high", 500_000),
            ("gemma4:31b", "high", 900_000),
        ]));

        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].runs, 4);
        // Nearest-rank: the median of four samples is the second, not an average.
        assert_eq!(observed[0].p50_seconds, 400);
        assert_eq!(observed[0].p90_seconds, 900);
    }

    /// Two models at the same level are two different waits; averaging them would
    /// describe a run nobody makes.
    #[test]
    fn each_model_and_level_is_its_own_estimate() {
        let observed = summarize(rows(&[
            ("big", "high", 600_000),
            ("big", "high", 600_000),
            ("big", "high", 600_000),
            ("small", "high", 30_000),
            ("small", "high", 30_000),
            ("small", "high", 30_000),
        ]));

        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].model, "big");
        assert_eq!(observed[0].p50_seconds, 600);
        assert_eq!(observed[1].model, "small");
        assert_eq!(observed[1].p50_seconds, 30);
    }

    /// Publishing an estimate from one run invites treating an outlier as the
    /// price of a level. Silence is the honest answer, and the client falls back
    /// to the grant.
    #[test]
    fn too_few_runs_publish_no_estimate() {
        let observed = summarize(rows(&[("m", "low", 1000), ("m", "low", 2000)]));
        assert!(observed.is_empty());
    }

    #[test]
    fn the_same_model_at_two_levels_stays_two_rows() {
        let observed = summarize(rows(&[
            ("m", "low", 10_000),
            ("m", "low", 10_000),
            ("m", "low", 10_000),
            ("m", "medium", 60_000),
            ("m", "medium", 60_000),
            ("m", "medium", 60_000),
        ]));

        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].effort, "low");
        assert_eq!(observed[1].effort, "medium");
    }
}
