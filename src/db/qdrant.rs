use async_trait::async_trait;
use qdrant_client::Payload;
use qdrant_client::Qdrant;
use qdrant_client::QdrantError;
use qdrant_client::qdrant::Condition;
use qdrant_client::qdrant::CountPointsBuilder;
use qdrant_client::qdrant::CreateCollectionBuilder;
use qdrant_client::qdrant::DeletePointsBuilder;
use qdrant_client::qdrant::Distance;
use qdrant_client::qdrant::Filter;
use qdrant_client::qdrant::HasIdCondition;
use qdrant_client::qdrant::NamedVectors;
use qdrant_client::qdrant::PointId;
use qdrant_client::qdrant::PointStruct;
use qdrant_client::qdrant::QueryPointsBuilder;
use qdrant_client::qdrant::SearchParamsBuilder;
use qdrant_client::qdrant::UpsertPointsBuilder;
use qdrant_client::qdrant::Vector;
use qdrant_client::qdrant::VectorParamsBuilder;
use qdrant_client::qdrant::VectorsConfigBuilder;
use qdrant_client::qdrant::condition;
use tracing::warn;

use crate::backend::v0::models::UUIDv4;
use crate::models::registry::{EmbeddingModelSpec, model_by_slug};

/// Generation of the Qdrant collection *layout* — vector names, dimensions and
/// distance metrics — carried as the last component of every collection name.
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
/// (`mindex-index --force`, or `--vectors-only` when only the vectors changed) and
/// dropping the collections left at the old version.
///
/// What a bump is *not* any more is silent: [`crate::worker::stale::check_and_publish`] runs
/// at startup and hourly, names every project whose current-layout collection is
/// missing or empty, names every collection left behind at another layout, and
/// publishes both as gauges.
///
/// `v3` is the dense-only layout: one named vector `"dense"`, per-model width,
/// and — the change that made the version *and* the name grammar move together —
/// one collection per `(project, model)`, so the model's slug sits between the
/// guid and this token. The `v1`/`v2` layouts (dense + sparse + ColBERT, no slug)
/// classify as [`CollectionAge::Previous`].
const COLLECTION_SCHEMA_VERSION: &str = "v3";

/// Production [`VectorStore`] backed by a Qdrant client. Carries the active
/// model's dense width (from the registry, at construction) and the one
/// query-side knob; wrapping the external `Qdrant` rather than impl'ing the
/// trait on it directly is what lets the tuning travel with the store without
/// widening the trait's `search` signature for every test fake.
pub struct QdrantStore {
    client: Qdrant,
    /// Dense vector width — `spec.dim` of the active model. Every collection
    /// this store creates is this wide, which is exactly why collections are
    /// per-model: two widths cannot share one named vector.
    dim: u64,
    search_hnsw_ef: u64,
}

impl QdrantStore {
    pub fn new(client: Qdrant, dim: u64, search_hnsw_ef: u64) -> Self {
        Self {
            client,
            dim,
            search_hnsw_ef,
        }
    }
}

/// Collection name for a project GUID (simple form) under a model's slug:
/// `{guid}_{slug}_{version}`, e.g. `2f1c…b6a_q3e06b_v3`.
pub fn collection_name(project_guid_simple: &str, collection_slug: &str) -> String {
    format!("{project_guid_simple}_{collection_slug}_{COLLECTION_SCHEMA_VERSION}")
}

/// What a collection name found in the store is, relative to this build and the
/// active model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionAge {
    /// This layout, the **active** model's slug: the collection search serves.
    Current,
    /// This layout, a *different registered* model's slug — a deliberate
    /// per-model store, not garbage. Switching `[model].id` back reuses it, so
    /// it is named at `info!` and never in a "drop this" message.
    OtherModel,
    /// A mindex collection at some *previous* layout — the legacy
    /// `{guid}_v1`/`{guid}_v2` grammar, or a slugged name at another version —
    /// still holding the whole pre-bump index and reachable by nothing.
    Previous,
    /// Not a mindex collection name (including a slug the registry does not
    /// know — a name mindex never wrote). Qdrant may be shared, so these are
    /// counted by nothing and named by nothing: a check that told an operator
    /// to drop somebody else's data would be worse than the problem it reports.
    Foreign,
}

/// Classify a collection name as [`CollectionAge`], relative to `active_slug`.
///
/// The grammar is what [`collection_name`] writes — 32 lowercase hex, `_`, a
/// registered slug, `_`, the layout version — plus the legacy two-part
/// `{guid}_{version}` grammar of the v1/v2 layouts, which classifies as
/// `Previous`. Both halves of every component are checked, so a collection
/// merely *ending* in `_v3` is still `Foreign`.
pub fn classify_collection(name: &str, active_slug: &str) -> CollectionAge {
    fn is_simple_guid(s: &str) -> bool {
        s.len() == 32
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }
    fn is_version(s: &str) -> bool {
        s.len() >= 2 && s.starts_with('v') && s[1..].bytes().all(|b| b.is_ascii_digit())
    }

    let Some((guid, rest)) = name.split_once('_') else {
        return CollectionAge::Foreign;
    };
    if !is_simple_guid(guid) {
        return CollectionAge::Foreign;
    }
    match rest.split_once('_') {
        // `{guid}_{slug}_{version}` — the v3 grammar. A slug the registry does
        // not know is a name mindex never wrote, so it stays Foreign however
        // version-shaped its tail is.
        Some((slug, version)) if is_version(version) => {
            if model_by_slug(slug).is_none() {
                CollectionAge::Foreign
            } else if version != COLLECTION_SCHEMA_VERSION {
                CollectionAge::Previous
            } else if slug == active_slug {
                CollectionAge::Current
            } else {
                CollectionAge::OtherModel
            }
        }
        Some(_) => CollectionAge::Foreign,
        // `{guid}_{version}` — the legacy grammar; every layout that used it
        // is previous by definition.
        None if is_version(rest) => CollectionAge::Previous,
        None => CollectionAge::Foreign,
    }
}

/// Qdrant collection name for a project GUID under a model spec. Convenience
/// over `collection_name(&guid.0.as_simple().to_string(), spec.collection_slug)`.
pub fn collection_for(project_guid: UUIDv4, spec: &EmbeddingModelSpec) -> String {
    collection_name(
        &project_guid.0.as_simple().to_string(),
        spec.collection_slug,
    )
}

#[derive(Clone)]
pub struct ChunkAsVector {
    pub guid: UUIDv4,
    pub dense: Vec<f32>,
}

impl From<ChunkAsVector> for PointStruct {
    fn from(value: ChunkAsVector) -> Self {
        // One vector, still *named*: the cheap slot a future second leg (a
        // sparse head, a prose leg) re-enters through without a grammar change.
        let vectors = NamedVectors::default().add_vector("dense", Vector::from(value.dense));

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
    /// Creates the collection (one named `dense` vector) if it is absent.
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
    /// collections left behind at a *previous* layout, which are invisible from
    /// SQLite (nothing there records the layout a project's vectors were written
    /// under) — and, since collections became per-model, by the project delete,
    /// which must drop every model's collection and not just the active one.
    ///
    /// Provided, and `None` rather than an empty list, for the same reason as
    /// `count_points`: a fake that cannot enumerate must say so, because "no
    /// collections" is itself an alarming answer and must never be manufactured.
    async fn list_collections(&self) -> Result<Option<Vec<String>>, VectorStoreError> {
        Ok(None)
    }

    /// Dense search restricted to `chunk_ids` via a `has_id` filter, returning
    /// the top `top_k` by cosine score.
    async fn search(
        &self,
        collection: &str,
        chunk_ids: Vec<UUIDv4>,
        dense: Vec<f32>,
        top_k: u64,
    ) -> Result<Vec<SearchHit>, VectorStoreError>;
}

#[async_trait]
impl VectorStore for QdrantStore {
    async fn ensure_project(&self, collection: &str) -> Result<(), VectorStoreError> {
        if self.client.collection_exists(collection).await? {
            return Ok(());
        }

        // One named vector at Qdrant's defaults: HNSW as shipped, fp32, in the
        // page cache. The whole non-default zoo the v2 layout carried belonged
        // to ColBERT, and left with it — dense on this corpus is megabytes.
        let mut vectors_config = VectorsConfigBuilder::default();
        vectors_config.add_named_vector_params(
            "dense",
            VectorParamsBuilder::new(self.dim, Distance::Cosine),
        );

        // `collection_exists` + `create_collection` is not atomic: two concurrent
        // first-time `/index` calls for the same new project can both see "absent" and
        // both create, the loser getting "already exists". Treat that as success — the
        // collection we wanted is there. (This guard sits *before* the per-file claim,
        // so the claim can't serialize it.) Matched on the rendered message because the
        // client surfaces no typed "already exists" variant.
        if let Err(e) = self
            .client
            .create_collection(
                CreateCollectionBuilder::new(collection).vectors_config(vectors_config),
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

        // One dense query over the candidate set. The v2 layout's nested
        // prefetch tree (dense + sparse → RRF → ColBERT rerank) is gone with
        // its vectors: plain RRF measured BELOW the single dense leg it fused
        // (bench/FINDINGS.md §10.3), and ColBERT was never measured to help.
        let response = self
            .client
            .query(
                QueryPointsBuilder::new(collection)
                    .query(dense)
                    .using("dense")
                    .limit(top_k)
                    .filter(filter)
                    .params(SearchParamsBuilder::default().hnsw_ef(self.search_hnsw_ef))
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
    use crate::models::registry::{EMBEDDING_MODELS, model_by_id};
    use uuid::Uuid;

    fn active() -> &'static EmbeddingModelSpec {
        model_by_id("qwen3-embedding-0.6b").expect("registered")
    }

    /// Two (project, model) pairs must never name the same collection — that is
    /// the outer half of project isolation, the `has_id` candidate filter being
    /// the inner one, and since v3 also the whole model-isolation mechanism:
    /// two dense widths cannot share a named vector, so they must not share a
    /// collection. Built from the **simple** (hyphen-less) guid form: it is
    /// what the schema stores and what every caller derives.
    #[test]
    fn a_collection_is_named_from_the_simple_guid_the_slug_and_the_version() {
        let guid = Uuid::parse_str("2f1c9a70-4d3e-4e9b-8b6a-1f2e3d4c5b6a").expect("a uuid");

        assert_eq!(
            collection_for(UUIDv4(guid), active()),
            format!("2f1c9a704d3e4e9b8b6a1f2e3d4c5b6a_q3e06b_{COLLECTION_SCHEMA_VERSION}"),
            "the hyphenated form must never reach a collection name"
        );
        assert_eq!(
            collection_for(UUIDv4(guid), active()),
            collection_name(&guid.as_simple().to_string(), active().collection_slug),
            "the convenience wrapper and the raw builder must agree; the GC sweep \
             and the metrics probe use one, the search path the other"
        );
    }

    /// Pins the version itself, not just its presence in the name.
    ///
    /// A bump breaks every existing project's search until it is reindexed, and does
    /// it without failing anything, so it must never be a side effect of an edit to
    /// the vector params above it. `v3` carries the dense-only per-model layout;
    /// changing the layout without changing this leaves old collections silently
    /// answering new-layout queries.
    #[test]
    fn the_collection_schema_version_is_pinned() {
        assert_eq!(
            COLLECTION_SCHEMA_VERSION, "v3",
            "the collection layout changed. Every project must be reindexed \
             (mindex-index --force, or --vectors-only for a pure vector change) \
             and the previous version's collections dropped; see docs/claude/qdrant.md"
        );
    }

    /// The stale-collection check and the project delete both act on this
    /// classification: `Current` is served, `OtherModel` is held (a registered
    /// model that is not active — switching back reuses it), `Previous` is
    /// named in a message telling an operator to delete it, and `Foreign` is
    /// never named at all. Qdrant may be shared, so a name mistaken for
    /// `Previous` is a message telling somebody to delete another service's data.
    #[test]
    fn the_classification_matrix_holds() {
        let guid = "a".repeat(32);
        let active_slug = active().collection_slug;

        assert_eq!(
            classify_collection(&collection_name(&guid, active_slug), active_slug),
            CollectionAge::Current
        );
        // Another *registered* model's v3 collection: deliberate, held.
        assert_eq!(
            classify_collection(&format!("{guid}_q3e8b_v3"), active_slug),
            CollectionAge::OtherModel
        );
        // The legacy grammars — every pre-v3 layout.
        assert_eq!(
            classify_collection(&format!("{guid}_v2"), active_slug),
            CollectionAge::Previous
        );
        assert_eq!(
            classify_collection(&format!("{guid}_v1"), active_slug),
            CollectionAge::Previous
        );
        // A slugged name at a non-current version.
        assert_eq!(
            classify_collection(&format!("{guid}_q3e06b_v4"), active_slug),
            CollectionAge::Previous
        );

        for foreign in [
            // A slug the registry does not know: a name mindex never wrote.
            format!("{guid}_notaslug_v3"),
            // The right shape, wrong alphabet.
            format!("{}_v1", "z".repeat(32)),
            // Uppercase hex: `collection_name` never writes it.
            format!("{}_q3e06b_v3", "A".repeat(32)),
            // The right suffix, no guid.
            "someone_elses_v1".to_string(),
            // A guid, no version.
            guid.clone(),
            // A guid with a trailing non-version tail.
            format!("{guid}_backup"),
            String::new(),
        ] {
            assert_eq!(
                classify_collection(&foreign, active_slug),
                CollectionAge::Foreign,
                "{foreign} was claimed as a mindex collection"
            );
        }
    }

    /// Distinct projects never share a collection, and neither do distinct
    /// models of one project — the per-model store is what makes a model switch
    /// reversible and a future model addition cheap.
    #[test]
    fn distinct_project_model_pairs_never_share_a_collection() {
        let a = UUIDv4(Uuid::from_u128(1));
        let b = UUIDv4(Uuid::from_u128(2));
        assert_ne!(collection_for(a, active()), collection_for(b, active()));
        // Same project, different registered models.
        let m06 = model_by_id("qwen3-embedding-0.6b").unwrap();
        let m8 = model_by_id("qwen3-embedding-8b").unwrap();
        assert_ne!(collection_for(a, m06), collection_for(a, m8));
        // And the same pair always names the same collection, or a reindex
        // would write somewhere the search path never looks.
        assert_eq!(collection_for(a, m06), collection_for(a, m06));
    }

    /// Every registered model's collection name must classify as a mindex
    /// collection — a slug that fails its own grammar would make that model's
    /// collections invisible to the stale check and to the project delete.
    #[test]
    fn every_registered_slug_round_trips_through_classification() {
        let guid = "b".repeat(32);
        for spec in EMBEDDING_MODELS {
            let name = collection_name(&guid, spec.collection_slug);
            let age = classify_collection(&name, spec.collection_slug);
            assert_eq!(age, CollectionAge::Current, "{name} did not round-trip");
        }
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
