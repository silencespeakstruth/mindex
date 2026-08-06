//! The compiled registry of embedding models mindex can run.
//!
//! One entry per canonical model id — the string `[model].id`, SQLite's
//! `embedding_models` table, `GET /config`, metrics labels and the research
//! journal all carry. The registry is the Rust half of a two-sided contract:
//! the SQLite table pins the same ids and dims with a `CHECK`, and startup
//! refuses to run unless the two sides agree (`verify_model_registry` in
//! `main.rs`), so a rebuilt binary can never silently reinterpret stored
//! vectors.
//!
//! Adding a model is deliberately a three-part change shipped in one commit:
//! a new entry here, a migration widening the table's `CHECK` (small-table
//! rebuild) plus its seed `INSERT`, and — that is all. Vectors for the new
//! model are produced by `mindex-index --vectors-only` (a pure re-embed over
//! stored chunks) as long as the new model shares a tokenizer with the one
//! that sliced them; a different tokenizer means a full reindex, and
//! `project_files.chunker_id` is what makes the difference detectable.

/// One embedding model mindex can be configured to serve. Compiled in, and
/// cross-checked against the `embedding_models` table at startup.
pub struct EmbeddingModelSpec {
    /// Canonical id: what config, SQLite, metrics and `GET /config` carry.
    pub id: &'static str,
    /// HF repo the serving side (vLLM) loads; the handshake expects it —
    /// or `[model].served_name` — in `GET /v1/models`.
    pub hf_repo: &'static str,
    /// Dense width. Validated against EVERY `/v1/embeddings` response row.
    pub dim: usize,
    /// Model context limit in tokens (32768 for the whole Qwen3 family).
    /// The slicer window and doc-chunk cap are validated against it.
    pub max_seq: usize,
    /// Prepended to QUERIES only; documents get nothing. The model is
    /// instruction-tuned and its card specifies `Instruct: {task}\nQuery:`
    /// on the query side — omitting it degrades retrieval silently.
    pub query_prefix: &'static str,
    /// Qdrant-name-safe short slug: collections are named
    /// `{guid_simple}_{collection_slug}_{schema_version}`.
    pub collection_slug: &'static str,
    /// Tokenizer the slicer loads. Identical for every Qwen3-Embedding size,
    /// which is what makes switching sizes a re-embed, never a re-slice.
    pub tokenizer_hf_id: &'static str,
}

/// Byte-for-byte the string in `bench/baselines/external_embedder.py` — the
/// archived 0.4540 nDCG@10 was measured WITH it, and a drifted prefix
/// degrades quality silently rather than failing. Do not reflow.
pub const QWEN3_QUERY_PREFIX: &str = "Instruct: Given a description of desired functionality, retrieve the source code that implements it\nQuery: ";

/// The tokenizer shared by every Qwen3-Embedding size. One identity string
/// means `project_files.chunker_id` matches across sizes, so a size switch
/// is `--vectors-only` and never a re-slice.
pub const QWEN3_TOKENIZER: &str = "Qwen/Qwen3-Embedding-0.6B";

/// Every model mindex can serve. Mirrored by the `embedding_models` table
/// (ids and dims), which `main.rs::verify_model_registry` enforces.
pub const EMBEDDING_MODELS: &[EmbeddingModelSpec] = &[
    EmbeddingModelSpec {
        id: "qwen3-embedding-0.6b",
        hf_repo: "Qwen/Qwen3-Embedding-0.6B",
        dim: 1024,
        max_seq: 32768,
        query_prefix: QWEN3_QUERY_PREFIX,
        collection_slug: "q3e06b",
        tokenizer_hf_id: QWEN3_TOKENIZER,
    },
    EmbeddingModelSpec {
        id: "qwen3-embedding-4b",
        hf_repo: "Qwen/Qwen3-Embedding-4B",
        dim: 2560,
        max_seq: 32768,
        query_prefix: QWEN3_QUERY_PREFIX,
        collection_slug: "q3e4b",
        tokenizer_hf_id: QWEN3_TOKENIZER,
    },
    EmbeddingModelSpec {
        id: "qwen3-embedding-8b",
        hf_repo: "Qwen/Qwen3-Embedding-8B",
        dim: 4096,
        max_seq: 32768,
        query_prefix: QWEN3_QUERY_PREFIX,
        collection_slug: "q3e8b",
        tokenizer_hf_id: QWEN3_TOKENIZER,
    },
];

/// Resolve a canonical id (`[model].id`) to its spec.
pub fn model_by_id(id: &str) -> Option<&'static EmbeddingModelSpec> {
    EMBEDDING_MODELS.iter().find(|m| m.id == id)
}

/// Resolve a collection-name slug back to its spec — the stale-collection
/// worker uses this to tell "a registered model that is not active" from a
/// name mindex never wrote.
pub fn model_by_slug(slug: &str) -> Option<&'static EmbeddingModelSpec> {
    EMBEDDING_MODELS.iter().find(|m| m.collection_slug == slug)
}

/// The list `[model].id` validation prints when the configured id is unknown.
pub fn known_ids() -> Vec<&'static str> {
    EMBEDDING_MODELS.iter().map(|m| m.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique() {
        let ids: HashSet<_> = EMBEDDING_MODELS.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), EMBEDDING_MODELS.len());
    }

    #[test]
    fn slugs_are_unique_and_collection_name_safe() {
        let slugs: HashSet<_> = EMBEDDING_MODELS.iter().map(|m| m.collection_slug).collect();
        assert_eq!(slugs.len(), EMBEDDING_MODELS.len());
        for m in EMBEDDING_MODELS {
            assert!(
                !m.collection_slug.is_empty()
                    && m.collection_slug.len() <= 16
                    && m.collection_slug
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "slug {:?} must be short lowercase alphanumeric — it is a collection-name component",
                m.collection_slug
            );
        }
    }

    #[test]
    fn dims_are_the_qwen3_family() {
        let dims: Vec<_> = EMBEDDING_MODELS.iter().map(|m| m.dim).collect();
        assert_eq!(dims, vec![1024, 2560, 4096]);
    }

    /// One tokenizer identity across every size is what makes a size switch a
    /// `--vectors-only` re-embed instead of a full re-slice. A second identity
    /// appearing here means `chunker_id` starts refusing that shortcut.
    #[test]
    fn every_size_shares_one_tokenizer() {
        for m in EMBEDDING_MODELS {
            assert_eq!(m.tokenizer_hf_id, QWEN3_TOKENIZER, "{} diverged", m.id);
        }
    }

    /// The prefix is pinned byte-for-byte against the string the benchmark
    /// measured with (`bench/baselines/external_embedder.py`). A reworded or
    /// reflowed prefix is not a style change: it silently degrades retrieval.
    #[test]
    fn the_query_prefix_is_pinned_byte_for_byte() {
        assert_eq!(
            QWEN3_QUERY_PREFIX,
            "Instruct: Given a description of desired functionality, retrieve the source code that implements it\nQuery: "
        );
    }

    #[test]
    fn lookups_resolve_and_unknowns_do_not() {
        assert_eq!(model_by_id("qwen3-embedding-0.6b").unwrap().dim, 1024);
        assert_eq!(model_by_slug("q3e8b").unwrap().id, "qwen3-embedding-8b");
        assert!(model_by_id("BAAI/bge-m3").is_none());
        assert!(model_by_slug("v2").is_none());
    }
}
