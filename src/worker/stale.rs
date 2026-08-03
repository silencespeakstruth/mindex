//! Says out loud when a project's vectors are not where the code is looking.
//!
//! `COLLECTION_SCHEMA_VERSION` is a component of every collection's *name*, so
//! bumping it does not migrate anything and does not fail anything. The new name
//! simply names no collection: `ensure_project` makes an empty one, SQLite goes on
//! reporting every file `indexed` (the prepare-phase hash skip never looks at the
//! collection layout), and search answers `404 search.no_match` for ever. There is no
//! error, no failed health check and no unusual log line — the service is, from every
//! angle it can see itself, working.
//!
//! That shape is what this worker exists for, and it is not hypothetical: the same
//! symptom arrives from a lost Qdrant volume, and it arrived once from a bump nobody
//! followed with a reindex. `mindex_project_vectors` can see it too, but only under
//! `[metrics].probe_dependencies` and only by comparing two families on a dashboard —
//! which is a thing an operator does *after* suspecting a problem, not a thing that
//! tells them to.
//!
//! Two questions, deliberately separate:
//!
//! - **Stale** — a project holds active chunks in SQLite, but its current-version
//!   collection is missing or empty. Its search is broken right now. Remedy: reindex.
//! - **Orphaned** — a collection exists at some *previous* version. Nothing is broken,
//!   but nothing can reach it either, and it holds the whole pre-bump index. SQLite
//!   records no layout, so this is the only thing in the system that can see it.
//!   Remedy: drop it, by hand, once the reindex is verified.
//!
//! The second is deliberately not automated. Dropping the previous version is what
//! makes a rollback impossible, and that decision belongs to whoever can see whether
//! the new index is good.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::backend::metrics::Metrics;
use crate::db::qdrant::{CollectionAge, VectorStore, classify_collection, collection_name};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
use tokio_util::future::FutureExt;

/// How often the check repeats. Hourly, like the other slow surveys: the conditions it
/// reports are both permanent until an operator acts, so a faster tick would only
/// repeat the same warning sooner.
const CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// What one pass found. Returned rather than only logged so the caller can publish it
/// and the test can assert on it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StaleReport {
    /// Projects with active chunks whose current-version collection is missing or empty.
    pub stale: Vec<String>,
    /// Collection names present at a previous schema version, or `None` when the store
    /// could not be asked.
    ///
    /// `None` rather than an empty vec, because the two must not publish the same
    /// gauge: an empty listing is the all-clear, and "I could not ask" is not.
    pub orphaned: Option<Vec<String>>,
}

/// One pass. Split from the loop so it is testable without a clock — the
/// `gc::collect` / `collect_once` precedent.
///
/// Returns `None` when the pass could not be completed, which is **not** the same as
/// finding nothing: the gauges are left untouched in that case, because `0` is the
/// healthy reading here and an unreachable Qdrant must not be able to publish the
/// all-clear.
pub(crate) async fn check_once(
    db_pool: &SQLite3Pool,
    store: &Arc<dyn VectorStore>,
    token: &CancellationToken,
) -> Option<StaleReport> {
    // Projects that believe they have something to search. A project with no active
    // chunks has nothing to be stale about — a freshly created one, or one whose files
    // all sliced to zero chunks, is not a defect and must not warn.
    let projects: Vec<String> = match db_pool
        .transaction(token.clone(), |tx| {
            tx.prepare(
                "SELECT DISTINCT project_guid FROM project_file_chunks WHERE status = 'active'",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(token)
        .await
    {
        Some(Ok(p)) => p,
        Some(Err(SQLite3PoolError::Cancelled)) | None => return None,
        Some(Err(e)) => {
            warn!(
                error = ?e,
                "Collection check: could not list projects with active chunks, so this \
                 pass says nothing. Sysadmin: check the DB file is readable and not \
                 locked by another process."
            );
            return None;
        }
    };

    let mut report = StaleReport::default();

    for guid in projects {
        if token.is_cancelled() {
            return None;
        }
        let collection = collection_name(&guid);
        match store.count_points(&collection).await {
            // The real store answers `Some(0)` for a missing collection too, which is
            // exactly right here: "absent" and "present but empty" are the same defect
            // and have the same remedy.
            Ok(Some(0)) => report.stale.push(guid),
            Ok(Some(_)) => {}
            // The store declines to answer (every test fake takes the trait's provided
            // impl). Not a finding, and not a reason to abandon the walk.
            Ok(None) => continue,
            Err(e) => {
                warn!(
                    error = %e,
                    project_guid = %guid,
                    "Collection check: could not count this project's vectors, so this \
                     pass cannot say whether its index is intact. Sysadmin: check Qdrant \
                     is reachable."
                );
                // One unreachable project makes the whole pass inconclusive. Reporting
                // the rest would publish a `stale` count that silently excluded the
                // project most likely to be the problem.
                return None;
            }
        }
    }

    // The orphan half is answered separately and may decline on its own: the stale
    // half is the one that says search is broken, and it must still be published when
    // the store can count but not enumerate.
    report.orphaned = match store.list_collections().await {
        Ok(Some(names)) => Some(
            names
                .into_iter()
                .filter(|n| classify_collection(n) == CollectionAge::Previous)
                .collect(),
        ),
        Ok(None) => None,
        Err(e) => {
            warn!(
                error = %e,
                "Collection check: could not list collections, so this pass cannot say \
                 whether a previous schema version is still holding disk. Sysadmin: \
                 check Qdrant is reachable."
            );
            None
        }
    };

    Some(report)
}

/// Run one pass and publish it. Used at startup and on every tick.
pub(crate) async fn check_and_publish(
    db_pool: &SQLite3Pool,
    store: &Arc<dyn VectorStore>,
    metrics: &Metrics,
    token: &CancellationToken,
) {
    let Some(report) = check_once(db_pool, store, token).await else {
        return;
    };

    if !report.stale.is_empty() {
        warn!(
            count = report.stale.len(),
            projects = %report.stale.join(", "),
            "These projects hold indexed chunks but their Qdrant collection is missing or \
             empty, so every search against them returns no match and nothing else reports \
             an error. Sysadmin: this is what a lost Qdrant volume looks like, and what a \
             COLLECTION_SCHEMA_VERSION bump looks like before the reindex that must follow \
             it. Fix: run `mindex-index --root <project> --force` for each."
        );
    }

    metrics.collections.stale.set(report.stale.len() as i64);

    // Only a listing that was actually answered moves its gauge; `None` leaves the
    // previous value (or the `-1` seed) standing, because `0` here is the all-clear
    // and an unreachable Qdrant must not be able to publish it.
    if let Some(orphaned) = report.orphaned {
        if !orphaned.is_empty() {
            warn!(
                count = orphaned.len(),
                collections = %orphaned.join(", "),
                "Qdrant holds collections from a previous mindex collection-schema version. \
                 Nothing reads them and nothing will; they are the whole pre-bump index, \
                 still occupying disk. Sysadmin: once the reindexed collections are \
                 verified, drop each with `curl -X DELETE <qdrant>/collections/<name>`. \
                 Deliberately not automatic — dropping them is what makes a rollback \
                 impossible."
            );
        }
        metrics.collections.orphaned.set(orphaned.len() as i64);
    }
}

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    store: Arc<dyn VectorStore>,
    metrics: Arc<Metrics>,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick of a tokio interval fires immediately; startup already ran a pass,
    // so skip it rather than repeating the same warnings a millisecond apart.
    interval.tick().await;

    info!(
        interval_secs = CHECK_INTERVAL.as_secs(),
        "Collection check: started."
    );

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Collection check: shutting down.");
                break;
            }
        }
        check_and_publish(&db_pool, &store, &metrics, &token).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::v0::models::UUIDv4;
    use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStoreError};
    use async_trait::async_trait;
    use std::collections::HashMap;

    /// A store that answers both provided methods from a fixed map, so a pass can be
    /// driven over any combination of present / empty / absent collections.
    struct Store {
        counts: HashMap<String, u64>,
        listing: Option<Vec<String>>,
        count_fails: bool,
    }

    impl Store {
        fn shared(counts: &[(&str, u64)], listing: Option<&[&str]>) -> Arc<dyn VectorStore> {
            Arc::new(Store {
                counts: counts.iter().map(|(k, v)| ((*k).into(), *v)).collect(),
                listing: listing.map(|l| l.iter().map(|s| (*s).into()).collect()),
                count_fails: false,
            })
        }
    }

    #[async_trait]
    impl VectorStore for Store {
        async fn ensure_project(&self, _: &str) -> Result<(), VectorStoreError> {
            Ok(())
        }
        async fn insert_batch(
            &self,
            _: &str,
            _: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            Ok(())
        }
        async fn delete_batch(&self, _: &str, _: Vec<String>) -> Result<(), VectorStoreError> {
            Ok(())
        }
        async fn delete_collection(&self, _: &str) -> Result<(), VectorStoreError> {
            Ok(())
        }
        async fn health(&self) -> Result<(), VectorStoreError> {
            Ok(())
        }
        async fn count_points(&self, c: &str) -> Result<Option<u64>, VectorStoreError> {
            if self.count_fails {
                return Err(VectorStoreError("qdrant is down".into()));
            }
            // Absent from the map means absent from the store, which the real impl
            // answers as `Some(0)`.
            Ok(Some(self.counts.get(c).copied().unwrap_or(0)))
        }
        async fn list_collections(&self) -> Result<Option<Vec<String>>, VectorStoreError> {
            Ok(self.listing.clone())
        }
        async fn search(
            &self,
            _: &str,
            _: Vec<UUIDv4>,
            _: Vec<f32>,
            _: Vec<u32>,
            _: Vec<f32>,
            _: Vec<Vec<f32>>,
            _: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            Ok(vec![])
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A migrated in-memory pool holding one project with `active_chunks` active
    /// chunks. One connection, because each `:memory:` connection is its own database.
    async fn pool_with(project: &str, active_chunks: usize) -> SQLite3Pool {
        let pool = SQLite3Pool::new(std::path::Path::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, migration) in crate::MIGRATIONS {
                tx.execute_batch(migration)?;
            }
            Ok(())
        })
        .await
        .expect("migrated");
        let project = project.to_string();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, 'BAAI/bge-m3')",
                [&project],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, programming_language, sha256, status)
                 VALUES (?1, 'BAAI/bge-m3', 'a.rs', 'rust', ?2, 'indexing')",
                rusqlite::params![&project, "0".repeat(64)],
            )?;
            for i in 0..active_chunks {
                tx.execute(
                    "INSERT INTO project_file_chunks
                         (project_guid, file_path, model_id, code, qdrant_guid,
                          start_line, end_line, start_column, end_column, status)
                     VALUES (?1, 'a.rs', 'BAAI/bge-m3', 'x', ?2, 1, 2, 0, 0, 'active')",
                    rusqlite::params![&project, format!("{i:032x}")],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seeded");
        pool
    }

    /// The condition the whole worker exists for: SQLite says indexed, Qdrant has
    /// nothing, and every other signal in the system reads healthy.
    #[tokio::test]
    async fn a_project_whose_collection_is_empty_is_stale() {
        let pool = pool_with(A, 3).await;
        let store = Store::shared(&[], None);

        let report = check_once(&pool, &store, &CancellationToken::new())
            .await
            .expect("the pass completed");

        assert_eq!(report.stale, vec![A.to_string()]);
    }

    /// The healthy case must be silent, or the warning is noise and stops being read.
    #[tokio::test]
    async fn a_populated_collection_is_not_reported() {
        let pool = pool_with(A, 3).await;
        let store = Store::shared(&[(&collection_name(A), 3)], None);

        let report = check_once(&pool, &store, &CancellationToken::new())
            .await
            .expect("the pass completed");

        assert!(report.stale.is_empty(), "{report:?}");
    }

    /// A project with nothing indexed has nothing to be stale about. Reporting it
    /// would make every fresh install start life with a warning about a project that
    /// is behaving correctly.
    #[tokio::test]
    async fn a_project_with_no_active_chunks_is_not_stale() {
        let pool = pool_with(A, 0).await;
        let store = Store::shared(&[], None);

        let report = check_once(&pool, &store, &CancellationToken::new())
            .await
            .expect("the pass completed");

        assert!(report.stale.is_empty(), "{report:?}");
    }

    /// The orphan half: a previous version's collection is invisible to SQLite, so
    /// this listing is the only thing that can name the disk it is holding.
    #[tokio::test]
    async fn a_previous_versions_collection_is_reported_as_orphaned() {
        let pool = pool_with(A, 1).await;
        let store = Store::shared(
            &[(&collection_name(A), 1)],
            Some(&[
                &collection_name(A),
                &format!("{A}_v1"),
                &format!("{B}_v1"),
                // Qdrant may be shared. Somebody else's collection must never be named
                // in a message telling an operator what to delete.
                "someone_elses_data",
            ]),
        );

        let report = check_once(&pool, &store, &CancellationToken::new())
            .await
            .expect("the pass completed");

        assert!(report.stale.is_empty(), "{report:?}");
        assert_eq!(
            report.orphaned,
            Some(vec![format!("{A}_v1"), format!("{B}_v1")]),
            "the current version was named, or a foreign collection was"
        );
    }

    /// `0` is the healthy reading, so a pass that could not run must produce no
    /// reading at all. An unreachable Qdrant publishing the all-clear is the precise
    /// failure the `-1` seed and this `None` exist to prevent.
    #[tokio::test]
    async fn an_unreachable_store_yields_no_verdict_rather_than_a_clean_one() {
        let pool = pool_with(A, 1).await;
        let store: Arc<dyn VectorStore> = Arc::new(Store {
            counts: HashMap::new(),
            listing: None,
            count_fails: true,
        });

        assert!(
            check_once(&pool, &store, &CancellationToken::new())
                .await
                .is_none(),
            "an unreachable Qdrant produced a verdict; a failed check must not be \
             able to spell the healthy answer"
        );
    }

    /// The gauges must stay at their "not checked yet" seed when a pass declines,
    /// rather than being set to the healthy zero.
    #[tokio::test]
    async fn a_declined_pass_leaves_the_gauges_untouched() {
        let pool = pool_with(A, 1).await;
        let store: Arc<dyn VectorStore> = Arc::new(Store {
            counts: HashMap::new(),
            listing: None,
            count_fails: true,
        });
        let metrics = Metrics::new();

        check_and_publish(&pool, &store, &metrics, &CancellationToken::new()).await;

        let text = metrics.render().expect("renders");
        assert!(
            text.contains("mindex_stale_collections -1"),
            "the gauge left its not-checked-yet seed on a failed pass: {text}"
        );
    }
}
