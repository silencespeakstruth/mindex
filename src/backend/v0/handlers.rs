use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use super::models::IndexRequest;
use crate::backend::error::ApiError;
use crate::backend::error::ProblemDetails;
use crate::backend::extract::{
    ActiveRunsScope, AdminScope, ApiJson, ApiPath, ApiQuery, DeleteScope, DriftScope, IndexScope,
    ListProjectsScope, MintScope, ResearchScope, SearchScope,
};
use crate::backend::http3;
use crate::backend::http3::EmbeddingModel;
use crate::backend::http3::RouterState;
use crate::backend::v0::models::ActiveResearchResponse;
use crate::backend::v0::models::ActiveResearchRun;
use crate::backend::v0::models::CancelRequest;
use crate::backend::v0::models::CancelResponse;
use crate::backend::v0::models::ChallengeRequest;
use crate::backend::v0::models::CheckState;
use crate::backend::v0::models::ChunkExcerpt;
use crate::backend::v0::models::CitationCounts;
use crate::backend::v0::models::Code;
use crate::backend::v0::models::CommitSummary;
use crate::backend::v0::models::ConfigResponse;
use crate::backend::v0::models::DeleteFilesRequest;
use crate::backend::v0::models::DeleteFilesResponse;
use crate::backend::v0::models::DescriptorAuthentication;
use crate::backend::v0::models::DriftRequest;
use crate::backend::v0::models::DriftResponse;
use crate::backend::v0::models::FileHistoryResponse;
use crate::backend::v0::models::FileInfo;
use crate::backend::v0::models::FileListQuery;
use crate::backend::v0::models::FileListResponse;
use crate::backend::v0::models::FileListing;
use crate::backend::v0::models::FileStatusCounts;
use crate::backend::v0::models::GcResponse;
use crate::backend::v0::models::HealthChecks;
use crate::backend::v0::models::HealthResponse;
use crate::backend::v0::models::HistoryResponse;
use crate::backend::v0::models::IndexEvent;
use crate::backend::v0::models::IndexQuery;
use crate::backend::v0::models::IndexResponse;
use crate::backend::v0::models::LanguageStats;
use crate::backend::v0::models::ListFilesResponse;
use crate::backend::v0::models::MintTokenRequest;
use crate::backend::v0::models::MintTokenResponse;
use crate::backend::v0::models::OutlineResponse;
use crate::backend::v0::models::OutlineSymbol;
use crate::backend::v0::models::ProgrammingLanguage;
use crate::backend::v0::models::ProjectListResponse;
use crate::backend::v0::models::ProjectStats;
use crate::backend::v0::models::ProjectSummary;
use crate::backend::v0::models::ReadChunksResponse;
use crate::backend::v0::models::ResearchCompleteness;
use crate::backend::v0::models::ResearchCorpusTotals;
use crate::backend::v0::models::ResearchEffortInfo;
use crate::backend::v0::models::ResearchFreshness;
use crate::backend::v0::models::ResearchHealth;
use crate::backend::v0::models::ResearchKind;
use crate::backend::v0::models::ResearchListQuery;
use crate::backend::v0::models::ResearchObservedEffort;
use crate::backend::v0::models::ResearchObservedInfo;
use crate::backend::v0::models::ResearchPinRequest;
use crate::backend::v0::models::ResearchRequest;
use crate::backend::v0::models::ResearchRunDependency;
use crate::backend::v0::models::ResearchRunDetail;
use crate::backend::v0::models::ResearchRunFile;
use crate::backend::v0::models::ResearchRunListResponse;
use crate::backend::v0::models::ResearchRunSummary;
use crate::backend::v0::models::ResearchVerification;
use crate::backend::v0::models::RetryRequest;
use crate::backend::v0::models::RetryResponse;
use crate::backend::v0::models::SearchFilter;
use crate::backend::v0::models::SearchRequest;
use crate::backend::v0::models::SearchResponse;
use crate::backend::v0::models::SearchResult;
use crate::backend::v0::models::SkipReason;
use crate::backend::v0::models::StatusResponse;
use crate::backend::v0::models::StreamChoice;
use crate::backend::v0::models::SymbolInfo;
use crate::backend::v0::models::SymbolsRequest;
use crate::backend::v0::models::SymbolsResponse;
use crate::backend::v0::models::UUIDv4;
use crate::backend::v0::models::VersionResponse;
use crate::backend::v0::models::{DeleteResearchRunsRequest, DeleteResearchRunsResponse};
use crate::backend::v0::models::{
    DescriptorDocuments, DescriptorEndpoint, DescriptorTransport, MindexDescriptor,
};
use crate::backend::v0::models::{GrepMatch, GrepResponse};
use crate::backend::v0::models::{HistoryPruneQuery, HistoryPruneResponse, HistoryRequest};
use crate::backend::v0::models::{
    ResearchConfigInfo, ResearchEffortLadder, ResearchSamplingInfo, SearchConfigInfo,
};
use crate::backend::v0::validate;
use crate::db::files::set_file_status;
use crate::db::qdrant::SearchHit;
use crate::db::qdrant::VectorStore;
use crate::db::qdrant::collection_for;
use crate::db::sqlite3::SQLite3Pool;
use crate::db::sqlite3::SQLite3PoolError;
use crate::embed::EmbedProgress;
use crate::embed::EmbedUpsertError;
use crate::embed::embed_and_upsert;
use crate::models::bge_m3::BGEm3EmbedRequest;
use crate::models::bge_m3::BGEm3EmbedResponse;
use crate::models::bge_m3::BGEm3Model;
use crate::models::bge_m3::EncodeError;
use crate::slicing::markdown::MarkdownSlicer;
use crate::slicing::symbols::SYMBOLS_DERIVATION_VERSION;
use crate::slicing::symbols::SymbolError;
use crate::slicing::symbols::SymbolExtractor;
use crate::slicing::traits::CHUNKS_DERIVATION_VERSION;
use crate::slicing::traits::SlicedChunk;
use crate::slicing::traits::Slicer;
use crate::slicing::traits::SlicerError;
use axum::Json;
use axum::debug_handler;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use qdrant_client::qdrant::point_id::PointIdOptions;
use rusqlite::OptionalExtension;
use rusqlite::ToSql;
use rusqlite::params;
use rusqlite::params_from_iter;
use sha2::Sha256;
use sha2::digest::FixedOutputReset;
use sha2::digest::Update;
use std::time::Duration;
use tokenizers::Tokenizer;
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::warn;
use tree_sitter::Language;
use uuid::Uuid;

pub trait OptionResultExt<T> {
    // `from_cancelled` takes `self` by value: it consumes the `Option<Result<..>>`
    // produced by `with_cancellation_token` and reinterprets `None` (timeout/cancel)
    // as `Err(Cancelled)`. The `from_*`-takes-no-self convention does not fit a
    // consuming adapter method, so the lint is intentionally allowed here.
    #[allow(clippy::wrong_self_convention)]
    fn from_cancelled(self) -> Result<T, SQLite3PoolError>;
}

impl<T> OptionResultExt<T> for Option<Result<T, SQLite3PoolError>> {
    fn from_cancelled(self) -> Result<T, SQLite3PoolError> {
        match self {
            Some(Ok(x)) => Ok(x),
            Some(Err(e)) => Err(e),
            None => Err(SQLite3PoolError::Cancelled),
        }
    }
}

fn slicer_err_to_pool_err(err: SlicerError) -> SQLite3PoolError {
    match err {
        SlicerError::Cancelled => SQLite3PoolError::Cancelled,
        other => {
            error!(error = %other, "Slicer failed to parse the source into chunks.");
            SQLite3PoolError::HTTPStatusCode(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// One positional bind value for the search query. Owned (so the whole vec is
/// `Send` and can move into the `spawn_blocking` transaction closure) and
/// `PartialEq` (so the query builder can be unit-tested against exact params).
#[derive(Debug, PartialEq, Clone)]
enum Bind {
    Guid(UUIDv4),
    Lang(ProgrammingLanguage),
    Path(String),
}

impl ToSql for Bind {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            Bind::Guid(g) => g.to_sql(),
            Bind::Lang(l) => l.to_sql(),
            Bind::Path(p) => p.to_sql(),
        }
    }
}

/// Builds `post_search`'s candidate query: selects **only** `c.qdrant_guid` — the
/// `has_id` set fed to Qdrant — never `code`/metadata, which `post_search` fetches
/// separately for just the top-k winners (loading display columns for every active
/// chunk would read megabytes per query and discard >99%).
///
/// Pure and side-effect-free so the fragile `?N` parameter-numbering — which must
/// stay in lock-step with the bind order — can be unit-tested in isolation. The
/// `WHERE` clause always pins the project and `c.status = 'active'`; the optional
/// `include`/`exclude` filters append language `IN`/`NOT IN` sets and path `GLOB`
/// clauses, numbering placeholders in push order.
fn build_search_query(project_guid: UUIDv4, req: &SearchRequest) -> (String, Vec<Bind>) {
    let mut param_number: usize = 1;

    // c.status = 'active' is always required to exclude soft-deleted chunks.
    let mut meta_where = vec![
        format!("c.project_guid = ?{}", param_number),
        "c.status = 'active'".to_string(),
    ];
    param_number += 1;
    let mut binds: Vec<Bind> = vec![Bind::Guid(project_guid)];

    if let Some(inc) = &req.include {
        if let Some(pls) = &inc.programming_languages {
            let placeholders: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{param_number}");
                    param_number += 1;
                    p
                })
                .collect();
            meta_where.push(format!(
                "f.programming_language IN ({})",
                placeholders.join(", ")
            ));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }

        if let Some(paths) = &inc.paths {
            let clauses: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("c.file_path GLOB ?{param_number}");
                    param_number += 1;
                    c
                })
                .collect();
            meta_where.push(format!("({})", clauses.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }

    if let Some(exc) = &req.exclude {
        if let Some(pls) = &exc.programming_languages {
            let placeholders: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{param_number}");
                    param_number += 1;
                    p
                })
                .collect();
            meta_where.push(format!(
                "f.programming_language NOT IN ({})",
                placeholders.join(", ")
            ));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }

        if let Some(paths) = &exc.paths {
            let clauses: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("c.file_path GLOB ?{param_number}");
                    param_number += 1;
                    c
                })
                .collect();
            meta_where.push(format!("NOT ({})", clauses.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }

    let sql = format!(
        "
    SELECT c.qdrant_guid
    FROM project_file_chunks c
    JOIN project_files f
        ON c.project_guid = f.project_guid
        AND c.model_id = f.model_id
        AND c.file_path = f.path
    WHERE {}",
        meta_where.join(" AND ")
    );

    (sql, binds)
}

/// Maps an API language to its tree-sitter grammar. Pure and total over the enum,
/// so adding a `ProgrammingLanguage` variant forces a new arm here (and is the one
/// spot a missing grammar would surface at compile time).
pub(crate) fn tree_sitter_language(pl: ProgrammingLanguage) -> Language {
    match pl {
        ProgrammingLanguage::Rust => Language::new(tree_sitter_rust::LANGUAGE),
        ProgrammingLanguage::Python => Language::new(tree_sitter_python::LANGUAGE),
        ProgrammingLanguage::JavaScript => Language::new(tree_sitter_javascript::LANGUAGE),
        ProgrammingLanguage::TypeScript => {
            Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
        ProgrammingLanguage::Tsx => Language::new(tree_sitter_typescript::LANGUAGE_TSX),
        ProgrammingLanguage::Go => Language::new(tree_sitter_go::LANGUAGE),
        ProgrammingLanguage::C => Language::new(tree_sitter_c::LANGUAGE),
        ProgrammingLanguage::Cpp => Language::new(tree_sitter_cpp::LANGUAGE),
        ProgrammingLanguage::Java => Language::new(tree_sitter_java::LANGUAGE),
        ProgrammingLanguage::CSharp => Language::new(tree_sitter_c_sharp::LANGUAGE),
        ProgrammingLanguage::Ruby => Language::new(tree_sitter_ruby::LANGUAGE),
        ProgrammingLanguage::Php => Language::new(tree_sitter_php::LANGUAGE_PHP),
        ProgrammingLanguage::Bash => Language::new(tree_sitter_bash::LANGUAGE),
        ProgrammingLanguage::Html => Language::new(tree_sitter_html::LANGUAGE),
        ProgrammingLanguage::Css => Language::new(tree_sitter_css::LANGUAGE),
        ProgrammingLanguage::Json => Language::new(tree_sitter_json::LANGUAGE),
        ProgrammingLanguage::Scala => Language::new(tree_sitter_scala::LANGUAGE),
        ProgrammingLanguage::Haskell => Language::new(tree_sitter_haskell::LANGUAGE),
        ProgrammingLanguage::Ocaml => Language::new(tree_sitter_ocaml::LANGUAGE_OCAML),
        ProgrammingLanguage::Zig => Language::new(tree_sitter_zig::LANGUAGE),
        ProgrammingLanguage::Sql => Language::new(tree_sitter_sequel::LANGUAGE),
        ProgrammingLanguage::Toml => Language::new(tree_sitter_toml_ng::LANGUAGE),
        ProgrammingLanguage::Yaml => Language::new(tree_sitter_yaml::LANGUAGE),
        // The *block* grammar. The inline grammar (emphasis, links) would only
        // subdivide text this slicer never cuts below the block, so it is not used.
        ProgrammingLanguage::Markdown => Language::new(tree_sitter_md::LANGUAGE),
    }
}

/// In-process mutual exclusion for indexing a single `(project, model, path)`.
///
/// `post_index`'s pipeline is several independent transactions (hash-check →
/// mark `indexing` → slice+insert → embed → `mark_indexed`), not one atomic unit.
/// Two concurrent `/index` requests for the *same* file would interleave at those
/// boundaries: the second `prepare` marks the first's freshly-inserted chunks
/// `deleted` (so the first embeds orphan vectors), and the second `mark_indexed`
/// hits an illegal `indexed→indexed` transition — possibly leaving `sha256`
/// describing a different chunk set than is `active` (silent staleness on the next
/// hash-skip). This claim serializes the whole per-file pipeline within one process
/// (mindex is single-instance). A multi-instance deployment would need a DB-level
/// CAS claim instead.
/// The per-file mutual-exclusion key: `{guid_simple}\0{model_id}\0{path}`. The NUL
/// separators can't appear in any component, so the join is unambiguous. Built
/// identically by the indexing handler and the retry worker so a claim taken by a
/// live `/index` is visible to the worker (and vice versa) — they share one lock
/// table. `guid_simple` must be the 32-char hyphen-less form (`Uuid::simple`), which
/// is exactly how the guid is stored in SQLite (see `UUIDv4`'s `ToSql`).
pub(crate) fn indexing_lock_key(guid_simple: &str, model_id: &str, path: &str) -> String {
    format!("{guid_simple}\u{0}{model_id}\u{0}{path}")
}

pub(crate) struct IndexClaim {
    locks: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl IndexClaim {
    /// `Some(claim)` if the slot was free, `None` if another request holds it.
    pub(crate) fn try_acquire(locks: &Arc<Mutex<HashSet<String>>>, key: String) -> Option<Self> {
        // Recover from a poisoned mutex rather than panic: the set is a plain
        // membership table, no invariant is broken by a panicked holder.
        let mut set = locks.lock().unwrap_or_else(|e| e.into_inner());
        if set.insert(key.clone()) {
            Some(IndexClaim {
                locks: Arc::clone(locks),
                key,
            })
        } else {
            None
        }
    }
}

impl Drop for IndexClaim {
    fn drop(&mut self) {
        let mut set = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(&self.key);
    }
}

/// A file that has been hash-checked, marked `indexing`, sliced, and had its
/// chunks inserted — awaiting the shared embed pass. `chunks` is drained into the
/// cross-file batch before `mark_indexed`. `_claim` holds the per-file lock for the
/// whole pipeline; it releases on drop (end of `post_index`, after `mark_indexed`).
struct Prepared {
    pl: ProgrammingLanguage,
    path: String,
    sha256: String,
    chunks: Vec<(UUIDv4, String)>,
    /// Symbol rows this file's prepare transaction inserted. Carried out of the
    /// transaction only so `post_index` can count them — nothing else reads it.
    symbols: usize,
    _claim: IndexClaim,
}

/// Borrowed view of everything indexing needs. `post_index` drives it in two
/// phases — `prepare` every file, then one batched `embed_all` across all of them —
/// so the GPU sees `embed_batch`-sized batches instead of one file's chunks at a time.
struct FileIndexer<'a> {
    db_pool: &'a SQLite3Pool,
    store: &'a dyn VectorStore,
    tokenizer: &'a Arc<Tokenizer>,
    embedder: &'a dyn BGEm3Model,
    model_id: &'a str,
    project_guid: UUIDv4,
    collection: &'a str,
    /// Embed/upsert batch sizing + sparse threshold (from config).
    embed_tuning: crate::embed::EmbedTuning,
    /// Slicer token window (from config).
    min_chunk_tokens: usize,
    max_chunk_tokens: usize,
    /// Index the lines the AST walk selects nothing for (from config).
    fill_gaps: bool,
    /// Documentation chunk cap (from config). Documentation has no minimum.
    max_doc_chunk_tokens: usize,
    /// Weight of the semantic-shift term when cutting documentation; 0 turns it
    /// off, along with the per-document `/encode`.
    doc_semantic_weight: f64,
    /// Request-scoped cancellation token (the handler's `CancellationGuard`).
    token: &'a CancellationToken,
    /// Shared set of `(project, model, path)` keys currently being indexed — the
    /// per-file mutual-exclusion table (see `IndexClaim`).
    indexing_locks: &'a Arc<Mutex<HashSet<String>>>,
    /// `force` from the request: bypass the unchanged-skip entirely, so a file is
    /// rebuilt even when its hash and derivation versions both match.
    force: bool,
}

/// True iff this file is already **successfully indexed** with this exact content:
/// a row exists with `status = 'indexed'` and a matching `sha256`. The stored
/// `sha256` always reflects the content whose chunks are currently in the table —
/// it is (re)written when the file enters `indexing` (the prepare upsert) and
/// confirmed at `indexed`.
///
/// The hash alone is not enough: it says the *file* is unchanged, not that the code
/// which derives chunks and symbols from it is. So the skip also requires the stored
/// derivation versions to match the current ones — a slicer or tags-query change
/// bumps its const, stops matching, and the next ordinary run rebuilds the file.
/// `NULL` (written before versioning existed) never matches, which is what makes the
/// backfill automatic.
///
/// Only an `indexed` row counts for the skip, because a
/// non-`indexed` row has no (complete) vectors: a file sliced but never embedded
/// (e.g. the embedder was down, leaving it `failed`/`indexing`) carries the right
/// hash without any vectors. Gating the skip on `status = 'indexed'` is what lets a
/// later re-index pick such a file back up instead of treating it as unchanged forever.
fn file_already_indexed(
    tx: &rusqlite::Transaction,
    project_guid: UUIDv4,
    path: &str,
    model_id: &str,
    sha256: &str,
) -> Result<bool, SQLite3PoolError> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT sha256 FROM project_files
             WHERE project_guid = ?1 AND path = ?2 AND model_id = ?3
               AND status = 'indexed'
               AND chunks_version = ?4 AND symbols_version = ?5",
            params![
                project_guid,
                path,
                model_id,
                CHUNKS_DERIVATION_VERSION,
                SYMBOLS_DERIVATION_VERSION
            ],
            |r| r.get(0),
        )
        .optional()?;
    Ok(existing.as_deref() == Some(sha256))
}

/// The prepare-phase upsert that moves a file into `indexing`. Extracted as a const
/// so the sha256-refresh regression test executes the exact production statement
/// (binds: ?1 project_guid, ?2 path, ?3 sha256, ?4 programming_language, ?5 model_id,
/// ?6 chunks_version, ?7 symbols_version).
///
/// It stamps the derivation versions in the same statement, which is the same
/// transaction as the chunk and symbol inserts downstream — so a row can never claim
/// a version whose rows were not actually produced.
const MARK_INDEXING_UPSERT_SQL: &str = "INSERT INTO project_files
         (project_guid, path, sha256, programming_language, model_id,
          status, status_updated_at, chunks_version, symbols_version)
     VALUES (?1, ?2, ?3, ?4, ?5, 'indexing', unixepoch(), ?6, ?7)
     ON CONFLICT (project_guid, model_id, path)
     DO UPDATE SET status = 'indexing', sha256 = excluded.sha256,
                   status_updated_at = unixepoch(),
                   chunks_version  = excluded.chunks_version,
                   symbols_version = excluded.symbols_version";

/// Extracts a file's symbols and inserts them, returning how many rows were written.
/// Shared by the full index path and the `symbols_only` rebuild so the two can never
/// disagree about what a file's symbol set is.
///
/// Best-effort by contract: a missing tags query, a query that fails to build, or a
/// failed extraction all degrade to "no symbols" (WARN) and return `Ok(0)` — chunks
/// and vectors are the primary product and must not be held hostage to a grammar's
/// tags query. Cancellation is the one error that propagates.
fn extract_and_insert_symbols(
    tx: &rusqlite::Transaction,
    project_guid: UUIDv4,
    model_id: &str,
    path: &str,
    pl: ProgrammingLanguage,
    code: &str,
    token: &CancellationToken,
) -> Result<usize, SQLite3PoolError> {
    let mut extractor = match SymbolExtractor::for_language(pl, tree_sitter_language(pl)) {
        Ok(None) => return Ok(0),
        Ok(Some(e)) => e,
        Err(e) => {
            warn!(
                error = ?e,
                "Symbol tags query failed to build; indexing the file without symbols."
            );
            return Ok(0);
        }
    };

    let symbols = match extractor.extract(code, token) {
        Ok(s) => s,
        Err(SymbolError::Cancelled) => return Err(SQLite3PoolError::Cancelled),
        Err(e) => {
            warn!(
                error = ?e,
                "Symbol extraction failed; indexing the file without symbols."
            );
            return Ok(0);
        }
    };

    info!(symbols_len = symbols.len(), "Extracted symbols.");
    for s in &symbols {
        tx.execute(
            "INSERT INTO project_file_symbols
                 (project_guid, model_id, file_path, name, kind,
                  start_line, end_line, start_column,
                  end_column, parent_name, parent_kind, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                project_guid,
                model_id,
                path,
                s.name,
                s.kind,
                s.start_line as i64,
                s.end_line as i64,
                s.start_column as i64,
                s.end_column as i64,
                s.parent_name,
                s.parent_kind,
                s.doc
            ],
        )?;
    }
    Ok(symbols.len())
}

impl FileIndexer<'_> {
    /// Slices a document, refining its boundaries with block embeddings.
    ///
    /// Runs outside the prepare transaction because the middle step is an
    /// `/encode` round-trip. The two CPU halves go to `spawn_blocking`: parsing
    /// and the packing DP are cheap, but "cheap" on a 40 KB document is still
    /// milliseconds of tokenizer work, which does not belong on a runtime
    /// thread.
    ///
    /// **An unreachable embedder degrades to structure-only rather than failing
    /// the file.** The semantic term is a refinement — measured as a no-op on a
    /// well-headed corpus — so cutting by headings alone is a good answer, and a
    /// far better one than leaving the documentation unindexed.
    async fn slice_document(
        &self,
        code: &str,
        tokenizer: &Arc<Tokenizer>,
        max_doc_chunk_tokens: usize,
    ) -> Result<Vec<SlicedChunk>, ApiError> {
        let weight = self.doc_semantic_weight;
        let language = tree_sitter_language(ProgrammingLanguage::Markdown);
        let (code_owned, tok, token) = (code.to_string(), tokenizer.clone(), self.token.clone());

        let (plan, code_owned, tok) = tokio::task::spawn_blocking(move || {
            let mut slicer = MarkdownSlicer::new(language, &*tok, max_doc_chunk_tokens, weight)?;
            let plan = slicer.plan(&code_owned, token)?;
            drop(slicer); // releases the borrow of `tok` so it can be handed back
            Ok::<_, SlicerError>((plan, code_owned, tok))
        })
        .await
        .map_err(|err| {
            error!(error = ?err, "The document slicer panicked while parsing.");
            ApiError::Internal
        })?
        .map_err(slicer_err_to_pool_err)
        .map_err(ApiError::from)?;

        // One `/encode` for the whole document's blocks. Skipped entirely when
        // the term is off, so `doc_semantic_weight = 0` costs no round-trip.
        let vectors = if weight > 0.0 && !plan.is_empty() {
            let texts: Vec<String> = plan
                .block_texts(&code_owned)
                .into_iter()
                .map(str::to_string)
                .collect();
            match self
                .embedder
                .encode(BGEm3EmbedRequest { texts }, self.token.clone())
                .await
            {
                Ok(resp) => Some(resp.dense_vecs),
                Err(err) => {
                    warn!(
                        error = ?err,
                        "Could not embed document blocks; cutting this document by heading \
                         structure alone. Chunking is slightly coarser, nothing is lost. \
                         Check the embedder is reachable if this repeats."
                    );
                    None
                }
            }
        } else {
            None
        };

        let language = tree_sitter_language(ProgrammingLanguage::Markdown);
        let token = self.token.clone();
        tokio::task::spawn_blocking(move || {
            let slicer = MarkdownSlicer::new(language, &*tok, max_doc_chunk_tokens, weight)?;
            slicer.segment(&code_owned, &plan, vectors.as_deref(), token)
        })
        .await
        .map_err(|err| {
            error!(error = ?err, "The document slicer panicked while segmenting.");
            ApiError::Internal
        })?
        .map_err(slicer_err_to_pool_err)
        .map_err(ApiError::from)
    }

    /// Phase 1 for one file: hash-check → mark `indexing` → mark old chunks deleted,
    /// slice, insert new chunks. Returns `Ok(None)` if unchanged (skipped),
    /// `Ok(Some(Prepared))` with its chunks (possibly empty, for too-short files),
    /// or `Err` — in which case *this* file's status is already recovered.
    async fn prepare(
        &self,
        pl: ProgrammingLanguage,
        path: &str,
        code: &str,
        hasher: &mut Sha256,
    ) -> Result<Option<Prepared>, ApiError> {
        let span = info_span!("indexing_file", ?pl, path);
        async move {
            let project_guid = self.project_guid;

            // ── claim the per-file slot (serialize concurrent same-file indexing) ─
            // Held across the whole pipeline via `Prepared._claim`; released on any
            // early return below (unchanged / error) when this local drops.
            let claim = {
                let key = indexing_lock_key(
                    &project_guid.0.as_simple().to_string(),
                    self.model_id,
                    path,
                );
                match IndexClaim::try_acquire(self.indexing_locks, key) {
                    Some(c) => c,
                    None => {
                        info!(
                            "The file is already being indexed by another in-flight \
                             request; skipping it so the rest of the batch can proceed."
                        );
                        return Err(ApiError::FileInFlight);
                    }
                }
            };

            hasher.update(code.as_bytes());
            let sha256 = hex::encode(hasher.finalize_fixed_reset());

            // ── hash check ───────────────────────────────────────────────
            {
                let (sha256_c, path_c, model_id_c) =
                    (sha256.clone(), path.to_string(), self.model_id.to_string());
                let force = self.force;
                let unchanged = self
                    .db_pool
                    .transaction(self.token.child_token(), move |tx| {
                        if force {
                            return Ok(false);
                        }
                        file_already_indexed(tx, project_guid, &path_c, &model_id_c, &sha256_c)
                    })
                    .with_cancellation_token(self.token)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(error = ?err, "Failed to read the stored file hash from SQLite.");
                        ApiError::from(err)
                    })?;

                if unchanged {
                    info!("The source code has not changed: no reindexing is required.");
                    return Ok(None);
                }

                info!("The source code has changed: reindexing is required.");
            }

            // ── status = 'indexing' (committed before heavy work) ────────
            {
                let (sha256_u, path_u, model_id_u) =
                    (sha256.clone(), path.to_string(), self.model_id.to_string());
                self.db_pool
                    .transaction(self.token.child_token(), move |tx| {
                        tx.execute(
                            MARK_INDEXING_UPSERT_SQL,
                            params![
                                project_guid,
                                path_u,
                                sha256_u,
                                pl,
                                model_id_u,
                                CHUNKS_DERIVATION_VERSION,
                                SYMBOLS_DERIVATION_VERSION
                            ],
                        )?;
                        Ok(())
                    })
                    .with_cancellation_token(self.token)
                    .await
                    .from_cancelled()
                    .map_err(|err| {
                        error!(error = ?err, "Failed to mark the file 'indexing' in SQLite.");
                        ApiError::from(err)
                    })?;
            }

            // ── mark old chunks deleted + slice + insert new chunks ───────
            let tokenizer = self.tokenizer.clone();
            let (path_m, model_id_m, code_m) =
                (path.to_string(), self.model_id.to_string(), code.to_string());
            let (min_chunk_tokens, max_chunk_tokens, max_doc_chunk_tokens, fill_gaps) = (
                self.min_chunk_tokens,
                self.max_chunk_tokens,
                self.max_doc_chunk_tokens,
                self.fill_gaps,
            );
            let slicer_token = self.token.clone();

            // Documentation is sliced *before* the transaction, because its
            // boundaries are refined by embedding distance and that is network
            // I/O. Everything else stays inside, sliced on the pool's blocking
            // thread exactly as before.
            let pre_sliced = if pl == ProgrammingLanguage::Markdown {
                Some(
                    self.slice_document(&code_m, &tokenizer, max_doc_chunk_tokens)
                        .await?,
                )
            } else {
                None
            };

            let result = self
                .db_pool
                .transaction(self.token.child_token(), move |tx| {
                    tx.execute(
                        "UPDATE project_file_chunks
                         SET status = 'deleted'
                         WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3
                           AND status = 'active'",
                        params![project_guid, path_m, model_id_m],
                    )?;
                    // Symbols parallel the chunk set: replaced (hard-delete, no
                    // Qdrant counterpart → no GC cycle) in the same transaction.
                    tx.execute(
                        "DELETE FROM project_file_symbols
                         WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3",
                        params![project_guid, path_m, model_id_m],
                    )?;

                    // Documentation arrives already sliced (see above); every
                    // other language is cut here. Both produce `SlicedChunk`,
                    // so nothing downstream of this branch differs.
                    let chunks = match pre_sliced {
                        Some(chunks) => chunks,
                        None => Slicer::new(
                            tree_sitter_language(pl),
                            &*tokenizer,
                            min_chunk_tokens,
                            max_chunk_tokens,
                            fill_gaps,
                        )
                        .map_err(slicer_err_to_pool_err)?
                        .parse(&code_m, slicer_token.clone())
                        .map_err(slicer_err_to_pool_err)?,
                    };

                    info!(chunks_len = chunks.len(), "Sliced the source code.");

                    // Best-effort symbol extraction: a failure here degrades the
                    // file to "no symbols" (WARN), never fails its indexing —
                    // chunks/vectors are the primary product.
                    let symbols = extract_and_insert_symbols(
                        tx,
                        project_guid,
                        &model_id_m,
                        &path_m,
                        pl,
                        &code_m,
                        &slicer_token,
                    )?;

                    let mut out: Vec<(UUIDv4, String)> = Vec::with_capacity(chunks.len());
                    for SlicedChunk {
                        code,
                        start_line,
                        end_line,
                        start_column,
                        end_column,
                        ..
                    } in &chunks
                    {
                        let qdrant_guid = UUIDv4(Uuid::new_v4());
                        tx.execute(
                            "INSERT INTO project_file_chunks
                                 (project_guid, file_path, code, model_id, qdrant_guid,
                                  start_line, end_line, start_column, end_column, status)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active')",
                            params![
                                project_guid,
                                path_m,
                                code,
                                model_id_m,
                                qdrant_guid,
                                *start_line as i64,
                                *end_line as i64,
                                *start_column as i64,
                                *end_column as i64
                            ],
                        )?;
                        out.push((qdrant_guid, code.clone()));
                    }

                    Ok((out, symbols))
                })
                .with_cancellation_token(self.token)
                .await
                .from_cancelled();

            let (chunks, symbols) = match result {
                Ok(v) => v,
                Err(SQLite3PoolError::Cancelled) => {
                    self.recover(path, "cancelled", false).await;
                    return Err(ApiError::Cancelled);
                }
                Err(SQLite3PoolError::HTTPStatusCode(_)) => {
                    // Set only by `slicer_err_to_pool_err` (always 500): a slicer failure.
                    self.recover(path, "failed", true).await;
                    return Err(ApiError::Internal);
                }
                Err(err) => {
                    error!(error = ?err, "Slicing / chunk insertion failed; marking file 'failed'.");
                    self.recover(path, "failed", true).await;
                    return Err(ApiError::Internal);
                }
            };

            Ok(Some(Prepared {
                pl,
                path: path.to_string(),
                sha256,
                chunks,
                symbols,
                _claim: claim,
            }))
        }
        .instrument(span)
        .await
    }

    /// Phase 2: embed + upsert every chunk across all prepared files in one batched
    /// pass (`embed_batch` chunks per `/encode`). `progress` is invoked per completed
    /// batch — `None` outside a streaming (`?stream=yes`) request.
    async fn embed_all(
        &self,
        chunks: &[(UUIDv4, String)],
        progress: Option<&(dyn Fn(EmbedProgress) + Send + Sync)>,
    ) -> Result<(), EmbedUpsertError> {
        embed_and_upsert(
            self.embedder,
            self.store,
            self.collection,
            chunks,
            self.token,
            self.embed_tuning,
            progress,
        )
        .await
    }

    /// Phase 3 for one file: mark it `indexed` and record the new sha256. The
    /// `AND status = 'indexing'` guard makes this a no-op (matching 0 rows, so no
    /// trigger fires) if a concurrent `POST /cancel` moved the file to `cancelled`
    /// since it was prepared — without it the raw `cancelled → indexed` UPDATE would
    /// trip the state-machine trigger and error the whole batch, leaving sibling
    /// files stuck in `indexing`.
    /// Returns whether the row actually moved. `false` is the cancel race the
    /// `AND status = 'indexing'` guard exists for — and it is not an error, but it is
    /// also not an indexed file: the caller used to report `indexed` to the client and
    /// to `index.files{outcome}` for a file that stayed `cancelled` in the database,
    /// because a 0-row `UPDATE` is `Ok(())`.
    async fn mark_indexed(&self, path: &str, sha256: &str) -> Result<bool, ApiError> {
        let project_guid = self.project_guid;
        let (sha256_f, path_f, model_id_f) = (
            sha256.to_string(),
            path.to_string(),
            self.model_id.to_string(),
        );
        self.db_pool
            .transaction(self.token.child_token(), move |tx| {
                tx.execute(
                    "UPDATE project_files
                     SET status = 'indexed', sha256 = ?1, retry_count = 0,
                         status_updated_at = unixepoch()
                     WHERE project_guid = ?2 AND path = ?3 AND model_id = ?4
                       AND status = 'indexing'",
                    params![sha256_f, project_guid, path_f, model_id_f],
                )
                .map_err(SQLite3PoolError::from)
            })
            .with_cancellation_token(self.token)
            .await
            .from_cancelled()
            .map_err(|err| {
                error!(error = ?err, "Failed to mark the file 'indexed' in SQLite.");
                ApiError::from(err)
            })
            .map(|rows| rows > 0)
    }

    /// Best-effort recovery: move the file to `status` (incrementing `retry_count`
    /// when `increment_retry`) on a cancellation/failure path.
    ///
    /// **Runs under a token of its own, never `self.token`.** The commonest reason to
    /// be here is that `self.token` was just cancelled — and `SQLite3Pool::run`
    /// short-circuits on a cancelled token before touching the database, so passing a
    /// child of it made the whole mechanism a no-op in exactly the case it exists for:
    /// the file stayed `indexing` and only the stuck-grace sweep (30 minutes) ever
    /// picked it up. The write is a single `UPDATE` and it is the thing that hands the
    /// file to the retry worker, so it must outlive the cancellation that caused it.
    /// The unit test used a fresh token and so agreed with the bug.
    async fn recover(&self, path: &str, status: &'static str, increment_retry: bool) {
        // Discarded deliberately, and this is the one caller entitled to: the request
        // is already failing or cancelled, so there is no answer to give and nothing
        // further to try. `set_file_status` has logged whatever went wrong, and the
        // stuck-grace sweep is the backstop.
        let _ = set_file_status(
            self.db_pool,
            &self.project_guid.0.as_simple().to_string(),
            path,
            self.model_id,
            status,
            increment_retry,
            CancellationToken::new(),
        )
        .await;
    }

    /// Recovers every already-prepared file when the batch is aborted (a later
    /// file's prepare failed, or the shared embed failed) — they are still
    /// `indexing` with chunks inserted, so this hands them to the retry worker.
    async fn recover_all(
        &self,
        prepared: &[Prepared],
        status: &'static str,
        increment_retry: bool,
    ) {
        for p in prepared {
            self.recover(&p.path, status, increment_retry).await;
        }
    }

    /// The `symbols_only` path: replace one file's symbol rows without slicing,
    /// embedding, or touching Qdrant. Symbols come from tree-sitter alone, so a
    /// `SYMBOLS_DERIVATION_VERSION` bump costs CPU here instead of a GPU pass —
    /// which is the entire reason chunks and symbols carry separate versions.
    ///
    /// Returns `Ok(None)` when the file is deliberately skipped:
    /// - it is not `indexed`, or its stored hash differs from the posted content.
    ///   Its chunks are stale too, and symbols describing newer text than the chunks
    ///   beside them would break the "symbols parallel chunks" invariant. The caller
    ///   should run an ordinary index pass for those files.
    /// - it is already at the current symbols version (unless `force`).
    ///
    /// Deliberately does **not** touch `project_files.status`: the file stays
    /// `indexed` throughout. Moving it through `indexing` would churn
    /// `status_updated_at` and the status log for work that cannot fail halfway —
    /// everything below is one transaction.
    async fn rebuild_symbols(
        &self,
        pl: ProgrammingLanguage,
        path: &str,
        code: &str,
        hasher: &mut Sha256,
    ) -> Result<Option<u64>, ApiError> {
        let span = info_span!("rebuilding_symbols", ?pl, path);
        async move {
            let project_guid = self.project_guid;

            // Same claim as the full path: a concurrent /index for this file would
            // otherwise race our DELETE + INSERT against its own.
            let _claim = {
                let key =
                    indexing_lock_key(&project_guid.0.as_simple().to_string(), self.model_id, path);
                match IndexClaim::try_acquire(self.indexing_locks, key) {
                    Some(c) => c,
                    None => {
                        info!("The file is already being indexed by another in-flight request.");
                        return Err(ApiError::FileInFlight);
                    }
                }
            };

            hasher.update(code.as_bytes());
            let sha256 = hex::encode(hasher.finalize_fixed_reset());

            let (path_m, model_id_m, code_m) = (
                path.to_string(),
                self.model_id.to_string(),
                code.to_string(),
            );
            let force = self.force;
            let token = self.token.clone();

            let written = self
                .db_pool
                .transaction(self.token.child_token(), move |tx| {
                    let stored: Option<(String, Option<String>)> = tx
                        .query_row(
                            "SELECT sha256, symbols_version FROM project_files
                              WHERE project_guid = ?1 AND path = ?2
                                AND model_id = ?3 AND status = 'indexed'",
                            params![project_guid, path_m, model_id_m],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;

                    let Some((stored_sha, stored_version)) = stored else {
                        return Ok(None);
                    };
                    if stored_sha != sha256 {
                        return Ok(None);
                    }
                    if !force && stored_version.as_deref() == Some(SYMBOLS_DERIVATION_VERSION) {
                        return Ok(None);
                    }

                    tx.execute(
                        "DELETE FROM project_file_symbols
                         WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3",
                        params![project_guid, path_m, model_id_m],
                    )?;
                    let n = extract_and_insert_symbols(
                        tx,
                        project_guid,
                        &model_id_m,
                        &path_m,
                        pl,
                        &code_m,
                        &token,
                    )?;

                    // `chunks_version` is COALESCEd, not assigned: this pass did not
                    // re-slice, so an already-versioned file keeps whatever produced
                    // its chunks. An *unversioned* file (NULL) is stamped with the
                    // current one anyway — its hash matched, so the chunks in the
                    // table are the ones this slicer would produce, and leaving NULL
                    // would send the next ordinary run through a full re-embed for
                    // nothing.
                    tx.execute(
                        "UPDATE project_files
                            SET symbols_version = ?4,
                                chunks_version  = COALESCE(chunks_version, ?5)
                          WHERE project_guid = ?1 AND path = ?2 AND model_id = ?3",
                        params![
                            project_guid,
                            path_m,
                            model_id_m,
                            SYMBOLS_DERIVATION_VERSION,
                            CHUNKS_DERIVATION_VERSION
                        ],
                    )?;
                    Ok(Some(n as u64))
                })
                .with_cancellation_token(self.token)
                .await
                .from_cancelled()
                .map_err(|err| {
                    error!(error = ?err, "Failed to rebuild the file's symbols in SQLite.");
                    ApiError::from(err)
                })?;

            Ok(written)
        }
        .instrument(span)
        .await
    }

    /// Reconciliation between Phase 1 and Phase 2: drop any prepared file that a
    /// concurrent `POST /cancel` (or `DELETE /files`) flipped out of `indexing`
    /// since it was prepared. `/cancel` deliberately does not take the per-file
    /// claim (so it can interrupt a held one), so the live request must check for
    /// itself before the expensive embed. Cancelled files' just-inserted `active`
    /// chunks are marked `deleted` so GC reclaims them — this also closes the race
    /// where `/cancel` landed after `status='indexing'` but before the chunks
    /// existed (the `/cancel` UPDATE then matched no chunks). Best-effort: a query
    /// failure leaves the set whole (worst case is a wasted embed; the still-
    /// `cancelled` file's `mark_indexed` matches 0 rows — its `AND status =
    /// 'indexing'` guard makes it a silent no-op, not a trigger rejection — and
    /// its chunks are GC'd anyway).
    async fn drop_cancelled(&self, prepared: Vec<Prepared>) -> Vec<Prepared> {
        if prepared.is_empty() {
            return prepared;
        }
        let project_guid = self.project_guid;
        let paths: Vec<String> = prepared.iter().map(|p| p.path.clone()).collect();
        let model_id = self.model_id.to_string();

        let cancelled: HashSet<String> = self
            .db_pool
            .transaction(self.token.child_token(), move |tx| {
                let placeholders = (3..3 + paths.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut binds: Vec<Bind> = Vec::with_capacity(paths.len() + 2);
                binds.push(Bind::Guid(project_guid));
                binds.push(Bind::Path(model_id));
                binds.extend(paths.into_iter().map(Bind::Path));
                tx.prepare(&format!(
                    "SELECT path FROM project_files
                     WHERE project_guid = ?1 AND model_id = ?2 AND status != 'indexing'
                       AND path IN ({placeholders})"
                ))?
                .query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?
                .collect::<Result<HashSet<_>, _>>()
                .map_err(SQLite3PoolError::from)
            })
            .with_cancellation_token(self.token)
            .await
            .from_cancelled()
            // Reading a failure as "nothing was cancelled" is the safe direction — the
            // batch proceeds, and `mark_indexed`'s `AND status = 'indexing'` still
            // refuses to resurrect a cancelled file — but it was also silent, so a
            // database that had stopped answering looked exactly like an ordinary run.
            .unwrap_or_else(|err| {
                if !matches!(err, SQLite3PoolError::Cancelled) {
                    warn!(
                        error = %err,
                        project_guid = %project_guid.0,
                        files = prepared.len(),
                        "Failed to re-read which files were cancelled mid-flight; \
                         embedding the whole batch. Any file cancelled since Phase 1 \
                         keeps its 'cancelled' status and its chunks are reclaimed by GC."
                    );
                }
                HashSet::new()
            });

        if cancelled.is_empty() {
            return prepared;
        }

        for path in &cancelled {
            let (pg, p, m) = (project_guid, path.clone(), self.model_id.to_string());
            // Not `self.token`: this is the cleanup half of a cancellation, so the
            // token that brought us here is routinely already cancelled — see
            // `recover`. Leaving the chunks `active` would let a cancelled file keep
            // stale vectors that nothing marks for GC.
            let dropped = self
                .db_pool
                .transaction(CancellationToken::new(), move |tx| {
                    tx.execute(
                        "UPDATE project_file_chunks SET status = 'deleted'
                         WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3
                           AND status = 'active'",
                        params![pg, p, m],
                    )?;
                    tx.execute(
                        "DELETE FROM project_file_symbols
                         WHERE project_guid = ?1 AND file_path = ?2 AND model_id = ?3",
                        params![pg, p, m],
                    )?;
                    Ok(())
                })
                .await;
            // The line used to be unconditional, and so asserted a cleanup it had not
            // checked: the `let _ =` above it discarded the only evidence either way.
            match dropped {
                Ok(()) => info!(
                    %path,
                    "Indexing cancelled mid-flight; skipping the embed pass for this file."
                ),
                Err(err) => warn!(
                    error = %err,
                    %path,
                    "Indexing was cancelled mid-flight but this file's chunks could not \
                     be marked deleted; skipping its embed pass anyway. Its old chunks \
                     stay 'active' until the next successful reindex."
                ),
            }
        }

        prepared
            .into_iter()
            .filter(|p| !cancelled.contains(&p.path))
            .collect()
    }
}

/// Index (or reindex) a batch of files for a project.
///
/// Files are grouped by language → path. The pipeline runs in two phases so the GPU
/// sees large batches: every file is hashed, marked `indexing`, sliced into 128–512
/// token chunks and its chunks inserted; then **all** files' chunks are embedded in
/// one batched pass and upserted to Qdrant; finally each file is marked `indexed`.
/// Re-indexing identical content (matching sha256) is skipped server-side and omitted
/// from the response. The project and its Qdrant collection are created on first use.
///
/// Reindex is append-only: old chunks are soft-deleted (reclaimed later by GC), never
/// deleted inline, so indexing latency is decoupled from Qdrant delete latency.
///
/// The unchanged-skip also compares **derivation versions**, so a slicer or tags-query
/// change reaches files already indexed. Two body flags override it:
/// - `force` — rebuild everything posted, matching hash and versions notwithstanding.
///   The escape hatch for what versioning cannot see (a grammar-crate bump, a corrupt
///   index, debugging); it costs a full re-slice and re-embed of everything posted.
/// - `symbols_only` — rebuild **only** `project_file_symbols`. No slicing, no embed
///   pass, no Qdrant contact at all; chunks and vectors are untouched and the file
///   never leaves `indexed`. This is the cheap half of a `SYMBOLS_DERIVATION_VERSION`
///   bump. A posted file whose hash no longer matches is skipped rather than rebuilt
///   (its chunks are stale too, and symbols must parallel the chunk set); the response
///   count is the number of symbol rows written instead of chunks.
///
/// **Streaming** (`?stream=yes`): the same pipeline reported live as SSE
/// (`text/event-stream`, named events with JSON `data`), for clients that want a
/// real progress display instead of one summary at the end. The wire shape lives
/// in four places that must move together: this doc comment, the OpenAPI 200
/// description, the `mindex-index` reader and the VS Code extension. Events:
///
/// - `started` `{files, symbols_only}` — the request was accepted; `files` is
///   what was posted (unchanged files are discovered per file, later), and
///   `symbols_only` says what unit every later `count` is in;
/// - `prepared` `{path, language, chunks, symbols}` — one file hash-checked,
///   sliced and its chunks inserted, awaiting the shared embed pass;
/// - `skipped` `{path, language, reason}` — a posted file that produced no work,
///   `reason` one of `unchanged` / `in_flight` / `cancelled` (a concurrent
///   `POST /cancel`); it is absent from `done.files` exactly as it would be
///   absent from the JSON response;
/// - `embedded` `{batch_chunks, chunks_done, chunks_total, elapsed_ms}` — one
///   embed batch encoded **and** upserted. `chunks_done` is cumulative and
///   `elapsed_ms` is the server's own clock since request start — the pair a
///   client needs for an honest chunks-per-second;
/// - `indexed` `{path, language, count}` — one file confirmed `indexed`;
///   `count` is chunks (or symbol rows under `symbols_only`);
/// - `done` `{files, files_indexed, chunks, elapsed_ms}` — terminal; `files` is
///   byte-for-byte the JSON mode's response body, so both modes tally
///   identically;
/// - `error` `{code, detail}` — terminal failure after the stream started (the
///   HTTP status is already 200); `code` is the stable `ApiError` code the JSON
///   mode would have carried in problem+json.
///
/// SSE comments are sent as keep-alive every 15 s. **Closing the connection
/// cancels the request** — the job notices at its next cancellation point and
/// recovers the batch exactly as a dropped JSON request would (499 semantics).
///
/// **Concurrency:** safe. Each `(project, model, path)` is serialized by an in-process
/// claim — a second in-flight request for the *same* file is **skipped** (it is absent
/// from the response, like an unchanged file); different files proceed in parallel.
/// A concurrent `POST /cancel` is reconciled before the embed pass. On any failure
/// the whole batch is recovered to `failed`/`cancelled` and the retry worker re-attempts it.
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/index",
    tag = "Indexing",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        IndexQuery,
    ),
    request_body = IndexRequest,
    responses(
        (status = 200, description = "Without `?stream=yes`: per-file chunk counts for the files \
actually (re)indexed (JSON). With `?stream=yes`: an SSE stream of indexing events — \
`started` `{files, symbols_only}`, `prepared` `{path, language, chunks, symbols}`, `skipped` \
`{path, language, reason: unchanged|in_flight|cancelled}`, `embedded` `{batch_chunks, \
chunks_done, chunks_total, elapsed_ms}` (one per embed batch, cumulative — the basis for a \
live chunks-per-second), `indexed` `{path, language, count}`, then exactly one terminal \
event: `done` `{files, files_indexed, chunks, elapsed_ms}` (where `files` is the JSON mode's \
response body) or `error` `{code, detail}` (a failure after the stream started; `code` is \
the stable `ApiError` code). Closing the connection cancels the request.", body = IndexResponse),
        (status = 400, description = "Validation failed (bad path, oversized file, too many files).", body = ProblemDetails),
        (status = 413, description = "The request body exceeded [server].max_body_mib.", body = ProblemDetails),
        (status = 499, description = "Client closed the connection; indexing was cancelled (nginx convention).", body = ProblemDetails),
        (status = 500, description = "SQLite, slicer, or Qdrant upsert failure; the batch was marked `failed` for the retry worker.", body = ProblemDetails),
        (status = 503, description = "The embedder is unreachable or returned persistent backpressure; the batch was marked `failed`.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_index(
    IndexScope(project_guid, _auth): IndexScope,
    ApiQuery(q): ApiQuery<IndexQuery>,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<IndexRequest>,
) -> Result<Response, ApiError> {
    validate::validate_index_request(&payload, s.max_files_per_request, s.max_code_bytes)?;

    if q.stream != Some(StreamChoice::Yes) {
        // JSON mode — the original behaviour: the guard's Drop (fired when a
        // disconnected client's handler future is dropped) cancels the work, and
        // every failure is an HTTP status.
        let guard = http3::CancellationGuard(CancellationToken::new());
        let started = std::time::Instant::now();
        let res = run_index_job(s, project_guid, payload, None, guard.0.clone(), started).await?;
        return Ok(Json(res).into_response());
    }

    // SSE mode. The handler future returns as soon as the stream is constructed,
    // so a CancellationGuard here would cancel the job at that very instant.
    // Instead the job is spawned detached (the research shape) and the *stream's*
    // Drop cancels the token: a client disconnect makes axum drop the SSE body,
    // which cancels the job, whose own recovery paths then mark the batch
    // `cancelled` — the same end state a dropped JSON request reaches.
    let token = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let job_token = token.clone();
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let terminal = match run_index_job(
            s,
            project_guid,
            payload,
            Some(tx.clone()),
            job_token,
            started,
        )
        .await
        {
            Ok(res) => {
                let files_indexed = res.files.values().map(HashMap::len).sum();
                let chunks = res.files.values().flat_map(HashMap::values).sum();
                IndexEvent::Done {
                    response: res,
                    files_indexed,
                    chunks,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                }
            }
            Err(e) => IndexEvent::Error {
                code: e.code().to_string(),
                detail: e.detail(),
            },
        };
        // A send failure means the client is gone; the job has already recovered.
        let _ = tx.send(terminal);
    });

    Ok(
        axum::response::sse::Sse::new(SseEventStream::new(rx, token))
            .keep_alive(
                axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
            )
            .into_response(),
    )
}

/// The whole indexing pipeline for one `/index` request, shared verbatim by both
/// response modes. `events` present = streaming: progress events are sent as the
/// work happens (a send to a disconnected receiver is silently dropped — the job
/// still runs to its next cancellation point). The terminal `done`/`error` event
/// is the *caller's* job, built from this function's return value.
async fn run_index_job(
    s: RouterState,
    project_guid: UUIDv4,
    payload: IndexRequest,
    events: Option<tokio::sync::mpsc::UnboundedSender<IndexEvent>>,
    token: CancellationToken,
    started: std::time::Instant,
) -> Result<IndexResponse, ApiError> {
    let span = info_span!("indexing", project_guid = %project_guid.0);

    async move {
        let emit = |e: IndexEvent| {
            if let Some(tx) = &events {
                let _ = tx.send(e);
            }
        };
        emit(IndexEvent::Started {
            files: payload.files.values().map(HashMap::len).sum(),
            symbols_only: payload.symbols_only,
        });

        let db_pool = s.db_pool;
        let qdrant = s.qdrant;
        let tokenizer = s.tokenizer;
        let indexing_locks = s.indexing_locks;
        let EmbeddingModel::BGEm3 { model_id, client } = s.model;

        let collection = collection_for(project_guid);

        // ── ensure project row ────────────────────────────────────────────────
        {
            let model_id = model_id.clone();
            db_pool
                .transaction(token.child_token(), move |tx| {
                    // Idempotent and concurrency-safe: two parallel first-time /index
                    // calls for the same new project both reach here. A SELECT-then-
                    // INSERT would let both pass the check and the second trip the
                    // (guid, model_id) PK, failing an otherwise-valid request with 500.
                    // ON CONFLICT DO NOTHING makes the loser a no-op instead. This is
                    // *before* the per-file claim, so the claim can't cover it.
                    let inserted = tx.execute(
                        "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)
                         ON CONFLICT (guid, model_id) DO NOTHING",
                        params![project_guid, model_id],
                    )?;
                    if inserted > 0 {
                        info!("Created a new project.");
                    } else {
                        info!("Project already exists.");
                    }
                    Ok(())
                })
                .with_cancellation_token(&token)
                .await
                .from_cancelled()
                .map_err(|err| {
                    error!(error = ?err, "Failed to ensure the project row in SQLite.");
                    ApiError::from(err)
                })?;
        }

        // ── ensure Qdrant collection ──────────────────────────────────────────
        // Skipped entirely under `symbols_only`: that path writes no vectors, and not
        // contacting Qdrant at all is part of what makes it cheap.
        if !payload.symbols_only {
            qdrant.ensure_project(&collection).await.map_err(|err| {
                error!(
                    error = ?err,
                    "Failed to ensure the Qdrant collection. \
                     Check Qdrant is reachable at --qdrant-server and accepting connections."
                );
                ApiError::Internal
            })?;
        }

        let mut res = IndexResponse {
            files: HashMap::new(),
        };
        let mut sha256_hasher = Sha256::default();

        let indexer = FileIndexer {
            db_pool: &db_pool,
            store: &*qdrant,
            tokenizer: &tokenizer,
            embedder: &*client,
            model_id: &model_id,
            project_guid,
            collection: &collection,
            embed_tuning: s.embed_tuning,
            min_chunk_tokens: s.min_chunk_tokens,
            max_chunk_tokens: s.max_chunk_tokens,
            fill_gaps: s.fill_gaps,
            max_doc_chunk_tokens: s.max_doc_chunk_tokens,
            doc_semantic_weight: s.doc_semantic_weight,
            token: &token,
            indexing_locks: &indexing_locks,
            force: payload.force,
        };

        // ── symbols_only: the cheap path. No slicing, no embed pass, no Qdrant; one
        //    transaction per file replaces its symbol rows and restamps its version.
        if payload.symbols_only {
            info!(force = payload.force, "Rebuilding symbols only.");
            for (pl, files) in payload.files.iter() {
                let pl = *pl;
                res.files.entry(pl).or_default();
                for (path, Code { code }) in files.iter() {
                    match indexer
                        .rebuild_symbols(pl, path, code, &mut sha256_hasher)
                        .await
                    {
                        Ok(Some(n)) => {
                            emit(IndexEvent::Indexed {
                                path: path.clone(),
                                language: pl,
                                count: n,
                            });
                            res.files.entry(pl).or_default().insert(path.clone(), n);
                        }
                        // Up to date, or stale/not-indexed (needs a full pass instead).
                        Ok(None) => emit(IndexEvent::Skipped {
                            path: path.clone(),
                            language: pl,
                            reason: SkipReason::Unchanged,
                        }),
                        // Another in-flight request holds the claim; skip it so the rest
                        // of the batch proceeds, exactly as the full path does.
                        Err(ApiError::FileInFlight) => emit(IndexEvent::Skipped {
                            path: path.clone(),
                            language: pl,
                            reason: SkipReason::InFlight,
                        }),
                        Err(e) => return Err(e),
                    }
                }
            }
            return Ok(res);
        }

        // ── Phase 1: prepare every file (hash-check, mark indexing, slice + insert).
        let m = s.metrics.clone();
        let guid_label = project_guid.0.simple().to_string();
        let file_outcome = |pl: ProgrammingLanguage, outcome: &'static str| {
            m.index
                .files
                .get_or_create(&crate::backend::metrics::ProjectLangOutcomeLabels {
                    project_guid: guid_label.clone(),
                    language: pl.name(),
                    outcome,
                })
                .inc();
        };

        let prepare_started = std::time::Instant::now();
        let mut prepared: Vec<Prepared> = Vec::new();
        for (pl, files) in payload.files.iter() {
            let pl = *pl;
            res.files.entry(pl).or_default();

            for (path, Code { code }) in files.iter() {
                // The size distribution is language-labelled but not
                // project-labelled: a histogram is a dozen-plus exposition lines,
                // and multiplying that by the project count buys a breakdown
                // nobody reads.
                m.index
                    .file_size
                    .get_or_create(&crate::backend::metrics::LangLabels {
                        language: pl.name(),
                    })
                    .observe(code.len() as f64);

                match indexer.prepare(pl, path, code, &mut sha256_hasher).await {
                    Ok(Some(p)) => {
                        let lang = crate::backend::metrics::ProjectLangLabels {
                            project_guid: guid_label.clone(),
                            language: pl.name(),
                        };
                        m.index
                            .chunks
                            .get_or_create(&lang)
                            .inc_by(p.chunks.len() as u64);
                        m.index
                            .symbols
                            .get_or_create(&lang)
                            .inc_by(p.symbols as u64);
                        m.index
                            .code_bytes
                            .get_or_create(&lang)
                            .inc_by(code.len() as u64);
                        m.index
                            .file_chunks
                            .get_or_create(&crate::backend::metrics::LangLabels {
                                language: pl.name(),
                            })
                            .observe(p.chunks.len() as f64);
                        emit(IndexEvent::Prepared {
                            path: path.clone(),
                            language: pl,
                            chunks: p.chunks.len(),
                            symbols: p.symbols,
                        });
                        prepared.push(p);
                    }
                    Ok(None) => {
                        emit(IndexEvent::Skipped {
                            path: path.clone(),
                            language: pl,
                            reason: SkipReason::Unchanged,
                        });
                        file_outcome(pl, "skipped_unchanged");
                    }
                    // Another in-flight request holds the claim for this file; skip it
                    // so the rest of the batch proceeds. Innocent co-batched files must
                    // not pay a retry_count penalty for an unrelated file's contention.
                    Err(ApiError::FileInFlight) => {
                        // Counted here because it is counted nowhere else: the error
                        // is swallowed and the request still 200s, so the HTTP
                        // middleware can never see this.
                        m.index.claim_conflicts.inc();
                        emit(IndexEvent::Skipped {
                            path: path.clone(),
                            language: pl,
                            reason: SkipReason::InFlight,
                        });
                        file_outcome(pl, "in_flight");
                    }
                    Err(e) => {
                        // A real prepare failure; recover the ones already prepared
                        // (they're 'indexing' with chunks inserted) before bailing.
                        file_outcome(pl, "failed");
                        for p in &prepared {
                            file_outcome(p.pl, "failed");
                        }
                        indexer.recover_all(&prepared, "failed", true).await;
                        return Err(e);
                    }
                }
            }
        }
        m.index
            .phase_duration
            .get_or_create(&crate::backend::metrics::PhaseLabels { phase: "prepare" })
            .observe(prepare_started.elapsed().as_secs_f64());

        // ── Reconcile against concurrent cancellation before the expensive embed pass:
        //    drop any file a `POST /cancel` flipped to 'cancelled' since it was prepared.
        //    Streaming reports the dropped files by set difference — computed only when
        //    someone is listening, so the JSON path pays nothing for it.
        //
        // Collected unconditionally now: the `events.is_some()` guard was right when
        // the only consumer was the SSE event, but a dropped file must also be counted
        // in `index.files{outcome}`, or a cancelled file vanishes from the per-file
        // outcome family in JSON mode — the two modes would tally differently, which
        // is the one thing `run_index_job` being shared is meant to prevent.
        let before: Vec<(ProgrammingLanguage, String)> =
            prepared.iter().map(|p| (p.pl, p.path.clone())).collect();
        let mut prepared = indexer.drop_cancelled(prepared).await;
        let kept: HashSet<&str> = prepared.iter().map(|p| p.path.as_str()).collect();
        for (pl, path) in before {
            if !kept.contains(path.as_str()) {
                file_outcome(pl, "cancelled");
                emit(IndexEvent::Skipped {
                    path,
                    language: pl,
                    reason: SkipReason::Cancelled,
                });
            }
        }
        drop(kept);

        // ── Phase 2: embed + upsert every chunk across all files in one batched pass.
        let counts: Vec<u64> = prepared.iter().map(|p| p.chunks.len() as u64).collect();
        let all_chunks: Vec<(UUIDv4, String)> = prepared
            .iter_mut()
            .flat_map(|p| std::mem::take(&mut p.chunks))
            .collect();

        info!(
            files = prepared.len(),
            chunks = all_chunks.len(),
            "Embedding request in batches."
        );

        let embed_started = std::time::Instant::now();
        // The per-batch progress closure: `elapsed_ms` is measured from *request*
        // start, not embed start, so a client's rate and ETA line up with what it
        // has watched since `started`.
        let embed_events = events.clone();
        let embed_progress = move |p: EmbedProgress| {
            if let Some(tx) = &embed_events {
                let _ = tx.send(IndexEvent::Embedded {
                    batch_chunks: p.batch_chunks,
                    chunks_done: p.chunks_done,
                    chunks_total: p.chunks_total,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
        };
        let embed_result = indexer.embed_all(&all_chunks, Some(&embed_progress)).await;
        m.index
            .phase_duration
            .get_or_create(&crate::backend::metrics::PhaseLabels { phase: "embed" })
            .observe(embed_started.elapsed().as_secs_f64());
        if embed_result.is_err() {
            let outcome = if matches!(embed_result, Err(EmbedUpsertError::Cancelled)) {
                "cancelled"
            } else {
                "failed"
            };
            for p in &prepared {
                file_outcome(p.pl, outcome);
            }
        }
        match embed_result {
            Ok(()) => {}
            Err(EmbedUpsertError::Timeout(budget)) => {
                error!(
                    ?budget,
                    "Embedder stayed busy for the whole call budget; marking batch \
                     'failed'. Sysadmin: the embedder is saturated — check its load, \
                     or raise [model].encode_timeout_ms."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(ApiError::EmbedderUnavailable);
            }
            Err(EmbedUpsertError::Cancelled) => {
                indexer.recover_all(&prepared, "cancelled", false).await;
                return Err(ApiError::Cancelled);
            }
            Err(EmbedUpsertError::Embed(request_err)) => {
                error!(
                    error = ?request_err,
                    "Embedding request failed; marking batch 'failed'. \
                     Check the model server at --model-server is up and reachable \
                     (from inside the container it must bind 0.0.0.0, not 127.0.0.1)."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(ApiError::EmbedderUnavailable);
            }
            Err(EmbedUpsertError::Decode(decode_err)) => {
                error!(
                    error = %decode_err,
                    "Embedder response decode failed; marking batch 'failed'. \
                     The embedder and mindex binary wire formats disagree — \
                     redeploy them from the same revision."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(ApiError::EmbedderUnavailable);
            }
            Err(EmbedUpsertError::Store(qdrant_err)) => {
                error!(
                    error = ?qdrant_err,
                    "Qdrant upsert failed; marking batch 'failed'. \
                     Check Qdrant is reachable at --qdrant-server."
                );
                indexer.recover_all(&prepared, "failed", true).await;
                return Err(ApiError::Internal);
            }
        }

        // ── Phase 3: mark each prepared file 'indexed' and tally the response.
        let mark_started = std::time::Instant::now();
        for (p, count) in prepared.iter().zip(counts) {
            if !indexer.mark_indexed(&p.path, &p.sha256).await? {
                // A `/cancel` landed after `drop_cancelled` re-read the statuses, so
                // this file is `cancelled` and its chunks are already marked deleted.
                // Saying `indexed` here — which is what an unchecked `UPDATE` used to
                // do — would report a file as indexed while the database says
                // otherwise, and the client would see no drift to correct it.
                info!(
                    path = %p.path,
                    "File was cancelled during the embed pass; not reporting it as indexed."
                );
                file_outcome(p.pl, "cancelled");
                emit(IndexEvent::Skipped {
                    path: p.path.clone(),
                    language: p.pl,
                    reason: SkipReason::Cancelled,
                });
                continue;
            }
            file_outcome(p.pl, "indexed");
            emit(IndexEvent::Indexed {
                path: p.path.clone(),
                language: p.pl,
                count,
            });
            *res.files
                .entry(p.pl)
                .or_default()
                .entry(p.path.clone())
                .or_insert(0) += count;
        }
        m.index
            .phase_duration
            .get_or_create(&crate::backend::metrics::PhaseLabels { phase: "mark" })
            .observe(mark_started.elapsed().as_secs_f64());

        info!("All files processed.");
        Ok(res)
    }
    .instrument(span)
    .await
}

/// Pure drift computation: classify each working-tree path against the server's
/// view. Kept separate from the handler so it is unit-testable without a DB.
///
/// `in_flight` is checked **first**: a file currently being indexed is reported
/// `indexing` and never `stale`/`missing`. Its stored `sha256` is the *incoming*
/// content's hash (written when the file enters `indexing`), but its vectors are
/// not ready yet, so it must be excluded from drift regardless — re-triggering it
/// would race the live batch.
fn compute_drift(
    indexed: &HashMap<String, String>,
    in_flight: &HashSet<String>,
    local: &HashMap<String, String>,
) -> DriftResponse {
    let mut out = DriftResponse::default();

    for (path, local_sha) in local {
        if in_flight.contains(path) {
            out.indexing.push(path.clone());
        } else if let Some(indexed_sha) = indexed.get(path) {
            if indexed_sha != local_sha {
                out.stale.push(path.clone());
            }
        } else {
            out.missing.push(path.clone());
        }
    }

    // Indexed but gone from the working tree — but an in-flight file absent locally
    // is left to settle, not called orphaned.
    for path in indexed.keys() {
        if !local.contains_key(path) && !in_flight.contains(path) {
            out.orphaned.push(path.clone());
        }
    }

    out.stale.sort();
    out.missing.sort();
    out.orphaned.sort();
    out.indexing.sort();
    out
}

/// Read the drift baseline from SQLite: `(indexed path→sha256, in-flight paths)`.
/// `failed`/`deleted` rows are excluded so their paths fall into `missing` (they do
/// need indexing); `indexed` carries a trustworthy hash, everything else is in flight.
async fn read_drift_baseline(
    s: &RouterState,
    token: &CancellationToken,
    project_guid: UUIDv4,
) -> Result<(HashMap<String, String>, HashSet<String>), ApiError> {
    let rows: Vec<(String, String, String)> = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let mut stmt = tx.prepare(
                "SELECT path, sha256, status FROM project_files
                 WHERE project_guid = ?1
                   AND status IN ('indexed', 'indexing', 'just_uploaded')",
            )?;
            let rows = stmt
                .query_map(params![project_guid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|err| {
            error!(
                error = ?err,
                project_guid = %project_guid.0,
                "Failed to read the project manifest for drift. Check the DB is writable."
            );
            ApiError::from(err)
        })?;

    let mut indexed: HashMap<String, String> = HashMap::new();
    let mut in_flight: HashSet<String> = HashSet::new();
    for (path, sha256, status) in rows {
        if status == "indexed" {
            indexed.insert(path, sha256);
        } else {
            in_flight.insert(path);
        }
    }
    Ok((indexed, in_flight))
}

/// `POST /projects/{guid}/drift` — compare the posted working-tree `path → sha256`
/// map against the index and return the divergence. Filesystem-agnostic: the client
/// walked and hashed; this only reads stored hashes. Unlike `post_search`, an empty
/// project is not a 404 — it just means every posted file is `missing`.
///
/// **Concurrency:** safe — pure read, takes no locks. In-flight files are reported as
/// `indexing` (never `stale`/`missing`) since their stored hash is the previous value.
#[utoipa::path(
    post,
    path = "/projects/{project_guid}/drift",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = DriftRequest,
    responses(
        (status = 200, description = "Working-tree divergence in four buckets (stale/missing/orphaned/indexing).", body = DriftResponse),
        (status = 400, description = "Validation failed (bad path, bad sha256, too many files).", body = ProblemDetails),
        (status = 499, description = "Client closed the connection.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_drift(
    DriftScope {
        guid: project_guid,
        in_scope,
    }: DriftScope,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<DriftRequest>,
) -> Result<Json<DriftResponse>, ApiError> {
    validate::validate_drift_request(&payload, s.max_drift_files)?;
    let guard = http3::CancellationGuard(CancellationToken::new());
    // A project this token cannot see answers exactly as a project that was never
    // indexed does — an empty baseline, so every posted file comes back `missing`.
    // This endpoint already documents that an unknown project is not a 404, so
    // reusing that path costs nothing and is what keeps it from being the one
    // route where a caller can distinguish "not mine" from "not there".
    let (indexed, in_flight) = if in_scope {
        read_drift_baseline(&s, &guard.0, project_guid).await?
    } else {
        Default::default()
    };
    let res = compute_drift(&indexed, &in_flight, &payload.files);

    // The one read endpoint whose *answer* is worth a log line. Drift is computed
    // from a manifest only the client can see, so when two clients disagree about
    // the same tree — the failure mode this endpoint exists to expose — this is the
    // only server-side record of what each of them claimed the tree contains.
    info!(
        project_guid = %project_guid.0,
        posted = payload.files.len(),
        baseline = indexed.len(),
        stale = res.stale.len(),
        missing = res.missing.len(),
        orphaned = res.orphaned.len(),
        indexing = res.indexing.len(),
        "Compared a working-tree manifest against the index."
    );

    // Counters, not gauges, and deliberately so: `/drift` compares against a
    // manifest only the *client* can produce — the server never walks a tree, so
    // there is no server-side drift level for a gauge to hold. A counter honestly
    // says "the checks that ran reported this much"; a gauge would claim to know
    // the tree between checks. A real drift gauge belongs to `mindex-watch`.
    let guid_label = project_guid.0.simple().to_string();
    s.metrics
        .index
        .drift_checks
        .get_or_create(&crate::backend::metrics::ProjectLabels {
            project_guid: guid_label.clone(),
        })
        .inc();
    for (class, n) in [
        ("stale", res.stale.len()),
        ("missing", res.missing.len()),
        ("orphaned", res.orphaned.len()),
        ("indexing", res.indexing.len()),
    ] {
        if n > 0 {
            s.metrics
                .index
                .drift_files
                .get_or_create(&crate::backend::metrics::ProjectClassLabels {
                    project_guid: guid_label.clone(),
                    class,
                })
                .inc_by(n as u64);
        }
    }

    Ok(Json(res))
}

/// Reconcile one project's git history against the posted commit set.
///
/// The whole operation is a **set difference on shas**, and that is the design
/// rather than an implementation detail: a sha is the hash of its own content,
/// so unlike a file there is no "same identity, different bytes" case to detect.
/// A commit the server holds and the request does not name is gone from the refs
/// the client tracks, whatever the reason — merged and pruned, rebased away,
/// force-pushed over. That is why history needs no equivalent of `/drift` and no
/// special handling for a rewritten branch.
///
/// `since` bounds the *deletion* half. Without it a client walking a window
/// ("the last month") would silently wipe everything older on every pass, since
/// from the server's side an unmentioned commit and a commit outside the walk
/// look identical.
async fn history_core(
    s: &RouterState,
    project_guid: UUIDv4,
    payload: HistoryRequest,
    token: &CancellationToken,
) -> Result<HistoryResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();

    s.db_pool
        .transaction(token.child_token(), move |tx| {
            reconcile_history(tx, project_guid, &model_id, &payload)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|err| {
            error!(
                error = ?err,
                project_guid = %project_guid.0,
                "Failed to reconcile the project's git history. Check the DB is writable."
            );
            ApiError::from(err)
        })
}

/// The reconciliation itself, over one transaction. Split out from
/// [`history_core`] so it can be exercised against a real `:memory:` pool —
/// `SQLite3Pool` is deliberately not a trait, so this is how the SQL gets
/// tested.
fn reconcile_history(
    tx: &rusqlite::Transaction,
    project_guid: UUIDv4,
    model_id: &str,
    payload: &HistoryRequest,
) -> Result<HistoryResponse, SQLite3PoolError> {
    // History may arrive before any file has been indexed — the git walk does
    // not depend on the working tree — so the project row cannot be assumed to
    // exist. Same ON CONFLICT DO NOTHING as `post_index`, and for the same
    // reason: two concurrent creators must not 500.
    tx.execute(
        "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)
         ON CONFLICT (guid, model_id) DO NOTHING",
        params![project_guid, model_id],
    )?;

    let mut indexed = 0usize;
    {
        let mut insert_commit = tx.prepare(
            "INSERT INTO project_commits
                 (project_guid, model_id, sha, author_name, author_email,
                  authored_at, committed_at, parent_count, subject, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (project_guid, model_id, sha) DO NOTHING",
        )?;
        let mut insert_path = tx.prepare(
            "INSERT INTO project_commit_paths
                 (project_guid, model_id, sha, path, change_type, old_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (project_guid, model_id, sha, path) DO NOTHING",
        )?;
        for c in &payload.commits {
            // A commit's content is immutable, so a row that already exists is
            // the same row: DO NOTHING makes a re-post a genuine no-op and this
            // count a real "new", not a euphemism for "attempted".
            indexed += insert_commit.execute(params![
                project_guid,
                model_id,
                c.sha,
                c.author_name,
                c.author_email,
                c.authored_at,
                c.committed_at,
                c.parent_count as i64,
                c.subject,
                c.body,
            ])?;
            for p in &c.paths {
                insert_path.execute(params![
                    project_guid,
                    model_id,
                    c.sha,
                    p.path,
                    p.change_type,
                    p.old_path,
                ])?;
            }
        }
    }

    // The posted set goes into a temp table rather than a `NOT IN (?, ?, …)`
    // list: that list is one bind per commit and would hit SQLite's variable
    // limit somewhere around 32k, which is inside the range
    // `[limits].max_history_commits` is allowed to permit. A temp table has no
    // such ceiling.
    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.posted_shas;
         CREATE TEMP TABLE posted_shas (sha TEXT NOT NULL PRIMARY KEY);",
    )?;
    {
        let mut stmt =
            tx.prepare("INSERT INTO temp.posted_shas (sha) VALUES (?1) ON CONFLICT DO NOTHING")?;
        for c in &payload.commits {
            stmt.execute(params![c.sha])?;
        }
    }

    // Paths go with their commit through ON DELETE CASCADE. Hard delete, not
    // soft: these rows own nothing outside SQLite, so there is no vector for a
    // GC pass to confirm gone first.
    let removed = tx.execute(
        "DELETE FROM project_commits
         WHERE project_guid = ?1
           AND model_id = ?2
           AND (?3 IS NULL OR committed_at >= ?3)
           AND sha NOT IN (SELECT sha FROM temp.posted_shas)",
        params![project_guid, model_id, payload.since],
    )?;
    tx.execute_batch("DROP TABLE IF EXISTS temp.posted_shas;")?;

    Ok(HistoryResponse {
        indexed,
        unchanged: payload.commits.len().saturating_sub(indexed),
        removed,
    })
}

/// `POST /v0/{guid}/history` — reconcile the project's commit history against
/// the set the client just walked out of git.
///
/// A **full-set replace within `since`**: commits the request names are inserted
/// if absent, and commits the server holds inside the window that the request
/// does not name are deleted. Because a sha is its own content hash there is no
/// update case at all, and a force-push, a rebase or any other history rewrite
/// is simply a reconciliation in which many shas orphan at once.
///
/// The commit rows deliberately do **not** become `project_files` rows — see the
/// schema comment above `project_commits`. The consequence worth stating at the
/// route: nothing posted here can ever appear in `POST /drift`, which is what
/// keeps `mindex-index --check` from reporting permanent, unclearable drift.
///
/// **Concurrency:** takes no `IndexClaim` and no locks — it touches neither
/// `project_files` nor Qdrant, so it cannot race indexing. One transaction, so a
/// failed request leaves the previous history intact.
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/history",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = HistoryRequest,
    responses(
        (status = 200, description = "Reconciliation counts (inserted / already held / dropped).", body = HistoryResponse),
        (status = 400, description = "Validation failed (bad sha, empty subject, oversized message, too many commits, bad path, old_path mismatch).", body = ProblemDetails),
        (status = 499, description = "Client closed the connection.", body = ProblemDetails),
        (status = 500, description = "SQLite write failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_history(
    IndexScope(project_guid, _auth): IndexScope,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<HistoryRequest>,
) -> Result<Json<HistoryResponse>, ApiError> {
    validate::validate_history_request(
        &payload,
        s.max_history_commits,
        s.max_commit_message_bytes,
    )?;
    let guard = http3::CancellationGuard(CancellationToken::new());
    let posted = payload.commits.len();
    let since = payload.since;
    let res = history_core(&s, project_guid, payload, &guard.0).await?;

    // Worth a log line for the same reason `/drift` is: the set being reconciled
    // against is one only the client can see, so this is the only server-side
    // record of what it claimed the tracked refs contain. `removed` is the
    // interesting column — a large value is a history rewrite, and nothing else
    // on the server would ever say one happened.
    info!(
        project_guid = %project_guid.0,
        posted,
        since,
        indexed = res.indexed,
        unchanged = res.unchanged,
        removed = res.removed,
        "Reconciled a git history manifest against the index."
    );

    Ok(Json(res))
}

/// The prune itself, over one transaction. Split out from [`delete_history`] for
/// the same reason [`reconcile_history`] is — `SQLite3Pool` is deliberately not
/// a trait, so a real `:memory:` pool is how this SQL gets tested.
///
/// The two bounds **intersect**: a commit dies only if `older_than` condemns it
/// *and* it is not among the newest `keep_last`. An absent bound is written as a
/// no-op rather than as a second query — `?3 IS NULL` disables the clock and
/// `LIMIT 0` protects nothing — so one statement serves all three shapes.
fn prune_history(
    tx: &rusqlite::Transaction,
    project_guid: UUIDv4,
    model_id: &str,
    q: &HistoryPruneQuery,
) -> Result<HistoryPruneResponse, SQLite3PoolError> {
    let removed = tx.execute(
        "DELETE FROM project_commits
         WHERE project_guid = ?1
           AND model_id = ?2
           AND (?3 IS NULL OR committed_at < ?3)
           AND sha NOT IN (
               SELECT sha FROM project_commits
               WHERE project_guid = ?1 AND model_id = ?2
               -- `sha` breaks the tie so a run is reproducible: same-second
               -- commits are common (a rebase stamps a whole branch at once),
               -- and an unordered LIMIT would keep an arbitrary one of them.
               ORDER BY committed_at DESC, sha DESC
               LIMIT ?4
           )",
        params![
            project_guid,
            model_id,
            q.older_than,
            q.keep_last.unwrap_or(0) as i64,
        ],
    )?;

    let remaining: i64 = tx.query_row(
        "SELECT COUNT(*) FROM project_commits WHERE project_guid = ?1 AND model_id = ?2",
        params![project_guid, model_id],
        |row| row.get(0),
    )?;

    Ok(HistoryPruneResponse {
        removed,
        remaining: remaining as usize,
    })
}

/// `DELETE /v0/{guid}/history` — prune the git-history channel by retention.
///
/// The counterpart to `POST`, which can only ever *reconcile*: it deletes what
/// the tracked refs no longer reach, so history that is still reachable never
/// ages out however old it gets. This is the handle for that — a retention
/// policy an operator or a cron applies, not something the indexer calls.
///
/// `keep_last=N` keeps the newest N commits; `older_than=<unix seconds>` deletes
/// what was committed before that instant; both together keep a commit that
/// *either* rule protects. Naming neither is a **400**, not a wipe.
///
/// Deleting is cheap and reversible in the only sense that matters: the source
/// of truth is the repository, so `mindex-index --history-only` rebuilds
/// whatever the refs still reach.
///
/// **Concurrency:** takes no `IndexClaim` and no locks — these rows own nothing
/// in Qdrant and are invisible to `/drift`, so this cannot race indexing. Hard
/// delete in one transaction, cascading to `project_commit_paths`; there is no
/// GC pass to wait for.
#[utoipa::path(
    delete,
    path = "/v0/{project_guid}/history",
    tag = "Indexing",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        HistoryPruneQuery,
    ),
    responses(
        (status = 200, description = "Commits deleted and commits still held.", body = HistoryPruneResponse),
        (status = 400, description = "Neither `keep_last` nor `older_than` was given.", body = ProblemDetails),
        (status = 499, description = "Client closed the connection.", body = ProblemDetails),
        (status = 500, description = "SQLite write failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn delete_history(
    DeleteScope(project_guid, _auth): DeleteScope,
    State(s): State<RouterState>,
    ApiQuery(q): ApiQuery<HistoryPruneQuery>,
) -> Result<Json<HistoryPruneResponse>, ApiError> {
    validate::validate_history_prune(&q)?;
    let guard = http3::CancellationGuard(CancellationToken::new());
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();

    let res = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            prune_history(tx, project_guid, &model_id, &q)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|err| {
            error!(
                error = ?err,
                project_guid = %project_guid.0,
                "Failed to prune the project's git history. Check the DB is writable."
            );
            ApiError::from(err)
        })?;

    info!(
        project_guid = %project_guid.0,
        removed = res.removed,
        remaining = res.remaining,
        "Pruned a project's git history."
    );

    Ok(Json(res))
}

/// Hybrid semantic + lexical code search within one project.
///
/// The query is embedded with BGE-M3 (dense + sparse + ColBERT). Candidate chunks are
/// the project's `active` chunks matching the optional `include`/`exclude` selector
/// (project isolation + soft-delete exclusion happen here, in SQLite). Qdrant then
/// prefetches top-200 dense + top-200 sparse, fuses with RRF, reranks with ColBERT
/// MaxSim, and returns the top-k. Results are sorted by score descending.
///
/// An empty candidate set (nothing indexed, or filtered to nothing) returns **404**
/// immediately without touching Qdrant.
///
/// **Concurrency:** safe — read-only, takes no locks; never blocks or is blocked by
/// indexing/GC. Honors client cancellation (**499**).
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/search",
    tag = "Search",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = SearchRequest,
    responses(
        (status = 200, description = "Ranked matches, sorted by score descending.", body = SearchResponse),
        (status = 400, description = "Validation failed (empty/oversized query, top_k out of range, oversized selector).", body = ProblemDetails),
        (status = 404, description = "No active chunks match (empty project or over-narrow filter).", body = ProblemDetails),
        (status = 499, description = "Client closed the connection.", body = ProblemDetails),
        (status = 500, description = "SQLite failure while building the candidate set or fetching display rows.", body = ProblemDetails),
        (status = 503, description = "The embedder or Qdrant is unreachable.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_search(
    SearchScope(project_guid, _auth): SearchScope,
    State(state): State<RouterState>,
    ApiJson(payload): ApiJson<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let span = info_span!("searching", project_guid = %project_guid.0);

    async move {
        validate::validate_query(&payload.query, state.max_query_bytes)?;
        validate::validate_top_k(payload.top_k, state.max_top_k)?;
        validate::validate_selector(&payload.include, state.max_selector_patterns)?;
        validate::validate_selector(&payload.exclude, state.max_selector_patterns)?;

        let guard = http3::CancellationGuard(CancellationToken::new());
        let results = search_core(&state, project_guid, &payload, &guard.0).await?;
        Ok(Json(SearchResponse { results }))
    }
    .instrument(span)
    .await
}

/// The `/search` core, shared by [`post_search`] and the research loop: embed the
/// query → SQLite candidate set → Qdrant hybrid search → display rows for the
/// top-k winners, sorted by score descending. `Err(NoMatch)` = empty candidate
/// set or no scored hits. Validation stays with the callers (the handler
/// validates client input; the research loop constructs requests itself).
pub(crate) async fn search_core(
    state: &RouterState,
    project_guid: UUIDv4,
    payload: &SearchRequest,
    token: &CancellationToken,
) -> Result<Vec<SearchResult>, ApiError> {
    let result = search_core_inner(state, project_guid, payload, token).await;
    // One recording point for every exit, so the outcome counter cannot drift
    // from the returned value as early-return paths are added. `NoMatch` is its
    // own outcome: an empty candidate set is an answer, not a failure.
    state
        .metrics
        .search
        .requests
        .get_or_create(&crate::backend::metrics::ProjectOutcomeLabels {
            project_guid: project_guid.0.simple().to_string(),
            outcome: match &result {
                Ok(_) => "hit",
                Err(ApiError::NoMatch) => "no_match",
                Err(_) => "error",
            },
        })
        .inc();
    if let Ok(hits) = &result {
        state.metrics.search.results.observe(hits.len() as f64);
    }
    result
}

async fn search_core_inner(
    state: &RouterState,
    project_guid: UUIDv4,
    payload: &SearchRequest,
    token: &CancellationToken,
) -> Result<Vec<SearchResult>, ApiError> {
    let stage = |name: &'static str, started: std::time::Instant| {
        state
            .metrics
            .search
            .stage_duration
            .get_or_create(&crate::backend::metrics::StageLabels { stage: name })
            .observe(started.elapsed().as_secs_f64());
    };
    let embed_started = std::time::Instant::now();
    // The query path deliberately uses `query_model`, not the indexing client:
    // when an operator has split the two, this is the instance that is not holding
    // the GPU. Same server, same fp32 numerics, so the vectors still agree with
    // the index side.
    let client = &state.query_model;
    let BGEm3EmbedResponse {
        dense_vecs,
        sparse_vecs,
        colbert_vecs,
    } = match client
        .encode(
            BGEm3EmbedRequest {
                texts: vec![payload.query.clone()],
            },
            token.clone(),
        )
        .await
    {
        Ok(val) => Ok(val),
        Err(EncodeError::Cancelled) => Err(ApiError::Cancelled),
        Err(EncodeError::Timeout(budget)) => {
            error!(
                ?budget,
                "Embedding the query ran past the whole-call budget while the embedder \
                 kept answering 'busy'. Sysadmin: the embedder is saturated — check its \
                 load, or raise [model].encode_timeout_ms."
            );
            Err(ApiError::EmbedderUnavailable)
        }
        Err(EncodeError::Request(request_err)) => {
            error!(
                error = ?request_err,
                "Failed to embed the search query. \
                 Check the model server at --model-server is up and reachable."
            );
            Err(ApiError::EmbedderUnavailable)
        }
        Err(EncodeError::Decode(decode_err)) => {
            error!(
                error = %decode_err,
                "Failed to decode the embedder's response for the search query. \
                 The embedder and mindex binary wire formats disagree — \
                 redeploy them from the same revision."
            );
            Err(ApiError::EmbedderUnavailable)
        }
    }?;

    stage("embed", embed_started);

    let candidates_started = std::time::Instant::now();
    let (sql, binds) = build_search_query(project_guid, payload);

    // Query 1 (candidate set): only the `qdrant_guid`s feeding Qdrant's `has_id`
    // filter — no `code`/metadata for the (potentially huge) full active set.
    let candidate_ids: Vec<UUIDv4> = state
        .db_pool
        .transaction(token.child_token(), move |tx| {
            tx.prepare(&sql)?
                .query_map(params_from_iter(binds), |row| row.get::<_, UUIDv4>(0))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|err| {
            if !matches!(err, SQLite3PoolError::Cancelled) {
                error!(error = %err, "Failed to query candidate chunks from SQLite.");
            }
            ApiError::from(err)
        })?;

    stage("candidates", candidates_started);
    state
        .metrics
        .search
        .candidates
        .observe(candidate_ids.len() as f64);

    if candidate_ids.is_empty() {
        return Err(ApiError::NoMatch);
    }

    // The embedder must return exactly one vector per head for the single query; an
    // empty list is an embedder contract violation, not a client error.
    let dense = dense_vecs
        .into_iter()
        .next()
        .ok_or(ApiError::EmbedderUnavailable)?;
    let sparse = sparse_vecs
        .into_iter()
        .next()
        .ok_or(ApiError::EmbedderUnavailable)?;
    let colbert = colbert_vecs
        .into_iter()
        .next()
        .ok_or(ApiError::EmbedderUnavailable)?;

    let qdrant_started = std::time::Instant::now();
    let search_hits = state
        .qdrant
        .search(
            &collection_for(project_guid),
            candidate_ids,
            dense,
            sparse.keys().copied().collect(),
            sparse.values().copied().collect(),
            colbert,
            payload
                .top_k
                .map(|k| k as u64)
                .unwrap_or(state.default_top_k),
        )
        .await
        .map_err(|err| {
            error!(
                error = ?err,
                "Qdrant query failed. Check Qdrant is reachable at --qdrant-server \
                 and the project's collection exists."
            );
            ApiError::QdrantUnavailable
        })?;
    stage("qdrant", qdrant_started);

    // Winners as (id, score), keeping Qdrant's order (we re-sort after the fetch).
    let scored: Vec<(UUIDv4, f32)> = search_hits
        .iter()
        .filter_map(|SearchHit { id, score }| match &id.point_id_options {
            Some(PointIdOptions::Uuid(uuid)) => match Uuid::parse_str(uuid) {
                Ok(uuid) => Some((UUIDv4(uuid), *score)),
                Err(err) => {
                    warn!(error = ?err, point_id = %uuid, "Qdrant returned a point id that is not a valid UUID; skipping it.");
                    None
                }
            },
            _ => None,
        })
        .collect();

    if scored.is_empty() {
        return Err(ApiError::NoMatch);
    }

    // Query 2 (display): fetch `code`/metadata for *only* the top-k winners.
    // (path, code, start_line, end_line, start_column, end_column) per winner id.
    type DisplayRows = std::collections::HashMap<UUIDv4, (String, String, i64, i64, i64, i64)>;
    let fetch_started = std::time::Instant::now();
    let winner_ids: Vec<UUIDv4> = scored.iter().map(|(uuid, _)| *uuid).collect();
    let display = state
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let placeholders = (1..=winner_ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT qdrant_guid, file_path, code, start_line, end_line, \
                        start_column, end_column
                 FROM project_file_chunks
                 WHERE status = 'active' AND qdrant_guid IN ({placeholders})"
            );
            tx.prepare(&sql)?
                .query_map(params_from_iter(winner_ids.iter()), |row| {
                    Ok((
                        row.get::<_, UUIDv4>(0)?,
                        (
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                        ),
                    ))
                })?
                .collect::<Result<DisplayRows, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|err| {
            if !matches!(err, SQLite3PoolError::Cancelled) {
                error!(error = %err, "Failed to fetch result rows from SQLite.");
            }
            ApiError::from(err)
        })?;
    stage("fetch", fetch_started);

    let mut results: Vec<SearchResult> = scored
        .iter()
        .filter_map(|(uuid, score)| {
            let (path, code, start_line, end_line, start_column, end_column) = display.get(uuid)?;
            Some(SearchResult {
                score: *score,
                path: path.clone(),
                code: code.clone(),
                start_line: *start_line as usize,
                end_line: *end_line as usize,
                start_column: *start_column as usize,
                end_column: *end_column as usize,
            })
        })
        .collect();

    // Winners whose chunk row was gone by the second query. Benign one at a time — the
    // candidate set was read from SQLite moments earlier, so a reindex soft-deleting a
    // chunk in between produces exactly this — but it was entirely silent, and the
    // all-orphaned case answered `200` with an empty list while an over-narrow filter
    // answers `404`. Two opposite spellings of "nothing", with the one meaning "the two
    // stores disagree" wearing the reassuring one.
    let orphaned = scored.len() - results.len();
    if orphaned > 0 {
        state
            .metrics
            .search
            .orphaned_winners
            .inc_by(orphaned as u64);
        warn!(
            orphaned,
            winners = scored.len(),
            "Qdrant scored chunks whose SQLite rows are gone; dropping them from the \
             results. A few mean a reindex raced this request; a steady rate means the \
             vector store and the index have diverged and the project needs reindexing."
        );
    }
    if results.is_empty() {
        return Err(ApiError::NoMatch);
    }

    let unscorable = rank_by_score(&mut results);
    if unscorable > 0 {
        state.metrics.search.unscorable_winners.inc_by(unscorable);
        warn!(
            unscorable,
            winners = results.len(),
            "The reranker scored chunks NaN; ranking them last rather than first. \
             Sysadmin: this is what a mismatched embedder produces — check both \
             instances are the same model at the same precision, and that the XPU \
             backend is off its default attention kernel (it returns NaN for padded \
             fp16 rows and still answers 200)."
        );
    }

    Ok(results)
}

/// Sort search results by score, best first, and report how many could not be
/// ordered at all.
///
/// **NaN must sort last, and this is not a theoretical concern.** `total_cmp` orders
/// `+NaN` above every finite value, so a plain descending sort by it hands the
/// *first* result slot to a chunk the reranker could not score — the top hit, the one
/// an agent reads and a human trusts. And NaN scores have a documented producer on
/// this hardware: the XPU backend's default attention kernel returns NaN for padded
/// fp16 rows and still answers 200 (`attention_backend()` in the embedder), as does
/// any split deployment whose two instances disagree about precision. The symptom
/// would be "search sometimes puts something irrelevant first", which reads as a
/// ranking-quality complaint rather than a broken embedder.
///
/// They are ranked last rather than dropped: the chunk matched the filters and the
/// candidate set, so it is a real answer with an unusable score, and silently
/// shortening the response is the failure mode the orphaned-winner counter exists to
/// stop repeating.
fn rank_by_score(results: &mut [SearchResult]) -> u64 {
    let unscorable = results.iter().filter(|r| r.score.is_nan()).count() as u64;
    results.sort_by(|a, b| match (a.score.is_nan(), b.score.is_nan()) {
        (false, false) => b.score.total_cmp(&a.score),
        // Ties among NaNs keep their relative order (`sort_by` is stable), so the
        // reranker's own sequence survives where nothing else can order them.
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
    });
    unscorable
}

/// `limit` used when a `/symbols` request omits it. Not configurable: it is a
/// response-shape default the client can always override (up to
/// `[limits].max_symbol_results`), not a tuning knob.
const DEFAULT_SYMBOL_LIMIT: usize = 20;

/// The `/symbols` lookup SQL + binds.
/// Ranking with an `anchor_path` is purely path-based and deterministic: same file
/// → 0, same directory (exact — not a deeper subtree) → 1, everything else → 2;
/// ties break by `path ASC, start_line ASC`. `COUNT(*) OVER ()` carries the full
/// total past the `LIMIT`.
/// The selector, when present, is appended as a `file_path IN (…)` subquery whose
/// binds land **last**, which is the placement every builder here uses so that a
/// caller can reason about bind positions without reading the selector's own SQL.
fn build_symbols_query(
    project_guid: UUIDv4,
    model_id: &str,
    req: &SymbolsRequest,
    limit: usize,
) -> (String, Vec<Bind>) {
    let mut sql = String::from(
        "SELECT file_path, kind, start_line, end_line, start_column, end_column,
                parent_name, parent_kind, doc, COUNT(*) OVER () AS total
         FROM project_file_symbols
         WHERE project_guid = ?1 AND model_id = ?2 AND name = ?3",
    );
    let mut binds: Vec<Bind> = vec![
        Bind::Guid(project_guid),
        Bind::Path(model_id.to_string()),
        Bind::Path(req.name.clone()),
    ];
    let mut next = 4;
    if let Some(kind) = &req.kind {
        sql.push_str(&format!(" AND kind = ?{next}"));
        binds.push(Bind::Path(kind.clone()));
        next += 1;
    }
    let scope = crate::research::ToolScope {
        include: req.include.clone(),
        exclude: req.exclude.clone(),
    };
    if scope.is_scoped() {
        let (scope_sql, scope_binds) = scope_subquery(project_guid, &scope, next);
        sql.push_str(&format!(" AND file_path IN ({scope_sql})"));
        next += scope_binds.len();
        binds.extend(scope_binds);
    }
    match &req.anchor_path {
        Some(anchor) => {
            let (a, d) = (next, next + 1);
            let dir = anchor
                .rfind('/')
                .map(|i| anchor[..=i].to_string())
                .unwrap_or_default();
            sql.push_str(&format!(
                " ORDER BY CASE
                      WHEN file_path = ?{a} THEN 0
                      WHEN (?{d} = '' AND instr(file_path, '/') = 0)
                        OR (?{d} != '' AND substr(file_path, 1, length(?{d})) = ?{d}
                            AND instr(substr(file_path, length(?{d}) + 1), '/') = 0)
                          THEN 1
                      ELSE 2
                  END, file_path ASC, start_line ASC"
            ));
            binds.push(Bind::Path(anchor.clone()));
            binds.push(Bind::Path(dir));
        }
        None => sql.push_str(" ORDER BY file_path ASC, start_line ASC"),
    }
    sql.push_str(&format!(" LIMIT {limit}"));
    (sql, binds)
}

/// Exact-name symbol lookup within one project.
///
/// Symbols (definitions, with kinds, enclosing definition and doc comments) are
/// extracted at indexing time from the **definition** tags of the language's
/// upstream tree-sitter tags query — purely syntactic, no type resolution. The
/// response is therefore a **candidate list, never a single answer**: an exact
/// name can legitimately have several definitions (same name in different
/// modules, overloads); `total_definitions` always carries the full count so a
/// truncated list is visible to the caller, who disambiguates.
///
/// It does **not** answer "who uses this name". The reference half of the table
/// was withdrawn in 1.1.0 — its edges were lexical, recording a token in call
/// position rather than which definition it binds to — and `grep` answers that
/// question lexically and says so.
///
/// Ranking is deterministic and purely path-based: with `anchor_path`, candidates
/// in that file come first, then its directory, then the rest; ties break by
/// `path ASC, start_line ASC`. An empty result is a valid answer ("this project
/// has no such symbol") and returns **200** with empty lists — unlike `/search`,
/// where an empty candidate set is a 404. An unknown project likewise just has
/// no symbols. Files whose language has no upstream tags query contribute none.
///
/// **Concurrency:** safe — read-only, takes no locks; never blocks or is blocked
/// by indexing/GC. Honors client cancellation (**499**).
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/symbols",
    tag = "Search",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = SymbolsRequest,
    responses(
        (status = 200, description = "Ranked candidate definitions and references (either list may be empty).", body = SymbolsResponse),
        (status = 400, description = "Validation failed (empty/oversized name, limit out of range).", body = ProblemDetails),
        (status = 499, description = "Client closed the connection.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_symbols(
    SearchScope(project_guid, _auth): SearchScope,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<SymbolsRequest>,
) -> Result<Json<SymbolsResponse>, ApiError> {
    validate::validate_symbols_request(
        &req.name,
        req.limit,
        s.max_symbol_name_bytes,
        s.max_symbol_results,
    )?;
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;

    let span = info_span!("symbols", project_guid = %project_guid.0, name = %req.name);
    async move {
        let guard = http3::CancellationGuard(CancellationToken::new());
        let resp = symbols_core(&s, project_guid, &req, &guard.0).await?;
        Ok(Json(resp))
    }
    .instrument(span)
    .await
}

/// The `/symbols` core, shared by [`post_symbols`] and the research loop: the
/// candidate query + assembly. Validation stays with the callers.
pub(crate) async fn symbols_core(
    s: &RouterState,
    project_guid: UUIDv4,
    req: &SymbolsRequest,
    token: &CancellationToken,
) -> Result<SymbolsResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let limit = req.limit.unwrap_or(DEFAULT_SYMBOL_LIMIT);

    let (sql, binds) = build_symbols_query(project_guid, model_id, req, limit);

    // What the selector hid. Only when there *is* a selector, and asked unscoped on
    // purpose: the point is the difference, and a scoped total cannot report what it
    // excluded. Without this, a run scoped to `docs/**` looking up a name defined in
    // `src/` reads exactly like a name that does not exist — and `/symbols` calls
    // that answer definitive.
    let scoped = req.include.is_some() || req.exclude.is_some();
    let unscoped_req = SymbolsRequest {
        name: req.name.clone(),
        kind: req.kind.clone(),
        // Ranking is irrelevant to a count, and an anchor would add two binds.
        anchor_path: None,
        limit: req.limit,
        include: None,
        exclude: None,
    };
    let count_sql = scoped.then(|| {
        // Only the totals matter here, and `COUNT(*) OVER ()` already carries them
        // past the LIMIT — so the same builder answers this with no second SQL to
        // keep in step.
        build_symbols_query(project_guid, model_id, &unscoped_req, limit)
    });

    let (definitions, total_definitions, out_of_scope_definitions) = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let unscoped_total = match &count_sql {
                Some((q, cb)) => tx
                    .prepare(q)?
                    .query_map(params_from_iter(cb.iter()), |r| {
                        r.get::<_, i64>(9).map(|n| n as u64)
                    })?
                    .next()
                    .transpose()?
                    .unwrap_or(0),
                None => 0,
            };
            let mut stmt = tx.prepare(&sql)?;
            let mut total: u64 = 0;
            let rows = stmt
                .query_map(params_from_iter(binds.iter()), |r| {
                    total = r.get::<_, i64>(9)? as u64;
                    Ok(SymbolInfo {
                        path: r.get(0)?,
                        kind: r.get(1)?,
                        start_line: r.get::<_, i64>(2)? as usize,
                        end_line: r.get::<_, i64>(3)? as usize,
                        start_column: r.get::<_, i64>(4)? as usize,
                        end_column: r.get::<_, i64>(5)? as usize,
                        parent_name: r.get(6)?,
                        parent_kind: r.get(7)?,
                        doc: r.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((rows, total, unscoped_total.saturating_sub(total)))
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|err| {
            if !matches!(err, SQLite3PoolError::Cancelled) {
                error!(error = %err, "Failed to query symbols from SQLite.");
            }
            ApiError::from(err)
        })?;

    Ok(SymbolsResponse {
        definitions,
        total_definitions,
        out_of_scope_definitions,
    })
}

// ─── /research ──────────────────────────────────────────────────────────────

/// Definitions per `outline` call. Generous because the point is *breadth* — a
/// truncated outline of a big file may omit exactly the name being looked for —
/// and the rows are metadata, not code: no embedder, no Qdrant, one indexed read.
const OUTLINE_LIMIT: usize = 300;
/// Paths per `list_files` call. Same reasoning; a path is a handful of bytes.
const LIST_FILES_LIMIT: usize = 300;

/// Does the run's scope admit this exact path?
///
/// One indexed read, and only when the run is actually scoped — an unscoped run must
/// pay nothing and must issue exactly the SQL it always did, which is what makes the
/// public `/symbols` endpoint sharing these cores provably unaffected.
///
/// Answers `true` for a path that is not indexed at all: "outside your scope" is a
/// claim about the *selector*, and letting a wrong path guess masquerade as a scope
/// violation would send the model looking for a wall that is not there. The caller's
/// own `indexed` read is what distinguishes those.
async fn path_in_scope(
    s: &RouterState,
    project_guid: UUIDv4,
    path: &str,
    scope: &crate::research::ToolScope,
    token: &CancellationToken,
) -> Result<bool, ApiError> {
    if !scope.is_scoped() {
        return Ok(true);
    }
    let (where_body, mut binds) = build_file_filter(project_guid, &scope.include, &scope.exclude);
    let n = binds.len() + 1;
    let sql = format!(
        "SELECT 1 FROM project_files WHERE {where_body} AND path = ?{n}
         UNION ALL
         SELECT 1 WHERE NOT EXISTS (
             SELECT 1 FROM project_files
             WHERE project_guid = ?1 AND path = ?{n} AND status != 'deleted')
         LIMIT 1"
    );
    binds.push(Bind::Path(path.to_string()));
    s.db_pool
        .transaction(token.child_token(), move |tx| {
            let mut stmt = tx.prepare(&sql)?;
            let hit: Option<i64> = stmt
                .query_map(params_from_iter(binds.iter()), |r| r.get(0))?
                .next()
                .transpose()?;
            Ok(hit.is_some())
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = %e,
                project_guid = %project_guid.0,
                %path,
                "Failed to check a path against the run's scope in SQLite."
            );
            ApiError::from(e)
        })
}

/// A file's definitions in source order — the cheap half of orientation.
///
/// Exists because the research loop's queries are natural language while the code
/// is written in identifiers: measured on this repo, an NL query retrieves the
/// *test* that describes a behaviour while the identifier that names its
/// implementation retrieves the implementation. A model that does not yet know a
/// name cannot ask the query that would work, and rephrasing burns its budget.
/// `outline` breaks that loop by handing over the names. Pure SQL over
/// `project_file_symbols` (covered by `idx_project_file_symbols_file`) — no GPU.
pub(crate) async fn outline_core(
    s: &RouterState,
    project_guid: UUIDv4,
    path: &str,
    scope: &crate::research::ToolScope,
    token: &CancellationToken,
) -> Result<OutlineResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();
    let owned_path = path.to_string();

    // A third read when the caller scoped the run, for the same reason there are
    // already two: "no such file", "indexed but with no definitions" and "indexed,
    // but outside what you asked for" are three different answers, and only the last
    // one is the caller's own doing. Skipped entirely when unscoped, so the public
    // endpoint's SQL is unchanged.
    let in_scope = path_in_scope(s, project_guid, path, scope, token).await?;
    if !in_scope {
        return Ok(OutlineResponse {
            path: path.to_string(),
            indexed: false,
            in_scope: false,
            programming_language: None,
            symbols: vec![],
            total_definitions: 0,
        });
    }

    let (language, rows): (Option<ProgrammingLanguage>, Vec<(OutlineSymbol, u64)>) = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            // Two reads, deliberately: "no such file" and "file with no symbols"
            // are different answers and a single query cannot tell them apart.
            let language: Option<ProgrammingLanguage> = tx
                .query_row(
                    "SELECT programming_language FROM project_files
                     WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3
                       AND status != 'deleted'",
                    (&project_guid, &model_id, &owned_path),
                    |r| r.get(0),
                )
                .optional()?;

            let mut stmt = tx.prepare(&format!(
                "SELECT name, kind, start_line, end_line, parent_name, parent_kind, doc,
                        COUNT(*) OVER () AS total
                 FROM project_file_symbols
                 WHERE project_guid = ?1 AND model_id = ?2 AND file_path = ?3
                 ORDER BY start_line ASC, name ASC
                 LIMIT {OUTLINE_LIMIT}"
            ))?;
            let rows = stmt
                .query_map((&project_guid, &model_id, &owned_path), |r| {
                    Ok((
                        OutlineSymbol {
                            name: r.get(0)?,
                            kind: r.get(1)?,
                            start_line: r.get::<_, i64>(2)? as usize,
                            end_line: r.get::<_, i64>(3)? as usize,
                            parent_name: r.get(4)?,
                            parent_kind: r.get(5)?,
                            doc: r.get(6)?,
                        },
                        r.get::<_, i64>(7)? as u64,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((language, rows))
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = %e,
                project_guid = %project_guid.0,
                path = %path,
                "Failed to read a file outline from SQLite."
            );
            ApiError::from(e)
        })?;

    let total_definitions = rows.first().map_or(0, |(_, t)| *t);
    Ok(OutlineResponse {
        path: path.to_string(),
        indexed: language.is_some(),
        programming_language: language,
        in_scope: true,
        symbols: rows.into_iter().map(|(sym, _)| sym).collect(),
        total_definitions,
    })
}

/// Chunks per `read_chunks` call, at `evidence_width` 1. Small: this returns
/// *code*, which is resent on every later turn, so it is the one research lookup
/// with a real context price — which is also why widening it is a per-request
/// grant a caller pays for in tokens, never a default.
const READ_CHUNKS_LIMIT: usize = 8;

/// The indexed code covering a line range of one file.
///
/// Exists because the loop could learn a location and then had no way to read it:
/// measured, a model handed `src/research.rs:445-624` by `symbols` spent its next
/// step *searching for the string* `"src/research.rs:445-624 research_inner"`.
///
/// **Chunk-backed, not file-backed, and it says so.** mindex stores no file text,
/// and chunk coverage is sparse by construction — the slicer emits nothing below
/// `min_chunk_tokens`, so imports, consts, type aliases and short helpers have no
/// chunk at all. Returning "" for those would tell the model the code is empty,
/// which is worse than telling it nothing; so the gaps are reported explicitly,
/// the same reason `outline` reports `indexed` separately from an empty symbol
/// list. Persisting file text to close the gap properly is a project (a table, a
/// storage cost, an invalidation surface), not a tool.
// One over clippy's arity line, and honestly so: every parameter is a distinct
// request axis (span, scope, width, cancellation) and a param struct would name
// this one call site's arguments and nothing else.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_chunks_core(
    s: &RouterState,
    project_guid: UUIDv4,
    path: &str,
    start_line: usize,
    end_line: usize,
    scope: &crate::research::ToolScope,
    limit: usize,
    token: &CancellationToken,
) -> Result<ReadChunksResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();
    let owned_path = path.to_string();

    // See `outline_core`: a refusal must not read as an empty range.
    if !path_in_scope(s, project_guid, path, scope, token).await? {
        return Ok(ReadChunksResponse {
            path: path.to_string(),
            indexed: false,
            in_scope: false,
            chunks: vec![],
        });
    }

    let (indexed, rows): (bool, Vec<ChunkExcerpt>) = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            // Same two-read reasoning as `outline_core`: "no such file" and "that
            // range has no chunk" are different answers.
            let indexed: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM project_files
                     WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3
                       AND status != 'deleted'",
                    (&project_guid, &model_id, &owned_path),
                    |r| r.get(0),
                )
                .optional()?;

            let mut stmt = tx.prepare(&format!(
                "SELECT start_line, end_line, code
                 FROM project_file_chunks
                 WHERE project_guid = ?1 AND model_id = ?2 AND file_path = ?3
                   AND status = 'active'
                   AND start_line <= ?5 AND end_line >= ?4
                 ORDER BY start_line ASC
                 LIMIT {limit}"
            ))?;
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        &project_guid,
                        &model_id,
                        &owned_path,
                        start_line as i64,
                        end_line as i64
                    ],
                    |r| {
                        Ok(ChunkExcerpt {
                            start_line: r.get::<_, i64>(0)? as usize,
                            end_line: r.get::<_, i64>(1)? as usize,
                            code: r.get(2)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((indexed.is_some(), rows))
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = %e,
                project_guid = %project_guid.0,
                path = %path,
                "Failed to read indexed chunks from SQLite."
            );
            ApiError::from(e)
        })?;

    Ok(ReadChunksResponse {
        path: path.to_string(),
        indexed,
        in_scope: true,
        chunks: rows,
    })
}

/// Commits per `file_history` call, at `evidence_width` 1.
///
/// Small where `outline`'s is large, and for the opposite reason: an outline is a
/// list of names and this is a list of prose. Twenty commit messages of this
/// repository's median length is already ~5k tokens of transcript, and the recent
/// ones are what answer "why is this the way it is" — a longer tail buys
/// archaeology nobody asked for at the price of the budget that would have read
/// the code. Transcript shape, not tuning, so the base is a const like
/// `GREP_LIMIT`; the per-request `evidence_width` grant is the one way to buy
/// the archaeology knowingly.
const FILE_HISTORY_LIMIT: usize = 20;

/// The commits that touched one path, newest first — the git channel's only
/// model-facing lookup.
///
/// Pure SQL over `project_commit_paths ⋈ project_commits`, covered by
/// `idx_project_commit_paths_path`: no embedder, no Qdrant, no HTTP handler of
/// its own, exactly like `outline_core` and `list_files_core`.
///
/// **Three flags, because an empty list has three different meanings** and the
/// bare `[]` reads as the least alarming one. The project's history may never
/// have been reconciled (`history_indexed: false` — not "this file is
/// uninteresting"); the run may not be allowed to read here (`in_scope: false` —
/// a refusal, since this is a path-keyed lookup); and the path may be one the
/// code channel does not hold (`path_indexed: false`), which is *normal* here
/// and nowhere else: a commit legitimately names files deleted years ago,
/// excluded by `.mindex`, or in an unsupported language. That last one is why
/// `project_commit_paths.path` carries no foreign key.
pub(crate) async fn file_history_core(
    s: &RouterState,
    project_guid: UUIDv4,
    path: &str,
    scope: &crate::research::ToolScope,
    limit: usize,
    token: &CancellationToken,
) -> Result<FileHistoryResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();
    let owned_path = path.to_string();

    // Path-keyed, so an out-of-scope path is refused rather than silently
    // emptied: an empty answer would tell the model this file has no history,
    // and send it hunting for other spellings of a path it simply may not read.
    if !path_in_scope(s, project_guid, path, scope, token).await? {
        return Ok(FileHistoryResponse {
            path: path.to_string(),
            history_indexed: true,
            in_scope: false,
            path_indexed: false,
            commits: vec![],
            total: 0,
        });
    }

    let (history_indexed, path_indexed, total, commits) = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            read_file_history(tx, project_guid, &model_id, &owned_path, limit)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|err| {
            error!(
                error = ?err,
                project_guid = %project_guid.0,
                path = %path,
                "Failed to read a file's commit history. Check the DB is writable."
            );
            ApiError::from(err)
        })?;

    Ok(FileHistoryResponse {
        path: path.to_string(),
        history_indexed,
        in_scope: true,
        path_indexed,
        commits,
        total,
    })
}

/// The three reads behind [`file_history_core`], over one transaction. Split out
/// for the same reason [`reconcile_history`] is: `SQLite3Pool` is deliberately
/// not a trait, so a real `:memory:` pool is the only way to test the SQL.
#[allow(clippy::type_complexity)]
fn read_file_history(
    tx: &rusqlite::Transaction,
    project_guid: UUIDv4,
    model_id: &str,
    owned_path: &str,
    limit: usize,
) -> Result<(bool, bool, usize, Vec<CommitSummary>), SQLite3PoolError> {
    // Does this project have a history channel at all? One indexed
    // existence check, and the single most load-bearing read here:
    // without it, "nobody reconciled this repository's commits" and
    // "nothing ever touched this file" are the same answer.
    let history_indexed: bool = tx
        .query_row(
            "SELECT 1 FROM project_commits
                     WHERE project_guid = ?1 AND model_id = ?2 LIMIT 1",
            params![project_guid, model_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    // Whether the code channel currently holds the path, so the answer
    // can distinguish "gone from the tree" from "never there".
    let path_indexed: bool = tx
        .query_row(
            "SELECT 1 FROM project_files
                     WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3
                       AND status != 'deleted' LIMIT 1",
            params![project_guid, model_id, owned_path],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    // Counted before the cap so truncation is visible rather than
    // implied by a list that happens to be exactly as long as the cap.
    let total: usize = tx.query_row(
        "SELECT COUNT(*) FROM project_commit_paths
                 WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3",
        params![project_guid, model_id, owned_path],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let mut stmt = tx.prepare(
        "SELECT c.sha, c.authored_at, c.author_name, c.subject, c.body,
                        p.change_type, p.old_path
                 FROM project_commit_paths p
                 JOIN project_commits c
                   ON c.project_guid = p.project_guid
                  AND c.model_id = p.model_id
                  AND c.sha = p.sha
                 WHERE p.project_guid = ?1 AND p.model_id = ?2 AND p.path = ?3
                 ORDER BY c.committed_at DESC
                 LIMIT ?4",
    )?;
    let commits = stmt
        .query_map(
            params![project_guid, model_id, owned_path, limit as i64],
            |r| {
                let sha: String = r.get(0)?;
                Ok(CommitSummary {
                    short_sha: sha.chars().take(8).collect(),
                    sha,
                    authored_at: r.get(1)?,
                    author_name: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    change_type: r.get(5)?,
                    old_path: r.get(6)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok((history_indexed, path_indexed, total, commits))
}

/// Indexed paths matching a glob — the other half of orientation (see
/// [`outline_core`]).
///
/// The glob is evaluated by **SQLite `GLOB`**, the same operator the `/search` and
/// `/files` path filters use, so this adds no fifth glob dialect. Note that
/// SQLite's `*` crosses `/` (unlike the `.mindex` contract, which is enforced
/// client-side) — so `src/*` also matches `src/db/qdrant.rs`. For a research model
/// asking "what is in src/" that is the *helpful* reading, but it is a divergence,
/// not an accident. `include`/`exclude` from the research request are applied too:
/// a caller that scoped the run must not be able to list its way out of that scope.
pub(crate) async fn list_files_core(
    s: &RouterState,
    project_guid: UUIDv4,
    glob: &str,
    scope: &crate::research::ToolScope,
    token: &CancellationToken,
) -> Result<ListFilesResponse, ApiError> {
    let (where_body, mut binds) = build_file_filter(project_guid, &scope.include, &scope.exclude);
    let n = binds.len() + 1;
    let sql = format!(
        "SELECT path, programming_language, COUNT(*) OVER () AS total
         FROM project_files
         WHERE {where_body} AND path GLOB ?{n}
         ORDER BY path ASC
         LIMIT {LIST_FILES_LIMIT}"
    );
    binds.push(Bind::Path(glob.to_string()));

    let rows: Vec<(FileListing, u64)> = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(binds.iter()), |r| {
                    Ok((
                        FileListing {
                            path: r.get(0)?,
                            programming_language: r.get(1)?,
                        },
                        r.get::<_, i64>(2)? as u64,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = %e,
                project_guid = %project_guid.0,
                %glob,
                "Failed to list project files from SQLite."
            );
            ApiError::from(e)
        })?;

    let total = rows.first().map_or(0, |(_, t)| *t);
    Ok(ListFilesResponse {
        files: rows.into_iter().map(|(f, _)| f).collect(),
        total,
    })
}

/// Matches per `grep` call, at `evidence_width` 1. Tighter than `list_files`'s 300
/// because each carries a line of source rather than a path, and every hit is
/// resent on every later turn.
const GREP_LIMIT: usize = 20;
/// Shortest literal `grep` will look for.
///
/// Not politeness: the query is a `LIKE '%…%'` over the biggest column in the schema,
/// so a one- or two-character pattern is a full scan of the corpus that returns
/// everything and tells the model nothing.
pub(crate) const GREP_MIN_PATTERN_CHARS: usize = 3;
/// Characters of the matching line reported per hit.
const GREP_EXCERPT_CHARS: usize = 200;

/// Escape the wildcards SQLite's `LIKE` would otherwise honour inside a literal.
///
/// Mandatory rather than defensive. `_` matches any character, and this codebase's
/// identifiers are full of it — an unescaped `read_chunks` also matches `readXchunks`,
/// so the tool would quietly answer a question nobody asked. `\` first, or the escapes
/// added below would themselves be escaped.
fn like_escape(pattern: &str) -> String {
    pattern
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Exact literal search over the indexed chunk text.
///
/// The gap `search` and `symbols` leave between them: `search` embeds the query and
/// matches *meaning*, which cannot find a specific string, and `symbols` only knows
/// names its language's tags query tagged — so an error code, a config key, a magic
/// constant or a string literal was unfindable. This reads the bytes the index already
/// holds: pure SQL, no embedder, no Qdrant, no new table.
///
/// Case-**insensitive** (`LIKE` is, for ASCII), which is the useful default for "does
/// this string appear anywhere"; exact case-sensitive identifier lookup is what
/// `/symbols` is for.
///
/// The cost is honest and bounded rather than hidden: a `LIKE` over
/// `project_file_chunks.code` scans the largest column in the schema, so the scope
/// subquery narrows `file_path` first, `GREP_LIMIT` stops the read early on a hit, and
/// `GREP_MIN_PATTERN_CHARS` rejects the degenerate pattern. The worst case left is one
/// unscoped no-match scan per step, on the research runtime. FTS5 is the real answer
/// and is deliberately not built yet: it is a table plus an invalidation surface, which
/// is a project, not a tool.
pub(crate) async fn grep_core(
    s: &RouterState,
    project_guid: UUIDv4,
    pattern: &str,
    glob: Option<&str>,
    scope: &crate::research::ToolScope,
    limit: usize,
    token: &CancellationToken,
) -> Result<GrepResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();

    // The scope goes first so its placeholders are `?1..`, leaving the chunk-level
    // binds to be numbered after it — see `scope_subquery`.
    let (scope_sql, mut binds) = scope_subquery(project_guid, scope, 1);
    let mut n = binds.len() + 1;
    let (p_guid, p_model, p_pattern) = (n, n + 1, n + 2);
    n += 3;
    binds.push(Bind::Guid(project_guid));
    binds.push(Bind::Path(model_id.clone()));
    binds.push(Bind::Path(format!("%{}%", like_escape(pattern))));
    let glob_clause = match glob {
        Some(g) => {
            binds.push(Bind::Path(g.to_string()));
            format!(" AND c.file_path GLOB ?{n}")
        }
        None => String::new(),
    };
    let sql = format!(
        "SELECT c.file_path, c.start_line, c.end_line, c.code, COUNT(*) OVER () AS total
         FROM project_file_chunks c
         WHERE c.project_guid = ?{p_guid} AND c.model_id = ?{p_model}
           AND c.status = 'active'
           AND c.code LIKE ?{p_pattern} ESCAPE '\\'
           AND c.file_path IN ({scope_sql}){glob_clause}
         ORDER BY c.file_path ASC, c.start_line ASC
         LIMIT {limit}"
    );

    // How many matches the scope hid. One extra query, and only when the run is
    // scoped: a filtered list whose total silently shrinks is indistinguishable from
    // a string that simply occurs less often.
    let out_of_scope_sql = scope.is_scoped().then_some(
        "SELECT COUNT(*) FROM project_file_chunks c
             WHERE c.project_guid = ?1 AND c.model_id = ?2 AND c.status = 'active'
               AND c.code LIKE ?3 ESCAPE '\\'",
    );
    let unscoped_binds = [
        Bind::Guid(project_guid),
        Bind::Path(model_id.clone()),
        Bind::Path(format!("%{}%", like_escape(pattern))),
    ];

    // How much was in reach at all — the same scope and glob, minus the pattern.
    //
    // Read only on a miss, the `out_of_scope` probe's rule: a second scan of the
    // biggest column in the schema is worth paying for exactly when it changes the
    // answer, and on a hit it changes nothing. It changes a great deal on a miss:
    // "no indexed chunk contains this" and "nothing here was searchable" are
    // different facts and were reported with the same sentence, so a glob that
    // matched no file read as proof that a literal does not exist. That is the
    // `file_history` three-flag problem in counter form — and it is what makes one
    // run report 0 occurrences of a string another run finds five times.
    let (cov_scope_sql, mut cov_binds) = scope_subquery(project_guid, scope, 1);
    let mut cn = cov_binds.len() + 1;
    let (c_guid, c_model) = (cn, cn + 1);
    cn += 2;
    cov_binds.push(Bind::Guid(project_guid));
    cov_binds.push(Bind::Path(model_id));
    let cov_glob_clause = match glob {
        Some(g) => {
            cov_binds.push(Bind::Path(g.to_string()));
            format!(" AND c.file_path GLOB ?{cn}")
        }
        None => String::new(),
    };
    let coverage_sql = format!(
        "SELECT COUNT(*), COUNT(DISTINCT c.file_path)
         FROM project_file_chunks c
         WHERE c.project_guid = ?{c_guid} AND c.model_id = ?{c_model}
           AND c.status = 'active'
           AND c.file_path IN ({cov_scope_sql}){cov_glob_clause}"
    );

    let owned_pattern = pattern.to_string();
    #[allow(clippy::type_complexity)]
    let (rows, total, all, coverage): (Vec<GrepMatch>, u64, u64, Option<(u64, u64)>) = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let mut stmt = tx.prepare(&sql)?;
            let rows: Vec<(String, usize, usize, String, u64)> = stmt
                .query_map(params_from_iter(binds.iter()), |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, i64>(1)? as usize,
                        r.get::<_, i64>(2)? as usize,
                        r.get(3)?,
                        r.get::<_, i64>(4)? as u64,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let total = rows.first().map_or(0, |r| r.4);
            let all = match out_of_scope_sql {
                Some(q) => tx.query_row(q, params_from_iter(unscoped_binds.iter()), |r| {
                    r.get::<_, i64>(0).map(|n| n as u64)
                })?,
                None => total,
            };
            // Located in Rust rather than in SQL: finding the matching line means
            // counting newlines before the first hit, which SQLite cannot do without
            // a recursive CTE nobody would want to read.
            let matches = rows
                .into_iter()
                .map(|(path, start_line, end_line, code, _)| {
                    let (match_line, excerpt) = locate_match(&code, &owned_pattern, start_line);
                    GrepMatch {
                        path,
                        start_line,
                        end_line,
                        match_line,
                        excerpt,
                    }
                })
                .collect();
            let coverage = if total == 0 {
                Some(
                    tx.query_row(&coverage_sql, params_from_iter(cov_binds.iter()), |r| {
                        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
                    })?,
                )
            } else {
                None
            };
            Ok((matches, total, all, coverage))
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = %e,
                project_guid = %project_guid.0,
                "Failed to grep indexed chunks in SQLite."
            );
            ApiError::from(e)
        })?;

    Ok(GrepResponse {
        matches: rows,
        total,
        out_of_scope: all.saturating_sub(total),
        searched_chunks: coverage.map(|c| c.0),
        searched_files: coverage.map(|c| c.1),
    })
}

/// The line a literal first occurs on within a chunk, and that line, trimmed.
///
/// `start_line` is the chunk's first line, so the offset is added to it. A pattern
/// that is not found (only possible if `LIKE`'s notion of matching and Rust's diverge)
/// falls back to the chunk's own start and first line, which is still true.
fn locate_match(code: &str, pattern: &str, start_line: usize) -> (usize, String) {
    let lower_code = code.to_lowercase();
    let offset = lower_code.find(&pattern.to_lowercase()).unwrap_or(0);
    // Counted in `lower_code`, never in `code`: lowercasing is not length-preserving
    // (`İ` U+0130 is two bytes and lowercases to three), so `offset` is a byte index
    // into the *lowered* string only. Slicing the original with it panics — out of
    // bounds, or mid-character — and this runs inside `spawn_blocking`, where a panic
    // costs a pool connection and reaches the client as a 499. The line number
    // survives the detour because no character lowercases into or out of a newline,
    // so the two strings hold the same newlines in the same order.
    let line_index = lower_code[..offset].matches('\n').count();
    let line = code.lines().nth(line_index).unwrap_or("").trim();
    let excerpt = if line.chars().count() > GREP_EXCERPT_CHARS {
        format!(
            "{}…",
            line.chars().take(GREP_EXCERPT_CHARS).collect::<String>()
        )
    } else {
        line.to_string()
    };
    (start_line + line_index, excerpt)
}

/// Paths per `file_versions` query. Only a guard against SQLite's bound-parameter
/// limit — the probe asks about every path a run has been shown, and chunking is
/// how it does that without a cap that would silently stop reporting staleness for
/// the later half of a long run's evidence.
const FILE_VERSIONS_CHUNK: usize = 400;

/// The index's own version of a set of files — the research freshness probe.
///
/// Not an HTTP endpoint and not a model-facing tool (see
/// [`ResearchTools::file_versions`](crate::research::ResearchTools::file_versions)):
/// a research run reads the index for up to half an hour while `mindex-index` and
/// `mindex-watch` keep writing to it, and nothing serializes the two. This is what
/// lets the run notice, and say, that a file it read has been reindexed since.
///
/// A `deleted` file is reported as absent rather than with its last hash: from a
/// reader's side "this file left the index" and "this file was never here" are the
/// same fact, and both mean the evidence must not be cited as current.
pub(crate) async fn file_versions_core(
    s: &RouterState,
    project_guid: UUIDv4,
    paths: Vec<String>,
    token: &CancellationToken,
) -> Result<Vec<crate::research::FileVersion>, ApiError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();
    let asked = paths.len();

    let rows: Vec<crate::research::FileVersion> = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let mut out = Vec::with_capacity(paths.len());
            for chunk in paths.chunks(FILE_VERSIONS_CHUNK) {
                // `?1`/`?2` pin the project; the paths follow, numbered from 3.
                let placeholders = (0..chunk.len())
                    .map(|i| format!("?{}", i + 3))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT path, sha256, status FROM project_files
                     WHERE project_guid = ?1 AND model_id = ?2
                       AND status != 'deleted'
                       AND path IN ({placeholders})"
                );
                let mut binds: Vec<Bind> = Vec::with_capacity(chunk.len() + 2);
                binds.push(Bind::Guid(project_guid));
                binds.push(Bind::Path(model_id.clone()));
                binds.extend(chunk.iter().map(|p| Bind::Path(p.clone())));

                let mut stmt = tx.prepare(&sql)?;
                let got = stmt
                    .query_map(params_from_iter(binds.iter()), |r| {
                        let status: String = r.get(2)?;
                        Ok(crate::research::FileVersion {
                            path: r.get(0)?,
                            sha256: r.get(1)?,
                            in_flight: status == "indexing",
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                out.extend(got);
            }
            Ok(out)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = %e,
                project_guid = %project_guid.0,
                asked,
                "Failed to read indexed file versions from SQLite (research freshness \
                 probe). Check the database is readable."
            );
            ApiError::from(e)
        })?;

    Ok(rows)
}

/// How many stored reports one `list_research` call may return.
///
/// Deliberately a `const`, not a config knob: the bound exists because the reply
/// is prompt tokens on every later turn, which is a property of the tool-loop
/// design and not of any deployment's hardware. The stored corpus itself is
/// bounded by retention, so 50 rows is "everything recent" in practice.
const LIST_RESEARCH_LIMIT: usize = 50;

/// The `list_research` tool's core: valid stored runs of one project, newest
/// first. Invalid runs (stale, or resting on a deleted/stale run) are excluded
/// here rather than flagged — a model has no business reading prose the validity
/// graph has already condemned.
pub(crate) async fn list_research_core(
    s: &RouterState,
    project_guid: UUIDv4,
    query: Option<String>,
    token: &CancellationToken,
) -> Result<Vec<crate::research::ResearchListing>, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    let pg = project_guid.0.simple().to_string();

    let rows = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let filter = query
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .map(|q| format!("%{}%", like_escape(q)));
            let sql = format!(
                "{ctes}
                 SELECT r.id, r.seq, r.title, r.question, r.created_at, r.kind,
                        {trust}
                   FROM research_runs r
                  WHERE r.project_guid = ?1
                    AND NOT EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = r.id)
                    {q_clause}
                  ORDER BY r.seq DESC
                  LIMIT {limit}",
                ctes = research_validity_ctes("?1", "?2"),
                q_clause = if filter.is_some() {
                    "AND (r.title LIKE ?3 ESCAPE '\\' OR r.question LIKE ?3 ESCAPE '\\')"
                } else {
                    ""
                },
                limit = LIST_RESEARCH_LIMIT,
                trust = research_trust_column(),
            );
            let mut stmt = tx.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&pg, &model_id];
            if let Some(f) = &filter {
                binds.push(f);
            }
            let out = stmt
                .query_map(binds.as_slice(), |row| {
                    Ok(crate::research::ResearchListing {
                        id: row.get(0)?,
                        seq: row.get(1)?,
                        title: row.get(2)?,
                        question: row.get(3)?,
                        created_at: row.get(4)?,
                        kind: row.get(5)?,
                        trust: row.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(out)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to list stored research for the research loop. Check the \
                 database is readable."
            );
            ApiError::from(e)
        })?;
    Ok(rows)
}

/// The `read_research` tool's core: one stored run's report, by per-project seq —
/// but only when the validity graph still vouches for it.
pub(crate) async fn read_research_core(
    s: &RouterState,
    project_guid: UUIDv4,
    seq: i64,
    token: &CancellationToken,
) -> Result<crate::research::StoredReport, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    let pg = project_guid.0.simple().to_string();

    let found = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let sql = format!(
                "{ctes}
                 SELECT r.question, r.report,
                        EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = r.id),
                        {trust}
                   FROM research_runs r
                  WHERE r.project_guid = ?1 AND r.seq = ?3",
                ctes = research_validity_ctes("?1", "?2"),
                trust = research_trust_column(),
            );
            let row = tx
                .query_row(&sql, rusqlite::params![pg, model_id, seq], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .optional()?;
            Ok(row)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                seq,
                "Failed to read a stored research report for the research loop. \
                 Check the database is readable."
            );
            ApiError::from(e)
        })?;
    Ok(match found {
        None => crate::research::StoredReport::Missing { seq },
        Some((_, _, true, _)) => crate::research::StoredReport::Invalid { seq },
        Some((question, report, false, trust)) => crate::research::StoredReport::Found {
            seq,
            question,
            report,
            trust,
        },
    })
}

/// Production [`ResearchTools`]: the research loop's index lookups are direct
/// internal calls to the `/search` and `/symbols` cores — no HTTP back to self.
struct StateResearchTools {
    state: RouterState,
    project_guid: UUIDv4,
    /// The run's `budget.evidence_width` grant. Constant per run, like
    /// `project_guid`, which is why it lives here rather than travelling on
    /// every call: the trait's surface stays untouched and no caller can pass
    /// a different width than the one the request was granted.
    evidence_width: u64,
}

/// One per-call evidence width, scaled by the run's `evidence_width` grant.
///
/// `max(1)` is defensive only — validation refuses 0 — so a malformed width can
/// never zero a tool; `saturating_mul` for the same reason on the other end.
fn scaled_width(base: usize, width: u64) -> usize {
    base.saturating_mul(width.max(1) as usize)
}

/// Longest derived title, in characters. This is the *fallback* rendering, cut
/// from the question rather than stored: nothing joins or sorts on it, the search
/// already runs over the whole `question`, and deriving keeps it true forever — a
/// stored copy of a *truncation* would go stale the day the rule changed. The
/// preferred title is `research_runs.title`, the report's own heading, which IS
/// stored because it is the model's output and not a derivation.
const RESEARCH_TITLE_CHARS: usize = 72;

/// A run's fallback display title: the question's first line, collapsed and cut at
/// a word boundary. Used when the run journalled no title of its own.
///
/// Cutting at a word boundary rather than mid-token is not decoration — a list of
/// titles is scanned, and a truncation that severs an identifier ("post_ind…") reads
/// as a different symbol.
pub(crate) fn research_title(question: &str) -> String {
    let line = question
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= RESEARCH_TITLE_CHARS {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(RESEARCH_TITLE_CHARS).collect();
    let head = match cut.rfind(' ') {
        // Only honour the word boundary if it leaves something to read; a question
        // whose first word is longer than the cap would otherwise become "…".
        Some(i) if i >= RESEARCH_TITLE_CHARS / 2 => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{}…", head.trim_end())
}

/// How many of a stored run's baseline files no longer match the index, and how many
/// it read in total.
///
/// This is the whole staleness computation, and it is the same comparison
/// [`Evidence::apply_versions`](crate::research::Evidence) makes during a live run —
/// asked later, of a run that has already finished. `sha256 IS NULL` folds "the file
/// was deleted" into "the file changed" on purpose: to a reader of a stored report
/// they are one fact, that what it describes no longer holds, which is exactly what
/// `Evidence::is_stale` means by `changed || removed`.
///
/// The `model_id` bind is load-bearing and easy to omit: `project_files` is keyed
/// `(project_guid, model_id, path)`, so joining on the path alone would match a run's
/// baseline against every embedding model the database has ever held.
/// `model_bind` is the positional placeholder (`"?2"`) the caller has bound the
/// embedding model id to; the two readers number their parameters differently, and a
/// hardcoded index here would silently mis-bind one of them.
fn research_staleness_columns(model_bind: &str) -> String {
    format!(
        "(SELECT COUNT(*) FROM research_run_files rf WHERE rf.run_id = r.id) AS files_total,
         (SELECT COUNT(*)
            FROM research_run_files rf
            LEFT JOIN project_files pf
                   ON pf.project_guid = r.project_guid
                  AND pf.model_id     = {model_bind}
                  AND pf.path         = rf.path
                  AND pf.status      != 'deleted'
           WHERE rf.run_id = r.id
             AND (pf.sha256 IS NULL OR pf.sha256 <> rf.sha256)) AS files_moved"
    )
}

/// The `WITH` block computing per-run *validity* for one project's stored research.
///
/// A run is valid when its own evidence still matches the index AND every run in
/// its context chain still exists and is itself valid. Nothing is materialized:
/// staleness can heal (a file reindexed back to the same bytes), and a deleted
/// parent — hard `DELETE` or the GC retention sweep, the two are the same event
/// here — is a dangling id in `context_run_ids_json`, so the cascade is immediate
/// by construction rather than by a write someone must remember to make.
///
/// The recursion cannot loop: a run's context is validated to exist at launch and
/// its own row does not exist yet, so every edge points to a strictly earlier row;
/// SQLite's recursive `UNION` deduplicates besides. `json_each` over
/// `context_run_ids_json` is the sanctioned exception to "read whole, never joined
/// on": the corpus is one project's retained runs, two orders of magnitude smaller
/// than anything the search path touches.
///
/// `guid_bind`/`model_bind` are the caller's positional placeholders for the
/// project guid (simple form) and the EMBEDDING model id — `"?1"`/`"?2"` for every
/// reader today, parameterised for the reason `research_staleness_columns` is.
/// Ids are compared exactly as stored (hyphenated `Uuid::to_string()` on both
/// sides); do not normalise them here.
fn research_validity_ctes(guid_bind: &str, model_bind: &str) -> String {
    format!(
        "WITH moved AS (
             SELECT r.id AS run_id,
                    (SELECT COUNT(*)
                       FROM research_run_files rf
                       LEFT JOIN project_files pf
                              ON pf.project_guid = r.project_guid
                             AND pf.model_id     = {model_bind}
                             AND pf.path         = rf.path
                             AND pf.status      != 'deleted'
                      WHERE rf.run_id = r.id
                        AND (pf.sha256 IS NULL OR pf.sha256 <> rf.sha256)) AS files_moved
               FROM research_runs r
              WHERE r.project_guid = {guid_bind}
         ),
         edges AS (
             SELECT r.id AS child_id, je.value AS parent_id
               FROM research_runs r, json_each(r.context_run_ids_json) je
              WHERE r.project_guid = {guid_bind}
         ),
         refs AS (
             SELECT child_id AS run_id, COUNT(*) AS n FROM edges GROUP BY child_id
         ),
         refd AS (
             SELECT parent_id AS run_id, COUNT(*) AS n FROM edges GROUP BY parent_id
         ),
         invalid (run_id) AS (
             SELECT run_id FROM moved WHERE files_moved > 0
             UNION
             SELECT e.child_id FROM edges e
              WHERE NOT EXISTS (SELECT 1 FROM research_runs p
                                 WHERE p.id = e.parent_id
                                   AND p.project_guid = {guid_bind})
             UNION
             SELECT e.child_id FROM edges e
               JOIN invalid i ON i.run_id = e.parent_id
         )"
    )
}

/// The flat transitive ancestry for a set of runs, in one recursive query.
///
/// Returns, per asked run id, every run its context chain reaches — deduplicated,
/// ascending by seq, deleted entries last. An ancestor that no longer exists keeps
/// its recorded id and reports `state: "deleted"` with no title or seq: the edge
/// is the child's own record of what it was fed, and it survives the parent.
fn research_dependencies(
    tx: &rusqlite::Transaction<'_>,
    pg: &str,
    model_id: &str,
    run_ids: &[String],
) -> rusqlite::Result<std::collections::HashMap<String, Vec<ResearchRunDependency>>> {
    let mut out: std::collections::HashMap<String, Vec<ResearchRunDependency>> =
        std::collections::HashMap::new();
    if run_ids.is_empty() {
        return Ok(out);
    }
    let placeholders = (0..run_ids.len())
        .map(|i| format!("?{}", i + 3))
        .collect::<Vec<_>>()
        .join(", ");
    // One WITH list: `ancestors` joins the validity CTEs rather than opening a
    // second block, which SQLite would refuse.
    let sql = format!(
        "{ctes},
         ancestors (root_id, anc_id) AS (
             SELECT e.child_id, e.parent_id FROM edges e
              WHERE e.child_id IN ({placeholders})
             UNION
             SELECT a.root_id, e.parent_id
               FROM ancestors a JOIN edges e ON e.child_id = a.anc_id
         )
         SELECT a.root_id, a.anc_id, p.seq, p.title, p.question,
                p.id IS NULL AS deleted,
                EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = a.anc_id) AS anc_invalid
           FROM ancestors a
           LEFT JOIN research_runs p
                  ON p.id = a.anc_id AND p.project_guid = ?1
          ORDER BY a.root_id, (p.seq IS NULL), p.seq",
        ctes = research_validity_ctes("?1", "?2"),
    );
    let mut stmt = tx.prepare(&sql)?;
    let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&pg, &model_id];
    for id in run_ids {
        binds.push(id);
    }
    let rows = stmt.query_map(binds.as_slice(), |row| {
        let root: String = row.get(0)?;
        let anc_id: String = row.get(1)?;
        let seq: Option<i64> = row.get(2)?;
        let stored_title: Option<String> = row.get(3)?;
        let question: Option<String> = row.get(4)?;
        let deleted: bool = row.get(5)?;
        let anc_invalid: bool = row.get(6)?;
        let state = if deleted {
            "deleted"
        } else if anc_invalid {
            "invalid"
        } else {
            "valid"
        };
        Ok((
            root,
            ResearchRunDependency {
                id: anc_id,
                seq,
                title: (!deleted).then(|| {
                    stored_title
                        .unwrap_or_else(|| research_title(question.as_deref().unwrap_or_default()))
                }),
                state,
            },
        ))
    })?;
    for row in rows {
        let (root, dep) = row?;
        out.entry(root).or_default().push(dep);
    }
    Ok(out)
}

/// Fold a run's ancestry into its summary: the `context` list and, when the run is
/// invalid, which of the three causes applies. Own staleness wins the naming —
/// a run that is both stale and resting on a deleted parent is reported `stale`,
/// since that is the defect the caller can act on from this row alone.
fn fill_validity(summary: &mut ResearchRunSummary, deps: Vec<ResearchRunDependency>) {
    if !summary.valid {
        summary.invalid_reason = if summary.files_moved > 0 {
            Some("stale")
        } else if deps.iter().any(|d| d.state == "deleted") {
            Some("context_deleted")
        } else {
            Some("context_invalid")
        };
    }
    summary.context = deps;
}

/// Load the earlier runs a request named, in the order it named them, with each
/// one's staleness measured against the index as it stands now.
///
/// Every id must be a run **of this project**: without that check one project could
/// read another's research by guessing a UUID, and the whole point of
/// `collection_for` isolation would be undone by a field on a request body. An id
/// that resolves to nothing is a 404 rather than a silent omission — a run answered
/// against context the caller believes it supplied, but did not, is unreproducible.
///
/// An id that resolves to an **invalid** run — stale itself, or resting
/// (transitively) on a deleted or stale run — is a 400: injecting it would hand the
/// new run confident, obsolete prose, which is precisely what the validity graph
/// exists to prevent. The client saw `valid` on the list before offering the pick,
/// so this trips only when the index moved between the pick and the submit.
async fn load_prior_reports(
    s: &RouterState,
    project_guid: &UUIDv4,
    ids: &[String],
    token: &CancellationToken,
) -> Result<Vec<crate::research::PriorReport>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    // The 32-char simple form, matching `UUIDv4`'s own `ToSql` and every other table
    // in the schema. `Uuid::to_string()` is hyphenated and would match nothing.
    let pg = project_guid.0.simple().to_string();
    let wanted: Vec<String> = ids.to_vec();

    let rows = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let placeholders = (0..wanted.len())
                .map(|i| format!("?{}", i + 3))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "{ctes}
                 SELECT r.id, r.seq, r.question, r.report, {cols},
                        EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = r.id)
                            AS run_invalid,
                        EXISTS (SELECT 1 FROM edges e
                                 WHERE e.child_id = r.id
                                   AND NOT EXISTS (SELECT 1 FROM research_runs p
                                                    WHERE p.id = e.parent_id
                                                      AND p.project_guid = ?1))
                            AS dangling_parent
                   FROM research_runs r
                  WHERE r.project_guid = ?1 AND r.id IN ({placeholders})",
                ctes = research_validity_ctes("?1", "?2"),
                cols = research_staleness_columns("?2"),
            );
            let mut stmt = tx.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&pg, &model_id];
            for id in &wanted {
                binds.push(id);
            }
            let out = stmt
                .query_map(binds.as_slice(), |row| {
                    Ok((
                        crate::research::PriorReport {
                            id: row.get(0)?,
                            seq: row.get(1)?,
                            question: row.get(2)?,
                            report: row.get(3)?,
                            files_total: row.get::<_, i64>(4)? as usize,
                            files_moved: row.get::<_, i64>(5)? as usize,
                        },
                        row.get::<_, bool>(6)?,
                        row.get::<_, bool>(7)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(out)
        })
        .with_cancellation_token(token)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(
                error = ?e,
                "Failed to read the prior research runs a request asked for. Check the \
                 database is readable."
            );
            ApiError::from(e)
        })?;

    // Re-order to the request's order and surface the first id that resolved to
    // nothing. A HashMap rather than a scan per id: the cap is small, but the shape
    // says the lookup is by id and does not invite someone to raise the cap later.
    // The unknown-id 404 keeps precedence over the invalid-run 400: "no such run"
    // is the sharper answer, and a client that mixed projects up should hear that
    // rather than a validity verdict about a run it never meant.
    let mut by_id: std::collections::HashMap<String, (crate::research::PriorReport, bool, bool)> =
        rows.into_iter().map(|r| (r.0.id.clone(), r)).collect();
    let resolved = ids
        .iter()
        .map(|id| {
            by_id
                .remove(id)
                .ok_or_else(|| ApiError::ResearchRunNotFound { run_id: id.clone() })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let offenders: Vec<(String, &'static str)> = resolved
        .iter()
        .filter(|(_, run_invalid, _)| *run_invalid)
        .map(|(report, _, dangling)| {
            let reason = if report.files_moved > 0 {
                "stale"
            } else if *dangling {
                "context_deleted"
            } else {
                "context_invalid"
            };
            (report.id.clone(), reason)
        })
        .collect();
    if !offenders.is_empty() {
        return Err(ApiError::ResearchContextInvalid { runs: offenders });
    }
    Ok(resolved.into_iter().map(|(report, _, _)| report).collect())
}

/// The production [`ResearchJournal`](crate::research::ResearchJournal): one
/// best-effort row per finished run.
///
/// Holds the request-side context the loop deliberately does not know about
/// (project, effort level, sampling), so `RunRecord` stays a description of what
/// the loop did rather than of how it was asked for.
struct SqliteResearchJournal {
    db_pool: Arc<crate::db::sqlite3::SQLite3Pool>,
    context: crate::db::research::RunContext,
}

#[async_trait::async_trait]
impl crate::research::ResearchJournal for SqliteResearchJournal {
    async fn record(
        &self,
        record: crate::research::RunRecord,
    ) -> Option<crate::research::RecordedRun> {
        // A fresh token: the request's own is cancelled the moment the client
        // disconnects, and a run that completed still deserves its record —
        // "the client stopped reading" is not "this never happened".
        crate::db::research::insert_run(
            &self.db_pool,
            self.context.clone(),
            record,
            CancellationToken::new(),
        )
        .await
    }
}

#[async_trait::async_trait]
impl crate::research::ResearchTools for StateResearchTools {
    async fn search(
        &self,
        req: SearchRequest,
        token: &CancellationToken,
    ) -> Result<Vec<SearchResult>, ApiError> {
        search_core(&self.state, self.project_guid, &req, token).await
    }

    async fn symbols(
        &self,
        req: SymbolsRequest,
        token: &CancellationToken,
    ) -> Result<SymbolsResponse, ApiError> {
        symbols_core(&self.state, self.project_guid, &req, token).await
    }

    async fn outline(
        &self,
        path: String,
        scope: &crate::research::ToolScope,
        token: &CancellationToken,
    ) -> Result<OutlineResponse, ApiError> {
        outline_core(&self.state, self.project_guid, &path, scope, token).await
    }

    async fn list_files(
        &self,
        glob: String,
        scope: &crate::research::ToolScope,
        token: &CancellationToken,
    ) -> Result<ListFilesResponse, ApiError> {
        list_files_core(&self.state, self.project_guid, &glob, scope, token).await
    }

    async fn file_history(
        &self,
        path: String,
        scope: &crate::research::ToolScope,
        token: &CancellationToken,
    ) -> Result<FileHistoryResponse, ApiError> {
        file_history_core(
            &self.state,
            self.project_guid,
            &path,
            scope,
            scaled_width(FILE_HISTORY_LIMIT, self.evidence_width),
            token,
        )
        .await
    }

    async fn grep(
        &self,
        pattern: String,
        glob: Option<String>,
        scope: &crate::research::ToolScope,
        token: &CancellationToken,
    ) -> Result<GrepResponse, ApiError> {
        grep_core(
            &self.state,
            self.project_guid,
            &pattern,
            glob.as_deref(),
            scope,
            scaled_width(GREP_LIMIT, self.evidence_width),
            token,
        )
        .await
    }

    async fn read_chunks(
        &self,
        path: String,
        start_line: usize,
        end_line: usize,
        scope: &crate::research::ToolScope,
        token: &CancellationToken,
    ) -> Result<ReadChunksResponse, ApiError> {
        read_chunks_core(
            &self.state,
            self.project_guid,
            &path,
            start_line,
            end_line,
            scope,
            scaled_width(READ_CHUNKS_LIMIT, self.evidence_width),
            token,
        )
        .await
    }

    async fn list_research(
        &self,
        query: Option<String>,
        token: &CancellationToken,
    ) -> Result<Vec<crate::research::ResearchListing>, ApiError> {
        list_research_core(&self.state, self.project_guid, query, token).await
    }

    async fn read_research(
        &self,
        seq: i64,
        token: &CancellationToken,
    ) -> Result<crate::research::StoredReport, ApiError> {
        read_research_core(&self.state, self.project_guid, seq, token).await
    }

    async fn file_versions(
        &self,
        paths: Vec<String>,
        token: &CancellationToken,
    ) -> Result<Vec<crate::research::FileVersion>, ApiError> {
        file_versions_core(&self.state, self.project_guid, paths, token).await
    }
}

/// A named SSE event that knows its own wire shape. `data()` must serialize to
/// one line (`serde_json::to_string` escapes newlines), because both SSE
/// consumers read the stream per `data:` line.
trait SseWireEvent {
    fn name(&self) -> &'static str;
    fn data(&self) -> serde_json::Value;
    /// Whether this event closes the stream. Both vocabularies end the same
    /// way — `done` or `error` — and `SseEventStream` uses this to notice a job
    /// that stopped without saying so.
    fn is_terminal(&self) -> bool;
    /// The terminal `error` to synthesise when the job's channel closed with no
    /// terminal event of its own. Deliberately an existing event name and an
    /// existing `ApiError` code, so the four-place SSE contract and
    /// `codes_are_stable` are both untouched — a consumer that already handles
    /// `error` handles this with no change.
    fn abnormal_end() -> Self
    where
        Self: Sized;
}

impl SseWireEvent for crate::research::ResearchEvent {
    fn name(&self) -> &'static str {
        self.name()
    }
    fn data(&self) -> serde_json::Value {
        self.data()
    }
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            crate::research::ResearchEvent::Done { .. }
                | crate::research::ResearchEvent::Error { .. }
        )
    }
    fn abnormal_end() -> Self {
        crate::research::ResearchEvent::Error {
            code: ApiError::Internal.code().to_string(),
            detail: "The research job stopped without producing a report. \
                     Nothing was saved; the run can be re-asked."
                .to_string(),
        }
    }
}

impl SseWireEvent for IndexEvent {
    fn name(&self) -> &'static str {
        self.name()
    }
    fn data(&self) -> serde_json::Value {
        self.data()
    }
    fn is_terminal(&self) -> bool {
        matches!(self, IndexEvent::Done { .. } | IndexEvent::Error { .. })
    }
    fn abnormal_end() -> Self {
        IndexEvent::Error {
            code: ApiError::Internal.code().to_string(),
            detail: "The indexing job stopped without reporting a result. \
                     Re-run the request; files left mid-flight are recovered by \
                     the retry worker."
                .to_string(),
        }
    }
}

/// The SSE body of one detached job (research, or a streaming `/index`). Owns
/// the event receiver and the job's cancellation token; **dropping the stream
/// cancels the job** — that is the whole cancellation contract (a client
/// disconnect makes axum drop the body).
///
/// A research job's semaphore permit deliberately does **not** ride here. The job
/// is spawned detached, so a permit held by the stream would be released the
/// instant a client disconnected while the job kept running to its next
/// cancellation point — briefly over-admitting past `max_concurrent`, which
/// matters now that a run may be granted an hour. The permit lives in the spawned
/// future instead, so a slot frees when the work stops rather than when the
/// reader leaves.
///
/// A job that stops *without* sending a terminal event is the one case this
/// stream has to invent something for, and it is not hypothetical: a panic in
/// the detached job aborts the task, drops the sender, and the channel simply
/// closes. To every consumer that is byte-for-byte a completed stream — which
/// is how a research run that panicked in `parse_citations` read as a success
/// that had merely produced no report, while nothing was journalled and no
/// error was logged anywhere the client could see. So the closing of the
/// channel is only a clean end if a terminal event went through first;
/// otherwise one `error` is synthesised (`SseWireEvent::abnormal_end`).
struct SseEventStream<E> {
    rx: tokio::sync::mpsc::UnboundedReceiver<E>,
    token: CancellationToken,
    /// Whether a `done`/`error` has already gone out, so the synthetic terminal
    /// is not appended to a stream that ended properly.
    saw_terminal: bool,
    /// Whether the synthetic terminal has been emitted, so the stream ends on
    /// the poll after it rather than repeating it forever.
    ended: bool,
}

impl<E> SseEventStream<E> {
    fn new(rx: tokio::sync::mpsc::UnboundedReceiver<E>, token: CancellationToken) -> Self {
        Self {
            rx,
            token,
            saw_terminal: false,
            ended: false,
        }
    }
}

impl<E> Drop for SseEventStream<E> {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

impl<E: SseWireEvent> futures_core::Stream for SseEventStream<E> {
    type Item = Result<axum::response::sse::Event, std::convert::Infallible>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.rx.poll_recv(cx).map(|opt| match opt {
            Some(e) => {
                this.saw_terminal |= e.is_terminal();
                Some(Ok(axum::response::sse::Event::default()
                    .event(e.name())
                    .data(e.data().to_string())))
            }
            // The channel closed. Ending here is correct only if the job said
            // how it ended; otherwise it died without a word.
            None if this.saw_terminal || this.ended => None,
            None => {
                this.ended = true;
                let e = E::abnormal_end();
                warn!(
                    event = e.name(),
                    "A streaming job ended without a terminal event; \
                     synthesising one so the client does not read the stream as \
                     successful. This means the job task died — check for a panic \
                     above this line."
                );
                Some(Ok(axum::response::sse::Event::default()
                    .event(e.name())
                    .data(e.data().to_string())))
            }
        })
    }
}

/// Iterative code research driven by a local Ollama model, streamed as SSE.
///
/// A long-lived, one-way stream: the server runs a research loop in which the
/// configured (or request-named) Ollama model asks the index one question per
/// turn — internal lookups against the index cores, **every one of them scoped by
/// the request's `include`/`exclude`** — then must write a final report. The scope is
/// enforced on all nine file-keyed tools, not only on retrieval: a path outside it is
/// refused by name, and name-keyed lookups drop the rows it hides and report how
/// many. So a scoped run cannot read its way out of its scope, and its report can
/// only speak about what it was given. The two stored-report tools
/// (`list_research`/`read_research`) are the deliberate non-file exception: reports
/// are not files, they are never citable (hearsay — nothing they show seeds the
/// citation evidence), and only *valid* runs are offered. Events (`text/event-stream`, named events with JSON
/// `data`):
///
/// - `started` `{run_id, model, effort, granted_seconds, worst_case_ms}` — always the
///   first frame, before any work. `run_id` names this run for its whole life:
///   `GET /research/active` lists it, `DELETE /research/active/{run_id}` cancels it,
///   and it is the id the run will be stored under if it finishes. Previously an id
///   existed only from `done` onwards (it was minted by the journal write), so a
///   running job could not be named at all — and a cancelled one, which is never
///   journalled, never could. `worst_case_ms` is `granted_seconds * 1000 +
///   [research].report_timeout_ms`: the two bound different phases, so this sum, not
///   `granted_seconds`, is how long the caller may wait;
/// - `thinking` `{text}` — deltas of the model's thinking (thinking models only);
/// - `step` `{n, action, <arg>, hits, spans, spans_truncated}` — one executed tool
///   call. `spans` are the `path:start-end` locations the call actually put in front
///   of the model — the same ones citation provenance is scored against — so the
///   trace says what was *read*, not merely what was asked for: `hits: 3` on a
///   4000-line file names no lines at all. Empty for calls that read nothing
///   (`note`, `revise_plan`) or return paths without usable spans (`list_files`).
///   Capped, with `spans_truncated` saying so rather than the cut being silent.
///   `action` is
///   `search`, `grep`, `symbols`, `outline`, `list_files`, `read_chunks`,
///   `note` or `revise_plan`, and the argument key is named for what it is: `query`,
///   `pattern`, `name`, `path`, `name`, `glob`, `path`, `text` and `plan`
///   respectively — exactly one is present per step. `note` and `revise_plan` write
///   to the run's own state rather than reading the index (the model's reasoning is
///   discarded between turns, so a scratchpad is the only way a conclusion survives
///   one), and cost a step like any other call;
/// - `progress` `{steps, max_steps, elapsed_ms, max_ms, tokens, max_tokens,
///   prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx, context_pct, turns,
///   generation_ms, model_load_ms, unaccounted_ms, eval_tokens_per_second,
///   binding, shares}` — budget consumption, so a live run is steerable instead of
///   opaque. Emitted once before the first turn (all limits, nothing spent), then
///   after every executed step and every completed turn. `binding` is the axis with
///   the largest **share spent** (`time`, `tokens`, `steps` or `context`), and
///   `shares` `{time, tokens, steps, context}` are those four shares as percentages.
///   Read them together: `binding` names a maximum, not a problem — a run 12% into
///   its time budget and less into everything else reports `binding: "time"`, which
///   without the shares beside it reads as a run about to expire. What actually
///   stopped a run is `done.reason`. Not emitted on a timer: interpolate
///   `elapsed_ms` locally between events. The four timing fields say where the
///   elapsed time went, which is what separates a slow model from a busy GPU —
///   the same symptom with opposite remedies. `generation_ms` is Ollama's own
///   generation time and `eval_tokens_per_second` is `eval_tokens` over it (so a
///   queued run still reports its true generation rate); `model_load_ms` is time
///   spent loading the model, and anything non-zero there after the first turn
///   means the model was evicted and reloaded mid-run, i.e. something else wanted
///   the device; `unaccounted_ms` is wall clock Ollama did not account for, which
///   is dominated by queueing when it is large. All four are `0` when the Ollama
///   in use does not report durations;
/// - `summary` `{text}` — the final Markdown report. Streamed as deltas when the
///   report was rewritten after its citation check; sent as one event otherwise,
///   because the first draft is withheld until that check has run;
/// - `citations` `{server_written, shown_paths, hearsay_only, total, verified,
///   path_only, unverified, unverified_paths, path_resolved, stale, stale_paths,
///   draft_unverified, draft_path_only, draft_stale, revalidation_steps}` —
///   emitted once, after the report and before `done`: the server's provenance
///   check on the report's `path:start-end` references, scored against the
///   locations this run's own tool calls actually returned. `verified` = the path
///   and an overlapping line range were both shown to the model; `path_only` = the
///   file was shown but not that range; `unverified` = no tool returned that path
///   at all, i.e. the model invented it (those paths are listed, deduplicated and
///   capped). Whether a range *exists* in the real file is deliberately not
///   checked — the index stores no line counts, so it would be unknowable for
///   every file until a full reindex. `stale` is the *freshness* verdict, which is
///   independent of the three above: how many citations point into a file the index
///   rewrote (or dropped) after this run had read it, with those paths listed in
///   `stale_paths`. A run can last half an hour while `mindex-index`/`mindex-watch`
///   keep writing, and nothing serializes them against research — indexing has
///   priority by design — so a perfectly verified citation can still describe code
///   that has been replaced. The counts always describe the report that was
///   streamed. The `draft_*`/`revalidation_steps` fields are `null`
///   unless the *first* draft failed this check and was sent back for correction —
///   with the offending locations named and, for a run that stopped by choice
///   rather than by budget, the tools briefly re-opened so it could read what it
///   had cited blindly or what had moved. Their presence is what distinguishes a
///   report that was right the first time from one that was repaired.
///   `server_written` says the report was not the model's: the report window
///   expired and the server assembled one. Read it before the counts — a
///   server-written report contains no `path:start-end`, so it always scores
///   `total: 0, verified: 0, unverified: 0`, which is byte-for-byte what a clean
///   report scores. Without this flag those two are indistinguishable, and "verified
///   0 even though it read the files" is exactly that confusion. `shown_paths` is
///   how many files this run's tools actually returned — the denominator the counts
///   never had, and what makes admissibility machine-checkable rather than a matter
///   of the reader's discipline: `verified: 0` over `shown_paths: 12` means the
///   report cited none of the dozen files it read, while `verified: 0` over
///   `shown_paths: 0` is the honest "nothing in this scope was shown to me", which
///   is the one case the server's own grounding gate exempts and therefore the one
///   case that reaches a caller looking exactly like a clean run — **unless**
///   `hearsay_only` is true. That flag says no tool returned a single path *and*
///   the run was holding somebody else's report (prior context, or a challenge
///   subject), so the zero is not an empty scope but an earlier answer restated
///   with no evidence of its own: refuse such a report rather than re-asking it
///   with a wider scope. A run that called only `list_files` reports
///   `shown_paths: 0` with `hearsay_only: false`. `path_resolved` counts citations
///   scored against a path they did not spell — a bare filename that named exactly
///   one shown file. `verified` therefore means "a path a tool returned,
///   identified unambiguously from what the report wrote", and this says how many
///   leaned on the second half;
/// - `excerpts` `{excerpts: [{path, start_line, end_line, code}], total, truncated}`
///   — emitted once between `citations` and `done`, and only when the report has at
///   least one **verified** citation: the indexed code at those locations,
///   verbatim. The server already holds these bytes, so this costs one SQL read and
///   no model tokens — which is the point. Asking the model to reproduce a file in
///   its report is the most reliable way to make it fail, so the report cites and
///   the server quotes. Only verified citations are shipped (a `path_only` or
///   `unverified` one names no location worth reading), the run's scope is enforced
///   on every read, and the caps drop whole chunks rather than cutting one —
///   `truncated` says some code did not fit. Best-effort: a read failure costs the
///   excerpt, never the run, so the event may be absent or short;
/// - `done` `{reason, prompt_version, run_id, seq, …every `progress` field}` — completion
///   (closes the stream), carrying the run's final cost as well as
///   `steps`/`elapsed_ms`. `prompt_version` identifies the generation of the
///   server's research instructions that drove the run: reports written under
///   different prompts are not comparable, and nothing else on the stream says
///   which was in force. `run_id`/`seq` name the stored run this became, so a client
///   that just watched it can offer it back as context for a later question
///   (`GET /projects/{project_guid}/research/{run_id}`); both are **null** when the
///   journal write failed, since the journal is best-effort and a fabricated id would
///   name a run nothing can fetch.
///   `reason` says *why* the loop stopped asking: `finalized` (the model judged the
///   evidence sufficient) or one of the cut-short outcomes — `time_exhausted` (the
///   wall-clock budget), `tokens_exhausted` (the local-token budget),
///   `budget_exhausted` (the step backstop),
///   `context_exhausted` (a prompt reached the allowed share of the model's context
///   window), `unparseable` (no usable reply) and `repeated_calls` (it kept
///   repeating calls it had already made). Anything but `finalized` means the report
///   was written on incomplete evidence, so a client may want to re-ask with a
///   bigger `budget` or a narrower question;
/// - `error` `{code, detail}` — a failure after the stream started; the HTTP status
///   is already 200 by then. `ollama.unavailable` (the chat call failed),
///   `research.model_lacks_tools` (the model wrote a tool call as text, so its
///   Ollama template cannot call tools — pick another model) or
///   `research.no_report` (the model produced nothing usable as a report — empty,
///   or one more tool call). On `research.no_report` any `summary` text already
///   streamed is **not** a report and must be discarded, not shown.
///
/// SSE comments are sent as keep-alive every 15 s. **Closing the connection
/// cancels the research job** — that is still the primary cancellation interface.
/// It is not the only one: `DELETE /research/active/{run_id}` cancels the same
/// token, for the case the disconnect rule cannot cover — a caller that has
/// abandoned the run while its socket is still open (an MCP client holds its
/// connection for as long as its own read timeout allows). Jobs run on a small
/// dedicated runtime; when all `[research].max_concurrent` slots are busy — a
/// number `GET /config` publishes, and `GET /research/active` accounts for — the
/// request is rejected up front with **429** `research.busy`.
///
/// **How long this can take.** `max_seconds` (the effort preset, or the request's
/// `budget`) is a *hard* deadline on the investigation, enforced by cancelling the
/// turn in flight rather than only polled between turns. The report phase then gets
/// its own `[research].report_timeout_ms`, so the longest a caller waits is
/// `max_seconds + report_timeout_ms`. A run stopped by its deadline still produces a
/// report — written on the evidence it had, and told to say in its first sentence that
/// it was cut short — and `done.reason` is `time_exhausted`. If even the report window
/// expires with nothing written, the server ships an honest account of the run in its
/// place rather than closing the stream without a `summary`.
///
/// **Concurrency:** read-only against the index; never blocks indexing/GC. The
/// loop's lookups share the SQLite pool and the embedder with regular traffic.
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/research",
    tag = "Search",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = ResearchRequest,
    responses(
        (status = 200, description = "SSE stream of research events \
(started/thinking/step/progress/summary/citations/excerpts/done/error). `started` is always the \
first frame and carries `run_id` — the name this run answers to for its whole life, in \
`GET /research/active` and in `DELETE /research/active/{run_id}` — plus `worst_case_ms`, the \
investigation deadline and the report window summed, which is the longest the caller may wait. \
`citations` reports the server's \
provenance check on the report's `path:start-end` references — \
`verified`/`path_only`/`unverified` counts plus the invented paths — scored against the \
locations this run's own tool calls returned, and its freshness check beside it: \
`stale`/`stale_paths` count the citations pointing into files the index rewrote or dropped \
while the run was reading (indexing is never blocked by research, so a verified citation can \
still describe replaced code). Its \
`draft_unverified`/`draft_path_only`/`draft_stale`/`revalidation_steps` fields are null \
unless the first draft failed those checks and was sent back for correction. Read \
`server_written` before any of the counts: it says the report window expired and the server \
assembled the report, which therefore cites nothing and scores `total: 0, verified: 0, \
unverified: 0` — byte-for-byte what a clean report scores, and indistinguishable from it \
without the flag. `shown_paths` counts the files this run's tools actually returned, so a \
caller can machine-check admissibility (`verified: 0` over `shown_paths: 12` cited none of \
what it read; over `shown_paths: 0` it was shown nothing at all) — but `hearsay_only` overrides \
that reading: nothing shown *and* an earlier report on the table, so the report is that earlier \
prose restated with no evidence of its own and must be refused rather than re-asked wider. \
`path_resolved` counts citations scored against a path they did not spell (a bare filename \
naming exactly one shown file), so `verified` stays readable as \"a path a tool returned, \
identified unambiguously from what the report wrote\". `progress` and `done` additionally carry \
`generation_ms`/`model_load_ms`/`unaccounted_ms`/`eval_tokens_per_second`, which say where the \
elapsed time went: a slow model and a busy GPU produce the same `elapsed_ms` and opposite \
remedies, and a large `unaccounted_ms` beside a healthy rate is queueing. \
`excerpts` follows `citations` when the report has at least one verified \
citation and carries the indexed code at those locations verbatim \
(`{path, start_line, end_line, code}`), so a caller needing a file's literal text gets it \
from the index rather than by asking the model to retype it into the report; scope is \
enforced, caps drop whole chunks, `truncated` says some did not fit, and the whole event is \
best-effort. `step` carries \
`spans`, the `path:start-end` locations that call actually returned, so the trace says what was \
read rather than only what was asked for. `progress` \
reports budget consumption during the run (steps/time/tokens/context plus `binding`, the \
axis with the largest share spent, and `shares`, the four percentages it was chosen from — \
`binding` names a maximum, not a problem, and without the shares beside it a run at 12% of its \
time budget reads as one that is running out); `done` repeats those fields and adds a `reason` — `finalized`, \
or one of \
`time_exhausted`/`tokens_exhausted`/`budget_exhausted`/`context_exhausted`/`unparseable`/`repeated_calls` \
when the report was cut short — plus `prompt_version`, the generation of the server's \
research instructions that produced the report, and `run_id`/`seq` naming the stored run it \
became (null if the best-effort journal write failed) so a client can offer it back as \
context for a later question. `max_seconds` is a hard deadline enforced by cancellation, and \
the report phase has its own `[research].report_timeout_ms` on top, so the longest a caller \
waits is the sum of the two; a run stopped by its deadline still ships a report that says \
so. Every file lookup the model makes is scoped by the request's `include`/`exclude` — an \
out-of-scope path is refused by name rather than answered empty; the stored-report browse \
tools (`list_research`/`read_research`) are the one unscoped exception, offer only valid \
runs, and their content is hearsay that cannot be cited. A run whose `context_run_ids` name \
an invalid run is refused up front with 400 `validation.research_context_invalid`.", content_type = "text/event-stream"),
        (status = 400, description = "Validation failed (empty/oversized question, oversized selector, no model, a model outside `[research].allowed_models` — `research.model_not_allowed`, out-of-range budget, an include/exclude scope matching no indexed file — `research.scope_matches_nothing`, a model Ollama reports cannot call tools — `research.model_lacks_tools`, or `context_run_ids` naming an invalid run — `validation.research_context_invalid`, with each offender and its reason in `meta.runs`).", body = ProblemDetails),
        (status = 429, description = "All research slots are busy.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_research(
    ResearchScope(project_guid, _auth): ResearchScope,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<ResearchRequest>,
) -> Result<Response, ApiError> {
    validate::validate_query(&req.question, s.max_query_bytes)?;
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;
    validate::research_budget(
        &req.budget,
        &validate::ResearchBudgetCaps {
            max_seconds: s.research_max_request_seconds,
            max_tokens: s.research_max_request_tokens,
            max_steps: s.research_max_request_steps,
            max_report_sections: s.research_max_request_report_sections,
            max_report_words: s.research_max_request_report_words,
            max_evidence_width: s.research_max_evidence_width,
        },
    )?;
    let mut context_run_ids = req.context_run_ids.clone().unwrap_or_default();
    validate::research_context(&mut context_run_ids, s.research_max_context_runs)?;
    let model = match req.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ if !s.research_default_model.is_empty() => s.research_default_model.clone(),
        _ => return Err(ApiError::ResearchModelMissing),
    };
    // Policy gate, before any slot or read is paid for: a model outside
    // `[research].allowed_models` is a 400 the caller can act on, not a run.
    if !s.research_allowed_models.allows(&model) {
        return Err(ApiError::ResearchModelNotAllowed { model });
    }

    // Loaded before the permit is taken: a request naming an unknown run is a 400/404
    // that should not first occupy one of `max_concurrent` slots, and the read is a
    // single indexed lookup.
    let load_guard = http3::CancellationGuard(CancellationToken::new());
    let prior_reports =
        load_prior_reports(&s, &project_guid, &context_run_ids, &load_guard.0).await?;

    let scope = crate::research::ToolScope {
        include: req.include,
        exclude: req.exclude,
    };
    let params = crate::research::ResearchParams {
        question: req.question,
        model,
        scope,
        budget: s.research_budget(req.effort, req.budget),
        sampling: s.research_sampling_for(req.seed),
        report_timeout_ms: s.research_report_timeout_ms,
        // Not an effort axis, so not in `Budget::resolve`: the override lands
        // straight from the request, `0` = no checkpoints for this run.
        checkpoint_every_steps: req
            .budget
            .and_then(|b| b.checkpoint_every_steps)
            .unwrap_or(s.research_checkpoint_every_steps),
        max_turn_thinking_chars: s.research_max_turn_thinking_chars,
        max_turn_seconds: s.research_max_turn_seconds,
        metrics: Some(s.metrics.clone()),
        prior_reports,
        max_context_chars: s.research_max_context_chars,
        challenge: None,
    };
    launch_research_job(s, project_guid, req.effort, params, "research", None).await
}

/// `POST /v0/{project_guid}/research/{run_id}/challenge` — set an opponent on a
/// stored report.
///
/// The challenge **is** a research run through the same loop: same admission
/// semaphore (the GPU is the scarce resource — a second pool would over-admit
/// the one thing `max_concurrent` protects), same budgets, same scope
/// enforcement, same citation-provenance gate on its own report. What differs
/// is the framing: the subject's report is injected as the thing under
/// examination (hearsay — the opponent may cite nothing from it and must
/// re-derive every location through its own tools, which *is* the refutation
/// work), the plan turn asks for the report's principal claims, and a closing
/// verdict turn scores each claim CONFIRMED / DISPUTED / REFUTED in a
/// server-dictated vocabulary.
///
/// The stream is the ordinary research stream plus **one** extra event,
/// `verdict` `{challenged_run_id, overall, grounded, claims: [{claim,
/// verdict}]}`, emitted after `excerpts` (when any) and before `done`.
/// `overall` is `confirmed`/`disputed`/`refuted`, or null when the verdict turn
/// produced nothing parseable — "challenged, inconclusive", never an acquittal.
/// `grounded: false` says the challenge's own report verified no citations,
/// which caps `overall` at `disputed`: an unshown accusation can dispute a
/// report but never refute it.
///
/// The subject must be **valid** (its files unmoved, its context chain alive):
/// a challenge scores claims against the code as indexed now, and "the code
/// changed" must not be spendable as "the report was wrong" — 400
/// `research.challenge_subject_invalid`. Challenging a challenge is refused
/// (400 `research.challenge_subject_is_challenge`): trust aggregation is
/// single-level. The verdict lands on the *subject* as a derived trust status in
/// the list/detail endpoints — nothing on the subject's row is written.
///
/// **One challenge per report, newest verdict wins.** Challenging the same
/// report again is how a standing verdict is contested, and it *replaces* rather
/// than accumulates: when this run is journalled with a parseable verdict, every
/// earlier challenge of the same subject is deleted in the same transaction (see
/// `db::research::insert_run`). A run that reaches **no** verdict evicts nothing
/// — an inconclusive re-check must not be able to erase a refutation. So the
/// caller's contract is: a fresh challenge is a bet that costs the current
/// verdict if it wins and leaves it standing if it comes back inconclusive.
/// `mindex_research_challenges_replaced_total` counts the evictions, which is
/// the only trace one leaves.
///
/// **Concurrency:** read-only against the index, like `POST /research`; shares
/// its semaphore, so a busy slot answers 429 `research.busy`.
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/research/{run_id}/challenge",
    tag = "Search",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        ("run_id" = String, Path, description = "The stored run to challenge."),
    ),
    request_body = ChallengeRequest,
    responses(
        (status = 200, description = "SSE stream of research events, exactly as `POST /v0/{project_guid}/research` emits them, plus one `verdict` event on this stream only — `{challenged_run_id, overall, grounded, claims}` after `excerpts` and before `done`. `overall` null = inconclusive (not an acquittal); `grounded: false` caps the verdict at `disputed`. On success with a parseable verdict this run **replaces** any existing challenge of the same subject — there is at most one standing challenge per report. An inconclusive run replaces nothing.", content_type = "text/event-stream"),
        (status = 400, description = "Validation failed: out-of-range budget, disallowed model, an invalid subject (`research.challenge_subject_invalid`), a subject that is itself a challenge (`research.challenge_subject_is_challenge`), a subject whose stored scope now matches no indexed file (`research.scope_matches_nothing`), or a model Ollama reports cannot call tools (`research.model_lacks_tools`).", body = ProblemDetails),
        (status = 404, description = "This project has no such run.", body = ProblemDetails),
        (status = 429, description = "All research slots are busy.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_research_challenge(
    ResearchScope((project_guid, run_id), _auth): ResearchScope<(UUIDv4, String)>,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<ChallengeRequest>,
) -> Result<Response, ApiError> {
    validate::research_budget(
        &req.budget,
        &validate::ResearchBudgetCaps {
            max_seconds: s.research_max_request_seconds,
            max_tokens: s.research_max_request_tokens,
            max_steps: s.research_max_request_steps,
            max_report_sections: s.research_max_request_report_sections,
            max_report_words: s.research_max_request_report_words,
            max_evidence_width: s.research_max_evidence_width,
        },
    )?;
    let model = match req.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ if !s.research_default_model.is_empty() => s.research_default_model.clone(),
        _ => return Err(ApiError::ResearchModelMissing),
    };
    if !s.research_allowed_models.allows(&model) {
        return Err(ApiError::ResearchModelNotAllowed { model });
    }

    // The subject, loaded before any slot is taken (the prior-reports rule).
    let load_guard = http3::CancellationGuard(CancellationToken::new());
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    let pg = project_guid;
    let rid = run_id.clone();
    let subject_row = s
        .db_pool
        .transaction(load_guard.0.child_token(), move |tx| {
            let sql = format!(
                "{ctes}
                 SELECT r.seq, r.question, r.report, r.kind, r.scoped, r.scope_spec_json,
                        EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = r.id) AS invalid_flag
                   FROM research_runs r
                  WHERE r.project_guid = ?1 AND r.id = ?3",
                ctes = research_validity_ctes("?1", "?2"),
            );
            tx.query_row(&sql, rusqlite::params![pg, model_id, rid], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            })
            .optional()
            .map_err(SQLite3PoolError::from)
        })
        .await
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to load a research run for challenging. Check the database is readable."
            );
            ApiError::from(e)
        })?;
    let Some((seq, question, report, kind, scoped, scope_spec_json, invalid)) = subject_row else {
        return Err(ApiError::ResearchRunNotFound { run_id });
    };
    if kind == "challenge" {
        return Err(ApiError::ChallengeSubjectIsChallenge { run_id });
    }
    if invalid {
        return Err(ApiError::ChallengeSubjectInvalid {
            run_id,
            reason: "stale_or_broken_context",
        });
    }
    // The challenge re-inhabits the subject's exact scope. A scoped run
    // journalled before the structured scope existed cannot be faithfully
    // re-scoped — refuse honestly rather than challenge with different walls.
    let scope: crate::research::ToolScope = match &scope_spec_json {
        Some(json) => serde_json::from_str(json).map_err(|e| {
            warn!(error = %e, "A stored scope_spec_json failed to parse; refusing the challenge.");
            ApiError::ChallengeSubjectInvalid {
                run_id: run_id.clone(),
                reason: "scope_unavailable",
            }
        })?,
        None if scoped != 0 => {
            return Err(ApiError::ChallengeSubjectInvalid {
                run_id,
                reason: "scope_unavailable",
            });
        }
        None => crate::research::ToolScope::default(),
    };

    let params = crate::research::ResearchParams {
        // Self-describing everywhere the question surfaces: the list, the title
        // fallback, `GET /research/active`.
        question: format!("Challenge research #{seq}: {question}"),
        model,
        scope,
        budget: s.research_budget(req.effort, req.budget),
        sampling: s.research_sampling_for(req.seed),
        report_timeout_ms: s.research_report_timeout_ms,
        checkpoint_every_steps: req
            .budget
            .and_then(|b| b.checkpoint_every_steps)
            .unwrap_or(s.research_checkpoint_every_steps),
        max_turn_thinking_chars: s.research_max_turn_thinking_chars,
        max_turn_seconds: s.research_max_turn_seconds,
        metrics: Some(s.metrics.clone()),
        // The subject is injected by the challenge machinery itself, not as a
        // prior report: it is the question, not background.
        prior_reports: Vec::new(),
        max_context_chars: s.research_max_context_chars,
        challenge: Some(crate::research::ChallengeSubject {
            run_id: run_id.clone(),
            seq,
            question,
            report,
        }),
    };
    launch_research_job(
        s,
        project_guid,
        req.effort,
        params,
        "challenge",
        Some(run_id),
    )
    .await
}

/// The shared tail of `POST /research` and `POST /research/{run_id}/challenge`:
/// admission, run identity, journal context, registry entry, job spawn and the
/// SSE response. One function so the two entrances cannot drift on any of the
/// invariants that live here (permit-in-the-job, registry-in-the-job, the
/// `started`-first frame).
async fn launch_research_job(
    s: RouterState,
    project_guid: UUIDv4,
    effort_level: crate::research::Effort,
    params: crate::research::ResearchParams,
    kind: &'static str,
    challenged_run_id: Option<String>,
) -> Result<Response, ApiError> {
    // Before the permit, and before the scope count: a run is a tool-calling loop, so
    // a model that cannot call tools spends a slot, a model load and a turn only to
    // fail on the symptom check inside the loop. `supports_tools` is three-valued and
    // only `Some(false)` refuses — an unreachable Ollama answers `None`, and a
    // pre-flight that cannot be performed must never become a refusal. The cost is a
    // per-process cached `/api/show`, the same one `num_ctx` already pays for.
    if s.research_ollama.supports_tools(&params.model).await == Some(false) {
        return Err(ApiError::ResearchModelLacksTools {
            model: params.model.clone(),
        });
    }

    // Before the permit, like the model-policy gate above it: a scope that admits no
    // file cannot produce a run worth a slot. Every model-facing tool is bounded by
    // the same subquery this counts, so such a run refuses every lookup and then
    // reports the question unanswerable — a finding-shaped non-answer that costs a
    // full budget and raises no error. One indexed COUNT is the whole cost, and only
    // for a scoped run: an unscoped one is unchanged by construction.
    if params.scope.is_scoped() {
        let (scope_sql, binds) = scope_subquery(project_guid, &params.scope, 1);
        let sql = format!("SELECT COUNT(*) FROM ({scope_sql})");
        let guard = http3::CancellationGuard(CancellationToken::new());
        let in_scope: i64 = s
            .db_pool
            .transaction(guard.0.clone(), move |tx| {
                let mut st = tx.prepare(&sql)?;
                let n: i64 =
                    st.query_row(rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?;
                Ok(n)
            })
            .await
            .map_err(|e| {
                error!(
                    error = ?e,
                    "Failed to count the files a research scope admits; refusing the run. \
                     Check that the SQLite database is readable."
                );
                ApiError::from(e)
            })?;
        if in_scope == 0 {
            return Err(ApiError::ResearchScopeEmpty {
                scope: params.scope.describe(),
            });
        }
    }

    let permit = s
        .research_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::ResearchBusy)?;

    let token = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    // Minted here rather than at the journal insert, which is where it used to be
    // born. A run that has no id until it ends cannot be listed, cancelled or named
    // in a bug report while it runs — and a cancelled run, which is never
    // journalled, had no id at all. Same uuid on the wire, in the registry and in
    // the row.
    let run_id = uuid::Uuid::new_v4().to_string();

    info!(
        project_guid = %project_guid.0,
        model = %params.model,
        prompt_version = crate::research::PROMPT_VERSION,
        effort = ?effort_level,
        kind,
        seed = ?params.sampling.seed,
        // The resolved budget, not the requested effort: with `budget` overrides the
        // level alone no longer says what the run was granted. The shape axes ride
        // here too — they are not journalled (shape knobs never were), so this line
        // is the only record of what a run was actually granted.
        max_seconds = params.budget.max_seconds,
        max_tokens = params.budget.max_tokens,
        max_steps = params.budget.max_steps,
        max_report_sections = params.budget.max_report_sections,
        max_report_words = params.budget.max_report_words,
        evidence_width = params.budget.evidence_width,
        checkpoint_every_steps = params.checkpoint_every_steps,
        "Starting a research job."
    );

    let ollama = s.research_ollama.clone();
    // The model's identity at admission, from the catalog worker's snapshot — no
    // network call on the request path. `(None, None)` when the catalog has not
    // seen the model (e.g. before its first tick): "not recorded", journalled as
    // NULL, never a fabricated digest.
    let (model_digest, model_details_json) =
        s.research_models.read().await.identity_of(&params.model);
    let effort = match effort_level {
        crate::research::Effort::Low => "low",
        crate::research::Effort::Medium => "medium",
        crate::research::Effort::High => "high",
    };
    // Both seams are wrapped here rather than instrumented inside the loop: the
    // journal is called exactly once per finished run with everything worth
    // measuring already in `RunRecord`, and the tools decorator covers all eight
    // calls without editing `execute`'s match.
    let tools: Arc<dyn crate::research::ResearchTools> =
        Arc::new(crate::research::MeteredResearchTools::new(
            Arc::new(StateResearchTools {
                state: s.clone(),
                project_guid,
                evidence_width: params.budget.evidence_width,
            }),
            s.metrics.clone(),
        ));
    let journal: Arc<dyn crate::research::ResearchJournal> =
        Arc::new(crate::db::research::MeteredJournal::new(
            Arc::new(SqliteResearchJournal {
                db_pool: s.db_pool.clone(),
                context: crate::db::research::RunContext {
                    id: run_id.clone(),
                    // Simple form, like every other table. `Uuid::to_string()` is
                    // hyphenated, which nothing else in the schema uses — and a
                    // per-project metric label in that spelling would never line up
                    // with `project_files`'.
                    project_guid: project_guid.0.simple().to_string(),
                    effort,
                    seed: params.sampling.seed,
                    temperature: params.sampling.temperature,
                    top_p: params.sampling.top_p,
                    model_digest,
                    model_details_json,
                    embedder_model_id: {
                        let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
                        model_id.clone()
                    },
                    server_version: env!("CARGO_PKG_VERSION"),
                    started_at: crate::unix_now(),
                    checkpoint_every_steps: params.checkpoint_every_steps,
                    // Rendered by the same one renderer the model reads, so the
                    // journal and the prompt can never describe the scope differently.
                    scope_json: params.scope.is_scoped().then(|| params.scope.describe()),
                    // The same scope as data, for a later challenge to re-inhabit.
                    scope_spec_json: params
                        .scope
                        .is_scoped()
                        .then(|| serde_json::to_string(&params.scope).ok())
                        .flatten(),
                    kind,
                    challenged_run_id: challenged_run_id.clone(),
                    retention_days: s.research_retention_days,
                },
            }),
            s.metrics.clone(),
            effort,
        ));
    let job_token = token.clone();

    // The longest this run may legitimately take. The two windows are separate
    // phases, not one budget — the report gets `report_timeout_ms` *after* the
    // investigation deadline — so neither number alone answers "how long might I
    // wait", and the sum is what `/research/active`, `/health` and the watchdog all
    // reason about.
    let worst_case_ms = params
        .budget
        .max_seconds
        .saturating_mul(1000)
        .saturating_add(params.report_timeout_ms);

    // Announced before the first turn, so a client holds the run's name for the
    // whole stream rather than only at `done`.
    let _ = tx.send(crate::research::ResearchEvent::Started {
        run_id: run_id.clone(),
        model: params.model.clone(),
        effort,
        granted_seconds: params.budget.max_seconds,
        worst_case_ms,
    });

    // Cloned before the spawn, like every other handle the job takes ownership of.
    // The job needs its own because the endings that produce no journal row are
    // exactly the ones `MeteredJournal` cannot see.
    let metrics_for_run = Arc::clone(&s.metrics);
    let registration = s.research_registry.register(
        run_id.clone(),
        project_guid.0.simple().to_string(),
        &params.question,
        params.model.clone(),
        effort,
        params.budget.max_seconds,
        worst_case_ms,
        job_token.clone(),
    );

    s.research_handle.spawn(async move {
        // The permit is held by the *work*, not by the reader: it is released when
        // this future unwinds, so an abandoned job cannot let a replacement in while
        // it is still spending GPU and DB time. See `SseEventStream`.
        //
        // The registry entry rides in the same future for the same reason, and it
        // has to be this future specifically: held anywhere else the two would
        // drift, and the list would either describe a slot that is free or hide one
        // that is not.
        let _permit = permit;
        let _registration = registration;
        crate::research::run_research(
            ollama,
            tools,
            journal,
            params,
            tx,
            job_token,
            Some(metrics_for_run),
        )
        .await;
    });

    let stream = SseEventStream::new(rx, token);
    Ok(axum::response::sse::Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
        )
        .into_response())
}

// ─── Management endpoints ───────────────────────────────────────────────────

/// List every known project with a compact summary.
///
/// Returns file count, files currently indexing, and active-chunk count per project.
/// Empty list when nothing has been indexed yet.
///
/// **Concurrency:** safe — read-only, takes no locks.
#[utoipa::path(
    get,
    path = "/projects",
    tag = "Projects",
    responses(
        (status = 200, description = "All projects with summary counts.", body = ProjectListResponse),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_projects(
    ListProjectsScope(auth): ListProjectsScope,
    State(s): State<RouterState>,
) -> Result<Json<ProjectListResponse>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());

    let projects = s
        .db_pool
        .transaction(guard.0.child_token(), |tx| {
            let mut stmt = tx.prepare(
                "SELECT p.guid,
                        (SELECT COUNT(*) FROM project_files f
                          WHERE f.project_guid = p.guid) AS files,
                        (SELECT COUNT(*) FROM project_files f
                          WHERE f.project_guid = p.guid AND f.status = 'indexing') AS indexing,
                        (SELECT COUNT(*) FROM project_file_chunks c
                          WHERE c.project_guid = p.guid AND c.status = 'active') AS active_chunks
                 FROM projects p
                 GROUP BY p.guid
                 ORDER BY p.guid",
            )?;
            stmt.query_map([], |r| {
                Ok(ProjectSummary {
                    project_guid: r.get::<_, String>(0)?,
                    files: r.get::<_, i64>(1)?,
                    indexing: r.get::<_, i64>(2)?,
                    active_chunks: r.get::<_, i64>(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = %e, "Failed to list projects from SQLite.");
            ApiError::from(e)
        })?;

    // Filtered here rather than in the SQL, deliberately: with authorization off
    // — which is every deployment that has not opted in — `visible_projects` is
    // `None` and this is a move, so the query above stays the byte-identical
    // statement it has always been rather than growing a parameter that is
    // usually a no-op.
    //
    // This listing is why the whole mechanism had to live in the server. The
    // project GUIDs are in a response *body*, and a gateway cannot filter a body
    // without parsing it — while a GUID is a bearer identifier, so leaking one
    // hands over that project's entire data plane.
    let projects = match auth.visible_projects() {
        None => projects,
        Some(visible) => projects
            .into_iter()
            .filter(|p| {
                visible
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(&p.project_guid))
            })
            .collect(),
    };

    Ok(Json(ProjectListResponse { projects }))
}

/// Aggregate statistics for one project.
///
/// `project_files` counted by status, plus a per-language inventory: files tracked,
/// files `indexed`, and chunks split into active vs soft-deleted (pending GC). The
/// language keys are every language the project *contains*, not only those with
/// chunks — see [`LanguageStats`] for why the difference matters.
///
/// **Concurrency:** safe — read-only, takes no locks.
#[utoipa::path(
    get,
    path = "/projects/{project_guid}",
    tag = "Projects",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    responses(
        (status = 200, description = "Per-status file counts and the per-language file/chunk inventory.", body = ProjectStats),
        (status = 404, description = "The project has never been seen.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_project_stats(
    SearchScope(project_guid, _auth): SearchScope,
    State(s): State<RouterState>,
) -> Result<Json<ProjectStats>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;

    let result = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM projects WHERE guid = ?1",
                    params![pg],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Ok(None);
            }

            let mut files = FileStatusCounts::default();
            {
                let mut stmt = tx.prepare(
                    "SELECT status, COUNT(*) FROM project_files
                     WHERE project_guid = ?1 GROUP BY status",
                )?;
                let rows = stmt.query_map(params![pg], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (status, n) = row?;
                    files.set(&status, n as u64);
                }
            }

            let mut languages: HashMap<String, LanguageStats> = HashMap::new();

            // Files first: this pass's key set is the whole inventory. (It is also a
            // superset of the chunk pass's, since the chunks FK is RESTRICT — but
            // neither pass assumes the other ran, so both use `or_default`.)
            {
                let mut stmt = tx.prepare(
                    "SELECT programming_language,
                            COUNT(*),
                            SUM(CASE WHEN status = 'indexed' THEN 1 ELSE 0 END)
                     FROM project_files
                     WHERE project_guid = ?1
                     GROUP BY programming_language",
                )?;
                let rows = stmt.query_map(params![pg], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?;
                for row in rows {
                    let (lang, files, indexed) = row?;
                    let entry = languages.entry(lang).or_default();
                    entry.files = files as u64;
                    entry.indexed_files = indexed as u64;
                }
            }

            {
                let mut stmt = tx.prepare(
                    "SELECT f.programming_language, c.status, COUNT(*)
                     FROM project_file_chunks c
                     JOIN project_files f
                         ON c.project_guid = f.project_guid
                         AND c.model_id = f.model_id
                         AND c.file_path = f.path
                     WHERE c.project_guid = ?1
                     GROUP BY f.programming_language, c.status",
                )?;
                let rows = stmt.query_map(params![pg], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?;
                for row in rows {
                    let (lang, status, n) = row?;
                    let entry = languages.entry(lang).or_default();
                    match status.as_str() {
                        "active" => entry.chunks_active = n as u64,
                        "deleted" => entry.chunks_deleted = n as u64,
                        _ => {}
                    }
                }
            }

            Ok(Some((files, languages)))
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to query project stats from SQLite.");
            ApiError::from(e)
        })?;

    match result {
        Some((files, languages)) => Ok(Json(ProjectStats {
            project_guid,
            files,
            languages,
        })),
        None => Err(ApiError::ProjectNotFound),
    }
}

/// Hard-delete an entire project (immediate, not soft).
///
/// Removes all chunks, files, the project row, its status log, and finally drops the
/// Qdrant collection (last, so a retry re-attempts a failed drop even once the rows
/// are gone). Idempotent: deleting a non-existent project (or re-deleting) is a 204.
///
/// **Concurrency:** safe but destructive — unlike `DELETE /files` this is *not* a soft
/// delete and does not wait for GC. Avoid issuing it against a project with live
/// `/index` requests in flight.
#[utoipa::path(
    delete,
    path = "/projects/{project_guid}",
    tag = "Projects",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    responses(
        (status = 204, description = "Project deleted (or did not exist) — idempotent."),
        (status = 500, description = "SQLite delete or Qdrant collection-drop failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn delete_project(
    DeleteScope(project_guid, _auth): DeleteScope,
    State(s): State<RouterState>,
) -> Result<StatusCode, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let collection = collection_for(project_guid);
    let pg = project_guid;

    s.db_pool
        .transaction(guard.0.child_token(), move |tx| {
            tx.execute("DELETE FROM project_file_chunks WHERE project_guid = ?1", params![pg])?;
            tx.execute("DELETE FROM project_file_symbols WHERE project_guid = ?1", params![pg])?;
            tx.execute("DELETE FROM project_files WHERE project_guid = ?1", params![pg])?;
            tx.execute("DELETE FROM projects WHERE guid = ?1", params![pg])?;
            tx.execute("DELETE FROM project_file_status_log WHERE project_guid = ?1", params![pg])?;
            Ok(())
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to hard-delete project rows from SQLite.");
            ApiError::from(e)
        })?;

    s.qdrant.delete_collection(&collection).await.map_err(|e| {
        error!(
            error = %e,
            collection = %collection,
            "Failed to delete the Qdrant collection. Check Qdrant is reachable at --qdrant-server."
        );
        ApiError::Internal
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Builds the `WHERE` body (without the keyword) and ordered binds selecting
/// `project_files` for `DELETE /files`. Mirrors the search filter: language
/// `IN`/`NOT IN` and path `GLOB` (ORed within a parenthesised group so OR cannot
/// leak across the AND-joined clauses), pinned to the project and excluding files
/// already `deleted`.
fn build_file_filter(
    project_guid: UUIDv4,
    include: &Option<SearchFilter>,
    exclude: &Option<SearchFilter>,
) -> (String, Vec<Bind>) {
    build_file_filter_from(project_guid, include, exclude, 1)
}

/// The paths a [`ToolScope`] admits, as a subquery for `file_path IN (…)`.
///
/// A subquery rather than a join, because `build_file_filter` emits **unqualified**
/// column names (`project_guid`, `status`, `path`, `programming_language`) — every
/// one of which is ambiguous against `project_file_chunks` or
/// `project_file_symbols`. Teaching it to qualify them would touch `DELETE /files`
/// and `POST /cancel`, the two destructive endpoints, to make a research lookup
/// tidier. This way the filter stays exactly the expression those endpoints already
/// rely on, and the same one idea scopes symbols and grep.
///
/// `first_bind` is where this fragment's placeholders start, so the caller can put
/// its own binds before it.
fn scope_subquery(
    project_guid: UUIDv4,
    scope: &crate::research::ToolScope,
    first_bind: usize,
) -> (String, Vec<Bind>) {
    let (where_body, binds) =
        build_file_filter_from(project_guid, &scope.include, &scope.exclude, first_bind);
    (
        format!("SELECT path FROM project_files WHERE {where_body}"),
        binds,
    )
}

/// As [`build_file_filter`], with the first placeholder number given. Split out so a
/// scope filter can be appended to a query that already has binds of its own.
fn build_file_filter_from(
    project_guid: UUIDv4,
    include: &Option<SearchFilter>,
    exclude: &Option<SearchFilter>,
    first_bind: usize,
) -> (String, Vec<Bind>) {
    let mut n = first_bind;
    let mut parts = vec![
        format!("project_guid = ?{n}"),
        "status != 'deleted'".to_string(),
    ];
    n += 1;
    let mut binds: Vec<Bind> = vec![Bind::Guid(project_guid)];

    if let Some(inc) = include {
        if let Some(pls) = inc.programming_languages.as_ref().filter(|v| !v.is_empty()) {
            let ph: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{n}");
                    n += 1;
                    p
                })
                .collect();
            parts.push(format!("programming_language IN ({})", ph.join(", ")));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }
        if let Some(paths) = inc.paths.as_ref().filter(|v| !v.is_empty()) {
            let cl: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("path GLOB ?{n}");
                    n += 1;
                    c
                })
                .collect();
            parts.push(format!("({})", cl.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }
    if let Some(exc) = exclude {
        if let Some(pls) = exc.programming_languages.as_ref().filter(|v| !v.is_empty()) {
            let ph: Vec<String> = pls
                .iter()
                .map(|_| {
                    let p = format!("?{n}");
                    n += 1;
                    p
                })
                .collect();
            parts.push(format!("programming_language NOT IN ({})", ph.join(", ")));
            binds.extend(pls.iter().map(|l| Bind::Lang(*l)));
        }
        if let Some(paths) = exc.paths.as_ref().filter(|v| !v.is_empty()) {
            let cl: Vec<String> = paths
                .iter()
                .map(|_| {
                    let c = format!("path GLOB ?{n}");
                    n += 1;
                    c
                })
                .collect();
            parts.push(format!("NOT ({})", cl.join(" OR ")));
            binds.extend(paths.iter().map(|p| Bind::Path(p.0.as_str().to_string())));
        }
    }

    (parts.join(" AND "), binds)
}

/// `DELETE /projects/{guid}/files` — soft-deletes files matching the selector:
/// marks their active chunks `deleted` and the files `deleted`; the next GC pass
/// (`POST /gc`, or the hourly worker) physically removes the vectors, the chunk
/// rows, and finally the empty file rows. Returns 204 when nothing matched, else
/// 200 with the count of files moved to `deleted`. A non-empty include/exclude is
/// required so an empty body cannot wipe the whole project.
///
/// **Concurrency:** safe — a soft delete (status flip), so it never races a live
/// `/index`/search the way an inline Qdrant delete would; physical removal is deferred
/// to GC. The empty-selector guard (**400**) prevents an accidental whole-project wipe.
#[utoipa::path(
    delete,
    path = "/projects/{project_guid}/files",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = DeleteFilesRequest,
    responses(
        (status = 200, description = "Files matched and soft-deleted.", body = DeleteFilesResponse),
        (status = 204, description = "The selector matched no files — nothing changed."),
        (status = 400, description = "Empty or oversized selector.", body = ProblemDetails),
        (status = 500, description = "SQLite failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn delete_files(
    DeleteScope(project_guid, _auth): DeleteScope,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<DeleteFilesRequest>,
) -> Result<Response, ApiError> {
    validate::require_nonempty_selector(&req.include, &req.exclude)?;
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;

    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;
    let (where_sql, binds) = build_file_filter(pg, &req.include, &req.exclude);

    // 1) Resolve matching file paths (path globs evaluated by SQLite GLOB, as in search).
    let select_sql = format!("SELECT path FROM project_files WHERE {where_sql}");
    let paths: Vec<String> = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let mut stmt = tx.prepare(&select_sql)?;
            let rows = stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to select files for deletion.");
            ApiError::from(e)
        })?;

    if paths.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 2) Soft-delete chunks + files, batched to stay under SQLite's bind-variable limit.
    let mut deleted_files: u64 = 0;
    for batch in paths.chunks(s.path_batch_size) {
        let batch: Vec<String> = batch.to_vec();
        let n = s
            .db_pool
            .transaction(guard.0.child_token(), move |tx| {
                let placeholders = (2..2 + batch.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut bs: Vec<Bind> = Vec::with_capacity(batch.len() + 1);
                bs.push(Bind::Guid(pg));
                bs.extend(batch.into_iter().map(Bind::Path));

                tx.execute(
                    &format!(
                        "UPDATE project_file_chunks SET status = 'deleted'
                         WHERE project_guid = ?1 AND status = 'active' AND file_path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                tx.execute(
                    &format!(
                        "DELETE FROM project_file_symbols
                         WHERE project_guid = ?1 AND file_path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                let files = tx.execute(
                    &format!(
                        "UPDATE project_files SET status = 'deleted', status_updated_at = unixepoch()
                         WHERE project_guid = ?1 AND status != 'deleted' AND path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                Ok(files as u64)
            })
            .with_cancellation_token(&guard.0)
            .await
            .from_cancelled()
            .map_err(|e| {
                // The count is the point: batches before this one are committed, so
                // the 500 the caller receives does not mean "nothing happened" — it
                // used to be indistinguishable from a no-op failure. Retrying the same
                // request is safe and completes the job (the `status != 'deleted'`
                // guard makes it idempotent), which is why this is a log rather than a
                // new response shape: the client's remedy is the same either way.
                error!(
                    error = ?e,
                    project_guid = %pg.0,
                    files_deleted_before_failure = deleted_files,
                    files_matched = paths.len(),
                    "Failed to soft-delete files part-way through; earlier batches are \
                     already committed. Re-running the same request completes it."
                );
                ApiError::from(e)
            })?;
        deleted_files += n;
    }

    if deleted_files == 0 {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::OK, Json(DeleteFilesResponse { deleted_files })).into_response())
    }
}

/// `POST /projects/{guid}/cancel` — best-effort cancel of in-flight indexing for the
/// files matching the selector. Only files in `status = 'indexing'` are touched: each
/// matched file's active chunks are marked `deleted` (the next GC pass removes any
/// vectors a racing embed already upserted) and the file moves `indexing → cancelled`
/// (a legal state-machine transition). Files already `indexed`/`failed`/etc. never
/// match, so their status is preserved — a cancellation that arrives after indexing
/// finished is a no-op. The live `/index` request reconciles against this at its
/// prepare→embed boundary, and the retry worker re-checks status after claiming, so a
/// cancelled file is neither re-embedded nor resurrected. Returns 204 when nothing
/// matched, else 200 with the count of files moved to `cancelled`. A non-empty
/// include/exclude is required so an empty body cannot blanket-cancel the project.
///
/// **Concurrency:** safe and intentionally lock-free — it deliberately does *not* take
/// the per-file indexing claim, so it can interrupt a held one. Correctness against a
/// live `/index` rests on re-reads (the indexer drops cancelled files before embedding;
/// the retry worker re-checks status after claiming), not a lock.
#[utoipa::path(
    post,
    path = "/projects/{project_guid}/cancel",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = CancelRequest,
    responses(
        (status = 200, description = "In-flight indexing cancelled for the matched files.", body = CancelResponse),
        (status = 204, description = "No `indexing` files matched (e.g. already finished) — nothing changed."),
        (status = 400, description = "Empty or oversized selector.", body = ProblemDetails),
        (status = 500, description = "SQLite failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_cancel(
    IndexScope(project_guid, _auth): IndexScope,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<CancelRequest>,
) -> Result<Response, ApiError> {
    validate::require_nonempty_selector(&req.include, &req.exclude)?;
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;

    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;
    let (where_sql, binds) = build_file_filter(pg, &req.include, &req.exclude);

    // 1) Resolve matching file paths, restricted to those being indexed *right now*.
    //    `build_file_filter` already constrains `status != 'deleted'`; appending a
    //    constant predicate keeps the existing bind numbering intact (no new bind).
    let select_sql =
        format!("SELECT path FROM project_files WHERE {where_sql} AND status = 'indexing'");
    let paths: Vec<String> = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let mut stmt = tx.prepare(&select_sql)?;
            let rows = stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to select files to cancel.");
            ApiError::from(e)
        })?;

    if paths.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // 2) Soft-delete the active chunks + move the files 'indexing'→'cancelled', batched
    //    to stay under SQLite's bind-variable limit. Re-asserting status='indexing' in
    //    the file UPDATE makes it a no-op for any row that raced to 'indexed' between
    //    the SELECT and here (the trigger would reject cancelled→… otherwise).
    let mut cancelled_files: u64 = 0;
    for batch in paths.chunks(s.path_batch_size) {
        let batch: Vec<String> = batch.to_vec();
        let n = s
            .db_pool
            .transaction(guard.0.child_token(), move |tx| {
                let placeholders = (2..2 + batch.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut bs: Vec<Bind> = Vec::with_capacity(batch.len() + 1);
                bs.push(Bind::Guid(pg));
                bs.extend(batch.into_iter().map(Bind::Path));

                tx.execute(
                    &format!(
                        "UPDATE project_file_chunks SET status = 'deleted'
                         WHERE project_guid = ?1 AND status = 'active' AND file_path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                tx.execute(
                    &format!(
                        "DELETE FROM project_file_symbols
                         WHERE project_guid = ?1 AND file_path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                let files = tx.execute(
                    &format!(
                        "UPDATE project_files SET status = 'cancelled', status_updated_at = unixepoch()
                         WHERE project_guid = ?1 AND status = 'indexing' AND path IN ({placeholders})"
                    ),
                    params_from_iter(bs.iter()),
                )?;
                Ok(files as u64)
            })
            .with_cancellation_token(&guard.0)
            .await
            .from_cancelled()
            .map_err(|e| {
                // As in `delete_files`: earlier batches are committed, so the 500 does
                // not mean nothing was cancelled. Re-running is safe — the
                // `status = 'indexing'` guard makes an already-cancelled file a no-op.
                error!(
                    error = ?e,
                    project_guid = %pg.0,
                    files_cancelled_before_failure = cancelled_files,
                    files_matched = paths.len(),
                    "Failed to cancel indexing part-way through; earlier batches are \
                     already committed. Re-running the same request completes it."
                );
                ApiError::from(e)
            })?;
        cancelled_files += n;
    }

    info!(project_guid = %pg.0, cancelled_files, "Cancelled in-flight indexing for matched files.");

    if cancelled_files == 0 {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::OK, Json(CancelResponse { cancelled_files })).into_response())
    }
}

/// `GET /projects/{guid}/files` — lists the project's files with per-file status,
/// language, content hash, active-chunk count, retry count, and last status change.
/// Optional `?status=` and `?language=` query filters narrow the set (e.g.
/// `?status=failed` is the dead-letter view). 404 if the project has never been seen
/// (mirrors `get_project_stats`); an empty file set on a known project is `200` with
/// `files: []`. Pure read — cancellation-safe, takes no locks.
///
/// **Concurrency:** safe — read-only. `?status=failed` is the dead-letter view.
#[utoipa::path(
    get,
    path = "/projects/{project_guid}/files",
    tag = "Projects",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        FileListQuery,
    ),
    responses(
        (status = 200, description = "Per-file listing (status / language / hash / chunk & retry counts).", body = FileListResponse),
        (status = 400, description = "Malformed query parameter (e.g. unknown language).", body = ProblemDetails),
        (status = 404, description = "The project has never been seen.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_files(
    SearchScope(project_guid, _auth): SearchScope,
    State(s): State<RouterState>,
    ApiQuery(q): ApiQuery<FileListQuery>,
) -> Result<Json<FileListResponse>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;

    let result = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM projects WHERE guid = ?1",
                    params![pg],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Ok(None);
            }

            // Optional status/language filters, numbered after the pinned project guid.
            let mut where_parts = vec!["f.project_guid = ?1".to_string()];
            let mut binds: Vec<Bind> = vec![Bind::Guid(pg)];
            let mut n = 2usize;
            if let Some(status) = q.status.as_ref() {
                where_parts.push(format!("f.status = ?{n}"));
                binds.push(Bind::Path(status.clone()));
                n += 1;
            }
            if let Some(lang) = q.language {
                where_parts.push(format!("f.programming_language = ?{n}"));
                binds.push(Bind::Lang(lang));
            }

            let sql = format!(
                "SELECT f.path, f.programming_language, f.status, f.sha256,
                        f.retry_count, f.status_updated_at,
                        (SELECT COUNT(*) FROM project_file_chunks c
                          WHERE c.project_guid = f.project_guid
                            AND c.model_id = f.model_id
                            AND c.file_path = f.path
                            AND c.status = 'active') AS chunk_count
                 FROM project_files f
                 WHERE {}
                 ORDER BY f.path",
                where_parts.join(" AND ")
            );
            let files = tx
                .prepare(&sql)?
                .query_map(params_from_iter(binds.iter()), |r| {
                    Ok(FileInfo {
                        path: r.get::<_, String>(0)?,
                        programming_language: r.get::<_, ProgrammingLanguage>(1)?,
                        status: r.get::<_, String>(2)?,
                        sha256: r.get::<_, String>(3)?,
                        retry_count: r.get::<_, i64>(4)?,
                        status_updated_at: r.get::<_, i64>(5)?,
                        chunk_count: r.get::<_, i64>(6)? as u64,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)?;
            Ok(Some(files))
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to list project files from SQLite.");
            ApiError::from(e)
        })?;

    match result {
        Some(files) => Ok(Json(FileListResponse { files })),
        None => Err(ApiError::ProjectNotFound),
    }
}

/// `POST /projects/{guid}/retry` — requeues `failed` files for the retry worker by
/// resetting their retry counter. The `include`/`exclude` selector (same shape as
/// cancel/delete) is **optional**: an empty body requeues *every* `failed` file —
/// retry is non-destructive, so a blanket dead-letter recovery is the useful default.
///
/// This is a **metadata-only** write: `status` stays `failed`, so it never passes
/// through the state-machine triggers (no transition to reject) and never takes the
/// per-file `IndexClaim`. It deliberately leaves `status_updated_at` untouched — the
/// retry worker only picks a `failed` file whose `status_updated_at` is older than
/// 60s, so keeping the old timestamp lets the next sweep (≤60s) re-embed it at once;
/// bumping it would add a needless 60s delay. It races benignly with that worker,
/// which re-checks status under its own claim. Returns 204 when nothing matched, else
/// 200 with the count of files requeued.
///
/// **Concurrency:** safe — a metadata-only write (`retry_count = 0`) that skips the
/// state-machine triggers and takes no claim. It races benignly with the retry worker.
/// An empty body requeues *every* `failed` file (retry is non-destructive).
#[utoipa::path(
    post,
    path = "/projects/{project_guid}/retry",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = RetryRequest,
    responses(
        (status = 200, description = "Matched `failed` files requeued for the retry worker.", body = RetryResponse),
        (status = 204, description = "No `failed` files matched — nothing changed."),
        (status = 400, description = "Oversized selector.", body = ProblemDetails),
        (status = 500, description = "SQLite failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_retry(
    IndexScope(project_guid, _auth): IndexScope,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<RetryRequest>,
) -> Result<Response, ApiError> {
    // Retry deliberately allows an empty body (= every `failed` file), so no
    // non-empty-selector requirement — only the pattern-count cap applies.
    validate::validate_selector(&req.include, s.max_selector_patterns)?;
    validate::validate_selector(&req.exclude, s.max_selector_patterns)?;

    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;
    let (where_sql, binds) = build_file_filter(pg, &req.include, &req.exclude);

    // `build_file_filter` already pins the project (and excludes 'deleted'); appending
    // a constant `status = 'failed'` keeps the existing bind numbering intact.
    let update_sql =
        format!("UPDATE project_files SET retry_count = 0 WHERE {where_sql} AND status = 'failed'");
    let requeued_files: u64 = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let n = tx.execute(&update_sql, params_from_iter(binds.iter()))?;
            Ok(n as u64)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = ?e, project_guid = %pg.0, "Failed to requeue failed files.");
            ApiError::from(e)
        })?;

    info!(project_guid = %pg.0, requeued_files, "Requeued failed files for the retry worker.");

    if requeued_files == 0 {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((StatusCode::OK, Json(RetryResponse { requeued_files })).into_response())
    }
}

/// `GET /status` — a live runtime/concurrency snapshot for diagnostics: how many
/// per-file indexing claims are held right now, whether a GC pass is running, SQLite
/// pool headroom, and global `project_files` counts by status. Cheap — one grouped
/// SQLite read plus two in-memory reads. Distinct from `GET /health` (dependency
/// liveness) and `GET /config` (static knobs).
///
/// **Concurrency:** safe — read-only. This is the endpoint to inspect *why* you saw a
/// 409 (`gc_running`) or a 500 (`pool_available` at 0); `indexing_claims` shows how
/// many files are mid-pipeline (same-file collisions are now skipped, not 429).
#[utoipa::path(
    get,
    path = "/status",
    tag = "Observability",
    responses(
        (status = 200, description = "Live runtime/concurrency snapshot.", body = StatusResponse),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_status(
    AdminScope(_auth): AdminScope,
    State(s): State<RouterState>,
) -> Result<Json<StatusResponse>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());

    let counts: Vec<(String, i64)> = s
        .db_pool
        .transaction(guard.0.child_token(), |tx| {
            tx.prepare("SELECT status, COUNT(*) FROM project_files GROUP BY status")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(SQLite3PoolError::from)
        })
        .with_cancellation_token(&guard.0)
        .await
        .from_cancelled()
        .map_err(|e| {
            error!(error = %e, "Failed to read global file-status counts from SQLite.");
            ApiError::from(e)
        })?;

    let mut files_by_status = FileStatusCounts::default();
    for (status, n) in &counts {
        files_by_status.set(status, *n as u64);
    }
    let indexing_files = files_by_status.indexing as i64;

    // In-memory state: the per-file claim table and the GC flag. Recover from a
    // poisoned lock rather than panic (it is a plain membership set — see `IndexClaim`).
    let indexing_claims = s
        .indexing_locks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    let gc_running = s.gc_flag.load(std::sync::atomic::Ordering::Acquire);

    Ok(Json(StatusResponse {
        indexing_claims,
        gc_running,
        pool_available: s.db_pool.available().await,
        pool_size: s.db_pool.size(),
        indexing_files,
        files_by_status,
    }))
}

/// `GET /config` — server capabilities and tuning knobs (the running version, the
/// embedding model, the canonical supported-language list, and the CLI-set
/// concurrency knobs). The `languages` array is the single source of truth clients
/// (e.g. the search frontend) read instead of hardcoding their own copy.
///
/// Almost all of it is fixed for the life of the process, but **not quite all**:
/// `research.models` is the local Ollama registry as of the last refresh, so a
/// client that wants a current model list re-reads this endpoint rather than
/// caching it once at startup.
///
/// **Concurrency:** safe — in-memory values plus one uncontended read of the model
/// catalog; no I/O.
#[utoipa::path(
    get,
    path = "/config",
    tag = "Config",
    responses(
        (status = 200, description = "Capabilities and tuning knobs, incl. the canonical language list and the current Ollama model list.", body = ConfigResponse),
    ),
)]
#[debug_handler]
pub async fn get_config(State(s): State<RouterState>) -> Json<ConfigResponse> {
    Json(config_snapshot(&s).await)
}

/// The assembled `/config` answer, shared by [`get_config`] and [`get_llms_txt`]
/// so the numbers the bootstrap document quotes can never disagree with what
/// `/config` serves — one assembly, two renderings.
async fn config_snapshot(s: &RouterState) -> ConfigResponse {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    // Cloned out and the guard dropped before anything else: the writer is a worker
    // on a tick, and this handler never holds the lock across an `.await`.
    let catalog = s.research_models.read().await.clone();
    // Same treatment, same reason: a worker writes it, this handler clones it out
    // and drops the guard rather than holding a lock across anything.
    let stats = s.research_stats.read().await.clone();
    ConfigResponse {
        version: env!("CARGO_PKG_VERSION"),
        model_id: model_id.clone(),
        languages: ProgrammingLanguage::ALL.iter().map(|l| l.name()).collect(),
        embed_batch: s.embed_tuning.embed_batch,
        db_pool_size: s.db_pool_size,
        stuck_grace_mins: s.stuck_grace_mins,
        max_retries: s.max_retries,
        search: SearchConfigInfo {
            default_top_k: s.default_top_k,
            max_top_k: s.max_top_k,
            max_query_bytes: s.max_query_bytes,
        },
        research: ResearchConfigInfo {
            default_model: s.research_default_model.clone(),
            // Filtered at read time, not in the catalog worker: the raw catalog
            // is "what Ollama has", the whitelist is presentation policy, and a
            // config reload story is simpler when only one of them owns it.
            models: if s.research_allowed_models.is_unrestricted() {
                catalog.models
            } else {
                catalog
                    .models
                    .into_iter()
                    .filter(|m| s.research_allowed_models.allows(m))
                    .collect()
            },
            allowed_models: s.research_allowed_models.patterns(),
            models_refreshed_at: catalog.refreshed_at,
            effort: ResearchEffortLadder {
                low: ResearchEffortInfo::new(&s.research_effort.low, s.research_report_timeout_ms),
                medium: ResearchEffortInfo::new(
                    &s.research_effort.medium,
                    s.research_report_timeout_ms,
                ),
                high: ResearchEffortInfo::new(
                    &s.research_effort.high,
                    s.research_report_timeout_ms,
                ),
            },
            max_request_seconds: s.research_max_request_seconds,
            max_request_tokens: s.research_max_request_tokens,
            max_request_steps: s.research_max_request_steps,
            max_request_report_sections: s.research_max_request_report_sections,
            max_request_report_words: s.research_max_request_report_words,
            max_evidence_width: s.research_max_evidence_width,
            max_concurrent: s.research_max_concurrent,
            max_context_runs: s.research_max_context_runs,
            max_context_chars: s.research_max_context_chars,
            report_timeout_ms: s.research_report_timeout_ms,
            checkpoint_every_steps: s.research_checkpoint_every_steps,
            list_page_limit: s.research_list_page_limit,
            max_delete_ids: s.max_research_delete_ids,
            sampling: ResearchSamplingInfo {
                temperature: s.research_sampling.temperature,
                top_p: s.research_sampling.top_p,
                seed: s.research_sampling.seed,
            },
            observed: ResearchObservedInfo {
                refreshed_at: stats.refreshed_at,
                efforts: stats
                    .observed
                    .into_iter()
                    .map(|o| ResearchObservedEffort {
                        model: o.model,
                        effort: o.effort,
                        runs: o.runs,
                        p50_seconds: o.p50_seconds,
                        p90_seconds: o.p90_seconds,
                    })
                    .collect(),
            },
        },
    }
}

/// The `content-type` `/llms.txt` is served as. Markdown, not `text/plain`: the
/// document *is* markdown and its consumers render or reason over structure.
pub const LLMS_TXT_CONTENT_TYPE: &str = "text/markdown; charset=utf-8";

/// `GET /llms.txt` — the bootstrap document for AI agents: a hand-written
/// workflow narrative (`llms_doc.md`, embedded at compile time) plus a live
/// configuration section rendered from the same [`config_snapshot`] that
/// `GET /config` serves, so the numbers the prose quotes cannot drift from the
/// JSON. The point is that a model handed only this URL can drive the whole
/// search/research workflow without MCP configuration or repo access.
///
/// **Deliberately undocumented in OpenAPI.** It is not JSON, not versioned, not
/// problem+json, and its consumer is a language model reading prose rather than
/// an API client — the `/metrics` precedent, which
/// `openapi_spec_is_complete_and_versioned` asserts on purpose. The
/// `llms_doc_mentions_only_routes_that_exist` test is the drift guard in the
/// other direction: every route the narrative names must exist in the spec.
///
/// **Concurrency:** safe — in-memory values plus two uncontended snapshot
/// reads; no I/O.
#[debug_handler]
pub async fn get_llms_txt(State(s): State<RouterState>) -> Response {
    let body = llms_document(&config_snapshot(&s).await);
    (
        [(axum::http::header::CONTENT_TYPE, LLMS_TXT_CONTENT_TYPE)],
        body,
    )
        .into_response()
}

/// The whole `/llms.txt` body: the static narrative plus the live section.
/// Pure over the snapshot so tests can render it without a `RouterState`.
fn llms_document(c: &ConfigResponse) -> String {
    format!(
        "{}\n{}",
        include_str!("llms_doc.md"),
        render_llms_live_section(c)
    )
}

/// Shape version of `/.well-known/mindex.json`. Bumped when a field changes
/// meaning, not when the server is upgraded — the two move independently, which
/// is why the document reports both.
pub const DESCRIPTOR_VERSION: u32 = 2;

/// `GET /.well-known/mindex.json` — what this server is, as data.
///
/// The machine twin of [`get_llms_txt`], and the floor under it. The narrative
/// is fetched over the network by a model whose client may classify a document
/// addressed to it as a prompt injection — observed in the field — and a caller
/// that loses it is left with nothing, because it was the only entry point.
/// JSON has no register to object to, so an agent that can reach the origin can
/// always discover the service, its endpoints and its current limits.
///
/// **Documented in OpenAPI, unlike `/llms.txt` and `/metrics`.** Same question,
/// opposite answer, and the difference is the audience: those two serve prose to
/// a reader and exposition to a scraper, this serves JSON to an API client —
/// which is what the spec exists to describe.
///
/// **Concurrency:** safe — a memoized endpoint inventory plus the same two
/// uncontended snapshot reads `GET /config` makes; no I/O.
#[utoipa::path(
    get,
    path = "/.well-known/mindex.json",
    tag = "Config",
    responses((
        status = 200,
        description = "Service identity, endpoint inventory and the live configuration snapshot.",
        body = MindexDescriptor,
    )),
)]
#[debug_handler]
pub async fn get_mindex_descriptor(State(s): State<RouterState>) -> Json<MindexDescriptor> {
    let schema_version = s.db_schema_version;
    let auth_enabled = s.auth.is_some();
    Json(descriptor_document(
        config_snapshot(&s).await,
        schema_version,
        auth_enabled,
    ))
}

/// The whole descriptor. Pure over the snapshot so tests can build it without a
/// `RouterState`, exactly as [`llms_document`] is.
fn descriptor_document(
    config: ConfigResponse,
    db_schema_version: i32,
    auth_enabled: bool,
) -> MindexDescriptor {
    MindexDescriptor {
        service: "mindex",
        summary: "Semantic code index over indexed source trees, with a local research agent \
                  that investigates them and returns cited reports.",
        version: env!("CARGO_PKG_VERSION"),
        db_schema_version,
        descriptor_version: DESCRIPTOR_VERSION,
        documents: DescriptorDocuments {
            openapi: "/api-docs/openapi.json",
            openapi_ui: "/swagger-ui",
            narrative: "/llms.txt",
        },
        // Present only when this deployment actually requires a token. An
        // always-on description would tell a caller to obtain a credential the
        // server would then ignore, which is a worse answer than silence.
        authentication: auth_enabled.then(|| DescriptorAuthentication {
            kind: "bearer-jwt",
            scheme: "Authorization: Bearer <token>",
            actions: crate::backend::auth::Action::ALL
                .iter()
                .map(|a| a.as_str())
                .collect(),
            note: "A project outside the token's scope answers 404, identically to a project \
                   that was never indexed. Do not render that as absence.",
        }),
        transport: DescriptorTransport {
            tls: true,
            alpn: vec!["h2", "http/1.1"],
        },
        endpoints: descriptor_endpoints().clone(),
        projects_url: "/projects",
        health_url: "/health",
        config_url: "/config",
        config,
    }
}

/// The endpoint inventory, built once from the OpenAPI spec.
///
/// Derived rather than written: the route table already exists in the router,
/// the spec, the narrative and the MCP tool sets, and a hand-maintained fifth
/// copy would be the one with nothing checking it. Built from the *serialized*
/// spec rather than utoipa's types for the reason the tests in this file do the
/// same — the JSON is the contract, and it cannot be invalidated by a utoipa
/// upgrade rearranging its internals.
fn descriptor_endpoints() -> &'static Vec<DescriptorEndpoint> {
    static ENDPOINTS: std::sync::LazyLock<Vec<DescriptorEndpoint>> =
        std::sync::LazyLock::new(build_descriptor_endpoints);
    &ENDPOINTS
}

fn build_descriptor_endpoints() -> Vec<DescriptorEndpoint> {
    let spec = serde_json::to_value(crate::backend::openapi::api_doc())
        .expect("the OpenAPI spec serializes — `openapi_spec_is_complete_and_versioned` pins it");

    let mut out: Vec<DescriptorEndpoint> = Vec::new();

    if let Some(paths) = spec["paths"].as_object() {
        for (path, item) in paths {
            let Some(ops) = item.as_object() else {
                continue;
            };
            for (method, op) in ops {
                // A path item also carries non-operation keys (`parameters`,
                // `summary`); only the verbs describe an endpoint.
                let method_upper = method.to_ascii_uppercase();
                if !matches!(
                    method_upper.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "TRACE"
                ) {
                    continue;
                }
                out.push(DescriptorEndpoint {
                    summary: operation_summary(op),
                    tag: op["tags"][0].as_str().map(str::to_string),
                    streaming: streaming_encoding(&method_upper, path),
                    method: method_upper,
                    path: path.clone(),
                    documented: true,
                });
            }
        }
    }

    // Real routes with no JSON contract to document. Reported so a caller sees
    // the whole surface, flagged so it knows the spec will not describe them.
    for (path, summary) in crate::backend::http3::UNDOCUMENTED_ROUTES {
        if crate::backend::http3::DESCRIPTOR_HIDDEN_ROUTES.contains(path) {
            continue;
        }
        out.push(DescriptorEndpoint {
            method: "GET".to_string(),
            path: (*path).to_string(),
            summary: (*summary).to_string(),
            tag: None,
            streaming: None,
            documented: false,
        });
    }

    // Deterministic, so the document diffs cleanly and a test can pin it. Sorted
    // rather than left in spec order: `serde_json::Value` preserves key order
    // only under a cargo feature, so relying on it would be a silent dependency.
    out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
    out
}

/// One line describing what an operation returns.
///
/// utoipa fills `summary` from the first line of a handler's doc comment only
/// when a blank line follows it, and folds everything into `description`
/// otherwise — so both shapes have to be handled or half the inventory would
/// report an empty string.
fn operation_summary(op: &serde_json::Value) -> String {
    if let Some(s) = op["summary"].as_str()
        && !s.trim().is_empty()
    {
        return s.trim().to_string();
    }
    op["description"]
        .as_str()
        .and_then(|d| d.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim()
        .to_string()
}

/// How this endpoint's response arrives, for the few that arrive in frames.
fn streaming_encoding(method: &str, path: &str) -> Option<&'static str> {
    crate::backend::http3::STREAMING_ENDPOINTS
        .iter()
        .find(|(m, p, _)| *m == method && *p == path)
        .map(|(_, _, enc)| *enc)
}

/// The "Live configuration" markdown appended to the narrative: the numbers a
/// caller needs before its first request — models, ladder, measured costs,
/// ceilings — restated from [`ConfigResponse`] rather than written, so they are
/// current by construction. Absent data is stated as absent (an unrefreshed
/// catalog, no observed runs), never papered over with an invented value.
fn render_llms_live_section(c: &ConfigResponse) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    // Infallible: `write!` into a `String` cannot fail.
    let _ = writeln!(out, "## Live configuration\n");
    let _ = writeln!(
        out,
        "Rendered at request time from the same snapshot `GET /config` serves \
         (mindex {}, embedding model `{}`).\n",
        c.version, c.model_id
    );

    let r = &c.research;
    let _ = writeln!(out, "### Research models\n");
    if r.models.is_empty() {
        if r.models_refreshed_at.is_none() {
            let _ = writeln!(
                out,
                "The model catalog has not been refreshed yet — what Ollama has is \
                 unknown right now. It is refreshed on a \
                 `[research].models_refresh_interval_seconds` tick, and \
                 `GET /config` carries the same list once it has been.\n"
            );
        } else {
            let _ = writeln!(
                out,
                "No models are currently available to `/research` (Ollama reports \
                 none, or none pass the `allowed_models` whitelist).\n"
            );
        }
    } else {
        for m in &r.models {
            let _ = writeln!(out, "- `{m}`");
        }
        let _ = writeln!(out);
    }
    if r.default_model.is_empty() {
        let _ = writeln!(
            out,
            "There is no default model: every research request must name one.\n"
        );
    } else {
        let _ = writeln!(
            out,
            "Requests that name no model run on `{}`.\n",
            r.default_model
        );
    }

    let _ = writeln!(out, "### Effort ladder (what each level grants)\n");
    let _ = writeln!(
        out,
        "| effort | max_seconds | max_tokens | max_steps | report sections | \
         report words | worst_case_seconds |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|");
    for (name, e) in [
        ("low", &r.effort.low),
        ("medium", &r.effort.medium),
        ("high", &r.effort.high),
    ] {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} |",
            name,
            e.max_seconds,
            e.max_tokens,
            e.max_steps,
            e.max_report_sections,
            e.max_report_words,
            e.worst_case_seconds
        );
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "### Measured cost (what a run actually takes)\n");
    if r.observed.efforts.is_empty() {
        let _ = writeln!(
            out,
            "No (model, effort) pair has enough journalled runs for an estimate \
             yet — fall back to the grants above.\n"
        );
    } else {
        let _ = writeln!(out, "| model | effort | runs | p50 s | p90 s |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for o in &r.observed.efforts {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                o.model, o.effort, o.runs, o.p50_seconds, o.p90_seconds
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "### Bounds\n");
    let _ = writeln!(
        out,
        "- Research slots (`max_concurrent`): {} — a 429 `research.busy` means \
         all are taken.",
        r.max_concurrent
    );
    let _ = writeln!(
        out,
        "- Budget override ceilings: {} seconds, {} tokens, {} steps, {} report \
         sections, {} report words, evidence width {}.",
        r.max_request_seconds,
        r.max_request_tokens,
        r.max_request_steps,
        r.max_request_report_sections,
        r.max_request_report_words,
        r.max_evidence_width
    );
    let _ = writeln!(
        out,
        "- Context chaining: up to {} prior runs, {} chars of their reports \
         injected.",
        r.max_context_runs, r.max_context_chars
    );
    let _ = writeln!(
        out,
        "- Search: top_k defaults to {} (max {}), queries up to {} bytes.",
        c.search.default_top_k, c.search.max_top_k, c.search.max_query_bytes
    );
    let _ = writeln!(
        out,
        "- Indexed languages the server supports: {}.",
        c.languages.join(", ")
    );
    out
}

/// The columns a summary is built from, shared by the list and the detail so the two
/// can never describe the same run differently. `?2` is the embedding model id.
/// Any query selecting these must prepend [`research_validity_ctes`] — `invalid` is
/// one of its CTEs, not a table.
///
/// The last three resolve a challenge's **subject** — its `seq`, and enough to
/// build its title by the same rule the row's own title uses. Three correlated
/// primary-key lookups, returning no row (and so NULL) for an ordinary research
/// run or a subject that has since been deleted. They exist because a challenge
/// row must be able to name what it attacked wherever it is rendered, and the
/// client used to guess: it looked for the subject among the rows it happened to
/// have loaded and degraded to a bare "open subject" link otherwise.
/// How many columns [`research_summary_columns`] selects, i.e. the index the
/// *next* column a caller appends will land on.
///
/// The detail query selects the summary and then four columns of its own, and
/// read them back at hardcoded indices — so adding one summary column silently
/// shifted `report` into `invalid_flag`'s place and the run's own report became
/// whatever the next column held. Naming the boundary is what stops the two from
/// drifting; `research_summary_columns_are_counted_correctly` pins it.
const RESEARCH_SUMMARY_COLUMNS: usize = 26;

/// The derived trust status of run `r` — the challenge channel's verdict,
/// aggregated at read time exactly as validity is (nothing is ever written to
/// the subject's row). Only **valid** challenges count: a challenge whose own
/// evidence has moved, or whose subject-project chain broke, stops counting the
/// moment it goes stale — the same `invalid` CTE decides both. Severity wins
/// across challenges; an inconclusive challenge (NULL verdict — its verdict
/// turn parsed to nothing) counts toward none of the three, so a run whose only
/// challenge was inconclusive reads `unchallenged` here and the challenge
/// itself stays visible in the corpus.
///
/// Requires [`research_validity_ctes`] prepended, like `invalid_flag`.
fn research_trust_column() -> &'static str {
    "COALESCE((SELECT CASE
         WHEN SUM(c.challenge_verdict = 'refuted') > 0 THEN 'refuted'
         WHEN SUM(c.challenge_verdict = 'disputed') > 0 THEN 'disputed'
         WHEN SUM(c.challenge_verdict = 'confirmed') > 0 THEN 'confirmed'
       END
       FROM research_runs c
      WHERE c.kind = 'challenge'
        AND c.challenged_run_id = r.id
        AND NOT EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = c.id)), 'unchallenged') AS trust"
}

fn research_summary_columns() -> String {
    format!(
        "r.id, r.seq, r.question, r.created_at, r.expires_at, r.model, r.effort,
         r.done_reason, r.citations_total, r.citations_verified, r.citations_unverified,
         r.steps, r.elapsed_ms, {}, r.title,
         EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = r.id) AS invalid_flag,
         COALESCE((SELECT n FROM refs WHERE refs.run_id = r.id), 0) AS references_count,
         COALESCE((SELECT n FROM refd WHERE refd.run_id = r.id), 0) AS referenced_by_count,
         r.kind, r.challenged_run_id, r.challenge_verdict,
         {trust},
         (SELECT s.seq FROM research_runs s WHERE s.id = r.challenged_run_id)
             AS challenged_seq,
         (SELECT s.title FROM research_runs s WHERE s.id = r.challenged_run_id)
             AS challenged_title,
         (SELECT s.question FROM research_runs s WHERE s.id = r.challenged_run_id)
             AS challenged_question",
        research_staleness_columns("?2"),
        trust = research_trust_column(),
    )
}

/// The corpus totals for one project: `?1` the guid, `?2` the embedding model id.
///
/// **No filter from the request is applied here, ever** — see
/// [`ResearchCorpusTotals`]. They are a fixed denominator; a count that shrank as
/// the reader typed into the search box would be a worse rendering of the page
/// length they can already see.
///
/// `gc_candidates` is a SUM over the **union** of the four buckets, not their
/// sum: a run that is both stale and partial is one report to delete, and a
/// button labelled with the sum would promise more than the pass then proposes.
/// The buckets are all `unpinned` — pinning is the one thing that takes a run off
/// the table, so the number and the proposal built from it cannot disagree.
///
/// Selects no report body, and reuses the `invalid`/`moved` CTEs the page query
/// already built, so this costs one more scan of one project's retained runs.
fn research_totals_sql() -> String {
    format!(
        "{ctes}
         SELECT COUNT(*),
                SUM(inv = 0),
                SUM(r.kind = 'challenge'),
                SUM(m.files_moved > 0),
                SUM(unpinned AND (inv = 1 OR m.files_moved > 0
                                  OR r.done_reason <> 'finalized'
                                  OR (r.kind = 'challenge'
                                      AND r.challenge_verdict IS NULL))),
                SUM(unpinned AND inv = 1),
                SUM(unpinned AND m.files_moved > 0),
                SUM(unpinned AND r.done_reason <> 'finalized'),
                SUM(unpinned AND r.kind = 'challenge'
                             AND r.challenge_verdict IS NULL)
           FROM (
               SELECT r.*,
                      EXISTS (SELECT 1 FROM invalid i WHERE i.run_id = r.id) AS inv,
                      (r.expires_at IS NOT NULL) AS unpinned
                 FROM research_runs r
                WHERE r.project_guid = ?1
           ) r
           JOIN moved m ON m.run_id = r.id",
        ctes = research_validity_ctes("?1", "?2"),
    )
}

fn research_totals_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResearchCorpusTotals> {
    // Every SUM is NULL over an empty corpus; COUNT is not.
    let n = |i: usize| -> rusqlite::Result<i64> { Ok(row.get::<_, Option<i64>>(i)?.unwrap_or(0)) };
    Ok(ResearchCorpusTotals {
        total: row.get(0)?,
        current: n(1)?,
        challenges: n(2)?,
        stale: n(3)?,
        gc_candidates: n(4)?,
        gc_invalid: n(5)?,
        gc_stale: n(6)?,
        gc_partial: n(7)?,
        gc_inconclusive: n(8)?,
    })
}

/// Build a summary from a row selected with [`research_summary_columns`].
///
/// `context` and `invalid_reason` are left for [`fill_validity`]: they need the
/// ancestry query, which runs once per page rather than per row.
fn research_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResearchRunSummary> {
    let question: String = row.get(2)?;
    let expires_at: Option<i64> = row.get(4)?;
    let files_moved: i64 = row.get(14)?;
    let stored_title: Option<String> = row.get(15)?;
    let invalid_flag: bool = row.get(16)?;
    // The subject's title by the *same* rule the row's own title uses below —
    // stored heading first, derived from the question otherwise — so a challenge
    // names its subject exactly as the subject's own row does. NULL for an
    // ordinary run, and for a subject that has been deleted since.
    let subject_title: Option<String> = row.get(24)?;
    let subject_question: Option<String> = row.get(25)?;
    let challenged_title = subject_title.or_else(|| subject_question.map(|q| research_title(&q)));
    Ok(ResearchRunSummary {
        id: row.get(0)?,
        seq: row.get(1)?,
        // The stored title — the report's own heading — when the run journalled
        // one; the derived question truncation otherwise. One non-null field on
        // the wire either way: `question` rides beside it, so a client that wants
        // the distinction has it.
        title: stored_title.unwrap_or_else(|| research_title(&question)),
        question,
        created_at: row.get(3)?,
        expires_at,
        pinned: expires_at.is_none(),
        model: row.get(5)?,
        effort: row.get(6)?,
        done_reason: row.get(7)?,
        citations_total: row.get(8)?,
        citations_verified: row.get(9)?,
        citations_unverified: row.get(10)?,
        steps: row.get(11)?,
        elapsed_ms: row.get(12)?,
        files_total: row.get(13)?,
        files_moved,
        stale: files_moved > 0,
        valid: !invalid_flag,
        invalid_reason: None,
        references_count: row.get(17)?,
        referenced_by_count: row.get(18)?,
        context: Vec::new(),
        kind: row.get(19)?,
        challenged_run_id: row.get(20)?,
        challenge_verdict: row.get(21)?,
        trust: row.get(22)?,
        challenged_seq: row.get(23)?,
        challenged_title,
    })
}

/// `GET /projects/{project_guid}/research` — the stored-research index, newest first.
///
/// **Keyset, never `OFFSET`.** Pages resume from `before_seq` against the unique
/// `(project_guid, seq)` index, which serves the equality, the range and the
/// `ORDER BY … DESC` in one backwards index scan with no sort. Offset paging over a
/// table that GC prunes and every run appends to would skip and repeat rows.
///
/// **The report body is never selected**, only searched — that is the whole reason
/// this is a separate endpoint from the detail one.
///
/// Every summary carries the run's derived `valid` verdict and its flat transitive
/// `context` ancestry (see [`research_validity_ctes`]); `valid=true|false` filters
/// on it, orthogonally to `freshness`, which stays the run's *own* staleness.
/// `kind=research|challenge` restricts by the stored `kind` column — the browse
/// half of the challenge feature; like every filter here it applies before the
/// page is cut. So do `completeness=finalized|partial` (whether the run reached
/// its own conclusion or a budget stopped it) and `challenged_run_id=<id>`, which
/// answers "what was said about *that* report" — the one query that finds a
/// challenge whose verdict was inconclusive or whose own evidence has since
/// moved, both of which `trust` correctly stops counting.
///
/// **Every filter applies before the `LIMIT`, and that is a contract, not an
/// implementation detail.** A client pruning a corpus pages this list to
/// exhaustion and stops when a page comes back short; a filter applied after the
/// cut would advance the cursor while returning fewer rows, and "short page means
/// no more" — the only inference on offer — would be wrong.
///
/// `q` is a `LIKE` over the title, the question and the report body, with
/// [`like_escape`], for the reason `grep` needs it: `_` is a
/// wildcard and this corpus's questions are full of identifiers. No index serves it;
/// the scan is bounded by one project's retained runs and stopped by `limit`. FTS5 is
/// the next rung of the documented ladder and is deliberately not taken — nothing has
/// measured `LIKE` insufficient over a corpus two orders of magnitude smaller than
/// `project_file_chunks`, which is the table that ladder was written about.
///
/// **Concurrency:** safe — read-only, takes no locks.
#[utoipa::path(
    get,
    path = "/projects/{project_guid}/research",
    tag = "Research",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        ResearchListQuery,
    ),
    responses(
        (status = 200, description = "One keyset page of stored runs, newest first, without their reports, plus `totals` — corpus-wide counts for the project that no filter on this request affects.", body = ResearchRunListResponse),
        (status = 400, description = "Malformed query parameter, or `limit` above `[research].list_page_limit`.", body = ProblemDetails),
        (status = 404, description = "The project has never been seen.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_research_runs(
    ResearchScope(project_guid, _auth): ResearchScope,
    State(s): State<RouterState>,
    ApiQuery(q): ApiQuery<ResearchListQuery>,
) -> Result<Json<ResearchRunListResponse>, ApiError> {
    validate::research_list_limit(q.limit, s.research_list_page_limit)?;
    let limit = q.limit.unwrap_or(s.research_list_page_limit);
    let guard = http3::CancellationGuard(CancellationToken::new());
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    let pg = project_guid;
    let pg_simple = project_guid.0.simple().to_string();

    let result = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM projects WHERE guid = ?1",
                    rusqlite::params![pg],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Ok(None);
            }

            let mut where_parts = vec!["r.project_guid = ?1".to_string()];
            let mut binds: Vec<Bind> = vec![Bind::Guid(pg), Bind::Path(model_id.clone())];
            let mut n = 3usize;
            if let Some(pattern) = q.q.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                // NULL title in the OR is harmless: `NULL LIKE x` is NULL, and the
                // question clause still decides.
                where_parts.push(format!(
                    "(r.title LIKE ?{n} ESCAPE '\\' OR r.question LIKE ?{n} ESCAPE '\\' \
                      OR r.report LIKE ?{n} ESCAPE '\\')"
                ));
                binds.push(Bind::Path(format!("%{}%", like_escape(pattern))));
                n += 1;
            }
            if let Some(before) = q.before_seq {
                where_parts.push(format!("r.seq < ?{n}"));
                binds.push(Bind::Path(before.to_string()));
                n += 1;
            }
            if let Some(pinned) = q.pinned {
                where_parts.push(
                    if pinned {
                        "r.expires_at IS NULL"
                    } else {
                        "r.expires_at IS NOT NULL"
                    }
                    .to_string(),
                );
            }
            if let Some(kind) = q.kind {
                where_parts.push(
                    match kind {
                        ResearchKind::Research => "r.kind = 'research'",
                        ResearchKind::Challenge => "r.kind = 'challenge'",
                    }
                    .to_string(),
                );
            }
            match q.completeness.unwrap_or(ResearchCompleteness::All) {
                ResearchCompleteness::All => {}
                ResearchCompleteness::Finalized => {
                    where_parts.push("r.done_reason = 'finalized'".to_string());
                }
                ResearchCompleteness::Partial => {
                    where_parts.push("r.done_reason <> 'finalized'".to_string());
                }
            }
            if let Some(subject) = q.challenged_run_id.as_deref() {
                // Served by `idx_research_runs_challenged`, which had no reader
                // until this filter — it was built for the trust subquery and is
                // exactly the index this wants.
                where_parts.push(format!("r.challenged_run_id = ?{n}"));
                binds.push(Bind::Path(subject.to_string()));
                n += 1;
            }

            // The freshness and validity filters are applied INSIDE, against the
            // derived columns, and before the LIMIT. Filtering a page after cutting
            // it would return fewer rows than asked for while the cursor still
            // advanced, so "a short page means there is no more" — the inference
            // the client draws — would be wrong.
            let mut outer = Vec::new();
            match q.freshness.unwrap_or(ResearchFreshness::All) {
                ResearchFreshness::All => {}
                ResearchFreshness::Fresh => outer.push("files_moved = 0"),
                ResearchFreshness::Stale => outer.push("files_moved > 0"),
            }
            match q.valid {
                Some(true) => outer.push("invalid_flag = 0"),
                Some(false) => outer.push("invalid_flag = 1"),
                None => {}
            }
            let outer_where = if outer.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", outer.join(" AND "))
            };
            let sql = format!(
                "{ctes}
                 SELECT * FROM (
                     SELECT {cols}
                       FROM research_runs r
                      WHERE {where_clause}
                 ) {outer_where}
                 ORDER BY seq DESC
                 LIMIT ?{n}",
                ctes = research_validity_ctes("?1", "?2"),
                cols = research_summary_columns(),
                where_clause = where_parts.join(" AND "),
            );
            binds.push(Bind::Path(limit.to_string()));

            let mut stmt = tx.prepare(&sql)?;
            let mut runs = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    research_summary_from_row(row)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            // The ancestry, once for the whole page rather than per row.
            let ids: Vec<String> = runs.iter().map(|r| r.id.clone()).collect();
            let mut deps = research_dependencies(tx, &pg_simple, &model_id, &ids)?;
            for run in &mut runs {
                fill_validity(run, deps.remove(&run.id).unwrap_or_default());
            }

            // The corpus totals: NONE of the request's filters are applied here,
            // deliberately (see `ResearchCorpusTotals`). Same transaction, same
            // validity CTE the page above already built and paid for, and no
            // report body — so the whole thing is one extra scan of one project's
            // retained runs.
            //
            // `gc_candidates` is a SUM over the *union* rather than the sum of the
            // four buckets: a run that is both stale and partial is one report to
            // delete, and a button labelled with the sum would promise more than
            // the pass then proposes.
            let totals = tx.query_row(
                &research_totals_sql(),
                rusqlite::params![pg, model_id],
                research_totals_from_row,
            )?;

            Ok(Some((runs, totals)))
        })
        .await
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to list stored research runs. Check the database is readable."
            );
            ApiError::from(e)
        })?;

    let (runs, totals) = result.ok_or(ApiError::ProjectNotFound)?;
    // Only a full page can have more behind it. Saying so here costs nothing and
    // saves the client a request whose only answer is "no".
    let next_before_seq = (runs.len() == limit)
        .then(|| runs.last().map(|r| r.seq))
        .flatten();
    Ok(Json(ResearchRunListResponse {
        runs,
        next_before_seq,
        totals,
    }))
}

/// `GET /projects/{project_guid}/research/{run_id}` — one stored run in full.
///
/// Carries the Markdown report and the per-file freshness detail behind the list's
/// `stale` boolean: a file that was *edited* and one that was *deleted* call for
/// different reading, and a single flag cannot say which happened.
///
/// **Concurrency:** safe — read-only, takes no locks.
#[utoipa::path(
    get,
    path = "/projects/{project_guid}/research/{run_id}",
    tag = "Research",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        ("run_id" = String, Path, description = "The run's stable id (from `done.run_id` or the list)."),
    ),
    responses(
        (status = 200, description = "The stored run, including its Markdown report and per-file freshness.", body = ResearchRunDetail),
        (status = 404, description = "This project has no such run.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_research_run(
    ResearchScope((project_guid, run_id), _auth): ResearchScope<(UUIDv4, String)>,
    State(s): State<RouterState>,
) -> Result<Json<ResearchRunDetail>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    let pg = project_guid;
    let pg_simple = project_guid.0.simple().to_string();
    let rid = run_id.clone();

    let found = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let sql = format!(
                "{ctes}
                 SELECT {cols}, r.report, r.prompt_version, r.context_run_ids_json, r.scope_json
                   FROM research_runs r
                  WHERE r.project_guid = ?1 AND r.id = ?3",
                ctes = research_validity_ctes("?1", "?2"),
                cols = research_summary_columns(),
            );
            let row = tx
                .query_row(&sql, rusqlite::params![pg, model_id, rid], |row| {
                    let summary = research_summary_from_row(row)?;
                    // The four detail-only columns, appended after the summary's —
                    // indexed from the boundary rather than from a literal, so a
                    // new summary column cannot quietly redirect them.
                    let n = RESEARCH_SUMMARY_COLUMNS;
                    let context_json: String = row.get(n + 2)?;
                    Ok((
                        summary,
                        row.get::<_, String>(n)?,
                        row.get::<_, String>(n + 1)?,
                        context_json,
                        row.get::<_, Option<String>>(n + 3)?,
                    ))
                })
                .optional()?;
            let Some((mut summary, report, prompt_version, context_json, scope)) = row else {
                return Ok(None);
            };
            let mut deps = research_dependencies(tx, &pg_simple, &model_id, &[summary.id.clone()])?;
            let own_deps = deps.remove(&summary.id).unwrap_or_default();
            fill_validity(&mut summary, own_deps);

            // The baselines, joined against the index as it stands. LEFT JOIN, because
            // a file that has left the index is a *result* here and not a missing row.
            let mut stmt = tx.prepare(
                "SELECT rf.path, rf.sha256, pf.sha256
                   FROM research_run_files rf
                   LEFT JOIN project_files pf
                          ON pf.project_guid = ?1
                         AND pf.model_id     = ?2
                         AND pf.path         = rf.path
                         AND pf.status      != 'deleted'
                  WHERE rf.run_id = ?3
                  ORDER BY rf.path",
            )?;
            // `model_id` is the EMBEDDING model, which is what project_files is keyed
            // by. `summary.model` is the Ollama model that drove the run — a different
            // thing entirely, and binding it here made every file read as `removed`.
            let files = stmt
                .query_map(rusqlite::params![pg, &model_id, &summary.id], |row| {
                    let sha256: String = row.get(1)?;
                    let current: Option<String> = row.get(2)?;
                    let state = match &current {
                        None => "removed",
                        Some(now) if now.eq_ignore_ascii_case(&sha256) => "fresh",
                        Some(_) => "changed",
                    };
                    Ok(ResearchRunFile {
                        path: row.get(0)?,
                        sha256,
                        current_sha256: current,
                        state,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(Some(ResearchRunDetail {
                summary,
                report,
                prompt_version,
                context_run_ids: serde_json::from_str(&context_json).unwrap_or_default(),
                scope,
                files,
            }))
        })
        .await
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to read a stored research run. Check the database is readable."
            );
            ApiError::from(e)
        })?;

    found
        .map(Json)
        .ok_or(ApiError::ResearchRunNotFound { run_id })
}

/// `GET /projects/{project_guid}/research/{run_id}/verification` — re-check a
/// stored report's citations against the journal and today's index.
///
/// The whole point of journalling the evidence spans: `check_citations` is a pure
/// function of `(report, spans, staleness)`, all three now in SQLite, so the check
/// re-runs **offline** — no model, no GPU, cheap enough to call whenever currency
/// matters. Two different questions come back:
///
/// - **Provenance** (`recomputed` vs `recorded`, `provenance_matches`): immutable
///   facts about the run. A mismatch means the journal or the reconstruction is
///   wrong — a bug, never news about the code. Scored **twice**, once with citation
///   path resolution and once without, and matching either way counts: resolution
///   arrived with `PROMPT_VERSION` 2.4 and changes the verdict on a bare filename,
///   so scoring an older row only the new way would report a correct journal as
///   broken. Retire the second scoring when a resolved path is stored per citation.
/// - **Staleness** (`stale_citations_now`, `files_moved`): computed against the
///   index as it stands *now*. This is the number that moves, and the reason to
///   call this endpoint at all.
///
/// Runs journalled before v1.3.0 have no stored spans; for them
/// `spans_available` is `false` and only the staleness half is computed —
/// recomputing provenance without spans would score every citation `unverified`
/// and read as a degraded report, which would be the check lying. Nothing is
/// stamped: like validity, the verdict is derived at read time, so it can never
/// disagree with a recomputation.
///
/// **Concurrency:** read-only — safe concurrently with anything, including a
/// live research run.
#[utoipa::path(
    get,
    path = "/projects/{project_guid}/research/{run_id}/verification",
    tag = "Research",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        ("run_id" = String, Path, description = "The run's stable id."),
    ),
    responses(
        (status = 200, description = "The re-checked citation report: recorded vs recomputed provenance, plus citation staleness against today's index.", body = ResearchVerification),
        (status = 404, description = "This project has no such run.", body = ProblemDetails),
        (status = 500, description = "SQLite read failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn get_research_verification(
    ResearchScope((project_guid, run_id), _auth): ResearchScope<(UUIDv4, String)>,
    State(s): State<RouterState>,
) -> Result<Json<ResearchVerification>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    let pg = project_guid;
    let pg_simple = project_guid.0.simple().to_string();
    let rid = run_id.clone();

    type FileStates = std::collections::HashMap<String, (bool, bool)>;
    let found = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let sql = format!(
                "{ctes}
                 SELECT {cols}, r.report, r.citations_path_only, r.stale_citations,
                        r.started_at
                   FROM research_runs r
                  WHERE r.project_guid = ?1 AND r.id = ?3",
                ctes = research_validity_ctes("?1", "?2"),
                cols = research_summary_columns(),
            );
            let row = tx
                .query_row(&sql, rusqlite::params![pg, model_id, rid], |row| {
                    let summary = research_summary_from_row(row)?;
                    // Indexed from the summary boundary, like the detail query —
                    // a new summary column must not quietly redirect these.
                    let n = RESEARCH_SUMMARY_COLUMNS;
                    Ok((
                        summary,
                        row.get::<_, String>(n)?,
                        row.get::<_, i64>(n + 1)?,
                        row.get::<_, i64>(n + 2)?,
                        row.get::<_, Option<i64>>(n + 3)?,
                    ))
                })
                .optional()?;
            let Some((mut summary, report, recorded_path_only, recorded_stale, started_at)) = row
            else {
                return Ok(None);
            };
            let mut deps = research_dependencies(tx, &pg_simple, &model_id, &[summary.id.clone()])?;
            let own_deps = deps.remove(&summary.id).unwrap_or_default();
            fill_validity(&mut summary, own_deps);

            // Per-path staleness NOW: the same LEFT JOIN as the detail endpoint,
            // reduced to the two flags `Evidence` keeps. A missing project_files
            // row is `removed` — a result, not an absence (the run_files comment).
            let mut stmt = tx.prepare(
                "SELECT rf.path, rf.sha256, pf.sha256
                   FROM research_run_files rf
                   LEFT JOIN project_files pf
                          ON pf.project_guid = ?1
                         AND pf.model_id     = ?2
                         AND pf.path         = rf.path
                         AND pf.status      != 'deleted'
                  WHERE rf.run_id = ?3",
            )?;
            let states: FileStates = stmt
                .query_map(rusqlite::params![pg, &model_id, &summary.id], |row| {
                    let path: String = row.get(0)?;
                    let baseline: String = row.get(1)?;
                    let current: Option<String> = row.get(2)?;
                    let (changed, removed) = match &current {
                        None => (false, true),
                        Some(now) if now.eq_ignore_ascii_case(&baseline) => (false, false),
                        Some(_) => (true, false),
                    };
                    Ok((path, (changed, removed)))
                })?
                .collect::<Result<_, _>>()?;

            let mut stmt =
                tx.prepare("SELECT path, spans_json FROM research_run_evidence WHERE run_id = ?1")?;
            let spans_rows: Vec<(String, String)> = stmt
                .query_map(rusqlite::params![&summary.id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<_, _>>()?;

            Ok(Some((
                summary,
                report,
                recorded_path_only,
                recorded_stale,
                started_at,
                states,
                spans_rows,
            )))
        })
        .await
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to read a stored research run for verification. Check the \
                 database is readable."
            );
            ApiError::from(e)
        })?;

    let Some((summary, report, recorded_path_only, recorded_stale, started_at, states, spans_rows)) =
        found
    else {
        return Err(ApiError::ResearchRunNotFound { run_id });
    };

    // Spans were journalled atomically with the row from v1.3.0 on, and
    // `started_at` arrived in the same migration — so it is the discriminator
    // between "this run recorded no spans" (old row) and "this run was shown
    // nothing" (legitimately empty evidence).
    let spans_available = started_at.is_some();

    // The stored evidence, staleness flags merged in. Baseline-only paths (probed
    // for a hash but journalled before spans existed) still enter with no spans,
    // so the staleness half covers every run either way.
    let mut stored: Vec<crate::research::StoredEvidence> = spans_rows
        .into_iter()
        .map(|(path, json)| {
            let (changed, removed) = states.get(&path).copied().unwrap_or((false, false));
            crate::research::StoredEvidence {
                spans: serde_json::from_str(&json).unwrap_or_default(),
                changed,
                removed,
                path,
            }
        })
        .collect();
    for (path, (changed, removed)) in &states {
        if !stored.iter().any(|e| &e.path == path) {
            stored.push(crate::research::StoredEvidence {
                path: path.clone(),
                spans: Vec::new(),
                changed: *changed,
                removed: *removed,
            });
        }
    }

    let rechecked = crate::research::recheck_citations(&report, &stored);
    let recorded = CitationCounts {
        total: summary.citations_total,
        verified: summary.citations_verified,
        path_only: recorded_path_only,
        unverified: summary.citations_unverified,
        stale: recorded_stale,
    };
    let counts = |r: &crate::research::CitationReport| CitationCounts {
        total: r.total as i64,
        verified: r.verified as i64,
        path_only: r.path_only as i64,
        unverified: r.unverified as i64,
        stale: r.stale as i64,
    };
    // Provenance only: `stale` is expected to move, and folding it in would make
    // every honest re-check after an edit read as a journal bug.
    let provenance = |r: &CitationCounts| {
        (r.total, r.verified, r.path_only, r.unverified)
            == (
                recorded.total,
                recorded.verified,
                recorded.path_only,
                recorded.unverified,
            )
    };

    // Scored twice, and the run is called honest if *either* scoring reproduces
    // what was journalled. Path resolution (PROMPT_VERSION 2.4) turns a bare
    // filename that names exactly one shown file from `unverified` into `verified`,
    // so re-checking an older row the new way would report a perfectly correct
    // journal as broken — and `provenance_matches: false` is rendered everywhere as
    // "the journal or this reconstruction is wrong, report a bug", a claim that must
    // never fire on healthy history. Both are pure functions over rows already in
    // memory, so the second scoring costs microseconds.
    //
    // The cost is real and bounded: for a report containing a resolvable bare
    // filename the check can no longer tell the two scorings apart, so it would miss
    // a journal that recorded the *other* one. Every other report scores identically
    // either way and is checked exactly as strictly as before. This goes away with a
    // stored resolved path per citation, which wants a migration.
    let resolved = spans_available.then(|| counts(&rechecked));
    let exact = spans_available
        .then(|| counts(&crate::research::recheck_citations_exact(&report, &stored)));
    let (recomputed, provenance_matches) = match (resolved, exact) {
        (Some(res), Some(ex)) => {
            let ok = provenance(&res) || provenance(&ex);
            // Report the scoring that matched, preferring today's — so the block a
            // caller reads is the one the verdict was reached on.
            let shown = if provenance(&res) { res } else { ex };
            (Some(shown), Some(ok))
        }
        _ => (None, None),
    };

    Ok(Json(ResearchVerification {
        run_id: summary.id.clone(),
        seq: summary.seq,
        valid: summary.valid,
        invalid_reason: summary.invalid_reason,
        spans_available,
        recorded,
        recomputed,
        provenance_matches,
        stale_citations_now: rechecked.stale as i64,
        stale_paths_now: rechecked.stale_paths,
        files_total: summary.files_total,
        files_moved: summary.files_moved,
    }))
}

/// `POST /projects/{project_guid}/research/{run_id}/pin` — exempt a run from the
/// retention sweep, or return it to it.
///
/// The **one** mutation on a row that is otherwise append-only, and it writes a
/// single column. `pinned: true` clears `expires_at`; `pinned: false` restores
/// `created_at + [research].retention_days`, which means unpinning a run older than
/// the window makes it eligible at the very next GC pass. That is deliberate:
/// stamping `now + retention` instead would turn "let it age normally" into a way of
/// *extending* a run's life, and a client toggling a checkbox twice would silently
/// renew everything it touched.
///
/// `pinned` **defaults to `true`**, so `{}` pins. Requiring it made the obvious
/// call on an endpoint named `/pin` a 400 naming a field the caller had no reason
/// to guess; unpinning is the surprising direction, and that is the one worth
/// spelling out.
///
/// **Concurrency:** safe — one row, one statement, no locks.
#[utoipa::path(
    post,
    path = "/projects/{project_guid}/research/{run_id}/pin",
    tag = "Research",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        ("run_id" = String, Path, description = "The run's stable id."),
    ),
    request_body(content = ResearchPinRequest, description = "`{\"pinned\": false}` returns the run to the retention sweep. `pinned` defaults to `true`, so `{}` pins."),
    responses(
        (status = 200, description = "The updated run summary, so the client renders the server's answer rather than its own guess.", body = ResearchRunSummary),
        (status = 404, description = "This project has no such run.", body = ProblemDetails),
        (status = 500, description = "SQLite write failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_research_pin(
    ResearchScope((project_guid, run_id), _auth): ResearchScope<(UUIDv4, String)>,
    State(s): State<RouterState>,
    ApiJson(req): ApiJson<ResearchPinRequest>,
) -> Result<Json<ResearchRunSummary>, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.to_string();
    let pg = project_guid;
    let pg_simple = project_guid.0.simple().to_string();
    let rid = run_id.clone();
    let retention_secs = (s.research_retention_days as i64).saturating_mul(24 * 3600);

    let found = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let changed = tx.execute(
                "UPDATE research_runs
                    SET expires_at = CASE WHEN ?3 THEN NULL ELSE created_at + ?4 END
                  WHERE project_guid = ?1 AND id = ?2",
                rusqlite::params![pg, rid, req.pinned, retention_secs],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            let sql = format!(
                "{ctes}
                 SELECT {cols} FROM research_runs r WHERE r.project_guid = ?1 AND r.id = ?3",
                ctes = research_validity_ctes("?1", "?2"),
                cols = research_summary_columns(),
            );
            let summary = tx
                .query_row(&sql, rusqlite::params![pg, model_id, rid], |row| {
                    research_summary_from_row(row)
                })
                .optional()?;
            let Some(mut summary) = summary else {
                return Ok(None);
            };
            let mut deps = research_dependencies(tx, &pg_simple, &model_id, &[summary.id.clone()])?;
            let own_deps = deps.remove(&summary.id).unwrap_or_default();
            fill_validity(&mut summary, own_deps);
            Ok(Some(summary))
        })
        .await
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to change a stored research run's pin state. Check the database is \
                 writable."
            );
            ApiError::from(e)
        })?;

    found
        .map(Json)
        .ok_or(ApiError::ResearchRunNotFound { run_id })
}

/// `DELETE /projects/{project_guid}/research/{run_id}` — drop one stored run.
///
/// Immediate hard delete; `research_run_files` cascades. Idempotent **204**, matching
/// `DELETE /projects/{guid}`: deleting something already gone is the outcome the
/// caller asked for. It exists because waiting out a TTL is not a workflow — a report
/// you know to be wrong should be removable the moment you know it.
///
/// Deleting a run also invalidates, at read time, every run whose context chain
/// reaches it: the survivors keep its id in `context_run_ids_json`, the id now
/// dangles, and [`research_validity_ctes`] reads a dangling reference as invalid,
/// transitively. No write happens anywhere but this row.
///
/// **Concurrency:** safe — one row, no locks, no Qdrant contact (a run owns no
/// vectors).
#[utoipa::path(
    delete,
    path = "/projects/{project_guid}/research/{run_id}",
    tag = "Research",
    params(
        ("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form."),
        ("run_id" = String, Path, description = "The run's stable id."),
    ),
    responses(
        (status = 204, description = "The run is gone (idempotent — also returned when it never existed)."),
        (status = 500, description = "SQLite write failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn delete_research_run(
    DeleteScope((project_guid, run_id), _auth): DeleteScope<(UUIDv4, String)>,
    State(s): State<RouterState>,
) -> Result<StatusCode, ApiError> {
    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;

    let id = run_id.clone();
    let deleted = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            let n = tx.execute(
                "DELETE FROM research_runs WHERE project_guid = ?1 AND id = ?2",
                rusqlite::params![pg, id],
            )?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to delete a stored research run. Check the database is writable."
            );
            ApiError::from(e)
        })?;

    // Logged even though the response cannot say it: this is the only mutation that
    // removes a stored run one at a time, and until it spoke, a corpus that had lost
    // rows was indistinguishable from one that never recorded them — a research run
    // vanishing has three possible causes (a disconnect before the journal, a report
    // refused by the Markdown gate, and this), and the other two already say so.
    // `deleted_runs` distinguishes a real removal from the idempotent no-op that
    // returns the same 204.
    info!(
        project_guid = %project_guid.0,
        run_id = %run_id,
        deleted_runs = deleted,
        "Deleted a stored research run."
    );

    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /projects/{project_guid}/research` — drop a batch of stored runs.
///
/// The plural of [`delete_research_run`], and it exists because pruning a corpus is
/// a *set* operation: the runs worth removing are the ones a human just picked out
/// of a list, and one request per pick makes "clear these twenty" twenty chances to
/// fail halfway. One transaction, one `IN (…)`, so the batch either lands or does
/// not.
///
/// Unknown ids are ignored rather than 404 — the same idempotence the single-run
/// delete has, and the only sane answer for a batch where one id was already gone.
/// `deleted_runs` is what actually went, so a caller can tell the difference. An
/// **empty** list is a 400 (`selector.empty`), the `require_nonempty_selector` rule:
/// emptying a project's corpus is asked for by naming its runs, never reached by
/// posting `{}`.
///
/// Like the single delete, this invalidates every descendant at read time and
/// writes nothing to reach them — see [`research_validity_ctes`].
///
/// **Concurrency:** safe — rows only, no locks, no Qdrant contact.
#[utoipa::path(
    delete,
    path = "/projects/{project_guid}/research",
    tag = "Research",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = DeleteResearchRunsRequest,
    responses(
        (status = 200, description = "Runs matched and deleted.", body = DeleteResearchRunsResponse),
        (status = 204, description = "None of the named runs existed — nothing changed."),
        (status = 400, description = "Empty or oversized id list.", body = ProblemDetails),
        (status = 500, description = "SQLite write failure.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn delete_research_runs(
    DeleteScope(project_guid, _auth): DeleteScope,
    State(s): State<RouterState>,
    ApiJson(mut req): ApiJson<DeleteResearchRunsRequest>,
) -> Result<Response, ApiError> {
    validate::research_delete_ids(&mut req.ids, s.max_research_delete_ids)?;

    let guard = http3::CancellationGuard(CancellationToken::new());
    let pg = project_guid;
    let ids = req.ids;

    let deleted = s
        .db_pool
        .transaction(guard.0.child_token(), move |tx| {
            // `?1` is the project; the ids follow it, so the first is `?2`.
            let placeholders = (0..ids.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
            binds.push(&pg);
            for id in &ids {
                binds.push(id);
            }
            let n = tx.execute(
                &format!(
                    "DELETE FROM research_runs \
                      WHERE project_guid = ?1 AND id IN ({placeholders})"
                ),
                binds.as_slice(),
            )?;
            Ok(n as u64)
        })
        .await
        .map_err(|e| {
            error!(
                error = ?e,
                project_guid = %project_guid.0,
                "Failed to delete a batch of stored research runs. Check the database is writable."
            );
            ApiError::from(e)
        })?;

    if deleted == 0 {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    info!(
        project_guid = %project_guid.0,
        deleted_runs = deleted,
        "Deleted stored research runs."
    );
    Ok(Json(DeleteResearchRunsResponse {
        deleted_runs: deleted,
    })
    .into_response())
}

/// `POST /gc` — runs a full GC pass synchronously and returns what it removed:
/// hard-deletes soft-deleted chunks (whose vectors are confirmed gone from Qdrant),
/// then the now-empty `deleted` file rows, then prunes the old status log. Blocking
/// by design; the periodic worker runs the same steps hourly. GC is global, so a
/// pass is serialized process-wide by `GcGuard`: a `POST /gc` arriving while one is
/// already running (a concurrent call or the hourly worker's tick) returns 409.
///
/// **Concurrency:** safe but globally serialized — GC is process-wide, so only one pass
/// runs at a time; a concurrent request (or one racing the hourly worker) gets **409**.
/// It only hard-deletes chunks whose Qdrant vectors are confirmed gone, so it never
/// orphans a vector. Synchronous: the response returns when the pass completes.
#[utoipa::path(
    post,
    path = "/gc",
    tag = "Garbage Collection",
    responses(
        (status = 200, description = "GC pass completed; counts of what was physically removed.", body = GcResponse),
        (status = 409, description = "A GC pass is already running (manual or the hourly worker) — retry later.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_gc(
    AdminScope(_auth): AdminScope,
    State(s): State<RouterState>,
) -> Result<Json<GcResponse>, ApiError> {
    let Some(_guard) = crate::worker::gc::GcGuard::try_acquire(&s.gc_flag) else {
        info!("POST /gc rejected: a garbage-collection pass is already running.");
        return Err(ApiError::GcRunning);
    };
    let cg = http3::CancellationGuard(CancellationToken::new());
    let out = crate::worker::gc::collect(
        &s.db_pool,
        &*s.qdrant,
        s.status_log_retention_days,
        &s.metrics,
        "manual",
        &cg.0,
    )
    .await;
    Ok(Json(GcResponse {
        chunks_removed: out.chunks.removed,
        files_removed: out.files.removed,
        status_log_pruned: out.status_log.removed,
        research_runs_pruned: out.research.removed,
        failed_phases: out.failed_phases().into_iter().map(str::to_owned).collect(),
    }))
}

/// `POST /auth/tokens` — issue a scoped bearer token, signed by this server.
///
/// The network half of minting; `mindex mint-token` is the other, and the other
/// is the bootstrap, because this one needs a token to call. That ordering is
/// deliberate — a mint endpoint reachable without one would be a credential
/// vending machine on the open port.
///
/// **A minted token can never exceed its minter** — not a wider action set, not
/// a wider project list, not a later expiry (`Claims::may_mint`). Without that
/// rule, handing somebody a read-only `mint` credential would hand them `admin`
/// one call later, which is exactly the escalation `mint` invites. It is the one
/// piece of logic in this feature with its own test.
///
/// The token is returned once. There is no store of issued tokens, by design:
/// the whole mechanism is stateless, and a list of live credentials would be the
/// state it exists without.
///
/// **Concurrency:** safe — no I/O, no locks; one HMAC.
#[utoipa::path(
    post,
    path = "/auth/tokens",
    tag = "Authorization",
    request_body = MintTokenRequest,
    responses(
        (status = 200, description = "The issued token. Returned once and stored nowhere.", body = MintTokenResponse),
        (status = 400, description = "Unknown action, unparseable project, or a grant wider than the minting token's own.", body = ProblemDetails),
        (status = 401, description = "No token, or one that does not verify.", body = ProblemDetails),
        (status = 403, description = "The token does not carry `mint`.", body = ProblemDetails),
        (status = 404, description = "Authorization is not enabled on this server.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_auth_tokens(
    MintScope(auth): MintScope,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<MintTokenRequest>,
) -> Result<Json<MintTokenResponse>, ApiError> {
    // With authorization off there is no keyring, so there is nothing to sign
    // with. 404 rather than 500: on such a deployment this endpoint genuinely is
    // not a thing the server does, and `MintScope` waved the request through
    // precisely because no token was required.
    let Some(state) = s.auth.as_ref() else {
        return Err(ApiError::ProjectNotFound);
    };
    let Some(minter) = auth.0.as_ref() else {
        return Err(ApiError::TokenMissing);
    };

    let actions = payload
        .actions
        .iter()
        .map(|a| {
            crate::backend::auth::Action::parse(a.trim()).ok_or_else(|| {
                ApiError::MalformedBody(format!(
                    "unknown action {a:?}; the vocabulary is {}",
                    crate::backend::auth::Action::ALL
                        .iter()
                        .map(|a| a.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let audiences = payload
        .audiences
        .iter()
        .map(|a| {
            crate::backend::auth::Audience::parse(a.trim()).ok_or_else(|| {
                ApiError::MalformedBody(format!(
                    "unknown audience {a:?}; the vocabulary is {}",
                    crate::backend::auth::Audience::ALL
                        .iter()
                        .map(|a| a.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // `0` means no expiry, and it is refused here on purpose: minting an eternal
    // credential must require shell access to the host, not a token. The local
    // `mint-token` command is the only way, and its whole audience is an
    // operator who can already read the signing key.
    if payload.days == 0 {
        return Err(ApiError::MalformedBody(
            "days must be at least 1; a non-expiring token can only be minted locally with \
             `mindex mint-token --days 0`"
                .to_string(),
        ));
    }
    let days = payload.days.min(state.max_token_days);

    // Built but not yet signed, so `may_mint` judges the real claim set rather
    // than a paraphrase of the request — the containment rule and the token must
    // be talking about the same thing.
    let (token, claims) = crate::backend::auth::mint_with_key(
        &state.keyring,
        payload.key_id.as_deref(),
        &payload.sub,
        payload.projects.clone(),
        actions,
        audiences,
        days,
    )
    .and_then(|(t, c)| minter.may_mint(&c).map(|()| (t, c)))
    .map_err(|e| {
        warn!(
            error = %e,
            minter = %minter.sub,
            "Refused to mint a token."
        );
        ApiError::MalformedBody(e.to_string())
    })?;

    info!(
        minter = %minter.sub,
        subject = %claims.sub,
        projects = ?claims.prj,
        actions = ?claims.act.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        audiences = ?claims.aud.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        expires_at = claims.exp,
        "Minted a bearer token."
    );

    Ok(Json(MintTokenResponse {
        token,
        expires_at: claims.exp,
        projects: claims.prj,
        actions: claims.act.iter().map(|a| a.as_str().to_string()).collect(),
        audiences: claims.aud.iter().map(|a| a.as_str().to_string()).collect(),
    }))
}

/// Running mindex version (also a trivial liveness ping).
///
/// **Concurrency:** safe — constant, no I/O.
#[utoipa::path(
    get,
    path = "/version",
    tag = "Observability",
    responses((status = 200, description = "The running mindex version.", body = VersionResponse)),
)]
#[debug_handler]
pub async fn get_version(State(s): State<RouterState>) -> Json<VersionResponse> {
    Json(VersionResponse {
        version: env!("CARGO_PKG_VERSION"),
        db_schema_version: s.db_schema_version,
    })
}

/// `GET /metrics` — Prometheus/OpenMetrics exposition of everything under
/// [`crate::backend::metrics`].
///
/// **Deliberately undocumented in OpenAPI.** It is not JSON, not versioned, not
/// problem+json, and its consumer is a scraper rather than an API client — so it
/// is the one route with no `#[utoipa::path]` and no `openapi.rs` entry, which
/// `openapi_spec_is_complete_and_versioned` asserts on purpose. Every other
/// convention from "When modifying code" still holds.
///
/// The cheap process gauges are refreshed here rather than on the collector's
/// tick: they are the same free in-memory reads `get_status` makes, so a scrape
/// may as well report them as of the scrape.
///
/// **Concurrency:** safe — reads counters and one in-memory lock table, no I/O.
#[debug_handler]
pub async fn get_metrics(
    AdminScope(_auth): AdminScope,
    State(s): State<RouterState>,
) -> Result<Response, ApiError> {
    let m = &s.metrics;

    // Recover from a poisoned lock rather than panic, exactly as `get_status`
    // does — it is a plain membership set.
    let claims = s
        .indexing_locks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    m.state.indexing_claims.set(claims as i64);
    // The flag itself is the truth, so read it here rather than trusting the GC
    // worker to have set the gauge — a panic mid-pass frees the flag via `Drop`
    // but would leave a set-on-entry gauge stuck at 1.
    m.gc.running.set(i64::from(
        s.gc_flag.load(std::sync::atomic::Ordering::Acquire),
    ));

    // Both gauges, from the same read. Only `permits_available` used to be
    // refreshed here while `research_active` was left to the collector's tick, so
    // one scrape could report a free permit *and* an active run — two numbers that
    // are each other's complement disagreeing inside a single response.
    let permits = s.research_semaphore.available_permits();
    m.state.research_permits_available.set(permits as i64);
    m.state
        .research_active
        .set(s.research_max_concurrent.saturating_sub(permits) as i64);
    // Free in-memory read, like the claim table above, and the one number that
    // tells a busy slot from a wedged one.
    m.state
        .research_inflight_oldest_age_seconds
        .set((s.research_registry.oldest_age_ms().unwrap_or(0) / 1000) as i64);

    m.db.pool_size.set(s.db_pool.size() as i64);
    m.db.pool_available.set(s.db_pool.available().await as i64);

    let body = m.render().map_err(|e| {
        error!(error = ?e, "Rendering the metrics registry failed.");
        ApiError::Internal
    })?;

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            crate::backend::metrics::CONTENT_TYPE,
        )],
        body,
    )
        .into_response())
}

/// Ceiling on one `GET /health` dependency probe.
///
/// Not configurable, and deliberately shorter than the clients' own health timeouts:
/// this is not "how long may the dependency take", it is "how long may the endpoint
/// that answers whether the service is alive be made to wait". A probe that has not
/// answered in three seconds is reported as failing, which is the honest verdict for
/// a caller deciding whether to send traffic here.
const HEALTH_PROBE_TIMEOUT_MS: u64 = 3_000;

/// A probe with its own ceiling, rendered as a failure when it does not answer in
/// time — `Err(Elapsed)` reads as `error` on the wire and logs a timeout in the
/// journal, which is what a stalled dependency deserves from a liveness check.
async fn bounded<T, E>(
    limit: Duration,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, ProbeFailure<E>> {
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(ProbeFailure::Failed(e)),
        Err(_) => Err(ProbeFailure::TimedOut(limit)),
    }
}

/// Why a bounded probe did not succeed. Kept apart from the dependency's own error
/// so the log says "did not answer in 3s" rather than reporting nothing at all.
#[derive(Debug)]
enum ProbeFailure<E> {
    Failed(E),
    #[allow(
        dead_code,
        reason = "read through the derived Debug in `probe`'s warn!, which dead-code \
                  analysis does not count. The duration is the whole point of the \
                  variant: \"did not answer in 3s\" is the diagnosis."
    )]
    TimedOut(Duration),
}

/// One dependency probe: the verdict for the wire, the reason for the log.
///
/// The raw error deliberately does not travel — see [`CheckState`]. Every probe
/// therefore looks identical from outside and different in the journal, which is
/// the only place it can be acted on, and the only place a hint about *which
/// process to go and start* is worth the bytes.
fn probe<T, E: std::fmt::Debug>(
    dependency: &'static str,
    hint: &'static str,
    result: Result<T, E>,
) -> (CheckState, Option<T>) {
    match result {
        Ok(v) => (CheckState::Ok, Some(v)),
        Err(e) => {
            warn!(
                dependency,
                error = ?e,
                "GET /health: a dependency liveness probe failed; reporting \
                 \"error\" with no detail on the wire. Sysadmin: {hint}"
            );
            (CheckState::Error, None)
        }
    }
}

/// `GET /health` — a *smart* readiness check: confirms both stores (SQLite +
/// Qdrant) and the embedder are reachable, pings the local Ollama behind
/// `/research`, and reports how many files are indexing globally.
///
/// `status` is tri-state and the server, not the caller, decides it: `ok` when
/// everything answers, `degraded` when only the **optional** Ollama is down
/// (indexing and search keep working, `/research` does not), `unhealthy` when a
/// required check failed or a research run is wedged. Two rules that are easy to
/// get backwards: a merely *busy* research slot is the service working and never
/// moves the verdict, and `checks.*` is only ever `"ok"` or `"error"` — the
/// reason a probe failed goes to a `warn!` at the probe site, never on the wire.
///
/// Each check is best-effort and independent, so one dead dependency is
/// pinpointed rather than collapsing the whole response.
///
/// **Concurrency:** safe — read-only probes. Always returns **200** at the HTTP level;
/// inspect the `status` field and per-dependency `checks`.
#[utoipa::path(
    get,
    path = "/health",
    tag = "Observability",
    responses((status = 200, description = "Dependency liveness. `ok` = every check passes; `degraded` = only the optional `ollama` is failing; `unhealthy` = a required check failed or a research run is wedged. Each `checks.*` value is exactly `ok` or `error` — the reason is logged server-side, never returned.", body = HealthResponse)),
)]
#[debug_handler]
pub async fn get_health(State(s): State<RouterState>) -> Json<HealthResponse> {
    let guard = http3::CancellationGuard(CancellationToken::new());

    // Every probe concurrently, and each of them bounded.
    //
    // They used to run one after another, so the worst case was the *sum* of the
    // dependency timeouts rather than the largest — and the SQLite probe had no bound
    // at all: it was the one `transaction` in the file without
    // `with_cancellation_token`, so a wedged pool or a stalled writer hung `GET /health`
    // itself. This is the endpoint every other liveness decision is made from; it is
    // the last one allowed to stop answering.
    let health_deadline = Duration::from_millis(HEALTH_PROBE_TIMEOUT_MS);
    let (sqlite_res, qdrant_res, embedder_res, query_res, ollama_res) = tokio::join!(
        async {
            bounded(
                health_deadline,
                s.db_pool.transaction(guard.0.child_token(), |tx| {
                    tx.query_row(
                        "SELECT COUNT(*) FROM project_files WHERE status = 'indexing'",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .map_err(SQLite3PoolError::from)
                }),
            )
            .await
        },
        bounded(health_deadline, s.qdrant.health()),
        bounded(health_deadline, {
            let EmbeddingModel::BGEm3 { client, .. } = &s.model;
            client.health()
        }),
        async {
            // Pinged separately only when it *is* separate: a split deployment can have
            // a healthy indexer and a dead query instance, which would otherwise show as
            // a green health check and every search failing.
            let EmbeddingModel::BGEm3 { client, .. } = &s.model;
            if Arc::ptr_eq(client, &s.query_model) {
                None
            } else {
                Some(bounded(health_deadline, s.query_model.health()).await)
            }
        },
        bounded(health_deadline, s.research_ollama.health()),
    );

    // SQLite: the global indexing-file count doubles as the liveness query.
    let (sqlite, count) = probe(
        "sqlite",
        "the database file must exist, be writable and have disk behind it; see \
         [database].path.",
        sqlite_res,
    );
    // `-1` is "not known", which is a different answer from `0` and the reason
    // the count rides on a probe that may have failed.
    let indexing_files = count.unwrap_or(-1);

    let (qdrant, _) = probe(
        "qdrant",
        "check Qdrant is running and that [qdrant].url resolves from this process.",
        qdrant_res,
    );

    let (embedder, _) = probe(
        "embedder",
        "check the BGE-M3 server is up and reachable from here — a 0.0.0.0 bind \
         there is not 127.0.0.1 from this process.",
        embedder_res,
    );
    let query_embedder = query_res.map(|r| {
        probe(
            "query_embedder",
            "check [model].query_server_url — the query instance fails \
             independently of the indexing one, and every search embeds \
             through it.",
            r,
        )
        .0
    });

    // The one optional dependency: only `/research` needs it, so it is the sole
    // producer of `degraded` and can never produce `unhealthy`.
    let (ollama, _) = probe(
        "ollama",
        "optional — only /research needs it; check `ollama serve` and \
         [research].url.",
        ollama_res,
    );

    // Research slots. Free reads off in-process state, so they cost nothing and are
    // reported even when Ollama is down — the slots are mindex's own.
    let slots_busy = s.research_registry.len();
    let oldest_inflight_age_ms = s.research_registry.oldest_age_ms();
    // The narrow rule: a *busy* slot is the service working, and must never read as
    // a degradation (with `max_concurrent = 1` that would be permanent). Only a run
    // that has outlived `max_seconds + report_timeout_ms` — every deadline it has —
    // is a defect, and it is one worth surfacing: it is holding a slot nothing else
    // will free until the watchdog gets to it.
    let wedged = s.research_registry.wedged();
    if let Some(oldest) = wedged.iter().max_by_key(|r| r.age_ms()) {
        warn!(
            wedged_runs = wedged.len(),
            oldest_run_id = %oldest.run_id,
            oldest_age_ms = oldest.age_ms(),
            "Research runs are holding slots past their own worst case; reporting \
             unhealthy. Sysadmin: see GET /research/active, and the watchdog will \
             cancel them on its next sweep."
        );
    }

    let checks = HealthChecks {
        sqlite,
        qdrant,
        embedder,
        query_embedder,
        ollama,
    };
    let status = checks.verdict(!wedged.is_empty());

    Json(HealthResponse {
        status,
        version: env!("CARGO_PKG_VERSION"),
        indexing_files,
        checks,
        research: ResearchHealth {
            slots_total: s.research_max_concurrent,
            slots_busy,
            oldest_inflight_age_ms,
        },
    })
}

/// `GET /research/active` — every research run holding a concurrency permit right
/// now, oldest first.
///
/// The stored-research list (`GET /projects/{guid}/research`) shows only runs that
/// **finished**: a run has no `seq` until it is journalled, and a cancelled run is
/// never journalled at all. So a live run was invisible everywhere — which, with
/// `[research].max_concurrent` typically small, meant an occupied slot could not be
/// attributed to a project, a question or an age, and could not be ended short of
/// restarting the process. This endpoint and `DELETE /research/active/{run_id}` are
/// the two halves of that fix.
///
/// Global rather than per-project, because the semaphore is: a caller planning a
/// queue needs to know the slots are gone, not merely that none of *its* runs hold
/// them.
///
/// **Concurrency:** safe — read-only, one in-memory lock, no I/O.
#[utoipa::path(
    get,
    path = "/research/active",
    tag = "Research",
    responses((status = 200, description = "Live research runs, oldest first.", body = ActiveResearchResponse)),
)]
#[debug_handler]
pub async fn get_research_active(
    ActiveRunsScope(auth): ActiveRunsScope,
    State(s): State<RouterState>,
) -> Json<ActiveResearchResponse> {
    let visible = auth.visible_projects();
    let runs: Vec<ActiveResearchRun> = s
        .research_registry
        .snapshot()
        .into_iter()
        // A live run names its project, its question and its model, so the list
        // is content and is filtered. The two counts below are **capacity** and
        // are not: this endpoint exists so a caller can tell that the slots are
        // gone, and a per-caller count would answer "none of yours are running"
        // to a caller about to queue behind somebody else's — the one question
        // it was built to answer, given the wrong answer politely.
        .filter(|r| {
            visible.is_none_or(|v| v.iter().any(|p| p.eq_ignore_ascii_case(&r.project_guid)))
        })
        .map(|r| ActiveResearchRun {
            age_ms: r.age_ms(),
            run_id: r.run_id,
            project_guid: r.project_guid,
            question: r.question,
            model: r.model,
            effort: r.effort.to_string(),
            started_at: r.started_at,
            granted_seconds: r.granted_seconds,
            worst_case_ms: r.worst_case_ms,
        })
        .collect();

    Json(ActiveResearchResponse {
        slots_total: s.research_max_concurrent,
        // Deliberately the registry's own count, not `runs.len()`: after the
        // filter above those differ, and this field must keep meaning "how many
        // of this server's slots are occupied".
        slots_busy: s.research_registry.len(),
        runs,
    })
}

/// `DELETE /research/active/{run_id}` — cancel a running research job.
///
/// Cancellation has always been *disconnect*: dropping the SSE stream cancels the
/// job token. That is still the mechanism — this endpoint cancels the very same
/// token — but it was the only hand on the lever, and it is the wrong one whenever
/// the client that opened the stream is gone while its socket is not. An
/// MCP-shaped caller holds its connection for as long as its own read timeout
/// allows (scout: 70 minutes), so an abandoned tool call leaves the server
/// correctly believing a client is still waiting, with the slot spoken for and no
/// way to say otherwise.
///
/// Idempotent: 204 whether or not the run was found, because "already finished" and
/// "never existed" are the same observable state a moment later and neither is an
/// error the caller can act on differently. The registry entry is **not** removed
/// here — it is released by the job as it unwinds, so a cancelled run keeps
/// (correctly) reporting its slot until it actually lets go.
///
/// **Concurrency:** safe — one in-memory lock, no I/O. Does **not** take an
/// `IndexClaim` or any other lock, and is deliberately callable while the run is
/// mid-turn.
#[utoipa::path(
    delete,
    path = "/research/active/{run_id}",
    tag = "Research",
    params(("run_id" = String, Path, description = "Run id from the `started` event, `done`, or `GET /research/active`.")),
    responses((status = 204, description = "Cancellation requested (or the run was already gone).")),
)]
#[debug_handler]
pub async fn delete_research_active(
    ActiveRunsScope(auth): ActiveRunsScope,
    ApiPath(run_id): ApiPath<String>,
    State(s): State<RouterState>,
) -> StatusCode {
    // A run this caller cannot see is not cancelled — and is answered exactly as
    // an unknown run id is. The endpoint is already documented idempotent either
    // way, so the non-oracle costs nothing: "already finished", "never existed"
    // and "not yours" are one observable state. What that makes invisible is the
    // refusal itself, which is why it is logged, and why the guard test asserts
    // on the target's token rather than on the 204.
    let mine = s
        .research_registry
        .snapshot()
        .into_iter()
        .find(|r| r.run_id == run_id)
        .is_some_and(|r| auth.covers_guid_str(&r.project_guid));

    if mine && s.research_registry.cancel(&run_id) {
        info!(%run_id, "Cancelled a research run on request.");
    } else {
        info!(%run_id, "No live research run with this id in this caller's scope; nothing to cancel.");
    }
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::v0::models::{
        ChangeType, CommitEntry, CommitPath, CommitSummary, GlobPattern, SearchFilter,
    };
    use glob::Pattern;
    use uuid::Uuid;

    /// Count the frames an `SseEventStream` yields once its sender is gone.
    /// Every `poll_recv` here is immediately ready, so a no-op waker is enough
    /// and the test needs no runtime.
    fn drain_frames<E: SseWireEvent>(stream: &mut SseEventStream<E>) -> usize {
        use futures_core::Stream;
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut n = 0;
        loop {
            match std::pin::Pin::new(&mut *stream).poll_next(&mut cx) {
                std::task::Poll::Ready(Some(Ok(_))) => n += 1,
                std::task::Poll::Ready(None) => return n,
                std::task::Poll::Pending => panic!("a closed channel must not park"),
            }
        }
    }

    fn result_at(score: f32, path: &str) -> SearchResult {
        SearchResult {
            score,
            path: path.to_string(),
            code: String::new(),
            start_line: 1,
            end_line: 2,
            start_column: 0,
            end_column: 1,
        }
    }

    fn ranked_paths(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|r| r.path.as_str()).collect()
    }

    /// The ordinary case: best first, whatever order Qdrant's fusion and rerank
    /// happened to return them in.
    #[test]
    fn results_come_back_best_first() {
        let mut results = vec![
            result_at(0.1, "c.rs"),
            result_at(0.9, "a.rs"),
            result_at(0.5, "b.rs"),
        ];
        assert_eq!(rank_by_score(&mut results), 0);
        assert_eq!(ranked_paths(&results), vec!["a.rs", "b.rs", "c.rs"]);
    }

    /// `total_cmp` orders `+NaN` above every finite value, so a plain descending sort
    /// by it hands the **first** result slot to a chunk the reranker could not score —
    /// the top hit, the one an agent reads and a human trusts.
    ///
    /// This is not hypothetical on this hardware: the embedder's XPU backend returns
    /// NaN for padded fp16 rows on its default attention kernel and still answers 200,
    /// and so does a split deployment whose two instances differ in precision. The
    /// symptom is "search sometimes puts something irrelevant first" — a ranking
    /// -quality complaint, not the broken embedder it actually is.
    #[test]
    fn an_unscorable_result_is_ranked_last_not_first() {
        let mut results = vec![
            result_at(0.4, "middle.rs"),
            result_at(f32::NAN, "broken.rs"),
            result_at(0.9, "best.rs"),
        ];

        let unscorable = rank_by_score(&mut results);

        assert_eq!(unscorable, 1, "the NaN score was not counted");
        assert_eq!(
            ranked_paths(&results),
            vec!["best.rs", "middle.rs", "broken.rs"],
            "a chunk the reranker could not score took the top result slot"
        );
        assert!(
            results.last().expect("three results").score.is_nan(),
            "the unscorable result must survive, just not lead"
        );
    }

    /// Every score NaN is the whole-batch version of the same fault. Nothing is
    /// dropped — the chunks matched the filters and the candidate set, so they are
    /// real answers with unusable scores — and the count is what says so.
    #[test]
    fn a_wholly_unscorable_result_set_is_reported_not_discarded() {
        let mut results = vec![
            result_at(f32::NAN, "a.rs"),
            result_at(f32::NAN, "b.rs"),
            result_at(f32::NAN, "c.rs"),
        ];

        assert_eq!(rank_by_score(&mut results), 3);
        assert_eq!(
            results.len(),
            3,
            "an unscorable batch was silently shortened"
        );
        // Stable sort: with nothing to order them by, the reranker's own sequence is
        // the only information left, and it is kept.
        assert_eq!(ranked_paths(&results), vec!["a.rs", "b.rs", "c.rs"]);
    }

    /// Negative and zero scores are ordinary values, not failures — a fusion score can
    /// legitimately be either, and treating them as unscorable would bury real hits.
    #[test]
    fn negative_and_zero_scores_are_ordinary_values() {
        let mut results = vec![
            result_at(-0.5, "neg.rs"),
            result_at(0.0, "zero.rs"),
            result_at(f32::NAN, "nan.rs"),
            result_at(0.2, "pos.rs"),
        ];

        assert_eq!(rank_by_score(&mut results), 1);
        assert_eq!(
            ranked_paths(&results),
            vec!["pos.rs", "zero.rs", "neg.rs", "nan.rs"]
        );
    }

    /// Infinities are orderable and must stay orderable — only NaN is the thing that
    /// cannot be compared at all.
    #[test]
    fn infinities_are_ranked_rather_than_counted_as_unscorable() {
        let mut results = vec![
            result_at(f32::NEG_INFINITY, "worst.rs"),
            result_at(0.5, "mid.rs"),
            result_at(f32::INFINITY, "best.rs"),
        ];

        assert_eq!(rank_by_score(&mut results), 0);
        assert_eq!(
            ranked_paths(&results),
            vec!["best.rs", "mid.rs", "worst.rs"]
        );
    }

    /// A probe that never answers must become a *failure*, not an unbounded wait.
    /// `GET /health` is the endpoint every other liveness decision is made from, and
    /// its SQLite probe was the one `transaction` in this file without a cancellation
    /// binding — a wedged pool hung the check that exists to report a wedged pool.
    #[tokio::test]
    async fn a_probe_that_never_answers_becomes_a_failure_at_the_deadline() {
        let limit = Duration::from_millis(50);
        let started = std::time::Instant::now();

        let out: Result<(), ProbeFailure<&str>> = bounded(limit, async {
            std::future::pending::<Result<(), &str>>().await
        })
        .await;

        assert!(
            matches!(out, Err(ProbeFailure::TimedOut(d)) if d == limit),
            "a hung probe must time out and say how long it waited: {out:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the probe outlived its own ceiling"
        );
    }

    /// The dependency's own error must survive as `Failed`, distinct from a timeout —
    /// "Qdrant said no" and "Qdrant said nothing" are different things to go and look
    /// at, and the `warn!` at the probe site is the only place either is readable.
    #[tokio::test]
    async fn a_probe_that_fails_is_not_reported_as_a_timeout() {
        let out: Result<(), ProbeFailure<&str>> =
            bounded(Duration::from_secs(30), async { Err("connection refused") }).await;

        match out {
            Err(ProbeFailure::Failed(e)) => assert_eq!(e, "connection refused"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A probe that answers in time passes its value through untouched — the SQLite
    /// probe's row count rides out this way, and `-1` (not known) has to stay
    /// distinguishable from a real `0`.
    #[tokio::test]
    async fn a_probe_that_answers_in_time_returns_its_value() {
        let out: Result<i64, ProbeFailure<&str>> =
            bounded(Duration::from_secs(30), async { Ok(0i64) }).await;
        assert_eq!(out.expect("should succeed"), 0);
    }

    /// The endpoint's worst case must be **one** ceiling, not the sum of them. The
    /// probes ran one after another for their whole life, so five dead dependencies
    /// cost five timeouts — on the one endpoint that is never allowed to stop
    /// answering. This pins the shape `get_health` relies on: bounded probes joined,
    /// not awaited in sequence.
    #[tokio::test]
    async fn five_hung_probes_cost_one_deadline_not_five() {
        let limit = Duration::from_millis(80);
        let hung =
            || async move { bounded(limit, std::future::pending::<Result<(), &str>>()).await };

        let started = std::time::Instant::now();
        let (a, b, c, d, e) = tokio::join!(hung(), hung(), hung(), hung(), hung());
        let elapsed = started.elapsed();

        for out in [a, b, c, d, e] {
            assert!(matches!(out, Err(ProbeFailure::TimedOut(_))));
        }
        assert!(
            elapsed < limit * 3,
            "five probes took {elapsed:?} against a {limit:?} ceiling — they are \
             running in sequence, so the worst case is the sum of the dependency \
             timeouts rather than the largest"
        );
    }

    /// A detached job that dies without a terminal event — the shape a panic
    /// takes — must not read as a completed stream. Before this, a research job
    /// that panicked in `parse_citations` closed its channel silently and every
    /// consumer treated the run as finished-with-no-report.
    #[test]
    fn a_stream_whose_job_dies_without_a_terminal_event_still_ends_in_error() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<IndexEvent>();
        let mut stream = SseEventStream::new(rx, CancellationToken::new());
        tx.send(IndexEvent::Started {
            files: 1,
            symbols_only: false,
        })
        .unwrap();
        drop(tx); // the job died here: no `done`, no `error`.

        assert_eq!(
            drain_frames(&mut stream),
            2,
            "the synthetic terminal must be appended"
        );
        assert!(stream.ended, "the stream must have synthesised a terminal");
        assert!(!stream.saw_terminal, "the job never sent one itself");
    }

    /// The mirror: a job that ended properly gets nothing appended, or every
    /// clean run would ship a spurious error after its `done`.
    #[test]
    fn a_stream_that_ended_properly_gets_no_synthetic_terminal() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<IndexEvent>();
        let mut stream = SseEventStream::new(rx, CancellationToken::new());
        tx.send(IndexEvent::Error {
            code: ApiError::Internal.code().to_string(),
            detail: "boom".into(),
        })
        .unwrap();
        drop(tx);

        assert_eq!(drain_frames(&mut stream), 1);
        assert!(stream.saw_terminal);
        assert!(!stream.ended, "nothing should have been synthesised");
    }

    /// The trust status is derived, like validity: severity wins across valid
    /// challenges, an inconclusive challenge counts toward nothing, and a
    /// challenge whose own evidence has moved stops counting the moment it goes
    /// stale — with no write anywhere.
    #[tokio::test]
    async fn trust_aggregates_valid_challenges_and_drops_stale_ones() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        let trust_sql = format!(
            "{ctes} SELECT {trust} FROM research_runs r WHERE r.id = ?3",
            ctes = research_validity_ctes("?1", "?2"),
            trust = research_trust_column(),
        );
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            let insert_run = |id: &str,
                              seq: i64,
                              kind: &str,
                              subject: Option<&str>,
                              verdict: Option<&str>|
             -> rusqlite::Result<()> {
                tx.execute(
                    "INSERT INTO research_runs (
                         id, project_guid, seq, question, model, prompt_version, effort,
                         kind, challenged_run_id, challenge_verdict,
                         granted_seconds, granted_tokens, granted_steps, granted_search_top_k,
                         done_reason, steps, turns, elapsed_ms,
                         prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx,
                         citations_total, citations_verified, citations_path_only,
                         citations_unverified, cited_paths_json, unverified_paths_json,
                         changed_files, removed_files, stale_citations, stale_paths_json,
                         notes_written, notes_rejected, plan_revisions, grep_calls, grep_hits,
                         out_of_scope_refusals, out_of_scope_rows, scoped,
                         forced_synthesis, report_window_ms, report_elapsed_ms, report
                     ) VALUES (
                         ?1, 'p1', ?2, 'q', 'm', '2.2', 'medium',
                         ?3, ?4, ?5,
                         1, 1, 1, 1,
                         'finalized', 1, 1, 1,
                         1, 1, 1, 1,
                         0, 0, 0,
                         0, '[]', '[]',
                         0, 0, 0, '[]',
                         0, 0, 0, 0, 0,
                         0, 0, 0,
                         0, 0, 0, 'report'
                     )",
                    params![id, seq, kind, subject, verdict],
                )?;
                Ok(())
            };
            insert_run("subj", 1, "research", None, None)?;
            let trust = |tx: &rusqlite::Transaction| -> rusqlite::Result<String> {
                tx.query_row(&trust_sql, params!["p1", "BAAI/bge-m3", "subj"], |r| {
                    r.get(0)
                })
            };
            assert_eq!(trust(tx)?, "unchallenged");

            // An inconclusive challenge (NULL verdict) counts toward nothing.
            insert_run("ch-null", 2, "challenge", Some("subj"), None)?;
            assert_eq!(trust(tx)?, "unchallenged");

            insert_run("ch-ok", 3, "challenge", Some("subj"), Some("confirmed"))?;
            assert_eq!(trust(tx)?, "confirmed");

            insert_run("ch-disp", 4, "challenge", Some("subj"), Some("disputed"))?;
            assert_eq!(trust(tx)?, "disputed");

            insert_run("ch-ref", 5, "challenge", Some("subj"), Some("refuted"))?;
            assert_eq!(trust(tx)?, "refuted");

            // The refuting challenge goes stale: its baseline names a file the
            // index does not hold, so the validity CTE reads it `removed` and the
            // refutation stops counting — no write anywhere.
            tx.execute(
                "INSERT INTO research_run_files (run_id, path, sha256)
                 VALUES ('ch-ref', 'src/gone.rs', ?1)",
                params!["0".repeat(64)],
            )?;
            assert_eq!(trust(tx)?, "disputed");
            Ok(())
        })
        .await
        .unwrap();
    }

    /// The `kind` list filter selects on the stored column inside the
    /// cursor-bounded subquery — the shape the handler builds — so a page of
    /// challenges is cut from challenges only and the full-page ⇒ more inference
    /// stays honest. The wire spellings are pinned alongside: an unknown value
    /// must refuse to deserialize (→ 400 malformed query), never scan as "all".
    #[tokio::test]
    async fn kind_filter_selects_only_the_asked_kind() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            let insert_run = |id: &str, seq: i64, kind: &str| -> rusqlite::Result<()> {
                tx.execute(
                    "INSERT INTO research_runs (
                         id, project_guid, seq, question, model, prompt_version, effort,
                         kind, challenged_run_id, challenge_verdict,
                         granted_seconds, granted_tokens, granted_steps, granted_search_top_k,
                         done_reason, steps, turns, elapsed_ms,
                         prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx,
                         citations_total, citations_verified, citations_path_only,
                         citations_unverified, cited_paths_json, unverified_paths_json,
                         changed_files, removed_files, stale_citations, stale_paths_json,
                         notes_written, notes_rejected, plan_revisions, grep_calls, grep_hits,
                         out_of_scope_refusals, out_of_scope_rows, scoped,
                         forced_synthesis, report_window_ms, report_elapsed_ms, report
                     ) VALUES (
                         ?1, 'p1', ?2, 'q', 'm', '2.2', 'medium',
                         ?3, NULL, NULL,
                         1, 1, 1, 1,
                         'finalized', 1, 1, 1,
                         1, 1, 1, 1,
                         0, 0, 0,
                         0, '[]', '[]',
                         0, 0, 0, '[]',
                         0, 0, 0, 0, 0,
                         0, 0, 0,
                         0, 0, 0, 'report'
                     )",
                    params![id, seq, kind],
                )?;
                Ok(())
            };
            insert_run("r1", 1, "research")?;
            insert_run("c1", 2, "challenge")?;
            insert_run("r2", 3, "research")?;

            let page = |tx: &rusqlite::Transaction,
                        kind: Option<ResearchKind>|
             -> rusqlite::Result<Vec<i64>> {
                let mut where_parts = vec!["r.project_guid = ?1".to_string()];
                if let Some(kind) = kind {
                    where_parts.push(
                        match kind {
                            ResearchKind::Research => "r.kind = 'research'",
                            ResearchKind::Challenge => "r.kind = 'challenge'",
                        }
                        .to_string(),
                    );
                }
                let sql = format!(
                    "{ctes}
                     SELECT * FROM (
                         SELECT {cols}
                           FROM research_runs r
                          WHERE {where_clause}
                     )
                     ORDER BY seq DESC
                     LIMIT 10",
                    ctes = research_validity_ctes("?1", "?2"),
                    cols = research_summary_columns(),
                    where_clause = where_parts.join(" AND "),
                );
                let mut stmt = tx.prepare(&sql)?;
                let seqs = stmt
                    .query_map(params!["p1", "BAAI/bge-m3"], |row| {
                        research_summary_from_row(row).map(|r| r.seq)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(seqs)
            };
            assert_eq!(page(tx, None)?, vec![3, 2, 1]);
            assert_eq!(page(tx, Some(ResearchKind::Research))?, vec![3, 1]);
            assert_eq!(page(tx, Some(ResearchKind::Challenge))?, vec![2]);
            Ok(())
        })
        .await
        .unwrap();
    }

    /// One `research_runs` row with every NOT NULL column filled and nothing
    /// interesting in it, for the tests that only care about the columns they set.
    /// Project `p1`, `done_reason = 'finalized'`, unpinned (`expires_at` set).
    fn insert_bare_run(
        tx: &rusqlite::Transaction<'_>,
        id: &str,
        seq: i64,
        kind: &str,
        challenged: Option<&str>,
        verdict: Option<&str>,
    ) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO research_runs (
                 id, project_guid, seq, expires_at, question, model, prompt_version, effort,
                 kind, challenged_run_id, challenge_verdict,
                 granted_seconds, granted_tokens, granted_steps, granted_search_top_k,
                 done_reason, steps, turns, elapsed_ms,
                 prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx,
                 citations_total, citations_verified, citations_path_only,
                 citations_unverified, cited_paths_json, unverified_paths_json,
                 changed_files, removed_files, stale_citations, stale_paths_json,
                 notes_written, notes_rejected, plan_revisions, grep_calls, grep_hits,
                 out_of_scope_refusals, out_of_scope_rows, scoped,
                 forced_synthesis, report_window_ms, report_elapsed_ms, report
             ) VALUES (
                 ?1, 'p1', ?2, 9999999999, 'q', 'm', '2.2', 'medium',
                 ?3, ?4, ?5,
                 1, 1, 1, 1,
                 'finalized', 1, 1, 1,
                 1, 1, 1, 1,
                 0, 0, 0,
                 0, '[]', '[]',
                 0, 0, 0, '[]',
                 0, 0, 0, 0, 0,
                 0, 0, 0,
                 0, 0, 0, 'report'
             )",
            params![id, seq, kind, challenged, verdict],
        )?;
        Ok(())
    }

    /// One page of summaries, built exactly the way the list handler builds it:
    /// the validity CTEs, the shared column list, and the extra predicates inside
    /// the cursor-bounded subquery — which is the property most of these tests are
    /// really about.
    fn summary_page(
        tx: &rusqlite::Transaction<'_>,
        extra_where: &[String],
    ) -> rusqlite::Result<Vec<ResearchRunSummary>> {
        let mut where_parts = vec!["r.project_guid = ?1".to_string()];
        where_parts.extend(extra_where.iter().cloned());
        let sql = format!(
            "{ctes}
             SELECT * FROM (
                 SELECT {cols}
                   FROM research_runs r
                  WHERE {where_clause}
             )
             ORDER BY seq DESC
             LIMIT 100",
            ctes = research_validity_ctes("?1", "?2"),
            cols = research_summary_columns(),
            where_clause = where_parts.join(" AND "),
        );
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt
            .query_map(params!["p1", "BAAI/bge-m3"], research_summary_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A challenge must be able to name what it attacked wherever it is rendered,
    /// so the subject's `seq` and title are resolved server-side. The client used
    /// to look for the subject among the rows it happened to hold and degrade to
    /// an anonymous "open subject" link when it was not there — which, on a list
    /// filtered to challenges, was always.
    ///
    /// The title follows the row's own rule: stored heading first, derived from
    /// the question otherwise. A deleted subject is the one thing NULL now means.
    #[tokio::test]
    async fn a_challenge_summary_names_its_subject() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            insert_bare_run(tx, "subj", 1, "research", None, None)?;
            tx.execute(
                "UPDATE research_runs SET question = 'How does GC work?' WHERE id = 'subj'",
                [],
            )?;
            insert_bare_run(tx, "titled", 2, "research", None, None)?;
            tx.execute(
                "UPDATE research_runs SET title = 'The sweep' WHERE id = 'titled'",
                [],
            )?;
            insert_bare_run(tx, "c1", 3, "challenge", Some("subj"), Some("refuted"))?;
            insert_bare_run(tx, "c2", 4, "challenge", Some("titled"), Some("confirmed"))?;
            // A challenge whose subject was deleted: both columns must read NULL.
            insert_bare_run(tx, "c3", 5, "challenge", Some("gone"), Some("disputed"))?;

            let rows = summary_page(tx, &[])?;
            let by_id = |id: &str| rows.iter().find(|r| r.id == id).expect("row");

            // A research run names no subject at all.
            let subj = by_id("subj");
            assert_eq!(
                (subj.challenged_seq, subj.challenged_title.as_deref()),
                (None, None)
            );

            // Derived from the subject's question, exactly as the subject's own
            // row derives its title.
            let c1 = by_id("c1");
            assert_eq!(c1.challenged_seq, Some(1));
            assert_eq!(c1.challenged_title.as_deref(), Some("How does GC work?"));

            // The subject's stored heading wins, again exactly as it does there.
            let c2 = by_id("c2");
            assert_eq!(c2.challenged_seq, Some(2));
            assert_eq!(c2.challenged_title.as_deref(), Some("The sweep"));

            // Deleted subject.
            let c3 = by_id("c3");
            assert_eq!(
                (c3.challenged_seq, c3.challenged_title.as_deref()),
                (None, None)
            );
            Ok(())
        })
        .await
        .unwrap();
    }

    /// `challenged_run_id` answers "what was said about *that* report", and must
    /// find the challenges `trust` deliberately stops counting — an inconclusive
    /// verdict among them. That is the whole reason it exists: the panel used to
    /// skip the lookup entirely whenever trust read `unchallenged`, so a report
    /// challenged inconclusively showed nothing at all about having been
    /// challenged.
    #[tokio::test]
    async fn the_challenged_run_id_filter_finds_every_challenge_of_one_subject() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            insert_bare_run(tx, "a", 1, "research", None, None)?;
            insert_bare_run(tx, "b", 2, "research", None, None)?;
            insert_bare_run(tx, "ca", 3, "challenge", Some("a"), None)?;
            insert_bare_run(tx, "cb", 4, "challenge", Some("b"), Some("refuted"))?;

            let ids = |subject: &str| -> rusqlite::Result<Vec<String>> {
                let clause = format!("r.challenged_run_id = '{subject}'");
                Ok(summary_page(tx, &[clause])?
                    .into_iter()
                    .map(|r| r.id)
                    .collect())
            };
            // Found despite the NULL verdict, which contributes to no trust value.
            assert_eq!(ids("a")?, vec!["ca".to_string()]);
            assert_eq!(ids("b")?, vec!["cb".to_string()]);
            assert!(ids("nobody")?.is_empty());
            Ok(())
        })
        .await
        .unwrap();
    }

    /// `completeness` is server-side precisely so a client can page to exhaustion
    /// and trust "a short page means no more": the predicate lands inside the
    /// cursor-bounded subquery, before the `LIMIT`, like every other filter here.
    #[tokio::test]
    async fn the_completeness_filter_splits_finished_from_budget_stopped_runs() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            insert_bare_run(tx, "done", 1, "research", None, None)?;
            insert_bare_run(tx, "outoftime", 2, "research", None, None)?;
            insert_bare_run(tx, "outoftokens", 3, "research", None, None)?;
            tx.execute(
                "UPDATE research_runs SET done_reason = 'time_exhausted' WHERE id = 'outoftime'",
                [],
            )?;
            tx.execute(
                "UPDATE research_runs SET done_reason = 'tokens_exhausted' \
                  WHERE id = 'outoftokens'",
                [],
            )?;

            let seqs = |clause: &str| -> rusqlite::Result<Vec<i64>> {
                Ok(summary_page(tx, &[clause.to_string()])?
                    .into_iter()
                    .map(|r| r.seq)
                    .collect())
            };
            assert_eq!(seqs("r.done_reason = 'finalized'")?, vec![1]);
            // Every stop reason at once, deliberately — a reader pruning a corpus
            // does not act on which budget ran out.
            assert_eq!(seqs("r.done_reason <> 'finalized'")?, vec![3, 2]);
            Ok(())
        })
        .await
        .unwrap();
    }

    /// The corpus totals: `current` is the transitive validity verdict (the same
    /// one that decides whether a run may be handed to the next question), the
    /// four GC buckets are unpinned-only, and `gc_candidates` is their **union** —
    /// a run that is both stale and partial is one report to delete, and a button
    /// labelled with the sum would promise more than the pass then proposes.
    #[tokio::test]
    async fn the_corpus_totals_count_the_union_and_never_the_pinned() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            // Clean and finished: neither a candidate nor a problem.
            insert_bare_run(tx, "clean", 1, "research", None, None)?;
            // Both stale and partial — the union must count it once.
            insert_bare_run(tx, "both", 2, "research", None, None)?;
            tx.execute(
                "UPDATE research_runs SET done_reason = 'time_exhausted' WHERE id = 'both'",
                [],
            )?;
            tx.execute(
                "INSERT INTO research_run_files (run_id, path, sha256) \
                 VALUES ('both', 'src/gone.rs', ?1)",
                // No `project_files` row for it, so the LEFT JOIN finds NULL and
                // the baseline counts as moved — the "removed" half of stale.
                params!["de".repeat(32)],
            )?;
            // An inconclusive challenge: its own bucket, and no trust verdict.
            insert_bare_run(tx, "incon", 3, "challenge", Some("clean"), None)?;
            // Pinned AND partial: pinning takes it off the table entirely.
            insert_bare_run(tx, "kept", 4, "research", None, None)?;
            tx.execute(
                "UPDATE research_runs SET expires_at = NULL, done_reason = 'time_exhausted' \
                  WHERE id = 'kept'",
                [],
            )?;

            let t = tx.query_row(
                &research_totals_sql(),
                params!["p1", "BAAI/bge-m3"],
                research_totals_from_row,
            )?;

            assert_eq!(t.total, 4, "every run of the project, of either kind");
            // `both` has a moved baseline file, so it is the only invalid one.
            assert_eq!(t.current, 3);
            assert_eq!(t.challenges, 1, "only `incon`");
            // Pinned included, unlike `gc_stale`: this one is a denominator, not a
            // delete proposal, and exempting pinned runs would under-report drift.
            assert_eq!(t.stale, 1, "only `both`");
            assert_eq!(t.gc_invalid, 1, "only `both`");
            assert_eq!(t.gc_stale, 1, "only `both`");
            assert_eq!(t.gc_partial, 1, "`both`; `kept` is pinned");
            assert_eq!(t.gc_inconclusive, 1, "only `incon`");
            // Two reports, not four: `both` is in three buckets at once.
            assert_eq!(
                t.gc_candidates, 2,
                "the union, never the sum of the buckets"
            );
            Ok(())
        })
        .await
        .unwrap();
    }

    /// A totals query that took the page's filters would just be a second, worse
    /// rendering of `runs.len()`. The SQL is the guard: it binds only the guid and
    /// the model id, and holds no placeholder any filter could reach.
    #[test]
    fn the_corpus_totals_query_takes_no_filter() {
        let sql = research_totals_sql();
        for filter in ["LIKE", "before_seq", "r.seq <", "invalid_flag =", "?3"] {
            assert!(
                !sql.contains(filter),
                "the totals query must be corpus-wide, but it mentions {filter}: {sql}"
            );
        }
        assert!(
            !sql.contains("r.report"),
            "the report body is never selected"
        );
    }

    /// The `completeness` wire spellings are a contract with the extension's
    /// filter select, like `kind`'s.
    #[test]
    fn research_completeness_wire_spellings_are_stable() {
        assert!(matches!(
            serde_json::from_str::<ResearchCompleteness>("\"all\""),
            Ok(ResearchCompleteness::All)
        ));
        assert!(matches!(
            serde_json::from_str::<ResearchCompleteness>("\"finalized\""),
            Ok(ResearchCompleteness::Finalized)
        ));
        assert!(matches!(
            serde_json::from_str::<ResearchCompleteness>("\"partial\""),
            Ok(ResearchCompleteness::Partial)
        ));
        assert!(serde_json::from_str::<ResearchCompleteness>("\"stopped\"").is_err());
        assert!(serde_json::from_str::<ResearchCompleteness>("\"Partial\"").is_err());
    }

    /// The `kind` wire spellings are a contract with the extension's filter
    /// select; an unknown value is a deserialization error, which `ApiQuery`
    /// turns into 400 `request.malformed_body` — never a silent "all".
    #[test]
    fn research_kind_wire_spellings_are_stable() {
        assert!(matches!(
            serde_json::from_str::<ResearchKind>("\"research\""),
            Ok(ResearchKind::Research)
        ));
        assert!(matches!(
            serde_json::from_str::<ResearchKind>("\"challenge\""),
            Ok(ResearchKind::Challenge)
        ));
        assert!(serde_json::from_str::<ResearchKind>("\"bogus\"").is_err());
        assert!(serde_json::from_str::<ResearchKind>("\"Research\"").is_err());
    }

    /// `RESEARCH_SUMMARY_COLUMNS` is the boundary the detail query indexes its own
    /// four columns from, so a summary column added without moving it silently
    /// hands the caller the wrong values — `report` would come back holding
    /// `invalid_flag`. Counted from the SQL itself rather than trusted.
    #[test]
    fn research_summary_columns_are_counted_correctly() {
        let cols = research_summary_columns();
        // One column per top-level comma. The subselects inside contain none, and
        // that is worth asserting rather than assuming.
        let mut depth = 0i32;
        let mut n = 1usize;
        for c in cols.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => n += 1,
                _ => {}
            }
        }
        assert_eq!(depth, 0, "unbalanced parentheses in the column list");
        assert_eq!(
            n, RESEARCH_SUMMARY_COLUMNS,
            "the summary selects {n} columns but RESEARCH_SUMMARY_COLUMNS says \
             {RESEARCH_SUMMARY_COLUMNS}; the detail query indexes its own columns \
             from that constant, so update it in the same commit"
        );
    }

    /// A title is cut from the question at a **word** boundary, because a list of
    /// titles is scanned rather than read: a cut through an identifier
    /// (`post_ind…`) reads as a different symbol, which is exactly the confusion the
    /// list exists to prevent.
    #[test]
    fn a_title_is_the_question_cut_at_a_word_boundary() {
        assert_eq!(research_title("How does GC work?"), "How does GC work?");
        // Collapsed, and only the first non-empty line.
        assert_eq!(
            research_title("\n  How   does\tGC work?\nsecond line"),
            "How does GC work?"
        );

        let long = "How does the prepare phase of post_index decide which files to \
                    skip and which to reslice completely";
        let title = research_title(long);
        assert!(title.ends_with('…'), "{title}");
        assert!(
            title.chars().count() <= RESEARCH_TITLE_CHARS + 1,
            "over the cap: {title}"
        );
        assert!(
            !title.trim_end_matches('…').ends_with(' '),
            "the ellipsis should follow a word, not a space: {title}"
        );
        // The cut fell between words, so every word before it survives whole.
        assert!(long.starts_with(title.trim_end_matches('…')), "{title}");

        // A single word longer than the cap has no boundary to honour; it is cut
        // anyway rather than collapsing to a bare ellipsis.
        let one_word = "a".repeat(200);
        let cut = research_title(&one_word);
        assert_eq!(cut.chars().count(), RESEARCH_TITLE_CHARS + 1);
        assert_eq!(research_title(""), "");
    }

    fn guid() -> UUIDv4 {
        UUIDv4(Uuid::nil())
    }

    fn glob(s: &str) -> GlobPattern {
        GlobPattern(Pattern::new(s).unwrap())
    }

    fn req(include: Option<SearchFilter>, exclude: Option<SearchFilter>) -> SearchRequest {
        SearchRequest {
            query: "q".into(),
            top_k: None,
            include,
            exclude,
        }
    }

    fn langs(v: Vec<ProgrammingLanguage>) -> SearchFilter {
        SearchFilter {
            paths: None,
            programming_languages: Some(v),
        }
    }

    fn paths(v: &[&str]) -> SearchFilter {
        SearchFilter {
            paths: Some(v.iter().map(|s| glob(s)).collect()),
            programming_languages: None,
        }
    }

    // ── hash-skip gating (regression) ───────────────────────────────────
    // The sha256 is written at `indexing` time (the column is NOT NULL), so a
    // file that was sliced but never embedded (embedder down → `failed`) carries
    // the correct hash without any vectors. The unchanged-skip must therefore key
    // on `status = 'indexed'`, not the hash alone — otherwise such a file is
    // skipped on every later re-index and never gets embedded.
    use crate::db::sqlite3::SQLite3Pool;
    use std::path::Path as FsPath;
    use tokio_util::sync::CancellationToken;

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    async fn pool_with_file(status: &'static str, sha: &'static str) -> SQLite3Pool {
        pool_with_versions(
            status,
            sha,
            Some(CHUNKS_DERIVATION_VERSION),
            Some(SYMBOLS_DERIVATION_VERSION),
        )
        .await
    }

    async fn pool_with_versions(
        status: &'static str,
        sha: &'static str,
        chunks_version: Option<&'static str>,
        symbols_version: Option<&'static str>,
    ) -> SQLite3Pool {
        // Pool size 1: the single ":memory:" connection is reused, so the row
        // inserted below is visible to the later hash-check transaction.
        let p = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        p.transaction(CancellationToken::new(), move |tx| {
            tx.execute_batch(
                "CREATE TABLE project_files (
                     project_guid    TEXT NOT NULL,
                     path            TEXT NOT NULL,
                     model_id        TEXT NOT NULL,
                     sha256          TEXT NOT NULL,
                     status          TEXT NOT NULL,
                     chunks_version  TEXT,
                     symbols_version TEXT
                 );",
            )?;
            // `None` binds as SQL NULL and models a file derived by an unknown
            // version: the skip compares for equality, and nothing equals NULL.
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, path, model_id, sha256, status,
                      chunks_version, symbols_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    guid(),
                    "src/a.py",
                    "m",
                    sha,
                    status,
                    chunks_version,
                    symbols_version
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        p
    }

    async fn already_indexed(p: &SQLite3Pool, sha: &'static str) -> bool {
        p.transaction(CancellationToken::new(), move |tx| {
            file_already_indexed(tx, guid(), "src/a.py", "m", sha)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn unversioned_file_is_never_skipped() {
        // Regression guard for the bug this versioning exists to prevent: every file
        // indexed before the symbols feature kept a matching hash, was skipped on
        // every later run, and so never got symbols extracted — `POST /symbols`
        // answered "no such symbol" (which its contract calls definitive) for a third
        // of the tree. Such files have no derivation row, and must never match.
        let p = pool_with_versions("indexed", SHA, None, None).await;
        assert!(
            !already_indexed(&p, SHA).await,
            "a row predating derivation versioning must be rebuilt, not skipped"
        );
    }

    #[tokio::test]
    async fn stale_symbols_version_is_not_skipped() {
        // A bumped tags query must rebuild the file even though its content and its
        // chunk derivation are untouched.
        let p =
            pool_with_versions("indexed", SHA, Some(CHUNKS_DERIVATION_VERSION), Some("0.9")).await;
        assert!(!already_indexed(&p, SHA).await);
    }

    #[tokio::test]
    async fn stale_chunks_version_is_not_skipped() {
        let p = pool_with_versions(
            "indexed",
            SHA,
            Some("0.9"),
            Some(SYMBOLS_DERIVATION_VERSION),
        )
        .await;
        assert!(!already_indexed(&p, SHA).await);
    }

    #[tokio::test]
    async fn current_versions_with_matching_hash_are_skipped() {
        // The other half of the contract: versioning must not defeat the skip itself,
        // or every run re-embeds the whole project.
        let p = pool_with_file("indexed", SHA).await;
        assert!(
            already_indexed(&p, SHA).await,
            "an up-to-date `indexed` file must still be skipped"
        );
    }

    #[tokio::test]
    async fn failed_file_with_matching_hash_is_not_skipped() {
        // A file left `failed` (embedder was down) keeps its content hash but has
        // no vectors — it must be re-indexed, not treated as unchanged.
        let p = pool_with_file("failed", SHA).await;
        assert!(
            !already_indexed(&p, SHA).await,
            "a `failed` file with a matching hash must NOT be skipped"
        );
    }

    #[tokio::test]
    async fn indexing_file_with_matching_hash_is_not_skipped() {
        let p = pool_with_file("indexing", SHA).await;
        assert!(
            !already_indexed(&p, SHA).await,
            "an in-flight `indexing` file must NOT be skipped"
        );
    }

    #[tokio::test]
    async fn indexed_file_with_matching_hash_is_skipped() {
        let p = pool_with_file("indexed", SHA).await;
        assert!(
            already_indexed(&p, SHA).await,
            "a successfully `indexed` file with a matching hash should be skipped"
        );
    }

    #[tokio::test]
    async fn indexed_file_with_changed_hash_is_not_skipped() {
        let p = pool_with_file("indexed", SHA).await;
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(
            !already_indexed(&p, other).await,
            "changed content must be re-indexed"
        );
    }

    #[test]
    fn no_filters_pins_project_and_active_status() {
        let (sql, binds) = build_search_query(guid(), &req(None, None));
        assert!(sql.contains("c.project_guid = ?1"));
        assert!(sql.contains("c.status = 'active'"));
        // No filter clauses beyond the two mandatory ones.
        assert!(!sql.contains("programming_language"));
        assert!(!sql.contains("GLOB"));
        assert_eq!(binds, vec![Bind::Guid(guid())]);
    }

    #[test]
    fn include_languages_numbered_from_two() {
        let (sql, binds) = build_search_query(
            guid(),
            &req(
                Some(langs(vec![
                    ProgrammingLanguage::Rust,
                    ProgrammingLanguage::Python,
                ])),
                None,
            ),
        );
        assert!(
            sql.contains("f.programming_language IN (?2, ?3)"),
            "sql was: {sql}"
        );
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Lang(ProgrammingLanguage::Rust),
                Bind::Lang(ProgrammingLanguage::Python),
            ]
        );
    }

    #[test]
    fn include_paths_use_glob_or() {
        let (sql, binds) =
            build_search_query(guid(), &req(Some(paths(&["src/**", "tests/**"])), None));
        assert!(
            sql.contains("c.file_path GLOB ?2 OR c.file_path GLOB ?3"),
            "sql was: {sql}"
        );
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Path("src/**".into()),
                Bind::Path("tests/**".into()),
            ]
        );
    }

    #[test]
    fn include_langs_and_paths_continue_numbering() {
        // langs take ?2,?3 then paths take ?4,?5 — the bind order must match.
        let inc = SearchFilter {
            paths: Some(vec![glob("a/**"), glob("b/**")]),
            programming_languages: Some(vec![ProgrammingLanguage::Go, ProgrammingLanguage::Sql]),
        };
        let (sql, binds) = build_search_query(guid(), &req(Some(inc), None));
        assert!(
            sql.contains("f.programming_language IN (?2, ?3)"),
            "sql was: {sql}"
        );
        assert!(
            sql.contains("c.file_path GLOB ?4 OR c.file_path GLOB ?5"),
            "sql was: {sql}"
        );
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Lang(ProgrammingLanguage::Go),
                Bind::Lang(ProgrammingLanguage::Sql),
                Bind::Path("a/**".into()),
                Bind::Path("b/**".into()),
            ]
        );
    }

    #[test]
    fn exclude_languages_use_not_in() {
        let (sql, _) = build_search_query(
            guid(),
            &req(None, Some(langs(vec![ProgrammingLanguage::Json]))),
        );
        assert!(
            sql.contains("f.programming_language NOT IN (?2)"),
            "sql was: {sql}"
        );
    }

    #[test]
    fn exclude_paths_are_negated() {
        let (sql, binds) = build_search_query(guid(), &req(None, Some(paths(&["vendor/**"]))));
        assert!(sql.contains("NOT (c.file_path GLOB ?2)"), "sql was: {sql}");
        assert_eq!(
            binds,
            vec![Bind::Guid(guid()), Bind::Path("vendor/**".into())]
        );
    }

    // ── include-glob OR precedence (regression, red until fixed) ────────────
    // Multiple include globs are ORed; without parentheses the OR leaks past the
    // AND-joined project/status pins: `pin AND pin AND g1 OR g2` parses as
    // `(pin AND pin AND g1) OR g2`, so the second glob matches soft-deleted
    // chunks and other projects' chunks.

    #[test]
    fn include_path_glob_group_is_parenthesized() {
        let (sql, _) = build_search_query(guid(), &req(Some(paths(&["src/**", "tests/**"])), None));
        assert!(
            sql.contains("(c.file_path GLOB ?2 OR c.file_path GLOB ?3)"),
            "include glob group must be parenthesized so OR cannot leak past AND: {sql}"
        );
    }

    #[tokio::test]
    async fn include_paths_do_not_leak_foreign_or_deleted_chunks() {
        let p1 = UUIDv4(Uuid::from_u128(1));
        let p2 = UUIDv4(Uuid::from_u128(2));
        let ga = UUIDv4(Uuid::from_u128(0xA)); // P1, active, src/a.rs → expected
        let gt = UUIDv4(Uuid::from_u128(0xB)); // P1, deleted, tests/t.rs → excluded
        let gx = UUIDv4(Uuid::from_u128(0xC)); // P2, active, tests/x.rs → excluded

        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            for (pg, path, chunk_guid, chunk_status) in [
                (p1, "src/a.rs", ga, "active"),
                (p1, "tests/t.rs", gt, "deleted"),
                (p2, "tests/x.rs", gx, "active"),
            ] {
                tx.execute(
                    "INSERT OR IGNORE INTO projects (guid, model_id)
                     VALUES (?1, 'BAAI/bge-m3')",
                    params![pg],
                )?;
                tx.execute(
                    "INSERT INTO project_files
                         (project_guid, model_id, path, sha256, programming_language, status)
                     VALUES (?1, 'BAAI/bge-m3', ?2, ?3, 'rust', 'indexing')",
                    params![pg, path, "0".repeat(64)],
                )?;
                tx.execute(
                    "INSERT INTO project_file_chunks
                         (project_guid, file_path, model_id, code, qdrant_guid,
                          start_line, end_line, start_column, end_column, status)
                     VALUES (?1, ?2, 'BAAI/bge-m3', 'code', ?3, 1, 2, 0, 1, ?4)",
                    params![pg, path, chunk_guid, chunk_status],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

        let (sql, binds) = build_search_query(p1, &req(Some(paths(&["src/**", "tests/**"])), None));
        let got: Vec<UUIDv4> = pool
            .transaction(CancellationToken::new(), move |tx| {
                tx.prepare(&sql)?
                    .query_map(params_from_iter(binds), |r| r.get::<_, UUIDv4>(0))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(SQLite3PoolError::from)
            })
            .await
            .unwrap();

        assert_eq!(
            got,
            vec![ga],
            "candidate set must contain only the project's own active chunks \
             matching the include globs — no soft-deleted or foreign-project chunks"
        );
    }

    // ── sha256 refresh at indexing start (regression, red until fixed) ──────
    // The prepare upsert must refresh sha256 on reindex of an existing row.
    // Otherwise a crash/embed-failure recovered by the retry worker (which marks
    // 'indexed' via set_file_status, never touching sha256) leaves the row with
    // the OLD content hash next to the NEW content's chunks — and a later revert
    // of the file to the old content is hash-skipped forever, serving stale chunks.
    #[tokio::test]
    async fn reindex_upsert_refreshes_sha256_at_indexing_start() {
        const SHA_OLD: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        const SHA_NEW: &str = "2222222222222222222222222222222222222222222222222222222222222222";

        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, 'BAAI/bge-m3')",
                params![guid()],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, 'BAAI/bge-m3', 'src/a.py', ?2, 'python', 'indexing')",
                params![guid(), SHA_OLD],
            )?;
            // indexing → indexed: the state a previously indexed file sits in.
            tx.execute(
                "UPDATE project_files SET status = 'indexed' WHERE project_guid = ?1",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // Reindex with changed content: the exact production prepare upsert.
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                MARK_INDEXING_UPSERT_SQL,
                params![
                    guid(),
                    "src/a.py",
                    SHA_NEW,
                    ProgrammingLanguage::Python,
                    "BAAI/bge-m3",
                    CHUNKS_DERIVATION_VERSION,
                    SYMBOLS_DERIVATION_VERSION
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let (status, sha, cv, sv): (String, String, Option<String>, Option<String>) = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.query_row(
                    "SELECT status, sha256, chunks_version, symbols_version
                       FROM project_files WHERE project_guid = ?1",
                    params![guid()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .map_err(SQLite3PoolError::from)
            })
            .await
            .unwrap();

        assert_eq!(status, "indexing");
        assert_eq!(
            sha, SHA_NEW,
            "the prepare upsert must refresh sha256 on conflict — a retry-worker \
             recovery marks 'indexed' without writing sha256, so a stale stored hash \
             would desync from the freshly inserted chunks"
        );
        assert_eq!(
            (cv.as_deref(), sv.as_deref()),
            (
                Some(CHUNKS_DERIVATION_VERSION),
                Some(SYMBOLS_DERIVATION_VERSION)
            ),
            "the same upsert stamps the derivation versions, in the transaction that \
             produces the chunks and symbols they describe — so a row can never claim \
             a version whose rows were not written"
        );
    }

    #[test]
    fn include_and_exclude_share_one_counter() {
        // include langs ?2; exclude paths ?3 — numbering is global across both.
        let (sql, binds) = build_search_query(
            guid(),
            &req(
                Some(langs(vec![ProgrammingLanguage::Rust])),
                Some(paths(&["target/**"])),
            ),
        );
        assert!(
            sql.contains("f.programming_language IN (?2)"),
            "sql was: {sql}"
        );
        assert!(sql.contains("NOT (c.file_path GLOB ?3)"), "sql was: {sql}");
        assert_eq!(
            binds,
            vec![
                Bind::Guid(guid()),
                Bind::Lang(ProgrammingLanguage::Rust),
                Bind::Path("target/**".into()),
            ]
        );
    }

    // ── drift ────────────────────────────────────────────────────────────────

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect()
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn drift_classifies_all_four_buckets() {
        let indexed = map(&[("same.rs", "h1"), ("changed.rs", "h2"), ("gone.rs", "h3")]);
        let in_flight = set(&["busy.rs"]);
        let local = map(&[
            ("same.rs", "h1"),    // in sync → omitted
            ("changed.rs", "hX"), // hash differs → stale
            ("new.rs", "h9"),     // not indexed → missing
            ("busy.rs", "hY"),    // in flight → indexing
                                  // gone.rs absent locally → orphaned
        ]);

        let d = compute_drift(&indexed, &in_flight, &local);

        assert_eq!(d.stale, vec!["changed.rs"]);
        assert_eq!(d.missing, vec!["new.rs"]);
        assert_eq!(d.orphaned, vec!["gone.rs"]);
        assert_eq!(d.indexing, vec!["busy.rs"]);
    }

    #[test]
    fn drift_empty_baseline_makes_everything_missing() {
        let d = compute_drift(&map(&[]), &set(&[]), &map(&[("a.rs", "h"), ("b.rs", "h")]));
        assert_eq!(d.missing, vec!["a.rs", "b.rs"]);
        assert!(d.stale.is_empty() && d.orphaned.is_empty() && d.indexing.is_empty());
    }

    #[test]
    fn drift_in_flight_never_stale_or_missing_even_when_hash_differs() {
        // An indexing row's stored sha256 is the *old* value, so a hash mismatch on
        // an in-flight file must NOT surface as stale/missing — only `indexing`.
        let indexed = map(&[("f.rs", "old_hash")]);
        let in_flight = set(&["f.rs"]);
        let local = map(&[("f.rs", "new_hash")]);

        let d = compute_drift(&indexed, &in_flight, &local);

        assert_eq!(d.indexing, vec!["f.rs"]);
        assert!(d.stale.is_empty());
        assert!(d.missing.is_empty());
        assert!(
            d.orphaned.is_empty(),
            "in-flight file must not be called orphaned"
        );
    }

    // ── keyed indexing claim ─────────────────────────────────────────────────

    #[test]
    fn index_claim_is_exclusive_and_releases_on_drop() {
        let locks: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let key = "guid\u{0}model\u{0}path".to_string();

        let first = IndexClaim::try_acquire(&locks, key.clone());
        assert!(first.is_some(), "first claim should succeed");

        // A second claim on the same key is refused while the first is held.
        assert!(
            IndexClaim::try_acquire(&locks, key.clone()).is_none(),
            "concurrent claim on the same key must be refused"
        );

        drop(first); // release

        // After release the key is claimable again.
        assert!(
            IndexClaim::try_acquire(&locks, key).is_some(),
            "key should be claimable again after the holder drops"
        );
    }

    #[test]
    fn claim_lifetime_is_bound_to_prepared_for_the_whole_pipeline() {
        // Regression guard for the "stale recover clobbers a later index" race:
        // the per-file claim is owned by `Prepared._claim`, and `post_index` keeps
        // the `Vec<Prepared>` in scope through embed_all AND mark_indexed/recover_all.
        // So a request that goes on to *fail* still holds the lock while it recovers
        // to `failed`; no second request for the same file can start (the contended
        // file is skipped) until the first fully terminates. This is what makes the interleaving
        // "req1 releases → req3 reindexes → req1's late `failed` lands" impossible:
        // req1 never releases mid-pipeline.
        let locks: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let key = indexing_lock_key("guid", "model", "f.rs");

        let claim = IndexClaim::try_acquire(&locks, key.clone()).expect("first acquires");
        let prepared = Prepared {
            pl: ProgrammingLanguage::Rust,
            path: "f.rs".to_string(),
            sha256: "h".to_string(),
            chunks: Vec::new(),
            symbols: 0,
            _claim: claim,
        };

        // While the first request's `Prepared` is alive — anywhere from slice through
        // embed through recover — every other same-file request is refused.
        assert!(
            IndexClaim::try_acquire(&locks, key.clone()).is_none(),
            "same-file claim must be refused for the whole pipeline, not just until slicing"
        );

        // Only when the pipeline ends (the `Prepared`, hence the claim, drops) does
        // the slot free up — at which point any next request sees the terminal state.
        drop(prepared);
        assert!(
            IndexClaim::try_acquire(&locks, key).is_some(),
            "slot must free up once the holding Prepared drops at end of post_index"
        );
    }

    /// The handler and the retry worker share **one** lock table, so a claim only
    /// works if they spell the key identically — and they build it from different
    /// sources. `post_index` converts a `UUIDv4` with `.as_simple()`; the worker uses
    /// the raw `project_guid` column its sweep read back. The two agree only because
    /// `UUIDv4`'s `ToSql` writes the 32-char hyphen-less form.
    ///
    /// A divergence here is silent in the worst way: the keys never collide, both
    /// claims succeed, and a live `/index` and the retry worker index the same file at
    /// once — the second `prepare` marks the first's fresh chunks `deleted`, the first
    /// embeds orphans, and `sha256` ends up describing a chunk set that is not the
    /// active one. No error is raised anywhere; the symptom is a file that hash-skips
    /// for ever while search cannot find it.
    #[tokio::test]
    async fn the_handler_and_the_retry_worker_spell_the_same_lock_key() {
        let guid = UUIDv4(Uuid::new_v4());
        const MODEL_ID: &str = "BAAI/bge-m3";
        const PATH: &str = "src/a.rs";

        // What the worker gets: whatever SQLite hands back for a stored `UUIDv4`.
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                rusqlite::params![guid, MODEL_ID],
            )?;
            Ok(())
        })
        .await
        .expect("migrations and seed");

        let from_db: String = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.query_row("SELECT guid FROM projects", [], |r| r.get(0))
                    .map_err(SQLite3PoolError::from)
            })
            .await
            .expect("read the guid back");

        let handler_key = indexing_lock_key(&guid.0.as_simple().to_string(), MODEL_ID, PATH);
        let worker_key = indexing_lock_key(&from_db, MODEL_ID, PATH);

        assert_eq!(
            handler_key, worker_key,
            "the handler and the retry worker are claiming different keys for the same \
             file, so neither can ever see the other's claim"
        );

        // And prove it in the table itself: one holder, the other refused.
        let locks: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let _held = IndexClaim::try_acquire(&locks, handler_key).expect("the handler claims it");
        assert!(
            IndexClaim::try_acquire(&locks, worker_key).is_none(),
            "the retry worker was allowed to index a file a live request holds"
        );
    }

    /// The NUL separators are what make the three components unambiguous: no guid,
    /// model id or path can contain one, so two different files can never collide on
    /// one key and one file can never produce two. A separator that *can* appear in a
    /// component (a `/`, say) would let `a/b` + `c` and `a` + `b/c` share a claim.
    #[test]
    fn distinct_files_never_share_a_lock_key() {
        let keys = [
            indexing_lock_key("g", "m", "a/b.rs"),
            indexing_lock_key("g", "m", "a/c.rs"),
            indexing_lock_key("g", "m2", "a/b.rs"),
            indexing_lock_key("g2", "m", "a/b.rs"),
            // The classic ambiguity: components that would run together under a
            // separator any of them could contain.
            indexing_lock_key("g", "m/x", "b.rs"),
            indexing_lock_key("g", "m", "x/b.rs"),
        ];
        let unique: HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "two distinct files share a claim");

        // ...and the same file always produces the same key.
        assert_eq!(
            indexing_lock_key("g", "m", "a/b.rs"),
            indexing_lock_key("g", "m", "a/b.rs")
        );
    }

    /// Under real contention exactly one caller may hold a file's slot, and the slot
    /// must be free again once every holder has let go — a claim leaked by a panicking
    /// or forgotten path would make that file permanently un-indexable, with `/index`
    /// answering 200 and the file never moving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_one_of_many_racing_claimants_wins_and_the_slot_frees_afterwards() {
        let locks: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let key = indexing_lock_key("g", "m", "hot.rs");
        let winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let (locks, key) = (Arc::clone(&locks), key.clone());
            let (winners, gate) = (Arc::clone(&winners), Arc::clone(&gate));
            tasks.push(tokio::spawn(async move {
                if let Some(_claim) = IndexClaim::try_acquire(&locks, key) {
                    winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Hold it until everyone has had their turn to try.
                    gate.notified().await;
                }
            }));
        }

        // Let the losers finish, then release the winner.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            winners.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "more than one caller holds the same file's indexing claim"
        );
        gate.notify_waiters();
        for t in tasks {
            t.await.expect("claimant finishes");
        }

        assert!(
            IndexClaim::try_acquire(&locks, key).is_some(),
            "the slot was never released — this file can no longer be indexed at all"
        );
        assert!(
            locks.lock().unwrap().is_empty(),
            "the lock table leaked a key; it grows without bound over the process's life"
        );
    }

    // ── retry requeue ────────────────────────────────────────────────────────

    /// `post_retry`'s UPDATE resets `retry_count` on a `failed` file and — critically
    /// — leaves `status_updated_at` untouched. The retry worker only picks a `failed`
    /// file whose timestamp is older than 60s, so preserving the old stamp lets the
    /// next sweep re-embed it immediately instead of after another grace window.
    #[tokio::test]
    async fn retry_resets_count_and_preserves_timestamp() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![guid(), "BAAI/bge-m3"],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, 'a.rs', ?3, 'rust', 'indexing')",
                params![guid(), "BAAI/bge-m3", "0".repeat(64)],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // Reach `failed` legally (indexing → failed), then pin a maxed retry_count and
        // an old status_updated_at directly — both are plain column writes, not status
        // transitions, so no trigger fires.
        let pg = guid().0.as_simple().to_string();
        let _ = set_file_status(
            &pool,
            &pg,
            "a.rs",
            "BAAI/bge-m3",
            "failed",
            true,
            CancellationToken::new(),
        )
        .await;
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                "UPDATE project_files SET retry_count = 9, status_updated_at = 1000
                 WHERE project_guid = ?1 AND path = 'a.rs'",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // Run exactly what `post_retry` runs: an empty selector (requeue all) plus the
        // constant `status = 'failed'`.
        let (where_sql, binds) = build_file_filter(guid(), &None, &None);
        let update_sql = format!(
            "UPDATE project_files SET retry_count = 0 WHERE {where_sql} AND status = 'failed'"
        );
        let n = pool
            .transaction(CancellationToken::new(), move |tx| {
                Ok(tx.execute(&update_sql, params_from_iter(binds.iter()))?)
            })
            .await
            .unwrap();
        assert_eq!(n, 1, "the failed file should be requeued");

        let (status, retry_count, updated): (String, i64, i64) = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.query_row(
                    "SELECT status, retry_count, status_updated_at FROM project_files
                     WHERE project_guid = ?1 AND path = 'a.rs'",
                    params![guid()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(SQLite3PoolError::from)
            })
            .await
            .unwrap();

        assert_eq!(
            status, "failed",
            "metadata-only write: status must stay 'failed'"
        );
        assert_eq!(
            retry_count, 0,
            "retry_count must be reset so the worker re-picks it"
        );
        assert_eq!(
            updated, 1000,
            "status_updated_at must NOT be bumped (else a +60s delay)"
        );
    }

    // ── phase-1 → phase-2 reconciliation (`drop_cancelled`) and batch recovery
    //    (`recover_all`) ─────────────────────────────────────────────────────────
    // These are the correctness core of a concurrent `POST /cancel` against a live
    // `/index`: a file flipped out of `indexing` between prepare and embed must be
    // dropped from the batch (its fresh chunks soft-deleted for GC), and an aborted
    // batch must hand every prepared file to the retry worker — none may stay
    // `indexing` with no one working on it.

    use crate::db::files::set_file_status;
    use crate::db::qdrant::{ChunkAsVector, SearchHit, VectorStoreError};
    use crate::models::bge_m3::{BGEm3EmbedRequest, BGEm3EmbedResponse, EncodeError};
    use async_trait::async_trait;
    use sha2::Digest;
    use std::collections::HashSet;
    use std::sync::Mutex;

    const MODEL: &str = "BAAI/bge-m3";

    /// Neither seam may be touched by `drop_cancelled`/`recover_all` — they are
    /// SQLite-only paths. Any call is a test failure.
    struct NoStore;
    #[async_trait]
    impl VectorStore for NoStore {
        async fn insert_batch(
            &self,
            _c: &str,
            _v: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            unreachable!("drop_cancelled/recover_all must not touch Qdrant")
        }
        async fn ensure_project(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_collection(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn health(&self) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_batch(&self, _c: &str, _g: Vec<String>) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn search(
            &self,
            _c: &str,
            _i: Vec<UUIDv4>,
            _d: Vec<f32>,
            _si: Vec<u32>,
            _sv: Vec<f32>,
            _cb: Vec<Vec<f32>>,
            _k: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            unreachable!()
        }
    }

    struct NoEmbedder;
    #[async_trait]
    impl crate::models::bge_m3::BGEm3Model for NoEmbedder {
        async fn encode(
            &self,
            _req: BGEm3EmbedRequest,
            _token: CancellationToken,
        ) -> Result<BGEm3EmbedResponse, EncodeError> {
            unreachable!("drop_cancelled/recover_all must not call the embedder")
        }
        async fn health(&self) -> Result<(), EncodeError> {
            unreachable!()
        }
    }

    /// Migrated pool with the project and `paths` each inserted `indexing` with
    /// `n_chunks` active chunks — the exact state `prepare` leaves behind.
    async fn pool_with_prepared_files(
        paths: &'static [&'static str],
        n_chunks: usize,
    ) -> SQLite3Pool {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![guid(), MODEL],
            )?;
            for path in paths {
                tx.execute(
                    "INSERT INTO project_files
                         (project_guid, model_id, path, sha256, programming_language, status)
                     VALUES (?1, ?2, ?3, ?4, 'rust', 'indexing')",
                    params![guid(), MODEL, path, "0".repeat(64)],
                )?;
                for _ in 0..n_chunks {
                    tx.execute(
                        "INSERT INTO project_file_chunks
                             (project_guid, file_path, model_id, code, qdrant_guid,
                              start_line, end_line, start_column, end_column, status)
                         VALUES (?1, ?2, ?3, 'code', ?4, 1, 2, 0, 1, 'active')",
                        params![guid(), path, MODEL, Uuid::new_v4().simple().to_string()],
                    )?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
        pool
    }

    /// Builds a `Prepared` for `path` the way `prepare` would: claim held, chunks
    /// carried. The chunk list content is irrelevant to the paths under test.
    fn prepared_for(locks: &Arc<Mutex<HashSet<String>>>, path: &str) -> Prepared {
        let key = indexing_lock_key(&guid().0.as_simple().to_string(), MODEL, path);
        Prepared {
            pl: ProgrammingLanguage::Rust,
            path: path.to_string(),
            sha256: "0".repeat(64),
            chunks: vec![(UUIDv4(Uuid::new_v4()), "code".to_string())],
            symbols: 0,
            _claim: IndexClaim::try_acquire(locks, key).expect("slot starts free"),
        }
    }

    /// (status, active_chunks, deleted_chunks) for one path.
    async fn file_state(pool: &SQLite3Pool, path: &'static str) -> (String, i64, i64) {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.query_row(
                "SELECT f.status,
                        (SELECT COUNT(*) FROM project_file_chunks c
                         WHERE c.project_guid = f.project_guid AND c.file_path = f.path
                           AND c.model_id = f.model_id AND c.status = 'active'),
                        (SELECT COUNT(*) FROM project_file_chunks c
                         WHERE c.project_guid = f.project_guid AND c.file_path = f.path
                           AND c.model_id = f.model_id AND c.status = 'deleted')
                 FROM project_files f
                 WHERE f.project_guid = ?1 AND f.path = ?2 AND f.model_id = ?3",
                params![guid(), path, MODEL],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    /// An `OllamaModel` that refuses every call — the SQL-only cores never chat.
    struct NoOllama;
    #[async_trait]
    impl crate::models::ollama::OllamaModel for NoOllama {
        async fn chat_stream(
            &self,
            _model: &str,
            _messages: &[crate::models::ollama::ChatMessage],
            _tools: &[crate::models::ollama::ToolSpec],
            _sampling: crate::models::ollama::Sampling,
            _on_delta: &mut (dyn FnMut(crate::models::ollama::ChatDelta) + Send),
            _token: &CancellationToken,
        ) -> Result<crate::models::ollama::ChatOutcome, crate::models::ollama::OllamaError>
        {
            unreachable!("a SQL-only core must not reach Ollama")
        }
        async fn list_models(&self) -> Result<Vec<String>, crate::models::ollama::OllamaError> {
            Ok(vec![])
        }
    }

    /// A `RouterState` wired to the given pool and to fakes that refuse every
    /// network call.
    ///
    /// The `*_core` functions — `grep`, `read_chunks`, `outline`, `list_files`,
    /// `symbols` — are pure SQL and are what both the research tool loop
    /// and several public endpoints run. They took a `&RouterState`, which is only
    /// obtainable from a fully wired server, so their real queries were reachable in
    /// tests **only through the research fakes that replace them**: the scope
    /// subquery, the `status = 'active'` filter, the two-read "no such file" vs "no
    /// rows in range" distinction and the out-of-scope counters were all exercised
    /// by nothing.
    ///
    /// Every scalar comes from `Config::default()` rather than a number chosen here,
    /// so a default that changes is seen by these tests instead of being shadowed.
    fn router_state(pool: SQLite3Pool) -> RouterState {
        let cfg = crate::config::Config::default();
        // The trivial tokenizer from `fixture()`: nothing here tokenizes, but the
        // field is not optional.
        let word_level = tokenizers::models::wordlevel::WordLevel::builder()
            .vocab([("[UNK]".to_string(), 0u32)].into_iter().collect())
            .unk_token("[UNK]".to_string())
            .build()
            .expect("static vocab");
        let embedder: Arc<dyn crate::models::bge_m3::BGEm3Model> = Arc::new(NoEmbedder);

        RouterState {
            tokenizer: Arc::new(Tokenizer::new(word_level)),
            db_pool: Arc::new(pool),
            qdrant: Arc::new(NoStore),
            model: EmbeddingModel::BGEm3 {
                model_id: MODEL.to_string(),
                client: Arc::clone(&embedder),
            },
            query_model: embedder,
            embed_tuning: crate::embed::EmbedTuning {
                embed_batch: cfg.indexing.embed_batch_chunks,
                upsert_batch: cfg.qdrant.upsert_batch_points,
                sparse_min_weight: cfg.indexing.sparse_min_weight,
            },
            min_chunk_tokens: cfg.slicer.min_chunk_tokens,
            max_chunk_tokens: cfg.slicer.max_chunk_tokens,
            fill_gaps: cfg.slicer.fill_gaps,
            max_doc_chunk_tokens: cfg.slicer.max_doc_chunk_tokens,
            doc_semantic_weight: cfg.slicer.doc_semantic_weight,
            default_top_k: cfg.search.default_top_k,
            max_top_k: cfg.search.max_top_k,
            max_query_bytes: cfg.search.max_query_bytes,
            max_code_bytes: cfg.limits.max_code_bytes,
            max_files_per_request: cfg.limits.max_files_per_request,
            max_drift_files: cfg.limits.max_drift_files,
            max_selector_patterns: cfg.limits.max_selector_patterns,
            max_symbol_name_bytes: cfg.limits.max_symbol_name_bytes,
            max_symbol_results: cfg.limits.max_symbol_results,
            max_history_commits: cfg.limits.max_history_commits,
            max_commit_message_bytes: cfg.limits.max_commit_message_bytes,
            max_research_delete_ids: cfg.limits.max_research_delete_ids,
            path_batch_size: cfg.indexing.path_batch_size,
            status_log_retention_days: cfg.workers.status_log_retention_days,
            max_retries: cfg.workers.max_retries,
            indexing_locks: Arc::new(Mutex::new(HashSet::new())),
            gc_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stuck_grace_mins: cfg.indexing.stuck_grace_minutes,
            db_pool_size: cfg.database.pool_size,
            db_schema_version: 0,
            research_handle: tokio::runtime::Handle::current(),
            research_semaphore: Arc::new(tokio::sync::Semaphore::new(cfg.research.max_concurrent)),
            research_max_concurrent: cfg.research.max_concurrent,
            research_registry: crate::backend::inflight::ResearchRegistry::new(),
            research_stats: Arc::new(tokio::sync::RwLock::new(Default::default())),
            research_ollama: Arc::new(NoOllama),
            research_default_model: cfg.research.default_model.clone(),
            research_allowed_models: crate::config::AllowedModels::compile(&[])
                .expect("an empty whitelist compiles"),
            research_effort: cfg.research.effort.clone(),
            research_max_request_seconds: cfg.research.max_request_seconds,
            research_max_request_tokens: cfg.research.max_request_tokens,
            research_max_request_steps: cfg.research.max_request_steps,
            research_max_request_report_sections: cfg.research.max_request_report_sections,
            research_max_request_report_words: cfg.research.max_request_report_words,
            research_max_evidence_width: cfg.research.max_evidence_width,
            research_report_timeout_ms: cfg.research.report_timeout_ms,
            research_checkpoint_every_steps: cfg.research.checkpoint_every_steps,
            research_max_turn_thinking_chars: cfg.research.max_turn_thinking_chars,
            research_max_turn_seconds: cfg.research.max_turn_seconds,
            research_retention_days: cfg.research.retention_days,
            research_max_context_runs: cfg.research.max_context_runs,
            research_max_context_chars: cfg.research.max_context_chars,
            research_list_page_limit: cfg.research.list_page_limit,
            research_sampling: crate::models::ollama::Sampling {
                temperature: cfg.research.temperature,
                top_p: cfg.research.top_p,
                seed: cfg.research.seed,
                num_predict: None,
            },
            research_models: Arc::new(tokio::sync::RwLock::new(Default::default())),
            metrics: Arc::new(crate::backend::metrics::Metrics::new()),
            // Authorization off, which is what makes every test in this module a
            // test of the handler rather than of the extractor in front of it.
            // The tests that *do* exercise authorization build their own state
            // with `router_state_with_auth`.
            auth: None,
        }
    }

    /// The owned pieces a test-local `FileIndexer` borrows (kept alive by the caller).
    struct IndexerFixture {
        tokenizer: Arc<Tokenizer>,
        token: CancellationToken,
    }

    fn fixture() -> IndexerFixture {
        // A functional-but-trivial tokenizer: everything maps to [UNK], so encode
        // succeeds (prepare needs that) while yielding far fewer than 128 tokens —
        // files slice to no chunks, which the paths under test don't care about.
        let word_level = tokenizers::models::wordlevel::WordLevel::builder()
            .vocab([("[UNK]".to_string(), 0u32)].into_iter().collect())
            .unk_token("[UNK]".to_string())
            .build()
            .expect("static vocab");
        IndexerFixture {
            tokenizer: Arc::new(Tokenizer::new(word_level)),
            token: CancellationToken::new(),
        }
    }

    /// A `FileIndexer` wired to fakes that reject any Qdrant/embedder call (the
    /// paths under test are SQLite-only).
    fn indexer<'a>(
        pool: &'a SQLite3Pool,
        locks: &'a Arc<Mutex<HashSet<String>>>,
        fx: &'a IndexerFixture,
    ) -> FileIndexer<'a> {
        FileIndexer {
            db_pool: pool,
            store: &NoStore,
            tokenizer: &fx.tokenizer,
            embedder: &NoEmbedder,
            model_id: MODEL,
            project_guid: guid(),
            collection: "unused",
            embed_tuning: crate::embed::EmbedTuning {
                embed_batch: 64,
                upsert_batch: 256,
                sparse_min_weight: 1e-5,
            },
            min_chunk_tokens: 128,
            max_chunk_tokens: 512,
            fill_gaps: true,
            max_doc_chunk_tokens: 1024,
            doc_semantic_weight: 1.0,
            token: &fx.token,
            indexing_locks: locks,
            force: false,
        }
    }

    /// One `indexed` Rust file whose stored hash matches `code`, with a derivation
    /// row at `symbols_version` (`None` = no derivation row at all).
    async fn pool_with_indexed_file(
        path: &'static str,
        code: &'static str,
        symbols_version: Option<&'static str>,
    ) -> SQLite3Pool {
        let mut hasher = Sha256::default();
        sha2::Digest::update(&mut hasher, code.as_bytes());
        let sha = hex::encode(hasher.finalize_fixed_reset());

        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![guid(), MODEL],
            )?;
            // A row may only enter as `just_uploaded`/`indexing` (status-machine
            // trigger), so
            // reach `indexed` the legal way rather than inserting it directly.
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, ?3, ?4, 'rust', 'indexing')",
                params![guid(), MODEL, path, sha],
            )?;
            tx.execute(
                "UPDATE project_files SET status = 'indexed'
                 WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3",
                params![guid(), MODEL, path],
            )?;
            // `None` leaves both columns NULL — the unversioned shape.
            if let Some(v) = symbols_version {
                tx.execute(
                    "UPDATE project_files
                        SET chunks_version = ?4, symbols_version = ?5
                      WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3",
                    params![guid(), MODEL, path, CHUNKS_DERIVATION_VERSION, v],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        pool
    }

    async fn symbol_count(pool: &SQLite3Pool) -> i64 {
        pool.transaction(CancellationToken::new(), |tx| {
            Ok(
                tx.query_row("SELECT COUNT(*) FROM project_file_symbols", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap()
    }

    const RUST_SRC: &str = "pub fn alpha() {}\npub fn beta() { alpha(); }\n";

    #[tokio::test]
    async fn rebuild_symbols_rewrites_a_file_at_a_stale_version() {
        let pool = pool_with_indexed_file("a.rs", RUST_SRC, Some("0.9")).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();
        let ix = indexer(&pool, &locks, &fx);

        let n = ix
            .rebuild_symbols(
                ProgrammingLanguage::Rust,
                "a.rs",
                RUST_SRC,
                &mut Sha256::default(),
            )
            .await
            .unwrap();

        assert!(n.is_some_and(|n| n > 0), "symbols must be written");
        assert!(symbol_count(&pool).await > 0);

        // The version is restamped, so a second pass is a no-op — the point of the
        // whole mechanism is that it converges rather than rebuilding every run.
        let again = ix
            .rebuild_symbols(
                ProgrammingLanguage::Rust,
                "a.rs",
                RUST_SRC,
                &mut Sha256::default(),
            )
            .await
            .unwrap();
        assert_eq!(again, None, "an up-to-date file must be skipped");
    }

    #[tokio::test]
    async fn rebuild_symbols_skips_a_file_whose_content_moved_on() {
        // Its chunks are stale too; symbols describing newer text than the chunks
        // beside them would break the "symbols parallel chunks" invariant.
        let pool = pool_with_indexed_file("a.rs", RUST_SRC, Some("0.9")).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();

        let n = indexer(&pool, &locks, &fx)
            .rebuild_symbols(
                ProgrammingLanguage::Rust,
                "a.rs",
                "pub fn gamma() {}\n",
                &mut Sha256::default(),
            )
            .await
            .unwrap();

        assert_eq!(n, None);
        assert_eq!(symbol_count(&pool).await, 0, "nothing may be written");
    }

    #[tokio::test]
    async fn rebuild_symbols_backfills_a_file_with_no_derivation_row() {
        // The unversioned shape: indexed, hash matches, both versions NULL.
        let pool = pool_with_indexed_file("a.rs", RUST_SRC, None).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();

        let n = indexer(&pool, &locks, &fx)
            .rebuild_symbols(
                ProgrammingLanguage::Rust,
                "a.rs",
                RUST_SRC,
                &mut Sha256::default(),
            )
            .await
            .unwrap();

        assert!(n.is_some_and(|n| n > 0));
        // `chunks_version` must be stamped too, even though this pass re-derived
        // only symbols: the hash matched, so the chunks already in the table are
        // the ones the current slicer produces. Leaving it NULL would send the
        // next ordinary run through a full re-embed for no reason — and that is
        // invisible unless the column is asserted, because the symbol rows are
        // correct either way.
        let versions: (Option<String>, Option<String>) = pool
            .transaction(CancellationToken::new(), |tx| {
                Ok(tx.query_row(
                    "SELECT chunks_version, symbols_version FROM project_files",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            (versions.0.as_deref(), versions.1.as_deref()),
            (
                Some(CHUNKS_DERIVATION_VERSION),
                Some(SYMBOLS_DERIVATION_VERSION)
            )
        );
    }

    #[tokio::test]
    async fn force_rebuilds_symbols_already_at_the_current_version() {
        let pool = pool_with_indexed_file("a.rs", RUST_SRC, Some(SYMBOLS_DERIVATION_VERSION)).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();

        let skipped = indexer(&pool, &locks, &fx)
            .rebuild_symbols(
                ProgrammingLanguage::Rust,
                "a.rs",
                RUST_SRC,
                &mut Sha256::default(),
            )
            .await
            .unwrap();
        assert_eq!(skipped, None, "current version is skipped without force");

        let forced = FileIndexer {
            force: true,
            ..indexer(&pool, &locks, &fx)
        }
        .rebuild_symbols(
            ProgrammingLanguage::Rust,
            "a.rs",
            RUST_SRC,
            &mut Sha256::default(),
        )
        .await
        .unwrap();
        assert!(
            forced.is_some_and(|n| n > 0),
            "force must override the skip"
        );
    }

    #[tokio::test]
    async fn drop_cancelled_keeps_files_still_indexing() {
        let pool = pool_with_prepared_files(&["a.rs", "b.rs"], 2).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "a.rs"), prepared_for(&locks, "b.rs")];

        let fx = fixture();
        let kept = indexer(&pool, &locks, &fx).drop_cancelled(prepared).await;
        let mut paths: Vec<_> = kept.iter().map(|p| p.path.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec!["a.rs", "b.rs"],
            "untouched files must all survive"
        );
        drop(kept); // release the claims before inspecting state

        // No collateral damage: both files still 'indexing', chunks still active.
        assert_eq!(
            file_state(&pool, "a.rs").await,
            ("indexing".to_string(), 2, 0)
        );
        assert_eq!(
            file_state(&pool, "b.rs").await,
            ("indexing".to_string(), 2, 0)
        );
    }

    #[tokio::test]
    async fn drop_cancelled_drops_flipped_files_and_soft_deletes_their_chunks() {
        let pool = pool_with_prepared_files(&["a.rs", "b.rs"], 2).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "a.rs"), prepared_for(&locks, "b.rs")];

        // A concurrent POST /cancel lands between prepare and embed: indexing → cancelled.
        let pg = guid().0.as_simple().to_string();
        let _ = set_file_status(
            &pool,
            &pg,
            "a.rs",
            MODEL,
            "cancelled",
            false,
            CancellationToken::new(),
        )
        .await;

        let fx = fixture();
        let kept = indexer(&pool, &locks, &fx).drop_cancelled(prepared).await;
        let paths: Vec<_> = kept.iter().map(|p| p.path.clone()).collect();
        assert_eq!(
            paths,
            vec!["b.rs"],
            "the cancelled file must be dropped from the batch"
        );

        // The cancelled file's just-inserted chunks are handed to GC; the survivor
        // is untouched.
        assert_eq!(
            file_state(&pool, "a.rs").await,
            ("cancelled".to_string(), 0, 2)
        );
        assert_eq!(
            file_state(&pool, "b.rs").await,
            ("indexing".to_string(), 2, 0)
        );
    }

    #[tokio::test]
    async fn recover_all_hands_every_prepared_file_to_the_retry_worker() {
        // The shared-embed-failure path: every prepared file goes indexing → failed
        // with its retry budget burned by one.
        let pool = pool_with_prepared_files(&["a.rs", "b.rs"], 1).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "a.rs"), prepared_for(&locks, "b.rs")];

        let fx = fixture();
        indexer(&pool, &locks, &fx)
            .recover_all(&prepared, "failed", true)
            .await;

        for path in ["a.rs", "b.rs"] {
            let (status, _, _) = file_state(&pool, path).await;
            assert_eq!(
                status, "failed",
                "{path} must not be left 'indexing' after an aborted batch"
            );
        }

        // The client-cancelled path: cancelled, without burning retry budget.
        let pool = pool_with_prepared_files(&["c.rs"], 1).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "c.rs")];
        let fx = fixture();
        indexer(&pool, &locks, &fx)
            .recover_all(&prepared, "cancelled", false)
            .await;
        assert_eq!(file_state(&pool, "c.rs").await.0, "cancelled");
    }

    /// The test above passes a *fresh* token, and for its whole life that is what made
    /// it green: `recover` used `self.token.child_token()`, and `SQLite3Pool::run`
    /// short-circuits on a cancelled token before touching the database. So on the one
    /// path this mechanism exists for — the client disconnected, cancelling the
    /// request token — recovery wrote nothing at all, and every prepared file stayed
    /// `indexing` until the 30-minute stuck-grace sweep. The token being cancelled is
    /// the case, not an edge of it.
    #[tokio::test]
    async fn recovery_still_writes_when_the_requests_token_is_already_cancelled() {
        let pool = pool_with_prepared_files(&["d.rs", "e.rs"], 1).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "d.rs"), prepared_for(&locks, "e.rs")];
        let fx = fixture();
        fx.token.cancel();

        indexer(&pool, &locks, &fx)
            .recover_all(&prepared, "cancelled", false)
            .await;

        for path in ["d.rs", "e.rs"] {
            assert_eq!(
                file_state(&pool, path).await.0,
                "cancelled",
                "{path} must be handed to the retry worker even though the request \
                 token is cancelled — that is when recovery is needed"
            );
        }
    }

    // ── symbol lifecycle: a file's symbol rows always parallel its chunk set ──

    async fn insert_symbol(pool: &SQLite3Pool, path: &'static str, name: &'static str) {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO project_file_symbols
                     (project_guid, model_id, file_path, name, kind,
                      start_line, end_line, start_column, end_column)
                 VALUES (?1, ?2, ?3, ?4, 'function', 1, 1, 0, 1)",
                params![guid(), MODEL, path, name],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    async fn symbol_names(pool: &SQLite3Pool, path: &'static str) -> Vec<String> {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.prepare(
                "SELECT name FROM project_file_symbols
                 WHERE project_guid = ?1 AND model_id = ?2 AND file_path = ?3
                 ORDER BY name",
            )?
            .query_map(params![guid(), MODEL, path], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn prepare_replaces_a_files_symbols() {
        // The fixture tokenizer yields no tokens, so slicing produces 0 chunks —
        // irrelevant here: symbol extraction must still run and replace the old rows.
        let pool = pool_with_prepared_files(&["a.rs"], 1).await;
        insert_symbol(&pool, "a.rs", "stale_symbol").await;

        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();
        let mut hasher = Sha256::new();
        let prepared = indexer(&pool, &locks, &fx)
            .prepare(
                ProgrammingLanguage::Rust,
                "a.rs",
                "fn fresh() { helper(); }",
                &mut hasher,
            )
            .await
            .unwrap();
        assert!(prepared.is_some(), "changed content must not be skipped");
        drop(prepared);

        let names = symbol_names(&pool, "a.rs").await;
        assert!(
            !names.iter().any(|n| n == "stale_symbol"),
            "old symbols must be hard-deleted on reindex"
        );
        assert_eq!(
            names,
            vec!["fresh"],
            "the new definition must be inserted — and `helper()`, a call, must not: \
             references are no longer extracted"
        );
    }

    #[tokio::test]
    async fn drop_cancelled_deletes_the_flipped_files_symbols() {
        let pool = pool_with_prepared_files(&["a.rs", "b.rs"], 1).await;
        insert_symbol(&pool, "a.rs", "gone").await;
        insert_symbol(&pool, "b.rs", "kept").await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let prepared = vec![prepared_for(&locks, "a.rs"), prepared_for(&locks, "b.rs")];

        let pg = guid().0.as_simple().to_string();
        let _ = set_file_status(
            &pool,
            &pg,
            "a.rs",
            MODEL,
            "cancelled",
            false,
            CancellationToken::new(),
        )
        .await;

        let fx = fixture();
        let kept = indexer(&pool, &locks, &fx).drop_cancelled(prepared).await;
        drop(kept);

        assert!(
            symbol_names(&pool, "a.rs").await.is_empty(),
            "a cancelled file's symbols must go with its chunks"
        );
        assert_eq!(
            symbol_names(&pool, "b.rs").await,
            vec!["kept"],
            "the surviving file's symbols are untouched"
        );
    }

    #[tokio::test]
    async fn symbols_block_file_prune_until_deleted() {
        // FK RESTRICT is the ordering guard: a project_files row cannot be pruned
        // while symbol rows still reference it — the soft-delete paths must have
        // removed them first (they do; this pins the schema-level backstop).
        let pool = pool_with_prepared_files(&["a.rs"], 0).await;
        insert_symbol(&pool, "a.rs", "s").await;
        let pg = guid().0.as_simple().to_string();
        let _ = set_file_status(
            &pool,
            &pg,
            "a.rs",
            MODEL,
            "deleted",
            false,
            CancellationToken::new(),
        )
        .await;

        let blocked = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.execute("DELETE FROM project_files WHERE path = 'a.rs'", [])?;
                Ok(())
            })
            .await;
        assert!(
            blocked.is_err(),
            "FK RESTRICT must reject pruning a file that still has symbols"
        );

        let pruned = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.execute(
                    "DELETE FROM project_file_symbols WHERE file_path = 'a.rs'",
                    [],
                )?;
                tx.execute("DELETE FROM project_files WHERE path = 'a.rs'", [])?;
                Ok(())
            })
            .await;
        assert!(pruned.is_ok(), "with symbols gone the prune must succeed");
    }

    // ── /symbols lookup: ranking + totals (the exact production SQL) ──────────

    /// Executes `build_symbols_query`, returning (paths, total).
    async fn run_symbols_query(
        pool: &SQLite3Pool,
        req: SymbolsRequest,
        limit: usize,
    ) -> (Vec<String>, u64) {
        let (sql, binds) = build_symbols_query(guid(), MODEL, &req, limit);
        pool.transaction(CancellationToken::new(), move |tx| {
            let mut total = 0u64;
            let paths = tx
                .prepare(&sql)?
                .query_map(params_from_iter(binds.iter()), |r| {
                    total = r.get::<_, i64>(9)? as u64;
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((paths, total))
        })
        .await
        .unwrap()
    }

    fn symbols_req(name: &str, anchor: Option<&str>) -> SymbolsRequest {
        SymbolsRequest {
            include: None,
            exclude: None,
            name: name.to_string(),
            kind: None,
            anchor_path: anchor.map(str::to_string),
            limit: None,
        }
    }

    /// The trap `grep` would otherwise carry into this codebase specifically: `_` is
    /// a `LIKE` wildcard and every identifier here is full of it, so an unescaped
    /// `read_chunks` also matches `readXchunks` — a wrong answer that looks right.
    #[test]
    fn a_grep_pattern_escapes_the_like_wildcards_it_contains() {
        assert_eq!(like_escape("read_chunks"), r"read\_chunks");
        assert_eq!(like_escape("100%"), r"100\%");
        // The backslash goes first, or the escapes added after it would be escaped in
        // turn and stop escaping anything.
        assert_eq!(like_escape(r"a\_b"), r"a\\\_b");
        assert_eq!(like_escape("plain"), "plain");
    }

    /// The line reported must be the line the literal is on, not the chunk's first —
    /// a citation to the chunk start would send the reader to the wrong place.
    #[test]
    fn a_grep_match_names_the_line_the_literal_is_on() {
        let code = "fn collect() {\n    let guard = GcGuard::new();\n    sweep();\n}";
        let (line, excerpt) = locate_match(code, "GcGuard", 10);
        assert_eq!(line, 11, "the second line of a chunk starting at 10");
        assert_eq!(excerpt, "let guard = GcGuard::new();");
        // Case-insensitive, as the tool description says.
        assert_eq!(locate_match(code, "gcguard", 10).0, 11);
    }

    /// Lowercasing is not length-preserving, so an offset found in the lowered copy is
    /// not an index into the original. Slicing the original with it panicked — and the
    /// panic happens inside `spawn_blocking`, where it costs a pool connection and
    /// reaches the caller as a 499 blaming them for a disconnect they never made. Any
    /// indexed file containing `İ` (Turkish, and ordinary in prose) made `grep` a way
    /// to take the pool apart four requests at a time.
    #[test]
    fn a_grep_match_survives_a_pattern_preceded_by_a_growing_character() {
        // Eight `İ` are 16 bytes and lowercase to 24, so the offset of the match in the
        // lowered string is past the end of the original.
        let code = "İİİİİİİİ\nlet x = 1;";
        assert!("İ".to_lowercase().len() > "İ".len(), "the premise");
        let (line, excerpt) = locate_match(code, "let x", 10);
        assert_eq!(line, 11, "the match is on the chunk's second line");
        assert_eq!(excerpt, "let x = 1;");
        // The same growth landing mid-character rather than out of bounds.
        let code = "İ İ let y = 2;";
        assert_eq!(locate_match(code, "let y", 1), (1, "İ İ let y = 2;".into()));
    }

    /// The scope must arrive as a subquery appended after the query's own binds —
    /// `build_file_filter` emits unqualified column names, so a join would be
    /// ambiguous, and binds landing anywhere but last would renumber the rest.
    #[test]
    fn build_symbols_query_scopes_rows_with_a_subquery_bound_last() {
        let mut req = symbols_req("collect", Some("src/db/qdrant.rs"));
        req.include = Some(SearchFilter {
            paths: Some(vec![crate::backend::v0::models::GlobPattern(
                glob::Pattern::new("docs/*").unwrap(),
            )]),
            programming_languages: None,
        });
        let (sql, binds) = build_symbols_query(guid(), "m", &req, 10);
        assert!(
            sql.contains("file_path IN (SELECT path FROM project_files"),
            "the scope is a subquery, not a join — build_file_filter emits unqualified \
             column names: {sql}"
        );
        assert_eq!(
            &binds[..3],
            &[
                Bind::Guid(guid()),
                Bind::Path("m".into()),
                Bind::Path("collect".into()),
            ],
            "the query's own binds must keep positions 1-3: {binds:?}"
        );
    }

    /// An unscoped lookup must build byte-for-byte the SQL it always did, so the
    /// public `/symbols` endpoint provably did not change when scoping was added.
    #[test]
    fn an_unscoped_symbols_lookup_builds_the_sql_it_always_did() {
        let (sql, binds) = build_symbols_query(guid(), "m", &symbols_req("collect", None), 10);
        assert!(!sql.contains("file_path IN"), "{sql}");
        assert_eq!(binds.len(), 3, "project, model, name — nothing else");
    }

    #[tokio::test]
    async fn symbols_query_ranks_anchor_file_then_exact_directory() {
        let files: &[&str] = &[
            "root.rs",
            "src/backend/error.rs",
            "src/db/deep/inner.rs",
            "src/db/files.rs",
            "src/db/qdrant.rs",
        ];
        let pool = pool_with_prepared_files(files, 0).await;
        for f in files {
            insert_symbol(&pool, f, "target").await;
        }

        let (paths, total) =
            run_symbols_query(&pool, symbols_req("target", Some("src/db/qdrant.rs")), 10).await;
        assert_eq!(total, 5);
        assert_eq!(
            paths,
            vec![
                "src/db/qdrant.rs", // tier 0: the anchor file itself
                "src/db/files.rs",  // tier 1: exactly src/db/ — NOT src/db/deep/
                "root.rs",          // tier 2: everything else, path ASC
                "src/backend/error.rs",
                "src/db/deep/inner.rs",
            ],
            "same file > same exact directory > rest (path ASC within tiers)"
        );
    }

    #[tokio::test]
    async fn symbols_query_root_anchor_treats_rootlevel_files_as_same_dir() {
        let files: &[&str] = &["main.rs", "other.rs", "src/lib.rs"];
        let pool = pool_with_prepared_files(files, 0).await;
        for f in files {
            insert_symbol(&pool, f, "target").await;
        }
        let (paths, _) = run_symbols_query(&pool, symbols_req("target", Some("main.rs")), 10).await;
        assert_eq!(paths, vec!["main.rs", "other.rs", "src/lib.rs"]);
    }

    #[tokio::test]
    async fn symbols_query_totals_survive_the_limit() {
        let files: &[&str] = &["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
        let pool = pool_with_prepared_files(files, 0).await;
        for f in files {
            insert_symbol(&pool, f, "popular").await;
        }
        let (paths, total) = run_symbols_query(&pool, symbols_req("popular", None), 2).await;
        assert_eq!(paths.len(), 2, "the limit caps the returned rows");
        assert_eq!(total, 5, "the total must report the full candidate count");
    }

    // ── git history reconciliation ──────────────────────────────────────
    // The channel's whole sync story is a set difference on shas, so these
    // pin the three shapes that difference takes: a re-post (nothing moves),
    // a windowed post (nothing outside the window moves) and a rewrite (the
    // old shas go, and their paths go with them).

    const HISTORY_MODEL: &str = "BAAI/bge-m3";

    async fn history_pool() -> SQLite3Pool {
        // Pool size 1 so the ":memory:" connection — and therefore the schema —
        // is the same one every later transaction gets.
        let p = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        p.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            Ok(())
        })
        .await
        .unwrap();
        p
    }

    fn commit_at(sha_byte: char, committed_at: i64, paths: &[&str]) -> CommitEntry {
        CommitEntry {
            sha: sha_byte.to_string().repeat(40),
            author_name: "T".into(),
            author_email: "t@example.com".into(),
            authored_at: committed_at,
            committed_at,
            parent_count: 1,
            subject: format!("commit {sha_byte}"),
            body: String::new(),
            paths: paths
                .iter()
                .map(|p| CommitPath {
                    path: (*p).to_string(),
                    change_type: ChangeType::Modified,
                    old_path: None,
                })
                .collect(),
        }
    }

    async fn reconcile(
        p: &SQLite3Pool,
        since: Option<i64>,
        commits: Vec<CommitEntry>,
    ) -> HistoryResponse {
        p.transaction(CancellationToken::new(), move |tx| {
            reconcile_history(
                tx,
                guid(),
                HISTORY_MODEL,
                &HistoryRequest { since, commits },
            )
        })
        .await
        .unwrap()
    }

    async fn stored_shas(p: &SQLite3Pool) -> Vec<String> {
        p.transaction(CancellationToken::new(), move |tx| {
            let mut stmt = tx.prepare("SELECT sha FROM project_commits ORDER BY committed_at")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
    }

    async fn path_row_count(p: &SQLite3Pool) -> i64 {
        p.transaction(CancellationToken::new(), move |tx| {
            Ok(
                tx.query_row("SELECT COUNT(*) FROM project_commit_paths", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await
        .unwrap()
    }

    /// A commit's content is its identity, so posting the same set twice must be
    /// a genuine no-op — that is what lets a client re-post its whole window on
    /// every run instead of negotiating a diff with the server first.
    #[tokio::test]
    async fn reposting_the_same_history_changes_nothing() {
        let p = history_pool().await;
        let commits = vec![
            commit_at('a', 100, &["src/a.rs"]),
            commit_at('b', 200, &["src/b.rs", "src/a.rs"]),
        ];

        let first = reconcile(&p, None, commits.clone()).await;
        assert_eq!(
            first,
            HistoryResponse {
                indexed: 2,
                unchanged: 0,
                removed: 0
            }
        );

        let second = reconcile(&p, None, commits).await;
        assert_eq!(
            second,
            HistoryResponse {
                indexed: 0,
                unchanged: 2,
                removed: 0
            }
        );
        assert_eq!(stored_shas(&p).await.len(), 2);
        assert_eq!(path_row_count(&p).await, 3);
    }

    /// `since` bounds the deletion half. Without it a client walking only the
    /// recent window would wipe everything older on every pass — from the
    /// server's side an unmentioned commit and one outside the walk look
    /// identical, and only the client knows which it meant.
    #[tokio::test]
    async fn a_windowed_post_leaves_everything_older_alone() {
        let p = history_pool().await;
        reconcile(
            &p,
            None,
            vec![
                commit_at('a', 100, &["src/a.rs"]),
                commit_at('b', 500, &["src/b.rs"]),
            ],
        )
        .await;

        // A run walking only from t=400 names just the newer commit.
        let res = reconcile(&p, Some(400), vec![commit_at('b', 500, &["src/b.rs"])]).await;
        assert_eq!(res.removed, 0, "the older commit is outside the window");
        assert_eq!(stored_shas(&p).await.len(), 2);

        // The same post with no window claims to speak for all of history.
        let res = reconcile(&p, None, vec![commit_at('b', 500, &["src/b.rs"])]).await;
        assert_eq!(res.removed, 1);
        assert_eq!(stored_shas(&p).await, vec!["b".repeat(40)]);
    }

    /// Force-push, rebase and any other rewrite are not special cases: the new
    /// refs simply reach a disjoint set of shas, and reconciliation drops the
    /// old ones. Their path rows go with them through ON DELETE CASCADE — the
    /// one thing that would otherwise leave orphans behind.
    #[tokio::test]
    async fn a_rewritten_history_orphans_the_old_shas_and_their_paths() {
        let p = history_pool().await;
        reconcile(
            &p,
            None,
            vec![
                commit_at('a', 100, &["src/a.rs"]),
                commit_at('b', 200, &["src/b.rs"]),
            ],
        )
        .await;
        assert_eq!(path_row_count(&p).await, 2);

        // Same window, entirely new shas: what a rebase looks like from here.
        let res = reconcile(
            &p,
            None,
            vec![
                commit_at('c', 100, &["src/a.rs"]),
                commit_at('d', 200, &["src/b.rs"]),
            ],
        )
        .await;
        assert_eq!(
            res,
            HistoryResponse {
                indexed: 2,
                unchanged: 0,
                removed: 2
            }
        );
        assert_eq!(stored_shas(&p).await, vec!["c".repeat(40), "d".repeat(40)]);
        assert_eq!(
            path_row_count(&p).await,
            2,
            "the dropped commits' paths must cascade away, not orphan"
        );
    }

    async fn prune(p: &SQLite3Pool, q: HistoryPruneQuery) -> HistoryPruneResponse {
        p.transaction(CancellationToken::new(), move |tx| {
            prune_history(tx, guid(), HISTORY_MODEL, &q)
        })
        .await
        .unwrap()
    }

    /// Retention is the half reconciliation structurally cannot do: a commit
    /// still reachable from the tracked refs is never dropped by a `POST`,
    /// however old it gets. Each bound alone, and `keep_last=0` as the explicit
    /// spelling of "drop the channel".
    #[tokio::test]
    async fn each_retention_bound_prunes_on_its_own_axis() {
        let p = history_pool().await;
        let all = vec![
            commit_at('a', 100, &["src/a.rs"]),
            commit_at('b', 200, &["src/b.rs"]),
            commit_at('c', 300, &["src/c.rs"]),
        ];
        reconcile(&p, None, all.clone()).await;

        // Rank alone: keep the two newest.
        let res = prune(
            &p,
            HistoryPruneQuery {
                keep_last: Some(2),
                older_than: None,
            },
        )
        .await;
        assert_eq!(
            res,
            HistoryPruneResponse {
                removed: 1,
                remaining: 2
            }
        );
        assert_eq!(stored_shas(&p).await, vec!["b".repeat(40), "c".repeat(40)]);
        assert_eq!(
            path_row_count(&p).await,
            2,
            "the pruned commit's paths must cascade away, not orphan"
        );

        // Clock alone.
        let res = prune(
            &p,
            HistoryPruneQuery {
                keep_last: None,
                older_than: Some(250),
            },
        )
        .await;
        assert_eq!(res.removed, 1);
        assert_eq!(stored_shas(&p).await, vec!["c".repeat(40)]);

        // `keep_last=0` is how "everything" is spelled out loud.
        reconcile(&p, None, all).await;
        let res = prune(
            &p,
            HistoryPruneQuery {
                keep_last: Some(0),
                older_than: None,
            },
        )
        .await;
        assert_eq!(
            res,
            HistoryPruneResponse {
                removed: 3,
                remaining: 0
            }
        );
        assert_eq!(path_row_count(&p).await, 0);
    }

    /// The bounds **intersect**, and that is the load-bearing choice: given two
    /// rules a destructive endpoint must take the conservative reading, so
    /// `keep_last` is a floor the clock cannot cut through. Union semantics here
    /// would make "prune anything older than a year, but never leave me with
    /// fewer than N" silently mean "delete everything older than a year".
    #[tokio::test]
    async fn the_two_bounds_intersect_so_keep_last_is_a_floor() {
        let p = history_pool().await;
        reconcile(
            &p,
            None,
            vec![
                commit_at('a', 100, &["src/a.rs"]),
                commit_at('b', 200, &["src/b.rs"]),
                commit_at('c', 300, &["src/c.rs"]),
            ],
        )
        .await;

        // The clock condemns all three; the floor saves the two newest.
        let res = prune(
            &p,
            HistoryPruneQuery {
                keep_last: Some(2),
                older_than: Some(1_000),
            },
        )
        .await;
        assert_eq!(
            res,
            HistoryPruneResponse {
                removed: 1,
                remaining: 2
            }
        );
        assert_eq!(stored_shas(&p).await, vec!["b".repeat(40), "c".repeat(40)]);

        // And a floor wider than the history protects all of it — the caller
        // sees that from `remaining` without a second request.
        let res = prune(
            &p,
            HistoryPruneQuery {
                keep_last: Some(99),
                older_than: Some(1_000),
            },
        )
        .await;
        assert_eq!(
            res,
            HistoryPruneResponse {
                removed: 0,
                remaining: 2
            }
        );
    }

    /// A prune leaves the channel in a state the next ordinary indexer run
    /// simply refills — the repository is the source of truth, so this handle is
    /// destructive without being lossy. Pinned because it is the reason the
    /// endpoint can be blunt: reconciliation re-inserts what the refs still
    /// reach, and `unchanged` proves the rest was untouched.
    #[tokio::test]
    async fn a_pruned_history_is_rebuilt_by_the_next_reconciliation() {
        let p = history_pool().await;
        let all = vec![
            commit_at('a', 100, &["src/a.rs"]),
            commit_at('b', 200, &["src/b.rs"]),
        ];
        reconcile(&p, None, all.clone()).await;
        prune(
            &p,
            HistoryPruneQuery {
                keep_last: Some(1),
                older_than: None,
            },
        )
        .await;

        let res = reconcile(&p, None, all).await;
        assert_eq!(
            res,
            HistoryResponse {
                indexed: 1,
                unchanged: 1,
                removed: 0
            }
        );
    }

    /// The regression guard for the whole two-table decision. A commit names
    /// paths the working tree may not contain — deleted long ago, excluded by
    /// `.mindex`, or in a language the enum does not carry. Had these been
    /// modelled as `project_files` rows, every one of them would be reported
    /// `orphaned` by every drift check forever, `mindex-index --check` would
    /// exit non-zero on a clean tree, and the watcher would keep trying to
    /// delete them.
    #[tokio::test]
    async fn commit_rows_are_invisible_to_drift() {
        let p = history_pool().await;
        reconcile(
            &p,
            None,
            vec![commit_at(
                'a',
                100,
                &["deleted/long/ago.rs", "vendor/excluded.rs"],
            )],
        )
        .await;

        // What read_drift_baseline reads: project_files, which history never touches.
        let files = p
            .transaction(CancellationToken::new(), move |tx| {
                Ok(tx.query_row("SELECT COUNT(*) FROM project_files", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await
            .unwrap();
        assert_eq!(files, 0, "history must not create project_files rows");

        let res = compute_drift(&HashMap::new(), &HashSet::new(), &HashMap::new());
        assert!(
            res.orphaned.is_empty(),
            "a commit's paths must never surface as working-tree drift"
        );
    }

    /// Deleting a project takes its history with it — the FK to `projects` is
    /// the one place commit rows are attached to anything.
    #[tokio::test]
    async fn deleting_a_project_cascades_to_its_history() {
        let p = history_pool().await;
        reconcile(&p, None, vec![commit_at('a', 100, &["src/a.rs"])]).await;

        p.transaction(CancellationToken::new(), move |tx| {
            tx.execute("DELETE FROM projects WHERE guid = ?1", params![guid()])?;
            Ok(())
        })
        .await
        .unwrap();

        assert!(stored_shas(&p).await.is_empty());
        assert_eq!(path_row_count(&p).await, 0);
    }

    async fn read_history(
        p: &SQLite3Pool,
        path: &'static str,
    ) -> (bool, bool, usize, Vec<CommitSummary>) {
        p.transaction(CancellationToken::new(), move |tx| {
            read_file_history(tx, guid(), HISTORY_MODEL, path, FILE_HISTORY_LIMIT)
        })
        .await
        .unwrap()
    }

    /// The single most load-bearing read in the tool. Without the channel probe,
    /// "nobody ever reconciled this repository's commits" and "nothing ever
    /// touched this file" are byte-for-byte the same answer — and the model
    /// reports the second, which is a claim about the file that nothing supports.
    #[tokio::test]
    async fn an_unreconciled_project_is_distinguishable_from_a_file_with_no_commits() {
        let p = history_pool().await;

        let (history_indexed, _, total, commits) = read_history(&p, "src/a.rs").await;
        assert!(!history_indexed, "no channel at all");
        assert_eq!((total, commits.len()), (0, 0));

        reconcile(&p, None, vec![commit_at('a', 100, &["src/other.rs"])]).await;

        let (history_indexed, _, total, commits) = read_history(&p, "src/a.rs").await;
        assert!(history_indexed, "the channel exists now");
        assert_eq!(
            (total, commits.len()),
            (0, 0),
            "but this particular file still has no commits"
        );
    }

    /// A commit names paths the code index does not hold — deleted years ago,
    /// excluded by `.mindex`, in an unsupported language. `path_indexed` is what
    /// keeps "gone from the tree" and "never there" apart, and the absence of a
    /// foreign key on `project_commit_paths.path` is what makes such a row
    /// storable at all.
    #[tokio::test]
    async fn a_commit_path_the_code_index_never_held_is_stored_and_flagged() {
        let p = history_pool().await;
        reconcile(
            &p,
            None,
            vec![commit_at('a', 100, &["deleted/long/ago.rs"])],
        )
        .await;

        let (_, path_indexed, total, commits) = read_history(&p, "deleted/long/ago.rs").await;
        assert!(!path_indexed, "the code channel does not hold it");
        assert_eq!(total, 1, "its history is real regardless");
        assert_eq!(commits[0].short_sha, "aaaaaaaa");
    }

    /// Newest first, capped, with the pre-cap total reported separately — so a
    /// truncated answer is visibly truncated rather than a list that happens to
    /// be exactly as long as the cap.
    #[tokio::test]
    async fn commits_come_back_newest_first_and_truncation_is_visible() {
        let p = history_pool().await;
        let shas: Vec<char> = ('a'..='z').take(FILE_HISTORY_LIMIT + 3).collect();
        let commits: Vec<_> = shas
            .iter()
            .enumerate()
            .map(|(i, c)| commit_at(*c, 100 + i as i64, &["src/a.rs"]))
            .collect();
        reconcile(&p, None, commits).await;

        let (_, _, total, got) = read_history(&p, "src/a.rs").await;
        assert_eq!(
            total,
            FILE_HISTORY_LIMIT + 3,
            "the pre-cap count is reported"
        );
        assert_eq!(got.len(), FILE_HISTORY_LIMIT);
        assert_eq!(
            got[0].sha,
            shas.last().unwrap().to_string().repeat(40),
            "newest first"
        );

        // The limit is threaded, not baked into the SQL: a widened grant
        // (`evidence_width` 2) actually reaches the read.
        let widened = p
            .transaction(CancellationToken::new(), move |tx| {
                read_file_history(
                    tx,
                    guid(),
                    HISTORY_MODEL,
                    "src/a.rs",
                    scaled_width(FILE_HISTORY_LIMIT, 2),
                )
            })
            .await
            .unwrap();
        assert_eq!(widened.3.len(), FILE_HISTORY_LIMIT + 3, "width 2 uncaps it");
    }

    /// A file that was moved carries its earlier history under its old name.
    /// Without `old_path` the trail simply stops at the rename with nothing
    /// saying why, which reads as "this file has no earlier history".
    #[tokio::test]
    async fn a_rename_is_stored_with_its_source_path() {
        let p = history_pool().await;
        let mut c = commit_at('a', 100, &[]);
        c.paths = vec![CommitPath {
            path: "src/new.rs".into(),
            change_type: ChangeType::Renamed,
            old_path: Some("src/old.rs".into()),
        }];
        reconcile(&p, None, vec![c]).await;

        let (_, _, total, got) = read_history(&p, "src/new.rs").await;
        assert_eq!(total, 1);
        assert_eq!(got[0].change_type, ChangeType::Renamed);
        assert_eq!(got[0].old_path.as_deref(), Some("src/old.rs"));
    }

    /// A [`ConfigResponse`] with just enough shape to render the `/llms.txt`
    /// live section — the tests below vary the model catalog and the observed
    /// stats, everything else is inert.
    fn llms_test_config(
        models: Vec<String>,
        models_refreshed_at: Option<i64>,
        observed: Vec<ResearchObservedEffort>,
    ) -> ConfigResponse {
        let effort = || ResearchEffortInfo {
            max_seconds: 300,
            max_tokens: 400_000,
            max_steps: 8,
            context_fraction: 0.5,
            search_top_k: 5,
            max_report_words: 400,
            max_report_sections: 6,
            evidence_width: 1,
            worst_case_seconds: 420,
        };
        ConfigResponse {
            version: "0.0.0-test",
            model_id: "test-embedder".into(),
            languages: vec!["rust"],
            embed_batch: 256,
            db_pool_size: 4,
            stuck_grace_mins: 30,
            max_retries: 3,
            search: SearchConfigInfo {
                default_top_k: 5,
                max_top_k: 100,
                max_query_bytes: 8192,
            },
            research: ResearchConfigInfo {
                default_model: "test-model:8b".into(),
                models,
                allowed_models: vec![],
                models_refreshed_at,
                effort: ResearchEffortLadder {
                    low: effort(),
                    medium: effort(),
                    high: effort(),
                },
                max_request_seconds: 3600,
                max_request_tokens: 6_000_000,
                max_request_steps: 64,
                max_request_report_sections: 12,
                max_request_report_words: 1800,
                max_evidence_width: 4,
                max_concurrent: 1,
                max_context_runs: 4,
                max_context_chars: 24_000,
                report_timeout_ms: 120_000,
                checkpoint_every_steps: 6,
                list_page_limit: 50,
                max_delete_ids: 500,
                sampling: ResearchSamplingInfo {
                    temperature: None,
                    top_p: None,
                    seed: None,
                },
                observed: ResearchObservedInfo {
                    refreshed_at: models_refreshed_at,
                    efforts: observed,
                },
            },
        }
    }

    /// Every backtick-quoted route the bootstrap document names, with an
    /// optional leading HTTP method stripped. This is what the drift guard
    /// checks against the OpenAPI spec.
    fn llms_route_mentions(doc: &str) -> Vec<String> {
        doc.split('`')
            .skip(1)
            .step_by(2)
            .filter_map(|span| {
                let path = ["GET ", "POST ", "DELETE ", "PUT ", "PATCH "]
                    .iter()
                    .find_map(|m| span.strip_prefix(m))
                    .unwrap_or(span);
                path.starts_with('/').then(|| path.to_string())
            })
            .collect()
    }

    /// The drift guard on the narrative half of `/llms.txt`: the document is
    /// the fourth copy of the workflow prose (after the OpenAPI description and
    /// the two MCP instruction blocks), and the routes it names are the part a
    /// test can hold still. Every backticked path must exist in the OpenAPI
    /// spec — a renamed or removed endpoint fails here instead of leaving the
    /// bootstrap document teaching a route that 404s.
    #[test]
    fn llms_doc_mentions_only_routes_that_exist() {
        // Deliberately outside the spec, each asserted so there by
        // `openapi_spec_is_complete_and_versioned` or served by the Swagger
        // merge rather than a documented handler. Read from the one list the
        // service descriptor also reads, rather than a second copy of the same
        // four strings — which is what this was, and what would have had to be
        // edited twice.
        let outside_spec: Vec<&str> = crate::backend::http3::UNDOCUMENTED_ROUTES
            .iter()
            .map(|(path, _)| *path)
            .collect();

        let doc = llms_document(&llms_test_config(vec![], None, vec![]));
        let spec =
            serde_json::to_value(crate::backend::openapi::api_doc()).expect("spec serializes");
        let paths = spec["paths"].as_object().expect("paths object");

        let mentions = llms_route_mentions(&doc);
        assert!(
            mentions.iter().any(|p| p.starts_with("/v0/")),
            "the extractor found no data-plane route at all — it is broken, not the doc"
        );
        for m in &mentions {
            assert!(
                paths.contains_key(m.as_str()) || outside_spec.contains(&m.as_str()),
                "llms_doc.md names a route the OpenAPI spec does not know: {m}"
            );
        }
    }

    /// The content-type is a wire contract like the OpenMetrics one on
    /// `/metrics`: markdown, never `text/plain`.
    #[test]
    fn llms_txt_content_type_is_markdown() {
        assert_eq!(LLMS_TXT_CONTENT_TYPE, "text/markdown; charset=utf-8");
    }

    /// An unrefreshed catalog must be stated as unknown, an empty-but-refreshed
    /// one as empty — and neither may invent a model name. The distinction is
    /// the same one `models_refreshed_at` carries on `/config`.
    #[test]
    fn llms_doc_is_honest_about_an_empty_model_catalog() {
        let never = llms_document(&llms_test_config(vec![], None, vec![]));
        assert!(never.contains("has not been refreshed"));

        let empty = llms_document(&llms_test_config(vec![], Some(1_700_000_000), vec![]));
        assert!(empty.contains("No models are currently available"));

        let populated = llms_document(&llms_test_config(
            vec!["glm-4:9b".into()],
            Some(1_700_000_000),
            vec![ResearchObservedEffort {
                model: "glm-4:9b".into(),
                effort: "medium".into(),
                runs: 12,
                p50_seconds: 180,
                p90_seconds: 420,
            }],
        ));
        assert!(populated.contains("- `glm-4:9b`"));
        assert!(populated.contains("| glm-4:9b | medium | 12 | 180 | 420 |"));
    }

    /// `/llms.txt` is fetched over the network by a model whose client may
    /// classify what it fetched as a prompt injection — GitHub Copilot did
    /// exactly that to an earlier draft of this document, on a corporate
    /// machine, and the endpoint was simply unusable there. The trigger is
    /// register, not content: a document that commands its reader, or that
    /// instructs the reader about how to treat its own instructions, is
    /// indistinguishable from an attack on the wire.
    ///
    /// So the document argues instead of ordering — every recommendation
    /// carries its reason, and the reader is "a caller", not "you". That much
    /// is a matter of writing and cannot be asserted. What *can* be pinned is
    /// the short list of constructions that most reliably trip a classifier,
    /// none of which has any business in an API reference. A regression here is
    /// silent: the document still renders, still passes every other test, and
    /// simply stops being readable by the clients it exists for.
    ///
    /// The whole rendered body is scanned, live section included — that section
    /// is generated, so nothing but a test keeps an imperative out of it.
    #[test]
    fn llms_doc_avoids_the_injection_signature() {
        const SIGNATURES: &[&str] = &[
            "ignore ",
            "disregard",
            "regardless of any",
            "you have been handed",
            "instructions above",
            "previous instructions",
            "system prompt",
        ];

        let doc = llms_document(&llms_test_config(
            vec!["glm-4:9b".into()],
            Some(1_700_000_000),
            vec![],
        ))
        .to_lowercase();

        for s in SIGNATURES {
            assert!(
                !doc.contains(s),
                "llms_doc.md contains {s:?} — a phrase that reads as an instruction \
                 to the model rather than a description of this API, and is what \
                 gets the document refused"
            );
        }
    }

    // ── the service descriptor ───────────────────────────────────────────────

    fn test_descriptor() -> MindexDescriptor {
        descriptor_document(llms_test_config(vec![], None, vec![]), 6, false)
    }

    /// The sync guard, and the reason the inventory is derived rather than
    /// written: it must be exactly the spec's set of operations, both ways. A
    /// documented endpoint the descriptor omits is a capability an agent cannot
    /// discover; an entry with no operation behind it is a 404 the descriptor
    /// promised. Neither is visible without this test, because both halves
    /// serialize perfectly well.
    #[test]
    fn descriptor_lists_every_route_the_spec_knows() {
        let spec = serde_json::to_value(crate::backend::openapi::api_doc()).expect("serializes");
        let paths = spec["paths"].as_object().expect("paths object");

        let mut from_spec: Vec<(String, String)> = Vec::new();
        for (path, item) in paths {
            for method in item.as_object().expect("path item").keys() {
                from_spec.push((method.to_ascii_uppercase(), path.clone()));
            }
        }
        from_spec.sort();

        let mut from_descriptor: Vec<(String, String)> = test_descriptor()
            .endpoints
            .into_iter()
            .filter(|e| e.documented)
            .map(|e| (e.method, e.path))
            .collect();
        from_descriptor.sort();

        assert_eq!(
            from_descriptor, from_spec,
            "the descriptor's documented inventory and the OpenAPI spec disagree"
        );
    }

    /// The undocumented half is exactly the routes that are deliberately absent
    /// from the spec, minus the ones the descriptor hides. Without this, adding
    /// a `#[utoipa::path]` to `/llms.txt` would leave it listed twice — once as
    /// documented and once as not.
    #[test]
    fn descriptor_undocumented_routes_are_the_ones_outside_the_spec() {
        let spec = serde_json::to_value(crate::backend::openapi::api_doc()).expect("serializes");
        let paths = spec["paths"].as_object().expect("paths object");

        for (path, _) in crate::backend::http3::UNDOCUMENTED_ROUTES {
            assert!(
                !paths.contains_key(*path),
                "{path} is in UNDOCUMENTED_ROUTES but the spec documents it"
            );
        }

        let mut listed: Vec<String> = test_descriptor()
            .endpoints
            .into_iter()
            .filter(|e| !e.documented)
            .map(|e| e.path)
            .collect();
        listed.sort();

        let mut expected: Vec<String> = crate::backend::http3::UNDOCUMENTED_ROUTES
            .iter()
            .map(|(p, _)| *p)
            .filter(|p| !crate::backend::http3::DESCRIPTOR_HIDDEN_ROUTES.contains(p))
            .map(str::to_string)
            .collect();
        expected.sort();

        assert_eq!(listed, expected);
        assert!(
            !listed.iter().any(|p| p == "/metrics"),
            "/metrics is routed only when [metrics].enabled, so advertising it \
             would promise a 404 on every deployment that has it off"
        );
    }

    /// The drift guard against the router itself. Neither axum nor utoipa can
    /// enumerate registered routes at runtime, so this reads the source text of
    /// `http3.rs` and pins every `.route("…")` literal against the union the
    /// descriptor reports plus the routes it deliberately hides. It is a
    /// source-text test on purpose: the alternative is nothing, and "nothing" is
    /// what let the route table and its four descriptions drift in the first
    /// place.
    #[test]
    fn the_route_table_holds_no_path_the_descriptor_omits() {
        // Only the production half: `http3.rs`'s own test module stands up
        // throwaway routers (`/slow` and friends) whose routes are not part of
        // the API and must not be discoverable.
        let src = include_str!("../http3.rs");
        let src = src
            .split_once("\n#[cfg(test)]")
            .map_or(src, |(head, _)| head);

        // `.route(` and its path literal are frequently separated by a newline
        // and indentation — rustfmt breaks the long ones — so the whitespace has
        // to be skipped or two thirds of the table goes unchecked.
        let mut routed: Vec<&str> = src
            .match_indices(".route(")
            .filter_map(|(i, m)| {
                let rest = src[i + m.len()..].trim_start();
                let body = rest.strip_prefix('"')?;
                body.find('"').map(|end| &body[..end])
            })
            .collect();
        routed.sort_unstable();
        routed.dedup();
        assert!(
            routed.len() > 20,
            "the extractor found {} routes — it is broken, not the router",
            routed.len()
        );

        let descriptor = test_descriptor();
        let mut known: std::collections::HashSet<&str> = descriptor
            .endpoints
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        known.extend(
            crate::backend::http3::DESCRIPTOR_HIDDEN_ROUTES
                .iter()
                .copied(),
        );

        for path in routed {
            assert!(
                known.contains(path),
                "{path} is registered in http3.rs but no discovery document reports it"
            );
        }
    }

    /// A summary is what a caller reads to choose an endpoint; an empty one
    /// makes the entry noise. Also pins that the derivation actually found
    /// utoipa's text rather than silently falling back to `""` — which is what a
    /// change in how utoipa splits `summary` from `description` would look like.
    #[test]
    fn descriptor_summaries_are_present_and_descriptive() {
        for e in test_descriptor().endpoints {
            assert!(
                e.summary.len() > 10,
                "{} {} has no usable summary: {:?}",
                e.method,
                e.path,
                e.summary
            );
        }
    }

    /// The inlined snapshot must be the `/config` body itself, not a trimmed or
    /// re-derived copy — that is what makes one request enough to bootstrap, and
    /// what stops the two endpoints answering differently about the same server.
    #[test]
    fn descriptor_config_is_the_config_endpoints_own_snapshot() {
        let snapshot = llms_test_config(vec!["glm-4:9b".into()], Some(1_700_000_000), vec![]);
        let expected = serde_json::to_value(&snapshot).expect("serializes");
        let descriptor = descriptor_document(snapshot, 6, false);
        assert_eq!(
            serde_json::to_value(&descriptor.config).expect("serializes"),
            expected
        );
    }

    /// One document, one version. `version` and `config.version` come from
    /// different structs and could be wired to different sources; a caller that
    /// finds them disagreeing has no way to tell which describes the running
    /// build. Driven through a real `RouterState` rather than the fixture
    /// config, which carries a deliberately fake version and so would agree with
    /// the bug.
    #[tokio::test]
    async fn descriptor_versions_agree() {
        let pool = pool_with_chunks(&[]).await;
        let s = router_state(pool);
        let d = descriptor_document(config_snapshot(&s).await, s.db_schema_version, false);

        assert_eq!(d.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(d.config.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(d.service, "mindex");
        // Explicitly null, never absent: a caller must be able to tell "this
        // server authenticates nothing" from "this server is too old to say".
        let json = serde_json::to_value(&d).expect("serializes");
        assert!(json.get("authentication").is_some());
        assert!(json["authentication"].is_null());
    }

    /// The streaming flag is the one hand-maintained fact in the inventory, so
    /// it is the one that can silently stop being true. Both research streams
    /// are SSE and `/index` is ndjson; everything else is a single body.
    #[test]
    fn descriptor_names_the_streaming_endpoints() {
        let streaming: std::collections::HashMap<String, &'static str> = test_descriptor()
            .endpoints
            .into_iter()
            .filter_map(|e| e.streaming.map(|s| (format!("{} {}", e.method, e.path), s)))
            .collect();

        assert_eq!(
            streaming.get("POST /v0/{project_guid}/research"),
            Some(&"sse")
        );
        assert_eq!(
            streaming.get("POST /v0/{project_guid}/research/{run_id}/challenge"),
            Some(&"sse")
        );
        assert_eq!(
            streaming.get("POST /v0/{project_guid}/index"),
            Some(&"ndjson")
        );
        assert_eq!(
            streaming.len(),
            crate::backend::http3::STREAMING_ENDPOINTS.len()
        );
        assert_eq!(streaming.get("POST /v0/{project_guid}/search"), None);
    }

    // ── the SQL-only cores ───────────────────────────────────────────────────

    /// A project with the given `(path, code)` chunks, all `active`, and their files
    /// `indexed`. `code` is stored verbatim, so a test can grep it.
    async fn pool_with_chunks(files: &[(&'static str, &'static str)]) -> SQLite3Pool {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(p, c)| ((*p).to_string(), (*c).to_string()))
            .collect();
        pool.transaction(CancellationToken::new(), move |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![guid(), MODEL],
            )?;
            for (path, code) in &files {
                let lang = if path.ends_with(".md") {
                    "markdown"
                } else {
                    "rust"
                };
                tx.execute(
                    "INSERT INTO project_files
                         (project_guid, model_id, path, sha256, programming_language, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'indexing')",
                    params![guid(), MODEL, path, "0".repeat(64), lang],
                )?;
                tx.execute(
                    "UPDATE project_files SET status = 'indexed'
                      WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3",
                    params![guid(), MODEL, path],
                )?;
                tx.execute(
                    "INSERT INTO project_file_chunks
                         (project_guid, file_path, model_id, code, qdrant_guid,
                          start_line, end_line, start_column, end_column, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, 10, 0, 1, 'active')",
                    params![
                        guid(),
                        path,
                        MODEL,
                        code,
                        Uuid::new_v4().simple().to_string()
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed");
        pool
    }

    fn unscoped() -> crate::research::ToolScope {
        crate::research::ToolScope {
            include: None,
            exclude: None,
        }
    }

    fn scoped_to(paths: &[&str]) -> crate::research::ToolScope {
        crate::research::ToolScope {
            include: Some(SearchFilter {
                paths: Some(paths.iter().map(|p| glob(p)).collect()),
                programming_languages: None,
            }),
            exclude: None,
        }
    }

    /// An empty `grep` result has **three** meanings, and reporting them as one is
    /// how a run honestly reports 0 hits for a literal the next run finds five times.
    /// `searched_chunks`/`searched_files` are what separate "nothing here was
    /// searchable" from "no indexed chunk contains this", and they are read only on a
    /// miss — the second scan is worth paying for exactly when it changes the answer.
    #[tokio::test]
    async fn a_grep_miss_says_whether_anything_was_searchable() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "fn alpha() { let x = 1; }"),
            ("src/b.rs", "fn beta() { let y = 2; }"),
        ])
        .await;
        let s = router_state(pool);

        // (a) A hit: the counts are not paid for.
        let hit = grep_core(
            &s,
            guid(),
            "alpha",
            None,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(hit.total, 1);
        assert_eq!(hit.matches.len(), 1);
        assert_eq!(hit.matches[0].path, "src/a.rs");
        assert!(
            hit.searched_chunks.is_none() && hit.searched_files.is_none(),
            "the reach counts were computed on a hit, where they change nothing"
        );

        // (b) Genuinely absent: there *was* something to search, and it said so.
        let absent = grep_core(
            &s,
            guid(),
            "gamma",
            None,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(absent.total, 0);
        assert_eq!(
            (absent.searched_chunks, absent.searched_files),
            (Some(2), Some(2)),
            "a real absence must say how much was in reach to make it meaningful"
        );

        // (c) Nothing searchable: the glob matches no file, so the same zero means
        // something entirely different. This is the case that used to read as proof.
        let unreachable = grep_core(
            &s,
            guid(),
            "alpha",
            Some("docs/**"),
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(unreachable.total, 0);
        assert_eq!(
            (unreachable.searched_chunks, unreachable.searched_files),
            (Some(0), Some(0)),
            "a glob that matched no file reported the same reach as a real search"
        );
    }

    /// `like_escape` is mandatory, not cosmetic: `_` is a `LIKE` wildcard, so an
    /// unescaped `read_chunks` also matches `readAchunks` — and, worse, an unescaped
    /// `%` matches everything, turning a miss into a full-corpus hit.
    #[tokio::test]
    async fn a_grep_pattern_is_matched_literally_not_as_a_like_expression() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "fn read_chunks() {}"),
            ("src/b.rs", "fn readXchunks() {}"),
            ("src/c.rs", "let pct = 100;"),
        ])
        .await;
        let s = router_state(pool);

        let underscore = grep_core(
            &s,
            guid(),
            "read_chunks",
            None,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(
            underscore.total,
            1,
            "`_` was treated as a wildcard: {:?}",
            underscore
                .matches
                .iter()
                .map(|m| &m.path)
                .collect::<Vec<_>>()
        );
        assert_eq!(underscore.matches[0].path, "src/a.rs");

        // A bare `%` must match nothing here, not everything.
        let percent = grep_core(
            &s,
            guid(),
            "%",
            None,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(
            percent.total, 0,
            "`%` matched the whole corpus instead of a literal percent sign"
        );
    }

    /// Scope is enforced in SQL, and for a text-keyed tool the rows are dropped
    /// **and counted**: a filtered total that silently shrinks is indistinguishable
    /// from a string that simply occurs less often.
    #[tokio::test]
    async fn a_scoped_grep_counts_what_the_walls_hid() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "fn target() {}"),
            ("tests/b.rs", "fn target() {}"),
            ("tests/c.rs", "fn target() {}"),
        ])
        .await;
        let s = router_state(pool);

        let scoped = grep_core(
            &s,
            guid(),
            "target",
            None,
            &scoped_to(&["src/**"]),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");

        assert_eq!(scoped.total, 1, "the scope did not hold");
        assert_eq!(scoped.matches[0].path, "src/a.rs");
        assert_eq!(
            scoped.out_of_scope, 2,
            "the run was not told how much its own scope hid"
        );

        // Unscoped, the same pattern finds all three — so `out_of_scope` was real.
        let all = grep_core(
            &s,
            guid(),
            "target",
            None,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(all.total, 3);
        assert_eq!(all.out_of_scope, 0, "an unscoped run hid nothing");
    }

    /// A soft-deleted chunk is gone from every read path — the `status = 'active'`
    /// rule. Without it a `DELETE /files` would keep answering greps until GC ran.
    #[tokio::test]
    async fn grep_never_returns_a_soft_deleted_chunk() {
        let pool = pool_with_chunks(&[("src/a.rs", "fn target() {}")]).await;
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                "UPDATE project_file_chunks SET status = 'deleted' WHERE project_guid = ?1",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .expect("soft delete");
        let s = router_state(pool);

        let out = grep_core(
            &s,
            guid(),
            "target",
            None,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(out.total, 0, "a soft-deleted chunk is still being searched");
        assert_eq!(
            (out.searched_chunks, out.searched_files),
            (Some(0), Some(0)),
            "and it must not count toward what was in reach either"
        );
    }

    /// `read_chunks` reads the **index**, never the file, and a path the scope
    /// refuses is an explicit refusal — `in_scope: false` — not an empty range. The
    /// two are opposite answers: one says "I am not allowed to look", the other says
    /// "I looked and there is nothing there".
    #[tokio::test]
    async fn read_chunks_distinguishes_refusal_from_absence_and_from_no_such_file() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "fn alpha() {}"),
            ("tests/b.rs", "fn beta() {}"),
        ])
        .await;
        let s = router_state(pool);

        // In scope, in range: the indexed code comes back.
        let hit = read_chunks_core(
            &s,
            guid(),
            "src/a.rs",
            1,
            10,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("read runs");
        assert!(hit.indexed && hit.in_scope);
        assert_eq!(hit.chunks.len(), 1);
        assert_eq!(hit.chunks[0].code, "fn alpha() {}");

        // Out of scope: a refusal, and deliberately not an empty range.
        let refused = read_chunks_core(
            &s,
            guid(),
            "tests/b.rs",
            1,
            10,
            &scoped_to(&["src/**"]),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("read runs");
        assert!(!refused.in_scope, "a refused path did not say so");
        assert!(refused.chunks.is_empty());

        // Indexed, but that range holds no chunk: a different answer again.
        let empty_range = read_chunks_core(
            &s,
            guid(),
            "src/a.rs",
            900,
            999,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("read runs");
        assert!(
            empty_range.indexed && empty_range.in_scope,
            "the file is indexed and allowed; only the range is empty"
        );
        assert!(empty_range.chunks.is_empty());

        // A path that was never indexed at all: the third answer.
        let unknown = read_chunks_core(
            &s,
            guid(),
            "src/never.rs",
            1,
            10,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("read runs");
        assert!(
            !unknown.indexed,
            "an unindexed path read as indexed-with-no-chunks"
        );
    }

    /// The scope is enforced on `read_chunks` in SQL, which is what stops a scoped
    /// run using the excerpt channel to read bytes it was refused. This must hold
    /// for the *content*, not merely the flag.
    #[tokio::test]
    async fn a_scoped_read_chunks_never_returns_refused_bytes() {
        let pool =
            pool_with_chunks(&[("secrets/keys.rs", "const TOKEN: &str = \"hunter2\";")]).await;
        let s = router_state(pool);

        let refused = read_chunks_core(
            &s,
            guid(),
            "secrets/keys.rs",
            1,
            10,
            &scoped_to(&["src/**"]),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("read runs");

        assert!(!refused.in_scope);
        assert!(
            refused.chunks.is_empty(),
            "a scoped run was handed the bytes its scope refused"
        );
    }

    /// A soft-deleted chunk must not come back through `read_chunks` either — the
    /// same `status = 'active'` rule, on the path that ships code to the model.
    #[tokio::test]
    async fn read_chunks_never_returns_a_soft_deleted_chunk() {
        let pool = pool_with_chunks(&[("src/a.rs", "fn alpha() {}")]).await;
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                "UPDATE project_file_chunks SET status = 'deleted' WHERE project_guid = ?1",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .expect("soft delete");
        let s = router_state(pool);

        let out = read_chunks_core(
            &s,
            guid(),
            "src/a.rs",
            1,
            10,
            &unscoped(),
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("read runs");
        assert!(
            out.chunks.is_empty(),
            "a soft-deleted chunk was shipped to the model"
        );
        assert!(
            out.indexed,
            "the file itself is still indexed; only its chunks are gone"
        );
    }

    /// Add definition rows for `path`. Each is `(name, kind, start_line, parent)`.
    async fn add_symbols(
        pool: &SQLite3Pool,
        path: &'static str,
        rows: &[(&'static str, &'static str, i64, Option<&'static str>)],
    ) {
        let rows: Vec<_> = rows
            .iter()
            .map(|(n, k, l, p)| {
                (
                    (*n).to_string(),
                    (*k).to_string(),
                    *l,
                    p.map(str::to_string),
                )
            })
            .collect();
        pool.transaction(CancellationToken::new(), move |tx| {
            for (name, kind, line, parent) in &rows {
                tx.execute(
                    "INSERT INTO project_file_symbols
                         (project_guid, model_id, file_path, name, kind,
                          start_line, end_line, start_column, end_column, parent_name)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 0, 1, ?7)",
                    params![guid(), MODEL, path, name, kind, line, parent],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed symbols");
    }

    /// `outline` reports three states, and the reason is the same one `read_chunks`
    /// has: a refusal that reads as an empty outline tells the caller the file has
    /// no definitions, which is a different and wrong fact. `indexed` separates
    /// "no such file" from "indexed with nothing tagged" — the case every language
    /// without a tags query is permanently in.
    #[tokio::test]
    async fn outline_separates_unindexed_from_untagged_from_refused() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "fn alpha() {}"),
            ("src/quiet.rs", "// nothing tagged here"),
            ("tests/b.rs", "fn beta() {}"),
        ])
        .await;
        add_symbols(&pool, "src/a.rs", &[("alpha", "function", 1, None)]).await;
        let s = router_state(pool);

        let found = outline_core(
            &s,
            guid(),
            "src/a.rs",
            &unscoped(),
            &CancellationToken::new(),
        )
        .await
        .expect("outline runs");
        assert!(found.indexed && found.in_scope);
        assert_eq!(found.symbols.len(), 1);
        assert_eq!(found.total_definitions, 1);
        assert_eq!(
            found.programming_language,
            Some(ProgrammingLanguage::Rust),
            "the language must be named — the `kind` labels are not uniform across \
             languages and a caller inferring from them needs to know which one"
        );

        // Indexed, nothing tagged. Every language with no tags query lives here, so
        // this must not read as "no such file".
        let quiet = outline_core(
            &s,
            guid(),
            "src/quiet.rs",
            &unscoped(),
            &CancellationToken::new(),
        )
        .await
        .expect("outline runs");
        assert!(
            quiet.indexed,
            "an indexed but untagged file read as unknown"
        );
        assert!(quiet.symbols.is_empty());

        // Never indexed.
        let unknown = outline_core(
            &s,
            guid(),
            "src/nope.rs",
            &unscoped(),
            &CancellationToken::new(),
        )
        .await
        .expect("outline runs");
        assert!(!unknown.indexed);

        // Refused: indexed, tagged, and deliberately not shown.
        add_symbols(&s.db_pool, "tests/b.rs", &[("beta", "function", 1, None)]).await;
        let refused = outline_core(
            &s,
            guid(),
            "tests/b.rs",
            &scoped_to(&["src/**"]),
            &CancellationToken::new(),
        )
        .await
        .expect("outline runs");
        assert!(
            !refused.in_scope,
            "a refused path read as an empty outline, i.e. as a file with no definitions"
        );
        assert!(refused.symbols.is_empty());
    }

    /// `list_files` is navigation, and its glob is SQLite `GLOB` — where `*` crosses
    /// `/`, unlike the `.mindex` dialect. The scope applies underneath it, so a run
    /// cannot widen its own walls by asking for `**`.
    #[tokio::test]
    async fn list_files_globs_over_the_scope_never_around_it() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "a"),
            ("src/deep/b.rs", "b"),
            ("tests/c.rs", "c"),
            ("README.md", "d"),
        ])
        .await;
        let s = router_state(pool);

        let all = list_files_core(&s, guid(), "*", &unscoped(), &CancellationToken::new())
            .await
            .expect("list runs");
        assert_eq!(all.total, 4, "SQLite GLOB `*` crosses `/`");

        let rust_only =
            list_files_core(&s, guid(), "src/*", &unscoped(), &CancellationToken::new())
                .await
                .expect("list runs");
        assert_eq!(
            rust_only.total, 2,
            "both src files, including the nested one"
        );

        // The widest possible glob cannot reach past the scope.
        let scoped = list_files_core(
            &s,
            guid(),
            "*",
            &scoped_to(&["src/**"]),
            &CancellationToken::new(),
        )
        .await
        .expect("list runs");
        assert_eq!(
            scoped.total, 2,
            "a run widened its own scope by asking for everything"
        );
        for f in &scoped.files {
            assert!(f.path.starts_with("src/"), "{} escaped the scope", f.path);
        }
    }

    /// A soft-deleted *file* must leave the listing at once. `list_files` is how a
    /// run decides what exists, so a deleted file offered here sends every later
    /// tool after something that is not there.
    #[tokio::test]
    async fn list_files_never_offers_a_soft_deleted_file() {
        let pool = pool_with_chunks(&[("src/a.rs", "a"), ("src/gone.rs", "b")]).await;
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                "UPDATE project_files SET status = 'deleted'
                  WHERE project_guid = ?1 AND path = 'src/gone.rs'",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .expect("soft delete");
        let s = router_state(pool);

        let out = list_files_core(&s, guid(), "*", &unscoped(), &CancellationToken::new())
            .await
            .expect("list runs");
        assert_eq!(out.total, 1);
        assert_eq!(out.files[0].path, "src/a.rs");
    }

    /// `/symbols` is contractually **definitive** — an empty answer means "no such
    /// symbol" — so it must return ranked candidates rather than one "the" answer
    /// when a name collides, and its full totals must survive the limit.
    #[tokio::test]
    async fn symbols_returns_ranked_candidates_with_honest_totals() {
        let pool = pool_with_chunks(&[
            ("src/anchor.rs", "a"),
            ("src/sibling.rs", "b"),
            ("other/far.rs", "c"),
        ])
        .await;
        for path in ["src/anchor.rs", "src/sibling.rs", "other/far.rs"] {
            add_symbols(
                &pool,
                match path {
                    "src/anchor.rs" => "src/anchor.rs",
                    "src/sibling.rs" => "src/sibling.rs",
                    _ => "other/far.rs",
                },
                &[("collide", "function", 1, None)],
            )
            .await;
        }
        let s = router_state(pool);

        let req = SymbolsRequest {
            name: "collide".to_string(),
            kind: None,
            anchor_path: Some("src/anchor.rs".to_string()),
            limit: Some(2),
            include: None,
            exclude: None,
        };
        let out = symbols_core(&s, guid(), &req, &CancellationToken::new())
            .await
            .expect("symbols runs");

        assert_eq!(
            out.total_definitions, 3,
            "the total must survive the limit, or truncation is invisible"
        );
        assert_eq!(out.definitions.len(), 2, "the limit was not applied");
        assert_eq!(
            out.definitions[0].path, "src/anchor.rs",
            "ranking is path-based: the anchor file comes first"
        );
        assert_eq!(
            out.definitions[1].path, "src/sibling.rs",
            "then its exact directory, then everything else"
        );
    }

    /// An empty `/symbols` answer is a 200, not a 404 — the endpoint's whole
    /// contract is that "no such symbol" is a *fact it asserts*, and an error would
    /// make it indistinguishable from a failed lookup.
    #[tokio::test]
    async fn an_unknown_symbol_is_an_empty_answer_not_a_failure() {
        let pool = pool_with_chunks(&[("src/a.rs", "a")]).await;
        add_symbols(&pool, "src/a.rs", &[("known", "function", 1, None)]).await;
        let s = router_state(pool);

        let req = SymbolsRequest {
            name: "nonexistent".to_string(),
            kind: None,
            anchor_path: None,
            limit: None,
            include: None,
            exclude: None,
        };
        let out = symbols_core(&s, guid(), &req, &CancellationToken::new())
            .await
            .expect("an unknown symbol is not an error");
        assert_eq!(out.total_definitions, 0);
        assert!(out.definitions.is_empty());
    }

    fn filter_of(paths: &[&str], langs: &[ProgrammingLanguage]) -> Option<SearchFilter> {
        Some(SearchFilter {
            paths: (!paths.is_empty()).then(|| paths.iter().map(|p| glob(p)).collect()),
            programming_languages: (!langs.is_empty()).then(|| langs.to_vec()),
        })
    }

    fn scope(
        include: Option<SearchFilter>,
        exclude: Option<SearchFilter>,
    ) -> crate::research::ToolScope {
        crate::research::ToolScope { include, exclude }
    }

    /// Paths admitted by a scope, sorted — read through the real SQL rather than by
    /// inspecting the generated string, since the string is not the contract.
    async fn admitted(s: &RouterState, sc: &crate::research::ToolScope) -> Vec<String> {
        let mut out: Vec<String> = list_files_core(s, guid(), "*", sc, &CancellationToken::new())
            .await
            .expect("list runs")
            .files
            .into_iter()
            .map(|f| f.path)
            .collect();
        out.sort();
        out
    }

    async fn mixed_project() -> RouterState {
        router_state(
            pool_with_chunks(&[
                ("src/a.rs", "a"),
                ("src/b.rs", "b"),
                ("src/gen/c.rs", "c"),
                ("tests/d.rs", "d"),
                ("docs/e.md", "e"),
            ])
            .await,
        )
    }

    /// `build_file_filter` is one expression with three callers that could not be
    /// more different in consequence: it is the walls of a scoped research run, the
    /// selector of `DELETE /files`, and the selector of `POST /cancel`. It had no
    /// test of its own. These read it through the real SQL, because the generated
    /// string is not the contract — what the database admits is.
    #[tokio::test]
    async fn an_absent_or_empty_selector_admits_the_whole_project() {
        let s = mixed_project().await;

        assert_eq!(admitted(&s, &scope(None, None)).await.len(), 5);
        // `Some` with empty lists is the same statement as `None`, and must not be
        // read as "admit nothing" — that would silently empty a scoped run and, on
        // the destructive endpoints, would be the more dangerous mistake.
        let empty = Some(SearchFilter {
            paths: Some(vec![]),
            programming_languages: Some(vec![]),
        });
        assert_eq!(admitted(&s, &scope(empty.clone(), empty)).await.len(), 5);
    }

    /// Several include globs are OR'd — a caller naming two directories wants both,
    /// not their (empty) intersection.
    #[tokio::test]
    async fn include_globs_are_alternatives_not_conjunctions() {
        let s = mixed_project().await;
        assert_eq!(
            admitted(&s, &scope(filter_of(&["src/**", "docs/**"], &[]), None)).await,
            vec!["docs/e.md", "src/a.rs", "src/b.rs", "src/gen/c.rs"]
        );
    }

    /// A language and a path in the same include are ANDed: they are two different
    /// axes of the same question, and OR-ing them would widen a scope past both.
    #[tokio::test]
    async fn a_language_and_a_path_narrow_together() {
        let s = mixed_project().await;
        assert_eq!(
            admitted(
                &s,
                &scope(filter_of(&["src/**"], &[ProgrammingLanguage::Rust]), None)
            )
            .await,
            vec!["src/a.rs", "src/b.rs", "src/gen/c.rs"]
        );
        // The same language with a path that admits only markdown yields nothing —
        // proving the two are combined rather than either standing alone.
        assert!(
            admitted(
                &s,
                &scope(filter_of(&["docs/**"], &[ProgrammingLanguage::Rust]), None)
            )
            .await
            .is_empty()
        );
    }

    /// **Exclude wins.** A file matching both is out, which is the rule `.mindex`
    /// states for the client walk and which the SQL must agree with — a blanket
    /// exclude that a later include could carve back open would mean the two halves
    /// of the same project describe different file sets, and that surfaces as
    /// permanent drift rather than as an error.
    #[tokio::test]
    async fn an_exclude_beats_an_include_that_also_matches() {
        let s = mixed_project().await;

        assert_eq!(
            admitted(
                &s,
                &scope(filter_of(&["src/**"], &[]), filter_of(&["src/gen/**"], &[]))
            )
            .await,
            vec!["src/a.rs", "src/b.rs"],
            "a file matched by both the include and the exclude was admitted"
        );

        // The same, by language.
        assert_eq!(
            admitted(
                &s,
                &scope(None, filter_of(&[], &[ProgrammingLanguage::Markdown]))
            )
            .await,
            vec!["src/a.rs", "src/b.rs", "src/gen/c.rs", "tests/d.rs"]
        );
    }

    /// An exclude alone is a legitimate scope — "everything but this" — and must not
    /// require an include to mean anything.
    #[tokio::test]
    async fn an_exclude_alone_still_narrows() {
        let s = mixed_project().await;
        assert_eq!(
            admitted(&s, &scope(None, filter_of(&["tests/**", "docs/**"], &[]))).await,
            vec!["src/a.rs", "src/b.rs", "src/gen/c.rs"]
        );
    }

    /// `status != 'deleted'` is part of the filter itself, not of its callers, so a
    /// soft-deleted file is outside *every* scope — including the selector of a
    /// second `DELETE /files`, which must not re-delete what is already gone, and
    /// the walls of a research run, which must not be shown a file that no longer
    /// exists.
    #[tokio::test]
    async fn a_soft_deleted_file_is_outside_every_selector() {
        let s = mixed_project().await;
        s.db_pool
            .transaction(CancellationToken::new(), |tx| {
                tx.execute(
                    "UPDATE project_files SET status = 'deleted'
                      WHERE project_guid = ?1 AND path = 'src/a.rs'",
                    params![guid()],
                )?;
                Ok(())
            })
            .await
            .expect("soft delete");

        for sc in [
            scope(None, None),
            scope(filter_of(&["src/**"], &[]), None),
            scope(filter_of(&[], &[ProgrammingLanguage::Rust]), None),
        ] {
            assert!(
                !admitted(&s, &sc).await.contains(&"src/a.rs".to_string()),
                "a soft-deleted file was admitted by a selector"
            );
        }
    }

    /// The scope fragment can be appended to a query that already has binds, and
    /// `first_bind` is what keeps the numbering straight. Get it wrong and the
    /// placeholders silently take each other's values — a scope bound to a pattern,
    /// a pattern bound to a guid — which SQLite reports as no rows rather than as an
    /// error. `grep_core` puts three binds before the scope; `callers_core` puts
    /// three before it too but rewrites one by index.
    #[tokio::test]
    async fn a_scope_appended_after_other_binds_still_binds_correctly() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "fn needle() {}"),
            ("tests/b.rs", "fn needle() {}"),
        ])
        .await;
        add_symbols(&pool, "src/a.rs", &[("needle", "function", 1, None)]).await;
        add_symbols(&pool, "tests/b.rs", &[("needle", "function", 2, None)]).await;
        let s = router_state(pool);
        let sc = scope(filter_of(&["src/**"], &[]), None);

        // grep: the pattern bind sits before the scope's.
        let g = grep_core(
            &s,
            guid(),
            "needle",
            None,
            &sc,
            8,
            &CancellationToken::new(),
        )
        .await
        .expect("grep runs");
        assert_eq!(g.total, 1, "the scope and the pattern binds crossed over");
        assert_eq!(g.matches[0].path, "src/a.rs");

        // symbols: the name bind sits before the scope's, and the scope's binds are
        // numbered from the end — so an off-by-one here shows up as the wrong rows.
        let req = SymbolsRequest {
            name: "needle".to_string(),
            kind: None,
            anchor_path: None,
            limit: None,
            include: Some(SearchFilter {
                paths: Some(vec![glob("src/**")]),
                programming_languages: None,
            }),
            exclude: None,
        };
        let sym = symbols_core(&s, guid(), &req, &CancellationToken::new())
            .await
            .expect("symbols runs");
        assert_eq!(
            sym.total_definitions, 1,
            "the name bind and the scope binds crossed over"
        );
        assert_eq!(
            sym.out_of_scope_definitions, 1,
            "the definition in tests/ is outside the scope and must be counted, \
             not absorbed"
        );
    }

    // ── drift: the buckets the four clients act on ───────────────────────────

    /// An in-flight file the client no longer has is left to settle rather than
    /// called orphaned: a delete would race the batch that is writing it, and the
    /// next sweep sees a settled state either way. Distinct from the in-flight case
    /// above, which has the file still present locally.
    #[test]
    fn an_in_flight_file_missing_locally_is_not_orphaned_yet() {
        let d = compute_drift(&map(&[("busy.rs", "h")]), &set(&["busy.rs"]), &map(&[]));

        assert!(
            d.orphaned.is_empty(),
            "a file mid-index was proposed for deletion"
        );
        // Nor `indexing`: that bucket describes the posted manifest, and this path
        // is not in it.
        assert!(d.indexing.is_empty());
        assert!(d.stale.is_empty() && d.missing.is_empty());
    }

    /// An empty manifest against a populated index means the working tree is gone,
    /// and every indexed file is genuinely orphaned. A legitimate answer rather than
    /// a malformed request — `validate_drift_request` agrees.
    #[test]
    fn an_empty_manifest_orphans_everything_indexed() {
        let d = compute_drift(&map(&[("a.rs", "x"), ("b.rs", "y")]), &set(&[]), &map(&[]));
        assert_eq!(d.orphaned, vec!["a.rs", "b.rs"]);
        assert!(d.stale.is_empty() && d.missing.is_empty());
    }

    /// Every bucket comes back sorted. Three clients diff these lists between runs
    /// and print them to humans, and the inputs are `HashMap`s — unsorted, an
    /// unchanged project would look different on every call, and `--check`'s output
    /// would reshuffle for no reason.
    #[test]
    fn every_drift_bucket_comes_back_sorted() {
        let d = compute_drift(
            &map(&[("z.rs", "1"), ("a.rs", "1"), ("m.rs", "1"), ("k.rs", "1")]),
            &set(&["k.rs"]),
            &map(&[
                ("z.rs", "2"),
                ("a.rs", "2"),
                ("k.rs", "1"),
                ("zz.rs", "3"),
                ("aa.rs", "3"),
            ]),
        );

        assert_eq!(d.stale, vec!["a.rs", "z.rs"]);
        assert_eq!(d.missing, vec!["aa.rs", "zz.rs"]);
        assert_eq!(d.orphaned, vec!["m.rs"]);
        assert_eq!(d.indexing, vec!["k.rs"]);
    }

    /// The status → bucket mapping, read against the real SQL. Its consequence is
    /// what a client does next, and the two directions fail differently:
    ///
    /// A `failed` file must fall to `missing`, so `--check` reports it and the
    /// client re-posts it. Read as `indexing` it would mean "leave it alone",
    /// and a permanently-failed file would never be re-offered by any client —
    /// the server's own retry worker gives up after `MAX_RETRIES`, so nothing
    /// else would ever try either.
    ///
    /// A `just_uploaded` or `indexing` file must fall to `indexing`, because its
    /// stored hash describes content whose vectors do not exist yet. Read as
    /// `indexed` it would report "in sync" for a file with nothing behind it.
    #[tokio::test]
    async fn the_drift_baseline_puts_each_status_where_its_client_will_act_on_it() {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![guid(), MODEL],
            )?;
            // One file per status, each reaching it the legal way.
            for (path, target) in [
                ("done.rs", "indexed"),
                ("busy.rs", "indexing"),
                ("fresh.rs", "just_uploaded"),
                ("broken.rs", "failed"),
                ("stopped.rs", "cancelled"),
                ("removed.rs", "deleted"),
            ] {
                let entry = if target == "just_uploaded" {
                    "just_uploaded"
                } else {
                    "indexing"
                };
                tx.execute(
                    "INSERT INTO project_files
                         (project_guid, model_id, path, sha256, programming_language, status)
                     VALUES (?1, ?2, ?3, ?4, 'rust', ?5)",
                    params![guid(), MODEL, path, "a".repeat(64), entry],
                )?;
                if target != entry {
                    tx.execute(
                        "UPDATE project_files SET status = ?4
                          WHERE project_guid = ?1 AND model_id = ?2 AND path = ?3",
                        params![guid(), MODEL, path, target],
                    )?;
                }
            }
            Ok(())
        })
        .await
        .expect("seed");
        let s = router_state(pool);

        let (indexed, in_flight) = read_drift_baseline(&s, &CancellationToken::new(), guid())
            .await
            .expect("baseline reads");

        assert_eq!(
            indexed.keys().collect::<Vec<_>>(),
            vec!["done.rs"],
            "only an `indexed` file carries a hash worth comparing"
        );
        let mut flight: Vec<&String> = in_flight.iter().collect();
        flight.sort();
        assert_eq!(
            flight,
            vec!["busy.rs", "fresh.rs"],
            "a file whose vectors are not ready yet must read as in flight"
        );

        // The three terminal-but-not-indexed statuses are in neither map, so
        // `compute_drift` sorts them into `missing` — work the client will do.
        for path in ["broken.rs", "stopped.rs", "removed.rs"] {
            assert!(!indexed.contains_key(path), "{path} claimed to be in sync");
            assert!(!in_flight.contains(path), "{path} claimed to be in flight");
        }

        // End to end, that is what the client is told.
        let posted = map(&[
            ("done.rs", &"a".repeat(64)),
            ("busy.rs", &"a".repeat(64)),
            ("fresh.rs", &"a".repeat(64)),
            ("broken.rs", &"a".repeat(64)),
            ("stopped.rs", &"a".repeat(64)),
            ("removed.rs", &"a".repeat(64)),
        ]
        .map(|(p, h)| (p, h.as_str())));
        let d = compute_drift(&indexed, &in_flight, &posted);

        assert_eq!(
            d.missing,
            vec!["broken.rs", "removed.rs", "stopped.rs"],
            "a failed, cancelled or deleted file must be offered back for indexing"
        );
        assert_eq!(d.indexing, vec!["busy.rs", "fresh.rs"]);
        assert!(d.stale.is_empty(), "done.rs matches its stored hash");
        assert!(d.orphaned.is_empty());
    }

    // ── search: the candidate set is the isolation mechanism ─────────────────

    /// Records the chunk ids the candidate query handed to Qdrant, and scores each
    /// of them so the winners come back.
    struct RecordingStore {
        asked: std::sync::Mutex<Vec<Vec<UUIDv4>>>,
    }

    #[async_trait]
    impl VectorStore for RecordingStore {
        async fn search(
            &self,
            _collection: &str,
            chunk_ids: Vec<UUIDv4>,
            _dense: Vec<f32>,
            _sparse_indices: Vec<u32>,
            _sparse_values: Vec<f32>,
            _colbert: Vec<Vec<f32>>,
            _top_k: u64,
        ) -> Result<Vec<SearchHit>, VectorStoreError> {
            self.asked.lock().unwrap().push(chunk_ids.clone());
            Ok(chunk_ids
                .into_iter()
                .enumerate()
                .map(|(i, id)| SearchHit {
                    id: qdrant_client::qdrant::PointId::from(id.0.to_string()),
                    score: 1.0 - (i as f32) / 100.0,
                })
                .collect())
        }
        async fn ensure_project(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn insert_batch(
            &self,
            _c: &str,
            _v: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_batch(&self, _c: &str, _g: Vec<String>) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_collection(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn health(&self) -> Result<(), VectorStoreError> {
            unreachable!()
        }
    }

    /// One vector per head for a single query — the shape `search_core` requires.
    struct OneVectorEmbedder;

    #[async_trait]
    impl crate::models::bge_m3::BGEm3Model for OneVectorEmbedder {
        async fn encode(
            &self,
            req: BGEm3EmbedRequest,
            _token: CancellationToken,
        ) -> Result<BGEm3EmbedResponse, EncodeError> {
            let n = req.texts.len();
            Ok(BGEm3EmbedResponse {
                dense_vecs: vec![vec![0.1; 4]; n],
                sparse_vecs: vec![std::collections::HashMap::from([(1u32, 0.5f32)]); n],
                colbert_vecs: vec![vec![vec![0.1; 4]]; n],
            })
        }
        async fn health(&self) -> Result<(), EncodeError> {
            Ok(())
        }
    }

    /// A state whose store records what it was asked, and whose embedder answers.
    fn searchable_state(pool: SQLite3Pool) -> (RouterState, Arc<RecordingStore>) {
        let store = Arc::new(RecordingStore {
            asked: std::sync::Mutex::new(vec![]),
        });
        let mut s = router_state(pool);
        s.qdrant = Arc::clone(&store) as Arc<dyn VectorStore>;
        let embedder: Arc<dyn crate::models::bge_m3::BGEm3Model> = Arc::new(OneVectorEmbedder);
        s.model = EmbeddingModel::BGEm3 {
            model_id: MODEL.to_string(),
            client: Arc::clone(&embedder),
        };
        s.query_model = embedder;
        (s, store)
    }

    fn query(q: &str) -> SearchRequest {
        SearchRequest {
            query: q.to_string(),
            top_k: None,
            include: None,
            exclude: None,
        }
    }

    /// **Project isolation is the candidate set and nothing else.** One Qdrant
    /// collection per project makes it hard to cross projects by accident, but
    /// within a collection the `has_id` filter built from this query is the *sole*
    /// mechanism — and it is also what excludes soft-deleted vectors, which are
    /// still physically present in Qdrant until GC runs.
    ///
    /// So the ids handed to the store are the invariant: a soft-deleted chunk in
    /// that list is a deleted file answering searches, and a foreign project's chunk
    /// in it is a data leak between projects.
    #[tokio::test]
    async fn only_active_chunks_of_this_project_reach_the_vector_store() {
        let pool = pool_with_chunks(&[("src/a.rs", "alpha"), ("src/b.rs", "beta")]).await;

        // A second project in the same database, and a soft-deleted chunk in this one.
        let other = UUIDv4(Uuid::from_u128(7));
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, ?2)",
                params![other, MODEL],
            )?;
            tx.execute(
                "INSERT INTO project_files
                     (project_guid, model_id, path, sha256, programming_language, status)
                 VALUES (?1, ?2, 'src/theirs.rs', ?3, 'rust', 'indexing')",
                params![other, MODEL, "0".repeat(64)],
            )?;
            tx.execute(
                "INSERT INTO project_file_chunks
                     (project_guid, file_path, model_id, code, qdrant_guid,
                      start_line, end_line, start_column, end_column, status)
                 VALUES (?1, 'src/theirs.rs', ?2, 'secret', ?3, 1, 2, 0, 1, 'active')",
                params![other, MODEL, Uuid::new_v4().simple().to_string()],
            )?;
            // And soft-delete one of ours.
            tx.execute(
                "UPDATE project_file_chunks SET status = 'deleted'
                  WHERE project_guid = ?1 AND file_path = 'src/b.rs'",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .expect("seed");

        let (s, store) = searchable_state(pool);
        let out = search_core(&s, guid(), &query("anything"), &CancellationToken::new())
            .await
            .expect("search runs");

        let asked = store.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "the store was called once");
        assert_eq!(
            asked[0].len(),
            1,
            "the candidate set must hold exactly this project's one active chunk"
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "src/a.rs");
        assert_eq!(out[0].code, "alpha");
    }

    /// An empty candidate set is answered **before** Qdrant is called at all. That
    /// is not an optimisation: a project whose collection does not exist yet would
    /// make the store raise, and a 503 `qdrant.unavailable` is the wrong answer to
    /// "your filter matched nothing".
    #[tokio::test]
    async fn an_empty_candidate_set_is_a_404_and_never_reaches_qdrant() {
        let pool = pool_with_chunks(&[("src/a.rs", "alpha")]).await;
        let (s, store) = searchable_state(pool);

        // A filter that admits nothing.
        let mut req = query("anything");
        req.include = Some(SearchFilter {
            paths: Some(vec![glob("nowhere/**")]),
            programming_languages: None,
        });

        let err = search_core(&s, guid(), &req, &CancellationToken::new())
            .await
            .expect_err("an empty candidate set is a NoMatch");
        assert_eq!(err.code(), "search.no_match");
        assert!(
            store.asked.lock().unwrap().is_empty(),
            "Qdrant was asked about a project whose candidate set was empty; if its \
             collection did not exist the caller would get a 503 instead of a 404"
        );
    }

    /// A project that has never been indexed is the same answer, by the same route —
    /// and must not be a 503 either.
    #[tokio::test]
    async fn an_unknown_project_is_a_404_not_a_dependency_failure() {
        let pool = pool_with_chunks(&[("src/a.rs", "alpha")]).await;
        let (s, store) = searchable_state(pool);

        let err = search_core(
            &s,
            UUIDv4(Uuid::from_u128(99)),
            &query("anything"),
            &CancellationToken::new(),
        )
        .await
        .expect_err("an unknown project matches nothing");
        assert_eq!(err.code(), "search.no_match");
        assert!(store.asked.lock().unwrap().is_empty());
    }

    /// The request's own `include`/`exclude` narrow the candidate set, with exclude
    /// winning — the same rule as a research scope, applied by a different builder,
    /// so the two must agree.
    #[tokio::test]
    async fn a_search_selector_narrows_the_candidate_set_with_exclude_winning() {
        let pool = pool_with_chunks(&[
            ("src/a.rs", "alpha"),
            ("src/gen/b.rs", "generated"),
            ("docs/c.md", "prose"),
        ])
        .await;
        let (s, store) = searchable_state(pool);

        let mut req = query("anything");
        req.include = Some(SearchFilter {
            paths: Some(vec![glob("src/**")]),
            programming_languages: None,
        });
        req.exclude = Some(SearchFilter {
            paths: Some(vec![glob("src/gen/**")]),
            programming_languages: None,
        });

        let out = search_core(&s, guid(), &req, &CancellationToken::new())
            .await
            .expect("search runs");

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "src/a.rs");
        assert_eq!(
            store.asked.lock().unwrap()[0].len(),
            1,
            "the excluded chunk was still offered to the reranker"
        );
    }

    /// A language filter narrows the same way — and reads the *file's* language, not
    /// anything stored on the chunk.
    #[tokio::test]
    async fn a_language_filter_narrows_the_candidate_set() {
        let pool = pool_with_chunks(&[("src/a.rs", "alpha"), ("docs/c.md", "prose")]).await;
        let (s, _store) = searchable_state(pool);

        let mut req = query("anything");
        req.include = Some(SearchFilter {
            paths: None,
            programming_languages: Some(vec![ProgrammingLanguage::Markdown]),
        });

        let out = search_core(&s, guid(), &req, &CancellationToken::new())
            .await
            .expect("search runs");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "docs/c.md");
    }

    /// Winners Qdrant scored whose SQLite row is gone are dropped **and counted**,
    /// and when *every* winner is one the caller gets a 404 rather than a 200 with
    /// an empty list. The reassuring spelling must not be the one that means the two
    /// stores have diverged.
    #[tokio::test]
    async fn winners_with_no_row_left_are_counted_and_all_of_them_is_a_404() {
        let pool = pool_with_chunks(&[("src/a.rs", "alpha")]).await;

        /// Scores a point id that belongs to no chunk row at all.
        struct GhostStore;
        #[async_trait]
        impl VectorStore for GhostStore {
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
                Ok(vec![SearchHit {
                    id: qdrant_client::qdrant::PointId::from(Uuid::from_u128(1234).to_string()),
                    score: 0.9,
                }])
            }
            async fn ensure_project(&self, _c: &str) -> Result<(), VectorStoreError> {
                unreachable!()
            }
            async fn insert_batch(
                &self,
                _c: &str,
                _v: Vec<ChunkAsVector>,
            ) -> Result<(), VectorStoreError> {
                unreachable!()
            }
            async fn delete_batch(
                &self,
                _c: &str,
                _g: Vec<String>,
            ) -> Result<(), VectorStoreError> {
                unreachable!()
            }
            async fn delete_collection(&self, _c: &str) -> Result<(), VectorStoreError> {
                unreachable!()
            }
            async fn health(&self) -> Result<(), VectorStoreError> {
                unreachable!()
            }
        }

        let (mut s, _) = searchable_state(pool);
        s.qdrant = Arc::new(GhostStore);

        let err = search_core(&s, guid(), &query("anything"), &CancellationToken::new())
            .await
            .expect_err("every winner was an orphan");
        assert_eq!(
            err.code(),
            "search.no_match",
            "an all-orphaned result set answered 200 with an empty list — the \
             reassuring spelling for the case that means the stores disagree"
        );
        assert!(
            s.metrics
                .render()
                .expect("renders")
                .contains("mindex_search_orphaned_winners_total 1"),
            "the orphaned winner was dropped silently"
        );
    }

    /// `mark_indexed` is the **last** line of defence on the cancel path, and the
    /// only status write in the file that deliberately does not go through
    /// `set_file_status`.
    ///
    /// `POST /cancel` takes no `IndexClaim` on purpose — it has to be able to
    /// interrupt one — so correctness against a live `/index` rests on two re-reads
    /// and this `AND status = 'indexing'`. A cancel landing *after* `drop_cancelled`
    /// has already re-read the statuses reaches phase 3 with the file `cancelled`;
    /// the UPDATE then matches zero rows, the illegal `cancelled → indexed`
    /// transition is never attempted, and the trigger stays the backstop rather than
    /// the mechanism.
    ///
    /// The return value is what the handler reports. Unchecked — which is what it
    /// was — the client is told `indexed` for a file the database says is
    /// `cancelled`, and sees no drift afterwards to correct it.
    #[tokio::test]
    async fn a_cancel_landing_during_the_embed_pass_is_refused_by_mark_indexed() {
        let pool = pool_with_prepared_files(&["a.rs"], 2).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();

        // The window `drop_cancelled` cannot cover: the cancel arrives after it ran.
        let pg = guid().0.as_simple().to_string();
        let _ = set_file_status(
            &pool,
            &pg,
            "a.rs",
            MODEL,
            "cancelled",
            false,
            CancellationToken::new(),
        )
        .await;

        let moved = indexer(&pool, &locks, &fx)
            .mark_indexed("a.rs", &"b".repeat(64))
            .await
            .expect("the write itself must not fail — it simply matches nothing");

        assert!(
            !moved,
            "a cancelled file was reported to the client as indexed"
        );
        assert_eq!(
            file_state(&pool, "a.rs").await.0,
            "cancelled",
            "the refused write must leave the file exactly as the cancel left it"
        );
    }

    /// The ordinary path, and the reason the guard above is a `bool` rather than a
    /// silent no-op: a file that really is `indexing` moves, has its hash confirmed
    /// and its retry counter cleared.
    #[tokio::test]
    async fn mark_indexed_confirms_the_hash_and_clears_the_retry_count() {
        let pool = pool_with_prepared_files(&["a.rs"], 2).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();
        let sha = "c".repeat(64);

        let moved = indexer(&pool, &locks, &fx)
            .mark_indexed("a.rs", &sha)
            .await
            .expect("write succeeds");

        assert!(moved);
        let (status, retries, stored) = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.query_row(
                    "SELECT status, retry_count, sha256 FROM project_files
                      WHERE project_guid = ?1 AND model_id = ?2 AND path = 'a.rs'",
                    params![guid(), MODEL],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .map_err(SQLite3PoolError::from)
            })
            .await
            .expect("read");

        assert_eq!(status, "indexed");
        assert_eq!(retries, 0, "a clean success clears prior failures");
        assert_eq!(
            stored, sha,
            "the hash is confirmed at `indexed`, not before"
        );
    }

    /// A file deleted out from under the batch is the same shape as a cancel: zero
    /// rows, no claim of success. `DELETE /projects/{guid}` hard-deletes rows, so
    /// this is reachable whenever a project is dropped mid-index.
    #[tokio::test]
    async fn mark_indexed_reports_a_file_that_vanished_rather_than_inventing_it() {
        let pool = pool_with_prepared_files(&["a.rs"], 2).await;
        let locks = Arc::new(Mutex::new(HashSet::new()));
        let fx = fixture();

        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute(
                "DELETE FROM project_file_chunks WHERE project_guid = ?1",
                params![guid()],
            )?;
            tx.execute(
                "DELETE FROM project_files WHERE project_guid = ?1",
                params![guid()],
            )?;
            Ok(())
        })
        .await
        .expect("hard delete");

        let moved = indexer(&pool, &locks, &fx)
            .mark_indexed("a.rs", &"d".repeat(64))
            .await
            .expect("write succeeds against no rows");
        assert!(
            !moved,
            "a file that no longer exists was reported as indexed"
        );
    }

    // ── /health, including the split-embedder configuration ──────────────────

    /// An embedder that answers or refuses on command.
    struct HealthEmbedder {
        ok: bool,
    }

    #[async_trait]
    impl crate::models::bge_m3::BGEm3Model for HealthEmbedder {
        async fn encode(
            &self,
            _req: BGEm3EmbedRequest,
            _token: CancellationToken,
        ) -> Result<BGEm3EmbedResponse, EncodeError> {
            unreachable!("health does not encode")
        }
        async fn health(&self) -> Result<(), EncodeError> {
            if self.ok {
                Ok(())
            } else {
                Err(EncodeError::Decode("embedder is down".into()))
            }
        }
    }

    /// A store that answers `health` and nothing else.
    struct HealthyStore;
    #[async_trait]
    impl VectorStore for HealthyStore {
        async fn health(&self) -> Result<(), VectorStoreError> {
            Ok(())
        }
        async fn ensure_project(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn insert_batch(
            &self,
            _c: &str,
            _v: Vec<ChunkAsVector>,
        ) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_batch(&self, _c: &str, _g: Vec<String>) -> Result<(), VectorStoreError> {
            unreachable!()
        }
        async fn delete_collection(&self, _c: &str) -> Result<(), VectorStoreError> {
            unreachable!()
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
            unreachable!()
        }
    }

    async fn health_state(index_ok: bool, query: Option<bool>) -> RouterState {
        let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            Ok(())
        })
        .await
        .expect("migrations apply");

        let mut s = router_state(pool);
        s.qdrant = Arc::new(HealthyStore);
        let indexer: Arc<dyn crate::models::bge_m3::BGEm3Model> =
            Arc::new(HealthEmbedder { ok: index_ok });
        s.model = EmbeddingModel::BGEm3 {
            model_id: MODEL.to_string(),
            client: Arc::clone(&indexer),
        };
        // `None` = one instance does both, which is the *same* `Arc` — that identity
        // is what `/health` decides on.
        s.query_model = match query {
            None => indexer,
            Some(ok) => Arc::new(HealthEmbedder { ok }),
        };
        s
    }

    /// A single-instance deployment must report **no** `query_embedder` check at
    /// all. Reporting one would claim a second instance exists, and the decision is
    /// made by `Arc::ptr_eq` rather than by comparing URLs precisely because the two
    /// URLs are equal in that configuration — comparing them would call one instance
    /// two things.
    #[tokio::test]
    async fn one_embedder_instance_reports_one_check() {
        let s = health_state(true, None).await;
        let Json(out) = get_health(axum::extract::State(s)).await;

        assert!(
            out.checks.query_embedder.is_none(),
            "a single-instance deployment claimed a second embedder"
        );
        assert_eq!(out.checks.embedder, CheckState::Ok);
        assert_eq!(
            out.status,
            crate::backend::v0::models::HealthStatus::Ok,
            "every check answers, so the verdict is ok — and a check that does not              exist must not be able to drag it down"
        );
    }

    /// The split configuration is rare and entirely possible — it is what frees the
    /// ~6 GiB of VRAM a resident fp32 model holds — and it has a failure mode the
    /// single-instance one does not: a healthy indexer beside a dead query instance,
    /// which without a separate probe is a green health check and every search
    /// failing.
    #[tokio::test]
    async fn a_split_deployment_probes_the_query_instance_separately() {
        // Both alive: two checks, both ok.
        let s = health_state(true, Some(true)).await;
        let Json(out) = get_health(axum::extract::State(s)).await;
        assert_eq!(out.checks.query_embedder, Some(CheckState::Ok));

        // The one that matters: indexing healthy, queries dead.
        let s = health_state(true, Some(false)).await;
        let Json(out) = get_health(axum::extract::State(s)).await;
        assert_eq!(
            out.checks.embedder,
            CheckState::Ok,
            "the indexing instance is fine and must say so"
        );
        assert_eq!(
            out.checks.query_embedder,
            Some(CheckState::Error),
            "a dead query instance was invisible; every search fails while health is green"
        );
        assert_eq!(
            out.status,
            crate::backend::v0::models::HealthStatus::Unhealthy,
            "the query embedder is a *required* dependency when it exists"
        );
    }

    /// `checks.*` is exactly `ok` or `error` — never the reason. This response is
    /// readable by anything that can reach the port, and a driver's error chain
    /// carries paths, URLs and versions.
    #[tokio::test]
    async fn a_failing_check_never_puts_its_reason_on_the_wire() {
        let s = health_state(false, None).await;
        let Json(out) = get_health(axum::extract::State(s)).await;

        let body = serde_json::to_string(&out).expect("serializes");
        assert!(
            body.contains(r#""embedder":"error""#),
            "the failing check must be named: {body}"
        );
        assert!(
            !body.contains("embedder is down"),
            "the probe's reason reached the wire: {body}"
        );
    }

    /// HTTP is always 200 — the verdict is in the body. A client keying on the
    /// status code would read "the service is fine" from a transport success.
    #[tokio::test]
    async fn health_answers_200_whatever_the_verdict() {
        for (index_ok, query) in [(true, None), (false, None), (true, Some(false))] {
            let s = health_state(index_ok, query).await;
            let resp = get_health(axum::extract::State(s)).await.into_response();
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::OK,
                "health answered a non-200; inspect `status`, not the code"
            );
        }
    }

    /// `indexing_files` rides on a probe that can fail, and `-1` is "not known" — a
    /// different answer from `0`, which would say the index is idle.
    #[tokio::test]
    async fn the_indexing_count_is_real_when_the_database_answers() {
        let s = health_state(true, None).await;
        let Json(out) = get_health(axum::extract::State(s)).await;
        assert_eq!(
            out.indexing_files, 0,
            "an answering database reports a real count, not the unknown sentinel"
        );
        assert_eq!(out.checks.sqlite, CheckState::Ok);
    }

    // ── Authorization, through the extractors that enforce it ────────────────
    //
    // Driven through a real `Router` rather than by calling handlers directly:
    // the whole mechanism lives in `FromRequestParts`, so a test that calls a
    // handler with a value it constructed itself would exercise everything
    // except the part under test.

    mod authorization {
        use super::*;
        use crate::backend::auth::{Action, Keyring, mint};
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        const SECRET: [u8; 32] = [42u8; 32];

        /// A router over an in-memory database already holding `projects`.
        ///
        /// Seeded before the state is built rather than through an endpoint,
        /// because every endpoint that could create one is itself behind the
        /// mechanism under test.
        async fn app(auth_on: bool, projects: &[uuid::Uuid]) -> axum::Router {
            let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
            let guids: Vec<String> = projects.iter().map(|g| g.simple().to_string()).collect();
            pool.transaction(CancellationToken::new(), move |tx| {
                for (_, m) in crate::MIGRATIONS {
                    tx.execute_batch(m)?;
                }
                for g in &guids {
                    tx.execute(
                        "INSERT INTO projects (guid, model_id) VALUES (?1, 'BAAI/bge-m3')",
                        params![g],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seeds");

            let mut state = router_state(pool);
            state.auth = auth_on.then(|| {
                Arc::new(crate::backend::http3::AuthState {
                    keyring: Keyring::from_secret("test", SECRET.to_vec()),
                    leeway_seconds: 60,
                    max_token_days: 90,
                })
            });
            axum::Router::new()
                .route(
                    "/projects/{project_guid}",
                    axum::routing::get(get_project_stats),
                )
                .route("/projects", axum::routing::get(get_projects))
                .route("/status", axum::routing::get(get_status))
                .with_state(state)
        }

        fn token_for(projects: &[&str], actions: &[Action]) -> String {
            let ring = Keyring::from_secret("test", SECRET.to_vec());
            mint(
                &ring,
                "test",
                projects.iter().map(|s| (*s).to_string()).collect(),
                actions.to_vec(),
                1,
            )
            .expect("mints")
            .0
        }

        /// The production route table, with authorization on.
        async fn full_router() -> axum::Router {
            let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
            pool.transaction(CancellationToken::new(), |tx| {
                for (_, m) in crate::MIGRATIONS {
                    tx.execute_batch(m)?;
                }
                Ok(())
            })
            .await
            .expect("migrates");
            let mut state = router_state(pool);
            state.auth = Some(Arc::new(crate::backend::http3::AuthState {
                keyring: Keyring::from_secret("test", SECRET.to_vec()),
                leeway_seconds: 60,
                max_token_days: 90,
            }));
            crate::backend::http3::production_router_for_test(state)
        }

        /// Like [`call`], for a route that is not a GET. An empty JSON body,
        /// which is enough: every assertion here lands before the body is read.
        async fn call_method(
            app: &axum::Router,
            method: &str,
            uri: &str,
            token: Option<&str>,
        ) -> (StatusCode, String) {
            let mut req = Request::builder().method(method).uri(uri);
            if let Some(t) = token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            let body = if method == "GET" || method == "DELETE" {
                Body::empty()
            } else {
                req = req.header("content-type", "application/json");
                Body::from("{}")
            };
            let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }

        async fn call(app: &axum::Router, uri: &str, token: Option<&str>) -> (StatusCode, String) {
            let mut req = Request::builder().uri(uri);
            if let Some(t) = token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            let resp = app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }

        /// The claim this whole design rests on: a project the token does not
        /// cover is **indistinguishable** from one that was never indexed.
        ///
        /// Asserted on the response bytes, not the status code — a status-only
        /// check passes with an oracle sitting in `detail` or `meta`, which is
        /// precisely where one would end up if somebody later decided the
        /// refusal ought to be more helpful.
        #[tokio::test]
        async fn an_out_of_scope_project_is_byte_identical_to_one_that_never_existed() {
            let app = app(true, &[]).await;
            let mine = uuid::Uuid::new_v4();
            let theirs = uuid::Uuid::new_v4();
            let nobodys = uuid::Uuid::new_v4();
            let t = token_for(&[&mine.to_string()], &[Action::Search]);

            let (s1, b1) = call(&app, &format!("/projects/{theirs}"), Some(&t)).await;
            let (s2, b2) = call(&app, &format!("/projects/{nobodys}"), Some(&t)).await;

            assert_eq!(s1, StatusCode::NOT_FOUND);
            assert_eq!(s1, s2);
            assert_eq!(
                b1, b2,
                "a foreign project and an absent one answer differently, which tells a \
                 prober which GUIDs exist"
            );
            assert!(b1.contains("project.not_found"), "{b1}");
        }

        /// The missing *action* is named, unlike the missing project — and the
        /// asymmetry is the reasoning. A caller that got here already proved it
        /// holds the project, so naming the action tells it nothing it could not
        /// read out of its own token, while hiding it would leave an
        /// under-scoped credential indistinguishable from a wrong one.
        #[tokio::test]
        async fn a_missing_action_is_refused_distinguishably() {
            let app = app(true, &[]).await;
            let guid = uuid::Uuid::new_v4();
            let t = token_for(&[&guid.to_string()], &[Action::Research]);

            let (status, body) = call(&app, &format!("/projects/{guid}"), Some(&t)).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(body.contains("auth.action_not_permitted"), "{body}");
            assert!(body.contains("search"), "must name the action: {body}");
        }

        /// The project check runs before the action check, so a caller that
        /// cannot see the project learns nothing about the action vocabulary.
        /// Reversing the two would turn every 403 into an existence oracle.
        #[tokio::test]
        async fn a_foreign_project_is_refused_before_the_action_is_considered() {
            let app = app(true, &[]).await;
            let theirs = uuid::Uuid::new_v4();
            let t = token_for(&[&uuid::Uuid::new_v4().to_string()], &[Action::Research]);

            let (status, body) = call(&app, &format!("/projects/{theirs}"), Some(&t)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert!(!body.contains("action"), "leaked the action check: {body}");
        }

        #[tokio::test]
        async fn no_token_and_a_bad_token_are_each_their_own_code() {
            let app = app(true, &[]).await;
            let guid = uuid::Uuid::new_v4();

            let (status, body) = call(&app, &format!("/projects/{guid}"), None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert!(body.contains("auth.token_missing"), "{body}");

            let forged = mint(
                &Keyring::from_secret("test", vec![9u8; 32]),
                "x",
                vec!["*".into()],
                vec![Action::Search],
                1,
            )
            .unwrap()
            .0;
            let (status, body) = call(&app, &format!("/projects/{guid}"), Some(&forged)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert!(body.contains("auth.token_invalid"), "{body}");
        }

        /// `GET /projects` is why authorization could not live in the gateway:
        /// the GUIDs are in a response **body**, and a GUID is a bearer
        /// identifier. A proxy cannot filter this without parsing JSON.
        #[tokio::test]
        async fn the_project_listing_shows_only_what_the_token_covers() {
            let mine = uuid::Uuid::new_v4();
            let theirs = uuid::Uuid::new_v4();
            let app = app(true, &[mine, theirs]).await;

            let t = token_for(&[&mine.to_string()], &[Action::Search]);
            let (status, body) = call(&app, "/projects", Some(&t)).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                body.contains(&mine.simple().to_string()),
                "own project missing: {body}"
            );
            assert!(
                !body.contains(&theirs.simple().to_string()),
                "another caller's project is listed: {body}"
            );

            let all = token_for(&["*"], &[Action::Search]);
            let (_, body) = call(&app, "/projects", Some(&all)).await;
            assert!(
                body.contains(&theirs.simple().to_string()),
                "a wildcard token must see everything: {body}"
            );
        }

        /// `/status` is global — file counts across every project, the pool and
        /// the claim table — so it takes `admin` and a project-scoped token
        /// cannot reach it however many projects it names.
        #[tokio::test]
        async fn a_global_route_needs_admin_however_wide_the_project_list() {
            let app = app(true, &[]).await;
            let wide = token_for(
                &["*"],
                &[
                    Action::Search,
                    Action::Research,
                    Action::Index,
                    Action::Delete,
                ],
            );
            let (status, body) = call(&app, "/status", Some(&wide)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert!(body.contains("admin"), "{body}");

            let admin = token_for(&["*"], &[Action::Admin]);
            let (status, _) = call(&app, "/status", Some(&admin)).await;
            assert_eq!(status, StatusCode::OK);
        }

        /// **The guarantee for handlers that do not exist yet.**
        ///
        /// A route added to the table without a `ROUTE_POLICY` row is refused at
        /// request time, not served. The build-time guard catches the same
        /// mistake, but only when the suite runs; this is what stands between a
        /// forgotten row and a live open endpoint.
        ///
        /// Written against a router carrying a *fabricated* route precisely
        /// because the real table has no such hole — testing it any other way
        /// would mean waiting for somebody to make the mistake.
        #[tokio::test]
        async fn a_route_with_no_policy_row_is_refused_rather_than_served() {
            let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
            pool.transaction(CancellationToken::new(), |tx| {
                for (_, m) in crate::MIGRATIONS {
                    tx.execute_batch(m)?;
                }
                Ok(())
            })
            .await
            .expect("migrates");
            let mut state = router_state(pool);
            state.auth = Some(Arc::new(crate::backend::http3::AuthState {
                keyring: Keyring::from_secret("test", SECRET.to_vec()),
                leeway_seconds: 60,
                max_token_days: 90,
            }));

            let app = crate::backend::http3::build_router_for_test(state);
            let t = token_for(&["*"], Action::ALL);
            let (status, body) = call(&app, "/a-future-endpoint", Some(&t)).await;

            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "an unlisted route was served: {body}"
            );
            assert!(body.contains("auth.route_not_configured"), "{body}");
        }

        /// The same route, unauthenticated, must not be served either — the
        /// refusal cannot depend on the caller having presented anything.
        #[tokio::test]
        async fn an_unlisted_route_is_refused_with_no_token_at_all() {
            let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
            pool.transaction(CancellationToken::new(), |tx| {
                for (_, m) in crate::MIGRATIONS {
                    tx.execute_batch(m)?;
                }
                Ok(())
            })
            .await
            .expect("migrates");
            let mut state = router_state(pool);
            state.auth = Some(Arc::new(crate::backend::http3::AuthState {
                keyring: Keyring::from_secret("test", SECRET.to_vec()),
                leeway_seconds: 60,
                max_token_days: 90,
            }));

            let app = crate::backend::http3::build_router_for_test(state);
            let (status, _) = call(&app, "/a-future-endpoint", None).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }

        /// **Every** non-public route, checked against every refusal, driven by
        /// `ROUTE_POLICY` itself.
        ///
        /// The other tests in this module exercise the mechanism on three routes.
        /// That proves the mechanism and says nothing about coverage, and
        /// coverage is the whole question for a security rule: a suite of
        /// hand-written per-endpoint tests is exhaustive on the day it is
        /// written and silently incomplete on the day a route is added. Driving
        /// it from the table means a new route is tested the moment it is
        /// listed — and it must be listed, because
        /// `every_route_is_named_by_the_authorization_policy` fails otherwise.
        ///
        /// Three assertions per route, and all three land **before** the
        /// handler, which is why this can run against fixtures whose embedder
        /// and vector store refuse everything:
        ///
        /// 1. no credential at all → 401
        /// 2. a valid token carrying every action *except* the one this route
        ///    needs → 403
        /// 3. for project-keyed routes, a token with the right action but naming
        ///    a different project → 404
        #[tokio::test]
        async fn every_route_refuses_every_way_it_should() {
            use crate::backend::http3::{ROUTE_POLICY, RoutePolicy};

            let app = full_router().await;
            let other = uuid::Uuid::new_v4();
            let mut checked = 0usize;

            for (method, path, policy) in ROUTE_POLICY {
                let Some(needed) = policy.action() else {
                    continue;
                };

                // A concrete URI for this route template. `{run_id}` is opaque
                // to authorization, so any string does.
                let uri = path
                    .replace("{project_guid}", &other.to_string())
                    .replace("{run_id}", "00000000-0000-4000-8000-000000000000");

                // 1. No credential.
                let (status, body) = call_method(&app, method, &uri, None).await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} answered without a token: {body}"
                );
                assert!(
                    body.contains("auth.token_missing"),
                    "{method} {path}: {body}"
                );

                // 2. Every action but the one it needs. A wildcard project, so
                //    the *only* thing that can refuse is the action.
                let others: Vec<Action> = Action::ALL
                    .iter()
                    .copied()
                    .filter(|a| *a != needed)
                    .collect();
                let t = token_for(&["*"], &others);
                let (status, body) = call_method(&app, method, &uri, Some(&t)).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "{method} {path} needs `{needed}` but served a token without it: {body}"
                );
                assert!(
                    body.contains("auth.action_not_permitted"),
                    "{method} {path}: {body}"
                );

                // 3. Right action, wrong project. Skipped for the routes that
                //    genuinely have no project: a global route cannot be out of
                //    project scope, and `/drift` answers an out-of-scope project
                //    as it answers an unknown one, by contract.
                if path.contains("{project_guid}") && *policy != RoutePolicy::Drift {
                    let t = token_for(&[&uuid::Uuid::new_v4().to_string()], &[needed]);
                    let (status, body) = call_method(&app, method, &uri, Some(&t)).await;
                    assert_eq!(
                        status,
                        StatusCode::NOT_FOUND,
                        "{method} {path} served a project the token does not name: {body}"
                    );
                    assert!(
                        body.contains("project.not_found"),
                        "{method} {path} refused a foreign project with a code that names \
                         the reason — that is the enumeration oracle: {body}"
                    );
                }

                checked += 1;
            }

            assert!(
                checked >= 25,
                "only {checked} routes were checked — the table or this loop is broken, \
                 not the server"
            );
        }

        /// `POST /drift` is the one route whose out-of-scope answer is a rewrite
        /// rather than a refusal, so the loop above skips it and this states the
        /// rule instead of leaving a hole.
        ///
        /// Its contract is that an unknown project is not a 404 — every posted
        /// file simply comes back `missing`. A project the token cannot see must
        /// answer identically, or `/drift` becomes the single endpoint where a
        /// caller can tell "not mine" from "not there".
        #[tokio::test]
        async fn drift_answers_an_out_of_scope_project_as_an_unknown_one() {
            let app = full_router().await;
            let mine = uuid::Uuid::new_v4();
            let theirs = uuid::Uuid::new_v4();
            let t = token_for(&[&mine.to_string()], &[Action::Search]);

            let body = serde_json::json!({ "files": { "a.rs": "0".repeat(64) } }).to_string();
            let post = |guid: uuid::Uuid| {
                let app = app.clone();
                let t = t.clone();
                let body = body.clone();
                async move {
                    let resp = app
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri(format!("/projects/{guid}/drift"))
                                .header("authorization", format!("Bearer {t}"))
                                .header("content-type", "application/json")
                                .body(Body::from(body))
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    let status = resp.status();
                    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    (status, String::from_utf8_lossy(&bytes).into_owned())
                }
            };

            let (s1, b1) = post(theirs).await;
            let (s2, b2) = post(uuid::Uuid::new_v4()).await;
            assert_eq!(s1, StatusCode::OK, "{b1}");
            assert_eq!(s1, s2);
            assert_eq!(
                b1, b2,
                "drift told an out-of-scope project apart from an unknown one"
            );
            assert!(b1.contains("missing"), "{b1}");
        }

        /// The five public routes stay reachable with no credential at all.
        ///
        /// `/health` and `/version` are liveness — a probe that needs a token
        /// reports the token's health, not the server's. `/config`, `/llms.txt`
        /// and the descriptor are discovery, and a document telling a caller it
        /// needs a credential cannot itself require one; that circularity is the
        /// failure this whole feature started from.
        #[tokio::test]
        async fn the_public_routes_need_no_token() {
            let pool = SQLite3Pool::new(FsPath::new(":memory:"), 1, 16384, "NORMAL");
            pool.transaction(CancellationToken::new(), |tx| {
                for (_, m) in crate::MIGRATIONS {
                    tx.execute_batch(m)?;
                }
                Ok(())
            })
            .await
            .expect("migrates");
            let mut state = router_state(pool);
            state.auth = Some(Arc::new(crate::backend::http3::AuthState {
                keyring: Keyring::from_secret("test", SECRET.to_vec()),
                leeway_seconds: 60,
                max_token_days: 90,
            }));

            // Stub handlers rather than the real ones: what is under test is
            // whether the layer lets these paths through, and the real `/health`
            // would fail on the refusing fakes this fixture is built from —
            // which would look like an authorization failure and is not one.
            let app = axum::Router::new()
                .route("/health", axum::routing::get(|| async { "ok" }))
                .route("/version", axum::routing::get(|| async { "ok" }))
                .route("/config", axum::routing::get(|| async { "ok" }))
                .route("/llms.txt", axum::routing::get(|| async { "ok" }))
                .route(
                    "/.well-known/mindex.json",
                    axum::routing::get(|| async { "ok" }),
                )
                .layer(axum::middleware::from_fn({
                    let auth = state.auth.clone();
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        let auth = auth.clone();
                        async move {
                            crate::backend::http3::enforce_route_policy_for_test(auth, req, next)
                                .await
                        }
                    }
                }))
                .with_state(state);

            for uri in [
                "/health",
                "/version",
                "/config",
                "/llms.txt",
                "/.well-known/mindex.json",
            ] {
                let (status, _) = call(&app, uri, None).await;
                assert!(
                    status.is_success(),
                    "{uri} demanded a credential; a probe that needs one reports the \
                     credential's health, not the server's"
                );
            }
        }

        /// The Swagger UI and the raw spec are `merge`d rather than routed, so
        /// they are absent from `ROUTE_POLICY` — and the default-deny layer would
        /// refuse them as unconfigured routes, logging a build defect on every
        /// visit to the documentation. `PUBLIC_PATH_PREFIXES` is what stops that,
        /// and this is what stops somebody deleting it.
        #[test]
        fn the_specification_paths_are_public_by_prefix() {
            for path in [
                "/swagger-ui",
                "/swagger-ui/index.html",
                "/api-docs/openapi.json",
            ] {
                assert!(
                    crate::backend::http3::PUBLIC_PATH_PREFIXES
                        .iter()
                        .any(|p| path.starts_with(p)),
                    "{path} would be refused as an unconfigured route"
                );
            }
            // And the exemption must stay narrow: a prefix that swallowed the
            // data plane would be the whole feature undone by one string.
            for path in ["/projects", "/v0/x/search", "/status", "/metrics", "/gc"] {
                assert!(
                    !crate::backend::http3::PUBLIC_PATH_PREFIXES
                        .iter()
                        .any(|p| path.starts_with(p)),
                    "{path} is exempted from authorization by a public prefix"
                );
            }
        }

        /// A token whose `prj` is empty reaches nothing. The alternative reading
        /// — "unrestricted" — is what turns a minter's omitted argument into
        /// full access, so it is pinned on the wire rather than only in the
        /// claim type.
        #[tokio::test]
        async fn an_empty_project_claim_reaches_no_project_at_all() {
            let guid = uuid::Uuid::new_v4();
            let app = app(true, &[guid]).await;
            let t = token_for(&[], &[Action::Search]);

            let (status, _) = call(&app, &format!("/projects/{guid}"), Some(&t)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            let (_, body) = call(&app, "/projects", Some(&t)).await;
            assert!(
                !body.contains(&guid.simple().to_string()),
                "an empty claim listed a project: {body}"
            );
        }

        /// Nothing about the credential may travel back to the caller. A token
        /// echoed into a `detail`, a key id named in a refusal, a subject
        /// reflected — each is a small leak that reads as helpfulness.
        #[tokio::test]
        async fn no_refusal_ever_echoes_the_credential() {
            let app = app(true, &[]).await;
            let guid = uuid::Uuid::new_v4();
            let secret_ish = "test";

            let forged = mint(
                &Keyring::from_secret("rotated-out", vec![3u8; 32]),
                "somebody",
                vec!["*".into()],
                vec![Action::Search],
                1,
            )
            .unwrap()
            .0;

            for token in [Some(forged.as_str()), None] {
                for uri in [
                    format!("/projects/{guid}"),
                    "/projects".into(),
                    "/status".into(),
                ] {
                    let (_, body) = call(&app, &uri, token).await;
                    assert!(
                        !body.contains("rotated-out"),
                        "a refusal named the key id: {body}"
                    );
                    assert!(
                        !body.contains("somebody"),
                        "a refusal echoed the token's subject: {body}"
                    );
                    assert!(
                        !body.contains(secret_ish) || !body.contains("kid"),
                        "a refusal leaked key material: {body}"
                    );
                    if let Some(t) = token {
                        assert!(!body.contains(t), "a refusal echoed the token: {body}");
                    }
                }
            }
        }

        /// An expired token is refused everywhere, and with its own code — the
        /// one token failure whose remedy is obvious and whose distinguishability
        /// leaks nothing, since the holder already proved it held a valid
        /// signature.
        #[tokio::test]
        async fn an_expired_token_is_refused_on_every_route() {
            let app = app(true, &[]).await;
            let ring = Keyring::from_secret("test", SECRET.to_vec());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let claims = crate::backend::auth::Claims {
                iss: "mindex".into(),
                sub: "t".into(),
                jti: "j".into(),
                iat: now - 7200,
                nbf: now - 7200,
                exp: Some(now - 3600),
                prj: vec!["*".into()],
                act: Action::ALL.to_vec(),
                aud: vec![],
            };
            let expired = crate::backend::auth::sign(&ring, &claims).unwrap();

            for uri in [
                format!("/projects/{}", uuid::Uuid::new_v4()),
                "/projects".into(),
                "/status".into(),
            ] {
                let (status, body) = call(&app, &uri, Some(&expired)).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {body}");
                assert!(body.contains("auth.token_expired"), "{uri}: {body}");
            }
        }

        /// A token for the right project with the wrong action must not leak the
        /// project's *existence* either way round — both answers are already
        /// pinned above, so this pins the pair against each other: they must not
        /// become distinguishable by status alone as the code evolves.
        #[tokio::test]
        async fn a_wrong_action_and_a_wrong_project_stay_different_answers() {
            let mine = uuid::Uuid::new_v4();
            let app = app(true, &[mine]).await;

            let wrong_action = token_for(&[&mine.to_string()], &[Action::Delete]);
            let (a, _) = call(&app, &format!("/projects/{mine}"), Some(&wrong_action)).await;

            let wrong_project = token_for(&[&uuid::Uuid::new_v4().to_string()], &[Action::Search]);
            let (b, _) = call(&app, &format!("/projects/{mine}"), Some(&wrong_project)).await;

            assert_eq!(a, StatusCode::FORBIDDEN);
            assert_eq!(b, StatusCode::NOT_FOUND);
            assert_ne!(
                a, b,
                "these are different conditions and must stay so: 403 says `your token is too \
                 narrow`, 404 says nothing at all"
            );
        }

        /// With `[auth].enabled` off — every deployment that has not opted in —
        /// the request path is what it always was, and a client-supplied
        /// `Authorization` header decides nothing. Both halves are one claim, so
        /// they are one test: a header that is *sometimes* honoured is worse
        /// than one that never is.
        #[tokio::test]
        async fn authorization_off_ignores_the_header_entirely() {
            let guid = uuid::Uuid::new_v4();
            let app = app(false, &[guid]).await;

            let (bare_status, bare) = call(&app, "/projects", None).await;
            for header in [
                token_for(&["*"], &[Action::Admin]),
                "not-a-token".to_string(),
                String::new(),
            ] {
                let (status, body) = call(&app, "/projects", Some(&header)).await;
                assert_eq!(status, bare_status, "a header changed the status");
                assert_eq!(body, bare, "a header changed the body");
            }
            assert!(
                bare.contains(&guid.simple().to_string()),
                "the unfiltered listing lost a project: {bare}"
            );
        }

        // ── Minting over HTTP ────────────────────────────────────────────────
        //
        // `may_mint` has its own exhaustive table in `auth.rs`, over every axis
        // and every action. These tests exist because that table proves nothing
        // about the *endpoint*: the handler builds a `Claims` from a JSON body,
        // caps the lifetime, chooses a key and only then consults the rule, and
        // any of those four steps could hand `may_mint` something other than the
        // token it is about to sign. What is checked here is therefore the seam,
        // not the rule — including, in the first test, that the token which comes
        // back out actually carries the narrowed scope rather than the requested
        // one.

        async fn post_json(
            app: &axum::Router,
            uri: &str,
            token: &str,
            body: serde_json::Value,
        ) -> (StatusCode, String) {
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }

        /// The happy path, and the only assertion that matters about it: the
        /// token handed back verifies, and its scope is the one that was asked
        /// for rather than the minter's.
        #[tokio::test]
        async fn a_minted_token_comes_back_verifiable_and_narrowed() {
            let project = uuid::Uuid::new_v4();
            let app = full_router().await;
            let minter = token_for(
                &[&project.to_string()],
                &[
                    Action::Mint,
                    Action::Search,
                    Action::Research,
                    Action::Delete,
                ],
            );

            let (status, body) = post_json(
                &app,
                "/auth/tokens",
                &minter,
                serde_json::json!({
                    "sub": "agent:review",
                    "projects": [project.to_string()],
                    "actions": ["search", "research"],
                    "days": 1,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");

            let issued: serde_json::Value = serde_json::from_str(&body).expect("json");
            let claims = crate::backend::auth::verify(
                &crate::backend::auth::Keyring::from_secret("test", SECRET.to_vec()),
                issued["token"].as_str().expect("a token"),
                60,
            )
            .expect("the issued token must verify against this server's key");

            assert!(claims.covers(&project));
            assert!(claims.permits(Action::Search) && claims.permits(Action::Research));
            for dropped in [Action::Delete, Action::Mint, Action::Admin, Action::Index] {
                assert!(
                    !claims.permits(dropped),
                    "the issued token gained {dropped}, which was not requested"
                );
            }
        }

        /// Every way of asking for more than the minter holds, refused over the
        /// wire. The pure rule is table-driven elsewhere; what this pins is that
        /// none of these reach a 200 through the handler's own construction of
        /// the claims — a bug that would be invisible to a unit test of
        /// `may_mint`, because `may_mint` would never be handed the wider claim.
        #[tokio::test]
        async fn the_endpoint_refuses_every_way_of_exceeding_the_minter() {
            let mine = uuid::Uuid::new_v4();
            let other = uuid::Uuid::new_v4();
            let app = full_router().await;
            // One day, so a request for more days is a request for a later expiry.
            let minter = token_for(&[&mine.to_string()], &[Action::Mint, Action::Search]);

            for (name, body) in [
                (
                    "a wider action",
                    serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                       "actions": ["admin"], "days": 1}),
                ),
                (
                    "an action alongside held ones",
                    serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                       "actions": ["search", "delete"], "days": 1}),
                ),
                (
                    "a project the minter does not hold",
                    serde_json::json!({"sub": "x", "projects": [other.to_string()],
                                       "actions": ["search"], "days": 1}),
                ),
                (
                    "the wildcard, from a named minter",
                    serde_json::json!({"sub": "x", "projects": ["*"],
                                       "actions": ["search"], "days": 1}),
                ),
                (
                    "a later expiry",
                    serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                       "actions": ["search"], "days": 30}),
                ),
                (
                    // The eternal token is refused before the containment rule is
                    // even consulted, and it must stay refused for both reasons:
                    // over the network there is no such thing.
                    "a token that never expires",
                    serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                       "actions": ["search"], "days": 0}),
                ),
            ] {
                let (status, out) = post_json(&app, "/auth/tokens", &minter, body).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{name} was minted: {out}");
                assert!(
                    !out.contains("eyJ"),
                    "{name} was refused but the response carried a token: {out}"
                );
            }
        }

        /// A token that cannot mint is refused by the *extractor*, before the
        /// body is read — which is why this is a 403 naming the action rather
        /// than a 400 about containment.
        #[tokio::test]
        async fn a_token_without_mint_cannot_reach_the_minting_endpoint() {
            let mine = uuid::Uuid::new_v4();
            let app = full_router().await;
            let no_mint = token_for(
                &[&mine.to_string()],
                &[
                    Action::Search,
                    Action::Research,
                    Action::Index,
                    Action::Delete,
                ],
            );

            let (status, body) = post_json(
                &app,
                "/auth/tokens",
                &no_mint,
                serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                   "actions": ["search"], "days": 1}),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert!(
                body.contains("auth.action_not_permitted") && body.contains("mint"),
                "the refusal must name the action, got: {body}"
            );
        }

        /// The write actions are mintable over the network, and that is a
        /// decision rather than an oversight.
        ///
        /// The alternative — a hard-coded read-only vocabulary at this endpoint —
        /// does not prevent a write token existing; it moves the minting to a
        /// shell on the server's host, where what gets issued is usually *wider*
        /// than what was asked for here. What keeps this safe is that the request
        /// is contained by the minting token, which the tests above establish
        /// exhaustively. So a minter holding `index` may pass it on, and one that
        /// does not may not — the same rule as every other action, with no
        /// special case for the dangerous-sounding ones.
        #[tokio::test]
        async fn a_write_action_is_mintable_exactly_when_the_minter_holds_it() {
            let mine = uuid::Uuid::new_v4();
            let app = full_router().await;

            for action in [Action::Index, Action::Delete] {
                let body = serde_json::json!({
                    "sub": "agent:writer",
                    "projects": [mine.to_string()],
                    "actions": ["search", action.as_str()],
                    "days": 1,
                });

                let holder = token_for(
                    &[&mine.to_string()],
                    &[Action::Mint, Action::Search, action],
                );
                let (status, out) = post_json(&app, "/auth/tokens", &holder, body.clone()).await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{action} was refused a holder: {out}"
                );
                let issued: serde_json::Value = serde_json::from_str(&out).unwrap();
                let granted = issued["actions"].as_array().unwrap();
                assert!(
                    granted.iter().any(|a| a == action.as_str()),
                    "{action} was asked for and not granted: {out}"
                );

                // And the same request from a minter without it is refused — so
                // the grant above came from the minter's own scope and not from
                // the endpoint being permissive about writes.
                let reader = token_for(&[&mine.to_string()], &[Action::Mint, Action::Search]);
                let (status, out) = post_json(&app, "/auth/tokens", &reader, body).await;
                assert_eq!(
                    status,
                    StatusCode::BAD_REQUEST,
                    "a read-only minter issued {action}: {out}"
                );
            }
        }

        /// The audience rides through the endpoint and is echoed back — the echo
        /// is what a client renders, and a client that displayed its *request*
        /// would report a label the token does not carry.
        #[tokio::test]
        async fn the_audience_is_carried_into_the_token_and_echoed_back() {
            let mine = uuid::Uuid::new_v4();
            let app = full_router().await;
            let minter = token_for(&[&mine.to_string()], &[Action::Mint, Action::Search]);

            let (status, out) = post_json(
                &app,
                "/auth/tokens",
                &minter,
                serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                   "actions": ["search"], "audiences": ["agent", "cli"],
                                   "days": 1}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{out}");
            let issued: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                issued["audiences"],
                serde_json::json!(["cli", "agent"].iter().collect::<Vec<_>>()).clone(),
                "the echo must be the normalized list: {out}"
            );

            // And it is really in the token, not only in the envelope.
            let ring = Keyring::from_secret("test", SECRET.to_vec());
            let claims = crate::backend::auth::verify(&ring, issued["token"].as_str().unwrap(), 60)
                .expect("verifies");
            assert_eq!(
                claims.aud,
                vec![
                    crate::backend::auth::Audience::Cli,
                    crate::backend::auth::Audience::Agent
                ]
            );
        }

        /// An unlabelled request must produce an unlabelled token rather than one
        /// nobody may use: this is the ordinary shape, and reading the omitted
        /// field as "no audience" would break every client at once.
        #[tokio::test]
        async fn omitting_the_audience_mints_a_token_every_client_accepts() {
            let mine = uuid::Uuid::new_v4();
            let app = full_router().await;
            let minter = token_for(&[&mine.to_string()], &[Action::Mint, Action::Search]);

            let (status, out) = post_json(
                &app,
                "/auth/tokens",
                &minter,
                serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                   "actions": ["search"], "days": 1}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{out}");
            let issued: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(issued["audiences"], serde_json::json!([]));
        }

        /// A typo'd audience must fail loudly. Silently dropping it would mint a
        /// token that every client accepts while its holder believes it is
        /// labelled — the failure being invisible is the whole problem.
        #[tokio::test]
        async fn an_unknown_audience_is_refused_rather_than_dropped() {
            let mine = uuid::Uuid::new_v4();
            let app = full_router().await;
            let minter = token_for(&[&mine.to_string()], &[Action::Mint, Action::Search]);

            for bad in ["ai", "vs-code", "VSCODE", "*"] {
                let (status, out) = post_json(
                    &app,
                    "/auth/tokens",
                    &minter,
                    serde_json::json!({"sub": "x", "projects": [mine.to_string()],
                                       "actions": ["search"], "audiences": [bad], "days": 1}),
                )
                .await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?} was minted: {out}");
                assert!(!out.contains("eyJ"), "{bad:?} produced a token: {out}");
            }
        }
    }
}
