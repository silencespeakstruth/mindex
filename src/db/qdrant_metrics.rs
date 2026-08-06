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

    async fn count_points(&self, collection: &str) -> Result<Option<u64>, VectorStoreError> {
        let t = Instant::now();
        let r = self.inner.count_points(collection).await;
        self.record("count_points", t, &r);
        r
    }

    async fn list_collections(&self) -> Result<Option<Vec<String>>, VectorStoreError> {
        let t = Instant::now();
        let r = self.inner.list_collections().await;
        self.record("list_collections", t, &r);
        r
    }

    async fn search(
        &self,
        collection: &str,
        chunk_ids: Vec<UUIDv4>,
        dense: Vec<f32>,
        top_k: u64,
    ) -> Result<Vec<SearchHit>, VectorStoreError> {
        let t = Instant::now();
        let r = self.inner.search(collection, chunk_ids, dense, top_k).await;
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

    /// A store that answers every method, so the decorator can be asked to forward
    /// each one.
    struct Answering;

    #[async_trait]
    impl VectorStore for Answering {
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
        async fn count_points(&self, _: &str) -> Result<Option<u64>, VectorStoreError> {
            Ok(Some(7))
        }
        async fn list_collections(&self) -> Result<Option<Vec<String>>, VectorStoreError> {
            Ok(Some(vec!["a_v1".into(), "a_v2".into()]))
        }
        async fn search(
            &self,
            _: &str,
            _: Vec<UUIDv4>,
            _: Vec<f32>,
            _: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            Ok(vec![])
        }
    }

    /// `count_points` and `list_collections` are the trait's two methods with a
    /// **provided default**, and both defaults decline (`Ok(None)`) so the test fakes
    /// need no change. That makes forgetting an override here uniquely dangerous:
    /// production always wraps the real store in this decorator, so a missing forward
    /// would answer `None` for every project — `probe_vector_counts` would publish
    /// nothing and `warn_stale` would report that it cannot enumerate collections, so
    /// the only two detectors for a lost Qdrant volume or a stale
    /// `COLLECTION_SCHEMA_VERSION` would be silently switched off. No error, no empty
    /// family to notice, just a panel that never has data.
    #[tokio::test]
    async fn the_decorator_forwards_the_count_rather_than_taking_the_declining_default() {
        let metrics = Arc::new(Metrics::new());
        let store = MeteredVectorStore::new(Arc::new(Answering), Arc::clone(&metrics));

        assert_eq!(
            store.count_points("c").await.expect("the fake answers"),
            Some(7),
            "the decorator swallowed the count and answered the trait's declining \
             default; the vector-count probe is dead in every real deployment"
        );
        assert_eq!(
            store
                .list_collections()
                .await
                .expect("the fake answers")
                .as_deref(),
            Some(&["a_v1".to_string(), "a_v2".to_string()][..]),
            "the decorator swallowed the listing and answered the trait's declining \
             default; the stale-collection check is blind in every real deployment"
        );
        let text = metrics.render().expect("renders");
        assert!(
            text.contains(r#"mindex_qdrant_ops_total{op="count_points",outcome="ok"} 1"#),
            "the forwarded call was not measured"
        );
        assert!(
            text.contains(r#"mindex_qdrant_ops_total{op="list_collections",outcome="ok"} 1"#),
            "the forwarded call was not measured"
        );
    }

    /// The decorator exists so no caller can be missed, which only holds while every
    /// method is wrapped. This walks all eight and checks each left its own `op`
    /// label — a method added to the trait and forwarded without instrumentation
    /// makes that op invisible to the dashboard while looking perfectly healthy.
    #[tokio::test]
    async fn every_store_operation_records_its_own_op_label() {
        let metrics = Arc::new(Metrics::new());
        let store = MeteredVectorStore::new(Arc::new(Answering), Arc::clone(&metrics));

        store.ensure_project("c").await.expect("ok");
        store.insert_batch("c", vec![]).await.expect("ok");
        store.delete_batch("c", vec![]).await.expect("ok");
        store.delete_collection("c").await.expect("ok");
        store.health().await.expect("ok");
        store.count_points("c").await.expect("ok");
        store.list_collections().await.expect("ok");
        store.search("c", vec![], vec![], 5).await.expect("ok");

        let text = metrics.render().expect("renders");
        for op in [
            "ensure_project",
            "insert_batch",
            "delete_batch",
            "delete_collection",
            "health",
            "count_points",
            "list_collections",
            "search",
        ] {
            assert!(
                text.contains(&format!(
                    r#"mindex_qdrant_ops_total{{op="{op}",outcome="ok"}} 1"#
                )),
                "{op} was not counted: {text}"
            );
            assert!(
                text.contains(&format!(
                    r#"mindex_qdrant_op_duration_seconds_count{{op="{op}"}} 1"#
                )),
                "{op} was not timed: {text}"
            );
        }
    }

    /// An empty upsert is a legitimate call (a file that sliced to nothing) and must
    /// not manufacture a points sample — `mindex_qdrant_points_total` is what a
    /// reader compares against the collection's own size.
    #[tokio::test]
    async fn an_empty_batch_records_the_op_but_no_points() {
        let metrics = Arc::new(Metrics::new());
        let store = MeteredVectorStore::new(Arc::new(Answering), Arc::clone(&metrics));

        store.insert_batch("c", vec![]).await.expect("ok");

        let text = metrics.render().expect("renders");
        assert!(text.contains(r#"mindex_qdrant_ops_total{op="insert_batch",outcome="ok"} 1"#));
        assert!(
            text.contains(r#"mindex_qdrant_points_total{op="insert_batch"} 0"#),
            "an empty batch must record zero points, not a phantom count: {text}"
        );
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
