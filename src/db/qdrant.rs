use async_trait::async_trait;
use qdrant_client::Payload;
use qdrant_client::Qdrant;
use qdrant_client::QdrantError;
use qdrant_client::qdrant::Condition;
use qdrant_client::qdrant::CountPointsBuilder;
use qdrant_client::qdrant::CreateCollectionBuilder;
use qdrant_client::qdrant::Datatype;
use qdrant_client::qdrant::DeletePointsBuilder;
use qdrant_client::qdrant::Distance;
use qdrant_client::qdrant::Filter;
use qdrant_client::qdrant::Fusion;
use qdrant_client::qdrant::HasIdCondition;
use qdrant_client::qdrant::HnswConfigDiffBuilder;
use qdrant_client::qdrant::MultiVectorComparator;
use qdrant_client::qdrant::MultiVectorConfigBuilder;
use qdrant_client::qdrant::NamedVectors;
use qdrant_client::qdrant::PointId;
use qdrant_client::qdrant::PointStruct;
use qdrant_client::qdrant::PrefetchQueryBuilder;
use qdrant_client::qdrant::Query;
use qdrant_client::qdrant::QueryPointsBuilder;
use qdrant_client::qdrant::SearchParamsBuilder;
use qdrant_client::qdrant::SparseVectorParamsBuilder;
use qdrant_client::qdrant::SparseVectorsConfigBuilder;
use qdrant_client::qdrant::UpsertPointsBuilder;
use qdrant_client::qdrant::Vector;
use qdrant_client::qdrant::VectorParamsBuilder;
use qdrant_client::qdrant::VectorsConfigBuilder;
use qdrant_client::qdrant::condition;
use tracing::warn;

use crate::backend::v0::models::UUIDv4;

/// Generation of the Qdrant collection *layout* — vector names, dimensions and
/// distance metrics — carried as a suffix on every collection name.
///
/// A single token, not the `MAJOR.MINOR` string the other internal versions use
/// (see [`CHUNKS_DERIVATION_VERSION`](crate::slicing::traits::CHUNKS_DERIVATION_VERSION)):
/// this is a name component, never a compared value, and a dot in a collection
/// name buys nothing.
///
/// **A bump is not self-healing.** The new name simply names no collection,
/// [`VectorStore::ensure_project`] creates an empty one, and SQLite goes on reporting
/// every file `indexed` — the prepare-phase skip does not look at the collection
/// layout. Search then returns nothing. Bumping it means reindexing every project
/// (`mindex-index --force`) and dropping the collections left at the old version.
///
/// What a bump is *not* any more is silent: [`crate::worker::stale::check_and_publish`] runs
/// at startup and hourly, names every project whose current-version collection is
/// missing or empty, names every collection left behind at another version, and
/// publishes both as gauges. That check exists because this was for a long time the
/// one version with no mismatch detection at all, and its failure mode — a healthy
/// service answering "no match" for ever, with no error anywhere — is the worst
/// shape a failure can take.
const COLLECTION_SCHEMA_VERSION: &str = "v2";

/// Dense / ColBERT vector width. **Structural, not configurable**: it is dictated by
/// the BGE-M3 model and baked into every collection's schema — changing it without a
/// matching model + collection rebuild silently breaks search.
pub(crate) const VECTOR_DIM: u64 = 1024;

/// Element type of the stored ColBERT multivector.
///
/// **Structural, not a tuning knob**: it is part of the collection's schema, so it
/// cannot change without a [`COLLECTION_SCHEMA_VERSION`] bump and a full reindex —
/// exactly what a config key would invite an operator to do by accident.
///
/// ColBERT is why this matters at all. It emits one 1024-wide row *per token*, so a
/// chunk costs hundreds of rows where dense costs one: measured on this repo's own
/// index, `vector_storage-colbert` was 838 MB per segment against 2.6 MB for dense
/// and 0.5 MB for sparse — **99.6% of the bytes**, ~1.85 MB per chunk. Halving the
/// element halves the whole store, and it is not a quality trade in the way
/// quantization would be: fp16 carries ~3 decimal digits, and this vector is only
/// ever used to *order* a pool of `fusion_limit` candidates that dense and sparse
/// already agreed were relevant.
const COLBERT_DATATYPE: Datatype = Datatype::Float16;

/// HNSW graph degree for the ColBERT vector — `0`, meaning **build no graph**.
///
/// Structural for the same reason as [`COLBERT_DATATYPE`]. Correct only because of an
/// invariant of [`VectorStore::search`]: the ColBERT stage is always the *outer*
/// query over a prefetch pool, never an entry point, so Qdrant rescores an explicit
/// candidate list and never traverses a graph. Building one was pure indexing cost
/// on the most expensive vector in the collection.
///
/// **The trap this sets:** a future query that used `.using("colbert")` *without* a
/// prefetch would not fail — it would silently brute-force the entire collection.
/// If ColBERT ever needs to be searched directly, this must go back to a real degree
/// and the schema version must be bumped with it.
const COLBERT_HNSW_M: u64 = 0;

/// Production [`VectorStore`] backed by a Qdrant client plus the retrieval prefetch
/// limits from `[qdrant]` config. Wrapping the external `Qdrant` (rather than impl'ing
/// the trait on it directly) is what lets the tuning travel with the store without
/// widening the trait's `search` signature for every test fake.
pub struct QdrantStore {
    client: Qdrant,
    dense_prefetch_limit: u32,
    sparse_prefetch_limit: u32,
    fusion_limit: u32,
    search_hnsw_ef: u64,
}

impl QdrantStore {
    pub fn new(
        client: Qdrant,
        dense_prefetch_limit: u32,
        sparse_prefetch_limit: u32,
        fusion_limit: u32,
        search_hnsw_ef: u64,
    ) -> Self {
        Self {
            client,
            dense_prefetch_limit,
            sparse_prefetch_limit,
            fusion_limit,
            search_hnsw_ef,
        }
    }
}

pub fn collection_name(project_guid_simple: &str) -> String {
    format!("{}_{}", project_guid_simple, COLLECTION_SCHEMA_VERSION)
}

/// What a collection name found in the store is, relative to this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionAge {
    /// A mindex collection at [`COLLECTION_SCHEMA_VERSION`].
    Current,
    /// A mindex collection at some *other* version — the store left behind by a bump,
    /// still holding every byte of the pre-bump index and reachable by nothing.
    Previous,
    /// Not a mindex collection name. Qdrant may be shared, so these are counted by
    /// nothing and named by nothing: a check that told an operator to drop somebody
    /// else's data would be worse than the problem it reports.
    Foreign,
}

/// Classify a collection name as [`CollectionAge`].
///
/// The name grammar is the one [`collection_name`] writes: a project GUID in simple
/// form (32 lowercase hex) then `_` then the schema version. Both halves are checked,
/// so a collection merely *ending* in `_v2` is still `Foreign`.
pub fn classify_collection(name: &str) -> CollectionAge {
    let Some((guid, version)) = name.rsplit_once('_') else {
        return CollectionAge::Foreign;
    };
    if guid.len() != 32
        || !guid
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return CollectionAge::Foreign;
    }
    if version == COLLECTION_SCHEMA_VERSION {
        CollectionAge::Current
    } else {
        CollectionAge::Previous
    }
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

    /// Drops the whole collection. Idempotent: a missing collection is a no-op,
    /// so a repeated project delete (or one racing the GC) does not error.
    async fn delete_collection(&self, collection: &str) -> Result<(), VectorStoreError>;

    /// Liveness ping of Qdrant itself (used by the mindex `/health` endpoint).
    async fn health(&self) -> Result<(), VectorStoreError>;

    /// Points currently in `collection`, or `None` when this store cannot say.
    ///
    /// The one detector for the failure documented at the top of this file: a lost
    /// Qdrant volume (or a bumped `COLLECTION_SCHEMA_VERSION`) leaves SQLite reporting
    /// every file `indexed` while `ensure_project` quietly makes an empty collection,
    /// so search answers `404 search.no_match` for ever with **no error anywhere**.
    /// Compared against `project_chunks_active`, a count is the difference between
    /// "this project is empty" and "this project's vectors are gone".
    ///
    /// Provided rather than required so the test fakes — which have no notion of point
    /// counts — opt out by saying so, rather than by inventing a number that would be
    /// compared against a real one.
    async fn count_points(&self, collection: &str) -> Result<Option<u64>, VectorStoreError> {
        let _ = collection;
        Ok(None)
    }

    /// Every collection this store holds, or `None` when it cannot enumerate them.
    ///
    /// Read by [`crate::worker::stale::check_and_publish`] for the half of its check that
    /// [`count_points`](VectorStore::count_points) cannot answer: naming the
    /// collections left behind at a *previous* [`COLLECTION_SCHEMA_VERSION`], which
    /// are invisible from SQLite (nothing there records the layout a project's
    /// vectors were written under) and which go on holding the whole pre-bump store
    /// until an operator drops them.
    ///
    /// Provided, and `None` rather than an empty list, for the same reason as
    /// `count_points`: a fake that cannot enumerate must say so, because "no
    /// collections" is itself an alarming answer and must never be manufactured.
    async fn list_collections(&self) -> Result<Option<Vec<String>>, VectorStoreError> {
        Ok(None)
    }

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
impl VectorStore for QdrantStore {
    async fn ensure_project(&self, collection: &str) -> Result<(), VectorStoreError> {
        if self.client.collection_exists(collection).await? {
            return Ok(());
        }

        let mut vectors_config = VectorsConfigBuilder::default();

        vectors_config.add_named_vector_params(
            "dense",
            VectorParamsBuilder::new(VECTOR_DIM, Distance::Cosine),
        );

        // The three non-default options all belong to ColBERT, which is 99.6% of the
        // bytes in a collection; `dense` is 2.6 MB per segment and is the vector the
        // prefetch actually searches, so it stays at Qdrant's defaults deliberately.
        // See each const for why it is structural rather than configurable.
        vectors_config.add_named_vector_params(
            "colbert",
            VectorParamsBuilder::new(VECTOR_DIM, Distance::Cosine)
                .multivector_config(MultiVectorConfigBuilder::new(MultiVectorComparator::MaxSim))
                .datatype(COLBERT_DATATYPE)
                // Never held in RAM. It is read for `fusion_limit` candidates per
                // query and nothing else, so page cache is the right place for it —
                // and the explicit flag stops a future Qdrant default from putting
                // multiple gigabytes per project back into the heap.
                .on_disk(true)
                .hnsw_config(HnswConfigDiffBuilder::default().m(COLBERT_HNSW_M)),
        );

        let mut sparse_config = SparseVectorsConfigBuilder::default();

        sparse_config.add_named_vector_params("sparse", SparseVectorParamsBuilder::default());

        // `collection_exists` + `create_collection` is not atomic: two concurrent
        // first-time `/index` calls for the same new project can both see "absent" and
        // both create, the loser getting "already exists". Treat that as success — the
        // collection we wanted is there. (This guard sits *before* the per-file claim,
        // so the claim can't serialize it.) Matched on the rendered message because the
        // client surfaces no typed "already exists" variant.
        if let Err(e) = self
            .client
            .create_collection(
                CreateCollectionBuilder::new(collection)
                    .vectors_config(vectors_config)
                    .sparse_vectors_config(sparse_config),
            )
            .await
        {
            let err: VectorStoreError = e.into();
            if !err.0.contains("already exists") {
                return Err(err);
            }
        }

        Ok(())
    }

    async fn insert_batch(
        &self,
        collection: &str,
        chunks: Vec<ChunkAsVector>,
    ) -> Result<(), VectorStoreError> {
        let points: Vec<PointStruct> = chunks.into_iter().map(|c| c.into()).collect();

        self.client
            .upsert_points(UpsertPointsBuilder::new(collection, points))
            .await?;

        Ok(())
    }

    async fn delete_batch(
        &self,
        collection: &str,
        qdrant_guids: Vec<String>,
    ) -> Result<(), VectorStoreError> {
        let attempt = self
            .client
            .delete_points(DeletePointsBuilder::new(collection).points(qdrant_guids))
            .await;
        let Err(err) = attempt else {
            return Ok(());
        };

        // A **missing collection** is a success, like it is for `delete_collection`
        // and `count_points` beside it: the vectors this was asked to remove are
        // demonstrably not there. Reported as a failure it was worse than cosmetic —
        // GC's rule is to keep a chunk row until its vector is confirmed gone, so
        // those rows could never be swept, their `deleted` file rows could never be
        // pruned behind the RESTRICT FK, and the backlog grew for the life of the
        // deployment. That is precisely the state a lost Qdrant volume leaves behind,
        // i.e. the one where GC needs to work most.
        //
        // Checked only after a failure, so the ordinary path pays no extra round
        // trip. And **only a definitive `false` converts**: if `collection_exists`
        // itself fails — an unreachable Qdrant — the original error stands. Reading
        // "I could not ask" as "it is not there" would hard-delete SQLite rows whose
        // vectors are still present, orphaning them with nothing left to track them,
        // which is the exact failure the confirm-before-delete rule exists to prevent.
        match self.client.collection_exists(collection).await {
            Ok(false) => {
                warn!(
                    collection,
                    "Qdrant has no such collection, so the vectors this delete would \
                     have removed are already gone; treating it as done. Sysadmin: if \
                     this project should have vectors, its collection has been lost — \
                     compare mindex_project_vectors against mindex_project_chunks_active \
                     and reindex."
                );
                Ok(())
            }
            _ => Err(err.into()),
        }
    }

    async fn delete_collection(&self, collection: &str) -> Result<(), VectorStoreError> {
        // Idempotent: skip if it never existed (e.g. a project deleted before any
        // file was indexed, or a repeated DELETE). Qualified call avoids resolving
        // back into this trait method.
        if !self.client.collection_exists(collection).await? {
            return Ok(());
        }
        self.client.delete_collection(collection).await?;
        Ok(())
    }

    async fn health(&self) -> Result<(), VectorStoreError> {
        self.client.health_check().await?;
        Ok(())
    }

    async fn count_points(&self, collection: &str) -> Result<Option<u64>, VectorStoreError> {
        // A missing collection is `Ok(Some(0))`, not an error: "the vectors are gone"
        // is precisely the answer being asked for, and raising here would make the
        // caller treat the interesting case as an unreachable Qdrant.
        if !self.client.collection_exists(collection).await? {
            return Ok(Some(0));
        }
        // Approximate: this runs on a metrics tick against every project, and an exact
        // count walks the collection. The number is compared against SQLite to spot an
        // order-of-magnitude divergence, not to reconcile row by row.
        let resp = self
            .client
            .count(CountPointsBuilder::new(collection).exact(false))
            .await?;
        Ok(resp.result.map(|r| r.count))
    }

    async fn list_collections(&self) -> Result<Option<Vec<String>>, VectorStoreError> {
        let resp = self.client.list_collections().await?;
        Ok(Some(resp.collections.into_iter().map(|c| c.name).collect()))
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

        let sparse_query: Vec<(u32, f32)> = sparse_indices.into_iter().zip(sparse_values).collect();

        // Two-stage retrieval, expressed as a *nested* prefetch — this nesting is
        // load-bearing. `QueryPointsBuilder` has a single `query` field, so two flat
        // `.query()` calls would make the second silently overwrite the first; the
        // RRF fusion would vanish and only the ColBERT rerank would run. Instead the
        // inner prefetch fuses dense+sparse (RRF) into a `fusion_limit`-candidate pool,
        // and the outer query reranks that pool with ColBERT MaxSim. The prefetch /
        // fusion limits come from `[qdrant]` config.
        let fusion_prefetch = PrefetchQueryBuilder::default()
            .prefetch(vec![
                PrefetchQueryBuilder::default()
                    .query(dense)
                    .using("dense")
                    .limit(self.dense_prefetch_limit)
                    .filter(filter.clone())
                    // The only stage `hnsw_ef` reaches: sparse is served by an
                    // inverted index and the ColBERT stage builds no graph at all
                    // (`COLBERT_HNSW_M`), so both would ignore it.
                    .params(SearchParamsBuilder::default().hnsw_ef(self.search_hnsw_ef))
                    .build(),
                PrefetchQueryBuilder::default()
                    .query(Query::from(sparse_query))
                    .using("sparse")
                    .limit(self.sparse_prefetch_limit)
                    .filter(filter.clone())
                    .build(),
            ])
            .query(Query::new_fusion(Fusion::Rrf))
            .limit(self.fusion_limit)
            .build();

        let response = self
            .client
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Two projects must never name the same collection — that is the outer half of
    /// project isolation, the `has_id` candidate filter being the inner one. And the
    /// name must be built from the **simple** (hyphen-less) form: it is what the
    /// schema stores, what `collection_name` is handed by the metrics probe and the
    /// GC sweep, and what `collection_for` derives for the search path. The two
    /// spellings disagreeing would send a search to a collection nothing indexes
    /// into — an empty result set, for ever, with no error anywhere.
    #[test]
    fn a_collection_is_named_from_the_simple_guid_and_the_schema_version() {
        let guid = Uuid::parse_str("2f1c9a70-4d3e-4e9b-8b6a-1f2e3d4c5b6a").expect("a uuid");

        assert_eq!(
            collection_for(UUIDv4(guid)),
            format!("2f1c9a704d3e4e9b8b6a1f2e3d4c5b6a_{COLLECTION_SCHEMA_VERSION}"),
            "the hyphenated form must never reach a collection name"
        );
        assert_eq!(
            collection_for(UUIDv4(guid)),
            collection_name(&guid.as_simple().to_string()),
            "the convenience wrapper and the raw builder must agree; the GC sweep \
             and the metrics probe use one, the search path the other"
        );
    }

    /// A name carries its schema version, which is what makes a bump a rename. That
    /// is the *whole* mechanism — a bumped version names no existing collection, so
    /// `ensure_collection` makes an empty one and every search answers 404 with no
    /// error. Pinned so the suffix cannot quietly stop being part of the name.
    #[test]
    fn the_schema_version_is_part_of_every_collection_name() {
        let name = collection_name("a".repeat(32).as_str());
        assert!(
            name.ends_with(&format!("_{COLLECTION_SCHEMA_VERSION}")),
            "{name} carries no schema version, so a bump would silently reuse the \
             old collection instead of naming a new one"
        );
        assert_eq!(name.len(), 32 + 1 + COLLECTION_SCHEMA_VERSION.len());
    }

    /// Pins the version itself, not just its presence in the name.
    ///
    /// A bump breaks every existing project's search until it is reindexed, and does
    /// it without failing anything, so it must never be a side effect of an edit to
    /// the vector params above it. `v2` carries the fp16 ColBERT with no HNSW graph;
    /// changing either of those without changing this leaves the two disagreeing,
    /// with old-layout collections silently answering new-layout queries.
    #[test]
    fn the_collection_schema_version_is_pinned() {
        assert_eq!(
            COLLECTION_SCHEMA_VERSION, "v2",
            "the collection layout changed. Every project must be reindexed \
             (mindex-index --force) and the previous version's collections dropped; \
             see docs/claude/qdrant.md"
        );
    }

    /// The stale-collection check acts on this classification: `Current` is left
    /// alone, `Previous` is named in a message telling an operator to delete it, and
    /// `Foreign` is never named at all. Qdrant may be shared, so a name mistaken for
    /// `Previous` is a message telling somebody to delete another service's data.
    #[test]
    fn only_a_mindex_collection_name_is_classified() {
        let guid = "a".repeat(32);
        assert_eq!(
            classify_collection(&collection_name(&guid)),
            CollectionAge::Current
        );
        assert_eq!(
            classify_collection(&format!("{guid}_v1")),
            CollectionAge::Previous
        );

        for foreign in [
            // The right shape, wrong alphabet.
            &format!("{}_v1", "z".repeat(32)),
            // Uppercase hex: `collection_name` never writes it, so a collection
            // spelled this way was created by something else.
            &format!("{}_v1", "A".repeat(32)),
            // The right suffix, no guid.
            "someone_elses_v1".to_string().as_str(),
            // A guid, no version.
            &guid,
            "",
        ] {
            assert_eq!(
                classify_collection(foreign),
                CollectionAge::Foreign,
                "{foreign} was claimed as a mindex collection"
            );
        }
    }

    /// Distinct projects, distinct collections — including two guids differing in
    /// one nibble, which is what a truncating or hashing name would collapse.
    #[test]
    fn distinct_projects_never_share_a_collection() {
        let a = UUIDv4(Uuid::from_u128(1));
        let b = UUIDv4(Uuid::from_u128(2));
        assert_ne!(collection_for(a), collection_for(b));
        // And the same project always names the same collection, or a reindex would
        // write somewhere the search path never looks.
        assert_eq!(collection_for(a), collection_for(a));
    }

    /// `count_points` is a **provided** method that declines by default, so every
    /// fake in the tree opts out without a line of code. That is deliberate, and it
    /// is also why `MeteredVectorStore` overriding it is load-bearing: a store that
    /// declines publishes no `project_vectors` gauge at all, which is correct for a
    /// fake and would silently disable the lost-volume detector in production.
    #[tokio::test]
    async fn the_default_count_declines_rather_than_answering_zero() {
        struct Minimal;

        #[async_trait]
        impl VectorStore for Minimal {
            async fn ensure_project(&self, _c: &str) -> Result<(), VectorStoreError> {
                Ok(())
            }
            async fn insert_batch(
                &self,
                _c: &str,
                _v: Vec<ChunkAsVector>,
            ) -> Result<(), VectorStoreError> {
                Ok(())
            }
            async fn delete_batch(
                &self,
                _c: &str,
                _g: Vec<String>,
            ) -> Result<(), VectorStoreError> {
                Ok(())
            }
            async fn delete_collection(&self, _c: &str) -> Result<(), VectorStoreError> {
                Ok(())
            }
            async fn health(&self) -> Result<(), VectorStoreError> {
                Ok(())
            }
            async fn search(
                &self,
                _c: &str,
                _ids: Vec<UUIDv4>,
                _d: Vec<f32>,
                _si: Vec<u32>,
                _sv: Vec<f32>,
                _cb: Vec<Vec<f32>>,
                _k: u64,
            ) -> Result<Vec<SearchHit>, VectorStoreError> {
                Ok(vec![])
            }
        }

        assert_eq!(
            Minimal
                .count_points("anything")
                .await
                .expect("declining is not an error"),
            None,
            "the default must decline, never answer zero — zero is the alarming \
             value and would page somebody about a healthy index"
        );
    }
}
