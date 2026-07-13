use glob::Pattern;
use rusqlite::{
    ToSql,
    types::{FromSql, FromSqlResult, ToSqlOutput, ValueRef},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// One source file's contents, keyed by path inside the language map of an
/// `IndexRequest`.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct Code {
    /// The full UTF-8 source text. Sliced server-side into 128–512-token chunks.
    pub code: String,
}

/// A language mindex can chunk. Serialized as its lowercase name (e.g. `"rust"`,
/// `"cpp"`, `"csharp"`); the same set is returned by `GET /config`.
#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Hash, Clone, Copy, ToSchema)]
#[schema(rename_all = "lowercase")]
pub enum ProgrammingLanguage {
    #[serde(rename = "rust")]       Rust,
    #[serde(rename = "python")]     Python,
    #[serde(rename = "javascript")] JavaScript,
    #[serde(rename = "typescript")] TypeScript,
    #[serde(rename = "tsx")]        Tsx,
    #[serde(rename = "go")]         Go,
    #[serde(rename = "c")]          C,
    #[serde(rename = "cpp")]        Cpp,
    #[serde(rename = "java")]       Java,
    #[serde(rename = "csharp")]     CSharp,
    #[serde(rename = "ruby")]       Ruby,
    #[serde(rename = "php")]        Php,
    #[serde(rename = "bash")]       Bash,
    #[serde(rename = "html")]       Html,
    #[serde(rename = "css")]        Css,
    #[serde(rename = "json")]       Json,
    #[serde(rename = "scala")]      Scala,
    #[serde(rename = "haskell")]    Haskell,
    #[serde(rename = "ocaml")]      Ocaml,
    #[serde(rename = "zig")]        Zig,
    #[serde(rename = "sql")]        Sql,
}

impl ToSql for ProgrammingLanguage {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.name()))
    }
}

impl ProgrammingLanguage {
    /// Every variant, in declaration order. The single source of truth for the
    /// supported-language set exposed by `GET /config` (so clients — e.g. the search
    /// frontend — read the live list instead of hardcoding their own copy).
    pub const ALL: &'static [ProgrammingLanguage] = &[
        ProgrammingLanguage::Rust,
        ProgrammingLanguage::Python,
        ProgrammingLanguage::JavaScript,
        ProgrammingLanguage::TypeScript,
        ProgrammingLanguage::Tsx,
        ProgrammingLanguage::Go,
        ProgrammingLanguage::C,
        ProgrammingLanguage::Cpp,
        ProgrammingLanguage::Java,
        ProgrammingLanguage::CSharp,
        ProgrammingLanguage::Ruby,
        ProgrammingLanguage::Php,
        ProgrammingLanguage::Bash,
        ProgrammingLanguage::Html,
        ProgrammingLanguage::Css,
        ProgrammingLanguage::Json,
        ProgrammingLanguage::Scala,
        ProgrammingLanguage::Haskell,
        ProgrammingLanguage::Ocaml,
        ProgrammingLanguage::Zig,
        ProgrammingLanguage::Sql,
    ];

    /// The lowercase wire name (matches the serde rename and the SQLite `ToSql`).
    pub fn name(self) -> &'static str {
        match self {
            ProgrammingLanguage::Rust       => "rust",
            ProgrammingLanguage::Python     => "python",
            ProgrammingLanguage::JavaScript => "javascript",
            ProgrammingLanguage::TypeScript => "typescript",
            ProgrammingLanguage::Tsx        => "tsx",
            ProgrammingLanguage::Go         => "go",
            ProgrammingLanguage::C          => "c",
            ProgrammingLanguage::Cpp        => "cpp",
            ProgrammingLanguage::Java       => "java",
            ProgrammingLanguage::CSharp     => "csharp",
            ProgrammingLanguage::Ruby       => "ruby",
            ProgrammingLanguage::Php        => "php",
            ProgrammingLanguage::Bash       => "bash",
            ProgrammingLanguage::Html       => "html",
            ProgrammingLanguage::Css        => "css",
            ProgrammingLanguage::Json       => "json",
            ProgrammingLanguage::Scala      => "scala",
            ProgrammingLanguage::Haskell    => "haskell",
            ProgrammingLanguage::Ocaml      => "ocaml",
            ProgrammingLanguage::Zig        => "zig",
            ProgrammingLanguage::Sql        => "sql",
        }
    }
}

impl FromSql for ProgrammingLanguage {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "rust"       => Ok(ProgrammingLanguage::Rust),
            "python"     => Ok(ProgrammingLanguage::Python),
            "javascript" => Ok(ProgrammingLanguage::JavaScript),
            "typescript" => Ok(ProgrammingLanguage::TypeScript),
            "tsx"        => Ok(ProgrammingLanguage::Tsx),
            "go"         => Ok(ProgrammingLanguage::Go),
            "c"          => Ok(ProgrammingLanguage::C),
            "cpp"        => Ok(ProgrammingLanguage::Cpp),
            "java"       => Ok(ProgrammingLanguage::Java),
            "csharp"     => Ok(ProgrammingLanguage::CSharp),
            "ruby"       => Ok(ProgrammingLanguage::Ruby),
            "php"        => Ok(ProgrammingLanguage::Php),
            "bash"       => Ok(ProgrammingLanguage::Bash),
            "html"       => Ok(ProgrammingLanguage::Html),
            "css"        => Ok(ProgrammingLanguage::Css),
            "json"       => Ok(ProgrammingLanguage::Json),
            "scala"      => Ok(ProgrammingLanguage::Scala),
            "haskell"    => Ok(ProgrammingLanguage::Haskell),
            "ocaml"      => Ok(ProgrammingLanguage::Ocaml),
            "zig"        => Ok(ProgrammingLanguage::Zig),
            "sql"        => Ok(ProgrammingLanguage::Sql),
            _            => Err(rusqlite::types::FromSqlError::InvalidType),
        }
    }
}

type UnixPath = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct UUIDv4(pub Uuid);

impl ToSql for UUIDv4 {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.simple().to_string()))
    }
}

impl FromSql for UUIDv4 {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        let uuid = Uuid::parse_str(s).map_err(|_| rusqlite::types::FromSqlError::InvalidType)?;
        Ok(UUIDv4(uuid))
    }
}

/// `POST /v0/{project_guid}/index` body. Files grouped by language, then by path —
/// one HTTP call can carry many files of many languages. Unchanged files (matching
/// stored sha256) are skipped server-side and absent from the response.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct IndexRequest {
    /// `language → (path → {code})`. Paths are repo-relative Unix paths.
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, Code>>,
}

/// `POST /v0/{project_guid}/index` response: per indexed file, the number of chunks
/// produced. `0` means the file sliced to no chunks (shorter than 128 tokens), **not**
/// "unchanged" — hash-unchanged files are skipped and omitted entirely.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct IndexResponse {
    /// `language → (path → chunk_count)`, covering only files actually (re)indexed.
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, u64>>,
}

/// A shell-style glob (e.g. `src/**`, `*.rs`) evaluated by SQLite `GLOB`. Serialized
/// as the raw pattern string.
#[derive(Debug)]
pub struct GlobPattern(pub Pattern);

impl<'l> Deserialize<'l> for GlobPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'l>,
    {
        let s = String::deserialize(deserializer)?;
        Pattern::new(&s)
            .map(GlobPattern)
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for GlobPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.as_str().serialize(serializer)
    }
}

/// A match selector reused by search and the management endpoints. `paths` and
/// `programming_languages` combine with AND; within each, entries combine with OR.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct SearchFilter {
    /// Glob patterns over repo-relative paths (e.g. `["src/**", "tests/**"]`).
    #[schema(value_type = Option<Vec<String>>, example = json!(["src/**"]))]
    pub paths: Option<Vec<GlobPattern>>,
    /// Restrict to (or, in `exclude`, drop) these languages.
    pub programming_languages: Option<Vec<ProgrammingLanguage>>,
}

/// `POST /v0/{project_guid}/search` body. Hybrid retrieval: dense + sparse prefetch →
/// RRF fusion → ColBERT MaxSim rerank → top-k.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct SearchRequest {
    /// Natural-language or code query; embedded with the same BGE-M3 model.
    pub query: String,
    /// Max results to return. Defaults to 5 when omitted.
    #[schema(default = 5, example = 5)]
    pub top_k: Option<usize>,
    /// Keep only chunks matching this selector.
    pub include: Option<SearchFilter>,
    /// Drop chunks matching this selector (applied after `include`).
    pub exclude: Option<SearchFilter>,
}

/// One ranked hit: the chunk's code plus its byte-accurate source span. Responses are
/// sorted by `score` descending.
#[derive(Serialize, Debug, ToSchema)]
pub struct SearchResult {
    /// Fusion/rerank score; higher is more relevant. Not normalized to any range.
    pub score: f32,
    pub path: UnixPath,
    /// The chunk's source text.
    pub code: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_column: usize,
    pub end_column: usize,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

// ─── Management endpoints ───────────────────────────────────────────────────

/// `DELETE /projects/{guid}/files` body — same selector shape as search, so the
/// same globs/languages that surface files can also remove them. At least one of
/// `include`/`exclude` must be non-empty (the handler rejects an empty body to
/// avoid wiping the whole project).
#[derive(Deserialize, Serialize, Debug, Default, ToSchema)]
pub struct DeleteFilesRequest {
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct DeleteFilesResponse {
    /// Number of files moved to `deleted` (their vectors are reclaimed by the next GC pass).
    pub deleted_files: u64,
}

/// `POST /projects/{guid}/cancel` body — same selector shape as `DeleteFilesRequest`,
/// so the same globs/languages that surface files can also cancel their in-flight
/// indexing. At least one of `include`/`exclude` must be non-empty (the handler
/// rejects an empty body so it can't blanket-cancel the whole project).
#[derive(Deserialize, Serialize, Debug, Default, ToSchema)]
pub struct CancelRequest {
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct CancelResponse {
    /// Number of files moved `indexing → cancelled`. Files already `indexed`/`failed`
    /// are never matched, so a too-late cancel reports `0`.
    pub cancelled_files: u64,
}

/// Per-status `project_files` counts. A fixed struct (not a sparse map) so the
/// response schema is self-documenting and every status is always present.
#[derive(Serialize, Debug, Default, ToSchema)]
pub struct FileStatusCounts {
    pub just_uploaded: u64,
    pub indexing: u64,
    pub indexed: u64,
    pub cancelled: u64,
    pub failed: u64,
    pub deleted: u64,
}

impl FileStatusCounts {
    pub fn set(&mut self, status: &str, count: u64) {
        match status {
            "just_uploaded" => self.just_uploaded = count,
            "indexing" => self.indexing = count,
            "indexed" => self.indexed = count,
            "cancelled" => self.cancelled = count,
            "failed" => self.failed = count,
            "deleted" => self.deleted = count,
            _ => {}
        }
    }
}

/// Active vs soft-deleted (pending GC) chunk counts for one language.
#[derive(Serialize, Debug, Default, ToSchema)]
pub struct ChunkCounts {
    pub active: u64,
    pub deleted: u64,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct ProjectStats {
    #[schema(value_type = String, example = "550e8400e29b41d4a716446655440000")]
    pub project_guid: UUIDv4,
    pub files: FileStatusCounts,
    /// Keyed by programming language. `deleted` here is "soft-deleted but not yet
    /// physically removed" (awaiting GC).
    pub chunks: HashMap<String, ChunkCounts>,
}

/// One row of `GET /projects` — a compact per-project summary (full per-language
/// breakdown is `GET /projects/{guid}`).
#[derive(Serialize, Debug, ToSchema)]
pub struct ProjectSummary {
    pub project_guid: String,
    /// Total files tracked for the project (any status).
    pub files: i64,
    /// Files currently in `status='indexing'`.
    pub indexing: i64,
    /// Active (non-soft-deleted) chunks across the project.
    pub active_chunks: i64,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectSummary>,
}

/// `POST /projects/{guid}/drift` body: the working tree's `path → sha256` map.
/// The server stays filesystem-agnostic — the client walks + hashes; the server
/// only compares this against what it already stored.
#[derive(Deserialize, Debug, ToSchema)]
pub struct DriftRequest {
    /// `path → sha256` of the working tree. The client walks + hashes; the server only
    /// compares against what it stored.
    pub files: HashMap<String, String>,
}

/// Divergence of the working tree from the index, in four buckets:
/// - `stale`: indexed but the content hash differs (needs reindex),
/// - `missing`: present locally but not indexed (`failed`/never-indexed),
/// - `orphaned`: indexed but absent locally (should be deleted from the index),
/// - `indexing`: currently being indexed — **no action**, it will settle.
#[derive(Serialize, Debug, Default, PartialEq, Eq, ToSchema)]
pub struct DriftResponse {
    /// Indexed but content hash differs — needs reindex.
    pub stale: Vec<String>,
    /// Present locally but not indexed (`failed`/never-indexed).
    pub missing: Vec<String>,
    /// Indexed but absent locally — should be deleted from the index.
    pub orphaned: Vec<String>,
    /// Currently being indexed — no action, it will settle.
    pub indexing: Vec<String>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct GcResponse {
    /// Soft-deleted chunks physically removed (vectors confirmed gone from Qdrant first).
    pub chunks_removed: usize,
    /// Emptied `deleted` file rows dropped.
    pub files_removed: usize,
    /// `project_file_status_log` rows pruned past the retention window.
    pub status_log_pruned: usize,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct VersionResponse {
    pub version: &'static str,
    /// Applied `PRAGMA user_version` — the highest migration version in the running binary.
    pub db_schema_version: i32,
}

/// One dependency's liveness: `"ok"` or `"error: <reason>"`.
#[derive(Serialize, Debug, ToSchema)]
pub struct HealthChecks {
    pub sqlite: String,
    pub qdrant: String,
    pub embedder: String,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct HealthResponse {
    /// `"ok"` only when all three dependency checks pass, else `"degraded"`.
    pub status: &'static str,
    pub version: &'static str,
    /// Files in `status='indexing'` across *all* projects right now.
    pub indexing_files: i64,
    pub checks: HealthChecks,
}

/// `GET /projects/{guid}/files` query string — optional filters. `language`
/// deserializes from its lowercase wire name (e.g. `?language=rust`).
#[derive(Deserialize, Debug, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FileListQuery {
    /// Filter by file status, e.g. `indexed`, `failed` (the dead-letter view), `indexing`.
    pub status: Option<String>,
    /// Filter by language (lowercase name, e.g. `rust`).
    pub language: Option<ProgrammingLanguage>,
}

/// One file in `GET /projects/{guid}/files`. `chunk_count` counts only `active`
/// chunks (soft-deleted ones awaiting GC are excluded).
#[derive(Serialize, Debug, ToSchema)]
pub struct FileInfo {
    pub path: UnixPath,
    pub programming_language: ProgrammingLanguage,
    /// Current state-machine status (`indexed`, `indexing`, `failed`, …).
    pub status: String,
    /// Content hash recorded at the last `indexing` start.
    pub sha256: String,
    /// Active (non-soft-deleted) chunk count for this file.
    pub chunk_count: u64,
    /// Times the retry worker has re-attempted this file (reset to 0 on success).
    pub retry_count: i64,
    /// Unix epoch seconds of the last status change.
    pub status_updated_at: i64,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct FileListResponse {
    pub files: Vec<FileInfo>,
}

/// `POST /projects/{guid}/retry` body — same selector shape as the cancel/delete
/// endpoints, but **both fields optional**: an empty body means "every `failed`
/// file". Retry is non-destructive (it only resets the retry counter so the worker
/// re-attempts the file), so a blanket requeue is the useful dead-letter-recovery
/// default rather than a footgun to guard against.
#[derive(Deserialize, Serialize, Debug, Default, ToSchema)]
pub struct RetryRequest {
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct RetryResponse {
    /// Number of `failed` files whose retry counter was reset for the retry worker.
    pub requeued_files: u64,
}

/// `GET /status` — live runtime/concurrency state (cheap to compute; for diagnosing
/// 429/409/503 and stuck work). Distinct from `GET /config` (static knobs) and
/// `GET /health` (dependency liveness).
#[derive(Serialize, Debug, ToSchema)]
pub struct StatusResponse {
    /// Per-file `(project, model, path)` indexing claims held right now — the size of
    /// the in-process mutual-exclusion table. A same-file collision is skipped
    /// server-side (the file is simply absent from that `/index` response), never
    /// surfaced as an error.
    pub indexing_claims: usize,
    /// Whether a garbage-collection pass is running (a `POST /gc` now returns 409).
    pub gc_running: bool,
    /// SQLite connections currently free in the pool (0 ⇒ the next `transaction`
    /// fails fast with `PoolEmpty` → 500).
    pub pool_available: usize,
    pub pool_size: usize,
    /// Files in `status='indexing'` across all projects.
    pub indexing_files: i64,
    /// Global `project_files` counts by status.
    pub files_by_status: FileStatusCounts,
}

/// `GET /config` — static server capabilities and tuning knobs. `languages` is the
/// canonical supported-language list (derived from the `ProgrammingLanguage` enum).
#[derive(Serialize, Debug, ToSchema)]
pub struct ConfigResponse {
    pub version: &'static str,
    pub model_id: String,
    pub languages: Vec<&'static str>,
    pub embed_batch: usize,
    pub db_pool_size: usize,
    pub stuck_grace_mins: i64,
    pub max_retries: i64,
}
