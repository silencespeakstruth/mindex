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
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_SERVER_URL.to_string(),
            protocol: DEFAULT_PROTOCOL.to_string(),
            batch_size_files: DEFAULT_BATCH_SIZE_FILES,
            concurrency: None,
            no_verify: false,
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
            toml::from_str(&text)
                .with_context(|| format!("cannot parse {} as TOML (unknown keys are rejected)", path.display()))?
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

    // Validation: collect all problems, fail with the full list.
    let mut errs = Vec::new();
    if cfg.server_url.trim().is_empty() {
        errs.push("server_url is empty; set it in the config file or --server".to_string());
    }
    if cfg.protocol.trim().is_empty() {
        errs.push("protocol is empty; set it in the config file or --protocol".to_string());
    }
    if cfg.batch_size_files < 1 {
        errs.push("batch_size_files must be >= 1 (default 100)".to_string());
    }
    if let Some(c) = cfg.concurrency {
        if c < 1 {
            errs.push("concurrency must be >= 1".to_string());
        }
    }
    if !errs.is_empty() {
        anyhow::bail!("invalid indexer configuration:\n  • {}", errs.join("\n  • "));
    }

    Ok(cfg)
}
