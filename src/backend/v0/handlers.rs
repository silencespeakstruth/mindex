use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use super::models::IndexRequest;
use crate::backend::error::ApiError;
use crate::backend::error::ProblemDetails;
use crate::backend::extract::{ApiJson, ApiPath, ApiQuery};
use crate::backend::http3;
use crate::backend::http3::EmbeddingModel;
use crate::backend::http3::RouterState;
use crate::backend::v0::models::CallDirection;
use crate::backend::v0::models::CallSite;
use crate::backend::v0::models::CallersResponse;
use crate::backend::v0::models::CancelRequest;
use crate::backend::v0::models::CancelResponse;
use crate::backend::v0::models::ChunkExcerpt;
use crate::backend::v0::models::Code;
use crate::backend::v0::models::CommitSummary;
use crate::backend::v0::models::ConfigResponse;
use crate::backend::v0::models::DeleteFilesRequest;
use crate::backend::v0::models::DeleteFilesResponse;
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
use crate::backend::v0::models::IndexResponse;
use crate::backend::v0::models::LanguageStats;
use crate::backend::v0::models::ListFilesResponse;
use crate::backend::v0::models::OutlineResponse;
use crate::backend::v0::models::OutlineSymbol;
use crate::backend::v0::models::ProgrammingLanguage;
use crate::backend::v0::models::ProjectListResponse;
use crate::backend::v0::models::ProjectStats;
use crate::backend::v0::models::ProjectSummary;
use crate::backend::v0::models::ReadChunksResponse;
use crate::backend::v0::models::ResearchRequest;
use crate::backend::v0::models::RetryRequest;
use crate::backend::v0::models::RetryResponse;
use crate::backend::v0::models::SearchFilter;
use crate::backend::v0::models::SearchRequest;
use crate::backend::v0::models::SearchResponse;
use crate::backend::v0::models::SearchResult;
use crate::backend::v0::models::StatusResponse;
use crate::backend::v0::models::SymbolInfo;
use crate::backend::v0::models::SymbolRoleFilter;
use crate::backend::v0::models::SymbolsRequest;
use crate::backend::v0::models::SymbolsResponse;
use crate::backend::v0::models::UUIDv4;
use crate::backend::v0::models::VersionResponse;
use crate::backend::v0::models::{GrepMatch, GrepResponse};
use crate::backend::v0::models::{HistoryPruneQuery, HistoryPruneResponse, HistoryRequest};
use crate::backend::v0::models::{ResearchConfigInfo, ResearchEffortLadder, ResearchSamplingInfo};
use crate::backend::v0::validate;
use crate::db::files::set_file_status;
use crate::db::qdrant::SearchHit;
use crate::db::qdrant::VectorStore;
use crate::db::qdrant::collection_for;
use crate::db::sqlite3::SQLite3Pool;
use crate::db::sqlite3::SQLite3PoolError;
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
                  role, start_line, end_line, start_column,
                  end_column, parent_name, parent_kind, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                project_guid,
                model_id,
                path,
                s.name,
                s.kind,
                s.role.as_str(),
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
    /// pass (`embed_batch` chunks per `/encode`).
    async fn embed_all(&self, chunks: &[(UUIDv4, String)]) -> Result<(), EmbedUpsertError> {
        embed_and_upsert(
            self.embedder,
            self.store,
            self.collection,
            chunks,
            self.token,
            self.embed_tuning,
        )
        .await
    }

    /// Phase 3 for one file: mark it `indexed` and record the new sha256. The
    /// `AND status = 'indexing'` guard makes this a no-op (matching 0 rows, so no
    /// trigger fires) if a concurrent `POST /cancel` moved the file to `cancelled`
    /// since it was prepared — without it the raw `cancelled → indexed` UPDATE would
    /// trip the state-machine trigger and error the whole batch, leaving sibling
    /// files stuck in `indexing`.
    async fn mark_indexed(&self, path: &str, sha256: &str) -> Result<(), ApiError> {
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
                )?;
                Ok(())
            })
            .with_cancellation_token(self.token)
            .await
            .from_cancelled()
            .map_err(|err| {
                error!(error = ?err, "Failed to mark the file 'indexed' in SQLite.");
                ApiError::from(err)
            })?;
        Ok(())
    }

    /// Best-effort recovery: move the file to `status` (incrementing `retry_count`
    /// when `increment_retry`) on a cancellation/failure path.
    async fn recover(&self, path: &str, status: &'static str, increment_retry: bool) {
        set_file_status(
            self.db_pool,
            &self.project_guid.0.as_simple().to_string(),
            path,
            self.model_id,
            status,
            increment_retry,
            self.token.child_token(),
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
            .unwrap_or_default();

        if cancelled.is_empty() {
            return prepared;
        }

        for path in &cancelled {
            let (pg, p, m) = (project_guid, path.clone(), self.model_id.to_string());
            let _ = self
                .db_pool
                .transaction(self.token.child_token(), move |tx| {
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
                .with_cancellation_token(self.token)
                .await
                .from_cancelled();
            info!(%path, "Indexing cancelled mid-flight; skipping the embed pass for this file.");
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
/// **Concurrency:** safe. Each `(project, model, path)` is serialized by an in-process
/// claim — a second in-flight request for the *same* file is **skipped** (it is absent
/// from the response, like an unchanged file); different files proceed in parallel.
/// A concurrent `POST /cancel` is reconciled before the embed pass. On any failure
/// the whole batch is recovered to `failed`/`cancelled` and the retry worker re-attempts it.
#[utoipa::path(
    post,
    path = "/v0/{project_guid}/index",
    tag = "Indexing",
    params(("project_guid" = String, Path, description = "Project UUID (v4), 32-char simple or hyphenated form.")),
    request_body = IndexRequest,
    responses(
        (status = 200, description = "Per-file chunk counts for the files actually (re)indexed.", body = IndexResponse),
        (status = 400, description = "Validation failed (bad path, oversized file, too many files).", body = ProblemDetails),
        (status = 413, description = "The request body exceeded [server].max_body_mib.", body = ProblemDetails),
        (status = 499, description = "Client closed the connection; indexing was cancelled (nginx convention).", body = ProblemDetails),
        (status = 500, description = "SQLite, slicer, or Qdrant upsert failure; the batch was marked `failed` for the retry worker.", body = ProblemDetails),
        (status = 503, description = "The embedder is unreachable or returned persistent backpressure; the batch was marked `failed`.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_index(
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<IndexRequest>,
) -> Result<Json<IndexResponse>, ApiError> {
    let span = info_span!("indexing", project_guid = %project_guid.0);

    async move {
        validate::validate_index_request(&payload, s.max_files_per_request, s.max_code_bytes)?;

        let guard = http3::CancellationGuard(CancellationToken::new());

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
                .transaction(guard.0.child_token(), move |tx| {
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
                .with_cancellation_token(&guard.0)
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
            token: &guard.0,
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
                            res.files.entry(pl).or_default().insert(path.clone(), n);
                        }
                        // Up to date, or stale/not-indexed (needs a full pass instead).
                        Ok(None) => {}
                        // Another in-flight request holds the claim; skip it so the rest
                        // of the batch proceeds, exactly as the full path does.
                        Err(ApiError::FileInFlight) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            return Ok(Json(res));
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
                        prepared.push(p);
                    }
                    Ok(None) => file_outcome(pl, "skipped_unchanged"),
                    // Another in-flight request holds the claim for this file; skip it
                    // so the rest of the batch proceeds. Innocent co-batched files must
                    // not pay a retry_count penalty for an unrelated file's contention.
                    Err(ApiError::FileInFlight) => {
                        // Counted here because it is counted nowhere else: the error
                        // is swallowed and the request still 200s, so the HTTP
                        // middleware can never see this.
                        m.index.claim_conflicts.inc();
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
        let mut prepared = indexer.drop_cancelled(prepared).await;

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
        let embed_result = indexer.embed_all(&all_chunks).await;
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
            indexer.mark_indexed(&p.path, &p.sha256).await?;
            file_outcome(p.pl, "indexed");
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
        Ok(Json(res))
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
    State(s): State<RouterState>,
    ApiJson(payload): ApiJson<DriftRequest>,
) -> Result<Json<DriftResponse>, ApiError> {
    validate::validate_drift_request(&payload, s.max_drift_files)?;
    let guard = http3::CancellationGuard(CancellationToken::new());
    let (indexed, in_flight) = read_drift_baseline(&s, &guard.0, project_guid).await?;
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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

    // Guarantee the response is sorted by score (descending), independent of the
    // order Qdrant's fusion/rerank happens to return.
    results.sort_by(|a, b| b.score.total_cmp(&a.score));

    Ok(results)
}

/// `limit` used when a `/symbols` request omits it. Not configurable: it is a
/// response-shape default the client can always override (up to
/// `[limits].max_symbol_results`), not a tuning knob.
const DEFAULT_SYMBOL_LIMIT: usize = 20;

/// The per-role `/symbols` lookup SQL + binds. Bind ?4 is a placeholder for the
/// role, rewritten by the caller per executed query (`'definition'`/`'reference'`).
/// Ranking with an `anchor_path` is purely path-based and deterministic: same file
/// → 0, same directory (exact — not a deeper subtree) → 1, everything else → 2;
/// ties break by `path ASC, start_line ASC`. `COUNT(*) OVER ()` carries the full
/// per-role total past the `LIMIT`.
/// The selector, when present, is appended as a `file_path IN (…)` subquery whose
/// binds land **last**. That placement is load-bearing: `symbols_core` rewrites the
/// role bind by Vec index (`binds[3]`), so inserting anything before it would silently
/// query the wrong role.
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
         WHERE project_guid = ?1 AND model_id = ?2 AND name = ?3 AND role = ?4",
    );
    let mut binds: Vec<Bind> = vec![
        Bind::Guid(project_guid),
        Bind::Path(model_id.to_string()),
        Bind::Path(req.name.clone()),
        Bind::Path(String::new()), // role placeholder (see doc comment)
    ];
    let mut next = 5;
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
/// Symbols (definitions + references, with kinds, enclosing definition and doc
/// comments) are extracted at indexing time from the language's upstream
/// tree-sitter tags query — purely syntactic, no type resolution. The response is
/// therefore **candidate lists, never a single answer**: an exact name can
/// legitimately have several definitions (same name in different modules,
/// overloads); `total_definitions`/`total_references` always carry the full
/// counts so a truncated list is visible to the caller, who disambiguates.
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
/// per-role candidate query + assembly. Validation stays with the callers.
pub(crate) async fn symbols_core(
    s: &RouterState,
    project_guid: UUIDv4,
    req: &SymbolsRequest,
    token: &CancellationToken,
) -> Result<SymbolsResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let limit = req.limit.unwrap_or(DEFAULT_SYMBOL_LIMIT);

    let (sql, mut binds) = build_symbols_query(project_guid, model_id, req, limit);

    let roles: Vec<&'static str> = match req.role {
        Some(SymbolRoleFilter::Definition) => vec!["definition"],
        Some(SymbolRoleFilter::Reference) => vec!["reference"],
        None => vec!["definition", "reference"],
    };

    // What the selector hid, per role. Only when there *is* a selector, and asked
    // unscoped on purpose: the point is the difference, and a scoped total cannot
    // report what it excluded. Without this, a run scoped to `docs/**` looking up a
    // name defined in `src/` reads exactly like a name that does not exist — and
    // `/symbols` calls that answer definitive.
    let scoped = req.include.is_some() || req.exclude.is_some();
    let mut unscoped_req = SymbolsRequest {
        name: req.name.clone(),
        role: req.role,
        kind: req.kind.clone(),
        anchor_path: req.anchor_path.clone(),
        limit: req.limit,
        include: None,
        exclude: None,
    };
    unscoped_req.anchor_path = None;
    let count_sql = scoped.then(|| {
        let (q, b) = build_symbols_query(project_guid, model_id, &unscoped_req, limit);
        // Only the totals matter here, and `COUNT(*) OVER ()` already carries them
        // past the LIMIT — so the same builder answers this with no second SQL to
        // keep in step.
        (q, b)
    });

    type SymbolRows = (Vec<SymbolInfo>, u64, u64);
    let per_role: Vec<(&'static str, SymbolRows)> = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            let mut out = Vec::with_capacity(roles.len());
            let mut stmt = tx.prepare(&sql)?;
            let mut count_stmt = match &count_sql {
                Some((q, _)) => Some(tx.prepare(q)?),
                None => None,
            };
            for role in roles {
                binds[3] = Bind::Path(role.to_string());
                let unscoped_total = match (&mut count_stmt, &count_sql) {
                    (Some(stmt), Some((_, cb))) => {
                        let mut cb = cb.clone();
                        cb[3] = Bind::Path(role.to_string());
                        stmt.query_map(params_from_iter(cb.iter()), |r| {
                            r.get::<_, i64>(9).map(|n| n as u64)
                        })?
                        .next()
                        .transpose()?
                        .unwrap_or(0)
                    }
                    _ => 0,
                };
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
                out.push((role, (rows, total, unscoped_total.saturating_sub(total))));
            }
            Ok(out)
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

    let mut resp = SymbolsResponse {
        definitions: Vec::new(),
        references: Vec::new(),
        total_definitions: 0,
        total_references: 0,
        out_of_scope_definitions: 0,
        out_of_scope_references: 0,
    };
    for (role, (rows, total, hidden)) in per_role {
        if role == "definition" {
            (
                resp.definitions,
                resp.total_definitions,
                resp.out_of_scope_definitions,
            ) = (rows, total, hidden);
        } else {
            (
                resp.references,
                resp.total_references,
                resp.out_of_scope_references,
            ) = (rows, total, hidden);
        }
    }
    Ok(resp)
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
        .await
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
                   AND role = 'definition'
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
        .await
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

/// Call sites per `callers` call. Between `outline`'s 300 and `read_chunks`'s 8:
/// the rows are compact metadata and the point is to see the *shape* of a name's
/// usage, but unlike an outline this is not bounded by one file — a common name
/// can span the whole repo, and this text is resent every later turn. Truncation
/// stays visible through `total_sites`.
const CALLERS_LIMIT: usize = 50;

/// The grouped call-edge SQL for one direction. Binds are `?1` project, `?2`
/// model, `?3` the name.
///
/// Pulled out of `callers_core` to be testable on its own (as `build_search_query`
/// and `build_symbols_query` are): the risky parts are the direction swap and the
/// windows over an aggregate, neither of which is visible from the response type.
///
/// The two directions differ only in which column *selects* rows and which pair
/// names the far end of the edge — `In` filters on `name` and reports the
/// enclosing definition, `Out` filters on `parent_name` and reports what was
/// referenced. Both column sets are literals chosen here, never caller input, so
/// the interpolation carries no injection surface.
/// `scope_clause` is an already-built `AND file_path IN (…)` fragment (empty when the
/// run is unscoped), so a scoped run cannot read call sites it was not given.
fn build_callers_query(direction: CallDirection, scope_clause: &str) -> String {
    let (filter_column, symbol_column, kind_column) = match direction {
        CallDirection::In => ("name", "parent_name", "parent_kind"),
        CallDirection::Out => ("parent_name", "name", "kind"),
    };
    // `COUNT(*) OVER ()` counts *groups* here — windows are applied after
    // grouping — so summing the per-group counts is what recovers the row total.
    format!(
        "SELECT file_path, {symbol_column}, {kind_column},
                MIN(start_line) AS first_line, COUNT(*) AS occurrences,
                COUNT(*) OVER () AS total_sites,
                SUM(COUNT(*)) OVER () AS total_references
         FROM project_file_symbols
         WHERE project_guid = ?1 AND model_id = ?2 AND role = 'reference'
           AND {filter_column} = ?3{scope_clause}
         GROUP BY file_path, {symbol_column}, {kind_column}
         ORDER BY file_path ASC, first_line ASC
         LIMIT {CALLERS_LIMIT}"
    )
}

/// The approximate call graph around one exact name — `parent_name`, read as an
/// edge.
///
/// A reference row already knows the definition it sits inside, so "who calls X"
/// is one indexed `SELECT` over data the symbol table already holds. The edges
/// are **lexical**: a reference records that a token appeared in a call position,
/// never which definition it binds to, so results for a common name mix unrelated
/// definitions and an aliased import breaks the edge entirely. That imprecision is
/// deliberate and reported rather than hidden — the alternative is a resolution
/// layer (LSP/SCIP) that needs each project to build on the indexing host, which
/// buys accuracy for some users by making quality silently vary between them.
///
/// Pure SQL, like `outline_core`/`list_files_core`: no embedder, no Qdrant. `In`
/// is served by `idx_project_file_symbols_lookup`, `Out` by
/// `idx_project_file_symbols_parent`.
pub(crate) async fn callers_core(
    s: &RouterState,
    project_guid: UUIDv4,
    name: &str,
    direction: CallDirection,
    scope: &crate::research::ToolScope,
    token: &CancellationToken,
) -> Result<CallersResponse, ApiError> {
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    let model_id = model_id.clone();
    let owned_name = name.to_string();
    // The scope's binds go after the query's own three, so the fixed `?1..?3` above
    // stay put.
    let (scope_clause, scope_binds) = if scope.is_scoped() {
        let (sql, binds) = scope_subquery(project_guid, scope, 4);
        (format!(" AND file_path IN ({sql})"), binds)
    } else {
        (String::new(), Vec::new())
    };
    let sql = build_callers_query(direction, &scope_clause);
    // Unscoped, for the difference only — see `symbols_core`.
    let unscoped_sql = scope
        .is_scoped()
        .then(|| build_callers_query(direction, ""));
    let mut binds: Vec<Bind> = vec![
        Bind::Guid(project_guid),
        Bind::Path(model_id.clone()),
        Bind::Path(owned_name.clone()),
    ];
    let unscoped_binds = binds.clone();
    binds.extend(scope_binds);

    let (defined, rows, all_sites): (bool, Vec<(CallSite, u64, u64)>, u64) = s
        .db_pool
        .transaction(token.child_token(), move |tx| {
            // Two reads, as in `outline_core`: an unknown identifier and one that
            // is defined but never referenced are different answers, and a single
            // query cannot tell them apart.
            let defined: bool = tx.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM project_file_symbols
                     WHERE project_guid = ?1 AND model_id = ?2 AND name = ?3
                       AND role = 'definition'
                 )",
                (&project_guid, &model_id, &owned_name),
                |r| r.get(0),
            )?;

            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(binds.iter()), |r| {
                    Ok((
                        CallSite {
                            path: r.get(0)?,
                            symbol: r.get(1)?,
                            kind: r.get(2)?,
                            first_line: r.get::<_, i64>(3)? as usize,
                            occurrences: r.get::<_, i64>(4)? as u64,
                        },
                        r.get::<_, i64>(5)? as u64,
                        r.get::<_, i64>(6)? as u64,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let scoped_sites = rows
                .first()
                .map_or(0, |(_, t, _): &(CallSite, u64, u64)| *t);
            let all_sites = match &unscoped_sql {
                Some(q) => {
                    let mut stmt = tx.prepare(q)?;
                    stmt.query_map(params_from_iter(unscoped_binds.iter()), |r| {
                        r.get::<_, i64>(5).map(|n| n as u64)
                    })?
                    .next()
                    .transpose()?
                    .unwrap_or(0)
                }
                None => scoped_sites,
            };
            Ok((defined, rows, all_sites))
        })
        .await
        .map_err(|e| {
            error!(
                error = %e,
                project_guid = %project_guid.0,
                name = %name,
                "Failed to read the call graph from SQLite."
            );
            ApiError::from(e)
        })?;

    let total_sites = rows.first().map_or(0, |(_, t, _)| *t);
    let total_references = rows.first().map_or(0, |(_, _, t)| *t);
    Ok(CallersResponse {
        name: name.to_string(),
        direction,
        defined,
        out_of_scope_sites: all_sites.saturating_sub(total_sites),
        sites: rows.into_iter().map(|(site, _, _)| site).collect(),
        total_sites,
        total_references,
    })
}

/// Chunks per `read_chunks` call. Small: this returns *code*, which is resent on
/// every later turn, so it is the one research lookup with a real context price.
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
pub(crate) async fn read_chunks_core(
    s: &RouterState,
    project_guid: UUIDv4,
    path: &str,
    start_line: usize,
    end_line: usize,
    scope: &crate::research::ToolScope,
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
                 LIMIT {READ_CHUNKS_LIMIT}"
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
        .await
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

/// Commits per `file_history` call.
///
/// Small where `outline`'s is large, and for the opposite reason: an outline is a
/// list of names and this is a list of prose. Twenty commit messages of this
/// repository's median length is already ~5k tokens of transcript, and the recent
/// ones are what answer "why is this the way it is" — a longer tail buys
/// archaeology nobody asked for at the price of the budget that would have read
/// the code. Transcript shape, not tuning, so it is a const like `GREP_LIMIT`.
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
            read_file_history(tx, project_guid, &model_id, &owned_path)
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
            params![
                project_guid,
                model_id,
                owned_path,
                FILE_HISTORY_LIMIT as i64
            ],
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
        .await
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

/// Matches per `grep` call. Tighter than `list_files`'s 300 because each carries a
/// line of source rather than a path, and every hit is resent on every later turn.
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
         LIMIT {GREP_LIMIT}"
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
        Bind::Path(model_id),
        Bind::Path(format!("%{}%", like_escape(pattern))),
    ];

    let owned_pattern = pattern.to_string();
    let (rows, total, all): (Vec<GrepMatch>, u64, u64) = s
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
            Ok((matches, total, all))
        })
        .await
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
    let line_index = code[..offset].matches('\n').count();
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
        .await
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

/// Production [`ResearchTools`]: the research loop's index lookups are direct
/// internal calls to the `/search` and `/symbols` cores — no HTTP back to self.
struct StateResearchTools {
    state: RouterState,
    project_guid: UUIDv4,
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
    async fn record(&self, record: crate::research::RunRecord) {
        // A fresh token: the request's own is cancelled the moment the client
        // disconnects, and a run that completed still deserves its record —
        // "the client stopped reading" is not "this never happened".
        crate::db::research::insert_run(
            &self.db_pool,
            self.context.clone(),
            record,
            CancellationToken::new(),
        )
        .await;
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

    async fn callers(
        &self,
        name: String,
        direction: CallDirection,
        scope: &crate::research::ToolScope,
        token: &CancellationToken,
    ) -> Result<CallersResponse, ApiError> {
        callers_core(
            &self.state,
            self.project_guid,
            &name,
            direction,
            scope,
            token,
        )
        .await
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
        file_history_core(&self.state, self.project_guid, &path, scope, token).await
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
            token,
        )
        .await
    }

    async fn file_versions(
        &self,
        paths: Vec<String>,
        token: &CancellationToken,
    ) -> Result<Vec<crate::research::FileVersion>, ApiError> {
        file_versions_core(&self.state, self.project_guid, paths, token).await
    }
}

/// The SSE body of one research job. Owns the event receiver and the job's
/// cancellation token; **dropping the stream cancels the job** — that is the
/// whole cancellation contract (a client disconnect makes axum drop the body).
///
/// The semaphore permit deliberately does **not** ride here. The job is spawned
/// detached, so a permit held by the stream would be released the instant a client
/// disconnected while the job kept running to its next cancellation point — briefly
/// over-admitting past `max_concurrent`, which matters now that a run may be granted
/// an hour. The permit lives in the spawned future instead, so a slot frees when the
/// work stops rather than when the reader leaves.
struct ResearchEventStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<crate::research::ResearchEvent>,
    token: CancellationToken,
}

impl Drop for ResearchEventStream {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

impl futures_core::Stream for ResearchEventStream {
    type Item = Result<axum::response::sse::Event, std::convert::Infallible>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx).map(|opt| {
            opt.map(|e| {
                Ok(axum::response::sse::Event::default()
                    .event(e.name())
                    .data(e.data().to_string()))
            })
        })
    }
}

/// Iterative code research driven by a local Ollama model, streamed as SSE.
///
/// A long-lived, one-way stream: the server runs a research loop in which the
/// configured (or request-named) Ollama model asks the index one question per
/// turn — internal lookups against the index cores, **every one of them scoped by
/// the request's `include`/`exclude`** — then must write a final report. The scope is
/// enforced on all nine tools, not only on retrieval: a path outside it is refused by
/// name, and name-keyed lookups drop the rows it hides and report how many. So a
/// scoped run cannot read its way out of its scope, and its report can only speak
/// about what it was given. Events (`text/event-stream`, named events with JSON
/// `data`):
///
/// - `thinking` `{text}` — deltas of the model's thinking (thinking models only);
/// - `step` `{n, action, <arg>, hits}` — one executed tool call. `action` is
///   `search`, `grep`, `symbols`, `outline`, `callers`, `list_files`, `read_chunks`,
///   `note` or `revise_plan`, and the argument key is named for what it is: `query`,
///   `pattern`, `name`, `path`, `name`, `glob`, `path`, `text` and `plan`
///   respectively — exactly one is present per step. `note` and `revise_plan` write
///   to the run's own state rather than reading the index (the model's reasoning is
///   discarded between turns, so a scratchpad is the only way a conclusion survives
///   one), and cost a step like any other call;
/// - `progress` `{steps, max_steps, elapsed_ms, max_ms, tokens, max_tokens,
///   prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx, context_pct, turns,
///   binding}` — budget consumption, so a live run is steerable instead of opaque.
///   Emitted once before the first turn (all limits, nothing spent), then after
///   every executed step and every completed turn. `binding` is the axis closest to
///   exhaustion (`time`, `tokens`, `steps` or `context`) — i.e. what this run will
///   run out of. Not emitted on a timer: interpolate `elapsed_ms` locally between
///   events;
/// - `summary` `{text}` — the final Markdown report. Streamed as deltas when the
///   report was rewritten after its citation check; sent as one event otherwise,
///   because the first draft is withheld until that check has run;
/// - `citations` `{total, verified, path_only, unverified, unverified_paths, stale,
///   stale_paths, draft_unverified, draft_path_only, draft_stale,
///   revalidation_steps}` —
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
///   report that was right the first time from one that was repaired;
/// - `done` `{reason, prompt_version, …every `progress` field}` — completion
///   (closes the stream), carrying the run's final cost as well as
///   `steps`/`elapsed_ms`. `prompt_version` identifies the generation of the
///   server's research instructions that drove the run: reports written under
///   different prompts are not comparable, and nothing else on the stream says
///   which was in force.
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
/// cancels the research job** — that is the cancellation interface; there is no
/// cancel endpoint. Jobs run on a small dedicated runtime; when all
/// `[research].max_concurrent` slots are busy the request is rejected up front
/// with **429** `research.busy`.
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
        (status = 200, description = "SSE stream of research events (thinking/step/progress/summary/citations/done/error). `citations` reports the server's provenance check on the report's `path:start-end` references — `verified`/`path_only`/`unverified` counts plus the invented paths — scored against the locations this run's own tool calls returned, and its freshness check beside it: `stale`/`stale_paths` count the citations pointing into files the index rewrote or dropped while the run was reading (indexing is never blocked by research, so a verified citation can still describe replaced code). Its `draft_unverified`/`draft_path_only`/`draft_stale`/`revalidation_steps` fields are null unless the first draft failed those checks and was sent back for correction. `progress` reports budget consumption during the run (steps/time/tokens/context plus `binding`, the axis closest to exhaustion); `done` repeats those fields and adds a `reason` — `finalized`, or one of `time_exhausted`/`tokens_exhausted`/`budget_exhausted`/`context_exhausted`/`unparseable`/`repeated_calls` when the report was cut short — plus `prompt_version`, the generation of the server's research instructions that produced the report. `max_seconds` is a hard deadline enforced by cancellation, and the report phase has its own `[research].report_timeout_ms` on top, so the longest a caller waits is the sum of the two; a run stopped by its deadline still ships a report that says so. Every lookup the model makes is scoped by the request's `include`/`exclude`, on all nine tools — an out-of-scope path is refused by name rather than answered empty.", content_type = "text/event-stream"),
        (status = 400, description = "Validation failed (empty/oversized question, oversized selector, no model, out-of-range budget).", body = ProblemDetails),
        (status = 429, description = "All research slots are busy.", body = ProblemDetails),
    ),
)]
#[debug_handler]
pub async fn post_research(
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
        },
    )?;
    let model = match req.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ if !s.research_default_model.is_empty() => s.research_default_model.clone(),
        _ => return Err(ApiError::ResearchModelMissing),
    };

    let permit = s
        .research_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::ResearchBusy)?;

    let token = CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    let params = crate::research::ResearchParams {
        question: req.question,
        model,
        scope: crate::research::ToolScope {
            include: req.include,
            exclude: req.exclude,
        },
        budget: s.research_budget(req.effort, req.budget),
        sampling: s.research_sampling_for(req.seed),
        report_timeout_ms: s.research_report_timeout_ms,
        max_turn_thinking_chars: s.research_max_turn_thinking_chars,
        metrics: Some(s.metrics.clone()),
    };
    info!(
        project_guid = %project_guid.0,
        model = %params.model,
        prompt_version = crate::research::PROMPT_VERSION,
        effort = ?req.effort,
        seed = ?params.sampling.seed,
        // The resolved budget, not the requested effort: with `budget` overrides the
        // level alone no longer says what the run was granted.
        max_seconds = params.budget.max_seconds,
        max_tokens = params.budget.max_tokens,
        max_steps = params.budget.max_steps,
        "Starting a research job."
    );

    let ollama = s.research_ollama.clone();
    let effort = match req.effort {
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
            }),
            s.metrics.clone(),
        ));
    let journal: Arc<dyn crate::research::ResearchJournal> =
        Arc::new(crate::db::research::MeteredJournal::new(
            Arc::new(SqliteResearchJournal {
                db_pool: s.db_pool.clone(),
                context: crate::db::research::RunContext {
                    project_guid: project_guid.0.to_string(),
                    effort,
                    seed: params.sampling.seed,
                    temperature: params.sampling.temperature,
                    // Rendered by the same one renderer the model reads, so the
                    // journal and the prompt can never describe the scope differently.
                    scope_json: params.scope.is_scoped().then(|| params.scope.describe()),
                },
            }),
            s.metrics.clone(),
            effort,
        ));
    let job_token = token.clone();
    s.research_handle.spawn(async move {
        // The permit is held by the *work*, not by the reader: it is released when
        // this future unwinds, so an abandoned job cannot let a replacement in while
        // it is still spending GPU and DB time. See `ResearchEventStream`.
        let _permit = permit;
        crate::research::run_research(ollama, tools, journal, params, tx, job_token).await;
    });

    let stream = ResearchEventStream { rx, token };
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
/// rely on, and the same one idea scopes symbols, callers and grep.
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
/// scope filter can be appended to a query that already has binds of its own — and
/// numbering from the end is what keeps `symbols_core`'s by-index rewrite of the role
/// bind valid.
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
                error!(error = ?e, project_guid = %pg.0, "Failed to soft-delete files.");
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
                error!(error = ?e, project_guid = %pg.0, "Failed to cancel indexing files.");
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
    ApiPath(project_guid): ApiPath<UUIDv4>,
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
pub async fn get_status(State(s): State<RouterState>) -> Result<Json<StatusResponse>, ApiError> {
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
    let EmbeddingModel::BGEm3 { model_id, .. } = &s.model;
    // Cloned out and the guard dropped before anything else: the writer is a worker
    // on a tick, and this handler never holds the lock across an `.await`.
    let catalog = s.research_models.read().await.clone();
    Json(ConfigResponse {
        version: env!("CARGO_PKG_VERSION"),
        model_id: model_id.clone(),
        languages: ProgrammingLanguage::ALL.iter().map(|l| l.name()).collect(),
        embed_batch: s.embed_tuning.embed_batch,
        db_pool_size: s.db_pool_size,
        stuck_grace_mins: s.stuck_grace_mins,
        max_retries: s.max_retries,
        research: ResearchConfigInfo {
            default_model: s.research_default_model.clone(),
            models: catalog.models,
            models_refreshed_at: catalog.refreshed_at,
            effort: ResearchEffortLadder {
                low: (&s.research_effort.low).into(),
                medium: (&s.research_effort.medium).into(),
                high: (&s.research_effort.high).into(),
            },
            max_request_seconds: s.research_max_request_seconds,
            max_request_tokens: s.research_max_request_tokens,
            max_request_steps: s.research_max_request_steps,
            report_timeout_ms: s.research_report_timeout_ms,
            sampling: ResearchSamplingInfo {
                temperature: s.research_sampling.temperature,
                top_p: s.research_sampling.top_p,
                seed: s.research_sampling.seed,
            },
        },
    })
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
pub async fn post_gc(State(s): State<RouterState>) -> Result<Json<GcResponse>, ApiError> {
    let Some(_guard) = crate::worker::gc::GcGuard::try_acquire(&s.gc_flag) else {
        info!("POST /gc rejected: a garbage-collection pass is already running.");
        return Err(ApiError::GcRunning);
    };
    let cg = http3::CancellationGuard(CancellationToken::new());
    let (chunks_removed, files_removed, status_log_pruned) = crate::worker::gc::collect(
        &s.db_pool,
        &*s.qdrant,
        s.status_log_retention_days,
        &s.metrics,
        "manual",
        &cg.0,
    )
    .await;
    Ok(Json(GcResponse {
        chunks_removed,
        files_removed,
        status_log_pruned,
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
pub async fn get_metrics(State(s): State<RouterState>) -> Result<Response, ApiError> {
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

    let permits = s.research_semaphore.available_permits();
    m.state.research_permits_available.set(permits as i64);

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

/// `GET /health` — a *smart* readiness check: confirms both stores (SQLite +
/// Qdrant) and the embedder are reachable, pings the local Ollama behind
/// `/research`, and reports how many files are indexing globally. `status` is
/// `"ok"` only if the three *required* checks pass; Ollama is an **optional**
/// dependency, so `checks.ollama` is reported but never degrades the verdict —
/// without it only `/research` stops working. Each check is best-effort and
/// independent, so one dead dependency is pinpointed rather than collapsing the
/// whole response.
///
/// **Concurrency:** safe — read-only probes. Always returns **200** at the HTTP level;
/// inspect the `status` field (`"ok"` vs `"degraded"`) and per-dependency `checks`.
#[utoipa::path(
    get,
    path = "/health",
    tag = "Observability",
    responses((status = 200, description = "Dependency liveness; `status` is `ok` only if the three required checks pass (the optional `ollama` check never degrades it).", body = HealthResponse)),
)]
#[debug_handler]
pub async fn get_health(State(s): State<RouterState>) -> Json<HealthResponse> {
    let guard = http3::CancellationGuard(CancellationToken::new());

    // SQLite: the global indexing-file count doubles as the liveness query.
    let (sqlite, indexing_files) = match s
        .db_pool
        .transaction(guard.0.child_token(), |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM project_files WHERE status = 'indexing'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
    {
        Ok(n) => ("ok".to_string(), n),
        Err(e) => (format!("error: {e}"), -1),
    };

    let qdrant = match s.qdrant.health().await {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error: {e}"),
    };

    let EmbeddingModel::BGEm3 { client, .. } = &s.model;
    let embedder = match client.health().await {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error: {e:?}"),
    };
    // Pinged separately only when it *is* separate: a split deployment can have a
    // healthy indexer and a dead query instance, which would otherwise show as a
    // green health check and every search failing.
    let query_embedder = if Arc::ptr_eq(client, &s.query_model) {
        None
    } else {
        Some(match s.query_model.health().await {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("error: {e:?}"),
        })
    };

    // Optional dependency: only `/research` needs it, so its state is reported
    // but deliberately kept out of the `status` verdict below.
    let ollama = match s.research_ollama.health().await {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error: {e}"),
    };

    let status = if sqlite == "ok"
        && qdrant == "ok"
        && embedder == "ok"
        && query_embedder.as_deref().unwrap_or("ok") == "ok"
    {
        "ok"
    } else {
        "degraded"
    };

    Json(HealthResponse {
        status,
        version: env!("CARGO_PKG_VERSION"),
        indexing_files,
        checks: HealthChecks {
            sqlite,
            qdrant,
            embedder,
            query_embedder,
            ollama,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::v0::models::{
        ChangeType, CommitEntry, CommitPath, CommitSummary, GlobPattern, SearchFilter,
    };
    use glob::Pattern;
    use uuid::Uuid;

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
        set_file_status(
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
        set_file_status(
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

    // ── symbol lifecycle: a file's symbol rows always parallel its chunk set ──

    async fn insert_symbol(pool: &SQLite3Pool, path: &'static str, name: &'static str) {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO project_file_symbols
                     (project_guid, model_id, file_path, name, kind, role,
                      start_line, end_line, start_column, end_column)
                 VALUES (?1, ?2, ?3, ?4, 'function', 'definition', 1, 1, 0, 1)",
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
        assert!(
            names.iter().any(|n| n == "fresh") && names.iter().any(|n| n == "helper"),
            "fresh content's definitions and references must be inserted, got {names:?}"
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
        set_file_status(
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
        set_file_status(
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

    /// Executes `build_symbols_query` for one role, returning (paths, total).
    async fn run_symbols_query(
        pool: &SQLite3Pool,
        req: SymbolsRequest,
        limit: usize,
        role: &'static str,
    ) -> (Vec<String>, u64) {
        let (sql, mut binds) = build_symbols_query(guid(), MODEL, &req, limit);
        binds[3] = Bind::Path(role.to_string());
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
            role: None,
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

    /// `symbols_core` rewrites the role bind **by Vec index**, so anything appended to
    /// the query must land after it. This is the guard on that: a scope filter inserted
    /// earlier would silently query the wrong role.
    #[test]
    fn build_symbols_query_scopes_rows_without_disturbing_the_role_bind() {
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
            binds[3],
            Bind::Path(String::new()),
            "index 3 must still be the role placeholder: {binds:?}"
        );
    }

    /// An unscoped lookup must build byte-for-byte the SQL it always did, so the
    /// public `/symbols` endpoint provably did not change when scoping was added.
    #[test]
    fn an_unscoped_symbols_lookup_builds_the_sql_it_always_did() {
        let (sql, binds) = build_symbols_query(guid(), "m", &symbols_req("collect", None), 10);
        assert!(!sql.contains("file_path IN"), "{sql}");
        assert_eq!(binds.len(), 4, "project, model, name, role — nothing else");
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

        let (paths, total) = run_symbols_query(
            &pool,
            symbols_req("target", Some("src/db/qdrant.rs")),
            10,
            "definition",
        )
        .await;
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
        let (paths, _) = run_symbols_query(
            &pool,
            symbols_req("target", Some("main.rs")),
            10,
            "definition",
        )
        .await;
        assert_eq!(paths, vec!["main.rs", "other.rs", "src/lib.rs"]);
    }

    #[tokio::test]
    async fn symbols_query_totals_survive_the_limit() {
        let files: &[&str] = &["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
        let pool = pool_with_prepared_files(files, 0).await;
        for f in files {
            insert_symbol(&pool, f, "popular").await;
        }
        let (paths, total) =
            run_symbols_query(&pool, symbols_req("popular", None), 2, "definition").await;
        assert_eq!(paths.len(), 2, "the limit caps the returned rows");
        assert_eq!(total, 5, "the total must report the full candidate count");

        // The other role is independent: no references exist.
        let (refs, ref_total) =
            run_symbols_query(&pool, symbols_req("popular", None), 2, "reference").await;
        assert!(refs.is_empty());
        assert_eq!(ref_total, 0);
    }

    // ── callers: the approximate call graph ─────────────────────────────────

    /// One reference row. `parent` is `None` for a reference at file scope — a
    /// top-level call or an import, which `callers` must report rather than drop.
    async fn insert_reference(
        pool: &SQLite3Pool,
        path: &'static str,
        name: &'static str,
        parent: Option<&'static str>,
        line: i64,
    ) {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.execute(
                "INSERT INTO project_file_symbols
                     (project_guid, model_id, file_path, name, kind, role,
                      start_line, end_line, start_column, end_column,
                      parent_name, parent_kind)
                 VALUES (?1, ?2, ?3, ?4, 'call', 'reference', ?5, ?5, 0, 1, ?6, ?7)",
                params![
                    guid(),
                    MODEL,
                    path,
                    name,
                    line,
                    parent,
                    parent.map(|_| "function")
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    /// `(path, symbol, kind, first_line, occurrences)` per group, plus the two totals.
    #[allow(clippy::type_complexity)]
    async fn run_callers_query(
        pool: &SQLite3Pool,
        name: &'static str,
        direction: CallDirection,
    ) -> (
        Vec<(String, Option<String>, Option<String>, i64, i64)>,
        i64,
        i64,
    ) {
        let sql = build_callers_query(direction, "");
        let rows: Vec<(String, Option<String>, Option<String>, i64, i64, i64, i64)> = pool
            .transaction(CancellationToken::new(), move |tx| {
                tx.prepare(&sql)?
                    .query_map(params![guid(), MODEL, name], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(SQLite3PoolError::from)
            })
            .await
            .unwrap();
        let total_sites = rows.first().map_or(0, |r| r.5);
        let total_references = rows.first().map_or(0, |r| r.6);
        (
            rows.into_iter()
                .map(|r| (r.0, r.1, r.2, r.3, r.4))
                .collect(),
            total_sites,
            total_references,
        )
    }

    /// `In` is the headline question — "who calls X". The grouping is the load-
    /// bearing part: one row per (file, enclosing definition) with the first line
    /// to read and how many times it happens, not one row per occurrence.
    #[tokio::test]
    async fn callers_in_groups_occurrences_by_enclosing_definition() {
        let files: &[&str] = &["a.rs", "b.rs"];
        let pool = pool_with_prepared_files(files, 0).await;
        // `caller_one` calls target twice in a.rs, `caller_two` once, and b.rs has
        // a top-level reference with no enclosing definition.
        insert_reference(&pool, "a.rs", "target", Some("caller_one"), 30).await;
        insert_reference(&pool, "a.rs", "target", Some("caller_one"), 10).await;
        insert_reference(&pool, "a.rs", "target", Some("caller_two"), 50).await;
        insert_reference(&pool, "b.rs", "target", None, 3).await;
        // Noise that must not be counted: a different name, in the same files.
        insert_reference(&pool, "a.rs", "unrelated", Some("caller_one"), 11).await;

        let (sites, total_sites, total_refs) =
            run_callers_query(&pool, "target", CallDirection::In).await;

        assert_eq!(
            sites,
            vec![
                // MIN(start_line), not the first row inserted.
                (
                    "a.rs".to_string(),
                    Some("caller_one".to_string()),
                    Some("function".to_string()),
                    10,
                    2
                ),
                (
                    "a.rs".to_string(),
                    Some("caller_two".to_string()),
                    Some("function".to_string()),
                    50,
                    1
                ),
                // A top-level reference is kept, with no symbol: dropping it would
                // make the totals disagree with the list.
                ("b.rs".to_string(), None, None, 3, 1),
            ]
        );
        assert_eq!(total_sites, 3, "windows count groups, not rows");
        assert_eq!(
            total_refs, 4,
            "summing the per-group counts recovers the row total"
        );
    }

    /// `Out` is the same table read the other way: filter on the enclosing
    /// definition, report what it referenced. Getting the column swap wrong would
    /// silently answer the `In` question instead.
    #[tokio::test]
    async fn callers_out_reports_what_a_definition_references() {
        let files: &[&str] = &["a.rs"];
        let pool = pool_with_prepared_files(files, 0).await;
        insert_reference(&pool, "a.rs", "alpha", Some("outer"), 12).await;
        insert_reference(&pool, "a.rs", "alpha", Some("outer"), 14).await;
        insert_reference(&pool, "a.rs", "beta", Some("outer"), 20).await;
        // A call in a *different* definition must not appear.
        insert_reference(&pool, "a.rs", "gamma", Some("elsewhere"), 90).await;

        let (sites, total_sites, total_refs) =
            run_callers_query(&pool, "outer", CallDirection::Out).await;

        let names: Vec<Option<String>> = sites.iter().map(|s| s.1.clone()).collect();
        assert_eq!(
            names,
            vec![Some("alpha".to_string()), Some("beta".to_string())],
            "only what `outer` references, and `gamma` is not one of them"
        );
        assert_eq!(sites[0].4, 2, "alpha is referenced twice");
        assert_eq!(sites[0].3, 12);
        assert_eq!((total_sites, total_refs), (2, 3));
    }

    /// An unknown name and a defined-but-unreferenced one both come back with no
    /// sites, so the row count alone cannot tell them apart — `defined` is what
    /// does, and it is the reason `callers_core` reads twice.
    #[tokio::test]
    async fn callers_finds_no_sites_for_an_unreferenced_or_unknown_name() {
        let files: &[&str] = &["a.rs"];
        let pool = pool_with_prepared_files(files, 0).await;
        insert_symbol(&pool, "a.rs", "lonely").await;

        for name in ["lonely", "no_such_symbol"] {
            let (sites, total_sites, total_refs) = run_callers_query(
                &pool,
                if name == "lonely" {
                    "lonely"
                } else {
                    "no_such_symbol"
                },
                CallDirection::In,
            )
            .await;
            assert!(sites.is_empty(), "{name} must yield no call sites");
            assert_eq!((total_sites, total_refs), (0, 0));
        }
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
            read_file_history(tx, guid(), HISTORY_MODEL, path)
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
}
