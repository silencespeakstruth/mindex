mod client;
mod config;
mod mindex_file;
mod scanner;
mod watcher;

use anyhow::{Context, Result};
use clap::Parser;
use client::{Code, DriftResponse};
use globset::GlobSet;
use notify::Watcher;
use scanner::detect_language;
use sha2::Digest as _;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use watcher::{PendingEvent, classify, convert_event};

// ─── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "mindex-watch",
    version,
    about = "Watch a project directory for file changes and keep the mindex index in sync.",
    long_about = "\
Watch a project directory for filesystem events (inotify on Linux), debounce\n\
changes, and automatically reindex modified files or remove deleted files from\n\
the mindex vector index. Also runs periodic drift checks to catch changes that\n\
happened while the watcher was offline.\n\
\n\
The project root must contain a .mindex marker file with the project GUID on\n\
the first non-comment line. Optional include_paths, exclude_paths, and languages\n\
scope lines in .mindex narrow which files are watched.\n\
\n\
Use --dry-run to log all planned actions without making any mutating HTTP calls\n\
(the drift check is still performed, as it is read-only)."
)]
struct Cli {
    /// Project root directory containing the .mindex file [default: .]
    #[arg(long, default_value = ".")]
    pwd: PathBuf,

    /// Path to a TOML config file; overrides XDG discovery
    /// ($XDG_CONFIG_HOME/mindex/watcher.toml then $XDG_CONFIG_DIRS).
    #[arg(long)]
    config: Option<PathBuf>,

    /// mindex server URL (default: https://127.0.0.1:11111; or config server_url)
    #[arg(long)]
    server: Option<String>,

    /// API protocol version in the URL path (default: v0; or config protocol)
    #[arg(long)]
    protocol: Option<String>,

    /// Skip TLS certificate verification (required for the default self-signed cert)
    #[arg(long)]
    no_verify: bool,

    /// Milliseconds to accumulate filesystem events before flushing a batch [default: 1000]
    #[arg(long, value_name = "MS")]
    debounce_ms: Option<u64>,

    /// Seconds between full drift checks [default: 300]
    #[arg(long, value_name = "SECS")]
    drift_interval: Option<u64>,

    /// Log all planned actions without making any mutating HTTP calls
    #[arg(long)]
    dry_run: bool,
}

// ─── Runtime config ───────────────────────────────────────────────────────────

struct Cfg {
    guid: String,
    server_url: String,
    protocol: String,
    debounce: Duration,
    drift_interval: Duration,
    dry_run: bool,
    root: PathBuf,
    include_set: Option<GlobSet>,
    exclude_set: Option<GlobSet>,
    /// Lowercase mindex language ids; empty = all languages.
    languages: Vec<String>,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let wcfg = config::resolve(config::Overrides {
        config: cli.config,
        server: cli.server,
        protocol: cli.protocol,
        no_verify: cli.no_verify,
        debounce_ms: cli.debounce_ms,
        drift_interval_secs: cli.drift_interval,
    })?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let root = cli
        .pwd
        .canonicalize()
        .with_context(|| format!("cannot access --pwd: {}", cli.pwd.display()))?;

    let mf = mindex_file::parse(&root.join(".mindex"))?;
    let (include_set, exclude_set) =
        mindex_file::build_globsets(&mf.include_paths, &mf.exclude_paths)?;

    let http = Arc::new(
        reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(wcfg.no_verify)
            .build()
            .context("failed to build HTTP client")?,
    );

    let cfg = Arc::new(Cfg {
        guid: mf.guid.clone(),
        server_url: wcfg.server_url.clone(),
        protocol: wcfg.protocol.clone(),
        debounce: Duration::from_millis(wcfg.debounce_ms),
        drift_interval: Duration::from_secs(wcfg.drift_interval_secs),
        dry_run: cli.dry_run,
        root: root.clone(),
        include_set,
        exclude_set,
        languages: mf.languages.clone(),
    });

    info!(
        server = %cfg.server_url,
        project = %cfg.guid,
        root = %root.display(),
        debounce_ms = wcfg.debounce_ms,
        drift_interval_secs = wcfg.drift_interval_secs,
        dry_run = cfg.dry_run,
        "mindex-watch starting",
    );

    // Cancellation token wired to Ctrl+C and SIGTERM.
    let cancel = CancellationToken::new();
    {
        let c = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("received Ctrl+C, shutting down");
            c.cancel();
        });
    }
    {
        let c = cancel.clone();
        tokio::spawn(async move {
            let mut sigterm =
                signal(SignalKind::terminate()).expect("SIGTERM handler registration failed");
            sigterm.recv().await;
            info!("received SIGTERM, shutting down");
            c.cancel();
        });
    }

    // Startup drift: catches changes that happened while the watcher was offline.
    info!("running startup drift check");
    run_drift(&cfg, &http, &cancel).await;
    if cancel.is_cancelled() {
        return Ok(());
    }

    // inotify watcher — the callback runs in a background thread, so we bridge
    // raw events to an async channel and classify them in the debounce task.
    let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut fs_watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                for raw in convert_event(event) {
                    let _ = raw_tx.send(raw);
                }
            }
            Err(e) => tracing::warn!(error = %e, "inotify error"),
        })
        .context("failed to create inotify watcher")?;

    fs_watcher
        .watch(&root, notify::RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", root.display()))?;

    info!(path = %root.display(), "watching for changes");

    let debounce_handle = {
        let cfg = cfg.clone();
        let http = http.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { debounce_task(raw_rx, cfg, http, cancel).await })
    };

    let drift_handle = {
        let cfg = cfg.clone();
        let http = http.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { drift_task(cfg, http, cancel).await })
    };

    cancel.cancelled().await;

    // Drop the watcher to close the inotify fd and let the sender side of the
    // channel drop, which causes debounce_task's rx.recv() to return None.
    drop(fs_watcher);

    let _ = debounce_handle.await;
    let _ = drift_handle.await;

    info!("mindex-watch stopped");
    Ok(())
}

// ─── Debounce task ────────────────────────────────────────────────────────────

async fn debounce_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<watcher::RawEvent>,
    cfg: Arc<Cfg>,
    http: Arc<reqwest::Client>,
    cancel: CancellationToken,
) {
    // Pending events keyed by relative path: HashMap::insert is last-event-wins,
    // which is exactly the deduplication semantics we want.
    let mut buf: HashMap<String, PendingEvent> = HashMap::new();
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        // Use a sentinel far in the future when there's no pending deadline so
        // the sleep arm never fires spuriously on an empty buffer.
        let far = tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600);
        let sleep_until = deadline.unwrap_or(far);

        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            raw = rx.recv() => {
                let Some(raw) = raw else { break };
                if let Some(pending) = classify(
                    raw,
                    &cfg.root,
                    &cfg.include_set,
                    &cfg.exclude_set,
                    &cfg.languages,
                ) {
                    let key = pending.rel().to_string();
                    // Set the deadline on the first event in a quiet window; do
                    // not slide it forward — this bounds the worst-case latency.
                    if deadline.is_none() {
                        deadline = Some(tokio::time::Instant::now() + cfg.debounce);
                    }
                    buf.insert(key, pending);
                }
            }
            _ = tokio::time::sleep_until(sleep_until) => {
                if !buf.is_empty() {
                    let events: Vec<PendingEvent> = buf.drain().map(|(_, v)| v).collect();
                    flush(events, &cfg, &http, &cancel).await;
                }
                deadline = None;
            }
        }
    }
}

// ─── Drift task ───────────────────────────────────────────────────────────────

async fn drift_task(cfg: Arc<Cfg>, http: Arc<reqwest::Client>, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(cfg.drift_interval);
    interval.tick().await; // discard the immediate first tick (startup drift already ran)

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                info!("running periodic drift check");
                run_drift(&cfg, &http, &cancel).await;
            }
        }
    }
}

// ─── Flush (debounce → API) ──────────────────────────────────────────────────

async fn flush(
    events: Vec<PendingEvent>,
    cfg: &Cfg,
    http: &reqwest::Client,
    cancel: &CancellationToken,
) {
    let mut upserts: HashMap<String, HashMap<String, Code>> = HashMap::new();
    let mut deletes: Vec<String> = Vec::new();

    for event in events {
        if cancel.is_cancelled() {
            return;
        }
        match event {
            PendingEvent::Delete { rel } => deletes.push(rel),
            PendingEvent::Upsert { rel, lang } => {
                let abs = cfg.root.join(&rel);
                match tokio::fs::read_to_string(&abs).await {
                    Ok(code) => {
                        upserts
                            .entry(lang.name().to_string())
                            .or_default()
                            .insert(rel, Code { code });
                    }
                    Err(e) => warn!(file = %rel, error = %e, "cannot read file, skipping"),
                }
            }
        }
    }

    let n_upsert: usize = upserts.values().map(|m| m.len()).sum();

    if cfg.dry_run {
        if n_upsert > 0 {
            let paths: Vec<&str> =
                upserts.values().flat_map(|m| m.keys().map(String::as_str)).collect();
            info!(files = n_upsert, ?paths, "[DRY-RUN] would index");
        }
        if !deletes.is_empty() {
            info!(files = deletes.len(), paths = ?deletes, "[DRY-RUN] would delete");
        }
        return;
    }

    if n_upsert > 0 {
        match client::index_batch(
            http,
            &cfg.server_url,
            &cfg.protocol,
            &cfg.guid,
            upserts,
            cancel,
        )
        .await
        {
            Ok(()) => info!(files = n_upsert, "indexed"),
            Err(e) => warn!(error = %e, "index_batch failed; drift will correct on next check"),
        }
    }

    if !deletes.is_empty() {
        let n = deletes.len();
        match client::delete_files(http, &cfg.server_url, &cfg.guid, deletes, cancel).await {
            Ok(confirmed) => info!(requested = n, confirmed, "deleted from index"),
            Err(e) => warn!(error = %e, "delete_files failed; drift will correct on next check"),
        }
    }
}

// ─── Drift check ──────────────────────────────────────────────────────────────

async fn run_drift(cfg: &Cfg, http: &reqwest::Client, cancel: &CancellationToken) {
    let manifest = build_manifest(cfg, cancel).await;
    if manifest.is_empty() {
        tracing::debug!("drift: no tracked files found under scope");
        return;
    }

    info!(files = manifest.len(), "drift: querying server");

    let drift = match client::check_drift(http, &cfg.server_url, &cfg.guid, manifest, cancel).await
    {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "drift check request failed (server unreachable?)");
            return;
        }
    };

    log_drift_summary(&drift);

    if cfg.dry_run {
        let n_reindex = drift.stale.len() + drift.missing.len();
        if n_reindex > 0 {
            info!(
                n = n_reindex,
                stale = drift.stale.len(),
                missing = drift.missing.len(),
                "[DRY-RUN] would reindex",
            );
        }
        if !drift.orphaned.is_empty() {
            info!(n = drift.orphaned.len(), "[DRY-RUN] would delete orphaned");
        }
        return;
    }

    let to_reindex: Vec<String> = drift.stale.into_iter().chain(drift.missing).collect();
    if !to_reindex.is_empty() {
        reindex_files(&to_reindex, cfg, http, cancel).await;
    }

    if !drift.orphaned.is_empty() {
        let n = drift.orphaned.len();
        match client::delete_files(http, &cfg.server_url, &cfg.guid, drift.orphaned, cancel).await
        {
            Ok(confirmed) => info!(requested = n, confirmed, "drift: deleted orphaned files"),
            Err(e) => warn!(error = %e, "drift: delete orphaned files failed"),
        }
    }
}

async fn build_manifest(cfg: &Cfg, cancel: &CancellationToken) -> HashMap<String, String> {
    let mut manifest = HashMap::new();

    for entry in walkdir::WalkDir::new(&cfg.root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        if cancel.is_cancelled() {
            break;
        }

        let abs = entry.into_path();
        let Ok(rel_raw) = abs.strip_prefix(&cfg.root) else { continue };
        let rel = rel_raw.to_string_lossy().replace('\\', "/");
        let rel_path = Path::new(rel.as_str());

        if let Some(ref excl) = cfg.exclude_set {
            if excl.is_match(rel_path) {
                continue;
            }
        }
        if let Some(ref incl) = cfg.include_set {
            if !incl.is_match(rel_path) {
                continue;
            }
        }

        let Some(lang) = detect_language(&abs) else { continue };
        if !cfg.languages.is_empty() && !cfg.languages.iter().any(|l| l == lang.name()) {
            continue;
        }

        if let Ok(content) = tokio::fs::read_to_string(&abs).await {
            // binary or unreadable files are omitted from the manifest — not indexable
            let digest = sha2::Sha256::digest(content.as_bytes());
            manifest.insert(rel, hex::encode(digest));
        }
    }

    manifest
}

async fn reindex_files(
    paths: &[String],
    cfg: &Cfg,
    http: &reqwest::Client,
    cancel: &CancellationToken,
) {
    let mut upserts: HashMap<String, HashMap<String, Code>> = HashMap::new();

    for rel in paths {
        if cancel.is_cancelled() {
            return;
        }
        let abs = cfg.root.join(rel);
        let Some(lang) = detect_language(&abs) else { continue };
        if !cfg.languages.is_empty() && !cfg.languages.iter().any(|l| l == lang.name()) {
            continue;
        }
        match tokio::fs::read_to_string(&abs).await {
            Ok(code) => {
                upserts
                    .entry(lang.name().to_string())
                    .or_default()
                    .insert(rel.clone(), Code { code });
            }
            Err(e) => warn!(file = %rel, error = %e, "drift: cannot read file for reindex"),
        }
    }

    if upserts.is_empty() {
        return;
    }
    let n: usize = upserts.values().map(|m| m.len()).sum();
    match client::index_batch(http, &cfg.server_url, &cfg.protocol, &cfg.guid, upserts, cancel)
        .await
    {
        Ok(()) => info!(files = n, "drift: reindexed"),
        Err(e) => warn!(error = %e, "drift: reindex failed"),
    }
}

fn log_drift_summary(drift: &DriftResponse) {
    if !drift.indexing.is_empty() {
        info!(files = drift.indexing.len(), "drift: files currently indexing (no action)");
    }
    if drift.stale.is_empty() && drift.missing.is_empty() && drift.orphaned.is_empty() {
        info!("drift: index in sync");
    } else {
        info!(
            stale = drift.stale.len(),
            missing = drift.missing.len(),
            orphaned = drift.orphaned.len(),
            "drift: divergence found",
        );
    }
}
