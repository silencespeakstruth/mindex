//! Two-level configuration for `mindex-watch`, mirroring the indexer: a TOML file
//! (XDG-resolved, `mindex/watcher.toml`) supplies base values, CLI flags override
//! them, both fall back to built-in defaults here. Resolution + every override is
//! reported on stderr.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

const DEFAULT_SERVER_URL: &str = "https://127.0.0.1:11111";
const DEFAULT_PROTOCOL: &str = "v0";
const DEFAULT_DEBOUNCE_MS: u64 = 1000;
const DEFAULT_DRIFT_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WatcherConfig {
    pub server_url: String,
    pub protocol: String,
    pub no_verify: bool,
    pub debounce_ms: u64,
    pub drift_interval_secs: u64,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_SERVER_URL.to_string(),
            protocol: DEFAULT_PROTOCOL.to_string(),
            no_verify: false,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            drift_interval_secs: DEFAULT_DRIFT_INTERVAL_SECS,
        }
    }
}

pub struct Overrides {
    pub config: Option<PathBuf>,
    pub server: Option<String>,
    pub protocol: Option<String>,
    pub no_verify: bool,
    pub debounce_ms: Option<u64>,
    pub drift_interval_secs: Option<u64>,
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
        paths.push(home.join("mindex").join("watcher.toml"));
    }
    let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/etc/xdg".to_string());
    for dir in config_dirs.split(':').filter(|d| !d.is_empty()) {
        paths.push(PathBuf::from(dir).join("mindex").join("watcher.toml"));
    }
    paths
}

pub fn resolve(ov: Overrides) -> Result<WatcherConfig> {
    let explicit = ov
        .config
        .clone()
        .or_else(|| std::env::var_os("MINDEX_WATCHER_CONFIG").map(PathBuf::from));
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
                    "an explicit config path (--config / $MINDEX_WATCHER_CONFIG) was given but \
                     no file was found there; correct the path or drop the override"
                );
            }
            eprintln!("config: no file found; using built-in defaults");
            WatcherConfig::default()
        }
    };

    if let Some(v) = ov.server {
        eprintln!("config: server_url overridden by --server ({v})");
        cfg.server_url = v;
    }
    if let Some(v) = ov.protocol {
        eprintln!("config: protocol overridden by --protocol ({v})");
        cfg.protocol = v;
    }
    if ov.no_verify && !cfg.no_verify {
        eprintln!("config: no_verify enabled by --no-verify");
        cfg.no_verify = true;
    }
    if let Some(v) = ov.debounce_ms {
        eprintln!("config: debounce_ms overridden by --debounce-ms ({v})");
        cfg.debounce_ms = v;
    }
    if let Some(v) = ov.drift_interval_secs {
        eprintln!("config: drift_interval_secs overridden by --drift-interval ({v})");
        cfg.drift_interval_secs = v;
    }

    let mut errs = Vec::new();
    if cfg.server_url.trim().is_empty() {
        errs.push("server_url is empty; set it in the config file or --server".to_string());
    }
    if cfg.protocol.trim().is_empty() {
        errs.push("protocol is empty; set it in the config file or --protocol".to_string());
    }
    if cfg.debounce_ms < 50 {
        errs.push("debounce_ms must be >= 50 (default 1000)".to_string());
    }
    if cfg.drift_interval_secs < 10 {
        errs.push("drift_interval_secs must be >= 10 (default 300)".to_string());
    }
    if !errs.is_empty() {
        anyhow::bail!("invalid watcher configuration:\n  • {}", errs.join("\n  • "));
    }

    Ok(cfg)
}
