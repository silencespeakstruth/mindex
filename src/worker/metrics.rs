//! The state-gauge collector: everything in [`StateMetrics`] is recomputed here,
//! on a tick, and nowhere else.
//!
//! Counters are recorded where the thing happens; *state* cannot be, because
//! nothing happens when a project simply continues to hold 400 files. So one
//! worker asks SQLite periodically, in the gc/retry shape (interval +
//! `MissedTickBehavior::Skip` + a cancellation token), with the tick body split
//! out as [`collect_once`] so it is testable without a clock.
//!
//! **Clear-and-repopulate is the rule this file exists to get right.** A
//! `prometheus-client` `Family` retains a label set for the life of the process,
//! so a deleted project would keep reporting its last known file count until
//! restart. Each tick therefore builds the complete new value map from SQL
//! *first*, then clears and repopulates in one synchronous block with no `.await`
//! between the two — `clear()` takes the family's write lock and `get_or_create`
//! takes it straight back, so a scrape cannot land in the gap. Two structural
//! guards keep this safe: only `StateMetrics` is ever cleared, and `StateMetrics`
//! holds gauges only. Clearing a *counter* would read as a process restart to
//! Prometheus and permanently re-baseline every `rate()` over it.
//!
//! **What is deliberately absent.** There is no `SUM(LENGTH(code))` per project:
//! `code` is the largest column in the schema, and scanning it every tick would
//! evict the page cache the search candidate query depends on — the write-time
//! `index_code_bytes_total` counter answers the same question better. And there
//! is no drift gauge: `/drift` compares against a *client-posted* manifest and
//! the server never walks a tree, so a server-side drift level does not exist to
//! be measured (see the counters in `post_drift`).

use std::sync::Arc;
use std::time::Duration;

use rusqlite::Row;
use tokio_util::future::FutureExt as _;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::backend::metrics::{
    DependencyLabels, Metrics, ProjectLabels, ProjectLangLabels, ProjectRoleLabels,
    ProjectStatusLabels,
};
use crate::backend::v0::models::ProgrammingLanguage;
use crate::db::qdrant::VectorStore;
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
use crate::models::bge_m3::BGEm3Model;
use crate::models::ollama::OllamaModel;

/// Collector settings (`[metrics]` config plus `[workers].max_retries`, which
/// defines "permanently failed").
///
/// A struct rather than five loose parameters, following `RetryTuning` — and it
/// is what keeps `run` under clippy's argument limit, which is also why `model_id`
/// belongs here rather than as a ninth parameter. `Clone` and not `Copy` because of
/// that `String`.
#[derive(Debug, Clone)]
pub struct MetricsTuning {
    pub refresh_interval_seconds: u64,
    pub probe_dependencies: bool,
    pub max_retries: i64,
    /// The embedding model whose rows this collector describes. `project_files` is
    /// keyed `(project_guid, model_id, path)`, so the research staleness join would
    /// otherwise match a run's baseline across every model the database has held.
    pub model_id: String,
}

/// The dependencies the collector probes, when probing is on.
///
/// `query_embedder` is `Some` only when the deployment is actually split, decided
/// by `Arc::ptr_eq` exactly as `GET /health` decides it — comparing URLs would
/// call one instance two things.
pub struct ProbeTargets {
    pub store: Arc<dyn VectorStore>,
    pub embedder: Arc<dyn BGEm3Model>,
    pub query_embedder: Option<Arc<dyn BGEm3Model>>,
    pub ollama: Arc<dyn OllamaModel>,
}

/// One project's row in an aggregate, before it becomes a gauge.
type Counted<K> = Vec<(String, K, i64)>;

/// Everything one tick reads, gathered before a single gauge is touched.
#[derive(Default)]
struct Snapshot {
    files_by_status: Counted<String>,
    files_by_language: Counted<&'static str>,
    chunks_active: Counted<&'static str>,
    chunks_deleted: Vec<(String, i64)>,
    symbols_by_role: Counted<String>,
    last_indexed: Vec<(String, i64)>,
    permanently_failed: Vec<(String, i64)>,
    projects: i64,
    status_log_rows: i64,
    research_runs: Vec<(String, i64)>,
    research_pinned: Vec<(String, i64)>,
    research_stale: Vec<(String, i64)>,
    db_size_bytes: i64,
}

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    metrics: Arc<Metrics>,
    tuning: MetricsTuning,
    probes: Option<ProbeTargets>,
    research_semaphore: Arc<tokio::sync::Semaphore>,
    research_max_concurrent: usize,
    token: CancellationToken,
) {
    let mut interval =
        tokio::time::interval(Duration::from_secs(tuning.refresh_interval_seconds.max(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!(
        refresh_interval_seconds = tuning.refresh_interval_seconds,
        probe_dependencies = tuning.probe_dependencies,
        "Metrics collector: started."
    );

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Metrics collector: shutting down.");
                break;
            }
        }

        collect_once(&db_pool, &metrics, &tuning, &token).await;

        // Derived, never incremented: a run finishes on the leaked research
        // runtime and a dropped SSE stream is its *normal* exit, so an inc/dec
        // pair around the spawn would leak on every ordinary cancellation.
        let available = research_semaphore.available_permits();
        metrics
            .state
            .research_permits_available
            .set(available as i64);
        metrics
            .state
            .research_active
            .set(research_max_concurrent.saturating_sub(available) as i64);

        if let Some(p) = &probes {
            probe_dependencies(p, &metrics).await;
        }
    }
}

/// One tick's worth of state. Split out of the loop so it is testable without a
/// clock — the `gc::collect` precedent.
pub(crate) async fn collect_once(
    db_pool: &SQLite3Pool,
    metrics: &Metrics,
    tuning: &MetricsTuning,
    token: &CancellationToken,
) {
    // One transaction for all seven aggregates: holding one of the pool's
    // connections for a few milliseconds a minute is nothing, while seven
    // transactions would be seven `spawn_blocking` round-trips.
    let model_id = tuning.model_id.clone();
    let max_retries = tuning.max_retries;
    let snapshot = match db_pool
        .transaction(token.clone(), move |tx| {
            read_snapshot(tx, max_retries, &model_id)
        })
        .with_cancellation_token(token)
        .await
    {
        Some(Ok(s)) => s,
        Some(Err(SQLite3PoolError::Cancelled)) | None => return,
        Some(Err(e)) => {
            warn!(
                error = ?e,
                "Metrics collector: failed to read the state aggregates; keeping the \
                 previous gauges for this tick. Check the DB file is readable."
            );
            return;
        }
    };

    apply(metrics, &snapshot);
}

fn read_snapshot(
    tx: &rusqlite::Transaction,
    max_retries: i64,
    model_id: &str,
) -> Result<Snapshot, SQLite3PoolError> {
    let mut s = Snapshot::default();

    // Language comes back as `ProgrammingLanguage` rather than a raw string, so
    // the label can be a `&'static str` from `name()` — the closed-set rule. A
    // value the CHECK constraint would reject cannot become a label.
    let lang = |row: &Row<'_>, idx: usize| -> rusqlite::Result<&'static str> {
        Ok(row.get::<_, ProgrammingLanguage>(idx)?.name())
    };

    s.files_by_status = tx
        .prepare("SELECT project_guid, status, COUNT(*) FROM project_files GROUP BY 1, 2")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    s.files_by_language = tx
        .prepare(
            "SELECT project_guid, programming_language, COUNT(*)
             FROM project_files GROUP BY 1, 2",
        )?
        .query_map([], |r| Ok((r.get(0)?, lang(r, 1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    s.chunks_active = tx
        .prepare(
            "SELECT c.project_guid, f.programming_language, COUNT(*)
             FROM project_file_chunks c
             JOIN project_files f
               ON f.project_guid = c.project_guid
              AND f.model_id     = c.model_id
              AND f.path         = c.file_path
             WHERE c.status = 'active'
             GROUP BY 1, 2",
        )?
        .query_map([], |r| Ok((r.get(0)?, lang(r, 1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    // The GC backlog, without a language dimension: nobody dashboards a backlog
    // by language, and dropping it halves the family.
    s.chunks_deleted = tx
        .prepare(
            "SELECT project_guid, COUNT(*) FROM project_file_chunks
             WHERE status = 'deleted' GROUP BY 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    s.symbols_by_role = tx
        .prepare("SELECT project_guid, role, COUNT(*) FROM project_file_symbols GROUP BY 1, 2")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    // A Unix-epoch gauge; Grafana renders `time() - x` as age.
    s.last_indexed = tx
        .prepare(
            "SELECT project_guid, MAX(status_updated_at) FROM project_files
             WHERE status = 'indexed' GROUP BY 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    // What `warn_permanently_failed` currently only logs: files the retry worker
    // has given up on and will never touch again.
    s.permanently_failed = tx
        .prepare(
            "SELECT project_guid, COUNT(*) FROM project_files
             WHERE status = 'failed' AND retry_count >= ?1 GROUP BY 1",
        )?
        .query_map([max_retries], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    s.projects = tx.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?;
    s.status_log_rows = tx.query_row("SELECT COUNT(*) FROM project_file_status_log", [], |r| {
        r.get(0)
    })?;

    s.research_runs = tx
        .prepare("SELECT project_guid, COUNT(*) FROM research_runs GROUP BY 1")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    s.research_pinned = tx
        .prepare(
            "SELECT project_guid, COUNT(*) FROM research_runs
              WHERE expires_at IS NULL GROUP BY 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    // The one join in this collector, and the one worth watching. It probes every
    // retained run's file list against `project_files` by primary key — order
    // `runs x files_per_run` indexed lookups a tick, which at the default
    // `[research].retention_days` is small, and which `retention_days` is the only
    // thing bounding. Raise that by an order of magnitude and this is the first gauge
    // to drop. It is still nothing like the `SUM(LENGTH(code))` scan the
    // "deliberately not measured" rule refused: that one evicts the page cache the
    // search candidate query depends on, this one touches an index.
    //
    // `model_id` is bound because project_files is keyed (project_guid, model_id,
    // path); joining on the path alone would match a baseline against every embedding
    // model the database has ever held.
    s.research_stale = tx
        .prepare(
            "SELECT r.project_guid, COUNT(*) FROM research_runs r
              WHERE EXISTS (
                    SELECT 1 FROM research_run_files rf
                    LEFT JOIN project_files pf
                           ON pf.project_guid = r.project_guid
                          AND pf.model_id     = ?1
                          AND pf.path         = rf.path
                          AND pf.status      != 'deleted'
                     WHERE rf.run_id = r.id
                       AND (pf.sha256 IS NULL OR pf.sha256 <> rf.sha256))
              GROUP BY 1",
        )?
        .query_map([model_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;

    let pages: i64 = tx.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let page_size: i64 = tx.pragma_query_value(None, "page_size", |r| r.get(0))?;
    s.db_size_bytes = pages.saturating_mul(page_size);

    Ok(s)
}

/// Clear every state family and repopulate it from `snapshot`.
///
/// **Synchronous, and deliberately has no `.await` in it.** See the module docs:
/// a scrape landing between a `clear()` and its repopulate would see an empty
/// family, and the only thing keeping that window closed is that nothing yields
/// inside this function. Do not make it `async`.
fn apply(metrics: &Metrics, snapshot: &Snapshot) {
    let s = &metrics.state;

    s.project_files.clear();
    for (project_guid, status, n) in &snapshot.files_by_status {
        s.project_files
            .get_or_create(&ProjectStatusLabels {
                project_guid: project_guid.clone(),
                status: status.clone(),
            })
            .set(*n);
    }

    s.project_files_by_language.clear();
    for (project_guid, language, n) in &snapshot.files_by_language {
        s.project_files_by_language
            .get_or_create(&ProjectLangLabels {
                project_guid: project_guid.clone(),
                language,
            })
            .set(*n);
    }

    s.project_chunks_active.clear();
    for (project_guid, language, n) in &snapshot.chunks_active {
        s.project_chunks_active
            .get_or_create(&ProjectLangLabels {
                project_guid: project_guid.clone(),
                language,
            })
            .set(*n);
    }

    s.project_chunks_deleted.clear();
    for (project_guid, n) in &snapshot.chunks_deleted {
        s.project_chunks_deleted
            .get_or_create(&ProjectLabels {
                project_guid: project_guid.clone(),
            })
            .set(*n);
    }

    s.project_symbols.clear();
    for (project_guid, role, n) in &snapshot.symbols_by_role {
        s.project_symbols
            .get_or_create(&ProjectRoleLabels {
                project_guid: project_guid.clone(),
                role: role.clone(),
            })
            .set(*n);
    }

    s.project_last_indexed.clear();
    for (project_guid, at) in &snapshot.last_indexed {
        s.project_last_indexed
            .get_or_create(&ProjectLabels {
                project_guid: project_guid.clone(),
            })
            .set(*at);
    }

    s.project_files_permanently_failed.clear();
    for (project_guid, n) in &snapshot.permanently_failed {
        s.project_files_permanently_failed
            .get_or_create(&ProjectLabels {
                project_guid: project_guid.clone(),
            })
            .set(*n);
    }

    s.projects.set(snapshot.projects);
    s.status_log_rows.set(snapshot.status_log_rows);
    for (family, rows) in [
        (&s.project_research_runs, &snapshot.research_runs),
        (&s.project_research_pinned, &snapshot.research_pinned),
        (&s.project_research_stale, &snapshot.research_stale),
    ] {
        family.clear();
        for (project_guid, n) in rows {
            family
                .get_or_create(&ProjectLabels {
                    project_guid: project_guid.clone(),
                })
                .set(*n);
        }
    }
    s.db_size_bytes.set(snapshot.db_size_bytes);
}

/// Ping every dependency concurrently. Each probe is already bounded by its
/// client's own health timeout, so the worst case is one timeout's worth of a
/// whole tick — and it is off the request path, unlike `GET /health`.
async fn probe_dependencies(p: &ProbeTargets, metrics: &Metrics) {
    let (qdrant, embedder, query, ollama) = tokio::join!(
        p.store.health(),
        p.embedder.health(),
        async {
            match &p.query_embedder {
                Some(c) => Some(c.health().await.is_ok()),
                None => None,
            }
        },
        p.ollama.health(),
    );

    let up = |dependency: &'static str, ok: bool| {
        metrics
            .state
            .dependency_up
            .get_or_create(&DependencyLabels { dependency })
            .set(i64::from(ok));
    };
    up("qdrant", qdrant.is_ok());
    up("embedder", embedder.is_ok());
    up("ollama", ollama.is_ok());
    // Reported only when the deployment is actually split; otherwise the series
    // would claim a second instance exists.
    if let Some(ok) = query {
        up("query_embedder", ok);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite3::SQLite3Pool;

    const MODEL: &str = "BAAI/bge-m3";

    fn tuning() -> MetricsTuning {
        MetricsTuning {
            refresh_interval_seconds: 60,
            probe_dependencies: false,
            max_retries: 3,
            model_id: MODEL.to_string(),
        }
    }

    // One connection: each ":memory:" connection is its own database, so a larger
    // pool would read a different (empty) one back.
    async fn pool() -> SQLite3Pool {
        let pool = SQLite3Pool::new(std::path::Path::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            crate::apply_pending_migrations(tx).map(|_| ())
        })
        .await
        .expect("migrations apply");
        pool
    }

    async fn seed_project(pool: &SQLite3Pool, guid: &'static str) {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                rusqlite::params![guid, MODEL],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, 'src/a.rs', ?3, 'rust', 'indexing')",
                rusqlite::params![guid, MODEL, "a".repeat(64)],
            )?;
            tx.execute(
                "UPDATE project_files SET status = 'indexed'
                 WHERE project_guid = ?1 AND path = 'src/a.rs'",
                rusqlite::params![guid],
            )?;
            Ok(())
        })
        .await
        .expect("seed");
    }

    /// The clear-and-repopulate guard, and the test most likely to catch a real
    /// bug: a `Family` never forgets a label set, so without the `clear()` a
    /// deleted project reports its last file count until the process restarts.
    #[tokio::test]
    async fn a_deleted_project_stops_reporting() {
        let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let pool = pool().await;
        seed_project(&pool, a).await;
        seed_project(&pool, b).await;

        let metrics = Metrics::new();
        let token = CancellationToken::new();
        collect_once(&pool, &metrics, &tuning(), &token).await;

        let text = metrics.render().expect("renders");
        assert!(text.contains(a), "project A missing: {text}");
        assert!(text.contains(b), "project B missing: {text}");
        assert!(
            text.contains(&format!(
                r#"mindex_project_files{{project_guid="{a}",status="indexed"}} 1"#
            )),
            "{text}"
        );
        assert!(text.contains("mindex_projects 2"), "{text}");

        // Drop project B outright (chunks first would be needed if it had any).
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute("DELETE FROM project_files WHERE project_guid = ?1", [b])?;
            tx.execute("DELETE FROM projects WHERE guid = ?1", [b])?;
            Ok(())
        })
        .await
        .expect("delete");

        collect_once(&pool, &metrics, &tuning(), &token).await;

        let text = metrics.render().expect("renders");
        assert!(text.contains(a), "project A should still report: {text}");
        assert!(
            !text.contains(b),
            "a deleted project is still reporting its last known state: {text}"
        );
        assert!(text.contains("mindex_projects 1"), "{text}");
    }

    /// `programming_language` becomes a label via `ProgrammingLanguage::name()`,
    /// not via the raw column — that is what keeps every label value in a set the
    /// server defines.
    #[tokio::test]
    async fn language_gauges_come_from_the_closed_language_set() {
        let a = "cccccccccccccccccccccccccccccccc";
        let pool = pool().await;
        seed_project(&pool, a).await;

        let metrics = Metrics::new();
        collect_once(&pool, &metrics, &tuning(), &CancellationToken::new()).await;

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(&format!(
                r#"mindex_project_files_by_language{{project_guid="{a}",language="rust"}} 1"#
            )),
            "{text}"
        );
    }

    /// An unreadable database must leave the previous tick's gauges standing
    /// rather than zeroing them: "I could not measure" is not "it is zero".
    #[tokio::test]
    async fn a_cancelled_tick_leaves_the_previous_gauges_alone() {
        let a = "dddddddddddddddddddddddddddddddd";
        let pool = pool().await;
        seed_project(&pool, a).await;

        let metrics = Metrics::new();
        collect_once(&pool, &metrics, &tuning(), &CancellationToken::new()).await;
        assert!(
            metrics
                .render()
                .expect("renders")
                .contains("mindex_projects 1")
        );

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        collect_once(&pool, &metrics, &tuning(), &cancelled).await;

        assert!(
            metrics
                .render()
                .expect("renders")
                .contains("mindex_projects 1"),
            "a cancelled tick wiped the gauges instead of leaving them"
        );
    }
}
