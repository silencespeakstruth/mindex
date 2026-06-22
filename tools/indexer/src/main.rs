mod client;
mod config;
mod scanner;

use anyhow::{Context, Result};
use clap::Parser;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use sha2::{Digest, Sha256};

use client::{Code, DriftRequest, IndexRequest, IndexResponse, check_drift, upload_batch};
use scanner::{FileEntry, Language, ScanResult, scan};

// ─── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "mindex-index",
    version,
    about = "Walk a directory tree and index source files into a mindex server.",
    long_about = "\
Walk a directory tree, detect source-code files by extension, and stream them\n\
to a mindex server in batches. Files whose content has not changed since the\n\
last index run are skipped automatically (server-side hash check).\n\
\n\
With --concurrency > 1 the files are split across independent worker streams\n\
that upload in parallel, each shown as its own progress bar. While one stream\n\
waits on the server's GPU-bound embedding, the others keep its CPU-bound slicer\n\
busy — so the wall time drops toward the slowest single stream instead of the\n\
sum of all of them.\n\
\n\
Cancellation: press Ctrl+C at any time. In-flight batch requests are dropped\n\
immediately; the server cancels the corresponding work and returns HTTP 499."
)]
struct Cli {
    /// Path to a TOML config file. Overrides XDG discovery
    /// ($XDG_CONFIG_HOME/mindex/indexer.toml then $XDG_CONFIG_DIRS).
    #[arg(long)]
    config: Option<PathBuf>,

    /// mindex server URL (default: https://127.0.0.1:11111; or config server_url)
    #[arg(long)]
    server: Option<String>,

    /// Project GUID — 32-char hex without dashes (e.g. the output of: uuidgen | tr -d -)
    #[arg(long)]
    project: String,

    /// Root directory; all paths stored in mindex are relative to this
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Include glob (repeatable). Matched against the path relative to --root.
    /// If none given, every file with a recognised extension is included.
    /// Example: --include 'src/**/*.rs' --include 'tests/**/*.rs'
    #[arg(long = "include", value_name = "GLOB")]
    includes: Vec<String>,

    /// Exclude glob (repeatable). Evaluated before includes.
    /// Example: --exclude 'target/**' --exclude 'node_modules/**' --exclude '.git/**'
    #[arg(long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,

    /// Skip TLS certificate verification (required for the default self-signed cert)
    #[arg(long)]
    no_verify: bool,

    /// API protocol version embedded in the URL path (default: v0; or config protocol)
    #[arg(long)]
    protocol: Option<String>,

    /// Maximum number of files per upload batch (default: 100; or config batch_size_files)
    #[arg(long)]
    batch_size: Option<usize>,

    /// Number of parallel upload streams. Files are split evenly across this
    /// many workers, each uploading one batch at a time and drawn as its own
    /// progress bar. Parallelism overlaps the server's CPU-bound slicing of one
    /// stream with the GPU-bound embedding of another, so it speeds up indexing
    /// even though the embedder itself processes batches one at a time.
    ///
    /// Default: the machine's logical CPU count, capped at 4.
    ///
    /// Ceiling — keep this at or below the server's --db-pool-size (default 4):
    /// the connection pool does not block when exhausted, it errors, and it is
    /// shared with the server's background workers. Each stream holds at most
    /// one connection at a time, so streams ≤ pool size fit; setting it higher
    /// makes batches fail with PoolEmpty and get retried, which is slower, not
    /// faster. To go above 4, raise the server's --db-pool-size to match.
    #[arg(long, value_name = "N")]
    concurrency: Option<usize>,

    /// Print one line per file showing chunk count or "unchanged"
    #[arg(short, long)]
    verbose: bool,

    /// Drift check: walk + hash the tree, compare against the index, and report
    /// what diverged (stale / missing / orphaned / indexing) WITHOUT uploading.
    /// Exits non-zero if any actionable drift (stale/missing/orphaned) is found;
    /// the informational `indexing` bucket does not affect the exit code.
    #[arg(long)]
    check: bool,

    /// With --check, print the raw drift JSON instead of the human-readable report
    /// (for scripts / the MCP `drift` tool).
    #[arg(long)]
    json: bool,
}

fn default_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(1)
}

// ─── Shared progress state (atomics, read by the footer ticker) ─────────────────

#[derive(Default)]
struct Shared {
    files_done: AtomicU64,
    chunks: AtomicU64,
    errors: AtomicU64,
    active: AtomicUsize,
}

#[derive(Default)]
struct WorkerStats {
    new_chunks: u64,
    too_short: u64,
    errors: usize,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Two-level config: TOML file (XDG: mindex/indexer.toml) → CLI overrides → defaults.
    let cfg = config::resolve(config::Overrides {
        config: cli.config.clone(),
        server: cli.server.clone(),
        protocol: cli.protocol.clone(),
        batch_size: cli.batch_size,
        concurrency: cli.concurrency,
        no_verify: cli.no_verify,
    })?;
    let concurrency = cfg.concurrency.unwrap_or_else(default_concurrency).max(1);

    // Wire Ctrl+C to a CancellationToken so in-flight requests are dropped cleanly.
    let cancel = CancellationToken::new();
    {
        let c = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!();
            c.cancel();
        });
    }

    let root = cli
        .root
        .canonicalize()
        .with_context(|| format!("cannot access root: {}", cli.root.display()))?;

    // ── Header (table-aligned: labels padded to a common width) ───────────────
    eprintln!();
    let row = |label: &str, value: String| {
        eprintln!("  {}  {}", style(format!("{label:<7}")).dim(), style(value).cyan());
    };
    row("server", cfg.server_url.clone());
    row("project", cli.project.clone());
    row("root", root.display().to_string());
    row("threads", concurrency.to_string());
    eprintln!();

    // ── Scan ────────────────────────────────────────────────────────────────
    let spin = spinner("Scanning…");
    let scan = scan(&root, &cli.includes, &cli.excludes).context("file scan failed")?;
    spin.finish_and_clear();

    // An empty tree still matters to --check: everything indexed is then orphaned,
    // so only short-circuit on empty when we're actually about to index.
    if scan.files.is_empty() && !cli.check {
        eprintln!(
            "  {} No source files found.{}",
            style("—").dim(),
            if scan.skipped_unknown > 0 {
                format!("  ({} files with unrecognised extensions skipped)", scan.skipped_unknown)
            } else {
                String::new()
            }
        );
        eprintln!();
        return Ok(());
    }

    print_scan_summary(&scan);

    // ── HTTP client (shared by every worker) ──────────────────────────────────
    let http = Arc::new(
        reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(cfg.no_verify)
            .build()
            .context("failed to build HTTP client")?,
    );

    let total = scan.files.len();

    // ── Drift check: hash the tree, ask the server what diverged, report, exit.
    // No uploads, no warm-up (read-only against an existing project).
    if cli.check {
        let actionable =
            run_check(&http, &cfg.server_url, &cli.project, scan.files, &cancel, cli.json).await?;
        if cancel.is_cancelled() || actionable {
            std::process::exit(1);
        }
        return Ok(());
    }

    // ── Warm-up: create the project row + Qdrant collection once, before fan-out.
    // post_index ensures both before it looks at the file map, so an empty request
    // has no side effects beyond that — this removes the create-collection race
    // that concurrent first requests would otherwise hit.
    if !cancel.is_cancelled() {
        upload_batch(
            &http,
            &cfg.server_url,
            &cfg.protocol,
            &cli.project,
            IndexRequest { files: HashMap::new() },
            &cancel,
        )
        .await
        .context("warm-up request failed (server unreachable, bad project GUID, or TLS?)")?;
    }

    // ── Shard files round-robin across workers (even file counts) ──────────────
    let n_workers = concurrency.min(total).max(1);
    let mut shards: Vec<Vec<FileEntry>> = (0..n_workers).map(|_| Vec::new()).collect();
    for (i, f) in scan.files.into_iter().enumerate() {
        shards[i % n_workers].push(f);
    }

    // ── One unified progress bar for the whole job. Workers are homogeneous
    // (each just drains its shard), so per-worker bars are noise — a single
    // bar (green = done, red = remaining) plus a compact status message in it
    // carries everything that matters. Workers only bump the shared counters;
    // the ticker below turns those into the bar's position + message. ─────────
    let shared = Arc::new(Shared::default());
    let t0 = Instant::now();
    let bar = aggregate_bar(total as u64);

    let mut handles = Vec::with_capacity(n_workers);
    for shard in shards {
        let bar = bar.clone();
        let http = http.clone();
        let shared = shared.clone();
        let cancel = cancel.clone();
        let server = cfg.server_url.clone();
        let protocol = cfg.protocol.clone();
        let project = cli.project.clone();
        let batch_size = cfg.batch_size_files;
        let verbose = cli.verbose;
        handles.push(tokio::spawn(async move {
            run_worker(
                shard, bar, http, server, protocol, project, batch_size, verbose, cancel, shared,
            )
            .await
        }));
    }

    // ── Drive the bar from the shared counters. Position updates every tick;
    // the speed line is the cumulative average (total chunks / elapsed) rather
    // than a windowed rate, so it stays stable instead of collapsing to zero
    // during the prepare-heavy gaps between embed bursts. ETA uses the same
    // cumulative file rate. ────────────────────────────────────────────────────
    let total_files = total as u64;
    let tick_stop = CancellationToken::new();
    let ticker = {
        let bar = bar.clone();
        let shared = shared.clone();
        let stop = tick_stop.clone();
        tokio::spawn(async move {
            loop {
                let done = shared.files_done.load(Ordering::Relaxed);
                let chunks = shared.chunks.load(Ordering::Relaxed);
                let active = shared.active.load(Ordering::Relaxed);
                let errs = shared.errors.load(Ordering::Relaxed);

                bar.set_position(done);

                let elapsed = t0.elapsed().as_secs_f64();
                let chunks_per_s = if elapsed > 0.0 { chunks as f64 / elapsed } else { 0.0 };
                let files_per_s = if elapsed > 0.0 { done as f64 / elapsed } else { 0.0 };
                let remaining = total_files.saturating_sub(done);
                let eta = if files_per_s > 0.0 {
                    remaining as f64 / files_per_s
                } else {
                    f64::INFINITY
                };
                bar.set_message(format!(
                    "{chunks_per_s:.0} chunks/s · ETA {} · {chunks} chunks · {active} active{}",
                    fmt_eta(eta),
                    if errs > 0 { format!(" · {errs} err") } else { String::new() },
                ));

                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                }
            }
        })
    };

    // ── Join workers, sum stats ────────────────────────────────────────────────
    let mut totals = WorkerStats::default();
    for h in handles {
        if let Ok(s) = h.await {
            totals.new_chunks += s.new_chunks;
            totals.too_short += s.too_short;
            totals.errors += s.errors;
        }
    }
    tick_stop.cancel();
    let _ = ticker.await;
    bar.finish_and_clear();

    // ── Summary ──────────────────────────────────────────────────────────────
    print_summary(
        t0.elapsed(),
        total,
        totals.new_chunks,
        totals.too_short,
        totals.errors,
        cancel.is_cancelled(),
    );

    if cancel.is_cancelled() || totals.errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}

// ─── Worker ─────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // a worker just needs all the request inputs
async fn run_worker(
    shard: Vec<FileEntry>,
    bar: ProgressBar,
    http: Arc<reqwest::Client>,
    server: String,
    protocol: String,
    project: String,
    batch_size: usize,
    verbose: bool,
    cancel: CancellationToken,
    shared: Arc<Shared>,
) -> WorkerStats {
    shared.active.fetch_add(1, Ordering::Relaxed);
    let mut stats = WorkerStats::default();

    'batches: for batch in shard.chunks(batch_size.max(1)) {
        if cancel.is_cancelled() {
            break;
        }

        // ── Read files (skip binary / unreadable) ────────────────────────
        let mut req_files: HashMap<String, HashMap<String, Code>> = HashMap::new();
        let mut readable: u64 = 0;

        for f in batch {
            match tokio::fs::read_to_string(&f.abs_path).await {
                Ok(content) => {
                    req_files
                        .entry(f.language.name().to_string())
                        .or_default()
                        .insert(f.rel_path.clone(), Code { code: content });
                    readable += 1;
                }
                Err(err) => {
                    stats.errors += 1;
                    shared.errors.fetch_add(1, Ordering::Relaxed);
                    shared.files_done.fetch_add(1, Ordering::Relaxed);
                    if verbose {
                        bar.println(format!(
                            "  {} {}  {}",
                            style("✗").red(),
                            f.rel_path,
                            style(format!("unreadable: {err}")).red().dim(),
                        ));
                    }
                }
            }
        }

        if req_files.is_empty() {
            continue;
        }

        match upload_batch(
            &http,
            &server,
            &protocol,
            &project,
            IndexRequest { files: req_files },
            &cancel,
        )
        .await
        {
            Ok(resp) => {
                let (chunks, too_short) = tally_response(&resp);
                stats.new_chunks += chunks;
                stats.too_short += too_short;
                shared.chunks.fetch_add(chunks, Ordering::Relaxed);
                if verbose {
                    print_verbose(&bar, &resp);
                }
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    break 'batches;
                }
                stats.errors += readable as usize;
                shared.errors.fetch_add(readable, Ordering::Relaxed);
                bar.println(format!(
                    "  {} batch error: {}",
                    style("✗").red(),
                    style(e.to_string()).red().dim(),
                ));
            }
        }

        shared.files_done.fetch_add(readable, Ordering::Relaxed);
    }

    shared.active.fetch_sub(1, Ordering::Relaxed);
    stats
}

// ─── Drift check ──────────────────────────────────────────────────────────────

/// Hash every scanned file (the SAME bytes `upload_batch` would send, so the digest
/// matches the server's `sha256`), POST the manifest to `/drift`, and report the
/// divergence. Returns `true` if there is **actionable** drift (stale/missing/orphaned).
async fn run_check(
    http: &reqwest::Client,
    server: &str,
    project: &str,
    files: Vec<FileEntry>,
    cancel: &CancellationToken,
    json: bool,
) -> Result<bool> {
    let spin = spinner("Hashing…");
    let mut manifest: HashMap<String, String> = HashMap::new();
    for f in &files {
        if cancel.is_cancelled() {
            break;
        }
        match tokio::fs::read_to_string(&f.abs_path).await {
            // Hash the exact uploaded bytes: server hashes `code.as_bytes()`.
            Ok(content) => {
                let mut hasher = Sha256::new();
                hasher.update(content.as_bytes());
                manifest.insert(f.rel_path.clone(), hex::encode(hasher.finalize()));
            }
            Err(err) => {
                // Unreadable/binary files are simply not part of the index, so omit
                // them from the manifest (they'd otherwise look "missing" forever).
                eprintln!(
                    "  {} {}  {}",
                    style("✗").red(),
                    f.rel_path,
                    style(format!("unreadable: {err}")).red().dim(),
                );
            }
        }
    }
    spin.finish_and_clear();

    let drift = check_drift(http, server, project, DriftRequest { files: manifest }, cancel)
        .await
        .context("drift check request failed (server unreachable, bad project GUID, or TLS?)")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "stale": drift.stale,
            "missing": drift.missing,
            "orphaned": drift.orphaned,
            "indexing": drift.indexing,
        }))?);
    } else {
        print_drift(&drift);
    }

    Ok(!drift.stale.is_empty() || !drift.missing.is_empty() || !drift.orphaned.is_empty())
}

/// Human-readable drift report. Each non-empty bucket is a labelled, sorted block;
/// a fully in-sync tree prints a single "in sync" line.
fn print_drift(d: &client::DriftResponse) {
    let block = |label: &str, color: console::Color, paths: &[String]| {
        if paths.is_empty() {
            return;
        }
        eprintln!("  {}", style(format!("{label} ({})", paths.len())).fg(color).bold());
        for p in paths {
            eprintln!("    {}", style(p).fg(color).dim());
        }
    };

    block("STALE", console::Color::Yellow, &d.stale);
    block("MISSING", console::Color::Red, &d.missing);
    block("ORPHANED", console::Color::Magenta, &d.orphaned);
    block("INDEXING", console::Color::Cyan, &d.indexing);

    if d.stale.is_empty() && d.missing.is_empty() && d.orphaned.is_empty() {
        let note = if d.indexing.is_empty() {
            String::new()
        } else {
            format!("  ({} file(s) currently indexing)", d.indexing.len())
        };
        eprintln!("  {} index in sync{}", style("✓").green(), style(note).dim());
    } else {
        eprintln!();
        eprintln!(
            "  {} index out of sync — run mindex-index (or delete orphaned paths) to refresh",
            style("⚠").yellow(),
        );
    }
    eprintln!();
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn aggregate_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    // Full terminal width via {wide_bar}; the file count rides at the end of the
    // bar line, the live speed/ETA on a second line.
    pb.set_style(
        ProgressStyle::with_template("  [{wide_bar:.green/red}] {pos}/{len} files\n  {msg}")
            .unwrap()
            .progress_chars("█░"),
    );
    pb.set_message("starting…");
    pb
}

/// Formats a seconds estimate as `m:ss` (or `h:mm:ss`); `—` when unknown.
fn fmt_eta(secs: f64) -> String {
    if !secs.is_finite() || secs > 359_999.0 {
        return "—".to_string();
    }
    let s = secs.round() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

fn print_scan_summary(scan: &ScanResult) {
    let mut by_lang: HashMap<Language, usize> = HashMap::new();
    for f in &scan.files {
        *by_lang.entry(f.language).or_default() += 1;
    }
    let mut counts: Vec<(Language, usize)> = by_lang.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name().cmp(b.0.name())));

    eprintln!("  {} files total:", style(scan.files.len()).bold());
    for (lang, n) in &counts {
        eprintln!("\t{}: {}", style(lang.name()).cyan().dim(), n);
    }
    eprintln!();
}

/// Returns (new_chunks, files_with_zero_chunks).
fn tally_response(resp: &IndexResponse) -> (u64, u64) {
    let mut new_chunks = 0u64;
    let mut too_short = 0u64;
    for paths in resp.files.values() {
        for &count in paths.values() {
            if count == 0 {
                too_short += 1;
            } else {
                new_chunks += count;
            }
        }
    }
    (new_chunks, too_short)
}

fn print_verbose(pb: &ProgressBar, resp: &IndexResponse) {
    let mut lines: Vec<(String, u64)> = resp
        .files
        .values()
        .flat_map(|paths| paths.iter().map(|(p, &c)| (p.clone(), c)))
        .collect();
    lines.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, count) in lines {
        if count == 0 {
            pb.println(format!(
                "  {} {}  {}",
                style("⊘").dim(),
                style(&path).dim(),
                style("0 chunks (too short)").dim(),
            ));
        } else {
            pb.println(format!(
                "  {} {}  {}",
                style("✓").green(),
                path,
                style(format!("{count} chunk{}", if count == 1 { "" } else { "s" })).green(),
            ));
        }
    }
}

fn print_summary(
    elapsed: Duration,
    total: usize,
    new_chunks: u64,
    n_no_chunks: u64,
    n_errors: usize,
    cancelled: bool,
) {
    let secs = elapsed.as_secs_f64();

    if cancelled {
        eprintln!(
            "  {} Cancelled after {secs:.1}s — {total} files queued · {} new chunks · {} too short · {} errors",
            style("⚠").yellow(),
            style(new_chunks).green(),
            n_no_chunks,
            style(n_errors).red(),
        );
    } else if n_errors > 0 {
        eprintln!(
            "  {} {secs:.1}s · {} files · {} new chunks · {} too short · {} errors",
            style("⚠").yellow(),
            style(total).bold(),
            style(new_chunks).green(),
            n_no_chunks,
            style(n_errors).red(),
        );
    } else {
        eprintln!(
            "  {} {secs:.1}s · {} files · {} new chunks · {} too short",
            style("✓").green(),
            style(total).bold(),
            style(new_chunks).green(),
            n_no_chunks,
        );
    }
    eprintln!();
}
