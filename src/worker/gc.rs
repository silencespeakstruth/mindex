use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::backend::metrics::{Metrics, TriggerOutcomeLabels};
use crate::db::qdrant::{VectorStore, collection_name};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};

/// Seconds in a day — used to turn the configured retention (in days) into the
/// `unixepoch()` arithmetic the status-log prune does.
pub(crate) const SECONDS_PER_DAY: i64 = 24 * 3600;

/// Process-wide GC mutual exclusion. GC is global (not per-project), so a single
/// flag serializes the whole pass. Mirrors `IndexClaim` but with one slot, so a
/// plain `AtomicBool` suffices instead of a keyed set. Shared by the HTTP handler
/// (`POST /gc`) and the hourly worker so a manual sweep and a tick never race —
/// the loser of the race rejects (handler → 409) or skips its tick (worker).
pub(crate) struct GcGuard(Arc<AtomicBool>);

impl GcGuard {
    /// `Some(guard)` if no GC was running, `None` if one already holds the flag.
    pub(crate) fn try_acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then(|| GcGuard(Arc::clone(flag)))
    }
}

impl Drop for GcGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One phase's result: rows removed, and whether the phase ran to completion.
///
/// Every phase used to return a bare `usize` with each error mapped to `0`, so
/// "nothing needed pruning" and "pruning failed" were the same answer — `POST /gc`
/// answered 200 with zeros either way, and `gc_runs{outcome="ok"}` was incremented
/// even when all four phases had failed. A GC broken for days looked idle.
///
/// A phase that fails part-way carries both a non-zero `removed` and `failed`.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Phase {
    pub removed: usize,
    pub failed: bool,
}

impl Phase {
    fn done(removed: usize) -> Self {
        Self {
            removed,
            failed: false,
        }
    }
    fn broke(removed: usize) -> Self {
        Self {
            removed,
            failed: true,
        }
    }
}

/// What one pass did, per phase, so a caller can tell a clean idle sweep from a
/// broken one.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct GcOutcome {
    pub chunks: Phase,
    pub files: Phase,
    pub status_log: Phase,
    pub research: Phase,
}

impl GcOutcome {
    /// The phases that did not finish, by name — what `POST /gc` reports and what the
    /// `outcome="error"` label is decided from.
    pub(crate) fn failed_phases(&self) -> Vec<&'static str> {
        [
            ("chunks", self.chunks.failed),
            ("files", self.files.failed),
            ("status_log", self.status_log.failed),
            ("research", self.research.failed),
        ]
        .into_iter()
        .filter_map(|(name, failed)| failed.then_some(name))
        .collect()
    }
}

/// One full GC pass: hard-delete confirmed-removed chunks, then drop now-empty
/// `deleted` file rows, prune the old status log, then reap expired research runs.
/// The step order is the invariant (chunks before files, since the chunk→file FK is
/// RESTRICT), so it lives in one place shared by the worker and `POST /gc`. Returns
/// a [`GcOutcome`] — per phase, what it removed *and* whether it finished.
/// Callers serialize this behind [`GcGuard`].
///
/// The research step is last only because it is newest — it shares no foreign key
/// with the ordering above. It lives *inside* `collect` rather than in a worker of
/// its own so it inherits `GcGuard` serialization and the `POST /gc` 409 for free.
///
/// `trigger` is `"worker"` or `"manual"` — recorded here rather than at the two
/// call sites so a pass can never be counted twice or not at all. Qdrant delete
/// failures need no counter of their own: they arrive on
/// `qdrant_ops_total{op="delete_batch",outcome="error"}` via the store decorator.
pub(crate) async fn collect(
    db_pool: &SQLite3Pool,
    store: &dyn VectorStore,
    status_log_retention_days: u64,
    metrics: &Metrics,
    trigger: &'static str,
    token: &CancellationToken,
) -> GcOutcome {
    let started = std::time::Instant::now();
    metrics.gc.running.set(1);

    let out = GcOutcome {
        chunks: sweep(db_pool, store, token).await,
        files: prune_deleted_files(db_pool, token).await,
        status_log: prune_status_log(db_pool, status_log_retention_days, token).await,
        research: prune_expired_research(db_pool, token).await,
    };

    let g = &metrics.gc;
    g.duration.observe(started.elapsed().as_secs_f64());
    g.chunks_removed.inc_by(out.chunks.removed as u64);
    g.files_pruned.inc_by(out.files.removed as u64);
    g.status_log_pruned.inc_by(out.status_log.removed as u64);
    g.research_pruned.inc_by(out.research.removed as u64);

    // Severity order, and `error` outranks `cancelled`: a shutdown that interrupts a
    // pass is routine, a phase that could not run is not, and the label had neither —
    // `ok` was recorded whenever the token was live, however many phases had failed.
    let failed = out.failed_phases();
    let outcome = if failed.is_empty() {
        // A pass that ran to completion under a cancelled token did partial work; say
        // so rather than calling it a clean sweep.
        if token.is_cancelled() {
            "cancelled"
        } else {
            "ok"
        }
    } else {
        error!(
            failed_phases = ?failed,
            trigger,
            "GC pass did not complete; the backlog it would have cleared is still there. \
             Sysadmin: the phase errors are logged above — check the database is \
             writable and Qdrant is reachable."
        );
        "error"
    };
    g.runs
        .get_or_create(&TriggerOutcomeLabels { trigger, outcome })
        .inc();
    g.running.set(0);

    out
}

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    store: Arc<dyn VectorStore>,
    gc_flag: Arc<AtomicBool>,
    gc_interval_seconds: u64,
    status_log_retention_days: u64,
    metrics: Arc<Metrics>,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(gc_interval_seconds.max(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("GC worker: shutting down.");
                break;
            }
        }

        // A manual `POST /gc` may be mid-pass; skip this tick rather than race it
        // (the next tick is an hour away, well within the deleted-row backlog's
        // tolerance). The guard frees the flag at the end of the iteration.
        let Some(_guard) = GcGuard::try_acquire(&gc_flag) else {
            info!("GC worker: a manual GC pass is in progress, skipping this tick.");
            continue;
        };

        info!("GC worker: starting sweep.");
        let out = collect(
            &db_pool,
            &*store,
            status_log_retention_days,
            &metrics,
            "worker",
            &token,
        )
        .await;
        let failed = out.failed_phases();
        // "Sweep complete" was logged unconditionally, including for a pass in which
        // every phase had errored. `collect` already logs the failure; this keeps the
        // routine line from claiming the opposite of it.
        if failed.is_empty() {
            info!(
                chunks_removed = out.chunks.removed,
                files_removed = out.files.removed,
                research_runs_pruned = out.research.removed,
                "GC worker: sweep complete."
            );
        } else {
            warn!(
                chunks_removed = out.chunks.removed,
                files_removed = out.files.removed,
                research_runs_pruned = out.research.removed,
                failed_phases = ?failed,
                "GC worker: sweep ended early; some phases did not run."
            );
        }
    }
}

/// Removes `project_files` rows that were marked `status='deleted'` (by
/// `DELETE /files`) once they have no chunk rows left — i.e. after [`sweep`] has
/// hard-deleted their (soft-deleted) chunks. The FK to chunks is RESTRICT, so this
/// can only fire after the chunks are gone; running it after `sweep` in the same
/// pass is what makes a delete eventually physical. Returns the rows removed.
pub(crate) async fn prune_deleted_files(db_pool: &SQLite3Pool, token: &CancellationToken) -> Phase {
    let removed = db_pool
        .transaction(token.clone(), move |tx| {
            let n = tx.execute(
                "DELETE FROM project_files
                 WHERE status = 'deleted'
                   AND NOT EXISTS (
                       SELECT 1 FROM project_file_chunks c
                       WHERE c.project_guid = project_files.project_guid
                         AND c.model_id     = project_files.model_id
                         AND c.file_path    = project_files.path
                   )",
                [],
            )?;
            Ok(n)
        })
        .await;

    match removed {
        Ok(n) => Phase::done(n),
        // Shutdown, not breakage: the next pass does the work.
        Err(SQLite3PoolError::Cancelled) => Phase::done(0),
        Err(e) => {
            error!(
                error = ?e,
                "GC worker: failed to prune deleted file rows. Sysadmin: check the \
                 database file is writable — until this succeeds, emptied file rows \
                 accumulate."
            );
            Phase::broke(0)
        }
    }
}

/// Deletes `project_file_status_log` rows older than `retention_days` (from
/// `[workers].status_log_retention_days`). A single `DELETE` (SQLite has no
/// `DELETE ... LIMIT` in the bundled build); the audit log is small relative to the
/// chunk tables, so one statement is fine.
pub(crate) async fn prune_status_log(
    db_pool: &SQLite3Pool,
    retention_days: u64,
    token: &CancellationToken,
) -> Phase {
    let max_age_secs = retention_days as i64 * SECONDS_PER_DAY;

    let pruned = db_pool
        .transaction(token.clone(), move |tx| {
            let n = tx.execute(
                "DELETE FROM project_file_status_log WHERE at < unixepoch() - ?1",
                rusqlite::params![max_age_secs],
            )?;
            Ok(n)
        })
        .await;

    match pruned {
        Ok(0) => Phase::done(0),
        Ok(rows) => {
            info!(
                rows,
                retention_days, "GC worker: pruned old status-log rows."
            );
            Phase::done(rows)
        }
        Err(SQLite3PoolError::Cancelled) => Phase::done(0),
        Err(e) => {
            error!(
                error = ?e,
                "GC worker: failed to prune the status log. Sysadmin: check the \
                 database file is writable — the log grows without bound until this \
                 succeeds."
            );
            Phase::broke(0)
        }
    }
}

/// Deletes stored research runs whose `expires_at` has passed, and their baseline
/// rows with them (`research_run_files` cascades).
///
/// **Takes no retention argument**, unlike [`prune_status_log`] beside it, and the
/// difference is the point: a run's deadline is stamped onto its row at insert from
/// `[research].retention_days`. So changing that setting moves future runs only, and
/// a run can opt out of it entirely — `expires_at IS NULL` means **pinned**, and the
/// predicate below can never reach one. Comparing against the *current* config here
/// would make pinning impossible to express and would silently re-date every stored
/// run the day an operator edited the config.
///
/// The partial index `idx_research_runs_expiry` covers exactly this predicate, so
/// pinned runs cost the sweep nothing at all.
pub(crate) async fn prune_expired_research(
    db_pool: &SQLite3Pool,
    token: &CancellationToken,
) -> Phase {
    let pruned = db_pool
        .transaction(token.clone(), move |tx| {
            let n = tx.execute(
                "DELETE FROM research_runs
                  WHERE expires_at IS NOT NULL AND expires_at < unixepoch()",
                [],
            )?;
            Ok(n)
        })
        .await;

    match pruned {
        Ok(0) => Phase::done(0),
        Ok(rows) => {
            info!(rows, "GC worker: reaped expired research runs.");
            Phase::done(rows)
        }
        Err(SQLite3PoolError::Cancelled) => Phase::done(0),
        Err(e) => {
            error!(
                error = ?e,
                "GC worker: failed to reap expired research runs. Sysadmin: check the \
                 database file is writable — expired reports are being retained past \
                 their retention window until this succeeds."
            );
            Phase::broke(0)
        }
    }
}

/// Hard-deletes soft-deleted chunks whose Qdrant vectors have been confirmed
/// removed. Returns the number of chunk rows deleted. Loops until no `deleted`
/// chunks remain (or every collection's Qdrant delete fails this pass).
pub(crate) async fn sweep(
    db_pool: &SQLite3Pool,
    store: &dyn VectorStore,
    token: &CancellationToken,
) -> Phase {
    let mut total_removed = 0usize;
    // Every `break` below that is not "the work is done" sets this. The loop
    // deliberately stops rather than spinning on a batch it cannot clear, but stopping
    // early and finishing are opposite states and the caller could not tell them apart.
    let mut failed = false;
    loop {
        if token.is_cancelled() {
            break;
        }

        let batch: Vec<(String, String)> = match db_pool
            .transaction(token.clone(), |tx| {
                tx.prepare(
                    "SELECT qdrant_guid, project_guid
                     FROM project_file_chunks
                     WHERE status = 'deleted'
                     LIMIT 256",
                )?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
            })
            .await
        {
            Ok(b) => b,
            Err(SQLite3PoolError::Cancelled) => break,
            Err(e) => {
                error!(
                    error = ?e,
                    "GC worker: failed to query deleted chunks from SQLite; aborting this \
                     sweep. Sysadmin: check the database file is readable and not locked."
                );
                failed = true;
                break;
            }
        };

        if batch.is_empty() {
            break;
        }

        // Group by project so we issue one delete call per collection.
        let mut by_project: HashMap<String, Vec<String>> = HashMap::new();
        for (guid, project) in &batch {
            by_project
                .entry(project.clone())
                .or_default()
                .push(guid.clone());
        }

        // Only hard-delete SQLite rows whose Qdrant vectors were actually removed.
        // If a collection's delete fails (transient Qdrant error), we keep its rows
        // marked 'deleted' so the next sweep retries them — otherwise the vectors would
        // be orphaned in Qdrant forever, with no SQLite row left to track them.
        let mut confirmed_deleted: Vec<String> = Vec::new();
        for (project_guid, guids) in &by_project {
            let coll = collection_name(project_guid);
            match store.delete_batch(&coll, guids.clone()).await {
                Ok(()) => confirmed_deleted.extend(guids.iter().cloned()),
                Err(e) => error!(
                    error = %e,
                    project_guid,
                    collection = %coll,
                    chunk_count = guids.len(),
                    "GC: Qdrant delete_batch failed; keeping rows for next sweep. \
                     Check Qdrant reachability and that the collection exists."
                ),
            }
        }

        if confirmed_deleted.is_empty() {
            // Nothing was confirmed removed from Qdrant this iteration (every collection
            // failed). Stop the inner loop to avoid spinning on the same un-deletable
            // batch; the next scheduled sweep will retry. The per-collection errors are
            // already logged above; what was missing is that the *pass* did not finish.
            failed = true;
            break;
        }

        // Hard-delete only the rows whose vectors are confirmed gone from Qdrant.
        let removed = db_pool
            .transaction(token.clone(), move |tx| {
                let placeholders = (1..=confirmed_deleted.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "DELETE FROM project_file_chunks
                     WHERE status = 'deleted' AND qdrant_guid IN ({placeholders})"
                );
                let n = tx.execute(&sql, rusqlite::params_from_iter(confirmed_deleted.iter()))?;
                Ok(n)
            })
            .await;

        match removed {
            Ok(n) => total_removed += n,
            Err(SQLite3PoolError::Cancelled) => break,
            Err(e) => {
                error!(
                    error = ?e,
                    "GC worker: failed to hard-delete swept chunk rows. Sysadmin: check \
                     the database file is writable."
                );
                // Vectors are already gone from Qdrant; the rows stay 'deleted' and a
                // later sweep retries the SQLite delete. Avoid spinning on this batch.
                failed = true;
                break;
            }
        }
    }
    if failed {
        Phase::broke(total_removed)
    } else {
        Phase::done(total_removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rusqlite::params;
    use std::collections::HashSet;
    use std::path::Path;
    use uuid::Uuid;

    use crate::backend::v0::models::UUIDv4;
    use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStoreError};

    /// `VectorStore` fake: `delete_batch` fails for any collection in `fail` and
    /// succeeds otherwise. The other methods are unreachable from `sweep`.
    struct FakeStore {
        fail: HashSet<String>,
    }

    #[async_trait]
    impl VectorStore for FakeStore {
        async fn delete_batch(
            &self,
            collection: &str,
            _guids: Vec<String>,
        ) -> Result<(), VectorStoreError> {
            if self.fail.contains(collection) {
                Err(VectorStoreError("forced failure".to_string()))
            } else {
                Ok(())
            }
        }

        async fn ensure_project(&self, _collection: &str) -> Result<(), VectorStoreError> {
            unreachable!("sweep does not call ensure_project")
        }
        async fn delete_collection(&self, _collection: &str) -> Result<(), VectorStoreError> {
            unreachable!("sweep does not call delete_collection")
        }
        async fn health(&self) -> Result<(), VectorStoreError> {
            unreachable!("sweep does not call health")
        }
        async fn insert_batch(
            &self,
            _collection: &str,
            _chunks: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            unreachable!("sweep does not call insert_batch")
        }
        async fn search(
            &self,
            _collection: &str,
            _chunk_ids: Vec<UUIDv4>,
            _dense: Vec<f32>,
            _sparse_indices: Vec<u32>,
            _sparse_values: Vec<f32>,
            _colbert: Vec<Vec<f32>>,
            _top_k: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            unreachable!("sweep does not call search")
        }
    }

    async fn migrated_pool() -> SQLite3Pool {
        let pool = SQLite3Pool::new(Path::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, migration) in crate::MIGRATIONS {
                tx.execute_batch(migration)?;
            }
            Ok(())
        })
        .await
        .unwrap();
        pool
    }

    /// Inserts a project + one file + `n` soft-deleted chunks. Returns nothing;
    /// the chunks are counted via `deleted_count`.
    async fn seed_deleted_chunks(pool: &SQLite3Pool, guid: &str, n: usize) {
        let g = guid.to_string();
        let qdrant_guids: Vec<String> = (0..n)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, 'BAAI/bge-m3')",
                params![g],
            )?;
            // 'indexing' is a legal entry status (the insert guard rejects terminal
            // states); GC only touches chunk rows, so the file's status is irrelevant.
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, 'BAAI/bge-m3', 'a.rs', ?2, 'rust', 'indexing')",
                params![g, "0".repeat(64)],
            )?;
            for qg in &qdrant_guids {
                tx.execute(
                    "INSERT INTO project_file_chunks
                         (project_guid, file_path, model_id, code, qdrant_guid,
                          start_line, end_line, start_column, end_column, status)
                     VALUES (?1, 'a.rs', 'BAAI/bge-m3', 'code', ?2, 1, 2, 0, 1, 'deleted')",
                    params![g, qg],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    async fn deleted_count(pool: &SQLite3Pool, guid: &str) -> i64 {
        let g = guid.to_string();
        pool.transaction(CancellationToken::new(), move |tx| {
            let n: i64 = tx.query_row(
                "SELECT COUNT(*) FROM project_file_chunks
                 WHERE project_guid = ?1 AND status = 'deleted'",
                params![g],
                |r| r.get(0),
            )?;
            Ok(n)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn sweep_removes_all_rows_when_qdrant_succeeds() {
        let pool = migrated_pool().await;
        let guid = "a".repeat(32);
        seed_deleted_chunks(&pool, &guid, 3).await;

        let store = FakeStore {
            fail: HashSet::new(),
        };
        sweep(&pool, &store, &CancellationToken::new()).await;

        assert_eq!(
            deleted_count(&pool, &guid).await,
            0,
            "all confirmed rows should be gone"
        );
    }

    #[tokio::test]
    async fn sweep_keeps_rows_whose_qdrant_delete_failed() {
        let pool = migrated_pool().await;
        let guid_ok = "a".repeat(32);
        let guid_fail = "b".repeat(32);
        seed_deleted_chunks(&pool, &guid_ok, 2).await;
        seed_deleted_chunks(&pool, &guid_fail, 2).await;

        // Fail only the second project's collection.
        let store = FakeStore {
            fail: HashSet::from([collection_name(&guid_fail)]),
        };
        sweep(&pool, &store, &CancellationToken::new()).await;

        // Confirmed-deleted project: rows gone. Failed project: rows kept for retry
        // (this is the orphan-prevention regression — old code deleted them anyway).
        assert_eq!(
            deleted_count(&pool, &guid_ok).await,
            0,
            "succeeded project should be swept"
        );
        assert_eq!(
            deleted_count(&pool, &guid_fail).await,
            2,
            "failed project's rows must remain"
        );
    }

    /// A sweep that could clear nothing stops rather than spinning — correct, and for
    /// its whole life indistinguishable from a sweep with nothing to do. Both returned
    /// `0`, `collect` counted `gc_runs{outcome="ok"}` either way, and `POST /gc`
    /// answered 200 with zeros. A GC that had been failing for days read as idle.
    #[tokio::test]
    async fn a_sweep_that_cleared_nothing_says_whether_it_could_not_or_need_not() {
        let guid = "c".repeat(32);

        // Nothing to do.
        let pool = migrated_pool().await;
        let idle = sweep(
            &pool,
            &FakeStore {
                fail: HashSet::new(),
            },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((idle.removed, idle.failed), (0, false));

        // Everything to do, and Qdrant refusing all of it.
        let pool = migrated_pool().await;
        seed_deleted_chunks(&pool, &guid, 2).await;
        let broken = sweep(
            &pool,
            &FakeStore {
                fail: HashSet::from([collection_name(&guid)]),
            },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            (broken.removed, broken.failed),
            (0, true),
            "same count as the idle sweep, opposite meaning"
        );

        let out = GcOutcome {
            chunks: broken,
            ..GcOutcome::default()
        };
        assert_eq!(out.failed_phases(), vec!["chunks"]);
    }

    #[tokio::test]
    async fn sweep_on_empty_is_a_noop() {
        let pool = migrated_pool().await;
        let store = FakeStore {
            fail: HashSet::new(),
        };
        // No deleted chunks at all: must return promptly without error.
        sweep(&pool, &store, &CancellationToken::new()).await;
    }

    #[test]
    fn gc_guard_serializes_and_releases() {
        let flag = Arc::new(AtomicBool::new(false));

        let guard = GcGuard::try_acquire(&flag).expect("free flag is acquirable");
        // A second acquire while the first is held must fail (serialization).
        assert!(
            GcGuard::try_acquire(&flag).is_none(),
            "held flag rejects a second guard"
        );

        drop(guard);
        // After the guard drops the flag is free again.
        assert!(
            GcGuard::try_acquire(&flag).is_some(),
            "dropped guard frees the flag"
        );
    }

    async fn status_log_count(pool: &SQLite3Pool) -> i64 {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.query_row("SELECT COUNT(*) FROM project_file_status_log", [], |r| {
                r.get(0)
            })
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    /// Seeds one run with the given expiry and one baseline row for it.
    async fn seed_run(pool: &SQLite3Pool, id: &str, expires_at: Option<i64>) {
        let id = id.to_string();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO research_runs (
                     id, project_guid, seq, expires_at,
                     question, model, prompt_version, effort,
                     granted_seconds, granted_tokens, granted_steps, granted_search_top_k,
                     done_reason, steps, turns, elapsed_ms,
                     prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx,
                     citations_total, citations_verified, citations_path_only,
                     citations_unverified, cited_paths_json, unverified_paths_json,
                     changed_files, removed_files, stale_citations, stale_paths_json,
                     notes_written, notes_rejected, plan_revisions, grep_calls, grep_hits,
                     out_of_scope_refusals, out_of_scope_rows, scoped,
                     forced_synthesis, report_window_ms, report_elapsed_ms, report
                 ) VALUES (
                     ?1, 'p', (SELECT COALESCE(MAX(seq), 0) + 1 FROM research_runs), ?2,
                     'q', 'm', '1.2', 'medium',
                     1, 1, 1, 1,
                     'finalized', 1, 1, 1,
                     1, 1, 1, 1,
                     0, 0, 0,
                     0, '[]', '[]',
                     0, 0, 0, '[]',
                     0, 0, 0, 0, 0,
                     0, 0, 0,
                     0, 0, 0, 'r'
                 )",
                rusqlite::params![id, expires_at],
            )?;
            tx.execute(
                "INSERT INTO research_run_files (run_id, path, sha256) VALUES (?1, 'a.rs', ?2)",
                rusqlite::params![id, "0".repeat(64)],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    async fn count(pool: &SQLite3Pool, sql: &'static str) -> i64 {
        pool.transaction(CancellationToken::new(), move |tx| {
            Ok(tx.query_row(sql, [], |r| r.get(0))?)
        })
        .await
        .unwrap()
    }

    /// The sweep must reap what has expired, leave what has not, and be structurally
    /// incapable of touching a **pinned** run — `expires_at IS NULL` is the whole
    /// mechanism by which a report worth keeping outlives the retention window, and a
    /// NULL that compared as "long ago" would silently delete exactly the runs
    /// somebody cared enough about to pin.
    #[tokio::test]
    async fn expired_research_is_reaped_but_pinned_research_is_never_touched() {
        let pool = migrated_pool().await;
        seed_run(&pool, "expired", Some(1)).await; // 1970 — long past
        seed_run(&pool, "pinned", None).await;
        seed_run(&pool, "future", Some(4_000_000_000)).await; // 2096

        let pruned = prune_expired_research(&pool, &CancellationToken::new()).await;
        assert_eq!(pruned.removed, 1, "exactly the expired run should go");
        assert!(!pruned.failed);

        let ids: i64 = count(
            &pool,
            "SELECT COUNT(*) FROM research_runs WHERE id IN ('pinned', 'future')",
        )
        .await;
        assert_eq!(ids, 2, "a pinned or future run must survive the sweep");
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM research_runs WHERE id = 'expired'"
            )
            .await,
            0
        );
        // The baselines go with the run: research_run_files cascades, so a reaped run
        // cannot leave rows behind that no longer join to anything.
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM research_run_files WHERE run_id = 'expired'"
            )
            .await,
            0,
            "the expired run's baselines should have cascaded away"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM research_run_files").await,
            2,
            "the surviving runs must keep theirs"
        );
    }

    #[tokio::test]
    async fn prune_expired_research_on_a_cancelled_token_does_nothing() {
        let pool = migrated_pool().await;
        seed_run(&pool, "expired", Some(1)).await;
        let token = CancellationToken::new();
        token.cancel();
        let phase = prune_expired_research(&pool, &token).await;
        assert_eq!(phase.removed, 0);
        assert!(
            !phase.failed,
            "a cancelled token is a shutdown, not a broken phase"
        );
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM research_runs").await, 1);
    }

    #[tokio::test]
    async fn prune_status_log_removes_only_expired_rows() {
        let pool = migrated_pool().await;

        // Two rows older than the 30-day retention, one fresh — inserted directly
        // with explicit `at` (the table has no insert guard).
        pool.transaction(CancellationToken::new(), |tx| {
            for age_days in [40_i64, 31, 1] {
                tx.execute(
                    "INSERT INTO project_file_status_log
                         (project_guid, model_id, path, old_status, new_status, retry_count, at)
                     VALUES ('p', 'BAAI/bge-m3', 'a.rs', NULL, 'indexing', 0, unixepoch() - ?1)",
                    params![age_days * 86_400],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(status_log_count(&pool).await, 3);

        prune_status_log(&pool, 30, &CancellationToken::new()).await;

        // The 40- and 31-day rows are gone; the 1-day row remains.
        assert_eq!(status_log_count(&pool).await, 1);
    }

    async fn file_count(pool: &SQLite3Pool, guid: &str) -> i64 {
        let g = guid.to_string();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM project_files WHERE project_guid = ?1",
                params![g],
                |r| r.get(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn prune_deleted_files_removes_only_emptied_deleted_files() {
        let pool = migrated_pool().await;
        let guid = "c".repeat(32);
        let g = guid.clone();
        let sha = "0".repeat(64);
        let qg = Uuid::new_v4().simple().to_string();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, 'BAAI/bge-m3')",
                params![g],
            )?;
            // (a) deleted file, no chunks → must be pruned.
            tx.execute(
                "INSERT INTO project_files (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, 'BAAI/bge-m3', 'gone.rs', ?2, 'rust', 'indexing')",
                params![g, sha],
            )?;
            tx.execute(
                "UPDATE project_files SET status='deleted' WHERE project_guid=?1 AND path='gone.rs'",
                params![g],
            )?;
            // (b) indexed file with an active chunk → must remain (not 'deleted').
            tx.execute(
                "INSERT INTO project_files (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, 'BAAI/bge-m3', 'keep.rs', ?2, 'rust', 'indexing')",
                params![g, sha],
            )?;
            tx.execute(
                "UPDATE project_files SET status='indexed' WHERE project_guid=?1 AND path='keep.rs'",
                params![g],
            )?;
            tx.execute(
                "INSERT INTO project_file_chunks
                     (project_guid, file_path, model_id, code, qdrant_guid, start_line, end_line, start_column, end_column, status)
                 VALUES (?1, 'keep.rs', 'BAAI/bge-m3', 'code', ?2, 1, 2, 0, 1, 'active')",
                params![g, qg],
            )?;
            // (c) deleted file that still has a (soft-deleted) chunk → must remain until
            // sweep removes the chunk first (FK RESTRICT + the NOT EXISTS guard).
            tx.execute(
                "INSERT INTO project_files (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, 'BAAI/bge-m3', 'pending.rs', ?2, 'rust', 'indexing')",
                params![g, sha],
            )?;
            tx.execute(
                "UPDATE project_files SET status='deleted' WHERE project_guid=?1 AND path='pending.rs'",
                params![g],
            )?;
            tx.execute(
                "INSERT INTO project_file_chunks
                     (project_guid, file_path, model_id, code, qdrant_guid, start_line, end_line, start_column, end_column, status)
                 VALUES (?1, 'pending.rs', 'BAAI/bge-m3', 'code', ?2, 1, 2, 0, 1, 'deleted')",
                params![g, Uuid::new_v4().simple().to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let removed = prune_deleted_files(&pool, &CancellationToken::new()).await;
        assert_eq!(
            removed.removed, 1,
            "only the emptied deleted file should be pruned"
        );
        // keep.rs (indexed) and pending.rs (deleted but still has a chunk) remain.
        assert_eq!(file_count(&pool, &guid).await, 2);
    }

    /// How many times `gc_runs{trigger,outcome}` has been incremented, read off the
    /// rendered exposition — the same text a scraper sees, so a renamed family or a
    /// counter that silently never fires shows up here.
    fn gc_runs(metrics: &Metrics, trigger: &str, outcome: &str) -> u64 {
        metrics
            .gc
            .runs
            .get_or_create(&TriggerOutcomeLabels {
                trigger: if trigger == "manual" {
                    "manual"
                } else {
                    "worker"
                },
                outcome: match outcome {
                    "ok" => "ok",
                    "error" => "error",
                    _ => "cancelled",
                },
            })
            .get()
    }

    /// The step order **is** the invariant: `project_file_chunks → project_files` is
    /// `ON DELETE RESTRICT`, so `prune_deleted_files` can only fire once `sweep` has
    /// hard-deleted the chunks. Running the file prune first would make a
    /// `DELETE /files` take two passes to become physical — or, with a project
    /// producing chunks faster than GC clears them, never.
    #[tokio::test]
    async fn one_pass_makes_a_soft_deleted_file_physical() {
        let pool = migrated_pool().await;
        let guid = "1".repeat(32);
        seed_deleted_chunks(&pool, &guid, 3).await;

        // What `DELETE /files` leaves behind: chunks soft-deleted, file soft-deleted.
        let g = guid.clone();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "UPDATE project_files SET status = 'deleted' WHERE project_guid = ?1",
                params![g],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let store = FakeStore {
            fail: HashSet::new(),
        };
        let metrics = Metrics::new();
        let out = collect(
            &pool,
            &store,
            30,
            &metrics,
            "worker",
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(out.chunks.removed, 3, "the chunks were not swept");
        assert_eq!(
            out.files.removed, 1,
            "the file row survived the same pass that emptied it — the phases ran in \
             the wrong order, so a delete now takes two passes to become physical"
        );
        assert_eq!(deleted_count(&pool, &guid).await, 0);
        assert_eq!(file_count(&pool, &guid).await, 0);
        assert!(out.failed_phases().is_empty());
    }

    /// A pass in which a phase could not run must be labelled `error`, not `ok`.
    /// `gc_runs{outcome="ok"}` was incremented whenever the token was live, however
    /// many phases had failed — so a GC broken for days read as idle.
    #[tokio::test]
    async fn a_pass_with_a_failed_phase_is_counted_as_an_error_and_names_it() {
        let pool = migrated_pool().await;
        let guid = "2".repeat(32);
        seed_deleted_chunks(&pool, &guid, 2).await;

        // Qdrant refuses this project's collection, so `sweep` confirms nothing.
        let store = FakeStore {
            fail: HashSet::from([collection_name(&guid)]),
        };
        let metrics = Metrics::new();

        let out = collect(
            &pool,
            &store,
            30,
            &metrics,
            "worker",
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            out.failed_phases(),
            vec!["chunks"],
            "the failing phase must be named, not merely counted"
        );
        assert_eq!(gc_runs(&metrics, "worker", "error"), 1);
        assert_eq!(
            gc_runs(&metrics, "worker", "ok"),
            0,
            "a pass with a dead phase was counted as a clean sweep"
        );
        // And the rows are still there for the next pass — the orphan-prevention rule.
        assert_eq!(deleted_count(&pool, &guid).await, 2);
    }

    /// Severity wins: `error` outranks `cancelled`. A shutdown interrupting a pass is
    /// routine; a phase that could not run is not, and burying the second under the
    /// first would hide a broken GC behind every restart — the failure is loudest
    /// exactly when the service is being bounced to try to clear it.
    ///
    /// The two conditions have to arrive in that order to coexist at all: under a
    /// token cancelled up front, every phase short-circuits *cleanly* and none can
    /// fail. So the store fails the delete and cancels the token as it does — which
    /// is the real sequence, a Qdrant error and then a SIGTERM mid-pass.
    #[tokio::test]
    async fn a_failed_phase_outranks_a_cancelled_token() {
        /// Fails every `delete_batch`, cancelling `on_call` as it goes.
        struct FailThenCancel {
            on_call: CancellationToken,
        }

        #[async_trait]
        impl VectorStore for FailThenCancel {
            async fn delete_batch(
                &self,
                _collection: &str,
                _guids: Vec<String>,
            ) -> Result<(), VectorStoreError> {
                self.on_call.cancel();
                Err(VectorStoreError("qdrant is gone".to_string()))
            }
            async fn ensure_project(&self, _collection: &str) -> Result<(), VectorStoreError> {
                unreachable!()
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
            async fn search(
                &self,
                _collection: &str,
                _chunk_ids: Vec<UUIDv4>,
                _dense: Vec<f32>,
                _sparse_indices: Vec<u32>,
                _sparse_values: Vec<f32>,
                _colbert: Vec<Vec<f32>>,
                _top_k: u64,
            ) -> Result<Vec<SearchHit>, VectorStoreError> {
                unreachable!()
            }
        }

        let pool = migrated_pool().await;
        let guid = "3".repeat(32);
        seed_deleted_chunks(&pool, &guid, 1).await;

        let token = CancellationToken::new();
        let store = FailThenCancel {
            on_call: token.clone(),
        };
        let metrics = Metrics::new();

        let out = collect(&pool, &store, 30, &metrics, "manual", &token).await;

        assert!(token.is_cancelled(), "the store must have cancelled it");
        assert_eq!(
            out.failed_phases(),
            vec!["chunks"],
            "the sweep failed before the cancellation landed"
        );
        assert_eq!(
            gc_runs(&metrics, "manual", "error"),
            1,
            "a broken phase was filed under the shutdown that followed it"
        );
        assert_eq!(gc_runs(&metrics, "manual", "cancelled"), 0);
        assert_eq!(gc_runs(&metrics, "manual", "ok"), 0);
    }

    /// A pass that ran under a cancelled token did partial work at best. It must not
    /// be counted as `ok` — a restart during a sweep would otherwise manufacture a
    /// clean-sweep sample every time.
    #[tokio::test]
    async fn a_cancelled_pass_is_never_counted_as_a_clean_sweep() {
        let pool = migrated_pool().await;
        let store = FakeStore {
            fail: HashSet::new(),
        };
        let metrics = Metrics::new();

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let out = collect(&pool, &store, 30, &metrics, "worker", &cancelled).await;

        assert!(
            out.failed_phases().is_empty(),
            "a cancelled phase is a shutdown, not a breakage"
        );
        assert_eq!(gc_runs(&metrics, "worker", "cancelled"), 1);
        assert_eq!(gc_runs(&metrics, "worker", "ok"), 0);
    }

    /// An idle pass with nothing to do is the healthy case and must stay `ok` — the
    /// distinction the whole `Phase` type exists to draw is between *this* and the
    /// failing pass above, and collapsing them in either direction is the bug.
    #[tokio::test]
    async fn a_pass_with_nothing_to_do_is_ok_not_an_error() {
        let pool = migrated_pool().await;
        let store = FakeStore {
            fail: HashSet::new(),
        };
        let metrics = Metrics::new();

        let out = collect(
            &pool,
            &store,
            30,
            &metrics,
            "manual",
            &CancellationToken::new(),
        )
        .await;

        assert!(out.failed_phases().is_empty());
        assert_eq!(out.chunks.removed, 0);
        assert_eq!(gc_runs(&metrics, "manual", "ok"), 1);
        assert_eq!(gc_runs(&metrics, "manual", "error"), 0);
    }

    /// `failed_phases` is what `POST /gc` puts on the wire, so its names are a client
    /// contract — a renamed phase silently empties whatever a client keys on.
    #[test]
    fn the_failed_phase_names_are_stable() {
        let all_broken = GcOutcome {
            chunks: Phase::broke(0),
            files: Phase::broke(0),
            status_log: Phase::broke(0),
            research: Phase::broke(0),
        };
        assert_eq!(
            all_broken.failed_phases(),
            vec!["chunks", "files", "status_log", "research"]
        );
        assert!(GcOutcome::default().failed_phases().is_empty());

        // A phase that failed part-way reports both what it managed and that it broke.
        let partial = GcOutcome {
            chunks: Phase::broke(17),
            ..Default::default()
        };
        assert_eq!(partial.chunks.removed, 17);
        assert_eq!(partial.failed_phases(), vec!["chunks"]);
    }

    /// GC and indexing run concurrently by design — GC is never allowed to block
    /// indexing — so the sweep must be safe against a file that is *resurrecting*.
    /// `deleted → indexing` is a legal transition, so a soft-deleted file can regain
    /// active chunks between one GC phase and the next. Pruning it then would either
    /// orphan those chunks or be refused by the RESTRICT FK, and the `NOT EXISTS`
    /// guard is what makes it neither.
    #[tokio::test]
    async fn a_resurrected_file_survives_the_pass_that_was_about_to_prune_it() {
        let pool = migrated_pool().await;
        let guid = "4".repeat(32);
        seed_deleted_chunks(&pool, &guid, 2).await;

        // The `DELETE /files` state: file and chunks both soft-deleted.
        let g = guid.clone();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "UPDATE project_files SET status = 'deleted' WHERE project_guid = ?1",
                params![g],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // A reindex lands while the row is still `deleted`: the file goes back to
        // `indexing` and a fresh chunk is inserted. This is the window GC must not
        // act in.
        let g = guid.clone();
        let fresh = Uuid::new_v4().simple().to_string();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "UPDATE project_files SET status = 'indexing' WHERE project_guid = ?1",
                params![g],
            )?;
            tx.execute(
                "INSERT INTO project_file_chunks
                     (project_guid, file_path, model_id, code, qdrant_guid,
                      start_line, end_line, start_column, end_column, status)
                 VALUES (?1, 'a.rs', 'BAAI/bge-m3', 'new code', ?2, 1, 2, 0, 1, 'active')",
                params![g, fresh],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let store = FakeStore {
            fail: HashSet::new(),
        };
        let out = collect(
            &pool,
            &store,
            30,
            &Metrics::new(),
            "worker",
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(out.chunks.removed, 2, "the old chunks are still swept");
        assert_eq!(
            out.files.removed, 0,
            "GC pruned a file that had come back to life; its fresh chunk is now \
             orphaned, or the FK refused and the pass failed"
        );
        assert_eq!(
            file_count(&pool, &guid).await,
            1,
            "the file row must survive"
        );
        // And the new chunk is untouched: only `deleted` rows are ever swept.
        let active: i64 = pool
            .transaction(CancellationToken::new(), {
                let g = guid.clone();
                move |tx| {
                    tx.query_row(
                        "SELECT COUNT(*) FROM project_file_chunks
                          WHERE project_guid = ?1 AND status = 'active'",
                        params![g],
                        |r| r.get(0),
                    )
                    .map_err(SQLite3PoolError::from)
                }
            })
            .await
            .unwrap();
        assert_eq!(active, 1, "the live chunk was swept");
    }

    /// A live file — never soft-deleted at all — must be invisible to every phase.
    /// This is the plainest form of "GC does not block indexing": a pass running
    /// against a project mid-index must change nothing about it.
    #[tokio::test]
    async fn a_pass_over_a_live_project_changes_nothing() {
        let pool = migrated_pool().await;
        let guid = "5".repeat(32);
        seed_deleted_chunks(&pool, &guid, 0).await;
        let g = guid.clone();
        let live = Uuid::new_v4().simple().to_string();
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO project_file_chunks
                     (project_guid, file_path, model_id, code, qdrant_guid,
                      start_line, end_line, start_column, end_column, status)
                 VALUES (?1, 'a.rs', 'BAAI/bge-m3', 'code', ?2, 1, 2, 0, 1, 'active')",
                params![g, live],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let store = FakeStore {
            fail: HashSet::new(),
        };
        let out = collect(
            &pool,
            &store,
            30,
            &Metrics::new(),
            "worker",
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(out.chunks.removed, 0);
        assert_eq!(out.files.removed, 0);
        assert!(out.failed_phases().is_empty());
        assert_eq!(file_count(&pool, &guid).await, 1);
    }

    /// The guard is what serializes the hourly worker against `POST /gc`, and it
    /// must hold under real contention: exactly one of many concurrent acquirers
    /// wins, and the flag is free again once they have all finished. A leaked flag
    /// turns every later `POST /gc` into a 409 and makes the worker skip every tick
    /// — GC off for the life of the process, silently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_one_of_many_concurrent_passes_holds_the_gc_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let (flag, winners, gate) =
                (Arc::clone(&flag), Arc::clone(&winners), Arc::clone(&gate));
            tasks.push(tokio::spawn(async move {
                if let Some(_guard) = GcGuard::try_acquire(&flag) {
                    winners.fetch_add(1, Ordering::SeqCst);
                    gate.notified().await;
                }
            }));
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "two GC passes ran at once; they would race the same deleted rows"
        );

        gate.notify_waiters();
        for t in tasks {
            t.await.expect("acquirer finishes");
        }
        assert!(
            GcGuard::try_acquire(&flag).is_some(),
            "the GC flag was leaked — every POST /gc is now a 409 and the worker \
             skips every tick, for the life of the process"
        );
    }
}
