use async_trait::async_trait;
use qdrant_client::Payload;
use qdrant_client::Qdrant;
use qdrant_client::QdrantError;
use qdrant_client::qdrant::Condition;
use qdrant_client::qdrant::CreateCollectionBuilder;
use qdrant_client::qdrant::DeletePointsBuilder;
use qdrant_client::qdrant::Distance;
use qdrant_client::qdrant::Filter;
use qdrant_client::qdrant::Fusion;
use qdrant_client::qdrant::HasIdCondition;
use qdrant_client::qdrant::MultiVectorComparator;
use qdrant_client::qdrant::MultiVectorConfigBuilder;
use qdrant_client::qdrant::NamedVectors;
use qdrant_client::qdrant::PointId;
use qdrant_client::qdrant::PointStruct;
use qdrant_client::qdrant::PrefetchQueryBuilder;
use qdrant_client::qdrant::Query;
use qdrant_client::qdrant::QueryPointsBuilder;
use qdrant_client::qdrant::SparseVectorParamsBuilder;
use qdrant_client::qdrant::SparseVectorsConfigBuilder;
use qdrant_client::qdrant::UpsertPointsBuilder;
use qdrant_client::qdrant::Vector;
use qdrant_client::qdrant::VectorParamsBuilder;
use qdrant_client::qdrant::VectorsConfigBuilder;
use qdrant_client::qdrant::condition;

use crate::backend::v0::models::UUIDv4;

const COLLECTION_SCHEMA_VERSION: &str = "v0";

pub fn collection_name(project_guid_simple: &str) -> String {
    format!("{}_{}", project_guid_simple, COLLECTION_SCHEMA_VERSION)
}

/// Qdrant collection name for a project GUID (its dashless simple form + schema
/// version). Convenience over `collection_name(&guid.0.as_simple().to_string())`.
pub fn collection_for(project_guid: UUIDv4) -> String {
    collection_name(&project_guid.0.as_simple().to_string())
}

#[derive(Clone)]
pub struct ChunkAsVector {
    pub guid: UUIDv4,
    pub dense: Vec<f32>,
    pub sparse_indices: Vec<u32>,
    pub sparse_values: Vec<f32>,
    pub colbert: Vec<Vec<f32>>,
}

impl From<ChunkAsVector> for PointStruct {
    fn from(value: ChunkAsVector) -> Self {
        let vectors = NamedVectors::default()
            .add_vector("dense", Vector::from(value.dense))
            .add_vector(
                "sparse",
                Vector::new_sparse(value.sparse_indices, value.sparse_values),
            )
            .add_vector("colbert", Vector::new_multi(value.colbert));

        PointStruct::new(
            value.guid.0.as_simple().to_string(),
            vectors,
            Payload::new(),
        )
    }
}

pub struct SearchHit {
    pub id: PointId,
    pub score: f32,
}

/// Error surfaced by [`VectorStore`]. Owns a rendered message so test fakes can
/// construct failures without needing to build a real `QdrantError`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct VectorStoreError(pub String);

impl From<QdrantError> for VectorStoreError {
    fn from(e: QdrantError) -> Self {
        VectorStoreError(e.to_string())
    }
}

/// The vector-store operations mindex performs, abstracted behind a trait so the
/// indexing handler, the search handler, and both workers can be unit-tested
/// against an in-memory fake instead of a live Qdrant. The production
/// implementation is `Qdrant` itself.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Creates the collection (dense + sparse + colbert vectors) if it is absent.
    async fn ensure_project(&self, collection: &str) -> Result<(), VectorStoreError>;

    /// Upserts a batch of chunk vectors into `collection`.
    async fn insert_batch(
        &self,
        collection: &str,
        chunks: Vec<ChunkAsVector>,
    ) -> Result<(), VectorStoreError>;

    /// Deletes the named points from `collection`.
    async fn delete_batch(
        &self,
        collection: &str,
        qdrant_guids: Vec<String>,
    ) -> Result<(), VectorStoreError>;

    /// Hybrid search: dense + sparse prefetch → RRF fusion → ColBERT MaxSim rerank,
    /// restricted to `chunk_ids` via a `has_id` filter, returning the top `top_k`.
    #[allow(clippy::too_many_arguments)] // irreducible inputs of one hybrid query
    async fn search(
        &self,
        collection: &str,
        chunk_ids: Vec<UUIDv4>,
        dense: Vec<f32>,
        sparse_indices: Vec<u32>,
        sparse_values: Vec<f32>,
        colbert: Vec<Vec<f32>>,
        top_k: u64,
    ) -> Result<Vec<SearchHit>, VectorStoreError>;
}

#[async_trait]
impl VectorStore for Qdrant {
    async fn ensure_project(&self, collection: &str) -> Result<(), VectorStoreError> {
        if self.collection_exists(collection).await? {
            return Ok(());
        }

        let mut vectors_config = VectorsConfigBuilder::default();

        vectors_config
            .add_named_vector_params("dense", VectorParamsBuilder::new(1024, Distance::Cosine));

        vectors_config.add_named_vector_params(
            "colbert",
            VectorParamsBuilder::new(1024, Distance::Cosine)
                .multivector_config(MultiVectorConfigBuilder::new(MultiVectorComparator::MaxSim)),
        );

        let mut sparse_config = SparseVectorsConfigBuilder::default();

        sparse_config.add_named_vector_params("sparse", SparseVectorParamsBuilder::default());

        self.create_collection(
            CreateCollectionBuilder::new(collection)
                .vectors_config(vectors_config)
                .sparse_vectors_config(sparse_config),
        )
        .await?;

        Ok(())
    }

    async fn insert_batch(
        &self,
        collection: &str,
        chunks: Vec<ChunkAsVector>,
    ) -> Result<(), VectorStoreError> {
        let points: Vec<PointStruct> = chunks.into_iter().map(|c| c.into()).collect();

        self.upsert_points(UpsertPointsBuilder::new(collection, points))
            .await?;

        Ok(())
    }

    async fn delete_batch(
        &self,
        collection: &str,
        qdrant_guids: Vec<String>,
    ) -> Result<(), VectorStoreError> {
        self.delete_points(DeletePointsBuilder::new(collection).points(qdrant_guids))
            .await?;

        Ok(())
    }

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
        let filter = Filter {
            must: vec![Condition {
                condition_one_of: Some(condition::ConditionOneOf::HasId(HasIdCondition {
                    has_id: chunk_ids
                        .into_iter()
                        .map(|UUIDv4(v4)| v4.simple().to_string())
                        .map(Into::into)
                        .collect(),
                })),
            }],
            ..Default::default()
        };

        let sparse_query: Vec<(u32, f32)> =
            sparse_indices.into_iter().zip(sparse_values).collect();

        // Two-stage retrieval, expressed as a *nested* prefetch — this nesting is
        // load-bearing. `QueryPointsBuilder` has a single `query` field, so two flat
        // `.query()` calls would make the second silently overwrite the first; the
        // RRF fusion would vanish and only the ColBERT rerank would run. Instead the
        // inner prefetch fuses dense+sparse (RRF) into a 200-candidate pool, and the
        // outer query reranks that pool with ColBERT MaxSim.
        let fusion_prefetch = PrefetchQueryBuilder::default()
            .prefetch(vec![
                PrefetchQueryBuilder::default()
                    .query(dense)
                    .using("dense")
                    .limit(200u32)
                    .filter(filter.clone())
                    .build(),
                PrefetchQueryBuilder::default()
                    .query(Query::from(sparse_query))
                    .using("sparse")
                    .limit(200u32)
                    .filter(filter.clone())
                    .build(),
            ])
            .query(Query::new_fusion(Fusion::Rrf))
            .limit(200u32)
            .build();

        let response = self
            .query(
                QueryPointsBuilder::new(collection)
                    .prefetch(vec![fusion_prefetch])
                    .query(colbert)
                    .using("colbert")
                    .limit(top_k)
                    .filter(filter)
                    .with_payload(false)
                    .with_vectors(false),
            )
            .await?;

        Ok(response
            .result
            .into_iter()
            .filter_map(|p| p.id.map(|id| SearchHit { id, score: p.score }))
            .collect())
    }
}
