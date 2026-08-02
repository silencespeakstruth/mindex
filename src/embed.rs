//! Shared embedding + Qdrant upsert pipeline used by both the indexing handler
//! (`post_index`) and the retry worker. Both need the identical "encode in
//! batches of `embed_batch`, split sparse weights, upsert in batches of
//! `upsert_batch`" loop; keeping it here means one code path and one place to
//! change vector assembly. Batch sizes + the sparse threshold come from config
//! via [`EmbedTuning`].

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::backend::v0::models::UUIDv4;
use crate::db::qdrant::{ChunkAsVector, VectorStore, VectorStoreError};
use crate::models::bge_m3::{BGEm3EmbedRequest, BGEm3EmbedResponse, BGEm3Model, EncodeError};

/// Tuning for the embed→upsert pipeline (from `[indexing]`/`[qdrant]` config),
/// passed as one value so the two callers (handler + retry worker) stay in sync.
#[derive(Debug, Clone, Copy)]
pub struct EmbedTuning {
    /// Chunks sent to the model server per `/encode` call (GPU batch lever).
    pub embed_batch: usize,
    /// Points sent to Qdrant per upsert.
    pub upsert_batch: usize,
    /// Sparse weights at or below this magnitude are dropped before upsert.
    pub sparse_min_weight: f32,
}

/// One embed batch's worth of progress, reported after the batch has been both
/// encoded **and** upserted — `chunks_done` never counts a chunk whose vectors
/// are not in Qdrant yet. `chunks_done`/`chunks_total` are cumulative over one
/// `embed_and_upsert` call, which is what a client needs to compute an honest
/// chunks-per-second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedProgress {
    /// Chunks in the batch that just completed.
    pub batch_chunks: usize,
    /// Chunks completed so far, this batch included.
    pub chunks_done: usize,
    /// Chunks this whole call was given.
    pub chunks_total: usize,
}

/// Failure modes of [`embed_and_upsert`], kept distinct so callers can map each
/// to their own control flow (HTTP status + file-status recovery in the handler;
/// a success flag in the retry worker).
#[derive(Debug)]
pub enum EmbedUpsertError {
    /// The cancellation token fired during embedding.
    Cancelled,
    /// The model server request failed.
    Embed(reqwest::Error),
    /// The embedder stayed busy until the whole-call budget was spent.
    Timeout(std::time::Duration),
    /// The embedder's binary response couldn't be decoded (wire-format skew).
    Decode(String),
    /// A vector-store upsert failed.
    Store(VectorStoreError),
}

/// Embeds `chunks` (each `(qdrant_guid, code)`) and upserts the resulting
/// multi-vectors into `collection`. `tuning.embed_batch` is the number of chunks
/// sent per `/encode` call — the lever for GPU batch size (the model server further
/// sub-batches by its own `--batch`); `tuning.upsert_batch` and
/// `tuning.sparse_min_weight` govern Qdrant upsert sizing and sparse pruning.
/// Side-effect-free apart from the embed/upsert I/O — and the optional `progress`
/// callback, the one deliberate departure from that sentence: it exists so a
/// streaming `/index` (`?stream=yes`) can report per-batch progress, is invoked
/// only after a batch is fully upserted, and a `None` caller (the retry worker,
/// every test that doesn't pin it) gets byte-for-byte the old behaviour.
pub async fn embed_and_upsert(
    embedder: &dyn BGEm3Model,
    store: &dyn VectorStore,
    collection: &str,
    chunks: &[(UUIDv4, String)],
    token: &CancellationToken,
    tuning: EmbedTuning,
    progress: Option<&(dyn Fn(EmbedProgress) + Send + Sync)>,
) -> Result<(), EmbedUpsertError> {
    let chunks_total = chunks.len();
    let mut chunks_done = 0usize;
    for batch in chunks.chunks(tuning.embed_batch.max(1)) {
        let texts: Vec<String> = batch.iter().map(|(_, c)| c.clone()).collect();
        let guids: Vec<UUIDv4> = batch.iter().map(|(g, _)| *g).collect();

        info!(batch_len = batch.len(), "Embedding a batch.");

        let BGEm3EmbedResponse {
            dense_vecs,
            sparse_vecs,
            colbert_vecs,
        } = match embedder
            .encode(BGEm3EmbedRequest { texts }, token.clone())
            .await
        {
            Ok(val) => val,
            Err(EncodeError::Cancelled) => return Err(EmbedUpsertError::Cancelled),
            Err(EncodeError::Request(e)) => return Err(EmbedUpsertError::Embed(e)),
            Err(EncodeError::Timeout(d)) => return Err(EmbedUpsertError::Timeout(d)),
            Err(EncodeError::Decode(e)) => return Err(EmbedUpsertError::Decode(e)),
        };

        // The embedder's contract is one row per text, positionally aligned. Nothing
        // checked it: the `zip` below silently truncates a short response — those
        // chunks are never upserted and their file is still marked `indexed`, so the
        // file is permanently missing vectors with no error anywhere — and a long one
        // indexes `guids[i]` out of bounds and panics.
        if dense_vecs.len() != guids.len()
            || sparse_vecs.len() != guids.len()
            || colbert_vecs.len() != guids.len()
        {
            return Err(EmbedUpsertError::Decode(format!(
                "embedder returned {} dense / {} sparse / {} colbert rows for {} texts; \
                 the embedder and this binary disagree about the wire format — redeploy \
                 them from the same revision",
                dense_vecs.len(),
                sparse_vecs.len(),
                colbert_vecs.len(),
                guids.len()
            )));
        }

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
                if *w > tuning.sparse_min_weight {
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

        for points_batch in vector_batch.chunks(tuning.upsert_batch.max(1)) {
            store
                .insert_batch(collection, points_batch.to_vec())
                .await
                .map_err(EmbedUpsertError::Store)?;
        }

        chunks_done += batch.len();
        if let Some(report) = progress {
            report(EmbedProgress {
                batch_chunks: batch.len(),
                chunks_done,
                chunks_total,
            });
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

    /// Tuning used by the embed tests (small embed batch to exercise batching).
    const TEST_TUNING: EmbedTuning = EmbedTuning {
        embed_batch: 64,
        upsert_batch: 256,
        sparse_min_weight: 1e-5,
    };

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
        async fn health(&self) -> Result<(), EncodeError> {
            unreachable!("embed_and_upsert does not call health")
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
        async fn delete_collection(&self, _collection: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn health(&self) -> Result<(), VectorStoreError> {
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
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: false,
        };
        let input = chunks(3);

        embed_and_upsert(
            &embedder,
            &store,
            "c",
            &input,
            &CancellationToken::new(),
            TEST_TUNING,
            None,
        )
        .await
        .expect("should succeed");

        let upserted = store.upserted.lock().unwrap().clone();
        let expected: Vec<UUIDv4> = input.iter().map(|(g, _)| *g).collect();
        assert_eq!(upserted, expected);
    }

    #[tokio::test]
    async fn empty_input_upserts_nothing() {
        let embedder = StubEmbedder { cancel: false };
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: false,
        };

        embed_and_upsert(
            &embedder,
            &store,
            "c",
            &[],
            &CancellationToken::new(),
            TEST_TUNING,
            None,
        )
        .await
        .expect("empty is a no-op success");

        assert!(store.upserted.lock().unwrap().is_empty());
    }

    /// The progress callback fires once per embed batch, after the batch is
    /// upserted, with cumulative counts — the contract the streaming `/index`
    /// mode's `embedded` events (and every client rate display) are built on.
    #[tokio::test]
    async fn progress_reports_each_batch_with_cumulative_counts() {
        let embedder = StubEmbedder { cancel: false };
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: false,
        };
        // 150 chunks at embed_batch 64 → batches of 64, 64, 22.
        let input = chunks(150);
        let seen: Mutex<Vec<EmbedProgress>> = Mutex::new(vec![]);
        let record = |p: EmbedProgress| seen.lock().unwrap().push(p);

        embed_and_upsert(
            &embedder,
            &store,
            "c",
            &input,
            &CancellationToken::new(),
            TEST_TUNING,
            Some(&record),
        )
        .await
        .expect("should succeed");

        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            vec![
                EmbedProgress {
                    batch_chunks: 64,
                    chunks_done: 64,
                    chunks_total: 150,
                },
                EmbedProgress {
                    batch_chunks: 64,
                    chunks_done: 128,
                    chunks_total: 150,
                },
                EmbedProgress {
                    batch_chunks: 22,
                    chunks_done: 150,
                    chunks_total: 150,
                },
            ]
        );
    }

    #[tokio::test]
    async fn store_failure_maps_to_store_error() {
        let embedder = StubEmbedder { cancel: false };
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: true,
        };

        let res = embed_and_upsert(
            &embedder,
            &store,
            "c",
            &chunks(1),
            &CancellationToken::new(),
            TEST_TUNING,
            None,
        )
        .await;
        assert!(matches!(res, Err(EmbedUpsertError::Store(_))));
    }

    #[tokio::test]
    async fn embedder_cancel_maps_to_cancelled() {
        let embedder = StubEmbedder { cancel: true };
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: false,
        };

        let res = embed_and_upsert(
            &embedder,
            &store,
            "c",
            &chunks(1),
            &CancellationToken::new(),
            TEST_TUNING,
            None,
        )
        .await;
        assert!(matches!(res, Err(EmbedUpsertError::Cancelled)));
        // Nothing should have been upserted on the cancel path.
        assert!(store.upserted.lock().unwrap().is_empty());
    }
}
