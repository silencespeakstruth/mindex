//! Two-level configuration: a TOML file (XDG-resolved) supplies the base values,
//! CLI flags override it, and both fall back to the built-in defaults defined here.
//!
//! Resolution order at startup (highest priority first):
//!   1. CLI flags (only the long-standing operational flags — see [`Cli`]).
//!   2. The config file, located by [`resolve_config_path`] (XDG canon).
//!   3. The compiled defaults in the `Default` impls below — the *single* source
//!      of every "sensible default"; they are not duplicated in clap.
//!
//! Every key carries its unit in its name (`*_ms`, `*_seconds`, `*_minutes`,
//! `*_chunks`, `*_tokens`, `*_bytes`, `*_days`, `*_points`, `*_mib`) so an operator
//! never has to guess what a number means.
//!
//! Structural invariants are deliberately **not** here — they would break the
//! system if changed independently and live as documented `const`s next to the
//! code that relies on them: the BGE-M3 vector width (`1024`), the `/encode` wire
//! magic, `COLLECTION_SCHEMA_VERSION`, HTTP `499`, `PRAGMA foreign_keys = ON`, and
//! `PRAGMA journal_mode = WAL`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde::Deserialize;
use tracing::{info, warn};
use url::Url;

// ── Built-in defaults (the only place "sensible defaults" are written) ──────────
const DEFAULT_BIND: &str = "127.0.0.1:11111";
const DEFAULT_CERT_PATH: &str = "cert.pem";
const DEFAULT_KEY_PATH: &str = "key.pem";
const DEFAULT_MAX_BODY_MIB: usize = 256;

const DEFAULT_MODEL_NAME: &str = "BAAI/bge-m3";
const DEFAULT_MODEL_SERVER: &str = "http://localhost:11211";
const DEFAULT_HEALTH_TIMEOUT_MS: u64 = 2000;
const DEFAULT_MAX_429_RETRIES: u32 = 3;
const DEFAULT_BACKOFF_BASE_MS: u64 = 200;
const DEFAULT_ENCODE_TIMEOUT_MS: u64 = 600_000;

const DEFAULT_QDRANT_SERVER: &str = "http://localhost:6334";
const DEFAULT_UPSERT_BATCH_POINTS: usize = 256;
const DEFAULT_DENSE_PREFETCH_LIMIT: u32 = 200;
const DEFAULT_SPARSE_PREFETCH_LIMIT: u32 = 200;
const DEFAULT_FUSION_LIMIT: u32 = 200;
/// Whole-request ceiling for one Qdrant call. The client's own default is **5 s**,
/// which nothing in this repo set or could override: a project whose candidate set is
/// large enough that fusion + ColBERT rerank exceeds it failed *every* search with
/// `qdrant.unavailable` 503, untunably. 30 s is generous for a query and still short
/// enough to fail a wedged connection rather than hold a request open.
const DEFAULT_QDRANT_TIMEOUT_MS: u64 = 30_000;
/// Establishing the connection, as opposed to answering. Separate because a Qdrant
/// that is down should be reported in seconds, whatever the request ceiling above is.
const DEFAULT_QDRANT_CONNECT_TIMEOUT_MS: u64 = 5_000;

const DEFAULT_DB_PATH: &str = "mindex.db";
const DEFAULT_DB_POOL_SIZE: usize = 4;
const DEFAULT_PAGE_SIZE_BYTES: u32 = 16384;
const DEFAULT_SYNCHRONOUS: &str = "normal";

const DEFAULT_EMBED_BATCH_CHUNKS: usize = 256;
const DEFAULT_STUCK_GRACE_MINUTES: i64 = 30;
const DEFAULT_PATH_BATCH_SIZE: usize = 500;
const DEFAULT_SPARSE_MIN_WEIGHT: f32 = 1e-5;

const DEFAULT_MIN_CHUNK_TOKENS: usize = 128;
const DEFAULT_MAX_CHUNK_TOKENS: usize = 512;
/// Validation ceiling for the *code* chunk window: 512 is the top of the range
/// BGE-M3 was measured to work best in, and the window is measured, not computed.
///
/// It is **not** the model's input limit, which this comment used to claim. The
/// embedder truncates at its `--maxlen`, default 8192 (`embedder/src/bge_m3_api`);
/// verified against the running server, where a ~1600-token text returns 2514
/// ColBERT vectors rather than 512. That distinction is what lets documentation
/// use a wider window — see [`MODEL_INPUT_LIMIT_TOKENS`].
const MODEL_MAX_TOKENS: usize = 512;

/// The point past which the embedder really does drop text on the floor.
/// Chunks above this are silently truncated, so it is a hard ceiling for any
/// window. Kept separate from [`MODEL_MAX_TOKENS`], which is a quality choice
/// for code rather than a capacity limit.
const MODEL_INPUT_LIMIT_TOKENS: usize = 8192;

/// Cap on a documentation chunk, measured rather than argued: at 512 the answer
/// is repeatedly cut away from the text that explains it (18/23 documentation
/// questions answered at 1024 against 15/23 at 512), and past 1024 nothing
/// improves while every retrieved hit costs proportionally more of a `/research`
/// transcript — which is 97% of what a run makes the GPU do.
const DEFAULT_MAX_DOC_CHUNK_TOKENS: usize = 1024;

/// Emit chunks for the lines the AST walk selects nothing for. Without it about
/// half of all source lines — every construct below `min_chunk_tokens`, and the
/// doc comments attached to them — are in no chunk and cannot be retrieved at
/// any price. Roughly doubles the chunk count, so it is a knob for anyone whose
/// corpus makes that a real cost; on this repository it is seconds.
const DEFAULT_FILL_GAPS: bool = true;

/// Weight of the semantic-shift term when cutting documentation, relative to the
/// cost of opening a chunk. On a densely-headed corpus it is measurably a no-op
/// (this repository: identical retrieval with and without), because the author
/// already marked every topic change; it earns its keep on documents with sparse
/// or absent headings, where structure alone degenerates to packing blindly up to
/// the cap. On by default for that reason, and free to turn off: 0 disables the
/// per-document `/encode` entirely. Above ~4 it outvotes chunk cost and the
/// segmentation collapses toward one block per chunk.
const DEFAULT_DOC_SEMANTIC_WEIGHT: f64 = 1.0;
/// Past this the term stops refining and starts shredding, so a larger value is
/// far more likely to be a typo than an intent.
const MAX_DOC_SEMANTIC_WEIGHT: f64 = 4.0;

const DEFAULT_TOP_K: u64 = 5;
const DEFAULT_MAX_TOP_K: u64 = 100;
const DEFAULT_MAX_QUERY_BYTES: usize = 32768;

const DEFAULT_MAX_CODE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_FILES_PER_REQUEST: usize = 10_000;
const DEFAULT_MAX_DRIFT_FILES: usize = 200_000;
const DEFAULT_MAX_SELECTOR_PATTERNS: usize = 256;
const DEFAULT_MAX_SYMBOL_NAME_BYTES: usize = 512;
const DEFAULT_MAX_SYMBOL_RESULTS: usize = 50;
const DEFAULT_MAX_HISTORY_COMMITS: usize = 20_000;
const DEFAULT_MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_RESEARCH_DELETE_IDS: usize = 500;

const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const DEFAULT_RESEARCH_WORKER_THREADS: usize = 2;
const DEFAULT_RESEARCH_MAX_CONCURRENT: usize = 2;
/// Ceiling on the Ollama context window, **not** a target: the window actually
/// requested is the model's own trained length, capped by this. Exists only
/// because `num_ctx` allocates KV cache up front — measured on an R9700 at ~30 KiB
/// per token with `OLLAMA_KV_CACHE_TYPE=q8_0` and ~54 KiB at f16, so a 262144-token
/// model would ask for ~7.5 GiB of VRAM unguarded. 128k passes most models'
/// windows through untouched while keeping that allocation bounded.
const DEFAULT_RESEARCH_MAX_NUM_CTX: u64 = 131072;
/// Ceilings on what one request's `budget` override may ask for. They bound the
/// request shape, like `[limits]` — the effort levels are the *defaults*, these are
/// what an operator is willing to let a single job hold a slot for.
const DEFAULT_RESEARCH_MAX_REQUEST_SECONDS: u64 = 3600;
const DEFAULT_RESEARCH_MAX_REQUEST_TOKENS: u64 = 8_000_000;
const DEFAULT_RESEARCH_MAX_REQUEST_STEPS: usize = 200;
/// Ceiling on a request's `budget.max_report_sections` override. Twelve is
/// deliberately conservative: sections are report turns, each retried up to
/// once, all inside `[research].report_timeout_ms` — a caller asking for the
/// ceiling should expect stubs, not a wider window.
const DEFAULT_RESEARCH_MAX_REQUEST_REPORT_SECTIONS: usize = 12;
/// Ceiling on a request's `budget.max_report_words` override. Validated at
/// startup against `max_num_ctx_tokens` exactly like the effort presets, which
/// is what keeps the `num_predict` arming safe for any value a request may pass.
const DEFAULT_RESEARCH_MAX_REQUEST_REPORT_WORDS: usize = 4000;
/// Ceiling on a request's `budget.evidence_width` override. Three because the
/// widened rows are resent on every later turn: width is paid in prompt tokens
/// each turn, not once, so a small integer is the whole useful range.
const DEFAULT_RESEARCH_MAX_EVIDENCE_WIDTH: u64 = 3;
/// Report sections one run may write, at every effort level. Six matches what
/// `PLAN_REQUEST` historically asked for ("3-6 sub-questions"); the plan turn's
/// prompt is templated from the resolved value, so raising this genuinely asks
/// the model for a longer plan rather than silently truncating one.
const DEFAULT_RESEARCH_MAX_REPORT_SECTIONS: usize = 6;
/// Multiplier on the per-call evidence widths (`read_chunks`, `grep`, `callers`,
/// `file_history`, `symbols`), at every effort level. One — the historical
/// widths — because width compounds into the token budget (every widened result
/// is resent on every later turn); this exists for a caller who knows the
/// question needs breadth, not as a default to raise.
const DEFAULT_RESEARCH_EVIDENCE_WIDTH: u64 = 1;
/// Floor on `max_report_sections`, preset and request alike.
///
/// This is `MIN_SECTIONED_PLAN_ITEMS` (research.rs derives its const from this
/// one): below three plan items the sectioned path does not engage at all, so a
/// smaller grant would name a report shape the mechanism cannot produce — and
/// the templated plan request "3-N" would be malformed. A caller wanting a
/// short report has `max_report_words`.
pub const MIN_REPORT_SECTIONS: usize = 3;
/// Chunks per `search` tool call, at every effort level. Same at all three
/// because widening it was measured *not* to be the fix: the runs that missed an
/// answer were already getting five hits per search and losing on query
/// formulation, not on evidence width. Kept as a knob so a harness can sweep it.
const DEFAULT_RESEARCH_SEARCH_TOP_K: u64 = 5;
/// Floor on a non-zero `[research.effort.*].max_report_words`.
///
/// Below this the instruction stops shaping a report and starts forbidding one:
/// the model writes a stub, the run's whole cost is spent, and the caller gets
/// less than the plan it already had. `0` — announce no length at all — is the
/// supported way to disable the ceiling; a tiny number is a mistake.
pub const MIN_REPORT_WORDS: usize = 150;
/// Tokens reserved per granted report word when arming `num_predict` on the
/// report turn.
///
/// Deliberately ~3× the honest prose ratio (~1.3-1.5 tokens/word, higher inside
/// code fences). `num_predict` is a **runaway** backstop, not the ceiling — the
/// ceiling is the word count in the prompt. Sizing it tight would make a report
/// that merely overshoots get cut mid-fence, which fails
/// `validate_report_markdown` and buys a full-volume rewrite of the document
/// that just failed: a long run turned into a lost one. So it must be
/// unreachable by anything but a genuine runaway, and `research_report_length_caps`
/// firing at all means this number (or the model) is wrong.
pub const REPORT_WORDS_TO_TOKENS: u64 = 4;
/// Per-turn transport ceiling, set **above every research budget on purpose** so it
/// can never fire first.
///
/// Measured twice, in the same afternoon, from the same wrong intuition. Tightened to
/// 120 s: glm's cold opening turn (model load plus a ~98k-token KV allocation) crossed
/// it and the run died at step 0. Raised to 600 s: a turn where glm looped in its
/// thinking channel crossed *that*, and the run died at step 0 again — with
/// `max_seconds` at 900, i.e. the deadline would have cancelled the turn 300 s later
/// and shipped a report from the evidence already gathered.
///
/// That is the whole lesson. A turn timeout is a `reqwest` error, so it fails the
/// **entire run** and streams `ollama.unavailable` — an infrastructure code for a
/// model that was only thinking too long. The deadline cancels the same turn and
/// still answers. A model that never stops generating is precisely the case the hard
/// deadline exists for, so letting a transport timeout preempt it inverts the design.
///
/// So this is not the run's bound and must not act like one: `max_seconds` bounds the
/// investigation, `report_timeout_ms` bounds the report, and this only catches a
/// connection that has died in a way neither notices. `validate` enforces that it
/// exceeds `max_request_seconds`, so a config cannot recreate the inversion.
const DEFAULT_RESEARCH_TURN_TIMEOUT_MS: u64 = 3_900_000;
/// How long a turn may produce **nothing at all** — not one thinking or content
/// token — before it is abandoned.
///
/// The gap [`DEFAULT_RESEARCH_TURN_TIMEOUT_MS`] deliberately leaves open. That one is
/// a dead-*socket* guard and must sit above every budget; a socket that is alive and
/// simply silent falls to nothing but `max_seconds`, which means an Ollama that never
/// starts answering spends the entire run and the client watches an empty stream for
/// as long as the budget allows. Measured here 2026-08-01: one run burned 300 s of a
/// 300 s budget at `steps: 0, turns: 0, prompt_tokens: 0` — Ollama was thrashing
/// between two context sizes (`Load failed … timed out waiting for llama-server to
/// start`) and mindex said nothing, because from its side the request had simply not
/// answered yet.
///
/// Two minutes, because the thing it must not preempt is a legitimately slow *first*
/// token: prompt evaluation of a long transcript on a loaded GPU is minutes of silence
/// by nature, and this fires only when even that has not begun. It bounds the silent
/// prefix of a turn, never the turn — once tokens flow, `turn_timeout_ms` is again the
/// only ceiling. `0` disables it, for a host where a cold load of a huge model is
/// normal and expected.
const DEFAULT_RESEARCH_FIRST_TOKEN_TIMEOUT_MS: u64 = 120_000;
/// How long the report phase gets *after* the investigation deadline. The whole
/// point of a separate window is that a run which hits the wall still gets to
/// synthesise what it found, so this cannot come out of the investigation's budget —
/// which makes `max_seconds + report_timeout_ms` the true worst-case wait.
///
/// Raised 120 s → 300 s when the report stopped being one turn: it now covers up to
/// `max_report_sections` generations plus a per-section repair pass. The window's
/// *meaning* is unchanged — the tail a caller waits after the investigation — but
/// what has to fit inside it is not. Anything reading the worst-case wait
/// (scout's `RESEARCH_TOTAL_TIMEOUT`) moves with it.
const DEFAULT_RESEARCH_REPORT_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_RESEARCH_HEALTH_TIMEOUT_MS: u64 = 2000;
/// Steps between checkpoint turns. Six against `medium`'s 20 gives three chances to
/// bank before the budget binds, at a cost of ~15% of the run's lookups — the number
/// most in need of measurement in this file.
const DEFAULT_RESEARCH_CHECKPOINT_EVERY_STEPS: usize = 6;
/// Floor on a non-zero `checkpoint_every_steps`. At 1 the run writes as often as it
/// looks, which is not an investigation. Shared with the request validator: the
/// per-request override obeys the same floor and the same `0 = off` spelling.
pub(crate) const MIN_RESEARCH_CHECKPOINT_EVERY_STEPS: usize = 2;
/// Checkpoint turns one run may take, whatever the interval says. A backstop on a
/// mis-set interval, not a tuning knob: eight banked drafts is already more than
/// the default section grant (`max_report_sections`, 6) can use.
pub const MAX_CHECKPOINTS: usize = 8;
/// How often the local Ollama model registry is re-read for `GET /config`. Five
/// minutes because the catalog changes only when a human runs `ollama pull`/`rm`,
/// and one tick is a single GET bounded by [`DEFAULT_RESEARCH_HEALTH_TIMEOUT_MS`].
const DEFAULT_RESEARCH_MODELS_REFRESH_SECONDS: u64 = 300;
/// Ninety days, against thirty for the status log: a stored report is the corpus a
/// later run reads, not an audit trail, and a question worth asking once tends to be
/// worth re-reading a season later. Pinning is the escape hatch for anything that
/// should outlive it.
const DEFAULT_RESEARCH_RETENTION_DAYS: u64 = 90;
/// Three earlier reports. More is not obviously better: each is resent every turn, and
/// the failure the feature addresses (not knowing the names) is usually cured by one.
const DEFAULT_RESEARCH_MAX_CONTEXT_RUNS: usize = 3;
/// ~6k tokens at four characters each — under a tenth of the smallest window the
/// server asks Ollama for, so an injected block cannot on its own crowd out the
/// investigation it is meant to accelerate.
const DEFAULT_RESEARCH_MAX_CONTEXT_CHARS: usize = 24_000;
/// A page of runs. Large enough that the common project never pages at all, small
/// enough that a page is one screen and one cheap query.
const DEFAULT_RESEARCH_LIST_PAGE_LIMIT: usize = 50;
/// Below this a context block cannot hold one useful report, so a cap under it is a
/// misconfiguration rather than a tight setting.
const MIN_RESEARCH_MAX_CONTEXT_CHARS: usize = 1000;
/// Thinking characters after which one turn is abandoned: twice
/// [`MIN_RESEARCH_MAX_TURN_THINKING_CHARS`], which is the smallest value that is still
/// clear of ordinary thinking.
///
/// Sized to catch **both** measured pathologies rather than to be provably safe against
/// one. Verified live on 2026-07-30: a wedged *investigation* turn generates ~18
/// chars/s and reaches this in ~445 s, leaving ~455 s of a 900 s deadline — enough to
/// actually investigate, since glm's healthy runs finish in ~180 s. A wedged *report*
/// turn generates ~310 chars/s and is caught in seconds. An earlier 20000 was clear of
/// every healthy turn observed but never tripped on the slow wedge at all, which made
/// it safe and useless.
///
/// The margin over the healthy population is 3.1× (the busiest healthy turn measured
/// averaged 2642 characters), and it is a margin over *averages*: per-turn maxima are
/// not recorded yet, so a false positive is possible. That is what the `warn!` names
/// the model and the count for. A volume bound is also structurally late against a
/// slow generator — the instrument that catches the slow wedge early is a clock armed
/// on the first thinking delta, which is a separate change.
const DEFAULT_RESEARCH_MAX_TURN_THINKING_CHARS: usize = 2 * MIN_RESEARCH_MAX_TURN_THINKING_CHARS;
/// Floor on a non-zero `max_turn_thinking_chars`. Below this the guard starts
/// abandoning healthy turns: the busiest measured healthy turn averaged 2642
/// characters, so anything in the low thousands is a guard on ordinary thinking.
/// Use `0` to disable rather than a small number.
const MIN_RESEARCH_MAX_TURN_THINKING_CHARS: usize = 4096;
/// Ceiling on `slow_turn_tokens_per_second`. No local model on any hardware this
/// service targets generates a thousand tokens a second, so a threshold at or above
/// it fires on every turn — and a warning that always fires is one nobody reads.
const MAX_RESEARCH_SLOW_TURN_TOKENS_PER_SECOND: f64 = 1000.0;
/// Floor on `report_timeout_ms`: below this the report turn cannot finish and every
/// truncated run would ship the server-written notice instead of a real report.
const MIN_RESEARCH_REPORT_TIMEOUT_MS: u64 = 5000;

const DEFAULT_METRICS_ENABLED: bool = true;
const DEFAULT_METRICS_REFRESH_SECONDS: u64 = 60;
const DEFAULT_METRICS_PROBE_DEPENDENCIES: bool = true;
const DEFAULT_METRICS_PER_PROJECT_HTTP_LABELS: bool = false;
/// Below this the per-project aggregate competes with the request path for a
/// pool connection; above it every scrape reports the same stale gauge.
const MIN_METRICS_REFRESH_SECONDS: u64 = 5;
const MAX_METRICS_REFRESH_SECONDS: u64 = 3600;

const DEFAULT_GC_INTERVAL_SECONDS: u64 = 3600;
const DEFAULT_STATUS_LOG_RETENTION_DAYS: u64 = 30;
const DEFAULT_RETRY_INTERVAL_SECONDS: u64 = 60;
const DEFAULT_FAILED_WARN_INTERVAL_SECONDS: u64 = 3600;
const DEFAULT_MAX_RETRIES: i64 = 3;

/// SQLite caps `page_size` at 65536 and requires a power of two ≥ 512.
const SQLITE_MIN_PAGE_SIZE: u32 = 512;
const SQLITE_MAX_PAGE_SIZE: u32 = 65536;
const VALID_SYNCHRONOUS: [&str; 4] = ["off", "normal", "full", "extra"];

// ── Sections ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// Max `/index` request body in MiB (indexing posts many files at once).
    pub max_body_mib: usize,
    /// Enable HTTP/3 over QUIC on the same port (UDP). When set, `--cert-path`
    /// and `--key-path` must also be explicitly provided as CLI flags.
    #[serde(default)]
    pub http3: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelConfig {
    pub name: String,
    pub server_url: Url,
    /// Optional **second** embedder instance serving only the *query* path
    /// (`/search`, and every `search` a `/research` run makes). Absent = one
    /// instance does both, which is today's behaviour and stays the default.
    ///
    /// The two workloads have opposite profiles and only one of them needs the
    /// GPU. Indexing sends batches of hundreds and is throughput-bound; a query is
    /// one text of ~20 tokens and is latency-bound, so serving it costs a few
    /// hundred milliseconds on a CPU instance — against a research turn of tens of
    /// seconds, irrelevant. What it buys is the ~6 GiB of VRAM the resident fp32
    /// model otherwise holds permanently, which on a 32 GiB card is the difference
    /// between a 23 GB model running on the GPU and running half on the CPU.
    ///
    /// Point it at another instance of the *same* server started with
    /// `--device cpu`: identical code, identical fp32 numerics, so index-side and
    /// query-side vectors still agree — which they must, or sparse set membership
    /// diverges and lexical recall degrades in a way that looks like a bad model.
    pub query_server_url: Option<Url>,
    /// Liveness-ping timeout for the embedder's `/health`.
    pub health_timeout_ms: u64,
    /// 429-backoff retries before giving up on an `/encode` call.
    pub max_429_retries: u32,
    /// First 429 backoff; doubles each retry.
    pub backoff_base_ms: u64,
    /// Ceiling on one `/encode` call, its 429 retries and their backoffs included —
    /// not per attempt. See `BGEm3Tuning::encode_timeout_ms`.
    pub encode_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QdrantConfig {
    pub server_url: Url,
    pub upsert_batch_points: usize,
    pub dense_prefetch_limit: u32,
    pub sparse_prefetch_limit: u32,
    pub fusion_limit: u32,
    /// Whole-request timeout for one Qdrant call.
    pub timeout_ms: u64,
    /// Connection-establishment timeout for one Qdrant call.
    pub connect_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DatabaseConfig {
    pub path: PathBuf,
    pub pool_size: usize,
    pub page_size_bytes: u32,
    /// One of off / normal / full / extra (case-insensitive).
    pub synchronous: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexingConfig {
    /// Chunks per `/encode` call (the GPU batch lever).
    pub embed_batch_chunks: usize,
    /// Minutes a file may sit in `indexing` before the retry worker treats it as
    /// crash-orphaned. Must exceed the longest legitimate in-flight request.
    pub stuck_grace_minutes: i64,
    /// Paths per batch on soft-delete / cancel (bounded by SQLite bind-var limit).
    pub path_batch_size: usize,
    /// Sparse weights at or below this magnitude are dropped before upsert.
    pub sparse_min_weight: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SlicerConfig {
    pub min_chunk_tokens: usize,
    pub max_chunk_tokens: usize,
    /// Whether to index the lines the AST walk leaves out (see `slicing::traits`).
    pub fill_gaps: bool,
    /// Cap on a documentation chunk. Documentation has **no** minimum: a short
    /// section is a complete claim, and the code slicer's floor would drop it.
    pub max_doc_chunk_tokens: usize,
    /// How much embedding distance influences where documentation is cut.
    /// 0 turns the term off, and with it the per-document `/encode`.
    pub doc_semantic_weight: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SearchConfig {
    /// `top_k` used when a `/search` request omits it.
    pub default_top_k: u64,
    /// Upper bound a request may set for `top_k` (rejected with 400 above this).
    pub max_top_k: u64,
    /// Maximum search-query length in bytes (rejected with 400 above this).
    pub max_query_bytes: usize,
}

/// Request-shape limits enforced at the API edge (each violation → 400 with a stable
/// error code). They bound resource use; raise them if a legitimate workload needs more.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// Maximum size of a single file's source `code` in an `/index` request.
    pub max_code_bytes: usize,
    /// Maximum number of files in one `/index` request.
    pub max_files_per_request: usize,
    /// Maximum number of entries in one `/drift` `path → sha256` map.
    pub max_drift_files: usize,
    /// Maximum globs + languages combined in one `include`/`exclude` selector.
    pub max_selector_patterns: usize,
    /// Maximum length of a `/symbols` `name` in bytes.
    pub max_symbol_name_bytes: usize,
    /// Upper bound a `/symbols` request may set for `limit` (per role).
    pub max_symbol_results: usize,
    /// Maximum number of commits in one `/history` reconciliation request.
    pub max_history_commits: usize,
    /// Maximum size of one commit's `subject` + `body` in bytes. A commit
    /// message is unbounded in git, so this is the `max_code_bytes` analogue for
    /// the history channel.
    pub max_commit_message_bytes: usize,
    /// Maximum number of run ids in one `DELETE /projects/{guid}/research` body.
    /// The ids become one `IN (…)` list, so this is what keeps a batch clear of
    /// SQLite's bind-variable limit — the `max_history_commits` argument, at the
    /// scale a research corpus actually reaches.
    pub max_research_delete_ids: usize,
}

/// `/research` — the Ollama-driven iterative research endpoint. All TOML-only:
/// research is an opt-in, host-local feature, not a deployment-shape flag.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResearchConfig {
    /// Base URL of the local Ollama server driving the research loop.
    pub ollama_url: Url,
    /// Model used when a request omits `model`. Empty = no default: a request
    /// without an explicit model is then rejected (400 `research.model_missing`).
    pub default_model: String,
    /// Glob whitelist of models `/research` may run (e.g. `["gemma4:*"]`). Empty =
    /// any model. Matched case-sensitively against the *resolved* model — request
    /// `model` or `default_model`, tag included, so `"gemma4:*"` matches
    /// `"gemma4:27b"` but not bare `"gemma4"`; list both, or use `"gemma4*"`. A
    /// request naming anything outside the list is refused with 400
    /// `research.model_not_allowed`, and `GET /config` publishes `research.models`
    /// already filtered to the allowed set. A non-empty list must cover a
    /// non-empty `default_model` (startup refuses otherwise — every defaulted
    /// request would be a 400).
    pub allowed_models: Vec<String>,
    /// Threads in the dedicated research runtime (research is rare — keep small).
    pub worker_threads: usize,
    /// Concurrent research jobs; beyond this a request gets 429 `research.busy`.
    pub max_concurrent: usize,
    /// **Ceiling** on the context window, not the window itself: each request names
    /// its own model, and a model's trained context length is discoverable
    /// (`/api/show`), so that length is what gets requested — capped by this value.
    /// Raise it if the GPU has room, lower it if `num_ctx` allocation is crowding
    /// the embedder out of VRAM.
    pub max_num_ctx_tokens: u64,
    /// Whole-turn timeout for one Ollama reply (thinking models think long). A
    /// backstop against a wedged server, not the run's budget: `max_seconds` is the
    /// budget and it is enforced by cancelling the turn in flight.
    pub turn_timeout_ms: u64,
    /// Abandon one turn if Ollama has not produced a single token within this many
    /// milliseconds of the request going out. `0` disables the guard.
    ///
    /// The complement of the two guards around it: `turn_timeout_ms` catches a socket
    /// that died, `max_turn_thinking_chars` catches a model that will not stop talking,
    /// and this catches the case both are blind to — a live connection that says
    /// nothing at all, which on this host means Ollama reloading a model it keeps
    /// evicting. It bounds only the **silent prefix** of a turn: the moment any token
    /// arrives, thinking or content, the guard is spent and `turn_timeout_ms` is again
    /// the only ceiling.
    pub first_token_timeout_ms: u64,
    /// Abandon one turn once its **thinking** channel has streamed this many
    /// characters. `0` disables the guard.
    ///
    /// The pathology it catches is a model that loops in the channel that gets
    /// discarded from the transcript: the socket is healthy and tokens keep arriving,
    /// so `turn_timeout_ms` cannot see it and only the deadline stops it — after the
    /// whole run has been spent producing nothing.
    ///
    /// Two pathologies, both measured live on 2026-07-30 and both caught by the default
    /// (8192): a wedged *investigation* turn generating ~18 chars/s is abandoned at
    /// ~445 s of a 900 s deadline, leaving enough budget to still investigate; a wedged
    /// *report* turn at ~310 chars/s is abandoned in seconds. The trade is a 3.1× margin
    /// over the busiest healthy turn measured (2642 characters) instead of the order of
    /// magnitude an earlier value had — which never tripped on the slow one.
    ///
    /// Not per-model, deliberately: the healthy populations of the two measured models
    /// differ by less than the margin above either of them.
    ///
    /// Characters rather than tokens because that is what a delta carries — and it is
    /// the more model-neutral unit of the two, since tokenizers differ.
    pub max_turn_thinking_chars: usize,
    /// Below this generation rate — tokens per second of Ollama's own
    /// `eval_duration` — a turn is logged as contention. `0` disables it.
    ///
    /// The third member of the per-turn guard family, and the only one that does not
    /// *stop* anything: it names a diagnosis. A run inching along at a token a second
    /// is the same symptom as a broken model, a bad prompt and a wedged server, and
    /// on a host whose GPU is shared with whatever else the operator is running, the
    /// answer is usually none of those — one measured run spent 985 s at ~1.5 tok/s
    /// with nothing anywhere able to say why.
    ///
    /// **Defaults to `0`, and that is the honest default.** A healthy rate is a fact
    /// about one model on one host — 15 tok/s is fine for a 30B and alarming for a
    /// 7B — so shipping a number would be guessing on the operator's behalf, exactly
    /// as `temperature` declines to. Read `mindex_research_turn_tokens_per_second`
    /// for a few runs, then set this under the low end of the healthy population.
    pub slow_turn_tokens_per_second: f64,
    /// How long the report phase gets after the investigation ends, in milliseconds.
    ///
    /// A window of its own rather than a slice of `max_seconds`, because a run
    /// stopped by its deadline is exactly the run that most needs to synthesise what
    /// it found. The whole phase — draft, citation check, any revalidation, the
    /// rewrite — is cancelled when this expires, and whatever exists is shipped; if
    /// nothing does, the server writes an honest notice saying the run was cut short.
    /// So the longest a caller can wait is `max_seconds + report_timeout_ms`.
    pub report_timeout_ms: u64,
    /// Investigate this many steps, then spend one turn banking what is answerable.
    /// `0` switches checkpoints off.
    ///
    /// The insurance policy on a run that does not finish. A deadline,
    /// `repeated_calls` or `unparseable` stop used to reach the report phase with
    /// nothing banked, and the caller got a server-written stub after fifteen
    /// minutes — the failure a real field report measured. A checkpoint turn writes
    /// the sections that are already answerable into the run's state, so that same
    /// stop ships findings instead.
    ///
    /// It **costs a step**, deliberately: charging it is what makes it visible in
    /// the budget the operator set, and "price the refusal as a step" is what every
    /// other turn-consuming addition here does. That is also its cost — at 6 against
    /// `medium`'s 20 steps, roughly 15% of the run's lookups become writing turns,
    /// which is a pure loss for a run that would have finished anyway. Measure
    /// coverage with it on and off at the same seeds before trusting this default;
    /// the honest possible answer is "0, except at `low`".
    pub checkpoint_every_steps: usize,
    /// Liveness-ping timeout for the Ollama check in `GET /health`. Ollama is an
    /// optional dependency, so the ping is short and never fails the health verdict.
    pub health_timeout_ms: u64,
    /// How often the local Ollama model registry is re-read for the `research.models`
    /// list `GET /config` publishes.
    ///
    /// A *catalog* refresh, not a health probe: a model pulled or removed after
    /// startup has to reach clients without restarting mindex. Cheap (one GET,
    /// bounded by `health_timeout_ms`) and never fatal — a failed tick keeps the
    /// previous list, so an Ollama that blips does not empty a client's picker.
    pub models_refresh_interval_seconds: u64,
    /// What each `effort` level actually buys. Budgets per level rather than one
    /// step count, because a "step" is not a unit of anything an operator cares
    /// about: `outline` is one indexed SELECT, `search` is a GPU embed plus a vector
    /// query, and one turn may ask for several at once.
    pub effort: EffortBudgets,
    /// Ceiling on a request's `budget.max_seconds` override. A caller may deepen a
    /// run past its effort level, but not past what an operator will let one job
    /// hold a `[research].max_concurrent` slot for.
    pub max_request_seconds: u64,
    /// Ceiling on a request's `budget.max_tokens` override — the GPU-work cap.
    pub max_request_tokens: u64,
    /// Ceiling on a request's `budget.max_steps` override. Also caps the
    /// `budget.checkpoint_every_steps` override: an interval above the step
    /// budget is equivalent to `0` (off), so a dedicated ceiling would guard
    /// nothing.
    pub max_request_steps: usize,
    /// Ceiling on a request's `budget.max_report_sections` override.
    ///
    /// Sections are report turns inside one fixed `report_timeout_ms` window,
    /// so this bounds wall-clock spent writing, not report quality: past the
    /// window's capacity extra sections ship as stubs. The floor is
    /// [`MIN_REPORT_SECTIONS`] — below it the sectioned path does not engage.
    pub max_request_report_sections: usize,
    /// Ceiling on a request's `budget.max_report_words` override.
    ///
    /// Held to the same startup check as the effort presets (`words ×`
    /// [`REPORT_WORDS_TO_TOKENS`] must stay under half of `max_num_ctx_tokens`),
    /// which is what makes any request-supplied value safe to arm `num_predict`
    /// with — there is no per-request window check.
    pub max_request_report_words: usize,
    /// Ceiling on a request's `budget.evidence_width` override.
    ///
    /// Width is paid in prompt tokens on **every** later turn (the transcript is
    /// resent), so the useful range is a small integer; past it a request is
    /// buying context exhaustion, not evidence.
    pub max_evidence_width: u64,
    /// Sampling temperature for every research turn. Absent = **the model's own
    /// Modelfile default**, which is the honest production default but makes model
    /// comparison meaningless: those defaults differ per model (measured here:
    /// temperature 1 for both `glm-4.7-flash` and `qwen3.6`, the latter also with
    /// presence_penalty 1.5). Pin it — low, e.g. 0.2 — before measuring anything.
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff. Same story as `temperature`: absent = the model's
    /// own default.
    pub top_p: Option<f64>,
    /// Default RNG seed. Absent = Ollama picks one per turn, so two runs of one
    /// question differ. A request's `seed` overrides this; that is the axis a
    /// measurement harness varies to get repetitions worth averaging.
    pub seed: Option<i64>,
    /// How long a finished run is kept before `/gc` reaps it.
    ///
    /// Stamped onto the row as `expires_at` at insert, **not** compared against this
    /// value at sweep time — so changing it affects future runs only, and a run's
    /// deadline is a property of the run rather than of the config it outlived. A
    /// **pinned** run has `expires_at IS NULL` and is never reaped, which is how a
    /// report worth keeping outlives any retention an operator picks.
    ///
    /// Not the `[workers].status_log_retention_days` case despite the shape: that is
    /// an audit log nobody reads twice, this is the corpus a later run is given as
    /// context. Hence the far longer default.
    pub retention_days: u64,
    /// How many earlier runs a request may name in `context_run_ids`.
    ///
    /// A cap on hearsay. Each injected report is prompt tokens on *every* turn (the
    /// whole transcript is resent), so this multiplies against `max_tokens` and
    /// `context_fraction` rather than being paid once. `0` disables the feature and
    /// makes any `context_run_ids` a 400.
    pub max_context_runs: usize,
    /// Total characters of prior reports injected into one run, across all of them.
    ///
    /// The last report included is truncated to fit, with a visible marker — a
    /// silently clipped report would let the model reason from half a conclusion,
    /// which is the `note`-cap argument. ~4 chars per token, so the default is of
    /// order 6k tokens: under a tenth of the smallest window the server asks for.
    pub max_context_chars: usize,
    /// Ceiling on `limit` for `GET /projects/{guid}/research`.
    ///
    /// A request-shape limit, so TOML-only like `[limits]` — tuning it in a container
    /// means mounting a `config.toml`.
    pub list_page_limit: usize,
}

/// The compiled form of `[research].allowed_models`: the glob patterns, parsed
/// once at startup and shared by `Arc` so cloning `RouterState` per request stays
/// free. Empty = unrestricted — the compiled default, so an absent key changes
/// nothing.
#[derive(Debug, Clone, Default)]
pub struct AllowedModels(Arc<[glob::Pattern]>);

impl AllowedModels {
    /// Compile every pattern, collecting one message per invalid glob (the
    /// config-validation contract: all problems at once, never fail-fast).
    pub fn compile(patterns: &[String]) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let mut compiled = Vec::with_capacity(patterns.len());
        for (i, pat) in patterns.iter().enumerate() {
            match glob::Pattern::new(pat) {
                Ok(p) => compiled.push(p),
                Err(err) => errors.push(format!(
                    "[research].allowed_models[{i}] = {pat:?} is not a valid glob: {err}. \
                     Fix: correct the pattern (glob syntax: `*`, `?`, `[..]`)."
                )),
            }
        }
        if errors.is_empty() {
            Ok(Self(compiled.into()))
        } else {
            Err(errors)
        }
    }

    /// An empty list means the whitelist is off, not that nothing is allowed.
    pub fn is_unrestricted(&self) -> bool {
        self.0.is_empty()
    }

    /// Case-sensitive match against the resolved model name, tag included.
    pub fn allows(&self, model: &str) -> bool {
        self.is_unrestricted() || self.0.iter().any(|p| p.matches(model))
    }

    /// The raw pattern strings, for `GET /config`.
    pub fn patterns(&self) -> Vec<String> {
        self.0.iter().map(|p| p.as_str().to_string()).collect()
    }
}

/// The budgets per effort level. A run stops at whichever is reached first, and
/// says which in `done.reason`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EffortBudgets {
    pub low: EffortBudget,
    pub medium: EffortBudget,
    pub high: EffortBudget,
}

/// One effort level's budgets.
///
/// **Wall-clock is the primary one** — measured, not assumed: at the deepest
/// setting a run's largest prompt was ~12k tokens against a 65k window (18%), so a
/// context ceiling almost never binds, while time is both what the caller waits and
/// what holds a `max_concurrent` slot. `max_tokens` is the *cost* axis (the whole
/// transcript is resent every turn, so tokens are what the GPU actually does). The
/// other two are guards: `context_fraction` against silent truncation on a
/// small-window model, `max_steps` against a loop that spends its time on cheap
/// lookups without accumulating evidence.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EffortBudget {
    /// Wall-clock deadline for the whole investigation, excluding the report phase.
    ///
    /// A **hard** deadline, not a poll: it is checked between turns for a graceful
    /// stop *and* enforced by cancelling the turn in flight, which aborts the Ollama
    /// request and every tool call under it. Whatever the run has found by then goes
    /// to the report phase, which has its own `[research].report_timeout_ms` — so the
    /// longest a caller waits is `max_seconds + report_timeout_ms`.
    pub max_seconds: u64,
    /// Local tokens (prompt + eval, summed over turns) the investigation may spend.
    /// Sized from measurement: a `medium` run of 8 steps read 52149 prompt + 3431
    /// eval tokens, and the sum grows super-linearly with turns because each turn
    /// resends the transcript.
    pub max_tokens: u64,
    /// Stop once a prompt reaches this fraction of the effective context window.
    /// Guards the small-window case; on a large window it never fires. Deliberately
    /// **not** overridable per request: raising it buys silent truncation, nothing
    /// else.
    pub context_fraction: f64,
    /// Backstop on executed tool calls.
    pub max_steps: usize,
    /// Chunks each `search` tool call returns to the model — the per-question
    /// evidence width.
    ///
    /// A knob rather than a `const` because the right number is model-dependent:
    /// it trades context (every hit is resent on every later turn, so a wider
    /// search compounds into the token budget) against coverage. Measured on this
    /// repo, widening it is *not* the fix for the failure it looks like: every
    /// search already returned 5 hits and the run still missed the answer, because
    /// the queries were natural language and this codebase's prose lives in its
    /// tests. Defaults therefore keep 5 at every level — this exists to be swept
    /// by a measurement harness, not to be raised on a hunch.
    ///
    /// Validated at startup against `[search].max_top_k`, which is what keeps the
    /// research loop inside that cap: it builds `SearchRequest` directly and never
    /// passes through the request-validation layer.
    pub search_top_k: u64,
    /// Ceiling, in words, announced to the model for the final report. `0` = say
    /// nothing about length (the behaviour before this knob existed).
    ///
    /// The one axis that is about **output** rather than input. It exists because
    /// retrieval is not where a research run fails: a field report over a real
    /// project found the loop locating the right files every time and failing at
    /// the report turn, deterministically by request shape — a broad question with
    /// five sub-questions failed, the same question narrowed to one file with a
    /// word limit succeeded in minutes. Nothing in the prompt had ever mentioned
    /// length, so the de-facto size of a report was `PLAN_REQUEST`'s "3-6
    /// sub-questions" multiplied by whatever the model felt like writing.
    ///
    /// A ceiling, not a target — the prompt says so — and it also drives
    /// `num_predict` on the report turn as a runaway backstop. Overridable per
    /// request (`budget.max_report_words`, `0` = off), capped by
    /// `[research].max_request_report_words`: the original "every value a caller
    /// would pick is bigger" objection is answered by the cap being held to the
    /// same window check as these presets.
    ///
    /// The numbers are unmeasured. `0` is what makes that honest: the whole
    /// mechanism reverts from config while a harness sweeps for where the cliff
    /// actually is, which is certainly per-model.
    pub max_report_words: usize,
    /// Report sections one run may write (the sectioned report's `.take()` cap,
    /// and the upper bound the plan turn's prompt asks for: "3-N sub-questions").
    ///
    /// Overridable per request (`budget.max_report_sections`), floor
    /// [`MIN_REPORT_SECTIONS`], capped by
    /// `[research].max_request_report_sections`. Sections trade depth per
    /// section for coverage inside one fixed `report_timeout_ms` window — more
    /// sections is not more report, past the window it is more stubs.
    pub max_report_sections: usize,
    /// Multiplier on the per-call evidence widths: `read_chunks`, `grep`,
    /// `callers`, `file_history`, `symbols`. `1` = the historical widths.
    ///
    /// Overridable per request (`budget.evidence_width`), capped by
    /// `[research].max_evidence_width`. Deliberately **not** applied to the
    /// navigation tools (`outline`, `list_files` — when 300 rows bind, the fix
    /// is a narrower glob), to `search` (its own axis, `search_top_k`), or to
    /// the excerpt channel (a response cap, not an evidence width). Width
    /// compounds into `max_tokens`: every widened result is resent on every
    /// later turn.
    pub evidence_width: u64,
}

/// Prometheus exposition and the state-gauge collector.
///
/// TOML-only, like `[limits]`: these shape an operator's observability stack,
/// not a per-invocation choice, and tuning them in a container means mounting a
/// `config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// Serve `GET /metrics` and run the state collector.
    ///
    /// This gates *exposure*, not measurement: the registry is always built and
    /// always written into. A counter increment is a relaxed atomic add, and the
    /// alternative — an `Option` check at every call site — costs far more in
    /// code than it saves in cycles.
    pub enabled: bool,
    /// How often the per-project state gauges are recomputed from SQLite.
    ///
    /// Should be at or below the scrape interval: slower, and consecutive
    /// scrapes report the same value, which renders as a staircase.
    pub refresh_interval_seconds: u64,
    /// Sample Qdrant/embedder/Ollama liveness on each refresh, into
    /// `mindex_dependency_up`. The probes run concurrently and are each bounded
    /// by their client's health timeout, so the cost is off the request path.
    pub probe_dependencies: bool,
    /// Add a `project_guid` label to the HTTP request counter.
    ///
    /// Off by default. `project_guid` is the only label whose value a client
    /// chooses, and while it is UUID-validated before it becomes one, crossing
    /// it with route makes the HTTP families grow with the project count for a
    /// breakdown the domain metrics already give per project.
    pub per_project_http_labels: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkerConfig {
    pub gc_interval_seconds: u64,
    pub status_log_retention_days: u64,
    pub retry_interval_seconds: u64,
    pub failed_warn_interval_seconds: u64,
    pub max_retries: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerConfig,
    pub model: ModelConfig,
    pub qdrant: QdrantConfig,
    pub database: DatabaseConfig,
    pub indexing: IndexingConfig,
    pub slicer: SlicerConfig,
    pub search: SearchConfig,
    pub limits: LimitsConfig,
    pub workers: WorkerConfig,
    pub research: ResearchConfig,
    pub metrics: MetricsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind addr"),
            cert_path: PathBuf::from(DEFAULT_CERT_PATH),
            key_path: PathBuf::from(DEFAULT_KEY_PATH),
            max_body_mib: DEFAULT_MAX_BODY_MIB,
            http3: false,
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_MODEL_NAME.to_string(),
            server_url: DEFAULT_MODEL_SERVER
                .parse()
                .expect("valid default model url"),
            // One instance serves both paths unless an operator splits them.
            query_server_url: None,
            health_timeout_ms: DEFAULT_HEALTH_TIMEOUT_MS,
            max_429_retries: DEFAULT_MAX_429_RETRIES,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            encode_timeout_ms: DEFAULT_ENCODE_TIMEOUT_MS,
        }
    }
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_QDRANT_SERVER
                .parse()
                .expect("valid default qdrant url"),
            upsert_batch_points: DEFAULT_UPSERT_BATCH_POINTS,
            dense_prefetch_limit: DEFAULT_DENSE_PREFETCH_LIMIT,
            sparse_prefetch_limit: DEFAULT_SPARSE_PREFETCH_LIMIT,
            fusion_limit: DEFAULT_FUSION_LIMIT,
            timeout_ms: DEFAULT_QDRANT_TIMEOUT_MS,
            connect_timeout_ms: DEFAULT_QDRANT_CONNECT_TIMEOUT_MS,
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_DB_PATH),
            pool_size: DEFAULT_DB_POOL_SIZE,
            page_size_bytes: DEFAULT_PAGE_SIZE_BYTES,
            synchronous: DEFAULT_SYNCHRONOUS.to_string(),
        }
    }
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            embed_batch_chunks: DEFAULT_EMBED_BATCH_CHUNKS,
            stuck_grace_minutes: DEFAULT_STUCK_GRACE_MINUTES,
            path_batch_size: DEFAULT_PATH_BATCH_SIZE,
            sparse_min_weight: DEFAULT_SPARSE_MIN_WEIGHT,
        }
    }
}

impl Default for SlicerConfig {
    fn default() -> Self {
        Self {
            min_chunk_tokens: DEFAULT_MIN_CHUNK_TOKENS,
            max_chunk_tokens: DEFAULT_MAX_CHUNK_TOKENS,
            fill_gaps: DEFAULT_FILL_GAPS,
            max_doc_chunk_tokens: DEFAULT_MAX_DOC_CHUNK_TOKENS,
            doc_semantic_weight: DEFAULT_DOC_SEMANTIC_WEIGHT,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_top_k: DEFAULT_TOP_K,
            max_top_k: DEFAULT_MAX_TOP_K,
            max_query_bytes: DEFAULT_MAX_QUERY_BYTES,
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_code_bytes: DEFAULT_MAX_CODE_BYTES,
            max_files_per_request: DEFAULT_MAX_FILES_PER_REQUEST,
            max_drift_files: DEFAULT_MAX_DRIFT_FILES,
            max_selector_patterns: DEFAULT_MAX_SELECTOR_PATTERNS,
            max_symbol_name_bytes: DEFAULT_MAX_SYMBOL_NAME_BYTES,
            max_symbol_results: DEFAULT_MAX_SYMBOL_RESULTS,
            max_history_commits: DEFAULT_MAX_HISTORY_COMMITS,
            max_commit_message_bytes: DEFAULT_MAX_COMMIT_MESSAGE_BYTES,
            max_research_delete_ids: DEFAULT_MAX_RESEARCH_DELETE_IDS,
        }
    }
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            ollama_url: DEFAULT_OLLAMA_URL
                .parse()
                .expect("valid default ollama url"),
            default_model: String::new(),
            allowed_models: Vec::new(),
            worker_threads: DEFAULT_RESEARCH_WORKER_THREADS,
            max_concurrent: DEFAULT_RESEARCH_MAX_CONCURRENT,
            max_num_ctx_tokens: DEFAULT_RESEARCH_MAX_NUM_CTX,
            effort: EffortBudgets::default(),
            max_request_seconds: DEFAULT_RESEARCH_MAX_REQUEST_SECONDS,
            max_request_tokens: DEFAULT_RESEARCH_MAX_REQUEST_TOKENS,
            max_request_steps: DEFAULT_RESEARCH_MAX_REQUEST_STEPS,
            max_request_report_sections: DEFAULT_RESEARCH_MAX_REQUEST_REPORT_SECTIONS,
            max_request_report_words: DEFAULT_RESEARCH_MAX_REQUEST_REPORT_WORDS,
            max_evidence_width: DEFAULT_RESEARCH_MAX_EVIDENCE_WIDTH,
            turn_timeout_ms: DEFAULT_RESEARCH_TURN_TIMEOUT_MS,
            first_token_timeout_ms: DEFAULT_RESEARCH_FIRST_TOKEN_TIMEOUT_MS,
            max_turn_thinking_chars: DEFAULT_RESEARCH_MAX_TURN_THINKING_CHARS,
            slow_turn_tokens_per_second: 0.0,
            report_timeout_ms: DEFAULT_RESEARCH_REPORT_TIMEOUT_MS,
            checkpoint_every_steps: DEFAULT_RESEARCH_CHECKPOINT_EVERY_STEPS,
            health_timeout_ms: DEFAULT_RESEARCH_HEALTH_TIMEOUT_MS,
            models_refresh_interval_seconds: DEFAULT_RESEARCH_MODELS_REFRESH_SECONDS,
            // Unset = the model's own Modelfile defaults. Deliberately not pinned
            // by default: a server-wide temperature is a quality decision an
            // operator should make knowingly, and every model ships one already.
            temperature: None,
            top_p: None,
            seed: None,
            retention_days: DEFAULT_RESEARCH_RETENTION_DAYS,
            max_context_runs: DEFAULT_RESEARCH_MAX_CONTEXT_RUNS,
            max_context_chars: DEFAULT_RESEARCH_MAX_CONTEXT_CHARS,
            list_page_limit: DEFAULT_RESEARCH_LIST_PAGE_LIMIT,
        }
    }
}

impl Default for EffortBudgets {
    fn default() -> Self {
        // The wall-clock ladder is 5 / 15 / 60 minutes, set from use rather than from
        // a single measurement: `medium` at 240 s was observed cutting glm off
        // mid-investigation on real questions about this repo, and since the deadline
        // is now hard, being cut off means shipping a partial report rather than
        // quietly running long.
        //
        // **The token budgets scale with it, and that is not optional.** The whole
        // transcript is resent every turn, so cost grows super-linearly: glm spent
        // ~160k prompt tokens in a 120 s run, which puts an hour somewhere north of
        // 5M even before the super-linear term. Leaving the old 1.5M on `high` would
        // make `tokens_exhausted` the universal stop and the new wall-clock
        // unreachable — the ladder would look raised and behave exactly as before.
        //
        // `max_steps` stays a coarse backstop and is barely moved: raising `medium`
        // 20 → 48 was measured to change nothing (median depth 16 → 16, citations
        // 60 → 32), which is precisely why time and tokens are the budgets and a step
        // is not.
        //
        // `max_report_words` is the newest axis and the only unmeasured one. It rises
        // with the ladder because a deeper run has more to say, not because 1800 was
        // observed anywhere; the field report that motivated it establishes only the
        // direction (shrinking the required answer is what worked) and not the cliff.
        // Sweep it per model before trusting any of these three numbers.
        Self {
            low: EffortBudget {
                max_seconds: 300,
                max_tokens: 400_000,
                context_fraction: 0.5,
                max_steps: 8,
                search_top_k: DEFAULT_RESEARCH_SEARCH_TOP_K,
                max_report_words: 400,
                max_report_sections: DEFAULT_RESEARCH_MAX_REPORT_SECTIONS,
                evidence_width: DEFAULT_RESEARCH_EVIDENCE_WIDTH,
            },
            medium: EffortBudget {
                max_seconds: 900,
                max_tokens: 1_200_000,
                context_fraction: 0.7,
                max_steps: 20,
                search_top_k: DEFAULT_RESEARCH_SEARCH_TOP_K,
                max_report_words: 900,
                max_report_sections: DEFAULT_RESEARCH_MAX_REPORT_SECTIONS,
                evidence_width: DEFAULT_RESEARCH_EVIDENCE_WIDTH,
            },
            high: EffortBudget {
                max_seconds: 3600,
                max_tokens: 6_000_000,
                context_fraction: 0.85,
                max_steps: 64,
                search_top_k: DEFAULT_RESEARCH_SEARCH_TOP_K,
                max_report_words: 1800,
                max_report_sections: DEFAULT_RESEARCH_MAX_REPORT_SECTIONS,
                evidence_width: DEFAULT_RESEARCH_EVIDENCE_WIDTH,
            },
        }
    }
}

impl Default for EffortBudget {
    fn default() -> Self {
        EffortBudgets::default().medium
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_METRICS_ENABLED,
            refresh_interval_seconds: DEFAULT_METRICS_REFRESH_SECONDS,
            probe_dependencies: DEFAULT_METRICS_PROBE_DEPENDENCIES,
            per_project_http_labels: DEFAULT_METRICS_PER_PROJECT_HTTP_LABELS,
        }
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            gc_interval_seconds: DEFAULT_GC_INTERVAL_SECONDS,
            status_log_retention_days: DEFAULT_STATUS_LOG_RETENTION_DAYS,
            retry_interval_seconds: DEFAULT_RETRY_INTERVAL_SECONDS,
            failed_warn_interval_seconds: DEFAULT_FAILED_WARN_INTERVAL_SECONDS,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

/// Command-line flags. Every operational setting is an `Option`: `None` means
/// "not passed", so the value falls through to the config file (then the
/// built-in default). The help text states the default but clap holds **no**
/// `default_value` — that single-sourcing is what makes "flag overrides file
/// overrides default" detectable.
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = concat!(
        "mindex is a high-performance semantic search engine built in Rust. ",
        "It leverages the BGE-M3 model for hybrid (dense/sparse) retrieval ",
        "combined with advanced reranking techniques to deliver accurate, ",
        "context-aware search results."
    )
)]
pub struct Cli {
    /// Path to a TOML config file. Overrides XDG discovery
    /// ($XDG_CONFIG_HOME/mindex/config.toml then $XDG_CONFIG_DIRS). If given and
    /// unreadable/invalid, startup fails.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Interface to bind the server (default: 127.0.0.1:11111).
    #[arg(short, long)]
    pub bind: Option<SocketAddr>,

    /// Path to the TLS certificate file (default: cert.pem).
    #[arg(long)]
    pub cert_path: Option<PathBuf>,

    /// Path to the TLS private key file (default: key.pem).
    #[arg(long)]
    pub key_path: Option<PathBuf>,

    /// Name of the model to use (default: BAAI/bge-m3).
    #[arg(long)]
    pub model: Option<String>,

    /// Model API server (default: http://localhost:11211).
    #[arg(long)]
    pub model_server: Option<Url>,

    /// Qdrant server (default: http://localhost:6334).
    #[arg(long)]
    pub qdrant_server: Option<Url>,

    /// Path to the SQLite database file (default: mindex.db; use :memory: for in-memory).
    #[arg(long)]
    pub db_path: Option<PathBuf>,

    /// DB pool size (default: 4).
    #[arg(long)]
    pub db_pool_size: Option<usize>,

    /// Chunks sent to the model server per /encode call during indexing (default: 256).
    #[arg(long)]
    pub embed_batch: Option<usize>,

    /// Max /index request body size in MiB (default: 256).
    #[arg(long)]
    pub max_body_mib: Option<usize>,

    /// Minutes a file may sit in 'indexing' before the retry worker treats it as
    /// crash-orphaned (default: 30). Must exceed the longest legitimate in-flight request.
    #[arg(long)]
    pub stuck_grace_mins: Option<i64>,

    /// Enable HTTP/3 over QUIC (UDP) on the same port as the TLS server.
    /// Requires --cert-path and --key-path to be explicitly provided.
    #[arg(long, default_value = "false")]
    pub http3: bool,
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// A fatal configuration problem. Its `Display` is the full, multi-line message
/// already logged; returning it from `main` aborts startup with a non-zero code.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Where the effective config came from (for logging and `GET /config`).
#[derive(Debug, Clone)]
pub enum ConfigSource {
    File(PathBuf),
    Defaults,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::File(p) => write!(f, "{}", p.display()),
            ConfigSource::Defaults => write!(f, "<built-in defaults>"),
        }
    }
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Candidate config paths in priority order, per the XDG Base Directory spec.
/// `explicit` (from `--config` or `$MINDEX_CONFIG`) wins outright; otherwise
/// `$XDG_CONFIG_HOME/mindex/config.toml` (defaulting to `~/.config`), then each
/// dir in `$XDG_CONFIG_DIRS` (defaulting to `/etc/xdg`).
fn candidate_paths(explicit: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(p) = explicit {
        return vec![p];
    }

    let mut paths = Vec::new();

    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(home) = config_home {
        paths.push(home.join("mindex").join("config.toml"));
    }

    let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/etc/xdg".to_string());
    for dir in config_dirs.split(':').filter(|d| !d.is_empty()) {
        paths.push(PathBuf::from(dir).join("mindex").join("config.toml"));
    }

    paths
}

/// Load the config file (if any), apply CLI overrides, validate, and log
/// everything an operator needs to diagnose a config mix-up: which paths were
/// checked, which file (if any) was loaded, and every value a flag overrode.
/// Returns the effective config and its source, or a fatal [`ConfigError`]
/// (already logged) on which the caller must refuse to start.
pub fn resolve(cli: &Cli) -> Result<(Config, ConfigSource), ConfigError> {
    let explicit = cli
        .config
        .clone()
        .or_else(|| std::env::var_os("MINDEX_CONFIG").map(PathBuf::from));
    let is_explicit = explicit.is_some();

    let candidates = candidate_paths(explicit);
    let mut chosen: Option<PathBuf> = None;
    for path in &candidates {
        if path.is_file() {
            info!(path = %path.display(), "Config file found.");
            chosen = Some(path.clone());
            break;
        }
        info!(path = %path.display(), "Config file not present here; continuing search.");
    }

    let (mut config, source) = match chosen {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|e| {
                ConfigError(format!(
                    "could not read config file {}: {e}. \
                     Fix: ensure the file exists and is readable by this process.",
                    path.display()
                ))
            })?;
            let parsed: Config = toml::from_str(&text).map_err(|e| {
                ConfigError(format!(
                    "could not parse config file {} as TOML: {e}. \
                     Fix: correct the syntax / key name shown above (unknown keys are rejected).",
                    path.display()
                ))
            })?;
            info!(path = %path.display(), "Loaded configuration from file.");
            (parsed, ConfigSource::File(path))
        }
        None => {
            if is_explicit {
                return Err(ConfigError(
                    "an explicit config path (--config / $MINDEX_CONFIG) was given but no file \
                     was found there. Fix: correct the path or remove the override."
                        .to_string(),
                ));
            }
            info!("No config file found in any XDG location; using built-in defaults.");
            (Config::default(), ConfigSource::Defaults)
        }
    };

    apply_cli_overrides(&mut config, cli);

    // --http3 requires explicit cert/key CLI flags because the QUIC endpoint reads
    // the certificate independently of axum-server.  Relying on path defaults risks
    // a silent misconfiguration, so we enforce clarity here.
    if cli.http3 && (cli.cert_path.is_none() || cli.key_path.is_none()) {
        let mut missing = Vec::new();
        if cli.cert_path.is_none() {
            missing.push("--cert-path");
        }
        if cli.key_path.is_none() {
            missing.push("--key-path");
        }
        return Err(ConfigError(format!(
            "--http3 requires {} to be explicitly provided as a CLI flag{}. \
             Fix: pass {} when enabling HTTP/3.",
            missing.join(" and "),
            if missing.len() == 1 { "" } else { "s" },
            missing.join(" and "),
        )));
    }

    if let Err(errors) = config.validate() {
        let body = errors
            .iter()
            .map(|e| format!("  • {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(ConfigError(format!(
            "configuration is invalid; refusing to start. {} problem(s) (source: {source}):\n{body}",
            errors.len()
        )));
    }

    Ok((config, source))
}

/// Apply each `Some(_)` flag onto the loaded config, logging every override so
/// "why is this value not what the file says" is answerable from the log alone.
fn apply_cli_overrides(cfg: &mut Config, cli: &Cli) {
    macro_rules! over {
        ($flag:expr, $target:expr, $key:literal) => {
            if let Some(v) = $flag.clone() {
                info!(key = $key, old = ?$target, new = ?v, "Config value overridden by CLI flag.");
                $target = v;
            }
        };
    }

    over!(cli.bind, cfg.server.bind, "server.bind");
    over!(cli.cert_path, cfg.server.cert_path, "server.cert_path");
    over!(cli.key_path, cfg.server.key_path, "server.key_path");
    over!(
        cli.max_body_mib,
        cfg.server.max_body_mib,
        "server.max_body_mib"
    );
    over!(cli.model, cfg.model.name, "model.name");
    over!(cli.model_server, cfg.model.server_url, "model.server_url");
    over!(
        cli.qdrant_server,
        cfg.qdrant.server_url,
        "qdrant.server_url"
    );
    over!(cli.db_path, cfg.database.path, "database.path");
    over!(
        cli.db_pool_size,
        cfg.database.pool_size,
        "database.pool_size"
    );
    over!(
        cli.embed_batch,
        cfg.indexing.embed_batch_chunks,
        "indexing.embed_batch_chunks"
    );
    over!(
        cli.stuck_grace_mins,
        cfg.indexing.stuck_grace_minutes,
        "indexing.stuck_grace_minutes"
    );
    if cli.http3 {
        info!(
            key = "server.http3",
            old = false,
            new = true,
            "Config value overridden by CLI flag."
        );
        cfg.server.http3 = true;
    }
}

// ── Validation ────────────────────────────────────────────────────────────────

impl Config {
    /// Collect **every** validation problem (not fail-fast) so an operator sees
    /// all of them in one startup attempt. Each message states the offending
    /// key + value, what is wrong, and how to fix it.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut e = Vec::new();

        if self.server.max_body_mib < 1 {
            e.push(format!(
                "[server].max_body_mib = {} is too small. Fix: set it to at least 1 (MiB).",
                self.server.max_body_mib
            ));
        }

        if self.model.max_429_retries > 20 {
            e.push(format!(
                "[model].max_429_retries = {} is implausibly high. Fix: use a small count (e.g. 3).",
                self.model.max_429_retries
            ));
        }
        if self.model.backoff_base_ms < 1 {
            e.push(
                "[model].backoff_base_ms = 0 disables backoff. Fix: use at least 1 (ms), e.g. 200."
                    .to_string(),
            );
        }
        if self.model.health_timeout_ms < 1 {
            e.push("[model].health_timeout_ms = 0 would time out instantly. Fix: use at least 1 (ms), e.g. 2000.".to_string());
        }
        if self.model.encode_timeout_ms < 1 {
            e.push("[model].encode_timeout_ms = 0 would time out instantly. Fix: use at least 1 (ms), e.g. 600000.".to_string());
        }

        if self.qdrant.upsert_batch_points < 1 {
            e.push("[qdrant].upsert_batch_points = 0 would never upsert. Fix: use at least 1 (e.g. 256).".to_string());
        }
        // Checked against `max_top_k`, not `default_top_k`: the default is only what a
        // request gets when it asks for nothing, while the maximum is what a request is
        // *permitted* to ask for. With the looser check a caller asking for the
        // documented maximum got a silently short result set — no warning at startup,
        // none at query time, and a correct-looking response.
        if self.qdrant.fusion_limit < self.search.max_top_k as u32 {
            e.push(format!(
                "[qdrant].fusion_limit = {} is below [search].max_top_k = {}; a request \
                 asking for max_top_k results would be silently truncated by the reranker. \
                 Fix: set fusion_limit >= max_top_k.",
                self.qdrant.fusion_limit, self.search.max_top_k
            ));
        }
        if self.qdrant.timeout_ms < 1 {
            e.push(
                "[qdrant].timeout_ms = 0 would time out every call immediately. \
                 Fix: use at least 1000 (e.g. 30000)."
                    .to_string(),
            );
        }
        if self.qdrant.connect_timeout_ms < 1 {
            e.push(
                "[qdrant].connect_timeout_ms = 0 would fail every connection attempt. \
                 Fix: use at least 1000 (e.g. 5000)."
                    .to_string(),
            );
        }
        if self.qdrant.dense_prefetch_limit < self.qdrant.fusion_limit {
            e.push(format!(
                "[qdrant].dense_prefetch_limit = {} is below fusion_limit = {}; fusion starves. \
                 Fix: set dense_prefetch_limit >= fusion_limit.",
                self.qdrant.dense_prefetch_limit, self.qdrant.fusion_limit
            ));
        }
        if self.qdrant.sparse_prefetch_limit < self.qdrant.fusion_limit {
            e.push(format!(
                "[qdrant].sparse_prefetch_limit = {} is below fusion_limit = {}; fusion starves. \
                 Fix: set sparse_prefetch_limit >= fusion_limit.",
                self.qdrant.sparse_prefetch_limit, self.qdrant.fusion_limit
            ));
        }

        if self.database.pool_size < 1 {
            e.push(
                "[database].pool_size = 0 leaves no connections. Fix: use at least 1 (e.g. 4)."
                    .to_string(),
            );
        }
        let ps = self.database.page_size_bytes;
        if !(SQLITE_MIN_PAGE_SIZE..=SQLITE_MAX_PAGE_SIZE).contains(&ps) || !ps.is_power_of_two() {
            e.push(format!(
                "[database].page_size_bytes = {ps} is invalid. Fix: use a power of two between \
                 {SQLITE_MIN_PAGE_SIZE} and {SQLITE_MAX_PAGE_SIZE} (e.g. 16384)."
            ));
        }
        if !VALID_SYNCHRONOUS.contains(&self.database.synchronous.to_lowercase().as_str()) {
            e.push(format!(
                "[database].synchronous = {:?} is not a valid SQLite mode. Fix: use one of {}.",
                self.database.synchronous,
                VALID_SYNCHRONOUS.join(" / ")
            ));
        }

        if self.indexing.embed_batch_chunks < 1 {
            e.push("[indexing].embed_batch_chunks = 0 would embed nothing. Fix: use at least 1 (e.g. 256).".to_string());
        }
        let pbs = self.indexing.path_batch_size;
        if !(1..=999).contains(&pbs) {
            e.push(format!(
                "[indexing].path_batch_size = {pbs} is out of range. Fix: use 1..=999 (SQLite \
                 bind-variable limit; default 500)."
            ));
        }
        if self.indexing.stuck_grace_minutes < 1 {
            e.push(format!(
                "[indexing].stuck_grace_minutes = {} is too small. Fix: use at least 1 (default 30).",
                self.indexing.stuck_grace_minutes
            ));
        } else if self.indexing.stuck_grace_minutes < 5 {
            warn!(
                value = self.indexing.stuck_grace_minutes,
                "[indexing].stuck_grace_minutes is very low; it must exceed the longest legitimate \
                 in-flight indexing request or the retry worker can race a live batch. Default is 30."
            );
        }
        if !(self.indexing.sparse_min_weight.is_finite() && self.indexing.sparse_min_weight >= 0.0)
        {
            e.push(format!(
                "[indexing].sparse_min_weight = {} must be a finite, non-negative threshold. \
                 Fix: use a small positive value (e.g. 0.00001).",
                self.indexing.sparse_min_weight
            ));
        }

        if self.slicer.min_chunk_tokens < 1 {
            e.push(
                "[slicer].min_chunk_tokens = 0 is invalid. Fix: use at least 1 (default 128)."
                    .to_string(),
            );
        }
        if self.slicer.min_chunk_tokens >= self.slicer.max_chunk_tokens {
            e.push(format!(
                "[slicer].min_chunk_tokens = {} must be < max_chunk_tokens = {}. Fix: widen the window.",
                self.slicer.min_chunk_tokens, self.slicer.max_chunk_tokens
            ));
        }
        if self.slicer.max_chunk_tokens > MODEL_MAX_TOKENS {
            e.push(format!(
                "[slicer].max_chunk_tokens = {} exceeds the BGE-M3 limit of {MODEL_MAX_TOKENS}; \
                 longer chunks are silently truncated. Fix: set max_chunk_tokens <= {MODEL_MAX_TOKENS}.",
                self.slicer.max_chunk_tokens
            ));
        }
        if self.slicer.max_doc_chunk_tokens < 1 {
            e.push(
                "[slicer].max_doc_chunk_tokens = 0 emits no documentation chunks. \
                 Fix: use at least 1 (default 1024)."
                    .to_string(),
            );
        }
        if self.slicer.max_doc_chunk_tokens > MODEL_INPUT_LIMIT_TOKENS {
            e.push(format!(
                "[slicer].max_doc_chunk_tokens = {} exceeds the embedder's input limit of \
                 {MODEL_INPUT_LIMIT_TOKENS}; longer chunks are silently truncated. \
                 Fix: set max_doc_chunk_tokens <= {MODEL_INPUT_LIMIT_TOKENS}.",
                self.slicer.max_doc_chunk_tokens
            ));
        }
        if !(0.0..=MAX_DOC_SEMANTIC_WEIGHT).contains(&self.slicer.doc_semantic_weight) {
            e.push(format!(
                "[slicer].doc_semantic_weight = {} is outside 0..={MAX_DOC_SEMANTIC_WEIGHT}. \
                 Fix: use 0 to cut documentation by heading structure alone, or a value up to \
                 {MAX_DOC_SEMANTIC_WEIGHT} to let embedding distance refine the boundaries \
                 (default 1). Higher weights collapse the segmentation to one block per chunk.",
                self.slicer.doc_semantic_weight
            ));
        }

        if self.search.default_top_k < 1 {
            e.push(
                "[search].default_top_k = 0 returns nothing. Fix: use at least 1 (default 5)."
                    .to_string(),
            );
        }
        if self.search.max_top_k < self.search.default_top_k {
            e.push(format!(
                "[search].max_top_k = {} is below default_top_k = {}; the default would be rejected. \
                 Fix: set max_top_k >= default_top_k.",
                self.search.max_top_k, self.search.default_top_k
            ));
        }
        if self.search.max_query_bytes < 1 {
            e.push("[search].max_query_bytes = 0 rejects every query. Fix: use at least 1 (default 32768).".to_string());
        }

        if self.limits.max_code_bytes < 1 {
            e.push("[limits].max_code_bytes = 0 rejects every file. Fix: use at least 1 (default 16 MiB).".to_string());
        }
        if self.limits.max_files_per_request < 1 {
            e.push("[limits].max_files_per_request = 0 rejects every index request. Fix: use at least 1 (default 10000).".to_string());
        }
        if self.limits.max_drift_files < 1 {
            e.push("[limits].max_drift_files = 0 rejects every drift request. Fix: use at least 1 (default 200000).".to_string());
        }
        if self.limits.max_selector_patterns < 1 {
            e.push("[limits].max_selector_patterns = 0 rejects every selector. Fix: use at least 1 (default 256).".to_string());
        }
        if self.limits.max_symbol_name_bytes < 1 {
            e.push("[limits].max_symbol_name_bytes = 0 rejects every symbol lookup. Fix: use at least 1 (default 512).".to_string());
        }
        if self.limits.max_symbol_results < 1 {
            e.push("[limits].max_symbol_results = 0 rejects every symbol lookup. Fix: use at least 1 (default 50).".to_string());
        }
        if self.limits.max_research_delete_ids < 1 {
            e.push("[limits].max_research_delete_ids = 0 rejects every batch delete. Fix: use at least 1 (default 500).".to_string());
        }
        if self.limits.max_history_commits < 1 {
            e.push("[limits].max_history_commits = 0 rejects every history reconciliation. Fix: use at least 1 (default 20000).".to_string());
        }
        if self.limits.max_commit_message_bytes < 1 {
            e.push("[limits].max_commit_message_bytes = 0 rejects every commit. Fix: use at least 1 (default 64 KiB).".to_string());
        }
        if self.research.worker_threads < 1 {
            e.push("[research].worker_threads = 0 leaves no thread to run research on. Fix: use at least 1 (default 2).".to_string());
        }
        if self.research.max_concurrent < 1 {
            e.push("[research].max_concurrent = 0 rejects every research request. Fix: use at least 1 (default 2).".to_string());
        }
        match AllowedModels::compile(&self.research.allowed_models) {
            Ok(allowed) => {
                // Only cross-check against a list that compiled whole: a broken
                // glob already has its own error, and a verdict from the partial
                // list would be a cascading second complaint about the same key.
                if !allowed.is_unrestricted()
                    && !self.research.default_model.is_empty()
                    && !allowed.allows(&self.research.default_model)
                {
                    e.push(format!(
                        "[research].default_model = {:?} matches no pattern in \
                         [research].allowed_models, so every request relying on the default \
                         would be refused with 400 research.model_not_allowed. Fix: add a \
                         matching pattern (note `\"name:*\"` does not match a bare `\"name\"`) \
                         or change default_model.",
                        self.research.default_model
                    ));
                }
            }
            Err(errors) => e.extend(errors),
        }
        for (level, b) in [
            ("low", &self.research.effort.low),
            ("medium", &self.research.effort.medium),
            ("high", &self.research.effort.high),
        ] {
            if b.max_seconds < 10 {
                e.push(format!(
                    "[research.effort.{level}].max_seconds = {} leaves no time for even one \
                     model turn. Fix: use at least 10 (defaults 60/240/900).",
                    b.max_seconds
                ));
            }
            if b.max_tokens < 1000 {
                e.push(format!(
                    "[research.effort.{level}].max_tokens = {} is below one prompt, so the run \
                     would stop before its first turn. Fix: use at least 1000 (defaults \
                     60000/400000/1500000); it counts prompt + eval tokens summed over turns, \
                     not per turn.",
                    b.max_tokens
                ));
            }
            if b.max_steps < 1 {
                e.push(format!(
                    "[research.effort.{level}].max_steps = 0 forbids every lookup, so the \
                     model would report on no evidence. Fix: use at least 1 (defaults \
                     6/20/48)."
                ));
            }
            if !(0.05..=1.0).contains(&b.context_fraction) {
                e.push(format!(
                    "[research.effort.{level}].context_fraction = {} is not a usable share of \
                     the context window. Fix: use 0.05-1.0 (defaults 0.5/0.7/0.85); it is a \
                     fraction of the window, not a token count.",
                    b.context_fraction
                ));
            }
            if b.search_top_k < 1 {
                e.push(format!(
                    "[research.effort.{level}].search_top_k = 0 makes every `search` tool call \
                     return nothing. Fix: use at least 1 (default {DEFAULT_RESEARCH_SEARCH_TOP_K})."
                ));
            }
            // The research loop builds its `SearchRequest` directly and so never
            // meets `validate::search_request`; this check is what keeps it inside
            // the same cap every API client is held to. Startup, not runtime,
            // because both sides are config — a client should not get a 400 for an
            // operator's mistake.
            if b.search_top_k > self.search.max_top_k {
                e.push(format!(
                    "[research.effort.{level}].search_top_k = {} exceeds [search].max_top_k = {}, \
                     so research would ask for more results than any other client may. Fix: lower \
                     it, or raise [search].max_top_k.",
                    b.search_top_k, self.search.max_top_k
                ));
            }
            // Zero is the sanctioned "say nothing about length" setting, so only a
            // small *non-zero* value is a mistake.
            if b.max_report_words > 0 && b.max_report_words < MIN_REPORT_WORDS {
                e.push(format!(
                    "[research.effort.{level}].max_report_words = {} cannot hold an answer to a \
                     research question, so the run would spend its whole budget investigating and \
                     then be told to write a stub. Fix: use at least {MIN_REPORT_WORDS} (defaults \
                     400/900/1800), or 0 to announce no length at all.",
                    b.max_report_words
                ));
            }
            // The report turn shares one window with the transcript it is written
            // from. Reserving more than half of it for generation means the evidence
            // cannot fit beside the report it is supposed to produce.
            let report_tokens = b.max_report_words as u64 * REPORT_WORDS_TO_TOKENS;
            if report_tokens >= self.research.max_num_ctx_tokens / 2 {
                e.push(format!(
                    "[research.effort.{level}].max_report_words = {} reserves about {} tokens for \
                     generation, which is at least half of [research].max_num_ctx_tokens = {} — \
                     the evidence would not fit in the window it has to share with the report. \
                     Fix: lower max_report_words, or raise max_num_ctx_tokens.",
                    b.max_report_words, report_tokens, self.research.max_num_ctx_tokens
                ));
            }
            // Below the sectioning threshold the grant names a report shape the
            // mechanism cannot produce: the sectioned path never engages under
            // MIN_REPORT_SECTIONS plan items, and the templated plan request
            // "3-N" would be malformed.
            if b.max_report_sections < MIN_REPORT_SECTIONS {
                e.push(format!(
                    "[research.effort.{level}].max_report_sections = {} is below the sectioning \
                     threshold, so the sectioned report could never engage. Fix: use at least \
                     {MIN_REPORT_SECTIONS} (default {DEFAULT_RESEARCH_MAX_REPORT_SECTIONS}).",
                    b.max_report_sections
                ));
            }
            if b.evidence_width < 1 {
                e.push(format!(
                    "[research.effort.{level}].evidence_width = 0 makes every evidence tool \
                     return nothing. Fix: use at least 1 (default \
                     {DEFAULT_RESEARCH_EVIDENCE_WIDTH}); it is a multiplier, not a row count."
                ));
            }
        }
        if self.research.retention_days < 1 {
            e.push(
                "[research].retention_days = 0 would let the first GC pass reap every run as \
                 soon as it is written, so no report could ever be reused as context. Fix: use \
                 at least 1 (default 90), and pin the runs that must outlive it."
                    .to_string(),
            );
        }
        if self.research.list_page_limit < 1 {
            e.push(
                "[research].list_page_limit = 0 makes every page of the research list empty. \
                 Fix: use at least 1 (default 50)."
                    .to_string(),
            );
        }
        // Zero runs is a legal way to switch the feature off; a zero *budget* for a
        // feature that is on is not, since the first report would be truncated to
        // nothing and the model would be handed an empty section it cannot use.
        if self.research.max_context_runs > 0
            && self.research.max_context_chars < MIN_RESEARCH_MAX_CONTEXT_CHARS
        {
            e.push(format!(
                "[research].max_context_chars = {} is too small to carry one useful report, but \
                 [research].max_context_runs = {} still offers the feature. Fix: raise it to at \
                 least {MIN_RESEARCH_MAX_CONTEXT_CHARS} (default {DEFAULT_RESEARCH_MAX_CONTEXT_CHARS}), \
                 or set max_context_runs = 0 to switch prior-research context off.",
                self.research.max_context_chars, self.research.max_context_runs
            ));
        }
        // The `search_top_k` trap again, one level up: an injected block is prompt
        // tokens on EVERY turn, so a cap larger than the window makes every run that
        // uses the feature die of context exhaustion before its first tool call — and
        // the edge validator never sees these two keys together. ~4 chars per token.
        let context_tokens = (self.research.max_context_chars as u64) / 4;
        if self.research.max_context_runs > 0 && context_tokens >= self.research.max_num_ctx_tokens
        {
            e.push(format!(
                "[research].max_context_chars = {} is about {context_tokens} tokens, which does not \
                 fit [research].max_num_ctx_tokens = {}; a run given prior context would exhaust its \
                 window before its first tool call. Fix: lower max_context_chars, or raise \
                 max_num_ctx_tokens.",
                self.research.max_context_chars, self.research.max_num_ctx_tokens
            ));
        }
        if let Some(t) = self.research.temperature
            && !(0.0..=2.0).contains(&t)
        {
            e.push(format!(
                "[research].temperature = {t} is outside the usable range. Fix: use 0.0-2.0 \
                 (omit the key to keep each model's own Modelfile default)."
            ));
        }
        if let Some(p) = self.research.top_p
            && !(0.0 < p && p <= 1.0)
        {
            e.push(format!(
                "[research].top_p = {p} is not a probability mass. Fix: use a value above 0 and \
                 at most 1 (omit the key to keep each model's own default)."
            ));
        }
        // The ceilings must not sit below what an effort level already grants, or an
        // `effort` a client cannot override would be unreachable through `budget`.
        for (key, ceiling, highest) in [
            (
                "max_request_seconds",
                self.research.max_request_seconds,
                self.research.effort.high.max_seconds,
            ),
            (
                "max_request_tokens",
                self.research.max_request_tokens,
                self.research.effort.high.max_tokens,
            ),
            (
                "max_request_steps",
                self.research.max_request_steps as u64,
                self.research.effort.high.max_steps as u64,
            ),
            (
                "max_request_report_sections",
                self.research.max_request_report_sections as u64,
                self.research.effort.high.max_report_sections as u64,
            ),
            (
                "max_request_report_words",
                self.research.max_request_report_words as u64,
                self.research.effort.high.max_report_words as u64,
            ),
            (
                "max_evidence_width",
                self.research.max_evidence_width,
                self.research.effort.high.evidence_width,
            ),
        ] {
            if ceiling < highest {
                e.push(format!(
                    "[research].{key} = {ceiling} is below [research.effort.high] ({highest}), so \
                     a request could not even ask for what `effort = \"high\"` already grants. \
                     Fix: raise it to at least {highest}."
                ));
            }
        }
        // The same window check the effort presets get, applied to the request
        // ceiling: it is what makes ANY value a request may pass safe to arm
        // `num_predict` with — there is no per-request window check.
        let ceiling_report_tokens =
            self.research.max_request_report_words as u64 * REPORT_WORDS_TO_TOKENS;
        if ceiling_report_tokens >= self.research.max_num_ctx_tokens / 2 {
            e.push(format!(
                "[research].max_request_report_words = {} would let a request reserve about \
                 {ceiling_report_tokens} tokens for generation, at least half of \
                 [research].max_num_ctx_tokens = {} — the evidence would not fit in the window \
                 it has to share with the report. Fix: lower max_request_report_words, or raise \
                 max_num_ctx_tokens.",
                self.research.max_request_report_words, self.research.max_num_ctx_tokens
            ));
        }
        // A ceiling below the floor would make every non-zero override invalid
        // while the field still reads as offered.
        if self.research.max_request_report_words < MIN_REPORT_WORDS {
            e.push(format!(
                "[research].max_request_report_words = {} is below the floor a non-zero \
                 override must clear ({MIN_REPORT_WORDS}), so every override but 0 would be \
                 rejected. Fix: use at least {MIN_REPORT_WORDS} (default \
                 {DEFAULT_RESEARCH_MAX_REQUEST_REPORT_WORDS}).",
                self.research.max_request_report_words
            ));
        }
        if self.research.max_num_ctx_tokens < 1024 {
            e.push(format!(
                "[research].max_num_ctx_tokens = {} cannot hold even one prompt + tool \
                 result. Fix: use at least 1024 (default 131072). Note this is a ceiling \
                 for VRAM, not the window: the model's own context length is requested \
                 when it is smaller.",
                self.research.max_num_ctx_tokens
            ));
        }
        if self.research.turn_timeout_ms < 1000 {
            e.push(format!(
                "[research].turn_timeout_ms = {} gives a thinking model no time to reply. \
                 Fix: use at least 1000 (default {DEFAULT_RESEARCH_TURN_TIMEOUT_MS}).",
                self.research.turn_timeout_ms
            ));
        }
        // The ordering that makes the deadline meaningful. A transport timeout below
        // the largest budget a request may ask for would preempt the deadline on
        // exactly the run the deadline exists for — a turn that never returns — and
        // turn "cut short, here is a report" into a failed run with an infrastructure
        // error code. Measured twice before it was written down.
        if self.research.turn_timeout_ms <= self.research.max_request_seconds * 1000 {
            e.push(format!(
                "[research].turn_timeout_ms = {} is not above [research].max_request_seconds \
                 = {} ({} ms), so a turn that never returns would fail the whole run as \
                 `ollama.unavailable` before the wall-clock deadline could cancel it and \
                 ship a report. The deadline is the run's bound; this is only a dead-socket \
                 guard. Fix: raise it above {} (default {DEFAULT_RESEARCH_TURN_TIMEOUT_MS}).",
                self.research.turn_timeout_ms,
                self.research.max_request_seconds,
                self.research.max_request_seconds * 1000,
                self.research.max_request_seconds * 1000,
            ));
        }
        // The silence guard must stay *under* the dead-socket one, or it is not a
        // guard at all: above it, the transport timeout always fires first and the
        // setting reads as enabled while never doing anything. A too-small value is
        // the other trap — it would abandon turns whose prompt evaluation is merely
        // long, so the floor is generous.
        if self.research.first_token_timeout_ms > 0 {
            if self.research.first_token_timeout_ms < 5000 {
                e.push(format!(
                    "[research].first_token_timeout_ms = {} would abandon a turn whose prompt \
                     evaluation is merely long — a big transcript is minutes of silence before \
                     the first token. Fix: use at least 5000 (default \
                     {DEFAULT_RESEARCH_FIRST_TOKEN_TIMEOUT_MS}), or 0 to disable it.",
                    self.research.first_token_timeout_ms
                ));
            }
            if self.research.first_token_timeout_ms >= self.research.turn_timeout_ms {
                e.push(format!(
                    "[research].first_token_timeout_ms = {} is not below \
                     [research].turn_timeout_ms = {}, so the whole-turn timeout would always \
                     fire first and the silence guard would never catch anything. Fix: lower it \
                     (default {DEFAULT_RESEARCH_FIRST_TOKEN_TIMEOUT_MS}), or 0 to disable it.",
                    self.research.first_token_timeout_ms, self.research.turn_timeout_ms,
                ));
            }
        }
        // A small non-zero value is the trap here, not a large one: the guard would
        // start abandoning turns that were only thinking, which is the same class of
        // mistake as a tight `turn_timeout_ms` and would look identical from outside
        // (runs that end early having found nothing). Disabling is spelled `0`.
        if self.research.checkpoint_every_steps > 0
            && self.research.checkpoint_every_steps < MIN_RESEARCH_CHECKPOINT_EVERY_STEPS
        {
            e.push(format!(
                "[research].checkpoint_every_steps = {} would spend most of the run writing \
                 instead of looking. Fix: use at least {MIN_RESEARCH_CHECKPOINT_EVERY_STEPS} \
                 (default {DEFAULT_RESEARCH_CHECKPOINT_EVERY_STEPS}), or 0 to switch \
                 checkpoints off.",
                self.research.checkpoint_every_steps
            ));
        }
        if self.research.max_turn_thinking_chars > 0
            && self.research.max_turn_thinking_chars < MIN_RESEARCH_MAX_TURN_THINKING_CHARS
        {
            e.push(format!(
                "[research].max_turn_thinking_chars = {} is below the volume an ordinary \
                 thinking turn produces (the busiest healthy turn measured on this host \
                 averaged 2642 characters), so the guard would abandon turns that were \
                 working. Fix: use at least {MIN_RESEARCH_MAX_TURN_THINKING_CHARS} \
                 (default {DEFAULT_RESEARCH_MAX_TURN_THINKING_CHARS}), or 0 to disable \
                 the guard.",
                self.research.max_turn_thinking_chars
            ));
        }
        // Both ends, because both make the log useless in opposite ways: a negative
        // (or NaN) threshold is nonsense a float lets you write, and a threshold
        // above what any local model reaches warns on every healthy turn — a warning
        // that fires always is a warning nobody reads.
        let slow = self.research.slow_turn_tokens_per_second;
        if slow < 0.0 || slow.is_nan() || slow >= MAX_RESEARCH_SLOW_TURN_TOKENS_PER_SECOND {
            e.push(format!(
                "[research].slow_turn_tokens_per_second = {slow} is not a usable rate: it \
                 must be at least 0 and below {MAX_RESEARCH_SLOW_TURN_TOKENS_PER_SECOND} \
                 tokens/s, or every turn of a healthy run would be logged as \
                 contention. Fix: set it just under the low end of what \
                 mindex_research_turn_tokens_per_second shows for your model, or 0 to \
                 disable the check (the default)."
            ));
        }
        if self.research.report_timeout_ms < MIN_RESEARCH_REPORT_TIMEOUT_MS {
            e.push(format!(
                "[research].report_timeout_ms = {} leaves no time to write the report, so \
                 every run that reaches its deadline would ship the server's \
                 cut-short notice instead of a real one. Fix: use at least \
                 {MIN_RESEARCH_REPORT_TIMEOUT_MS} (default \
                 {DEFAULT_RESEARCH_REPORT_TIMEOUT_MS}).",
                self.research.report_timeout_ms
            ));
        }
        if self.research.health_timeout_ms < 1 {
            e.push("[research].health_timeout_ms = 0 would time out instantly. Fix: use at least 1 (ms), e.g. 2000.".to_string());
        }
        if self.research.models_refresh_interval_seconds < 1 {
            e.push(format!(
                "[research].models_refresh_interval_seconds = 0 would re-read the Ollama \
                 model registry as fast as it answers. There is no \"0 = off\": disabling \
                 the refresh only takes the model list away from clients. Fix: use at \
                 least 1 (default {DEFAULT_RESEARCH_MODELS_REFRESH_SECONDS})."
            ));
        }

        if self.workers.gc_interval_seconds < 1 {
            e.push(
                "[workers].gc_interval_seconds = 0 is invalid. Fix: use at least 1 (default 3600)."
                    .to_string(),
            );
        }
        if self.workers.retry_interval_seconds < 1 {
            e.push("[workers].retry_interval_seconds = 0 is invalid. Fix: use at least 1 (default 60).".to_string());
        }
        if self.workers.failed_warn_interval_seconds < 1 {
            e.push("[workers].failed_warn_interval_seconds = 0 is invalid. Fix: use at least 1 (default 3600).".to_string());
        }
        if self.workers.status_log_retention_days < 1 {
            e.push("[workers].status_log_retention_days = 0 would prune the log immediately. Fix: use at least 1 (default 30).".to_string());
        }
        if self.workers.max_retries < 0 {
            e.push(format!(
                "[workers].max_retries = {} is negative. Fix: use 0 or more (default 3).",
                self.workers.max_retries
            ));
        }

        if self.metrics.refresh_interval_seconds < MIN_METRICS_REFRESH_SECONDS {
            e.push(format!(
                "[metrics].refresh_interval_seconds = {} is below {MIN_METRICS_REFRESH_SECONDS}; \
                 an aggregate over every project that often competes with the request path for a \
                 pool connection. Fix: use at least {MIN_METRICS_REFRESH_SECONDS} (default {DEFAULT_METRICS_REFRESH_SECONDS}).",
                self.metrics.refresh_interval_seconds
            ));
        }
        if self.metrics.refresh_interval_seconds > MAX_METRICS_REFRESH_SECONDS {
            e.push(format!(
                "[metrics].refresh_interval_seconds = {} is above {MAX_METRICS_REFRESH_SECONDS}; \
                 every scrape in between would report the same stale gauge. Fix: keep it at or \
                 below your scrape interval (default {DEFAULT_METRICS_REFRESH_SECONDS}).",
                self.metrics.refresh_interval_seconds
            ));
        }
        // A probe pass that eats a large share of the interval makes the collector
        // miss ticks, which shows up as gaps in the state gauges rather than as an
        // error. The worst case is the two probes running back to back.
        if self.metrics.probe_dependencies {
            let probe_budget_ms = self.model.health_timeout_ms + self.research.health_timeout_ms;
            let half_interval_ms = self.metrics.refresh_interval_seconds * 1000 / 2;
            if probe_budget_ms > half_interval_ms {
                e.push(format!(
                    "[metrics].probe_dependencies is on, but the health timeouts \
                     ([model].health_timeout_ms = {} + [research].health_timeout_ms = {} = {probe_budget_ms} ms) \
                     exceed half of [metrics].refresh_interval_seconds = {} ({half_interval_ms} ms), so a slow \
                     dependency would make the collector miss ticks. Fix: raise refresh_interval_seconds, \
                     lower the health timeouts, or set probe_dependencies = false.",
                    self.model.health_timeout_ms,
                    self.research.health_timeout_ms,
                    self.metrics.refresh_interval_seconds
                ));
            }
        }

        if e.is_empty() { Ok(()) } else { Err(e) }
    }

    /// Normalised, upper-case `synchronous` value for the SQLite PRAGMA (validation
    /// has already confirmed it is one of the allowed modes).
    pub fn sqlite_synchronous(&self) -> String {
        self.database.synchronous.to_uppercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(s)
    }

    /// The shipped example must parse *and* validate — `deny_unknown_fields` makes
    /// a stale key in it a hard error for anyone who copies it, and an example
    /// config that does not load is a documented lie. Also asserts it stays in step
    /// with the defaults, so a renamed key cannot be half-applied.
    #[test]
    fn the_example_config_parses_and_validates() {
        let text = include_str!("../config.example.toml");
        let cfg = parse(text).expect("config.example.toml must parse");
        cfg.validate().expect("config.example.toml must validate");
        let def = Config::default();
        assert_eq!(
            cfg.research.effort.medium.max_steps, def.research.effort.medium.max_steps,
            "the example's effort budgets drifted from the compiled defaults"
        );
        assert_eq!(
            cfg.research.effort.medium.max_tokens, def.research.effort.medium.max_tokens,
            "the example's token budgets drifted from the compiled defaults"
        );
        assert_eq!(
            cfg.research.max_request_tokens, def.research.max_request_tokens,
            "the example's request ceilings drifted from the compiled defaults"
        );
        assert_eq!(
            cfg.research.max_num_ctx_tokens, def.research.max_num_ctx_tokens,
            "the example's context ceiling drifted from the compiled default"
        );
        assert_eq!(
            cfg.research.effort.medium.search_top_k, def.research.effort.medium.search_top_k,
            "the example's search_top_k drifted from the compiled default"
        );
        // The sampling keys are shipped commented out on purpose: unset means each
        // model's own Modelfile default, and uncommenting them is an operator's
        // deliberate quality decision. An example that pinned them would silently
        // change every copier's runs.
        assert_eq!(cfg.research.temperature, None);
        assert_eq!(cfg.research.top_p, None);
        assert_eq!(cfg.research.seed, None);
    }

    /// Prior-research context is paid on every turn, so its cap and the context
    /// window are two halves of one setting — and nothing at the request edge ever
    /// sees them together. Startup is the only place that can catch the combination.
    #[test]
    fn a_context_block_that_cannot_fit_the_window_is_rejected() {
        let cfg = parse(
            "[research]\nmax_num_ctx_tokens = 4096\nmax_context_runs = 2\n\
             max_context_chars = 65536\n",
        )
        .expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter().any(|m| m.contains("max_context_chars")),
            "the error must name the key: {err:?}"
        );

        // Switching the feature off makes the same numbers harmless.
        //
        // The report budgets go with it, and that is the new rule working rather
        // than a fixture patched to be quiet: a 4096-token window genuinely cannot
        // hold the default 900-word report *and* the evidence it is written from, so
        // an operator who shrinks the window for VRAM is told to shrink this too —
        // including the request ceiling, which is held to the same window check so
        // that no *override* can reserve what the presets may not.
        let off = parse(
            "[research]\nmax_num_ctx_tokens = 4096\nmax_context_runs = 0\n\
             max_context_chars = 65536\nmax_request_report_words = 150\n\
             [research.effort.low]\nmax_report_words = 0\n\
             [research.effort.medium]\nmax_report_words = 0\n\
             [research.effort.high]\nmax_report_words = 0\n",
        )
        .expect("parses");
        off.validate()
            .expect("an unused context cap must not fail startup");
    }

    /// The report-shape and evidence-width knobs get the same startup guardrails
    /// as the budget axes: a ceiling below what `effort = "high"` already grants
    /// is a contradiction, and so is a preset the mechanism cannot honour.
    #[test]
    fn report_shape_ceilings_and_floors_are_enforced_at_startup() {
        // Ceiling below effort.high, for each new pair.
        for (key, toml) in [
            (
                "max_request_report_sections",
                // The default high preset (6) already exceeds a ceiling of 4.
                "[research]\nmax_request_report_sections = 4\n".to_string(),
            ),
            (
                "max_request_report_words",
                // 150 clears the non-zero floor, so the only complaint left is
                // the ceiling sitting under high's 1800.
                "[research]\nmax_request_report_words = 150\n".to_string(),
            ),
            (
                "max_evidence_width",
                "[research]\nmax_evidence_width = 1\n[research.effort.high]\nevidence_width = 2\n"
                    .to_string(),
            ),
        ] {
            let cfg = parse(&toml).expect("parses");
            let err = cfg.validate().expect_err("must be rejected");
            assert!(
                err.iter().any(|m| m.contains(key)),
                "{key} below effort.high must be named: {err:?}"
            );
        }
        // Preset floors: sections below the sectioning threshold, width of zero.
        let cfg = parse("[research.effort.low]\nmax_report_sections = 2\nevidence_width = 0\n")
            .expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter().any(|m| m.contains("max_report_sections")),
            "{err:?}"
        );
        assert!(err.iter().any(|m| m.contains("evidence_width")), "{err:?}");
        // A words ceiling below the non-zero floor would offer a field on which
        // every override but 0 is rejected.
        let cfg = parse("[research]\nmax_request_report_words = 100\n").expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter()
                .any(|m| m.contains("max_request_report_words") && m.contains("150")),
            "{err:?}"
        );
    }

    /// A window this small cannot hold the report *and* what the report is written
    /// from, and the two keys are set in different places by different people — an
    /// operator lowering `max_num_ctx_tokens` for VRAM has no reason to look at the
    /// effort ladder. Startup is where that collision is cheap to find.
    #[test]
    fn a_report_budget_that_cannot_share_the_window_is_refused() {
        let cfg = parse("[research]\nmax_num_ctx_tokens = 4096\n").expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter()
                .any(|m| m.contains("max_report_words") && m.contains("max_num_ctx_tokens")),
            "the error must name both keys: {err:?}"
        );
    }

    /// Below the floor the instruction stops shaping a report and starts forbidding
    /// one: the run spends its whole budget investigating and is then told to write
    /// a stub. `0` is the supported way to say "announce no length"; a tiny number
    /// is a mistake, and the message has to say which of the two the operator wants.
    #[test]
    fn a_report_word_budget_too_small_is_refused() {
        let cfg = parse("[research.effort.low]\nmax_report_words = 20\n").expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter()
                .any(|m| m.contains("max_report_words = 20") && m.contains("or 0 to announce")),
            "{err:?}"
        );
        // Zero is not a small number here, it is the off switch.
        let off = parse("[research.effort.low]\nmax_report_words = 0\n").expect("parses");
        off.validate().expect("0 switches the ceiling off");
    }

    /// A contention threshold is refused at both ends, and for the same reason each
    /// way: a warning that cannot fire and a warning that always fires are equally
    /// useless, and the second is worse because it buries the ones that matter.
    #[test]
    fn a_slow_turn_threshold_outside_the_usable_range_is_refused() {
        for spelling in ["-1.0", "1000.0", "5000.0"] {
            let cfg = parse(&format!(
                "[research]\nslow_turn_tokens_per_second = {spelling}\n"
            ))
            .expect("parses");
            let err = cfg.validate().expect_err("must be rejected");
            assert!(
                err.iter()
                    .any(|m| m.contains("slow_turn_tokens_per_second")),
                "{spelling}: {err:?}"
            );
        }
        // Zero is the off switch and the shipped default — a healthy rate is a fact
        // about one model on one host, so guessing one here would be worse than
        // measuring it.
        let off = parse("[research]\nslow_turn_tokens_per_second = 0\n").expect("parses");
        off.validate().expect("0 disables the check");
        assert_eq!(off.research.slow_turn_tokens_per_second, 0.0);
        let set = parse("[research]\nslow_turn_tokens_per_second = 8.5\n").expect("parses");
        set.validate().expect("a plausible rate is accepted");
    }

    /// The silence guard has to sit strictly between "long prompt evaluation" and
    /// "dead socket", and both walls are rejected at startup rather than discovered
    /// as runs that end early or a setting that reads as on and never fires.
    #[test]
    fn a_silence_guard_that_could_never_fire_is_rejected() {
        // At or above the dead-socket timeout the transport always wins.
        let cfg = parse("[research]\nfirst_token_timeout_ms = 3900000\n").expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter()
                .any(|m| m.contains("first_token_timeout_ms") && m.contains("turn_timeout_ms")),
            "{err:?}"
        );

        // Too small preempts a turn that was merely thinking about a long transcript.
        let cfg = parse("[research]\nfirst_token_timeout_ms = 500\n").expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(err.iter().any(|m| m.contains("at least 5000")), "{err:?}");

        // Off is a legal setting, for a host where a cold load is expected.
        let cfg = parse("[research]\nfirst_token_timeout_ms = 0\n").expect("parses");
        cfg.validate().expect("0 disables the guard");
    }

    /// Zero retention is not a tight setting, it is a corpus that cannot exist: the
    /// next GC pass reaps every run before anything can reference it.
    #[test]
    fn zero_research_retention_is_rejected() {
        let cfg = parse("[research]\nretention_days = 0\n").expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(err.iter().any(|m| m.contains("retention_days")), "{err:?}");
    }

    /// The research loop builds its `SearchRequest` directly and so never passes
    /// through `validate::search_request`; startup is the only place that can hold
    /// it to the same cap every other client obeys.
    #[test]
    fn research_search_top_k_may_not_exceed_the_search_cap() {
        let cfg = parse("[search]\nmax_top_k = 10\n\n[research.effort.high]\nsearch_top_k = 11\n")
            .expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter()
                .any(|m| m.contains("search_top_k = 11") && m.contains("[search].max_top_k")),
            "{err:?}"
        );
    }

    #[test]
    fn out_of_range_sampling_is_rejected() {
        let cfg = parse("[research]\ntemperature = 5.0\ntop_p = 0.0\n").expect("parses");
        let err = cfg.validate().expect_err("must be rejected");
        assert!(
            err.iter().any(|m| m.contains("[research].temperature")),
            "{err:?}"
        );
        assert!(
            err.iter().any(|m| m.contains("[research].top_p")),
            "{err:?}"
        );
    }

    #[test]
    fn empty_toml_yields_all_defaults() {
        let cfg = parse("").expect("empty TOML is valid");
        let def = Config::default();
        assert_eq!(
            cfg.indexing.embed_batch_chunks,
            def.indexing.embed_batch_chunks
        );
        assert_eq!(cfg.slicer.max_chunk_tokens, 512);
        assert_eq!(cfg.workers.gc_interval_seconds, 3600);
        assert!(cfg.metrics.enabled);
        assert_eq!(cfg.metrics.refresh_interval_seconds, 60);
        assert!(!cfg.metrics.per_project_http_labels);
        cfg.validate().expect("defaults are valid");
    }

    #[test]
    fn a_too_frequent_metrics_refresh_is_rejected() {
        let cfg = parse("[metrics]\nrefresh_interval_seconds = 0\n").expect("parses");
        let errs = cfg.validate().expect_err("0 is out of range");
        assert!(
            errs.iter()
                .any(|m| m.contains("[metrics].refresh_interval_seconds")),
            "expected the refresh-interval rule to fire, got: {errs:?}"
        );
    }

    /// The probe pass must fit comfortably inside a tick, or the collector
    /// silently misses ticks and the state gauges gap.
    #[test]
    fn probes_that_outlast_half_a_tick_are_rejected() {
        let cfg = parse(
            "[metrics]\nrefresh_interval_seconds = 5\nprobe_dependencies = true\n\
             [model]\nhealth_timeout_ms = 5000\n",
        )
        .expect("parses");
        let errs = cfg
            .validate()
            .expect_err("probe budget exceeds half a tick");
        assert!(
            errs.iter().any(|m| m.contains("probe_dependencies")),
            "expected the probe-budget rule to fire, got: {errs:?}"
        );
    }

    #[test]
    fn partial_toml_fills_missing_from_defaults() {
        let cfg = parse("[slicer]\nmin_chunk_tokens = 64\n").expect("valid");
        assert_eq!(cfg.slicer.min_chunk_tokens, 64);
        // Untouched key in the present section still defaults.
        assert_eq!(cfg.slicer.max_chunk_tokens, 512);
        // Absent section entirely defaults.
        assert_eq!(cfg.database.pool_size, 4);
    }

    #[test]
    fn unknown_key_is_rejected_with_its_name() {
        let err = parse("[indexing]\nembed_batch = 256\n").expect_err("typo must fail");
        assert!(err.to_string().contains("embed_batch"), "got: {err}");
    }

    #[test]
    fn cli_override_beats_file_and_default() {
        let mut cfg = parse("[indexing]\nembed_batch_chunks = 128\n").expect("valid");
        assert_eq!(cfg.indexing.embed_batch_chunks, 128); // file beats default (256)
        let cli = Cli {
            config: None,
            bind: None,
            cert_path: None,
            key_path: None,
            model: None,
            model_server: None,
            qdrant_server: None,
            db_path: None,
            db_pool_size: None,
            embed_batch: Some(512),
            max_body_mib: None,
            stuck_grace_mins: None,
            http3: false,
        };
        apply_cli_overrides(&mut cfg, &cli);
        assert_eq!(cfg.indexing.embed_batch_chunks, 512); // flag beats file
    }

    #[test]
    fn validation_collects_multiple_errors() {
        let mut cfg = Config::default();
        cfg.database.pool_size = 0;
        cfg.slicer.max_chunk_tokens = 9000;
        cfg.database.synchronous = "sometimes".into();
        cfg.qdrant.fusion_limit = 1; // below default_top_k=5
        let errs = cfg.validate().expect_err("should be invalid");
        assert!(
            errs.len() >= 4,
            "expected several errors, got {}: {errs:?}",
            errs.len()
        );
        assert!(
            errs.iter()
                .any(|m| m.contains("page") || m.contains("pool_size"))
        );
        assert!(errs.iter().any(|m| m.contains("max_chunk_tokens")));
        assert!(errs.iter().any(|m| m.contains("synchronous")));
        assert!(errs.iter().any(|m| m.contains("fusion_limit")));
    }

    /// Each broken glob gets its own collected error naming index and pattern —
    /// the collect-all contract, not fail-fast at the first one.
    #[test]
    fn every_invalid_allowed_models_glob_is_named_with_its_index() {
        let mut cfg = Config::default();
        cfg.research.allowed_models = vec!["gemma4:*".into(), "a[".into(), "b[".into()];
        let errs = cfg.validate().expect_err("should be invalid");
        assert!(
            errs.iter()
                .any(|m| m.contains("allowed_models[1]") && m.contains("a[")),
            "first broken glob must be named: {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|m| m.contains("allowed_models[2]") && m.contains("b[")),
            "second broken glob must be named: {errs:?}"
        );
    }

    /// A default model the whitelist does not cover would turn every defaulted
    /// request into a 400 — startup is the only place that sees both keys together.
    #[test]
    fn a_default_model_outside_the_whitelist_is_rejected_at_startup() {
        let mut cfg = Config::default();
        cfg.research.default_model = "qwen3.6".into();
        cfg.research.allowed_models = vec!["gemma4:*".into()];
        let errs = cfg.validate().expect_err("should be invalid");
        assert!(
            errs.iter()
                .any(|m| m.contains("default_model") && m.contains("allowed_models")),
            "the error must name both keys: {errs:?}"
        );

        // A covered default passes; so does any default with the whitelist off.
        cfg.research.allowed_models = vec!["gemma4:*".into(), "qwen3.6".into()];
        cfg.validate().expect("a covered default_model must pass");
        cfg.research.allowed_models = Vec::new();
        cfg.validate()
            .expect("an empty whitelist restricts nothing");
    }

    /// The matching semantics callers rely on: case-sensitive, tag included —
    /// `"gemma4:*"` covers tagged variants but not the bare name — and an empty
    /// list means unrestricted, not "nothing allowed".
    #[test]
    fn allowed_models_matches_case_sensitively_with_the_tag() {
        let allowed = AllowedModels::compile(&["gemma4:*".to_string()]).expect("valid globs");
        assert!(!allowed.is_unrestricted());
        assert!(allowed.allows("gemma4:27b"));
        assert!(!allowed.allows("gemma4"), "the tag separator is literal");
        assert!(!allowed.allows("Gemma4:27b"), "matching is case-sensitive");
        assert_eq!(allowed.patterns(), vec!["gemma4:*".to_string()]);

        let unrestricted = AllowedModels::compile(&[]).expect("empty list compiles");
        assert!(unrestricted.is_unrestricted());
        assert!(unrestricted.allows("anything-at-all"));
    }

    #[test]
    fn zero_encode_timeout_is_rejected() {
        let mut cfg = Config::default();
        cfg.model.encode_timeout_ms = 0;
        let errs = cfg.validate().expect_err("should be invalid");
        assert!(errs.iter().any(|m| m.contains("encode_timeout_ms")));
    }

    #[test]
    fn xdg_config_home_preferred_over_config_dirs() {
        // Resolution is by file existence; here we only assert the ordering of the
        // candidate list (XDG_CONFIG_HOME first, then XDG_CONFIG_DIRS).
        // SAFETY: single-threaded test; we restore nothing as the process is short-lived.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/home/u/.config");
            std::env::set_var("XDG_CONFIG_DIRS", "/etc/xdg:/usr/etc/xdg");
        }
        let paths = candidate_paths(None);
        assert_eq!(
            paths[0],
            PathBuf::from("/home/u/.config/mindex/config.toml")
        );
        assert_eq!(paths[1], PathBuf::from("/etc/xdg/mindex/config.toml"));
        assert_eq!(paths[2], PathBuf::from("/usr/etc/xdg/mindex/config.toml"));
    }

    #[test]
    fn explicit_path_wins_outright() {
        let paths = candidate_paths(Some(PathBuf::from("/tmp/my.toml")));
        assert_eq!(paths, vec![PathBuf::from("/tmp/my.toml")]);
    }
}
