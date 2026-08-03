//! Two-level configuration for `mindex-index`, mirroring the server: a TOML file
//! (XDG-resolved, `mindex/indexer.toml`) supplies base values, CLI flags override
//! them, both fall back to the built-in defaults here. Keys carry units
//! (`*_files`). Resolution + every override is reported on stderr so a config
//! mix-up is diagnosable from the run output (stdout stays clean for `--json`).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

const DEFAULT_SERVER_URL: &str = "https://127.0.0.1:11111";
const DEFAULT_PROTOCOL: &str = "v0";
const DEFAULT_BATCH_SIZE_FILES: usize = 100;
/// Two years. A window is needed at all because a commit's value decays while
/// its cost does not, and one bound alone is not enough: an age bound alone
/// indexes nothing on a repository idle for a year, a count bound alone reaches
/// back a fortnight on a repository having a furious month. Both apply, and the
/// stricter one binds.
const DEFAULT_HISTORY_MAX_AGE_DAYS: u64 = 730;
const DEFAULT_HISTORY_MAX_COMMITS: usize = 5_000;
/// "wip", "fix", "." — a message this short carries no information but still
/// occupies one of `file_history`'s slots on every lookup that touches it.
const DEFAULT_HISTORY_MIN_MESSAGE_BYTES: usize = 40;

/// File-backed settings (only the truly operational knobs; per-invocation flags
/// like `--project`/`--root`/`--check` are never in the file).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexerConfig {
    pub server_url: String,
    pub protocol: String,
    pub batch_size_files: usize,
    /// `None` → fall back to the CPU-count default at run time.
    pub concurrency: Option<usize>,
    pub no_verify: bool,
    /// PEM bundle to trust *in addition to* the OS store, for a server whose CA
    /// the host does not know. `None` = the OS store alone, which is right
    /// whenever the CA is installed system-wide (mkcert, a corporate root).
    pub ca_cert: Option<PathBuf>,
    /// Sent as `Authorization: Bearer` on every request, for a server running
    /// with `[auth].enabled`. `None` sends no header, which is what a server
    /// that authorizes nothing wants.
    ///
    /// It is the only credential a client sends. An `X-Api-Key` for a gateway in
    /// front of the server used to sit beside it, and requiring both is what made
    /// a token unusable on its own: the shared key had to travel with it. Issue a
    /// token with `mindex mint-token`.
    pub token: Option<String>,

    /// Reconcile the project's git history alongside the working tree.
    ///
    /// **Off by default**, deliberately: an existing deployment must behave
    /// byte-for-byte as it did until someone asks for the second channel.
    pub history: bool,
    /// Which refs bound the history walk. Overridden by `.mindex`'s `git_refs`
    /// and then by `--git-ref`; the fallback here is the current branch alone.
    pub git_refs: Vec<String>,
    /// Age bound on the walk. `None` = no age bound (the count bound still
    /// applies).
    pub history_max_age_days: Option<u64>,
    /// Count bound on the walk, applied together with the age bound.
    pub history_max_commits: usize,
    /// Commits whose whole message is shorter than this are not posted.
    pub history_min_message_bytes: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_SERVER_URL.to_string(),
            protocol: DEFAULT_PROTOCOL.to_string(),
            batch_size_files: DEFAULT_BATCH_SIZE_FILES,
            concurrency: None,
            no_verify: false,
            ca_cert: None,
            token: None,
            history: false,
            git_refs: vec!["HEAD".to_string()],
            history_max_age_days: Some(DEFAULT_HISTORY_MAX_AGE_DAYS),
            history_max_commits: DEFAULT_HISTORY_MAX_COMMITS,
            history_min_message_bytes: DEFAULT_HISTORY_MIN_MESSAGE_BYTES,
        }
    }
}

/// CLI overrides handed to [`resolve`]. `no_verify` is additive (a `--no-verify`
/// flag can only turn the setting on, since a bool flag cannot express "off").
pub struct Overrides {
    pub config: Option<PathBuf>,
    pub server: Option<String>,
    pub protocol: Option<String>,
    pub batch_size: Option<usize>,
    pub concurrency: Option<usize>,
    pub no_verify: bool,
    pub ca_cert: Option<PathBuf>,
    pub token: Option<String>,
    /// `Some(true)` from `--history`, `Some(false)` from `--no-history`, `None`
    /// when neither was passed. A plain bool cannot express "explicitly off",
    /// and the file default being `false` makes that distinction necessary.
    pub history: Option<bool>,
    pub history_since_days: Option<u64>,
    pub history_max_commits: Option<usize>,
}

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
        paths.push(home.join("mindex").join("indexer.toml"));
    }
    let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/etc/xdg".to_string());
    for dir in config_dirs.split(':').filter(|d| !d.is_empty()) {
        paths.push(PathBuf::from(dir).join("mindex").join("indexer.toml"));
    }
    paths
}

/// Load the indexer config file (if any), apply CLI overrides, validate, and
/// report it all on stderr. Returns the effective config or a fatal error.
pub fn resolve(ov: Overrides) -> Result<IndexerConfig> {
    let explicit = ov
        .config
        .clone()
        .or_else(|| std::env::var_os("MINDEX_INDEXER_CONFIG").map(PathBuf::from));
    let is_explicit = explicit.is_some();

    let mut chosen = None;
    for path in candidate_paths(explicit) {
        if path.is_file() {
            eprintln!("config: using {}", path.display());
            chosen = Some(path);
            break;
        }
        eprintln!("config: not found at {}", path.display());
    }

    let mut cfg = match chosen {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read config file {}", path.display()))?;
            toml::from_str(&text).with_context(|| {
                format!(
                    "cannot parse {} as TOML (unknown keys are rejected)",
                    path.display()
                )
            })?
        }
        None => {
            if is_explicit {
                anyhow::bail!(
                    "an explicit config path (--config / $MINDEX_INDEXER_CONFIG) was given but no \
                     file was found there; correct the path or drop the override"
                );
            }
            eprintln!("config: no file found; using built-in defaults");
            IndexerConfig::default()
        }
    };

    // Apply overrides, reporting each.
    if let Some(v) = ov.server {
        eprintln!("config: server_url overridden by --server ({v})");
        cfg.server_url = v;
    }
    if let Some(v) = ov.protocol {
        eprintln!("config: protocol overridden by --protocol ({v})");
        cfg.protocol = v;
    }
    if let Some(v) = ov.batch_size {
        eprintln!("config: batch_size_files overridden by --batch-size ({v})");
        cfg.batch_size_files = v;
    }
    if let Some(v) = ov.concurrency {
        eprintln!("config: concurrency overridden by --concurrency ({v})");
        cfg.concurrency = Some(v);
    }
    if ov.no_verify && !cfg.no_verify {
        eprintln!("config: no_verify enabled by --no-verify");
        cfg.no_verify = true;
    }
    if let Some(v) = ov.ca_cert {
        eprintln!("config: ca_cert overridden by --ca-cert ({})", v.display());
        cfg.ca_cert = Some(v);
    }
    // Never echoed, unlike every other override above: this one is a secret, and
    // the resolution report goes to stderr where it lands in logs and CI output.
    if let Some(v) = ov.token {
        eprintln!("config: token overridden by --token / $MINDEX_TOKEN (value hidden)");
        cfg.token = Some(v);
    }

    // Last resort, after every explicit source: the shared credentials file,
    // looked up by the *resolved* server URL — so it must run after the
    // `--server` override above, not before it.
    if cfg.token.is_none() {
        cfg.token = mindexfile::credentials::credentials_for(&cfg.server_url)?.token;
    }
    if let Some(v) = ov.history {
        eprintln!(
            "config: history overridden by --{} ({v})",
            if v { "history" } else { "no-history" }
        );
        cfg.history = v;
    }
    if let Some(v) = ov.history_since_days {
        eprintln!("config: history_max_age_days overridden by --history-since-days ({v})");
        cfg.history_max_age_days = Some(v);
    }
    if let Some(v) = ov.history_max_commits {
        eprintln!("config: history_max_commits overridden by --history-max-commits ({v})");
        cfg.history_max_commits = v;
    }

    // Validation: collect all problems, fail with the full list.
    let mut errs = Vec::new();
    // The token is fully resolved by here — flag, environment, file, credentials
    // — so this is the one place that sees what will actually be sent. A token
    // labelled for another kind of holder is refused rather than warned about:
    // the server does not check `aud`, so the request would work, and a warning
    // that is followed by success is a warning nobody reads twice.
    if let Some(t) = cfg.token.as_deref()
        && let Some(refusal) =
            mindexfile::token::audience_refusal(t, mindexfile::token::AUDIENCE_CLI)
    {
        errs.push(refusal);
    }
    if cfg.server_url.trim().is_empty() {
        errs.push("server_url is empty; set it in the config file or --server".to_string());
    }
    if cfg.protocol.trim().is_empty() {
        errs.push("protocol is empty; set it in the config file or --protocol".to_string());
    }
    if cfg.batch_size_files < 1 {
        errs.push("batch_size_files must be >= 1 (default 100)".to_string());
    }
    if let Some(c) = cfg.concurrency
        && c < 1
    {
        errs.push("concurrency must be >= 1".to_string());
    }
    if cfg.history_max_commits < 1 {
        errs.push("history_max_commits must be >= 1 (default 5000)".to_string());
    }
    if let Some(d) = cfg.history_max_age_days
        && d < 1
    {
        errs.push(
            "history_max_age_days must be >= 1; omit the key entirely for no age bound".to_string(),
        );
    }
    if cfg.history && cfg.git_refs.iter().all(|r| r.trim().is_empty()) {
        errs.push(
            "history is enabled but git_refs is empty; name at least one ref pattern (default \"HEAD\")"
                .to_string(),
        );
    }
    // Checked here rather than at connect time: a mistyped CA path would otherwise
    // surface as a TLS handshake failure, which reads as a server problem.
    if let Some(ref p) = cfg.ca_cert
        && !p.is_file()
    {
        errs.push(format!(
            "ca_cert points at {}, which is not a readable file",
            p.display()
        ));
    }
    if !errs.is_empty() {
        anyhow::bail!(
            "invalid indexer configuration:\n  • {}",
            errs.join("\n  • ")
        );
    }

    Ok(cfg)
}
