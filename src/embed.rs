//! Shared embedding + Qdrant upsert pipeline used by both the indexing handler
//! (`post_index`) and the retry worker. Both need the identical "encode in
//! batches of 64, split sparse weights, upsert in batches of 256" loop; keeping
//! it here means one code path and one place to change batch sizes or vector
//! assembly.

use qdrant_client::{Qdrant, QdrantError};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::backend::v0::models::UUIDv4;
use crate::db::qdrant::{ChunkAsVector, insert_batch};
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
pub enum EmbedUpsertError {
    /// The cancellation token fired during embedding.
    Cancelled,
    /// The model server request failed.
    Embed(reqwest::Error),
    /// A Qdrant upsert failed.
    Store(QdrantError),
}

/// Embeds `chunks` (each `(qdrant_guid, code)`) and upserts the resulting
/// multi-vectors into `collection`. Side-effect-free apart from the embed/upsert
/// I/O, so behaviour is identical for every caller.
pub async fn embed_and_upsert(
    embedder: &dyn BGEm3Model,
    qdrant: &Qdrant,
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
            insert_batch(qdrant, collection, points_batch.to_vec())
                .await
                .map_err(EmbedUpsertError::Store)?;
        }
    }

    Ok(())
}
