use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::db::qdrant::{VectorStore, collection_name};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};

pub async fn run(db_pool: Arc<SQLite3Pool>, store: Arc<dyn VectorStore>, token: CancellationToken) {
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

        info!("GC worker: starting sweep.");
        sweep(&db_pool, &*store, &token).await;
        info!("GC worker: sweep complete.");
    }
}

async fn sweep(db_pool: &SQLite3Pool, store: &dyn VectorStore, token: &CancellationToken) {
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
        let _ = db_pool
            .transaction(token.clone(), move |tx| {
                let placeholders = (1..=confirmed_deleted.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "DELETE FROM project_file_chunks
                     WHERE status = 'deleted' AND qdrant_guid IN ({placeholders})"
                );
                tx.execute(&sql, rusqlite::params_from_iter(confirmed_deleted.iter()))?;
                Ok(())
            })
            .await;
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
}
