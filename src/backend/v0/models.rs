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
    #[serde(rename = "rust")]
    Rust,
    #[serde(rename = "python")]
    Python,
    #[serde(rename = "javascript")]
    JavaScript,
    #[serde(rename = "typescript")]
    TypeScript,
    #[serde(rename = "tsx")]
    Tsx,
    #[serde(rename = "go")]
    Go,
    #[serde(rename = "c")]
    C,
    #[serde(rename = "cpp")]
    Cpp,
    #[serde(rename = "java")]
    Java,
    #[serde(rename = "csharp")]
    CSharp,
    #[serde(rename = "ruby")]
    Ruby,
    #[serde(rename = "php")]
    Php,
    #[serde(rename = "bash")]
    Bash,
    #[serde(rename = "html")]
    Html,
    #[serde(rename = "css")]
    Css,
    #[serde(rename = "json")]
    Json,
    #[serde(rename = "scala")]
    Scala,
    #[serde(rename = "haskell")]
    Haskell,
    #[serde(rename = "ocaml")]
    Ocaml,
    #[serde(rename = "zig")]
    Zig,
    #[serde(rename = "sql")]
    Sql,
    #[serde(rename = "toml")]
    Toml,
    #[serde(rename = "yaml")]
    Yaml,
    /// Documentation. The only variant whose chunking is not the AST walk in
    /// [`Slicer`](crate::slicing::traits::Slicer) — see
    /// [`MarkdownSlicer`](crate::slicing::markdown::MarkdownSlicer).
    #[serde(rename = "markdown")]
    Markdown,
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
        ProgrammingLanguage::Toml,
        ProgrammingLanguage::Yaml,
        ProgrammingLanguage::Markdown,
    ];

    /// The lowercase wire name (matches the serde rename and the SQLite `ToSql`).
    pub fn name(self) -> &'static str {
        match self {
            ProgrammingLanguage::Rust => "rust",
            ProgrammingLanguage::Python => "python",
            ProgrammingLanguage::JavaScript => "javascript",
            ProgrammingLanguage::TypeScript => "typescript",
            ProgrammingLanguage::Tsx => "tsx",
            ProgrammingLanguage::Go => "go",
            ProgrammingLanguage::C => "c",
            ProgrammingLanguage::Cpp => "cpp",
            ProgrammingLanguage::Java => "java",
            ProgrammingLanguage::CSharp => "csharp",
            ProgrammingLanguage::Ruby => "ruby",
            ProgrammingLanguage::Php => "php",
            ProgrammingLanguage::Bash => "bash",
            ProgrammingLanguage::Html => "html",
            ProgrammingLanguage::Css => "css",
            ProgrammingLanguage::Json => "json",
            ProgrammingLanguage::Scala => "scala",
            ProgrammingLanguage::Haskell => "haskell",
            ProgrammingLanguage::Ocaml => "ocaml",
            ProgrammingLanguage::Zig => "zig",
            ProgrammingLanguage::Sql => "sql",
            ProgrammingLanguage::Toml => "toml",
            ProgrammingLanguage::Yaml => "yaml",
            ProgrammingLanguage::Markdown => "markdown",
        }
    }
}

impl FromSql for ProgrammingLanguage {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "rust" => Ok(ProgrammingLanguage::Rust),
            "python" => Ok(ProgrammingLanguage::Python),
            "javascript" => Ok(ProgrammingLanguage::JavaScript),
            "typescript" => Ok(ProgrammingLanguage::TypeScript),
            "tsx" => Ok(ProgrammingLanguage::Tsx),
            "go" => Ok(ProgrammingLanguage::Go),
            "c" => Ok(ProgrammingLanguage::C),
            "cpp" => Ok(ProgrammingLanguage::Cpp),
            "java" => Ok(ProgrammingLanguage::Java),
            "csharp" => Ok(ProgrammingLanguage::CSharp),
            "ruby" => Ok(ProgrammingLanguage::Ruby),
            "php" => Ok(ProgrammingLanguage::Php),
            "bash" => Ok(ProgrammingLanguage::Bash),
            "html" => Ok(ProgrammingLanguage::Html),
            "css" => Ok(ProgrammingLanguage::Css),
            "json" => Ok(ProgrammingLanguage::Json),
            "scala" => Ok(ProgrammingLanguage::Scala),
            "haskell" => Ok(ProgrammingLanguage::Haskell),
            "ocaml" => Ok(ProgrammingLanguage::Ocaml),
            "zig" => Ok(ProgrammingLanguage::Zig),
            "sql" => Ok(ProgrammingLanguage::Sql),
            "toml" => Ok(ProgrammingLanguage::Toml),
            "yaml" => Ok(ProgrammingLanguage::Yaml),
            "markdown" => Ok(ProgrammingLanguage::Markdown),
            _ => Err(rusqlite::types::FromSqlError::InvalidType),
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

    /// Rebuild every posted file even when its stored hash *and* derivation versions
    /// match — the escape hatch for what versioning cannot see (a grammar-crate bump
    /// with the const untouched, a corrupted index, debugging). Costs a full re-slice
    /// and re-embed of everything posted, so scope the request rather than the flag.
    #[serde(default)]
    pub force: bool,

    /// Rebuild **only** `project_file_symbols`: no slicing, no embedding, no Qdrant
    /// contact, chunks and vectors untouched. This is the cheap half of a
    /// `SYMBOLS_DERIVATION_VERSION` bump — symbols come from tree-sitter alone, so
    /// re-deriving them never needs the GPU.
    ///
    /// A posted file whose content no longer matches the stored hash is **skipped**,
    /// not rebuilt: its chunks are stale too, and symbols that describe newer text
    /// than the chunks beside them would break the "symbols parallel chunks"
    /// invariant. Run an ordinary index pass for those.
    #[serde(default)]
    pub symbols_only: bool,
}

/// `POST /v0/{project_guid}/index` response: per indexed file, the number of chunks
/// produced. `0` means the file sliced to no chunks (shorter than 128 tokens), **not**
/// "unchanged" — hash-unchanged files are skipped and omitted entirely.
///
/// Under `symbols_only` the count is the number of **symbol rows** written instead,
/// and the same omission rule applies (a file that was already at the current
/// `SYMBOLS_DERIVATION_VERSION`, or whose hash no longer matches, is absent).
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct IndexResponse {
    /// `language → (path → chunk_count)`, covering only files actually (re)indexed.
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, u64>>,
}

/// A shell-style glob (e.g. `src/**`, `*.rs`) evaluated by SQLite `GLOB`. Serialized
/// as the raw pattern string.
#[derive(Debug, Clone)]
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
#[derive(Deserialize, Serialize, Debug, Clone, ToSchema)]
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

/// A `/symbols` role filter (the table's `role` column values).
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SymbolRoleFilter {
    Definition,
    Reference,
}

/// Which way to read the approximate call graph.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CallDirection {
    /// Who references the name: reference rows carrying it, grouped by the
    /// definition each one sits inside.
    In,
    /// What the definition of that name references: reference rows whose
    /// enclosing definition is it.
    Out,
}

/// `POST /v0/{project_guid}/symbols` body — exact-name symbol lookup over the
/// definitions/references extracted at indexing time.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct SymbolsRequest {
    /// Exact symbol name (case-sensitive), e.g. a function or type identifier.
    pub name: String,
    /// Restrict to one role; both are returned when omitted.
    pub role: Option<SymbolRoleFilter>,
    /// Restrict to one tags.scm kind (`function`, `method`, `class`, `call`, …).
    pub kind: Option<String>,
    /// Ranking anchor: candidates in this file rank first, then its directory,
    /// then the rest. No filtering — only ordering.
    pub anchor_path: Option<UnixPath>,
    /// Max results *per role*. Defaults to 20 when omitted.
    #[schema(default = 20, example = 20)]
    pub limit: Option<usize>,
    /// Keep only occurrences in files matching this selector. Same shape and
    /// semantics as `/search`'s, so a caller that has scoped one lookup can scope
    /// this one the same way.
    ///
    /// Rows outside the selector are dropped **and counted**
    /// (`out_of_scope_definitions`/`_references`): a filtered list whose totals
    /// silently shrink is indistinguishable from a name that simply occurs less
    /// often, and `/symbols` calls its "no such symbol" answer definitive.
    pub include: Option<SearchFilter>,
    /// Drop occurrences in files matching this selector (applied after `include`).
    pub exclude: Option<SearchFilter>,
}

/// One symbol occurrence. `parent_*` name the nearest enclosing definition
/// (e.g. the function a call site sits in); `doc` is the attached doc comment
/// when the language's tags query captures one.
#[derive(Serialize, Debug, ToSchema)]
pub struct SymbolInfo {
    pub path: UnixPath,
    /// The tags.scm syntax type: `function`, `method`, `class`, `call`, ….
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_column: usize,
    pub end_column: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// Candidate lists, never a single "the" answer: an exact name can legally have
/// several definitions (overloads, same name in different modules). `total_*`
/// always carry the full counts so a truncated list is visible.
#[derive(Serialize, Debug, ToSchema)]
pub struct SymbolsResponse {
    pub definitions: Vec<SymbolInfo>,
    pub references: Vec<SymbolInfo>,
    pub total_definitions: u64,
    pub total_references: u64,
    /// Definitions the selector excluded. Zero for an unscoped lookup, and reported
    /// rather than absorbed: "not found" and "found, outside what you asked for" are
    /// different answers, and only one of them means the name does not exist.
    #[serde(skip_serializing_if = "is_zero")]
    pub out_of_scope_definitions: u64,
    /// References the selector excluded.
    #[serde(skip_serializing_if = "is_zero")]
    pub out_of_scope_references: u64,
}

/// Keeps the zero case off the wire, so an unscoped response is byte-identical to
/// what it was before scoping existed.
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One definition in a file's outline. Unlike [`SymbolInfo`] this carries the
/// **name**: `/symbols` is looked up *by* name, whereas an outline exists to hand
/// names to a caller that does not know any yet.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct OutlineSymbol {
    pub name: String,
    /// The tags.scm syntax type: `function`, `method`, `class`, ….
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// A file's definitions in source order. `indexed` distinguishes the two ways a
/// list can be empty — an unknown path (the caller guessed) from an indexed file
/// that yields no symbols (a language without a tags query, or genuinely nothing
/// to declare). Collapsing them would tell the caller its path was right.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct OutlineResponse {
    pub path: UnixPath,
    pub indexed: bool,
    /// Whether the caller's selector admits this file. `false` means the file may be
    /// perfectly well indexed and was simply not offered — reported separately from
    /// `indexed` for the same reason `indexed` exists at all: a refusal that reads as
    /// an empty outline tells the caller the file has no definitions, which is a
    /// different and wrong fact. Always `true` for an unscoped lookup.
    pub in_scope: bool,
    /// The file's language, `None` when it is not indexed. Present because the
    /// `kind` labels come from the language's upstream tags query and are not
    /// uniform across languages — Rust structs and enums both surface as `class`,
    /// which invited a model to search for `class Effort extends` (observed).
    /// Naming the language keeps that inference honest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub programming_language: Option<ProgrammingLanguage>,
    pub symbols: Vec<OutlineSymbol>,
    /// Full count before the limit, so truncation stays visible.
    pub total_definitions: u64,
}

/// One end of an approximate call edge, grouped per (file, symbol) pair.
///
/// *Which* end depends on the lookup's direction: with `In` this is the definition
/// the reference sits inside (the caller), with `Out` it is the name being
/// referenced (the callee). `symbol` is `None` only under `In`, for a reference at
/// file scope with no enclosing definition — a top-level call, an import. Those
/// rows are kept rather than dropped: they are real references, and omitting them
/// would make the totals disagree with the list for no visible reason.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct CallSite {
    pub path: UnixPath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The tags.scm syntax type of `symbol`, on the same terms as everywhere else:
    /// upstream query data, not uniform across languages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// First occurrence within the file, 1-indexed — where to point `read_chunks`.
    pub first_line: usize,
    /// How many reference rows this pair accounts for.
    pub occurrences: u64,
}

/// The approximate call graph around one exact name.
///
/// **Lexical, not resolved.** A reference row records that a token appeared in a
/// call position, never which definition it binds to, so these edges are exact up
/// to name collision: a common name (`new`, `get`, `collect`) mixes unrelated
/// definitions, and an aliased import breaks the edge entirely. Candidates to
/// confirm, not a resolved graph.
///
/// `defined` separates the two ways `sites` can be empty, the way
/// [`OutlineResponse::indexed`] does: an identifier the index has never seen from
/// one that is defined and simply never referenced.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct CallersResponse {
    pub name: String,
    pub direction: CallDirection,
    pub defined: bool,
    pub sites: Vec<CallSite>,
    /// Distinct (file, symbol) pairs before the limit, so truncation stays visible.
    pub total_sites: u64,
    /// Reference rows behind those pairs, before grouping and the limit.
    pub total_references: u64,
    /// Call sites the run's scope excluded. `defined` is deliberately *not* scoped:
    /// "there is no such name" and "it exists, outside your scope" must read
    /// differently, and the second is the more useful of the two.
    #[serde(skip_serializing_if = "is_zero")]
    pub out_of_scope_sites: u64,
}

/// One file in a listing.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct FileListing {
    pub path: UnixPath,
    pub programming_language: ProgrammingLanguage,
}

/// One indexed chunk overlapping a requested line range.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct ChunkExcerpt {
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
}

/// The indexed code covering a line range — an internal research lookup, with no
/// HTTP handler of its own.
///
/// `indexed` is reported separately from an empty `chunks` list on purpose: a
/// wrong path and a range the slicer produced no chunk for must read differently,
/// or the reader concludes the code is empty. Chunk coverage is *sparse by
/// construction* (nothing below `min_chunk_tokens` is emitted), so an empty list
/// for an indexed file is an ordinary answer, not an error.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct ReadChunksResponse {
    pub path: UnixPath,
    pub indexed: bool,
    /// Whether the caller's selector admits this file — see
    /// [`OutlineResponse::in_scope`].
    pub in_scope: bool,
    pub chunks: Vec<ChunkExcerpt>,
}

/// Literal matches over the indexed chunk text, path-ordered. `total` is the
/// pre-limit count.
///
/// The counterpart to `/search` rather than a variant of it: search embeds the query
/// and matches meaning, which cannot find an exact string — an error code, a config
/// key, a magic constant — and `/symbols` only knows names its language's tags query
/// tagged. This reads the bytes.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct GrepResponse {
    pub matches: Vec<GrepMatch>,
    pub total: u64,
    /// Matches the run's scope excluded, reported for the same reason as
    /// [`SymbolsResponse::out_of_scope_definitions`].
    #[serde(skip_serializing_if = "is_zero")]
    pub out_of_scope: u64,
}

/// One literal match: the chunk that contains it, plus the line the match itself is
/// on.
///
/// Both spans are given deliberately. `match_line` is what a reader wants; the
/// chunk's `start_line`/`end_line` are what a citation can be *verified* against,
/// since the run's evidence ledger records shown spans and a single line inside a
/// chunk would otherwise look like a range no tool returned.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct GrepMatch {
    pub path: UnixPath,
    pub start_line: usize,
    pub end_line: usize,
    pub match_line: usize,
    /// The matching line, trimmed and capped — enough to judge the hit, not enough
    /// to fill a transcript with it.
    pub excerpt: String,
}

/// Paths matching a glob, path-ordered. `total` is the pre-limit count.
#[derive(Serialize, Debug, ToSchema, PartialEq)]
pub struct ListFilesResponse {
    pub files: Vec<FileListing>,
    pub total: u64,
}

/// `POST /v0/{project_guid}/research` body. The response is a one-way SSE stream
/// (`text/event-stream`); see the endpoint description for the event contract.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
pub struct ResearchRequest {
    /// The research question — same contract as `/search`'s `query` (natural
    /// language or code terms), but typically broader/multi-part.
    pub question: String,
    /// Ollama model to drive the loop (e.g. a local thinking model). Falls back
    /// to the server's `[research].default_model`; 400 if both are absent.
    pub model: Option<String>,
    /// Research depth — selects a preset from `[research.effort.*]`. `budget`
    /// below overrides individual axes of it.
    #[schema(value_type = String, example = "medium")]
    pub effort: crate::research::Effort,
    /// Per-request budget, overriding the `effort` preset field by field. Absent
    /// fields keep the preset's value. Each is capped by `[research].max_request_*`
    /// (400 `validation.research_budget_out_of_range` above the cap).
    pub budget: Option<ResearchBudgetOverride>,
    /// Scope every index lookup the model makes (same semantics as `/search`).
    pub include: Option<SearchFilter>,
    pub exclude: Option<SearchFilter>,
    /// RNG seed for the model's sampling, overriding `[research].seed`. Absent =
    /// whatever the server configured, and if that is absent too, Ollama picks one
    /// per turn — so two runs of one question differ. Set it to make a run
    /// repeatable, or vary it deliberately to measure a model's own spread.
    pub seed: Option<i64>,
}

/// Per-request overrides for the `effort` preset.
///
/// `context_fraction` is deliberately absent: it is a guard against Ollama
/// silently trimming the transcript on a small-window model, not a quality lever,
/// so raising it per request buys nothing but truncation. It stays in config.
#[derive(Deserialize, Serialize, Debug, ToSchema, Default, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct ResearchBudgetOverride {
    /// Wall-clock for the investigation — the budget the caller actually waits for.
    pub max_seconds: Option<u64>,
    /// Local tokens (prompt + eval, summed over turns) — what the run costs the GPU.
    pub max_tokens: Option<u64>,
    /// Executed tool calls. A backstop, not a measure of work: `outline` is one
    /// indexed SELECT while `search` is a GPU embed plus a vector query.
    pub max_steps: Option<usize>,
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

/// One language's inventory in a project: files tracked, files that reached
/// `indexed`, and chunks live vs soft-deleted (awaiting GC).
///
/// The **file** counts are the load-bearing half, not decoration on the chunk
/// ones. Keyed on chunks alone, a language whose every file is `failed` — or whose
/// files all sliced to zero chunks — has no chunk rows at all and so was absent
/// from this map entirely, indistinguishable from a language the project does not
/// contain. That is the difference between "nothing to search here" and "nothing
/// here", and a client that offers the user a language filter needs to tell them
/// apart.
#[derive(Serialize, Debug, Default, ToSchema)]
pub struct LanguageStats {
    /// `project_files` rows in any status, including `deleted`.
    pub files: u64,
    /// Of those, the ones currently `indexed`.
    pub indexed_files: u64,
    /// Chunks a search can reach.
    pub chunks_active: u64,
    /// Soft-deleted but not yet physically removed (awaiting GC).
    pub chunks_deleted: u64,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct ProjectStats {
    #[schema(value_type = String, example = "550e8400e29b41d4a716446655440000")]
    pub project_guid: UUIDv4,
    pub files: FileStatusCounts,
    /// The project's language inventory, keyed by the lowercase language name.
    ///
    /// A `String` key rather than `ProgrammingLanguage`, deliberately: the closed
    /// set is already guaranteed by the `programming_language` `CHECK`, and reading
    /// the column as the enum here — as `worker/metrics.rs` must, since a metric
    /// *label* has to come from a set the server defines — would newly turn a
    /// language present in the `CHECK` but missing a `FromSql` arm into a 500 on
    /// this endpoint.
    pub languages: HashMap<String, LanguageStats>,
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

/// How a commit touched one path. Mirrors git's raw status letters, narrowed to
/// the five that name a path change; the SQLite CHECK carries the same set.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    /// `old_path` carries the source. Git detects these heuristically (`-M`), so
    /// a rename is a good guess, not a fact.
    Renamed,
    /// `old_path` carries the source, same caveat as `Renamed`.
    Copied,
}

impl ChangeType {
    pub fn name(self) -> &'static str {
        match self {
            ChangeType::Added => "added",
            ChangeType::Modified => "modified",
            ChangeType::Deleted => "deleted",
            ChangeType::Renamed => "renamed",
            ChangeType::Copied => "copied",
        }
    }

    /// Whether this change type must carry an `old_path`. Validation enforces
    /// the biconditional: a rename without a source is unusable, and a
    /// modification with one is a client bug worth surfacing.
    pub fn requires_old_path(self) -> bool {
        matches!(self, ChangeType::Renamed | ChangeType::Copied)
    }
}

impl rusqlite::ToSql for ChangeType {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.name()))
    }
}

impl rusqlite::types::FromSql for ChangeType {
    fn column_result(
        value: rusqlite::types::ValueRef<'_>,
    ) -> rusqlite::types::FromSqlResult<ChangeType> {
        match value.as_str()? {
            "added" => Ok(ChangeType::Added),
            "modified" => Ok(ChangeType::Modified),
            "deleted" => Ok(ChangeType::Deleted),
            "renamed" => Ok(ChangeType::Renamed),
            "copied" => Ok(ChangeType::Copied),
            _ => Err(rusqlite::types::FromSqlError::InvalidType),
        }
    }
}

/// One path a commit touched.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, ToSchema)]
pub struct CommitPath {
    /// Repo-relative, forward slashes — the same spelling `/index` uses, so the
    /// join back into the code channel is plain equality.
    pub path: UnixPath,
    pub change_type: ChangeType,
    /// Source path of a rename or copy; absent for every other change type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<UnixPath>,
}

/// One commit, as the client read it out of git.
#[derive(Deserialize, Serialize, Debug, Clone, ToSchema)]
pub struct CommitEntry {
    /// Full hex sha (40 for SHA-1 repositories, 64 for SHA-256), lowercase.
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    /// When the work was done. A rebase preserves this and moves `committed_at`.
    pub authored_at: i64,
    /// When this sha came to exist. Reconciliation windows are cut on this.
    pub committed_at: i64,
    /// Number of parents; `> 1` means a merge.
    pub parent_count: usize,
    /// First line of the message.
    pub subject: String,
    /// Everything after the first line; `""` when there is none.
    #[serde(default)]
    pub body: String,
    pub paths: Vec<CommitPath>,
}

/// `POST /v0/{guid}/history` body: the commits reachable from the refs the
/// client tracks, within `since`.
///
/// This is a **full-set replace within the window**, not an append: whatever the
/// server holds inside the window and this request does not name is dropped. A
/// sha is the hash of its own content, so there is no "same commit, different
/// bytes" case to detect — reconciliation is a set difference, and a force-push
/// or a rebase is simply one in which many shas orphan at once.
#[derive(Deserialize, Debug, ToSchema)]
pub struct HistoryRequest {
    /// Lower bound (unix seconds, on `committed_at`) of the window this request
    /// speaks for. `null` means "this is the whole history": anything the
    /// request does not name is dropped.
    ///
    /// Load-bearing for any windowed client: without it a run walking only the
    /// last month would delete everything older on every pass.
    #[serde(default)]
    pub since: Option<i64>,
    pub commits: Vec<CommitEntry>,
}

#[derive(Serialize, Debug, Default, PartialEq, Eq, ToSchema)]
pub struct HistoryResponse {
    /// Commits stored for the first time by this request.
    pub indexed: usize,
    /// Commits the server already held. Immutable content makes a re-post a
    /// no-op, so this is a real count and not a euphemism for "skipped".
    pub unchanged: usize,
    /// Commits dropped because the request did not name them — the force-push
    /// and history-rewrite signal.
    pub removed: usize,
}

/// `DELETE /v0/{guid}/history` query string — the retention bounds.
///
/// At least one is required, and that is the same rule the destructive file
/// endpoints follow: a wipe must be asked for, never arrived at by omitting a
/// parameter. `keep_last=0` is how you spell "drop the whole channel" out loud.
///
/// Given both, the bounds are **intersected, not unioned**: a commit is deleted
/// only when *both* condemn it. That is what makes `keep_last=200&older_than=…`
/// mean "prune old history but never leave me with fewer than 200 commits",
/// which is the reading a destructive endpoint should take when two rules
/// disagree.
#[derive(Deserialize, Debug, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryPruneQuery {
    /// Keep the newest N commits by `committed_at` whatever else is asked.
    /// Absent means no rank floor; `0` means keep none.
    pub keep_last: Option<usize>,
    /// Delete only commits committed strictly before this instant (unix
    /// seconds, UTC — the same clock `committed_at` and `since` are on, because
    /// two time spellings on one resource is worse than one unfriendly one).
    pub older_than: Option<i64>,
}

#[derive(Serialize, Debug, Default, PartialEq, Eq, ToSchema)]
pub struct HistoryPruneResponse {
    /// Commits deleted; their path rows went with them through CASCADE.
    pub removed: usize,
    /// Commits this project still holds — so the caller can see the effect
    /// without a second request, and see a bound that protected everything.
    pub remaining: usize,
}

/// One commit as `file_history` reports it.
#[derive(Serialize, Debug, Clone, PartialEq, Eq, ToSchema)]
pub struct CommitSummary {
    pub sha: String,
    /// First 8 characters of `sha` — what a human pastes into `git show`.
    pub short_sha: String,
    pub authored_at: i64,
    pub author_name: String,
    pub subject: String,
    pub body: String,
    /// How this commit touched the path that was asked about.
    pub change_type: ChangeType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<UnixPath>,
}

/// What `file_history` returns for one path.
///
/// The three flags exist so that an empty `commits` list is never ambiguous —
/// the same reason `outline` reports `indexed` separately from an empty symbol
/// list. Empty because nothing touched this file, empty because the project's
/// history was never reconciled, and empty because the run may not read here are
/// three different answers, and a bare `[]` reads as the first one.
#[derive(Serialize, Debug, PartialEq, Eq, ToSchema)]
pub struct FileHistoryResponse {
    pub path: UnixPath,
    /// Whether this project has **any** commits at all. `false` means the
    /// history channel was never reconciled for it — not that this file has no
    /// history.
    pub history_indexed: bool,
    /// Whether the path is inside the run's scope. `false` means the lookup was
    /// refused, not that it found nothing.
    pub in_scope: bool,
    /// Whether the path currently has a row in the code channel. A commit
    /// legitimately names paths the index does not hold — deleted long ago,
    /// excluded by `.mindex`, or in an unsupported language — and saying so is
    /// the difference between "gone" and "never there".
    pub path_indexed: bool,
    /// Newest first, capped; `total` says whether that cap bit.
    pub commits: Vec<CommitSummary>,
    /// Commits touching this path before the cap was applied.
    pub total: usize,
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
    /// The **separate** query-path embedder, present only when
    /// `[model].query_server_url` splits the workloads. Absent means one instance
    /// serves both and `embedder` already covers it. Counted in `status` when
    /// present: a dead query instance is every search failing, not a degradation
    /// of something optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_embedder: Option<String>,
    /// The local Ollama behind `/research`. **Optional dependency**: reported
    /// here but never counted in `status` — an error means research is
    /// unavailable while indexing and search keep working.
    pub ollama: String,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct HealthResponse {
    /// `"ok"` only when the required checks pass (SQLite, Qdrant, embedder, and
    /// the query embedder when deployed separately), else `"degraded"`.
    /// `checks.ollama` never affects it.
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

/// `GET /config` — server capabilities and tuning knobs. `languages` is the
/// canonical supported-language list (derived from the `ProgrammingLanguage` enum).
///
/// Almost all of this is fixed for the life of the process. The exception is
/// [`ResearchConfigInfo::models`], which a worker refreshes on a tick — so a client
/// that renders a model picker re-reads this endpoint instead of caching it once at
/// activation.
#[derive(Serialize, Debug, ToSchema)]
pub struct ConfigResponse {
    pub version: &'static str,
    pub model_id: String,
    pub languages: Vec<&'static str>,
    pub embed_batch: usize,
    pub db_pool_size: usize,
    pub stuck_grace_mins: i64,
    pub max_retries: i64,
    /// What `/search` accepts. Published for the reason the effort ladder is: a
    /// client that renders a bound has to get the bound from the server or it is
    /// guessing, and the VS Code form guessed `50` against a real ceiling of 100.
    pub search: SearchConfigInfo,
    /// What `/research` grants and allows. Published so clients render the real
    /// numbers instead of copies: three of them had drifted from the server's
    /// budgets independently before this existed.
    pub research: ResearchConfigInfo,
}

/// The `/search` request bounds, as served by `GET /config`.
///
/// These are the values [`crate::backend::v0::validate`] enforces at the edge, so a
/// client that renders a slider or a character counter from them cannot offer an
/// input the server will reject. `default_top_k` is what an omitted `top_k` becomes,
/// which is a different number from the one a client should preselect — it is
/// published so a client can say "server default" rather than invent a value.
#[derive(Serialize, Debug, ToSchema)]
pub struct SearchConfigInfo {
    pub default_top_k: u64,
    pub max_top_k: u64,
    pub max_query_bytes: usize,
}

/// The `/research` budgets, as served by `GET /config`.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchConfigInfo {
    /// Model used when a request omits one; empty means a request must name one.
    pub default_model: String,
    /// The models the server's Ollama has locally, as of `models_refreshed_at`.
    /// Refreshed on a `[research].models_refresh_interval_seconds` tick, so this
    /// endpoint is worth re-reading rather than caching once.
    ///
    /// Published so a client can offer a **closed** choice instead of a free-text
    /// field whose typo comes back as `ollama.unavailable` mid-run — the same reason
    /// the effort ladder is published instead of copied.
    ///
    /// Empty means either "Ollama has no models" or "Ollama has not been reached",
    /// and those are told apart by `models_refreshed_at`, never by the list.
    pub models: Vec<String>,
    /// Unix seconds of the last successful model-registry read; `null` = never
    /// succeeded (so an empty `models` says nothing about what Ollama has).
    pub models_refreshed_at: Option<i64>,
    pub effort: ResearchEffortLadder,
    /// Ceilings on a request's `budget` override.
    pub max_request_seconds: u64,
    pub max_request_tokens: u64,
    pub max_request_steps: usize,
    /// How long the report phase gets after the investigation deadline, in
    /// milliseconds. Published because it is the other half of what a caller waits:
    /// `effort.*.max_seconds` bounds the investigation, and the longest a request can
    /// take is that plus this.
    pub report_timeout_ms: u64,
    /// The sampling every research turn runs at.
    pub sampling: ResearchSamplingInfo,
}

/// `[research].temperature`/`top_p`/`seed` as served by `GET /config`.
///
/// Published for the same reason as the effort ladder: a client that has to
/// *record* what produced a run cannot read the server's TOML, and a harness that
/// takes the number from its own flag records what the operator meant to set
/// rather than what was in force. `null` means the key is unset, and Ollama uses
/// the model's own Modelfile default — which differs per model, and is exactly the
/// state in which a comparison between models measures Modelfiles.
///
/// `seed` is the configured *default*; a request's own `seed` overrides it, so a
/// run's seed is not necessarily this one.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchSamplingInfo {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchEffortLadder {
    pub low: ResearchEffortInfo,
    pub medium: ResearchEffortInfo,
    pub high: ResearchEffortInfo,
}

/// One effort level's budgets. `context_fraction` is reported but not
/// request-overridable — see [`ResearchBudgetOverride`].
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchEffortInfo {
    pub max_seconds: u64,
    pub max_tokens: u64,
    pub max_steps: usize,
    pub context_fraction: f64,
    /// Chunks each `search` tool call returns to the model. Not overridable either
    /// — published because a measurement harness must record what produced a run,
    /// and this is the one evidence-width knob that changes the answer.
    pub search_top_k: u64,
}

impl From<&crate::config::EffortBudget> for ResearchEffortInfo {
    fn from(b: &crate::config::EffortBudget) -> Self {
        Self {
            max_seconds: b.max_seconds,
            max_tokens: b.max_tokens,
            max_steps: b.max_steps,
            context_fraction: b.context_fraction,
            search_top_k: b.search_top_k,
        }
    }
}
