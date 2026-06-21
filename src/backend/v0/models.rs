use glob::Pattern;
use rusqlite::{
    ToSql,
    types::{FromSql, FromSqlResult, ToSqlOutput, ValueRef},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Deserialize, Serialize, Debug)]
pub struct Code {
    pub code: String,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Hash, Clone, Copy)]
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

#[derive(Deserialize, Serialize, Debug)]
pub struct IndexRequest {
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, Code>>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct IndexResponse {
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, u64>>,
}

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

#[derive(Deserialize, Serialize, Debug)]
pub struct SearchFilter {
    pub paths: Option<Vec<GlobPattern>>,
    pub programming_languages: Option<Vec<ProgrammingLanguage>>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub top_k: Option<usize>,
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug)]
pub struct SearchResult {
    pub score: f32,
    pub path: UnixPath,
    pub code: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_column: usize,
    pub end_column: usize,
}

#[derive(Serialize, Debug)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

// ─── Management endpoints ───────────────────────────────────────────────────

/// `DELETE /projects/{guid}/files` body — same selector shape as search, so the
/// same globs/languages that surface files can also remove them. At least one of
/// `include`/`exclude` must be non-empty (the handler rejects an empty body to
/// avoid wiping the whole project).
#[derive(Deserialize, Serialize, Debug, Default)]
pub struct DeleteFilesRequest {
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug)]
pub struct DeleteFilesResponse {
    pub deleted_files: u64,
}

/// `POST /projects/{guid}/cancel` body — same selector shape as `DeleteFilesRequest`,
/// so the same globs/languages that surface files can also cancel their in-flight
/// indexing. At least one of `include`/`exclude` must be non-empty (the handler
/// rejects an empty body so it can't blanket-cancel the whole project).
#[derive(Deserialize, Serialize, Debug, Default)]
pub struct CancelRequest {
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug)]
pub struct CancelResponse {
    pub cancelled_files: u64,
}

/// Per-status `project_files` counts. A fixed struct (not a sparse map) so the
/// response schema is self-documenting and every status is always present.
#[derive(Serialize, Debug, Default)]
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
#[derive(Serialize, Debug, Default)]
pub struct ChunkCounts {
    pub active: u64,
    pub deleted: u64,
}

#[derive(Serialize, Debug)]
pub struct ProjectStats {
    pub project_guid: UUIDv4,
    pub files: FileStatusCounts,
    /// Keyed by programming language. `deleted` here is "soft-deleted but not yet
    /// physically removed" (awaiting GC).
    pub chunks: HashMap<String, ChunkCounts>,
}

/// One row of `GET /projects` — a compact per-project summary (full per-language
/// breakdown is `GET /projects/{guid}`).
#[derive(Serialize, Debug)]
pub struct ProjectSummary {
    pub project_guid: String,
    pub files: i64,
    pub indexing: i64,
    pub active_chunks: i64,
}

#[derive(Serialize, Debug)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectSummary>,
}

/// `POST /projects/{guid}/drift` body: the working tree's `path → sha256` map.
/// The server stays filesystem-agnostic — the client walks + hashes; the server
/// only compares this against what it already stored.
#[derive(Deserialize, Debug)]
pub struct DriftRequest {
    pub files: HashMap<String, String>,
}

/// Divergence of the working tree from the index, in four buckets:
/// - `stale`: indexed but the content hash differs (needs reindex),
/// - `missing`: present locally but not indexed (`failed`/never-indexed),
/// - `orphaned`: indexed but absent locally (should be deleted from the index),
/// - `indexing`: currently being indexed — **no action**, it will settle.
#[derive(Serialize, Debug, Default, PartialEq, Eq)]
pub struct DriftResponse {
    pub stale: Vec<String>,
    pub missing: Vec<String>,
    pub orphaned: Vec<String>,
    pub indexing: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct GcResponse {
    pub chunks_removed: usize,
    pub files_removed: usize,
    pub status_log_pruned: usize,
}

#[derive(Serialize, Debug)]
pub struct VersionResponse {
    pub version: &'static str,
}

/// One dependency's liveness: `"ok"` or `"error: <reason>"`.
#[derive(Serialize, Debug)]
pub struct HealthChecks {
    pub sqlite: String,
    pub qdrant: String,
    pub embedder: String,
}

#[derive(Serialize, Debug)]
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
#[derive(Deserialize, Debug, Default)]
pub struct FileListQuery {
    pub status: Option<String>,
    pub language: Option<ProgrammingLanguage>,
}

/// One file in `GET /projects/{guid}/files`. `chunk_count` counts only `active`
/// chunks (soft-deleted ones awaiting GC are excluded).
#[derive(Serialize, Debug)]
pub struct FileInfo {
    pub path: UnixPath,
    pub programming_language: ProgrammingLanguage,
    pub status: String,
    pub sha256: String,
    pub chunk_count: u64,
    pub retry_count: i64,
    pub status_updated_at: i64,
}

#[derive(Serialize, Debug)]
pub struct FileListResponse {
    pub files: Vec<FileInfo>,
}

/// `POST /projects/{guid}/retry` body — same selector shape as the cancel/delete
/// endpoints, but **both fields optional**: an empty body means "every `failed`
/// file". Retry is non-destructive (it only resets the retry counter so the worker
/// re-attempts the file), so a blanket requeue is the useful dead-letter-recovery
/// default rather than a footgun to guard against.
#[derive(Deserialize, Serialize, Debug, Default)]
pub struct RetryRequest {
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
}

#[derive(Serialize, Debug)]
pub struct RetryResponse {
    pub requeued_files: u64,
}

/// `GET /status` — live runtime/concurrency state (cheap to compute; for diagnosing
/// 429/409/503 and stuck work). Distinct from `GET /config` (static knobs) and
/// `GET /health` (dependency liveness).
#[derive(Serialize, Debug)]
pub struct StatusResponse {
    /// Per-file `(project, model, path)` indexing claims held right now — the size of
    /// the in-process mutual-exclusion table (contention here is what returns 429).
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
#[derive(Serialize, Debug)]
pub struct ConfigResponse {
    pub version: &'static str,
    pub model_id: String,
    pub languages: Vec<&'static str>,
    pub embed_batch: usize,
    pub db_pool_size: usize,
    pub stuck_grace_mins: i64,
    pub max_retries: i64,
}
