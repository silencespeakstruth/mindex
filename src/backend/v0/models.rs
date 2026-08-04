use glob::Pattern;
use rusqlite::{
    ToSql,
    types::{FromSql, FromSqlResult, ToSqlOutput, ValueRef},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
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
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, ToSchema)]
pub struct IndexResponse {
    /// `language → (path → chunk_count)`, covering only files actually (re)indexed.
    pub files: HashMap<ProgrammingLanguage, HashMap<UnixPath, u64>>,
}

/// `POST /v0/{project_guid}/index` query.
#[derive(Deserialize, Debug, Default, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct IndexQuery {
    /// `yes` switches the response to an SSE stream of per-file / per-batch
    /// indexing events (see the endpoint description); `no` or absent keeps the
    /// original one-shot JSON summary. An enum rather than a bool so the wire
    /// spelling is exactly `stream=yes|no` and anything else is a 400, not a
    /// silently-ignored truthy string.
    pub stream: Option<StreamChoice>,
}

/// `POST /v0/{project_guid}/research` and `.../research/{run_id}/challenge` query.
///
/// The same `?stream=yes|no` opt-in `/index` has, and for a sharper reason.
/// Research was the one endpoint whose response was *forced* into frames, which
/// makes disconnecting the cancellation interface — so a caller that issues the
/// request and does not read it to `done` spends the whole budget and receives
/// nothing, with no error raised anywhere. That is the default a naive caller
/// gets, and no amount of documentation reaches one that has not read it.
/// Streaming is worth asking for when the run is being watched; it is a trap when
/// it is not, so it is now the thing you ask for rather than the thing you get.
#[derive(Deserialize, Debug, Default, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ResearchQuery {
    /// `yes` streams the run as SSE (see the endpoint description); `no` or absent
    /// answers one [`ResearchResponse`] when the run ends. Same enum, same 400 on
    /// a typo, for the same reason it is an enum on `/index`.
    pub stream: Option<StreamChoice>,
}

/// The two spellings of `?stream=`. A typo is a 400 (`request.malformed_body`),
/// never a silent fallback to the JSON mode the caller did not ask for.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StreamChoice {
    Yes,
    No,
}

/// Why a posted file produced no work and is absent from the final counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Stored hash and derivation versions match — nothing to do. Under
    /// `symbols_only` this also covers a file whose hash no longer matches (its
    /// chunks are stale too, so symbols alone must not be rebuilt) — the server
    /// does not distinguish the two there.
    Unchanged,
    /// Another in-flight request holds this file's indexing claim.
    InFlight,
    /// A concurrent `POST /cancel` flipped the file after it was prepared.
    Cancelled,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Unchanged => "unchanged",
            SkipReason::InFlight => "in_flight",
            SkipReason::Cancelled => "cancelled",
        }
    }
}

/// One SSE event of a streaming `/index` request (`?stream=yes`). `name()`/`data()`
/// define the wire shape, which is mirrored in four places that must move
/// together: `post_index`'s doc comment, its OpenAPI 200 description, the
/// `mindex-index` CLI reader (`tools/indexer/src/client.rs`) and the VS Code
/// extension (`tools/vscode/src/api.ts`) — both consumers ignore what they don't
/// recognise, so a field added here and nowhere else is simply never seen.
///
/// The counts are chosen so a client can compute an honest chunks-per-second:
/// `embedded` fires once per GPU batch with a monotonic `chunks_done` /
/// `chunks_total` and the server's own `elapsed_ms`, which is exactly the pair a
/// windowed rate needs.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexEvent {
    /// The request was accepted; counts name what was posted, not what will be
    /// (re)indexed — unchanged files are discovered per file, later.
    /// `symbols_only` says what unit every later `count` is in (symbol rows
    /// instead of chunks), so a live display never has to guess.
    Started { files: usize, symbols_only: bool },
    /// Phase 1, one file hash-checked, sliced and its chunks inserted — now
    /// awaiting the shared embed pass. `chunks` is this file's chunk count.
    Prepared {
        path: String,
        language: ProgrammingLanguage,
        chunks: usize,
        symbols: usize,
    },
    /// A posted file that produced no work (absent from the final counts, exactly
    /// as it is absent from the JSON response).
    Skipped {
        path: String,
        language: ProgrammingLanguage,
        reason: SkipReason,
    },
    /// Phase 2, one embed batch encoded **and** upserted. `chunks_done` is
    /// cumulative across the whole request; `elapsed_ms` is measured from request
    /// start on the server's clock, so rates survive client-side buffering.
    Embedded {
        batch_chunks: usize,
        chunks_done: usize,
        chunks_total: usize,
        elapsed_ms: u64,
    },
    /// Phase 3, one file confirmed `indexed`. `count` is what the JSON response
    /// would report for it: chunks, or symbol rows under `symbols_only`.
    Indexed {
        path: String,
        language: ProgrammingLanguage,
        count: u64,
    },
    /// Terminal success (closes the stream). `files` is byte-for-byte the JSON
    /// mode's `IndexResponse.files`, so both modes tally identically; the totals
    /// beside it are a convenience for one-line summaries.
    Done {
        response: IndexResponse,
        files_indexed: usize,
        chunks: u64,
        elapsed_ms: u64,
    },
    /// Terminal failure after the stream started (the HTTP status is already
    /// 200). `code` is the same stable `ApiError` code the JSON mode would have
    /// carried in its problem+json body.
    Error { code: String, detail: String },
}

impl IndexEvent {
    pub fn name(&self) -> &'static str {
        match self {
            IndexEvent::Started { .. } => "started",
            IndexEvent::Prepared { .. } => "prepared",
            IndexEvent::Skipped { .. } => "skipped",
            IndexEvent::Embedded { .. } => "embedded",
            IndexEvent::Indexed { .. } => "indexed",
            IndexEvent::Done { .. } => "done",
            IndexEvent::Error { .. } => "error",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            IndexEvent::Started {
                files,
                symbols_only,
            } => json!({ "files": files, "symbols_only": symbols_only }),
            IndexEvent::Prepared {
                path,
                language,
                chunks,
                symbols,
            } => json!({
                "path": path,
                "language": language.name(),
                "chunks": chunks,
                "symbols": symbols,
            }),
            IndexEvent::Skipped {
                path,
                language,
                reason,
            } => json!({
                "path": path,
                "language": language.name(),
                "reason": reason.as_str(),
            }),
            IndexEvent::Embedded {
                batch_chunks,
                chunks_done,
                chunks_total,
                elapsed_ms,
            } => json!({
                "batch_chunks": batch_chunks,
                "chunks_done": chunks_done,
                "chunks_total": chunks_total,
                "elapsed_ms": elapsed_ms,
            }),
            IndexEvent::Indexed {
                path,
                language,
                count,
            } => json!({
                "path": path,
                "language": language.name(),
                "count": count,
            }),
            IndexEvent::Done {
                response,
                files_indexed,
                chunks,
                elapsed_ms,
            } => json!({
                // Serializing our own wire type cannot fail; Null would only ever
                // signal a bug in `IndexResponse`'s Serialize impl.
                "files": serde_json::to_value(&response.files).unwrap_or(Value::Null),
                "files_indexed": files_indexed,
                "chunks": chunks,
                "elapsed_ms": elapsed_ms,
            }),
            IndexEvent::Error { code, detail } => json!({ "code": code, "detail": detail }),
        }
    }
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

/// `POST /v0/{project_guid}/symbols` body — exact-name lookup over the
/// definitions extracted at indexing time.
///
/// **`deny_unknown_fields`, and the reason is specific to this body.** It used to
/// carry a `role` filter, dropped when references stopped being extracted. Without
/// this, a client still sending `role: "reference"` is answered `200` with the
/// *definitions* — the one wrong answer that costs nothing to detect and looks
/// exactly like a right one. A stale field is now `request.malformed_body`, which
/// says what happened.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolsRequest {
    /// Exact symbol name (case-sensitive), e.g. a function or type identifier.
    pub name: String,
    /// Restrict to one tags.scm kind (`function`, `method`, `class`, …).
    pub kind: Option<String>,
    /// Ranking anchor: candidates in this file rank first, then its directory,
    /// then the rest. No filtering — only ordering.
    pub anchor_path: Option<UnixPath>,
    /// Max results. Defaults to 20 when omitted.
    #[schema(default = 20, example = 20)]
    pub limit: Option<usize>,
    /// Keep only occurrences in files matching this selector. Same shape and
    /// semantics as `/search`'s, so a caller that has scoped one lookup can scope
    /// this one the same way.
    ///
    /// Rows outside the selector are dropped **and counted**
    /// (`out_of_scope_definitions`): a filtered list whose totals silently shrink is
    /// indistinguishable from a name that simply occurs less often, and `/symbols`
    /// calls its "no such symbol" answer definitive.
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
    pub total_definitions: u64,
    /// Definitions the selector excluded. Zero for an unscoped lookup, and reported
    /// rather than absorbed: "not found" and "found, outside what you asked for" are
    /// different answers, and only one of them means the name does not exist.
    #[serde(skip_serializing_if = "is_zero")]
    pub out_of_scope_definitions: u64,
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
    /// How much was in reach when the search found nothing: chunks searched, and
    /// the files they came from. `None` on a hit — the second scan is only worth
    /// paying for when it changes the answer.
    ///
    /// It changes the answer a great deal on a miss. "No indexed chunk contains
    /// this" and "nothing here was searchable" are different facts and were
    /// reported with one sentence, so a glob matching no file, or a scope holding
    /// none, read as proof that a literal does not exist anywhere. Two runs of the
    /// same question could then honestly report 0 and 5 occurrences of the same
    /// string. `file_history` answers this with three flags; `grep`'s version of
    /// the same honesty is a count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searched_chunks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searched_files: Option<u64>,
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

/// What a research or challenge run answers when the caller did not ask for
/// frames (`?stream=yes` absent) — the whole run in one body, once it has ended.
///
/// **Every field here is the payload of the frame of the same name, rendered by
/// the same `ResearchEvent::data()` the stream uses.** That is the entire design
/// and it is not an implementation detail: the alternative was a second
/// serialization of the report, the citations and the cost, which is a fifth copy
/// of a contract that already lives in four places and drifts by going quiet. So
/// this type documents *where* each object is described rather than describing it
/// again, and the `*_wire_fields_are_stable` tests keep covering both modes.
///
/// What is absent is as deliberate: `thinking`, `step` and `progress` are dropped.
/// A caller that wanted to watch the run asks for the stream; one that did not
/// wanted the answer, and the trace is exactly the volume it did not want. The
/// step count survives on `done`, and the full trace is journalled — see
/// `GET /projects/{project_guid}/research/{run_id}`.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchResponse {
    /// The run's name for its whole life — what `GET /research/active` lists and
    /// `DELETE /research/active/{run_id}` cancels. Always present, unlike
    /// `done.run_id`, which is null when the best-effort journal write failed:
    /// this one names the run that happened, that one names the row it became.
    pub run_id: String,
    /// The model that drove the loop, resolved (the request's, or the server's
    /// `[research].default_model`).
    pub model: String,
    /// The effort level, and what it granted. The `started` frame's fields.
    pub effort: String,
    pub granted_seconds: u64,
    /// `max_seconds * 1000 + report_timeout_ms` — the longest this call could
    /// legitimately have taken. Reported after the fact because it is what makes
    /// an elapsed time readable.
    pub worst_case_ms: u64,
    /// The Markdown report: every `summary` delta, concatenated. Empty only when
    /// the run produced nothing at all, which the `done.reason` explains.
    pub report: String,
    /// The `citations` frame — the server's provenance and freshness check on the
    /// report above. Absent only if the run ended before it was scored.
    #[schema(value_type = Object)]
    pub citations: Option<serde_json::Value>,
    /// The `excerpts` frame — the indexed code at every verified citation. Absent
    /// when nothing verified, and best-effort besides.
    #[schema(value_type = Object)]
    pub excerpts: Option<serde_json::Value>,
    /// The `verdict` frame. Challenge runs only; always absent for
    /// `POST /research`, exactly as the frame is.
    #[schema(value_type = Object)]
    pub verdict: Option<serde_json::Value>,
    /// The `done` frame — `reason`, `prompt_version`, `run_id`/`seq` and the run's
    /// full cost. Always present: a body without it would be a run that never
    /// terminated, which is a 500 here rather than a partial answer.
    #[schema(value_type = Object)]
    pub done: serde_json::Value,
}

/// `POST /v0/{project_guid}/research` body. The response is one
/// [`ResearchResponse`], or an SSE stream under `?stream=yes`; see the endpoint
/// description for the event contract.
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
    /// Earlier runs of this project whose reports are handed to this one as
    /// background, in the order given. Ids come from `done.run_id` or from
    /// `GET /projects/{guid}/research`.
    ///
    /// The reports are injected into the transcript before the model plans, so they
    /// can supply the identifiers a cold run would have to spend steps discovering.
    /// They are **not** evidence: nothing in them can be cited, and a citation copied
    /// out of one is reported to the reader as `unverified`, exactly as if the model
    /// had invented it.
    ///
    /// Capped by `[research].max_context_runs` (400
    /// `validation.research_context_too_many`); an id that is not a run of this
    /// project is a 404 `research.run_not_found`.
    pub context_run_ids: Option<Vec<String>>,
}

/// `POST /v0/{project_guid}/research/{run_id}/challenge` body — the same knobs
/// as `ResearchRequest` minus what the subject supplies: the question comes from
/// the stored run, the scope is the subject's own (a challenge may read exactly
/// what its subject could, and nothing more), and prior context is the subject
/// itself.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChallengeRequest {
    /// Ollama model to drive the opponent. Falls back to
    /// `[research].default_model`; 400 if both are absent. Challenging with a
    /// *different* model than wrote the subject is the interesting experiment.
    pub model: Option<String>,
    /// Challenge depth — the same `[research.effort.*]` ladder.
    #[schema(value_type = String, example = "medium")]
    pub effort: crate::research::Effort,
    /// Per-request budget override, identical semantics to `POST /research`.
    pub budget: Option<ResearchBudgetOverride>,
    /// RNG seed, as on `POST /research`.
    pub seed: Option<i64>,
}

/// Per-request overrides for the `effort` preset.
///
/// Two axes are deliberately absent. `context_fraction` is a guard against
/// Ollama silently trimming the transcript on a small-window model, not a
/// quality lever, so raising it per request buys nothing but truncation.
/// `search_top_k` was measured not to be the fix for the failure it looks like
/// (runs lose on query formulation, not evidence width); it stays a config knob
/// for a measurement harness. Both stay in config.
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
    /// Ceiling, in words, announced to the model for the final report. `0` = say
    /// nothing about length; a non-zero value must be at least 150. Capped by
    /// `[research].max_request_report_words` (400
    /// `validation.research_shape_out_of_range` outside the range).
    pub max_report_words: Option<usize>,
    /// Report sections the run may write — also the upper bound the plan turn
    /// asks for ("3-N sub-questions"). At least 3 (the sectioning threshold),
    /// capped by `[research].max_request_report_sections`. Sections share one
    /// fixed report window: past its capacity, extra sections ship as stubs.
    pub max_report_sections: Option<usize>,
    /// Investigate this many steps, then spend one turn banking what is already
    /// answerable. `0` = no checkpoints for this run; a non-zero value must be
    /// at least 2 and no more than `[research].max_request_steps` (an interval
    /// above the step budget is `0` spelled differently). Overrides
    /// `[research].checkpoint_every_steps`.
    pub checkpoint_every_steps: Option<usize>,
    /// Multiplier on the per-call evidence widths (`read_chunks`, `grep`,
    /// `file_history`, `symbols`). `1` = the historical widths;
    /// capped by `[research].max_evidence_width`. Width is paid in prompt
    /// tokens on every later turn — it compounds into `max_tokens`.
    pub evidence_width: Option<u64>,
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
    /// Stored research runs reaped for having passed their `expires_at`. A **pinned**
    /// run (`expires_at` null) has no expiry and is never counted here.
    pub research_runs_pruned: usize,
    /// Phases that did not run to completion — `chunks`, `files`, `status_log`,
    /// `research` — empty when the pass was clean.
    ///
    /// Without it every count above is ambiguous: each phase mapped its errors to `0`,
    /// so a 200 full of zeros meant either "nothing needed collecting" or "collection
    /// is broken", and a caller had no way to tell. The counts are still real — a phase
    /// that failed part-way reports what it did manage — so this is the field that says
    /// whether they are the whole story. The reasons are in the server log; this names
    /// where to look.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_phases: Vec<String>,
}

/// `GET /projects/{guid}/research` query. **Keyset**, not offset: pages are
/// resumed from `before_seq`, so a run written or reaped between two pages cannot
/// make the reader skip or repeat a row the way `OFFSET` would.
#[derive(Deserialize, Debug, Default, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ResearchListQuery {
    /// Case-insensitive substring of the title, the question **or** the report
    /// body. Literal — `_` and `%` are escaped, so searching for `read_chunks`
    /// cannot also match `readXchunks`.
    pub q: Option<String>,
    /// Return runs whose `seq` is strictly below this. Absent = the newest page.
    /// Take it from the previous response's `next_before_seq`.
    pub before_seq: Option<i64>,
    /// Page size, capped by `[research].list_page_limit` (400
    /// `validation.research_list_limit_out_of_range` above it).
    pub limit: Option<usize>,
    /// `all` (default), `fresh`, or `stale` — filtered before the page is cut, so a
    /// full page always means there may be more.
    pub freshness: Option<ResearchFreshness>,
    /// Restrict to pinned (`true`) or unpinned (`false`) runs.
    pub pinned: Option<bool>,
    /// Restrict to fully-valid (`true`) or invalid (`false`) runs — validity being
    /// the transitive verdict, not just the run's own staleness (that is
    /// `freshness`). Filtered before the page is cut, like `freshness`.
    pub valid: Option<bool>,
    /// Restrict to ordinary `research` runs or to `challenge` runs. Filtered before
    /// the page is cut, like the rest.
    pub kind: Option<ResearchKind>,
    /// Restrict to challenges aimed at this run id — "what was said about *that*
    /// report". Served by `idx_research_runs_challenged`.
    ///
    /// The inverse direction of `challenged_seq`/`challenged_title`, and needed
    /// separately because it is an id lookup that must answer regardless of which
    /// page a client holds, and must include the stale and inconclusive
    /// challenges that `trust` deliberately stops counting.
    pub challenged_run_id: Option<String>,
    /// `all` (default), `finalized`, or `partial` — whether the run reached its
    /// own conclusion or was stopped by a budget. Filtered before the page is
    /// cut, like the rest.
    ///
    /// Server-side rather than a client-side test on `done_reason` because a
    /// client that pages this list to exhaustion (to select or prune everything
    /// matching a filter) can only trust "a full page means there may be more" if
    /// every filter applied before the `LIMIT`.
    pub completeness: Option<ResearchCompleteness>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResearchFreshness {
    All,
    Fresh,
    Stale,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResearchKind {
    Research,
    Challenge,
}

/// Whether a run reached its own conclusion or was stopped by a budget.
///
/// `Partial` is `done_reason <> 'finalized'` — every stop reason at once, rather
/// than a filter per reason: to a reader pruning a corpus the distinction between
/// running out of time and running out of tokens is not one they act on, and a
/// per-reason filter would have to grow a variant every time `DoneReason` does.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResearchCompleteness {
    All,
    Finalized,
    Partial,
}

/// One stored run, without its report. The list is a separate endpoint precisely so
/// that a page of fifty runs does not carry fifty reports.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchRunSummary {
    /// Stable identity — what every per-run endpoint keys on.
    pub id: String,
    /// Per-project ordinal: short enough to type, and the keyset cursor. **Not**
    /// identity: it is renumbered if a project's runs are ever wiped entirely.
    pub seq: i64,
    /// The report's own heading when the run stored one, else derived from
    /// `question` — see the handler. Never null on the wire.
    pub title: String,
    pub question: String,
    pub created_at: i64,
    /// When `/gc` may reap this run. `null` = **pinned**, never reaped.
    pub expires_at: Option<i64>,
    pub pinned: bool,
    pub model: String,
    pub effort: String,
    pub done_reason: String,
    pub citations_total: i64,
    pub citations_verified: i64,
    pub citations_unverified: i64,
    pub steps: i64,
    pub elapsed_ms: i64,
    /// Files the run read and recorded a baseline for.
    pub files_total: i64,
    /// How many of those have changed or left the index since. `0` means the report
    /// still describes what is there.
    pub files_moved: i64,
    /// `files_moved > 0` — the same `changed || removed` the live run's own freshness
    /// probe means by stale.
    pub stale: bool,
    /// Derived validity, never stored: the run itself is fresh AND every run in its
    /// transitive context still exists and is itself valid. A deleted or GC-reaped
    /// ancestor therefore invalidates the whole chain the moment it goes, with no
    /// write anywhere.
    pub valid: bool,
    /// Why `valid` is false: `stale` (its own files moved), `context_deleted` (a
    /// run in its context chain was deleted or reaped), or `context_invalid` (an
    /// ancestor is itself stale or broken). `null` when valid.
    pub invalid_reason: Option<&'static str>,
    /// How many runs this one was launched **on** — the length of its own
    /// `context_run_ids`, so *direct* dependencies only.
    ///
    /// Deliberately not the same number as `context.len()`, which is the
    /// *transitive* ancestry: a run built on one report that was itself built on
    /// three has `references_count = 1` and four entries in `context`. The direct
    /// count is what a human chose; the transitive list is what they inherited,
    /// and conflating them makes a shallow run look deep.
    pub references_count: i64,
    /// How many other runs of this project name this one in their context —
    /// direct edges in, counted across the whole corpus rather than the page.
    ///
    /// The other half of "is this report load-bearing", and the number that makes
    /// a delete confirmation honest: removing a run invalidates every descendant,
    /// so a caller is owed the count before they agree to it.
    pub referenced_by_count: i64,
    /// The run's transitive context ancestry, flattened and deduplicated —
    /// ascending `seq`, deleted entries last. What lets a human pick context with
    /// confidence: every report this one leaned on, each with its own state.
    pub context: Vec<ResearchRunDependency>,
    /// `research` or `challenge` — whether this run answered a question or
    /// attacked another run's report.
    pub kind: String,
    /// For a challenge: the run it attacked. May name a run that no longer
    /// exists (no FK by design); `null` on research runs.
    pub challenged_run_id: Option<String>,
    /// For a challenge: its overall verdict over the subject's claims
    /// (`confirmed`/`disputed`/`refuted`), or `null` — inconclusive, which is
    /// **not** an acquittal. `null` on research runs.
    pub challenge_verdict: Option<String>,
    /// The derived trust status of THIS run, from valid challenges aimed at it:
    /// `refuted` > `disputed` > `confirmed` > `unchallenged` (severity wins; a
    /// stale challenge stops counting automatically; an inconclusive one counts
    /// toward none). Derived at read time like `valid` — nothing is stored.
    pub trust: String,
    /// For a challenge: the **subject's** per-project ordinal, resolved
    /// server-side. `null` on a research run, and on a challenge whose subject
    /// has since been deleted — which is the only thing that null now means.
    ///
    /// Here because a challenge row has to name what it attacked wherever it is
    /// rendered, and a client cannot resolve it: `challenged_run_id` is a uuid,
    /// and the subject is very often not on the page the client happens to hold.
    pub challenged_seq: Option<i64>,
    /// For a challenge: the subject's title, built by the same stored-heading-else
    /// -derived-from-question rule as `title`, so the two spellings of one report
    /// cannot disagree. `null` under the same conditions as `challenged_seq`.
    pub challenged_title: Option<String>,
}

/// One run in another run's context chain (direct or transitive).
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchRunDependency {
    /// The id as recorded at launch — always present, even when the run is gone.
    pub id: String,
    /// `null` when the run no longer exists.
    pub seq: Option<i64>,
    /// Stored-or-derived title; `null` when the run no longer exists (render a
    /// "deleted report" marker).
    pub title: Option<String>,
    /// `valid`, `invalid`, or `deleted`.
    pub state: &'static str,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchRunListResponse {
    pub runs: Vec<ResearchRunSummary>,
    /// Pass as `before_seq` for the next page. `null` when the page came back short,
    /// which is how a client knows to stop without spending a request to find out.
    pub next_before_seq: Option<i64>,
    /// Corpus-wide counts for the project — see [`ResearchCorpusTotals`].
    pub totals: ResearchCorpusTotals,
}

/// How big this project's stored-research corpus is, and how much of it is worth
/// keeping.
///
/// **Deliberately unaffected by every filter on the request** — `q`, `freshness`,
/// `valid`, `pinned`, `kind`, `completeness` and `before_seq` all move `runs` and
/// none of them move these. They are a fixed denominator: "74 of 128 current"
/// answers a question the page in front of the reader cannot, and a count that
/// shrank as the reader typed into the search box would be a worse rendering of
/// `runs.len()`.
///
/// One extra `SELECT` inside the transaction the page already runs, reusing the
/// same recursive validity CTE — so this costs the CTE nothing and the report
/// bodies nothing (none is selected here either).
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchCorpusTotals {
    /// Every stored run of this project, of either kind.
    pub total: i64,
    /// How many are **valid** — the same transitive predicate `ResearchRunSummary
    /// .valid` reports and the same one the server enforces on `context_run_ids`.
    /// So this is literally "how many of these could be handed to the next
    /// question", not a softer notion of freshness.
    pub current: i64,
    /// How many of `total` are challenges rather than research runs. A corpus is
    /// two populations with different lifecycles — a challenge is *about* another
    /// report — and "128 reports" without this says nothing about how many
    /// questions were actually asked.
    pub challenges: i64,
    /// How many have at least one baseline file changed or removed since, **pinned
    /// included**. Deliberately not `gc_stale`, which is unpinned-only because it
    /// feeds a delete proposal: as a denominator that exemption would under-report
    /// the corpus's actual drift.
    pub stale: i64,
    /// The union of the four buckets below: how many **unpinned** runs a
    /// garbage-collection pass would propose deleting. A run in several buckets is
    /// counted once here, so this is never the sum of the four.
    pub gc_candidates: i64,
    /// Unpinned and invalid — the server already refuses these as context.
    pub gc_invalid: i64,
    /// Unpinned, with at least one baseline file changed or gone.
    pub gc_stale: i64,
    /// Unpinned, stopped by a budget rather than finished (`done_reason` is not
    /// `finalized`), so the report rests on partial evidence.
    pub gc_partial: i64,
    /// Unpinned challenges whose verdict turn produced nothing parseable. They
    /// count toward no trust verdict and carry no finding.
    pub gc_inconclusive: i64,
}

/// What became of one file the run read.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchRunFile {
    pub path: String,
    /// The index's hash when the run first read it.
    pub sha256: String,
    /// The index's hash now; `null` when the file is no longer indexed.
    pub current_sha256: Option<String>,
    /// `fresh`, `changed` or `removed`. Three values rather than a boolean because a
    /// file that was deleted and a file that was edited call for different reading.
    pub state: &'static str,
}

/// One stored run in full, including the report.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchRunDetail {
    #[serde(flatten)]
    pub summary: ResearchRunSummary,
    /// The report, as Markdown — the thing the whole table exists to keep.
    pub report: String,
    pub prompt_version: String,
    /// Earlier runs that were fed to this one as context.
    pub context_run_ids: Vec<String>,
    /// The run's file scope as it was described to the model; `null` if unscoped.
    pub scope: Option<String>,
    /// Per-file freshness — the honest form of `stale`.
    pub files: Vec<ResearchRunFile>,
}

/// One citation-verdict tally, for the verification endpoint: the same five
/// counters the run journalled, in the same buckets.
#[derive(Serialize, Debug, PartialEq, Eq, ToSchema)]
pub struct CitationCounts {
    pub total: i64,
    pub verified: i64,
    pub path_only: i64,
    pub unverified: i64,
    /// Orthogonal to the three verdicts, as everywhere else.
    pub stale: i64,
}

/// `GET /projects/{guid}/research/{run_id}/verification` — the stored report's
/// citations re-checked against the journal and today's index. Pure SQLite:
/// no model, no GPU, safe to call as often as staleness matters.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchVerification {
    pub run_id: String,
    pub seq: i64,
    /// Derived validity, the same predicate the list/detail endpoints compute.
    pub valid: bool,
    /// Why `valid` is false; `null` when valid.
    pub invalid_reason: Option<&'static str>,
    /// Whether the run was journalled with its evidence spans (v1.3.0+). Runs
    /// stored before that can only be re-checked for staleness — recomputing
    /// their provenance would score every citation `unverified` and read as a
    /// degraded report, which would be the check lying.
    pub spans_available: bool,
    /// The counters the run itself journalled, verbatim.
    pub recorded: CitationCounts,
    /// The same check re-run today. Provenance must equal `recorded`'s (report
    /// and spans are immutable); `stale` is computed against the index as it
    /// stands **now** — the number that moves, and the reason to call this.
    /// `null` when `spans_available` is false.
    pub recomputed: Option<CitationCounts>,
    /// `recomputed`'s provenance == `recorded`'s (total/verified/path_only/
    /// unverified; `stale` deliberately excluded — it is *expected* to move).
    /// `false` means the journal and the re-check disagree about immutable
    /// facts: report a bug. `null` when `spans_available` is false.
    ///
    /// The report is scored both with and without citation path resolution, and
    /// either match satisfies this: resolution arrived with `PROMPT_VERSION` 2.4 and
    /// changes a bare filename's verdict, so a run journalled before it would
    /// otherwise read as broken. For a report carrying such a citation the check is
    /// correspondingly weaker — it can no longer tell the two scorings apart.
    pub provenance_matches: Option<bool>,
    /// Citation staleness against today's index, computable for every run —
    /// old rows included — from the baselines alone.
    pub stale_citations_now: i64,
    /// The cited paths behind that count, deduplicated and capped like the
    /// run's own `stale_paths`.
    pub stale_paths_now: Vec<String>,
    /// Baseline currency, the same numbers the summary carries: how many files
    /// the run read, and how many have changed or left the index since.
    pub files_total: i64,
    pub files_moved: i64,
}

/// `DELETE /projects/{guid}/research` body — the ids to drop.
///
/// A list rather than the `include`/`exclude` selector the file endpoints take:
/// a run is not a path, and the only thing a caller ever wants to remove is the
/// set it just picked out of a list it was looking at. The `require_nonempty_selector`
/// rule still applies (an empty list is a **400**, not a whole-project wipe) —
/// clearing the corpus is asked for by naming every id, never arrived at by
/// forgetting a field.
#[derive(Deserialize, Serialize, Debug, Default, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteResearchRunsRequest {
    /// Run ids, as returned by the list endpoint. Unknown ids are ignored, so the
    /// call is idempotent the way the single-run delete is.
    pub ids: Vec<String>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct DeleteResearchRunsResponse {
    /// How many rows were actually removed — never more than `ids.len()`, and
    /// less when some were already gone.
    pub deleted_runs: u64,
}

/// `POST /projects/{guid}/research/{run_id}/pin` body.
#[derive(Deserialize, Serialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResearchPinRequest {
    /// `true` clears the expiry outright. `false` restores
    /// `created_at + [research].retention_days` — so unpinning a run older than the
    /// window makes it eligible at the very next sweep, which is what "let it age
    /// normally" honestly means.
    ///
    /// Defaults to `true`, so `POST …/pin` with a body of `{}` pins. It was
    /// required, which made the obvious call — pin this run — a 400 naming a field
    /// the caller had no reason to guess, on an endpoint whose own name already
    /// says what it does. Unpinning is the surprising direction and is the one that
    /// must be spelled out.
    #[serde(default = "pinned_by_default")]
    pub pinned: bool,
}

fn pinned_by_default() -> bool {
    true
}

#[derive(Serialize, Debug, ToSchema)]
pub struct VersionResponse {
    pub version: &'static str,
    /// Applied `PRAGMA user_version` — the highest migration version in the running binary.
    pub db_schema_version: i32,
}

/// One dependency's liveness. Two values and no third.
///
/// The **reason** a probe failed is deliberately not here. This response is
/// readable by anything that can reach the port, and a driver's error string
/// carries file paths, URLs, ports and version strings; it is also the wrong
/// thing to render at a user, who can act on "Qdrant is not answering" and
/// cannot act on a tonic transport chain. The reason goes to a `warn!` at the
/// probe site, with a sysadmin hint — the one place it can be acted on.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckState {
    Ok,
    Error,
}

/// The health verdict.
///
/// Tri-state because "one optional dependency is down" and "the service cannot
/// do its job" used to be the same word, which made the word useless: every
/// client had to decide for itself whether `degraded` was worth disabling
/// anything over, and to do that it needed its own copy of which check is
/// required. Now the server answers that, since it is the only party that knows.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Ok,
    Degraded,
    Unhealthy,
}

/// Per-dependency liveness. Names, never reasons — see [`CheckState`].
#[derive(Serialize, Debug, ToSchema)]
pub struct HealthChecks {
    pub sqlite: CheckState,
    pub qdrant: CheckState,
    pub embedder: CheckState,
    /// The **separate** query-path embedder, present only when
    /// `[model].query_server_url` splits the workloads. Absent means one instance
    /// serves both and `embedder` already covers it. Counted in `status` when
    /// present: a dead query instance is every search failing, not a degradation
    /// of something optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_embedder: Option<CheckState>,
    /// The local Ollama behind `/research`. **The one optional dependency**, and
    /// therefore the sole producer of `degraded`: an error means research is
    /// unavailable while indexing and search keep working. It can never produce
    /// `unhealthy`.
    pub ollama: CheckState,
}

impl HealthChecks {
    /// The verdict, computed next to the data so nothing can compute it a second
    /// way and disagree.
    ///
    /// `unhealthy` beats `degraded`: a caller acting on the milder word while a
    /// required dependency is dead is the whole failure this vocabulary exists to
    /// prevent, so severity wins over recency and over count.
    pub fn verdict(&self, wedged: bool) -> HealthStatus {
        let required_failed = [
            Some(self.sqlite),
            Some(self.qdrant),
            Some(self.embedder),
            self.query_embedder,
        ]
        .into_iter()
        .flatten()
        .any(|c| c == CheckState::Error);

        if required_failed || wedged {
            HealthStatus::Unhealthy
        } else if self.ollama == CheckState::Error {
            HealthStatus::Degraded
        } else {
            HealthStatus::Ok
        }
    }
}

/// Research admission, as `GET /health` reports it.
///
/// This block exists because of a real outage shape: with `max_concurrent = 1` a
/// single occupied slot is a total outage of `/research`, and health said `"ok"`
/// throughout — every *dependency* was alive, and the pool of slots, the thing that
/// was actually exhausted, was not checked at all. A health endpoint that is green
/// while the main scenario is dead is worse than none: it actively misleads.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchHealth {
    /// `[research].max_concurrent`.
    pub slots_total: usize,
    /// Slots held right now. `slots_busy == slots_total` means the next request is
    /// refused with 429 `research.busy` — normal under load, not a defect.
    pub slots_busy: usize,
    /// Age of the oldest live run, `null` when none is running. Busy says nothing
    /// on its own; this is what separates a queue from a wedge.
    pub oldest_inflight_age_ms: Option<u64>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct HealthResponse {
    /// `ok` when every check passes; `degraded` when only the optional Ollama is
    /// failing (indexing and search still work, `/research` does not);
    /// `unhealthy` when a required check failed — SQLite, Qdrant, the embedder,
    /// or the query embedder when deployed separately.
    ///
    /// Research reaches this in exactly one narrow case: a run that has outlived
    /// `max_seconds + report_timeout_ms` is holding a slot no deadline of its own
    /// will free, and that is `unhealthy`. A merely *busy* slot never moves the
    /// verdict — that is the service working.
    pub status: HealthStatus,
    pub version: &'static str,
    /// Files in `status='indexing'` across *all* projects right now.
    pub indexing_files: i64,
    pub checks: HealthChecks,
    /// Research concurrency. Reported unconditionally, including when Ollama is
    /// down: the slots are mindex's own state, not a dependency's.
    pub research: ResearchHealth,
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

/// `GET /research/active` — one research run that is still running.
///
/// Deliberately separate from [`ResearchRunSummary`], which describes a **stored**
/// run: a live run has no `seq`, no `done_reason` and no report, and giving it
/// nullable versions of all three would invite a client to render one shape for two
/// different things.
#[derive(Serialize, Debug, ToSchema)]
pub struct ActiveResearchRun {
    /// The id `DELETE /research/active/{run_id}` takes, and the id this run will be
    /// stored under if it finishes and its journal write succeeds.
    pub run_id: String,
    pub project_guid: String,
    /// The opening of the question, so a human recognises their own run.
    pub question: String,
    pub model: String,
    pub effort: String,
    /// Unix seconds when the run was admitted.
    pub started_at: i64,
    /// How long it has been running.
    pub age_ms: u64,
    /// The investigation deadline it was granted (`budget.max_seconds`).
    pub granted_seconds: u64,
    /// `granted_seconds * 1000 + report_timeout_ms` — the longest this run may
    /// legitimately take, since the report phase gets its own window *after* the
    /// investigation deadline. An `age_ms` past this is a defect, not a queue, and
    /// it is what the watchdog and `GET /health` both act on.
    pub worst_case_ms: u64,
}

/// `GET /research/active` — every research run holding a concurrency permit.
#[derive(Serialize, Debug, ToSchema)]
pub struct ActiveResearchResponse {
    /// Oldest first: the order that puts a suspected wedge at the top.
    pub runs: Vec<ActiveResearchRun>,
    /// `[research].max_concurrent`.
    pub slots_total: usize,
    /// Slots held right now. Equal to `runs.len()`, restated so a caller planning a
    /// queue reads one number instead of counting.
    pub slots_busy: usize,
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
    /// Empty means "Ollama has no models", "Ollama has not been reached", or
    /// "everything Ollama has is outside `allowed_models`" — the first two are
    /// told apart by `models_refreshed_at`, the third by `allowed_models`, never
    /// by the list. When `allowed_models` is non-empty this list is already
    /// filtered to it, so a client's picker offers only what a request may name.
    pub models: Vec<String>,
    /// The `[research].allowed_models` glob patterns bounding which models
    /// `/research` will run; empty = unrestricted. A request naming a model
    /// outside them is refused with 400 `research.model_not_allowed`.
    pub allowed_models: Vec<String>,
    /// Unix seconds of the last successful model-registry read; `null` = never
    /// succeeded (so an empty `models` says nothing about what Ollama has).
    pub models_refreshed_at: Option<i64>,
    pub effort: ResearchEffortLadder,
    /// Ceilings on a request's `budget` override. `max_request_steps` also caps
    /// the `checkpoint_every_steps` override — an interval above the step budget
    /// is `0` spelled differently, so it has no ceiling of its own.
    pub max_request_seconds: u64,
    pub max_request_tokens: u64,
    pub max_request_steps: usize,
    /// Ceiling on `budget.max_report_sections` (floor: 3, the sectioning
    /// threshold).
    pub max_request_report_sections: usize,
    /// Ceiling on `budget.max_report_words` (floor for non-zero values: 150;
    /// `0` = announce no length).
    pub max_request_report_words: usize,
    /// Ceiling on `budget.evidence_width` (floor: 1).
    pub max_evidence_width: u64,
    /// `[research].max_concurrent` — how many runs this server admits at once.
    ///
    /// Published because a caller could otherwise learn it only by being refused:
    /// there was no way to tell "one slot, so queue your two investigations" from
    /// "plenty of slots, run them together", and on a single-GPU host the answer is
    /// almost always 1. A 429 `research.busy` means this many are already running.
    pub max_concurrent: usize,
    /// How many earlier runs one request may name in `context_run_ids`; `0` = the
    /// feature is off and any id is a 400.
    pub max_context_runs: usize,
    /// Total characters of prior reports injected into a run's transcript. A real
    /// budget axis — the transcript is resent every turn — so a caller chaining
    /// follow-ups is choosing how much of `max_tokens` to spend on hearsay.
    pub max_context_chars: usize,
    /// How long the report phase gets after the investigation deadline, in
    /// milliseconds. Published because it is the other half of what a caller waits:
    /// `effort.*.max_seconds` bounds the investigation, and the longest a request can
    /// take is that plus this.
    pub report_timeout_ms: u64,
    /// Steps between the turns that bank what the run can already answer; `0` = off.
    /// Published because it comes out of the same step budget `effort.*.max_steps`
    /// grants — a harness comparing coverage across runs has to know how many of
    /// those steps went to writing rather than looking.
    pub checkpoint_every_steps: usize,
    /// `[research].list_page_limit` — the default and maximum page size of
    /// `GET /projects/{guid}/research`. Published so a client paging the corpus
    /// can size its loop instead of guessing: guessing 50 against a real 100
    /// doubles the request count for no reason, and guessing high is a 400.
    pub list_page_limit: usize,
    /// `[limits].max_research_delete_ids` — how many run ids one
    /// `DELETE /projects/{guid}/research` accepts. From `[limits]`, not
    /// `[research]`, but published here because the endpoint it bounds is a
    /// research one and a client offering "select everything matching this
    /// filter" has to know where to stop and say so.
    pub max_delete_ids: usize,
    /// The sampling every research turn runs at.
    pub sampling: ResearchSamplingInfo,
    /// What runs at each `(model, effort)` have actually cost on this server
    /// lately. See [`ResearchObservedInfo`].
    pub observed: ResearchObservedInfo,
}

/// Measured cost of a research run, as served by `GET /config`.
///
/// The effort ladder says what a level **grants** — `high` allows an hour. Nothing
/// said what a level **takes**, and the two are not close: measured here, `high`
/// runs finish in about seven minutes against that hour. A caller had no way to
/// price a level before choosing it, which is how `effort: high` ends up on a
/// question that reads one dictionary literal, and how a caller queues two
/// investigations without knowing whether that is fifteen minutes or two hours.
///
/// Percentiles over real runs from the journal, per `(model, effort)` because a
/// 31B model and a 3B model at the same level are not the same wait. A pair with
/// too few runs to say anything is simply absent — a client with no row falls back
/// to the grant.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchObservedInfo {
    /// Unix seconds of the last successful read; `null` = never read, which is a
    /// different statement from "no runs recorded" (an empty `efforts`).
    pub refreshed_at: Option<i64>,
    pub efforts: Vec<ResearchObservedEffort>,
}

/// One `(model, effort)` pair's measured cost.
#[derive(Serialize, Debug, ToSchema)]
pub struct ResearchObservedEffort {
    pub model: String,
    pub effort: String,
    /// Runs the estimate is built from — the basis for trusting it.
    pub runs: usize,
    /// Typical wall clock, end to end. This is the number to show a user waiting.
    pub p50_seconds: u64,
    /// The slow tail. Compare against the level's `worst_case_seconds`: the gap
    /// between them is how much of the grant is headroom rather than expectation.
    pub p90_seconds: u64,
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
    /// Word ceiling announced to the model for the final report; `0` = no length
    /// is announced. Overridable per request (`budget.max_report_words`, capped
    /// by `max_request_report_words`) — published so a client can render the
    /// preset a run falls back to when it does not override.
    pub max_report_words: usize,
    /// Report sections the run may write — the default a request's
    /// `budget.max_report_sections` overrides, capped by
    /// `max_request_report_sections`.
    pub max_report_sections: usize,
    /// Multiplier on the per-call evidence widths — the default a request's
    /// `budget.evidence_width` overrides, capped by `max_evidence_width`.
    pub evidence_width: u64,
    /// `max_seconds + report_timeout_ms / 1000` — the longest a request at this
    /// level may take before its last frame.
    ///
    /// Derived, and published because deriving it is exactly what callers were not
    /// doing: `max_seconds` and `report_timeout_ms` bound **different phases**, so
    /// neither is the answer to "how long might I wait", and reading `max_seconds`
    /// as the whole wait understates `high` by five minutes. Stated once, by the
    /// server that owns both numbers.
    pub worst_case_seconds: u64,
}

impl ResearchEffortInfo {
    /// Built with `report_timeout_ms` rather than through `From` so the derived
    /// worst case cannot be forgotten: the report window is not part of
    /// [`crate::config::EffortBudget`], and a conversion that could not see it is
    /// how the two halves stayed unrelated on the wire.
    pub fn new(b: &crate::config::EffortBudget, report_timeout_ms: u64) -> Self {
        Self {
            max_seconds: b.max_seconds,
            max_tokens: b.max_tokens,
            max_steps: b.max_steps,
            context_fraction: b.context_fraction,
            search_top_k: b.search_top_k,
            max_report_words: b.max_report_words,
            max_report_sections: b.max_report_sections,
            evidence_width: b.evidence_width,
            worst_case_seconds: b.max_seconds.saturating_add(report_timeout_ms / 1000),
        }
    }
}

// ─── The machine-readable service descriptor (`/.well-known/mindex.json`) ─────

/// What this server is, as **data** rather than prose — the machine twin of
/// `/llms.txt`, served at `/.well-known/mindex.json`.
///
/// It exists because the prose document is not always readable. `/llms.txt` is
/// fetched over the network by a model whose client may classify a document
/// addressed to it as a prompt injection, and at least one frontier assistant
/// does exactly that; a caller that loses the narrative then has nothing left,
/// because the narrative was the only entry point. JSON carries no register for
/// a classifier to object to, so this is the floor: an agent that can reach the
/// origin can always learn what the service is, which endpoints exist and what
/// the current limits are, without reading a single imperative sentence.
///
/// [RFC 8615](https://www.rfc-editor.org/rfc/rfc8615) is why it lives under
/// `/.well-known/`: an agent that knows only an origin can ask the host what it
/// is without having been told a path first. The `mindex` suffix is **not**
/// IANA-registered — stated plainly rather than glossed over; unregistered
/// vendor suffixes are common practice, and the alternative (`/descriptor.json`)
/// costs the zero-knowledge probe that is the whole point.
///
/// Unlike `/llms.txt`, this **is** in the OpenAPI spec, and the contrast is
/// deliberate: `/llms.txt` serves prose to a reader, this serves JSON to a
/// client, and JSON to a client is precisely what the spec is for.
#[derive(Serialize, Debug, ToSchema)]
pub struct MindexDescriptor {
    /// Always `"mindex"`. The identity check a caller makes before trusting the
    /// rest of the document.
    pub service: &'static str,
    /// One sentence on what the service does, for a caller deciding whether to
    /// read further.
    pub summary: &'static str,
    /// Running mindex version — the same value `GET /version` and
    /// `GET /config` report, from the same source, so one document cannot
    /// disagree with itself.
    pub version: &'static str,
    /// Applied `PRAGMA user_version`, as on `GET /version`.
    pub db_schema_version: i32,
    /// The shape version of *this* document, bumped when a field changes
    /// meaning. Distinct from `version`: the server can be upgraded many times
    /// without the descriptor's shape moving.
    pub descriptor_version: u32,
    pub documents: DescriptorDocuments,
    /// `null` when this deployment authorizes nothing, a description of the
    /// scheme when it does.
    ///
    /// Serialized either way rather than skipped: "authorizes nothing" and "too
    /// old to say" must not look the same on the wire, which is why the field
    /// existed as an always-`null` `Option<()>` before there was anything to put
    /// in it. Now that there is, a caller handed only a URL can learn that it
    /// needs a credential — instead of reading a closed connection or a 401 and
    /// guessing.
    pub authentication: Option<DescriptorAuthentication>,
    pub transport: DescriptorTransport,
    /// Every endpoint this build serves, derived from the OpenAPI spec rather
    /// than written by hand — see [`DescriptorEndpoint`].
    pub endpoints: Vec<DescriptorEndpoint>,
    /// Where the project inventory lives. A pointer, not the list: reading it
    /// costs a SQLite round trip, and this handler is deliberately I/O-free so
    /// that discovery still answers when the database is busy — the one moment
    /// a caller most needs to be told what the server is.
    pub projects_url: &'static str,
    pub health_url: &'static str,
    /// Where [`MindexDescriptor::config`] came from. Kept beside the inlined
    /// copy because two of its fields (`research.models`, `research.observed`)
    /// are worker-refreshed, so a long-lived client must re-read rather than
    /// cache this document once.
    pub config_url: &'static str,
    /// The live snapshot `GET /config` serves, inlined so that bootstrapping
    /// costs one request. Built from the same `config_snapshot` call, so it
    /// cannot drift from the endpoint it mirrors.
    pub config: ConfigResponse,
}

/// Where the human- and machine-readable descriptions of this API live.
/// How a caller proves it may do something, when this deployment requires it.
#[derive(Serialize, Debug, ToSchema)]
pub struct DescriptorAuthentication {
    /// Always `"bearer-jwt"` today. Named rather than implied so a second scheme
    /// can be added without a caller having to infer which one it is looking at.
    pub kind: &'static str,
    /// The header, spelled out. `Authorization: Bearer <token>`.
    pub scheme: &'static str,
    /// The action vocabulary a token may carry. Published because a caller
    /// requesting one has to name what it needs, and guessing at the spelling is
    /// a 400 it cannot debug from the outside.
    pub actions: Vec<&'static str>,
    /// The one thing a caller must be told and cannot discover: this server
    /// answers **404** for a project outside the token's scope, exactly as it
    /// does for a project that never existed. A client that renders that as
    /// "no such project" will send its user hunting for an indexing problem
    /// that does not exist.
    pub note: &'static str,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct DescriptorDocuments {
    /// Full request/response schemas for every documented endpoint.
    pub openapi: &'static str,
    /// The same spec, rendered interactively.
    pub openapi_ui: &'static str,
    /// The prose companion to this document: the workflow, the reasoning behind
    /// it, and the semantics no schema can carry.
    pub narrative: &'static str,
}

/// How to talk to this server.
///
/// HTTP/3 is deliberately absent. It is an optional second listener whose
/// availability the server announces per response in the `alt-svc` header, and a
/// deployment is commonly reached through a proxy that forwards TCP only — so a
/// descriptor field claiming h3 would be advertising a port that, from the
/// caller's position, may forward nothing.
#[derive(Serialize, Debug, ToSchema)]
pub struct DescriptorTransport {
    /// Always `true`: TLS is the only transport this server has.
    pub tls: bool,
    /// ALPN protocols the TCP listener offers, in the order it offers them.
    pub alpn: Vec<&'static str>,
}

/// One endpoint, as the descriptor reports it.
///
/// The list is **derived from the OpenAPI spec at first use**, never written out:
/// the route table already has four copies in this repo (the router, the spec,
/// the narrative document and the MCP tool sets), and a hand-written fifth would
/// be the one nothing checks. What a maintainer edits is the handler's
/// `#[utoipa::path]`; this follows.
#[derive(Serialize, Debug, Clone, ToSchema)]
pub struct DescriptorEndpoint {
    /// Uppercase HTTP method.
    pub method: String,
    /// Path template, with `{project_guid}`-style parameters left in place.
    pub path: String,
    /// What the endpoint returns, one line, from the handler's own
    /// documentation.
    pub summary: String,
    /// OpenAPI tag group, absent for routes outside the spec.
    pub tag: Option<String>,
    /// How the response arrives when the caller asks for frames instead of a
    /// single JSON body: `"sse"` for the research streams, `"ndjson"` for
    /// `/index` — all three under `?stream=yes`, and all three answering one body
    /// without it. `null` for everything else.
    ///
    /// The one fact here a maintainer must keep by hand — the spec records the
    /// response body, not that it arrives in frames — so a new streaming
    /// endpoint needs an entry in `STREAMING_ENDPOINTS` beside its route.
    pub streaming: Option<&'static str>,
    /// Whether this route appears in the OpenAPI spec. `false` marks the
    /// deliberately undocumented ones (the narrative, the spec itself, the UI),
    /// which are real routes with no JSON contract to describe.
    pub documented: bool,
}

/// `POST /auth/tokens` — issue a scoped bearer token.
#[derive(Deserialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MintTokenRequest {
    /// Label naming the holder. Appears in the token and in this server's logs;
    /// never a decision input.
    pub sub: String,
    /// Project GUIDs the token may reach, or exactly `["*"]` for all of them.
    ///
    /// The wildcard must be spelled: an empty list is a token that reaches
    /// nothing, and reading an omitted field as "everything" is how a minter
    /// hands out full access by accident.
    pub projects: Vec<String>,
    /// `search` / `research` / `index` / `delete` / `admin` / `mint`.
    ///
    /// Every one of them is mintable here, write actions included — a token that
    /// may index is an ordinary thing to need, and refusing to issue one would
    /// only move that work to a shell on the server's host. What stops it being
    /// an escalation is that the request is contained by the minting token, not
    /// that some actions are unspeakable. The *asking* is what should be
    /// deliberate, and a caller naming `index` in a JSON body has been.
    pub actions: Vec<String>,
    /// `cli` / `vscode` / `agent`: which kinds of holder this token is for.
    ///
    /// Optional, and an omitted or empty list means **every** kind. It is a label
    /// clients honour, never something this server enforces — nothing about a
    /// request identifies the process behind it. See `auth::Audience`.
    #[serde(default)]
    pub audiences: Vec<String>,
    /// Lifetime in days, capped by `[auth].max_token_days` **and** by the
    /// minting token's own remaining life.
    pub days: u64,
    /// Sign under this key id rather than the active one, so revoking this
    /// credential later is one line deleted from the key file instead of a
    /// rotation that logs out every client.
    #[serde(default)]
    pub key_id: Option<String>,
}

/// The issued token. Returned once and stored nowhere.
#[derive(Serialize, Debug, ToSchema)]
pub struct MintTokenResponse {
    pub token: String,
    /// Unix seconds. Echoed so a caller can schedule a renewal without parsing
    /// the token it was just handed.
    ///
    /// Never `null` from this endpoint: a non-expiring token is mintable only by
    /// the local `mint-token` command, deliberately. The field is nullable all
    /// the same, so that a client reading it does not have to be rewritten if
    /// that ever changes.
    pub expires_at: Option<u64>,
    /// The normalized project list actually written into the token — dashless,
    /// lowercased. Echoed because it is what the server will compare against,
    /// and a caller that spelled a GUID differently should be able to see that
    /// its request was understood.
    pub projects: Vec<String>,
    pub actions: Vec<String>,
    /// The audiences written into the token, sorted and deduplicated. Empty means
    /// it is labelled for no particular holder and every client will accept it.
    pub audiences: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang() -> ProgrammingLanguage {
        ProgrammingLanguage::Rust
    }

    /// Every check passing, with the optional query embedder absent.
    fn healthy() -> HealthChecks {
        HealthChecks {
            sqlite: CheckState::Ok,
            qdrant: CheckState::Ok,
            embedder: CheckState::Ok,
            query_embedder: None,
            ollama: CheckState::Ok,
        }
    }

    /// The verdict is the one thing every client keys its behaviour on, and the
    /// distinction it draws — "a feature is unavailable" vs "the service cannot
    /// work" — is only useful if severity always wins. These pin that, including
    /// the case that makes it a fold rather than a chain of `if`s: Ollama down
    /// *and* a required check down is `unhealthy`, not `degraded`.
    #[test]
    fn the_health_verdict_lets_severity_win() {
        assert_eq!(healthy().verdict(false), HealthStatus::Ok);

        let ollama_down = HealthChecks {
            ollama: CheckState::Error,
            ..healthy()
        };
        assert_eq!(ollama_down.verdict(false), HealthStatus::Degraded);

        let sqlite_down = HealthChecks {
            sqlite: CheckState::Error,
            ..healthy()
        };
        assert_eq!(sqlite_down.verdict(false), HealthStatus::Unhealthy);

        let both_down = HealthChecks {
            sqlite: CheckState::Error,
            ollama: CheckState::Error,
            ..healthy()
        };
        assert_eq!(both_down.verdict(false), HealthStatus::Unhealthy);

        for down in [
            HealthChecks {
                qdrant: CheckState::Error,
                ..healthy()
            },
            HealthChecks {
                embedder: CheckState::Error,
                ..healthy()
            },
        ] {
            assert_eq!(down.verdict(false), HealthStatus::Unhealthy);
        }
    }

    /// The split query embedder is required *when it exists* and must not be
    /// counted when it does not — `None` is "one instance serves both", which
    /// `embedder` has already reported on.
    #[test]
    fn an_absent_query_embedder_is_not_a_failing_one() {
        assert_eq!(healthy().verdict(false), HealthStatus::Ok);

        let split_ok = HealthChecks {
            query_embedder: Some(CheckState::Ok),
            ..healthy()
        };
        assert_eq!(split_ok.verdict(false), HealthStatus::Ok);

        let split_down = HealthChecks {
            query_embedder: Some(CheckState::Error),
            ..healthy()
        };
        assert_eq!(split_down.verdict(false), HealthStatus::Unhealthy);
    }

    /// A wedged run has no failing *dependency* behind it — every check is green
    /// and the slot is still unusable — so it is the one input to the verdict
    /// that the checks cannot express.
    #[test]
    fn a_wedged_run_is_unhealthy_with_every_check_green() {
        assert_eq!(healthy().verdict(true), HealthStatus::Unhealthy);
    }

    /// The wire words. Three verdicts and two check states, all lowercase; a
    /// client dispatches on these strings and an older one has to keep reading
    /// `"ok"`.
    #[test]
    fn health_wire_values_are_stable() {
        for (status, wire) in [
            (HealthStatus::Ok, "\"ok\""),
            (HealthStatus::Degraded, "\"degraded\""),
            (HealthStatus::Unhealthy, "\"unhealthy\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), wire);
        }
        assert_eq!(serde_json::to_string(&CheckState::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&CheckState::Error).unwrap(),
            "\"error\""
        );
    }

    /// The reason must not travel. Serialising a failing set produces the bare
    /// word and nothing else — this is the regression guard for the whole
    /// change, and it is cheap enough to be worth pinning here as well as in the
    /// integration suite.
    #[test]
    fn a_failing_check_carries_no_reason_on_the_wire() {
        let checks = HealthChecks {
            qdrant: CheckState::Error,
            ..healthy()
        };
        let json = serde_json::to_string(&checks).unwrap();
        assert!(json.contains("\"qdrant\":\"error\""), "{json}");
        assert!(!json.contains("error:"), "a reason leaked: {json}");
        // Absent, not null: a caller distinguishes "no split deployment" from
        // "the split instance failed".
        assert!(!json.contains("query_embedder"), "{json}");
    }

    /// The `/index` SSE event names are a wire contract, exactly like `ApiError`
    /// codes: the `mindex-index` CLI and the VS Code extension dispatch on them,
    /// and both silently drop what they don't recognise — so a renamed event
    /// fails by going quiet, not by erroring. Changing one is deliberate: update
    /// `post_index`'s doc comment, its OpenAPI 200 description and both readers.
    #[test]
    fn index_event_names_are_stable() {
        let events = [
            (
                IndexEvent::Started {
                    files: 1,
                    symbols_only: false,
                },
                "started",
            ),
            (
                IndexEvent::Prepared {
                    path: "a.rs".into(),
                    language: lang(),
                    chunks: 2,
                    symbols: 3,
                },
                "prepared",
            ),
            (
                IndexEvent::Skipped {
                    path: "a.rs".into(),
                    language: lang(),
                    reason: SkipReason::Unchanged,
                },
                "skipped",
            ),
            (
                IndexEvent::Embedded {
                    batch_chunks: 4,
                    chunks_done: 8,
                    chunks_total: 16,
                    elapsed_ms: 100,
                },
                "embedded",
            ),
            (
                IndexEvent::Indexed {
                    path: "a.rs".into(),
                    language: lang(),
                    count: 5,
                },
                "indexed",
            ),
            (
                IndexEvent::Done {
                    response: IndexResponse {
                        files: HashMap::new(),
                    },
                    files_indexed: 1,
                    chunks: 2,
                    elapsed_ms: 3,
                },
                "done",
            ),
            (
                IndexEvent::Error {
                    code: "internal".into(),
                    detail: "boom".into(),
                },
                "error",
            ),
        ];
        for (event, name) in events {
            assert_eq!(event.name(), name);
        }
    }

    /// Every event's `data` keys, pinned. Both consumers read these by name and
    /// ignore unknown keys, so a renamed field disappears without an error.
    #[test]
    fn index_event_data_names_its_fields_on_the_wire() {
        let keys = |v: Value| -> Vec<String> {
            let Value::Object(map) = v else {
                panic!("event data must be a JSON object");
            };
            map.keys().cloned().collect()
        };

        assert_eq!(
            keys(
                IndexEvent::Started {
                    files: 1,
                    symbols_only: true,
                }
                .data()
            ),
            ["files", "symbols_only"]
        );
        let prepared = IndexEvent::Prepared {
            path: "a.rs".into(),
            language: lang(),
            chunks: 2,
            symbols: 3,
        }
        .data();
        assert_eq!(prepared["language"], "rust");
        assert_eq!(keys(prepared), ["chunks", "language", "path", "symbols"]);
        assert_eq!(
            keys(
                IndexEvent::Skipped {
                    path: "a.rs".into(),
                    language: lang(),
                    reason: SkipReason::InFlight,
                }
                .data()
            ),
            ["language", "path", "reason"]
        );
        assert_eq!(
            keys(
                IndexEvent::Embedded {
                    batch_chunks: 4,
                    chunks_done: 8,
                    chunks_total: 16,
                    elapsed_ms: 100,
                }
                .data()
            ),
            ["batch_chunks", "chunks_done", "chunks_total", "elapsed_ms"]
        );
        assert_eq!(
            keys(
                IndexEvent::Indexed {
                    path: "a.rs".into(),
                    language: lang(),
                    count: 5,
                }
                .data()
            ),
            ["count", "language", "path"]
        );
        assert_eq!(
            keys(
                IndexEvent::Done {
                    response: IndexResponse {
                        files: HashMap::new(),
                    },
                    files_indexed: 1,
                    chunks: 2,
                    elapsed_ms: 3,
                }
                .data()
            ),
            ["chunks", "elapsed_ms", "files", "files_indexed"]
        );
        assert_eq!(
            keys(
                IndexEvent::Error {
                    code: "internal".into(),
                    detail: "boom".into(),
                }
                .data()
            ),
            ["code", "detail"]
        );
    }

    /// `done.files` must be byte-for-byte the JSON mode's `IndexResponse.files`,
    /// so a streaming client tallies exactly what a JSON client parses.
    #[test]
    fn done_files_matches_the_json_response_shape() {
        let mut files = HashMap::new();
        files.insert(lang(), HashMap::from([("src/a.rs".to_string(), 7u64)]));
        let response = IndexResponse {
            files: files.clone(),
        };
        let done = IndexEvent::Done {
            response: response.clone(),
            files_indexed: 1,
            chunks: 7,
            elapsed_ms: 1,
        }
        .data();
        let via_json_mode =
            serde_json::to_value(&response).expect("IndexResponse always serializes");
        assert_eq!(done["files"], via_json_mode["files"]);
    }

    /// The skip reasons ride the wire as these exact strings.
    #[test]
    fn skip_reason_wire_values_are_stable() {
        assert_eq!(SkipReason::Unchanged.as_str(), "unchanged");
        assert_eq!(SkipReason::InFlight.as_str(), "in_flight");
        assert_eq!(SkipReason::Cancelled.as_str(), "cancelled");
    }

    /// `?stream=` accepts exactly `yes`/`no`; anything else must be a 400 at the
    /// extractor, never a silent fall-through to the JSON mode.
    #[test]
    fn stream_choice_parses_yes_and_no_only() {
        let q: IndexQuery = serde_urlencoded::from_str("stream=yes").expect("yes parses");
        assert_eq!(q.stream, Some(StreamChoice::Yes));
        let q: IndexQuery = serde_urlencoded::from_str("stream=no").expect("no parses");
        assert_eq!(q.stream, Some(StreamChoice::No));
        let q: IndexQuery = serde_urlencoded::from_str("").expect("absent parses");
        assert_eq!(q.stream, None);
        assert!(serde_urlencoded::from_str::<IndexQuery>("stream=true").is_err());
        assert!(serde_urlencoded::from_str::<IndexQuery>("streem=yes").is_err());
    }

    /// The same contract on `/research`, where getting it wrong is worse than on
    /// `/index`: a `?stream=true` silently read as "absent" would hand a caller
    /// that asked to watch the run a body it only sees seventy minutes later,
    /// while a typo'd key on the *other* side would stream at a caller that will
    /// not read the frames and cancel the run by disconnecting.
    #[test]
    fn the_research_stream_choice_parses_yes_and_no_only() {
        let q: ResearchQuery = serde_urlencoded::from_str("stream=yes").expect("yes parses");
        assert_eq!(q.stream, Some(StreamChoice::Yes));
        let q: ResearchQuery = serde_urlencoded::from_str("stream=no").expect("no parses");
        assert_eq!(q.stream, Some(StreamChoice::No));
        let q: ResearchQuery = serde_urlencoded::from_str("").expect("absent parses");
        assert_eq!(q.stream, None);
        assert!(serde_urlencoded::from_str::<ResearchQuery>("stream=true").is_err());
        assert!(serde_urlencoded::from_str::<ResearchQuery>("streem=yes").is_err());
    }

    // ── the language checklist, as far as one crate can check it ─────────────

    /// **The silent step of the Languages checklist.** The
    /// `project_files.programming_language` CHECK lives in *two* files, and editing
    /// only the first is invisible: `v1.0.0_schema.sql` builds a fresh database and
    /// is never re-read, so a database in use needs the later rebuild migration to
    /// carry the widened list too. Get it wrong and the language works perfectly in
    /// a new container and fails on every existing one — a 500 from a constraint,
    /// at insert time, for that language only.
    ///
    /// This walks `ALL` against a fully migrated database, which is the state a real
    /// deployment is in, so the second copy is the one being checked.
    #[tokio::test]
    async fn every_language_in_all_is_accepted_by_the_migrated_schema() {
        use crate::db::sqlite3::SQLite3Pool;
        use tokio_util::sync::CancellationToken;

        let pool = SQLite3Pool::new(std::path::Path::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            for (_, m) in crate::MIGRATIONS {
                tx.execute_batch(m)?;
            }
            tx.execute(
                "INSERT INTO projects (guid, model_id) VALUES (?1, 'BAAI/bge-m3')",
                ["a".repeat(32)],
            )?;
            Ok(())
        })
        .await
        .expect("migrations apply");

        for pl in ProgrammingLanguage::ALL {
            let pl = *pl;
            let inserted = pool
                .transaction(CancellationToken::new(), move |tx| {
                    tx.execute(
                        "INSERT INTO project_files
                             (project_guid, model_id, path, sha256, programming_language, status)
                         VALUES (?1, 'BAAI/bge-m3', ?2, ?3, ?4, 'indexing')",
                        rusqlite::params![
                            "a".repeat(32),
                            format!("src/{}.x", pl.name()),
                            "0".repeat(64),
                            pl
                        ],
                    )?;
                    Ok(())
                })
                .await;
            assert!(
                inserted.is_ok(),
                "the schema rejects `{}`, which `ProgrammingLanguage::ALL` offers. \
                 The CHECK constraint is in two files — a new language needs the \
                 rebuild migration as well as v1.0.0_schema.sql: {inserted:?}",
                pl.name()
            );
        }
    }

    /// A missing `FromSql` arm fails on **read**, not on write: rows insert fine and
    /// then every query selecting the column 500s. So the round trip has to be
    /// checked in both directions, per language.
    #[tokio::test]
    async fn every_language_survives_the_round_trip_through_sqlite() {
        use crate::db::sqlite3::{SQLite3Pool, SQLite3PoolError};
        use tokio_util::sync::CancellationToken;

        let pool = SQLite3Pool::new(std::path::Path::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute_batch("CREATE TABLE t (pl TEXT NOT NULL);")?;
            Ok(())
        })
        .await
        .expect("table");

        for pl in ProgrammingLanguage::ALL {
            let pl = *pl;
            let back: ProgrammingLanguage = pool
                .transaction(CancellationToken::new(), move |tx| {
                    tx.execute("DELETE FROM t", [])?;
                    tx.execute("INSERT INTO t (pl) VALUES (?1)", rusqlite::params![pl])?;
                    tx.query_row("SELECT pl FROM t", [], |r| r.get(0))
                        .map_err(SQLite3PoolError::from)
                })
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "`{}` does not round-trip through SQLite — a missing FromSql \
                         arm fails on read, so rows insert fine and every later query \
                         500s: {e:?}",
                        pl.name()
                    )
                });
            assert_eq!(back, pl);
        }
    }

    /// The same for the wire: a missing serde rename makes a request naming that
    /// language a 400 `request.malformed_body`, and the language is simply
    /// unusable through the API while being present everywhere else.
    #[test]
    fn every_language_survives_the_round_trip_through_json() {
        for pl in ProgrammingLanguage::ALL {
            let json = serde_json::to_string(pl).expect("serializes");
            assert_eq!(
                json,
                format!("\"{}\"", pl.name()),
                "the serde name and `name()` disagree; `GET /config` publishes one \
                 and requests are parsed with the other"
            );
            let back: ProgrammingLanguage =
                serde_json::from_str(&json).expect("deserializes by its own name");
            assert_eq!(back, *pl);
        }
    }

    /// `name()` is the label on every metric, the value in `GET /config`'s list and
    /// the SQL literal. Two languages sharing one would make a project's per-language
    /// counts silently merge.
    #[test]
    fn no_two_languages_share_a_name() {
        let mut seen = std::collections::HashMap::new();
        for pl in ProgrammingLanguage::ALL {
            if let Some(other) = seen.insert(pl.name(), *pl) {
                panic!(
                    "{:?} and {pl:?} both call themselves {:?}",
                    other,
                    pl.name()
                );
            }
        }
        assert_eq!(seen.len(), ProgrammingLanguage::ALL.len());
    }

    /// `ALL` must hold each language once. A duplicate would make every `ALL`-driven
    /// check — the tags-query construction test among them — run twice for one
    /// language and, more to the point, would mean the list was edited by hand
    /// without being read.
    #[test]
    fn all_lists_every_language_exactly_once() {
        let unique: std::collections::HashSet<_> = ProgrammingLanguage::ALL.iter().collect();
        assert_eq!(
            unique.len(),
            ProgrammingLanguage::ALL.len(),
            "ProgrammingLanguage::ALL contains a duplicate"
        );
        assert!(!ProgrammingLanguage::ALL.is_empty());
    }
}
