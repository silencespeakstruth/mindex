//! Shared embedding + Qdrant upsert pipeline used by both the indexing handler
//! (`post_index`), its `vectors_only` re-embed branch, and the retry worker.
//! All need the identical "embed in batches of `embed_batch`, upsert in batches
//! of `upsert_batch`" loop; keeping it here means one code path and one place
//! to change vector assembly. Batch sizes come from config via [`EmbedTuning`].

use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::backend::v0::models::UUIDv4;
use crate::db::qdrant::{ChunkAsVector, VectorStore, VectorStoreError};
use crate::models::embedder::{Embedder, EncodeError};

/// Tuning for the embed→upsert pipeline (from `[indexing]`/`[qdrant]` config),
/// passed as one value so the callers (handler + retry worker) stay in sync.
#[derive(Debug, Clone, Copy)]
pub struct EmbedTuning {
    /// Chunks sent to the model server per `/v1/embeddings` call (the request
    /// batch lever; vLLM further schedules with its own continuous batching).
    pub embed_batch: usize,
    /// Points sent to Qdrant per upsert.
    pub upsert_batch: usize,
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
    /// The embedder's response was not what the registry promised (shape, row
    /// count or dimension).
    Decode(String),
    /// A vector-store upsert failed.
    Store(VectorStoreError),
}

/// Embeds `chunks` (each `(qdrant_guid, code)`) and upserts the resulting dense
/// vectors into `collection`. `tuning.embed_batch` is the number of chunks sent
/// per `/v1/embeddings` call; `tuning.upsert_batch` governs Qdrant upsert
/// sizing. Side-effect-free apart from the embed/upsert I/O — and the optional
/// `progress` callback, the one deliberate departure from that sentence: it
/// exists so a streaming `/index` (`?stream=yes`) can report per-batch progress,
/// is invoked only after a batch is fully upserted, and a `None` caller (the
/// retry worker, every test that doesn't pin it) gets byte-for-byte the old
/// behaviour.
pub async fn embed_and_upsert(
    embedder: &dyn Embedder,
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

        let dense_vecs = match embedder.embed(texts, token.clone()).await {
            Ok(val) => val,
            Err(EncodeError::Cancelled) => return Err(EmbedUpsertError::Cancelled),
            Err(EncodeError::Request(e)) => return Err(EmbedUpsertError::Embed(e)),
            Err(EncodeError::Timeout(d)) => return Err(EmbedUpsertError::Timeout(d)),
            Err(EncodeError::Decode(e)) => return Err(EmbedUpsertError::Decode(e)),
        };

        // The embedder's contract is one row per text, positionally aligned. The
        // HTTP client already refuses a mismatch, but this function is also fed
        // by test fakes — and the historical failure was exactly here: a `zip`
        // silently truncated a short response, those chunks were never upserted,
        // and their file was still marked `indexed` with no error anywhere.
        if dense_vecs.len() != guids.len() {
            return Err(EmbedUpsertError::Decode(format!(
                "embedder returned {} rows for {} texts; the embedder and this \
                 binary disagree about the request — check the embedder's log",
                dense_vecs.len(),
                guids.len()
            )));
        }

        let vector_batch: Vec<ChunkAsVector> = dense_vecs
            .into_iter()
            .zip(guids)
            .map(|(dense, guid)| ChunkAsVector { guid, dense })
            .collect();

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
    use std::sync::Mutex;
    use uuid::Uuid;

    use crate::db::qdrant::SearchHit;

    /// Tuning used by the embed tests (small embed batch to exercise batching).
    const TEST_TUNING: EmbedTuning = EmbedTuning {
        embed_batch: 64,
        upsert_batch: 256,
    };

    /// Embedder fake: returns deterministic vectors aligned to the input count, or
    /// `Cancelled` when configured to.
    struct StubEmbedder {
        cancel: bool,
    }

    #[async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(
            &self,
            texts: Vec<String>,
            _token: CancellationToken,
        ) -> Result<Vec<Vec<f32>>, EncodeError> {
            if self.cancel {
                return Err(EncodeError::Cancelled);
            }
            Ok(vec![vec![0.1; 4]; texts.len()])
        }
        async fn health(&self) -> Result<(), EncodeError> {
            unreachable!("embed_and_upsert does not call health")
        }
        async fn served_models(&self) -> Result<Vec<String>, EncodeError> {
            unreachable!("embed_and_upsert does not call served_models")
        }
    }

    /// An embedder whose reply does not have one row per text — the misalignment
    /// that used to be silent. Row count is `texts.len() + delta`, clamped at
    /// zero, so one fake covers both the short and the long response.
    struct MisalignedEmbedder {
        delta: isize,
    }

    #[async_trait]
    impl Embedder for MisalignedEmbedder {
        async fn embed(
            &self,
            texts: Vec<String>,
            _token: CancellationToken,
        ) -> Result<Vec<Vec<f32>>, EncodeError> {
            let rows = texts.len().saturating_add_signed(self.delta);
            Ok(vec![vec![0.1; 4]; rows])
        }
        async fn health(&self) -> Result<(), EncodeError> {
            unreachable!("embed_and_upsert does not call health")
        }
        async fn served_models(&self) -> Result<Vec<String>, EncodeError> {
            unreachable!("embed_and_upsert does not call served_models")
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

    /// A response with fewer rows than texts used to be truncated by `zip`: those
    /// chunks were never upserted, the file was still marked `indexed`, and nothing
    /// anywhere said so. The only honest answer is to fail the batch — the retry
    /// worker then re-attempts it.
    #[tokio::test]
    async fn a_short_embedder_response_fails_the_batch_instead_of_losing_chunks() {
        let embedder = MisalignedEmbedder { delta: -1 };
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: false,
        };

        let res = embed_and_upsert(
            &embedder,
            &store,
            "c",
            &chunks(4),
            &CancellationToken::new(),
            TEST_TUNING,
            None,
        )
        .await;

        assert!(
            matches!(res, Err(EmbedUpsertError::Decode(_))),
            "a short response was accepted: {res:?}"
        );
        assert!(
            store.upserted.lock().unwrap().is_empty(),
            "a misaligned batch must upsert nothing at all"
        );
    }

    /// The mirror case: more rows than texts used to index `guids[i]` out of
    /// bounds and panic — which `SQLite3Pool` would then have reported to the
    /// client as a disconnect. It must be a decode error, on the wire and in the
    /// log.
    #[tokio::test]
    async fn a_long_embedder_response_is_an_error_not_a_panic() {
        let embedder = MisalignedEmbedder { delta: 1 };
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: false,
        };

        let res = embed_and_upsert(
            &embedder,
            &store,
            "c",
            &chunks(4),
            &CancellationToken::new(),
            TEST_TUNING,
            None,
        )
        .await;

        assert!(
            matches!(res, Err(EmbedUpsertError::Decode(_))),
            "a long response was accepted: {res:?}"
        );
    }

    /// The misalignment check runs per batch, so a reply that is correct for the
    /// first batch and short for the second must still fail — and must not report
    /// progress for a batch it did not complete.
    #[tokio::test]
    async fn misalignment_is_caught_in_a_later_batch_too() {
        /// Correct for every batch but the second.
        struct SecondBatchShort {
            calls: Mutex<usize>,
        }

        #[async_trait]
        impl Embedder for SecondBatchShort {
            async fn embed(
                &self,
                texts: Vec<String>,
                _token: CancellationToken,
            ) -> Result<Vec<Vec<f32>>, EncodeError> {
                let nth = {
                    let mut c = self.calls.lock().unwrap();
                    *c += 1;
                    *c
                };
                let n = texts.len();
                let rows = if nth == 2 { n - 1 } else { n };
                Ok(vec![vec![0.1; 4]; rows])
            }
            async fn health(&self) -> Result<(), EncodeError> {
                unreachable!()
            }
            async fn served_models(&self) -> Result<Vec<String>, EncodeError> {
                unreachable!()
            }
        }

        let embedder = SecondBatchShort {
            calls: Mutex::new(0),
        };
        let store = RecordingStore {
            upserted: Mutex::new(vec![]),
            fail_upsert: false,
        };
        let seen: Mutex<Vec<EmbedProgress>> = Mutex::new(vec![]);
        let record = |p: EmbedProgress| seen.lock().unwrap().push(p);

        // 100 chunks at embed_batch 64 → batches of 64 then 36; the second is short.
        let res = embed_and_upsert(
            &embedder,
            &store,
            "c",
            &chunks(100),
            &CancellationToken::new(),
            TEST_TUNING,
            Some(&record),
        )
        .await;

        assert!(
            matches!(res, Err(EmbedUpsertError::Decode(_))),
            "the second batch's misalignment slipped through: {res:?}"
        );
        assert_eq!(
            store.upserted.lock().unwrap().len(),
            64,
            "the first batch is legitimately upserted; the second must not be"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "progress must be reported only for batches that completed"
        );
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
