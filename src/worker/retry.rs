use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::backend::v0::models::UUIDv4;
use crate::db::files::set_file_status;
use crate::db::qdrant::{VectorStore, collection_name};
use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
use crate::embed::{EmbedUpsertError, embed_and_upsert};
use crate::models::bge_m3::BGEm3Model;

pub(crate) const MAX_RETRIES: i64 = 3;

/// How often the worker re-warns about permanently-failed files (in addition to
/// the one-shot warning at startup).
const FAILED_WARN_INTERVAL: Duration = Duration::from_secs(3600);

/// Logs a WARN if any files have exhausted their retries (`status='failed'` with
/// `retry_count >= MAX_RETRIES`). The retry worker stops touching such files, so
/// they are otherwise invisible. Called once at startup and periodically by the
/// worker.
pub(crate) async fn warn_permanently_failed(db_pool: &SQLite3Pool, token: CancellationToken) {
    let count: i64 = db_pool
        .transaction(token, |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM project_files
                 WHERE status = 'failed' AND retry_count >= ?1",
                rusqlite::params![MAX_RETRIES],
                |r| r.get(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap_or(0);

    if count > 0 {
        warn!(
            count,
            max_retries = MAX_RETRIES,
            "Files have exhausted their retries and are stuck in 'failed'; they will not be \
             retried automatically. Re-push them to reindex, and check the model server \
             (--model-server) and Qdrant (--qdrant-server) reachability."
        );
    }
}

pub async fn run(
    db_pool: Arc<SQLite3Pool>,
    store: Arc<dyn VectorStore>,
    model_client: Arc<dyn BGEm3Model>,
    model_id: String,
    embed_batch: usize,
    stuck_grace_secs: i64,
    token: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Throttle the periodic permanently-failed warning. Start in the past so the
    // first sweep with such files warns immediately.
    let mut last_failed_warn = Instant::now() - FAILED_WARN_INTERVAL;

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = token.cancelled() => {
                info!("Retry worker: shutting down.");
                break;
            }
        }

        if last_failed_warn.elapsed() >= FAILED_WARN_INTERVAL {
            warn_permanently_failed(&db_pool, token.clone()).await;
            last_failed_warn = Instant::now();
        }

        let stuck_files: Vec<(String, String, String)> = db_pool
            .transaction(token.clone(), {
                let model_id = model_id.clone();
                move |tx| {
                // The `indexing` grace must exceed the longest legitimate in-flight
                // request, otherwise the worker races a live batch (which holds its
                // files in 'indexing' for the whole embed pass) — re-embedding them
                // one-by-one and tripping illegal status transitions.
                tx.prepare(
                    "SELECT project_guid, path, model_id
                     FROM project_files
                     WHERE model_id = ?2
                       AND ((status IN ('just_uploaded', 'indexing')
                             AND status_updated_at < unixepoch() - ?3)
                         OR (status = 'failed'
                             AND retry_count < ?1
                             AND status_updated_at < unixepoch() - 60))",
                )?
                .query_map(rusqlite::params![MAX_RETRIES, model_id, stuck_grace_secs], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
            }})
            .await
            .unwrap_or_default();

        for (project_guid, path, file_model_id) in stuck_files {
            if token.is_cancelled() {
                break;
            }

            info!(%project_guid, %path, "Retry worker: retrying stuck file.");
            retry_file(
                &db_pool, &*store, &*model_client, &project_guid, &path, &file_model_id,
                embed_batch, &token,
            )
            .await;
        }
    }
}

/// Re-indexes one stuck/failed file: `*→indexing → {indexed | failed}`. Extracted
/// from the `run` loop so it can be unit-tested with a fake `VectorStore` +
/// `BGEm3Model` (no live Qdrant or model server).
#[allow(clippy::too_many_arguments)] // irreducible per-file deps (db, store, embedder, ids, batch, token)
async fn retry_file(
    db_pool: &SQLite3Pool,
    store: &dyn VectorStore,
    embedder: &dyn BGEm3Model,
    project_guid: &str,
    path: &str,
    model_id: &str,
    embed_batch: usize,
    token: &CancellationToken,
) {
    // Move to 'indexing' first so the whole attempt is a clean
    // {failed,indexing,…}→indexing→{indexed,failed} path through the state machine
    // (the triggers forbid failed→failed / failed→indexed directly).
    set_file_status(db_pool, project_guid, path, model_id, "indexing", false, token.clone()).await;

    let chunks: Vec<(String, String)> = db_pool
        .transaction(token.clone(), {
            let (pg, p, m) = (project_guid.to_string(), path.to_string(), model_id.to_string());
            move |tx| {
                tx.prepare(
                    "SELECT qdrant_guid, code
                     FROM project_file_chunks
                     WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3
                       AND status = 'active'",
                )?
                .query_map(rusqlite::params![pg, p, m], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
            }
        })
        .await
        .unwrap_or_default();

    if chunks.is_empty() {
        // No active chunks → the file sliced to nothing (too short). The worker can't
        // re-slice, so mark it 'indexed' (0 chunks) rather than looping it to 'failed'
        // forever — 'failed'→'indexed' is illegal, so a wrong 'failed' here would trap it.
        info!(%project_guid, %path, "Retry worker: no active chunks (too short); marking 'indexed'.");
        set_file_status(db_pool, project_guid, path, model_id, "indexed", false, token.clone()).await;
        return;
    }

    let collection = collection_name(project_guid);

    // The chunk rows store qdrant_guid as text; parse back to UUIDv4 for upsert.
    let to_embed: Vec<(UUIDv4, String)> = chunks
        .into_iter()
        .map(|(g, code)| (UUIDv4(Uuid::parse_str(&g).unwrap_or_default()), code))
        .collect();

    let success = match embed_and_upsert(embedder, store, &collection, &to_embed, token, embed_batch)
        .await
    {
        Ok(()) => true,
        Err(EmbedUpsertError::Cancelled) => false,
        Err(EmbedUpsertError::Embed(e)) => {
            error!(
                error = ?e,
                %project_guid,
                %path,
                "Retry worker: embedding request failed; leaving file 'failed'. \
                 Check the model server at --model-server is up."
            );
            false
        }
        Err(EmbedUpsertError::Store(e)) => {
            error!(
                error = ?e,
                %project_guid,
                %path,
                "Retry worker: Qdrant upsert failed; leaving file 'failed'. \
                 Check Qdrant is reachable at --qdrant-server."
            );
            false
        }
    };

    set_file_status(
        db_pool,
        project_guid,
        path,
        model_id,
        if success { "indexed" } else { "failed" },
        !success,
        token.clone(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rusqlite::params;
    use std::collections::HashMap;
    use std::path::Path;

    use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStoreError};
    use crate::models::bge_m3::{BGEm3EmbedRequest, BGEm3EmbedResponse, EncodeError};

    const PG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const MODEL: &str = "BAAI/bge-m3";
    const PATH: &str = "a.rs";

    struct OkEmbedder;
    #[async_trait]
    impl BGEm3Model for OkEmbedder {
        async fn encode(
            &self,
            req: BGEm3EmbedRequest,
            _token: CancellationToken,
        ) -> Result<BGEm3EmbedResponse, EncodeError> {
            let n = req.texts.len();
            Ok(BGEm3EmbedResponse {
                dense_vecs: vec![vec![0.1; 4]; n],
                sparse_vecs: vec![HashMap::from([(1u32, 0.5f32)]); n],
                colbert_vecs: vec![vec![vec![0.1; 4]]; n],
            })
        }
    }

    /// `VectorStore` fake whose `insert_batch` succeeds or fails on demand.
    struct Store {
        fail_upsert: bool,
    }
    #[async_trait]
    impl VectorStore for Store {
        async fn insert_batch(
            &self,
            _collection: &str,
            _chunks: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            if self.fail_upsert {
                Err(VectorStoreError("forced upsert failure".to_string()))
            } else {
                Ok(())
            }
        }
        async fn ensure_project(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_batch(&self, _c: &str, _g: Vec<String>) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn search(
            &self,
            _c: &str,
            _i: Vec<UUIDv4>,
            _d: Vec<f32>,
            _si: Vec<u32>,
            _sv: Vec<f32>,
            _cb: Vec<Vec<f32>>,
            _k: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            unreachable!()
        }
    }

    /// Migrated pool with a project + a file currently in `'failed'` (retry_count=1)
    /// and `n_chunks` active chunks — i.e. a file the retry worker would pick up.
    async fn pool_with_failed_file(n_chunks: usize) -> SQLite3Pool {
        let pool = SQLite3Pool::new(Path::new(":memory:"), 1);
        pool.transaction(CancellationToken::new(), move |tx| {
            for m in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![PG, MODEL],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, ?3, ?4, 'rust', 'indexing')",
                params![PG, MODEL, PATH, "0".repeat(64)],
            )?;
            for _ in 0..n_chunks {
                tx.execute(
                    "INSERT INTO project_file_chunks
                         (project_guid, file_path, model_id, code, qdrant_guid,
                          start_line, end_line, start_column, end_column, status)
                     VALUES (?1, ?2, ?3, 'code', ?4, 1, 2, 0, 1, 'active')",
                    params![PG, PATH, MODEL, Uuid::new_v4().simple().to_string()],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        // indexing → failed (retry_count = 1): the state the worker retries from.
        set_file_status(&pool, PG, PATH, MODEL, "failed", true, CancellationToken::new()).await;
        pool
    }

    async fn current(pool: &SQLite3Pool) -> (String, i64) {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.query_row(
                "SELECT status, retry_count FROM project_files
                 WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3",
                params![PG, MODEL, PATH],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    async fn log_pairs(pool: &SQLite3Pool) -> Vec<(Option<String>, String)> {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.prepare("SELECT old_status, new_status FROM project_file_status_log ORDER BY id")?
                .query_map([], |r| {
                    Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn retry_success_goes_failed_indexing_indexed_and_resets_count() {
        let pool = pool_with_failed_file(2).await;
        let store = Store { fail_upsert: false };

        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &CancellationToken::new()).await;

        assert_eq!(current(&pool).await, ("indexed".to_string(), 0));
        // The transition log proves the path went through 'indexing', never failed→indexed.
        let pairs = log_pairs(&pool).await;
        assert!(pairs.contains(&(Some("failed".into()), "indexing".into())), "{pairs:?}");
        assert!(pairs.contains(&(Some("indexing".into()), "indexed".into())), "{pairs:?}");
    }

    #[tokio::test]
    async fn retry_store_failure_goes_failed_indexing_failed_and_bumps_count() {
        let pool = pool_with_failed_file(2).await;
        let store = Store { fail_upsert: true };

        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &CancellationToken::new()).await;

        // Was failed(1) → indexing → failed(2).
        assert_eq!(current(&pool).await, ("failed".to_string(), 2));
        let pairs = log_pairs(&pool).await;
        assert!(pairs.contains(&(Some("failed".into()), "indexing".into())), "{pairs:?}");
        assert!(pairs.contains(&(Some("indexing".into()), "failed".into())), "{pairs:?}");
    }

    #[tokio::test]
    async fn retry_with_no_active_chunks_marks_indexed() {
        let pool = pool_with_failed_file(0).await;
        let store = Store { fail_upsert: false };

        retry_file(&pool, &store, &OkEmbedder, PG, PATH, MODEL, 64, &CancellationToken::new()).await;

        // No chunks (too short) → indexing → indexed, retry_count reset. Must NOT be
        // 'failed' (that would trap it: failed→indexed is an illegal transition).
        assert_eq!(current(&pool).await, ("indexed".to_string(), 0));
    }
}
