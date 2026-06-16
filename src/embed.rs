//! Shared embedding + Qdrant upsert pipeline used by both the indexing handler
//! (`post_index`) and the retry worker. Both need the identical "encode in
//! batches of 64, split sparse weights, upsert in batches of 256" loop; keeping
//! it here means one code path and one place to change batch sizes or vector
//! assembly.

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::backend::v0::models::UUIDv4;
use crate::db::qdrant::{ChunkAsVector, VectorStore, VectorStoreError};
use crate::models::bge_m3::{BGEm3EmbedRequest, BGEm3EmbedResponse, BGEm3Model, EncodeError};

/// Number of chunks sent to the model server per `/encode` call.
const EMBED_BATCH: usize = 64;
/// Number of points sent to Qdrant per upsert.
const UPSERT_BATCH: usize = 256;
/// Sparse weights at or below this magnitude are dropped before upsert.
const SPARSE_MIN_WEIGHT: f32 = 1e-5;

/// Failure modes of [`embed_and_upsert`], kept distinct so callers can map each
/// to their own control flow (HTTP status + file-status recovery in the handler;
/// a success flag in the retry worker).
#[derive(Debug)]
pub enum EmbedUpsertError {
    /// The cancellation token fired during embedding.
    Cancelled,
    /// The model server request failed.
    Embed(reqwest::Error),
    /// A vector-store upsert failed.
    Store(VectorStoreError),
}

/// Embeds `chunks` (each `(qdrant_guid, code)`) and upserts the resulting
/// multi-vectors into `collection`. Side-effect-free apart from the embed/upsert
/// I/O, so behaviour is identical for every caller.
pub async fn embed_and_upsert(
    embedder: &dyn BGEm3Model,
    store: &dyn VectorStore,
    collection: &str,
    chunks: &[(UUIDv4, String)],
    token: &CancellationToken,
) -> Result<(), EmbedUpsertError> {
    for batch in chunks.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, c)| c.clone()).collect();
        let guids: Vec<UUIDv4> = batch.iter().map(|(g, _)| *g).collect();

        info!(batch_len = batch.len(), "Embedding a batch.");

        let BGEm3EmbedResponse {
            dense_vecs,
            sparse_vecs,
            colbert_vecs,
        } = match embedder.encode(BGEm3EmbedRequest { texts }, token.clone()).await {
            Ok(val) => val,
            Err(EncodeError::Cancelled) => return Err(EmbedUpsertError::Cancelled),
            Err(EncodeError::Request(e)) => return Err(EmbedUpsertError::Embed(e)),
        };

        let mut vector_batch: Vec<ChunkAsVector> = Vec::with_capacity(guids.len());
        for (i, ((dense, sparse), colbert)) in dense_vecs
            .into_iter()
            .zip(sparse_vecs.iter())
            .zip(colbert_vecs)
            .enumerate()
        {
            // Single pass: split the thresholded sparse weights into the parallel
            // index/value arrays Qdrant expects.
            let mut sparse_indices: Vec<u32> = Vec::with_capacity(sparse.len());
            let mut sparse_values: Vec<f32> = Vec::with_capacity(sparse.len());
            for (k, w) in sparse.iter() {
                if *w > SPARSE_MIN_WEIGHT {
                    sparse_indices.push(*k);
                    sparse_values.push(*w);
                }
            }

            vector_batch.push(ChunkAsVector {
                guid: guids[i],
                dense,
                sparse_indices,
                sparse_values,
                colbert,
            });
        }

        for points_batch in vector_batch.chunks(UPSERT_BATCH) {
            store
                .insert_batch(collection, points_batch.to_vec())
                .await
                .map_err(EmbedUpsertError::Store)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use uuid::Uuid;

    use crate::db::qdrant::SearchHit;

    /// Embedder fake: returns deterministic vectors aligned to the input count, or
    /// `Cancelled` when configured to.
    struct StubEmbedder {
        cancel: bool,
    }

    #[async_trait]
    impl BGEm3Model for StubEmbedder {
        async fn encode(
            &self,
            req: BGEm3EmbedRequest,
            _token: CancellationToken,
        ) -> Result<BGEm3EmbedResponse, EncodeError> {
            if self.cancel {
                return Err(EncodeError::Cancelled);
            }
            let n = req.texts.len();
            Ok(BGEm3EmbedResponse {
                dense_vecs: vec![vec![0.1; 4]; n],
                sparse_vecs: vec![HashMap::from([(1u32, 0.5f32)]); n],
                colbert_vecs: vec![vec![vec![0.1; 4]]; n],
            })
        }
    }

    /// Store fake: records the guids it was asked to upsert, or fails when configured.
    struct RecordingStore {
        upserted: Mutex<Vec<UUIDv4>>,
        fail_upsert: bool,
    }

    #[async_trait]
    impl VectorStore for RecordingStore {
        async fn insert_batch(
            &self,
            _collection: &str,
            chunks: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            if self.fail_upsert {
                return Err(VectorStoreError("boom".to_string()));
            }
            self.upserted
                .lock()
                .unwrap()
                .extend(chunks.iter().map(|c| c.guid));
            Ok(())
        }

        async fn ensure_project(&self, _collection: &str) -> Result<(), VectorStoreError> {
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
            _sparse_indices: Vec<u32>,
            _sparse_values: Vec<f32>,
            _colbert: Vec<Vec<f32>>,
            _top_k: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            unreachable!()
        }
    }

    fn chunks(n: usize) -> Vec<(UUIDv4, String)> {
        (0..n)
            .map(|i| (UUIDv4(Uuid::new_v4()), format!("code {i}")))
            .collect()
    }

    #[tokio::test]
    async fn upserts_every_chunk_in_order() {
        let embedder = StubEmbedder { cancel: false };
        let store = RecordingStore { upserted: Mutex::new(vec![]), fail_upsert: false };
        let input = chunks(3);

        embed_and_upsert(&embedder, &store, "c", &input, &CancellationToken::new())
            .await
            .expect("should succeed");

        let upserted = store.upserted.lock().unwrap().clone();
        let expected: Vec<UUIDv4> = input.iter().map(|(g, _)| *g).collect();
        assert_eq!(upserted, expected);
    }

    #[tokio::test]
    async fn empty_input_upserts_nothing() {
        let embedder = StubEmbedder { cancel: false };
        let store = RecordingStore { upserted: Mutex::new(vec![]), fail_upsert: false };

        embed_and_upsert(&embedder, &store, "c", &[], &CancellationToken::new())
            .await
            .expect("empty is a no-op success");

        assert!(store.upserted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn store_failure_maps_to_store_error() {
        let embedder = StubEmbedder { cancel: false };
        let store = RecordingStore { upserted: Mutex::new(vec![]), fail_upsert: true };

        let res = embed_and_upsert(&embedder, &store, "c", &chunks(1), &CancellationToken::new()).await;
        assert!(matches!(res, Err(EmbedUpsertError::Store(_))));
    }

    #[tokio::test]
    async fn embedder_cancel_maps_to_cancelled() {
        let embedder = StubEmbedder { cancel: true };
        let store = RecordingStore { upserted: Mutex::new(vec![]), fail_upsert: false };

        let res = embed_and_upsert(&embedder, &store, "c", &chunks(1), &CancellationToken::new()).await;
        assert!(matches!(res, Err(EmbedUpsertError::Cancelled)));
        // Nothing should have been upserted on the cancel path.
        assert!(store.upserted.lock().unwrap().is_empty());
    }
}
