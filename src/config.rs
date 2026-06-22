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

const DEFAULT_QDRANT_SERVER: &str = "http://localhost:6334";
const DEFAULT_UPSERT_BATCH_POINTS: usize = 256;
const DEFAULT_DENSE_PREFETCH_LIMIT: u32 = 200;
const DEFAULT_SPARSE_PREFETCH_LIMIT: u32 = 200;
const DEFAULT_FUSION_LIMIT: u32 = 200;

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
/// BGE-M3 truncates at 512 input tokens; a larger chunk window would silently lose
/// the tail of every long chunk, so it is a hard validation ceiling.
const MODEL_MAX_TOKENS: usize = 512;

const DEFAULT_TOP_K: u64 = 5;

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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelConfig {
    pub name: String,
    pub server_url: Url,
    /// Liveness-ping timeout for the embedder's `/health`.
    pub health_timeout_ms: u64,
    /// 429-backoff retries before giving up on an `/encode` call.
    pub max_429_retries: u32,
    /// First 429 backoff; doubles each retry.
    pub backoff_base_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QdrantConfig {
    pub server_url: Url,
    pub upsert_batch_points: usize,
    pub dense_prefetch_limit: u32,
    pub sparse_prefetch_limit: u32,
    pub fusion_limit: u32,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SearchConfig {
    /// `top_k` used when a `/search` request omits it.
    pub default_top_k: u64,
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
    pub workers: WorkerConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind addr"),
            cert_path: PathBuf::from(DEFAULT_CERT_PATH),
            key_path: PathBuf::from(DEFAULT_KEY_PATH),
            max_body_mib: DEFAULT_MAX_BODY_MIB,
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_MODEL_NAME.to_string(),
            server_url: DEFAULT_MODEL_SERVER.parse().expect("valid default model url"),
            health_timeout_ms: DEFAULT_HEALTH_TIMEOUT_MS,
            max_429_retries: DEFAULT_MAX_429_RETRIES,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
        }
    }
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_QDRANT_SERVER.parse().expect("valid default qdrant url"),
            upsert_batch_points: DEFAULT_UPSERT_BATCH_POINTS,
            dense_prefetch_limit: DEFAULT_DENSE_PREFETCH_LIMIT,
            sparse_prefetch_limit: DEFAULT_SPARSE_PREFETCH_LIMIT,
            fusion_limit: DEFAULT_FUSION_LIMIT,
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
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self { default_top_k: DEFAULT_TOP_K }
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
    over!(cli.max_body_mib, cfg.server.max_body_mib, "server.max_body_mib");
    over!(cli.model, cfg.model.name, "model.name");
    over!(cli.model_server, cfg.model.server_url, "model.server_url");
    over!(cli.qdrant_server, cfg.qdrant.server_url, "qdrant.server_url");
    over!(cli.db_path, cfg.database.path, "database.path");
    over!(cli.db_pool_size, cfg.database.pool_size, "database.pool_size");
    over!(cli.embed_batch, cfg.indexing.embed_batch_chunks, "indexing.embed_batch_chunks");
    over!(cli.stuck_grace_mins, cfg.indexing.stuck_grace_minutes, "indexing.stuck_grace_minutes");
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
            e.push("[model].backoff_base_ms = 0 disables backoff. Fix: use at least 1 (ms), e.g. 200.".to_string());
        }
        if self.model.health_timeout_ms < 1 {
            e.push("[model].health_timeout_ms = 0 would time out instantly. Fix: use at least 1 (ms), e.g. 2000.".to_string());
        }

        if self.qdrant.upsert_batch_points < 1 {
            e.push("[qdrant].upsert_batch_points = 0 would never upsert. Fix: use at least 1 (e.g. 256).".to_string());
        }
        if self.qdrant.fusion_limit < self.search.default_top_k as u32 {
            e.push(format!(
                "[qdrant].fusion_limit = {} is below [search].default_top_k = {}; the reranker \
                 cannot return top_k results. Fix: set fusion_limit >= default_top_k.",
                self.qdrant.fusion_limit, self.search.default_top_k
            ));
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
            e.push("[database].pool_size = 0 leaves no connections. Fix: use at least 1 (e.g. 4).".to_string());
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
        if !(self.indexing.sparse_min_weight.is_finite() && self.indexing.sparse_min_weight >= 0.0) {
            e.push(format!(
                "[indexing].sparse_min_weight = {} must be a finite, non-negative threshold. \
                 Fix: use a small positive value (e.g. 0.00001).",
                self.indexing.sparse_min_weight
            ));
        }

        if self.slicer.min_chunk_tokens < 1 {
            e.push("[slicer].min_chunk_tokens = 0 is invalid. Fix: use at least 1 (default 128).".to_string());
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

        if self.search.default_top_k < 1 {
            e.push("[search].default_top_k = 0 returns nothing. Fix: use at least 1 (default 5).".to_string());
        }

        if self.workers.gc_interval_seconds < 1 {
            e.push("[workers].gc_interval_seconds = 0 is invalid. Fix: use at least 1 (default 3600).".to_string());
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

    #[test]
    fn empty_toml_yields_all_defaults() {
        let cfg = parse("").expect("empty TOML is valid");
        let def = Config::default();
        assert_eq!(cfg.indexing.embed_batch_chunks, def.indexing.embed_batch_chunks);
        assert_eq!(cfg.slicer.max_chunk_tokens, 512);
        assert_eq!(cfg.workers.gc_interval_seconds, 3600);
        cfg.validate().expect("defaults are valid");
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
        assert!(errs.len() >= 4, "expected several errors, got {}: {errs:?}", errs.len());
        assert!(errs.iter().any(|m| m.contains("page") || m.contains("pool_size")));
        assert!(errs.iter().any(|m| m.contains("max_chunk_tokens")));
        assert!(errs.iter().any(|m| m.contains("synchronous")));
        assert!(errs.iter().any(|m| m.contains("fusion_limit")));
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
        assert_eq!(paths[0], PathBuf::from("/home/u/.config/mindex/config.toml"));
        assert_eq!(paths[1], PathBuf::from("/etc/xdg/mindex/config.toml"));
        assert_eq!(paths[2], PathBuf::from("/usr/etc/xdg/mindex/config.toml"));
    }

    #[test]
    fn explicit_path_wins_outright() {
        let paths = candidate_paths(Some(PathBuf::from("/tmp/my.toml")));
        assert_eq!(paths, vec![PathBuf::from("/tmp/my.toml")]);
    }
}
