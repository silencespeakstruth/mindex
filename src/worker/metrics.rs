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
    DependencyLabels, Metrics, ProjectLabels, ProjectLangLabels, ProjectStatusLabels,
};
use crate::backend::v0::models::ProgrammingLanguage;
use crate::db::qdrant::{VectorStore, collection_name};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
use crate::models::embedder::Embedder;
use crate::models::ollama::OllamaModel;
use crate::models::registry::model_by_id;

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
    /// The active embedding model's canonical id — what names the collections
    /// the vector-count probe counts.
    pub model_id: String,
}

/// The dependencies the collector probes, when probing is on.
///
/// `query_embedder` is `Some` only when the deployment is actually split, decided
/// by `Arc::ptr_eq` exactly as `GET /health` decides it — comparing URLs would
/// call one instance two things.
pub struct ProbeTargets {
    pub store: Arc<dyn VectorStore>,
    pub embedder: Arc<dyn Embedder>,
    pub query_embedder: Option<Arc<dyn Embedder>>,
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
    symbols: Vec<(String, i64)>,
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
            probe_vector_counts(&p.store, &db_pool, &tuning.model_id, &metrics, &token).await;
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
    let max_retries = tuning.max_retries;
    let snapshot = match db_pool
        .transaction(token.clone(), move |tx| read_snapshot(tx, max_retries))
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

    s.symbols = tx
        .prepare("SELECT project_guid, COUNT(*) FROM project_file_symbols GROUP BY 1")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
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
    s.research_stale = tx
        .prepare(
            "SELECT r.project_guid, COUNT(*) FROM research_runs r
              WHERE EXISTS (
                    SELECT 1 FROM research_run_files rf
                    LEFT JOIN project_files pf
                           ON pf.project_guid = r.project_guid
                          AND pf.path         = rf.path
                          AND pf.status      != 'deleted'
                     WHERE rf.run_id = r.id
                       AND (pf.sha256 IS NULL OR pf.sha256 <> rf.sha256))
              GROUP BY 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
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
    for (project_guid, n) in &snapshot.symbols {
        s.project_symbols
            .get_or_create(&ProjectLabels {
                project_guid: project_guid.clone(),
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
    // Last, and only on the path that got a whole snapshot: this is what tells a
    // reader that the frozen-looking gauges above are current rather than stale.
    s.state_refreshed_at.set(crate::unix_now());
}

/// Ask Qdrant how many points each project actually holds.
///
/// Every other number on this dashboard comes from SQLite, which is why the failure
/// documented in `db/qdrant.rs` has no detector: with Qdrant's volume gone, SQLite
/// still reports every file `indexed`, `ensure_project` silently makes an empty
/// collection, and search answers `404 search.no_match` for ever. Against
/// `project_chunks_active`, this is the number that disagrees.
///
/// Costs one round-trip per project per tick, so it rides with the other probes under
/// `[metrics].probe_dependencies`. A project the store cannot answer for is **left
/// unwritten**, not zeroed — zero is the alarming value here and must never be
/// manufactured by an unreachable Qdrant, which `dependency_up{qdrant}` already
/// reports.
async fn probe_vector_counts(
    store: &Arc<dyn VectorStore>,
    db_pool: &SQLite3Pool,
    model_id: &str,
    metrics: &Metrics,
    token: &CancellationToken,
) {
    // The active model's collections are the ones search serves, so they are
    // the ones whose emptiness is the lost-volume signal. Resolved per call so
    // a bad id degrades to a no-op probe rather than a panic in a worker.
    let Some(spec) = model_by_id(model_id) else {
        warn!(
            model_id,
            "Metrics collector: unknown model id; skipping the vector-count probe."
        );
        return;
    };
    let projects = match db_pool
        .transaction(token.clone(), |tx| {
            tx.prepare("SELECT guid FROM projects")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(token)
        .await
    {
        Some(Ok(p)) => p,
        Some(Err(SQLite3PoolError::Cancelled)) | None => return,
        Some(Err(e)) => {
            warn!(
                error = ?e,
                "Metrics collector: failed to list projects for the vector-count probe; \
                 keeping the previous counts for this tick."
            );
            return;
        }
    };

    let mut counted: Vec<(String, u64)> = Vec::with_capacity(projects.len());
    for guid in projects {
        match store
            .count_points(&collection_name(&guid, spec.collection_slug))
            .await
        {
            Ok(Some(n)) => counted.push((guid, n)),
            // The store declines to answer (every test fake takes the trait's provided
            // impl, by design). Not an error, and not a zero — but also not a reason to
            // abandon the walk: `SELECT guid FROM projects` has no ORDER BY, so a
            // `return` here would make every other project's count depend on which
            // project happened to be created first.
            Ok(None) => continue,
            Err(e) => warn!(
                error = %e,
                project_guid = %guid,
                "Metrics collector: could not count this project's vectors; it will be \
                 absent from mindex_project_vectors this tick rather than reported as \
                 zero. Sysadmin: check Qdrant is reachable."
            ),
        }
    }

    // Cleared and repopulated whole, like every other family here, so a deleted
    // project stops reporting its last known count. Nothing awaits between the two.
    metrics.state.project_vectors.clear();
    for (project_guid, n) in counted {
        metrics
            .state
            .project_vectors
            .get_or_create(&ProjectLabels { project_guid })
            .set(n as i64);
    }
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

    use async_trait::async_trait;
    use std::collections::HashMap;

    use crate::backend::v0::models::UUIDv4;
    use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStoreError};

    const MODEL: &str = "qwen3-embedding-0.6b";

    /// A store that answers the vector-count probe from a script keyed by collection
    /// name: `Some(Ok(n))` counts, `Some(Err)` fails, and a name absent from the map
    /// declines with `Ok(None)` exactly as the trait's provided default does.
    struct CountingStore {
        counts: HashMap<String, Result<u64, &'static str>>,
        /// When set, every collection declines — the shape every other test fake has.
        declines: bool,
    }

    impl CountingStore {
        fn with(pairs: &[(&str, Result<u64, &'static str>)]) -> Self {
            Self {
                counts: pairs
                    .iter()
                    .map(|(g, r)| {
                        (
                            collection_name(
                                g,
                                model_by_id("qwen3-embedding-0.6b")
                                    .expect("registered")
                                    .collection_slug,
                            ),
                            *r,
                        )
                    })
                    .collect(),
                declines: false,
            }
        }
    }

    #[async_trait]
    impl VectorStore for CountingStore {
        async fn count_points(&self, collection: &str) -> Result<Option<u64>, VectorStoreError> {
            if self.declines {
                return Ok(None);
            }
            match self.counts.get(collection) {
                Some(Ok(n)) => Ok(Some(*n)),
                Some(Err(e)) => Err(VectorStoreError((*e).to_string())),
                None => Ok(None),
            }
        }

        async fn ensure_project(&self, _collection: &str) -> Result<(), VectorStoreError> {
            unreachable!("the vector-count probe creates nothing")
        }
        async fn delete_collection(&self, _collection: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn health(&self) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn insert_batch(
            &self,
            _collection: &str,
            _chunks: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_batch(
            &self,
            _collection: &str,
            _guids: Vec<String>,
        ) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn search(
            &self,
            _collection: &str,
            _chunk_ids: Vec<UUIDv4>,
            _dense: Vec<f32>,
            _top_k: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            unreachable!()
        }
    }

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
                "INSERT INTO projects (guid) VALUES (?1)",
                rusqlite::params![guid],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, path, sha256, programming_language, status)
                 VALUES (?1, 'src/a.rs', ?2, 'rust', 'indexing')",
                rusqlite::params![guid, "a".repeat(64)],
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

    /// The whole point of the probe: SQLite says the project holds chunks, Qdrant
    /// says it holds no points. That divergence is the only detector for a lost
    /// Qdrant volume — `ensure_collection` silently remakes an empty collection,
    /// every file still reads `indexed`, and search answers 404 for ever.
    #[tokio::test]
    async fn a_project_whose_vectors_are_gone_reports_zero_against_its_chunk_count() {
        let a = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let pool = pool().await;
        seed_project(&pool, a).await;

        let store: Arc<dyn VectorStore> = Arc::new(CountingStore::with(&[(a, Ok(0))]));
        let metrics = Metrics::new();
        let token = CancellationToken::new();

        probe_vector_counts(&store, &pool, "qwen3-embedding-0.6b", &metrics, &token).await;

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(&format!(
                r#"mindex_project_vectors{{project_guid="{a}"}} 0"#
            )),
            "a project the store answered zero for must say zero: {text}"
        );
    }

    /// A store that cannot answer for a project must leave that project **absent**,
    /// never zero — zero is the alarming value here, and an unreachable Qdrant
    /// manufacturing it would page somebody about a healthy index. The projects the
    /// same tick *could* count must still be reported.
    #[tokio::test]
    async fn a_project_the_store_cannot_count_is_absent_never_zero() {
        let good = "11111111111111111111111111111111";
        let bad = "22222222222222222222222222222222";
        let pool = pool().await;
        seed_project(&pool, good).await;
        seed_project(&pool, bad).await;

        let store: Arc<dyn VectorStore> = Arc::new(CountingStore::with(&[
            (good, Ok(7)),
            (bad, Err("qdrant down")),
        ]));
        let metrics = Metrics::new();

        probe_vector_counts(
            &store,
            &pool,
            "qwen3-embedding-0.6b",
            &metrics,
            &CancellationToken::new(),
        )
        .await;

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(&format!(
                r#"mindex_project_vectors{{project_guid="{good}"}} 7"#
            )),
            "one project failing must not cost the others their counts: {text}"
        );
        assert!(
            !text.contains(&format!(
                r#"mindex_project_vectors{{project_guid="{bad}"}}"#
            )),
            "an uncountable project was given a number anyway: {text}"
        );
    }

    /// The clear-and-repopulate rule applies to this family too: a `Family` never
    /// forgets a label set, so a deleted project would keep reporting the vector
    /// count it had on the tick before it was dropped.
    #[tokio::test]
    async fn a_deleted_project_stops_reporting_its_vector_count() {
        let a = "33333333333333333333333333333333";
        let b = "44444444444444444444444444444444";
        let pool = pool().await;
        seed_project(&pool, a).await;
        seed_project(&pool, b).await;

        let store: Arc<dyn VectorStore> =
            Arc::new(CountingStore::with(&[(a, Ok(11)), (b, Ok(22))]));
        let metrics = Metrics::new();
        let token = CancellationToken::new();

        probe_vector_counts(&store, &pool, "qwen3-embedding-0.6b", &metrics, &token).await;
        assert!(metrics.render().expect("renders").contains(&format!(
            r#"mindex_project_vectors{{project_guid="{b}"}} 22"#
        )));

        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute("DELETE FROM project_files WHERE project_guid = ?1", [b])?;
            tx.execute("DELETE FROM projects WHERE guid = ?1", [b])?;
            Ok(())
        })
        .await
        .expect("delete");

        probe_vector_counts(&store, &pool, "qwen3-embedding-0.6b", &metrics, &token).await;

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(&format!(
                r#"mindex_project_vectors{{project_guid="{a}"}} 11"#
            )),
            "{text}"
        );
        assert!(
            !text.contains(&format!(r#"mindex_project_vectors{{project_guid="{b}"}}"#)),
            "a deleted project is still reporting its last vector count: {text}"
        );
    }

    /// Every other `VectorStore` in the tree — and every test fake — takes the
    /// trait's provided `count_points`, which declines. Declining must publish
    /// nothing at all: a store that cannot count is not a store reporting zeros.
    #[tokio::test]
    async fn a_store_that_declines_to_count_publishes_no_gauge() {
        let a = "55555555555555555555555555555555";
        let pool = pool().await;
        seed_project(&pool, a).await;

        let store: Arc<dyn VectorStore> = Arc::new(CountingStore {
            counts: HashMap::new(),
            declines: true,
        });
        let metrics = Metrics::new();

        probe_vector_counts(
            &store,
            &pool,
            "qwen3-embedding-0.6b",
            &metrics,
            &CancellationToken::new(),
        )
        .await;

        assert!(
            !metrics
                .render()
                .expect("renders")
                .contains("mindex_project_vectors{"),
            "a declining store manufactured a gauge"
        );
    }

    /// One project the store declines to answer for must not cost every *other*
    /// project its count. The probe walks projects in whatever order SQLite returns
    /// them, so a decline that abandoned the walk would make the whole family's
    /// contents depend on insertion order — a metric that is right or wrong
    /// depending on which project was created first.
    #[tokio::test]
    async fn one_declined_project_does_not_abandon_the_rest_of_the_walk() {
        let declined = "88888888888888888888888888888888";
        let counted = "99999999999999999999999999999999";
        let pool = pool().await;
        // Seeded first, so it is the first row `SELECT guid FROM projects` returns.
        seed_project(&pool, declined).await;
        seed_project(&pool, counted).await;

        // `declined` is absent from the map, so the store answers `Ok(None)` for it.
        let store: Arc<dyn VectorStore> = Arc::new(CountingStore::with(&[(counted, Ok(5))]));
        let metrics = Metrics::new();

        probe_vector_counts(
            &store,
            &pool,
            "qwen3-embedding-0.6b",
            &metrics,
            &CancellationToken::new(),
        )
        .await;

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(&format!(
                r#"mindex_project_vectors{{project_guid="{counted}"}} 5"#
            )),
            "a decline on an earlier project swallowed a later project's count: {text}"
        );
        assert!(
            !text.contains(&format!(
                r#"mindex_project_vectors{{project_guid="{declined}"}}"#
            )),
            "the declined project was given a number: {text}"
        );
    }

    /// A cancelled tick must not clear the family it cannot refill — the same rule
    /// `collect_once` follows, in the one place that reaches Qdrant.
    #[tokio::test]
    async fn a_cancelled_probe_leaves_the_previous_vector_counts_alone() {
        let a = "66666666666666666666666666666666";
        let pool = pool().await;
        seed_project(&pool, a).await;

        let store: Arc<dyn VectorStore> = Arc::new(CountingStore::with(&[(a, Ok(9))]));
        let metrics = Metrics::new();

        probe_vector_counts(
            &store,
            &pool,
            "qwen3-embedding-0.6b",
            &metrics,
            &CancellationToken::new(),
        )
        .await;
        assert!(metrics.render().expect("renders").contains(&format!(
            r#"mindex_project_vectors{{project_guid="{a}"}} 9"#
        )));

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        probe_vector_counts(&store, &pool, "qwen3-embedding-0.6b", &metrics, &cancelled).await;

        assert!(
            metrics.render().expect("renders").contains(&format!(
                r#"mindex_project_vectors{{project_guid="{a}"}} 9"#
            )),
            "a cancelled probe wiped the counts it could not refresh"
        );
    }

    /// `state_refreshed_timestamp_seconds` dates the last *successful* snapshot. A
    /// failed read deliberately keeps the previous gauges, which was
    /// indistinguishable from a healthy tick — so the timestamp must not advance
    /// when nothing was read.
    #[tokio::test]
    async fn the_refresh_timestamp_advances_only_on_a_tick_that_read_something() {
        let a = "77777777777777777777777777777777";
        let pool = pool().await;
        seed_project(&pool, a).await;

        let metrics = Metrics::new();
        assert_eq!(
            metrics.state.state_refreshed_at.get(),
            0,
            "nothing has been collected yet"
        );

        collect_once(&pool, &metrics, &tuning(), &CancellationToken::new()).await;
        let stamped = metrics.state.state_refreshed_at.get();
        assert!(stamped > 0, "a successful tick did not date itself");

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        collect_once(&pool, &metrics, &tuning(), &cancelled).await;

        assert_eq!(
            metrics.state.state_refreshed_at.get(),
            stamped,
            "a tick that read nothing dated the gauges as fresh"
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
