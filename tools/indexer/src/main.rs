mod client;
mod scanner;

use anyhow::{Context, Result};
use clap::Parser;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use client::{Code, IndexRequest, IndexResponse, upload_batch};
use scanner::{Language, ScanResult, scan};

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
Cancellation: press Ctrl+C at any time. The in-flight batch request is dropped\n\
immediately; the server cancels the corresponding work and returns HTTP 499."
)]
struct Cli {
    /// mindex server URL
    #[arg(long, default_value = "https://127.0.0.1:11111")]
    server: String,

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

    /// API protocol version embedded in the URL path
    #[arg(long, default_value = "v0")]
    protocol: String,

    /// Maximum number of files per upload batch (one HTTP request per batch)
    #[arg(long, default_value_t = 100)]
    batch_size: usize,

    /// Print one line per file showing chunk count or "unchanged"
    #[arg(short, long)]
    verbose: bool,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Wire Ctrl+C to a CancellationToken so in-flight requests are dropped cleanly.
    let cancel = CancellationToken::new();
    {
        let c = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            // Print a blank line so the progress bar is not overwritten by ^C.
            eprintln!();
            c.cancel();
        });
    }

    let root = cli
        .root
        .canonicalize()
        .with_context(|| format!("cannot access root: {}", cli.root.display()))?;

    // ── Header ──────────────────────────────────────────────────────────────
    eprintln!();
    eprintln!("  {}    {}", style("server").dim(), style(&cli.server).cyan());
    eprintln!("  {}   {}", style("project").dim(), style(&cli.project).cyan());
    eprintln!("  {}      {}", style("root").dim(), style(root.display()).cyan());
    eprintln!();

    // ── Scan ────────────────────────────────────────────────────────────────
    let spin = spinner("Scanning…");
    let scan = scan(&root, &cli.includes, &cli.excludes).context("file scan failed")?;
    spin.finish_and_clear();

    if scan.files.is_empty() {
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

    // ── HTTP client ─────────────────────────────────────────────────────────
    let http = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(cli.no_verify)
        .build()
        .context("failed to build HTTP client")?;

    // ── Upload batches ───────────────────────────────────────────────────────
    let total = scan.files.len();
    let pb = progress_bar(total as u64);
    let t0 = Instant::now();

    let mut new_chunks: u64 = 0;
    let mut n_no_chunks: u64 = 0;
    let mut n_errors: usize = 0;
    let mut n_done: usize = 0;

    'batches: for batch in scan.files.chunks(cli.batch_size) {
        if cancel.is_cancelled() {
            break;
        }

        // ── Read files (skip binary / unreadable) ────────────────────────
        let mut req_files: HashMap<String, HashMap<String, Code>> = HashMap::new();
        let mut readable: usize = 0;

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
                    n_errors += 1;
                    n_done += 1;
                    pb.inc(1);
                    if cli.verbose {
                        pb.println(format!(
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

        // ── Upload ───────────────────────────────────────────────────────
        match upload_batch(
            &http,
            &cli.server,
            &cli.protocol,
            &cli.project,
            IndexRequest { files: req_files },
            &cancel,
        )
        .await
        {
            Ok(resp) => {
                tally_response(&resp, &mut new_chunks, &mut n_no_chunks);
                if cli.verbose {
                    print_verbose(&pb, &resp);
                }
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    break 'batches;
                }
                n_errors += readable;
                pb.println(format!(
                    "  {} batch error: {}",
                    style("✗").red(),
                    style(e.to_string()).red().dim(),
                ));
            }
        }

        n_done += readable;
        pb.set_position(n_done as u64);
        pb.set_message(format!("{new_chunks} chunks"));
    }

    pb.finish_and_clear();

    // ── Summary ──────────────────────────────────────────────────────────────
    print_summary(t0.elapsed(), total, new_chunks, n_no_chunks, n_errors, cancel.is_cancelled());

    if cancel.is_cancelled() || n_errors > 0 {
        std::process::exit(1);
    }

    Ok(())
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

fn progress_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("  [{bar:44.green/white}] {pos}/{len}  {msg}")
            .unwrap()
            .progress_chars("█░"),
    );
    pb.set_message("0 chunks");
    pb
}

fn print_scan_summary(scan: &ScanResult) {
    let mut by_lang: HashMap<Language, usize> = HashMap::new();
    for f in &scan.files {
        *by_lang.entry(f.language).or_default() += 1;
    }
    let mut counts: Vec<(Language, usize)> = by_lang.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name().cmp(b.0.name())));

    eprint!("  {} files", style(scan.files.len()).bold());
    for (lang, n) in &counts {
        eprint!("  {}:{}", style(lang.name()).cyan().dim(), n);
    }
    eprintln!("\n");
}

fn tally_response(resp: &IndexResponse, new_chunks: &mut u64, n_no_chunks: &mut u64) {
    for paths in resp.files.values() {
        for &count in paths.values() {
            if count == 0 {
                *n_no_chunks += 1;
            } else {
                *new_chunks += count;
            }
        }
    }
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
