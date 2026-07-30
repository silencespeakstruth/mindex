//! Metrics decorator over [`VectorStore`].
//!
//! Composed once in `main.rs`, which is the whole point: the handlers, the GC
//! sweep and the retry worker all reach Qdrant through the same `Arc<dyn
//! VectorStore>`, so wrapping it covers every caller with no call-site edits and
//! no way for a future caller to be missed. Editing call sites would have meant
//! finding them all, twice — once now and once for each one added later.
//!
//! This also subsumes GC's per-collection `delete_batch` failures: they arrive
//! here as `op="delete_batch", outcome="error"`.

use std::sync::Arc;
use std::time::Instant;

use crate::backend::v0::models::UUIDv4;
use async_trait::async_trait;

use crate::backend::metrics::{Metrics, OpLabels, OpOutcomeLabels};
use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStore, VectorStoreError};

pub struct MeteredVectorStore {
    inner: Arc<dyn VectorStore>,
    metrics: Arc<Metrics>,
}

impl MeteredVectorStore {
    pub fn new(inner: Arc<dyn VectorStore>, metrics: Arc<Metrics>) -> Self {
        Self { inner, metrics }
    }

    /// Time one op and record its outcome. `op` is a `&'static str` chosen here,
    /// never derived from anything a caller supplies.
    fn record<T>(&self, op: &'static str, started: Instant, result: &Result<T, VectorStoreError>) {
        let q = &self.metrics.qdrant;
        q.duration
            .get_or_create(&OpLabels { op })
            .observe(started.elapsed().as_secs_f64());
        q.ops
            .get_or_create(&OpOutcomeLabels {
                op,
                outcome: if result.is_ok() { "ok" } else { "error" },
            })
            .inc();
    }
}

#[async_trait]
impl VectorStore for MeteredVectorStore {
    async fn ensure_project(&self, collection: &str) -> Result<(), VectorStoreError> {
        let t = Instant::now();
        let r = self.inner.ensure_project(collection).await;
        self.record("ensure_project", t, &r);
        r
    }

    async fn insert_batch(
        &self,
        collection: &str,
        chunks: Vec<ChunkAsVector>,
    ) -> Result<(), VectorStoreError> {
        let points = chunks.len() as u64;
        let t = Instant::now();
        let r = self.inner.insert_batch(collection, chunks).await;
        self.record("insert_batch", t, &r);
        // Only successful writes count as points stored; a failed batch that was
        // counted would make "points upserted" disagree with the collection.
        if r.is_ok() {
            self.metrics
                .qdrant
                .points
                .get_or_create(&OpLabels { op: "insert_batch" })
                .inc_by(points);
        }
        r
    }

    async fn delete_batch(
        &self,
        collection: &str,
        qdrant_guids: Vec<String>,
    ) -> Result<(), VectorStoreError> {
        let points = qdrant_guids.len() as u64;
        let t = Instant::now();
        let r = self.inner.delete_batch(collection, qdrant_guids).await;
        self.record("delete_batch", t, &r);
        if r.is_ok() {
            self.metrics
                .qdrant
                .points
                .get_or_create(&OpLabels { op: "delete_batch" })
                .inc_by(points);
        }
        r
    }

    async fn delete_collection(&self, collection: &str) -> Result<(), VectorStoreError> {
        let t = Instant::now();
        let r = self.inner.delete_collection(collection).await;
        self.record("delete_collection", t, &r);
        r
    }

    async fn health(&self) -> Result<(), VectorStoreError> {
        let t = Instant::now();
        let r = self.inner.health().await;
        self.record("health", t, &r);
        r
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the trait method, whose inputs are the irreducible parts \
                  of one hybrid query"
    )]
    async fn search(
        &self,
        collection: &str,
        chunk_ids: Vec<UUIDv4>,
        dense: Vec<f32>,
        sparse_indices: Vec<u32>,
        sparse_values: Vec<f32>,
        colbert: Vec<Vec<f32>>,
        top_k: u64,
    ) -> Result<Vec<SearchHit>, VectorStoreError> {
        let t = Instant::now();
        let r = self
            .inner
            .search(
                collection,
                chunk_ids,
                dense,
                sparse_indices,
                sparse_values,
                colbert,
                top_k,
            )
            .await;
        self.record("search", t, &r);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Failing;

    #[async_trait]
    impl VectorStore for Failing {
        async fn ensure_project(&self, _: &str) -> Result<(), VectorStoreError> {
            Ok(())
        }
        async fn insert_batch(
            &self,
            _: &str,
            _: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            Err(VectorStoreError("qdrant said no".to_string()))
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

    /// A failed batch must be visible as an error *and* must not inflate the
    /// points counter — otherwise "points upserted" drifts from the collection.
    #[tokio::test]
    async fn a_failed_batch_is_an_error_outcome_and_stores_no_points() {
        let metrics = Arc::new(Metrics::new());
        let store = MeteredVectorStore::new(Arc::new(Failing), Arc::clone(&metrics));

        assert!(store.insert_batch("c", vec![]).await.is_err());

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(r#"mindex_qdrant_ops_total{op="insert_batch",outcome="error"} 1"#),
            "{text}"
        );
        assert!(
            !text.contains(r#"mindex_qdrant_points_total{op="insert_batch"}"#),
            "a failed batch counted its points: {text}"
        );
        assert!(text.contains(r#"mindex_qdrant_op_duration_seconds_count{op="insert_batch"} 1"#));
    }

    #[tokio::test]
    async fn a_successful_delete_counts_its_points() {
        let metrics = Arc::new(Metrics::new());
        let store = MeteredVectorStore::new(Arc::new(Failing), Arc::clone(&metrics));

        store
            .delete_batch("c", vec!["a".into(), "b".into(), "c".into()])
            .await
            .expect("the fake succeeds");

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(r#"mindex_qdrant_points_total{op="delete_batch"} 3"#),
            "{text}"
        );
    }
}
