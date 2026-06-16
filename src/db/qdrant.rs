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

pub async fn ensure_project(client: &Qdrant, project_guid: &str) -> Result<(), QdrantError> {
    if client.collection_exists(project_guid).await? {
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

    client
        .create_collection(
            CreateCollectionBuilder::new(project_guid)
                .vectors_config(vectors_config)
                .sparse_vectors_config(sparse_config),
        )
        .await?;

    Ok(())
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

pub async fn insert_batch(
    client: &Qdrant,
    collection: &str,
    chunks: Vec<ChunkAsVector>,
) -> Result<(), QdrantError> {
    let points: Vec<PointStruct> = chunks.into_iter().map(|c| c.into()).collect();

    client
        .upsert_points(UpsertPointsBuilder::new(collection, points))
        .await?;

    Ok(())
}

pub async fn delete_batch(
    client: &Qdrant,
    collection: &str,
    qdrant_guids: Vec<String>,
) -> Result<(), QdrantError> {
    client
        .delete_points(DeletePointsBuilder::new(collection).points(qdrant_guids))
        .await?;

    Ok(())
}

pub struct SearchHit {
    pub id: PointId,
    pub score: f32,
}

// The eight parameters are the irreducible inputs of one hybrid query (collection,
// id filter, the three vector modalities, and top_k); bundling them into a struct
// would only move the same fields around without improving clarity.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    client: &Qdrant,
    collection: &str,
    chunk_ids: Vec<UUIDv4>,
    dense: Vec<f32>,
    sparse_indices: Vec<u32>,
    sparse_values: Vec<f32>,
    colbert: Vec<Vec<f32>>,
    top_k: u64,
) -> Result<Vec<SearchHit>, QdrantError> {
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

    let sparse_query: Vec<(u32, f32)> = sparse_indices.into_iter().zip(sparse_values).collect();

    let response = client
        .query(
            QueryPointsBuilder::new(collection)
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
