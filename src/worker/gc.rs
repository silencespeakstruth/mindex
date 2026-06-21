use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::db::qdrant::{VectorStore, collection_name};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};

/// How long transition rows are kept in `project_file_status_log`. The log grows
/// one row per status change, so it is pruned alongside the chunk sweep.
const STATUS_LOG_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600); // 30 days

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

/// One full GC pass: hard-delete confirmed-removed chunks, then drop now-empty
/// `deleted` file rows, then prune the old status log. The step order is the
/// invariant (chunks before files, since the chunk→file FK is RESTRICT), so it
/// lives in one place shared by the worker and `POST /gc`. Returns
/// `(chunks_removed, files_removed, status_log_pruned)`. Callers serialize this
/// behind [`GcGuard`].
pub(crate) async fn collect(
    db_pool: &SQLite3Pool,
    store: &dyn VectorStore,
    token: &CancellationToken,
) -> (usize, usize, usize) {
    let chunks = sweep(db_pool, store, token).await;
    let files = prune_deleted_files(db_pool, token).await;
    let log = prune_status_log(db_pool, token).await;
    (chunks, files, log)
}

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    store: Arc<dyn VectorStore>,
    gc_flag: Arc<AtomicBool>,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
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
        let (chunks, files, _log) = collect(&db_pool, &*store, &token).await;
        info!(chunks_removed = chunks, files_removed = files, "GC worker: sweep complete.");
    }
}

/// Removes `project_files` rows that were marked `status='deleted'` (by
/// `DELETE /files`) once they have no chunk rows left — i.e. after [`sweep`] has
/// hard-deleted their (soft-deleted) chunks. The FK to chunks is RESTRICT, so this
/// can only fire after the chunks are gone; running it after `sweep` in the same
/// pass is what makes a delete eventually physical. Returns the rows removed.
pub(crate) async fn prune_deleted_files(db_pool: &SQLite3Pool, token: &CancellationToken) -> usize {
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
        Ok(n) => n,
        Err(SQLite3PoolError::Cancelled) => 0,
        Err(e) => {
            error!(error = ?e, "GC worker: failed to prune deleted file rows.");
            0
        }
    }
}

/// Deletes `project_file_status_log` rows older than [`STATUS_LOG_RETENTION`].
/// A single `DELETE` (SQLite has no `DELETE ... LIMIT` in the bundled build); the
/// audit log is small relative to the chunk tables, so one statement is fine.
pub(crate) async fn prune_status_log(db_pool: &SQLite3Pool, token: &CancellationToken) -> usize {
    let max_age_secs = STATUS_LOG_RETENTION.as_secs() as i64;

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
        Ok(0) => 0,
        Ok(rows) => {
            info!(
                rows,
                retention_days = STATUS_LOG_RETENTION.as_secs() / 86_400,
                "GC worker: pruned old status-log rows."
            );
            rows
        }
        Err(SQLite3PoolError::Cancelled) => 0,
        Err(e) => {
            error!(error = ?e, "GC worker: failed to prune the status log.");
            0
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
) -> usize {
    let mut total_removed = 0usize;
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
                error!(error = ?e, "GC worker: failed to query deleted chunks from SQLite; aborting this sweep.");
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
            // batch; the next scheduled sweep will retry.
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
                error!(error = ?e, "GC worker: failed to hard-delete swept chunk rows.");
                // Vectors are already gone from Qdrant; the rows stay 'deleted' and a
                // later sweep retries the SQLite delete. Avoid spinning on this batch.
                break;
            }
        }
    }
    total_removed
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
        let pool = SQLite3Pool::new(Path::new(":memory:"), 1);
        pool.transaction(CancellationToken::new(), |tx| {
            for migration in crate::MIGRATIONS {
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
        let qdrant_guids: Vec<String> =
            (0..n).map(|_| Uuid::new_v4().simple().to_string()).collect();
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

        let store = FakeStore { fail: HashSet::new() };
        sweep(&pool, &store, &CancellationToken::new()).await;

        assert_eq!(deleted_count(&pool, &guid).await, 0, "all confirmed rows should be gone");
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
        assert_eq!(deleted_count(&pool, &guid_ok).await, 0, "succeeded project should be swept");
        assert_eq!(deleted_count(&pool, &guid_fail).await, 2, "failed project's rows must remain");
    }

    #[tokio::test]
    async fn sweep_on_empty_is_a_noop() {
        let pool = migrated_pool().await;
        let store = FakeStore { fail: HashSet::new() };
        // No deleted chunks at all: must return promptly without error.
        sweep(&pool, &store, &CancellationToken::new()).await;
    }

    #[test]
    fn gc_guard_serializes_and_releases() {
        let flag = Arc::new(AtomicBool::new(false));

        let guard = GcGuard::try_acquire(&flag).expect("free flag is acquirable");
        // A second acquire while the first is held must fail (serialization).
        assert!(GcGuard::try_acquire(&flag).is_none(), "held flag rejects a second guard");

        drop(guard);
        // After the guard drops the flag is free again.
        assert!(GcGuard::try_acquire(&flag).is_some(), "dropped guard frees the flag");
    }

    async fn status_log_count(pool: &SQLite3Pool) -> i64 {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.query_row("SELECT COUNT(*) FROM project_file_status_log", [], |r| r.get(0))
                .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
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

        prune_status_log(&pool, &CancellationToken::new()).await;

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
        assert_eq!(removed, 1, "only the emptied deleted file should be pruned");
        // keep.rs (indexed) and pending.rs (deleted but still has a chunk) remain.
        assert_eq!(file_count(&pool, &guid).await, 2);
    }
}
