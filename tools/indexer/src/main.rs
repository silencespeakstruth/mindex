mod client;
mod config;
mod git;
mod scanner;

use anyhow::{Context, Result, bail};
use clap::Parser;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use sha2::{Digest, Sha256};

use client::{
    Code, DriftRequest, HistoryRequest, IndexRequest, IndexResponse, IndexStreamEvent, check_drift,
    post_history, upload_batch, upload_batch_streaming,
};
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
The project GUID and the standing include/exclude/language scope come from the\n\
.mindex file at --root; --project/--include/--exclude/--language override it.\n\
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

    /// Project GUID. Defaults to the `guid:` of the .mindex file at --root.
    /// Either spelling is accepted (dashed or 32-char hex).
    #[arg(long)]
    project: Option<String>,

    /// Root directory; all paths stored in mindex are relative to this
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Include glob (repeatable). Matched against the path relative to --root.
    /// Overrides the .mindex `include_paths`. If neither is given, every file
    /// with a recognised extension is included.
    /// Example: --include 'src/**/*.rs' --include 'tests/**/*.rs'
    #[arg(long = "include", value_name = "GLOB")]
    includes: Vec<String>,

    /// Exclude glob (repeatable). Evaluated before includes. Overrides the
    /// .mindex `exclude_paths` — which is normally what you want indexing this
    /// project, so reach for this only to narrow a one-off run.
    /// Example: --exclude 'target/**' --exclude 'node_modules/**' --exclude '.git/**'
    #[arg(long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,

    /// Language filter (repeatable), lowercase mindex ids. Overrides the
    /// .mindex `languages`. Example: --language rust --language python
    #[arg(long = "language", value_name = "LANG")]
    languages: Vec<String>,

    /// Print the resolved project GUID to stdout and exit. Lets scripts (the
    /// post-commit hook) reach the project identity without parsing .mindex
    /// themselves — this binary is the one parser.
    #[arg(long)]
    print_guid: bool,

    /// Skip TLS certificate verification (required for the default self-signed cert)
    #[arg(long)]
    no_verify: bool,

    /// PEM bundle to trust in addition to the OS store (for a CA this host does not
    /// know); or config ca_cert
    #[arg(long, value_name = "PATH")]
    ca_cert: Option<PathBuf>,

    /// API key sent as `X-Api-Key` on every request; or $MINDEX_API_KEY, or config
    /// api_key. mindex itself has no authentication — set this only when a reverse
    /// proxy in front of it demands a key. Prefer the environment variable: a flag
    /// value is visible in `ps` to every user on the machine.
    ///
    /// $MINDEX_API_KEY is read in `main` rather than via clap's `env` attribute,
    /// which needs clap's non-default `env` feature.
    #[arg(long, value_name = "KEY")]
    api_key: Option<String>,

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

    /// Reindex every matched file even if the server considers it up to date.
    ///
    /// The server normally skips a file whose content hash AND derivation versions
    /// both match, so an ordinary run already picks up slicer/tags-query changes on
    /// its own. Use this only for what versioning cannot see: a grammar-crate bump
    /// with the version constant untouched, a suspected-corrupt index, or debugging.
    /// Every matched file is re-sliced and re-embedded, so scope the run with
    /// --include/--exclude rather than forcing the whole tree.
    #[arg(long)]
    force: bool,

    /// Rebuild only the symbol table, leaving chunks and vectors untouched.
    ///
    /// Symbols come from tree-sitter alone, so this never touches the GPU or Qdrant —
    /// it is the cheap way to apply a SYMBOLS_DERIVATION_VERSION bump. Files whose
    /// content changed since they were indexed are skipped (their chunks are stale
    /// too); run without this flag for those.
    #[arg(long)]
    symbols_only: bool,

    /// Also reconcile the project's git history (the second content channel).
    ///
    /// Walks the commits reachable from the tracked refs within the configured
    /// window and posts them; the server inserts what it lacks and drops what
    /// this run did not name. Since a sha is its own content hash, a force-push
    /// or a rebase needs no special handling — it is one reconciliation in which
    /// many shas orphan at once. Costs no GPU and no Qdrant work.
    #[arg(long, conflicts_with = "no_history")]
    history: bool,

    /// Skip the git history reconciliation even if the config file enables it.
    #[arg(long)]
    no_history: bool,

    /// Reconcile only the git history, leaving the working tree alone.
    ///
    /// Restricts the run to the history phase; it does NOT switch the channel on
    /// — pass --history as well for a one-off against a config that has it off.
    /// That split is what lets the post-commit hook pass this unconditionally
    /// without enabling the channel behind the operator's back.
    ///
    /// A separate mode rather than a flag on an ordinary run because --include
    /// narrows the walk too: a run scoped to one commit's files would filter
    /// every *other* commit's paths down to those and drop nearly the whole
    /// history as out of scope.
    #[arg(long, conflicts_with_all = ["no_history", "check", "symbols_only"])]
    history_only: bool,

    /// Ref pattern bounding the history walk (repeatable), e.g.
    /// --git-ref master --git-ref 'feat/*'. Overrides the .mindex `git_refs`;
    /// like --include/--exclude it REPLACES the list rather than extending it.
    #[arg(long = "git-ref", value_name = "PATTERN")]
    git_refs: Vec<String>,

    /// Age bound on the history walk, in days. Applied together with
    /// --history-max-commits; the stricter of the two binds.
    #[arg(long, value_name = "DAYS")]
    history_since_days: Option<u64>,

    /// Count bound on the history walk. Applied together with
    /// --history-since-days; the stricter of the two binds.
    #[arg(long, value_name = "N")]
    history_max_commits: Option<usize>,
}

/// Project identity + file selection for this run.
#[derive(Debug)]
struct Scope {
    project: String,
    includes: Vec<String>,
    excludes: Vec<String>,
    languages: Vec<String>,
    /// Ref patterns whose commits make up the project's history. Empty means the
    /// indexer config's own fallback — this is a scope key, not a switch; whether
    /// history is walked at all is `--history`.
    git_refs: Vec<String>,
}

/// Resolves identity and scope with the crate's standing precedence,
/// **CLI flag > .mindex > default** (`config.rs` layers the transport knobs the
/// same way). A missing `.mindex` is not an error — the flags can carry a run on
/// their own — but a malformed one is: silently indexing an unfiltered tree is
/// exactly the accident the file exists to prevent.
fn resolve_scope(cli: &Cli, root: &Path) -> Result<Scope> {
    let path = root.join(mindexfile::FILE_NAME);
    let file = if path.is_file() {
        Some(mindexfile::parse(&path)?)
    } else {
        None
    };

    let project = match cli.project.as_deref() {
        Some(p) => mindexfile::normalize_guid(p).context("bad --project")?,
        None => match &file {
            Some(f) => f.guid.clone(),
            None => bail!(
                "no project GUID: pass --project, or put a .mindex file with a \
                 `guid:` key at {}",
                path.display()
            ),
        },
    };

    // A flag *replaces* the file's list rather than adding to it: a one-off run
    // scoped to `--include src/**` must not still drag in the file's includes.
    let pick = |flag: &[String], from_file: Option<&Vec<String>>| -> Vec<String> {
        if flag.is_empty() {
            from_file.cloned().unwrap_or_default()
        } else {
            flag.to_vec()
        }
    };

    Ok(Scope {
        project,
        includes: pick(&cli.includes, file.as_ref().map(|f| &f.include_paths)),
        excludes: pick(&cli.excludes, file.as_ref().map(|f| &f.exclude_paths)),
        languages: pick(&cli.languages, file.as_ref().map(|f| &f.languages)),
        git_refs: pick(&cli.git_refs, file.as_ref().map(|f| &f.git_refs)),
    })
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
    /// The most recently touched file (prepared or indexed), for the bar's status
    /// line. With several workers it is simply the last event to arrive — a live
    /// "what is it doing right now", not an ordered log.
    current: std::sync::Mutex<String>,
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

    // --print-guid answers one question and exits, before any config or network
    // work, so a script can capture clean stdout.
    if cli.print_guid {
        let root = cli
            .root
            .canonicalize()
            .with_context(|| format!("cannot access root: {}", cli.root.display()))?;
        println!("{}", resolve_scope(&cli, &root)?.project);
        return Ok(());
    }

    // Two-level config: TOML file (XDG: mindex/indexer.toml) → CLI overrides → defaults.
    let cfg = config::resolve(config::Overrides {
        config: cli.config.clone(),
        server: cli.server.clone(),
        protocol: cli.protocol.clone(),
        batch_size: cli.batch_size,
        concurrency: cli.concurrency,
        no_verify: cli.no_verify,
        ca_cert: cli.ca_cert.clone(),
        // Same precedence as every other setting: flag beats environment beats
        // file. `MINDEX_API_KEY` is the spelling the MCP servers and
        // mindex-search.sh already use.
        api_key: cli.api_key.clone().or_else(|| {
            std::env::var("MINDEX_API_KEY")
                .ok()
                .filter(|v| !v.is_empty())
        }),
        // Two flags for one setting: the file default is `false`, so `--history`
        // alone could never express "off" and a config that enabled it would be
        // unoverridable from the command line.
        history: match (cli.history, cli.no_history) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        },
        history_since_days: cli.history_since_days,
        history_max_commits: cli.history_max_commits,
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

    let scope = resolve_scope(&cli, &root)?;

    // ── Header (table-aligned: labels padded to a common width) ───────────────
    eprintln!();
    let row = |label: &str, value: String| {
        eprintln!(
            "  {}  {}",
            style(format!("{label:<7}")).dim(),
            style(value).cyan()
        );
    };
    row("server", cfg.server_url.clone());
    row("project", scope.project.clone());
    row("root", root.display().to_string());
    row("threads", concurrency.to_string());
    eprintln!();

    // ── History-only: reconcile the second channel and exit. Placed before the
    // scan because skipping the tree entirely is the whole point of the mode —
    // it is what makes the post-commit hook cheap enough to run every commit.
    if cli.history_only {
        if !cfg.history {
            eprintln!(
                "history is disabled (indexer config `history = false`); nothing to do. \
                 Pass --history as well for a one-off."
            );
            return Ok(());
        }
        let http = build_http_client(&cfg).context("failed to build HTTP client")?;
        reconcile_history(&http, &cfg, &scope, &root, &cancel).await?;
        if cancel.is_cancelled() {
            std::process::exit(1);
        }
        return Ok(());
    }

    // ── Scan ────────────────────────────────────────────────────────────────
    let spin = spinner("Scanning…");
    let scan = scan(&root, &scope.includes, &scope.excludes, &scope.languages)
        .context("file scan failed")?;
    spin.finish_and_clear();

    // An empty tree still matters to --check: everything indexed is then orphaned,
    // so only short-circuit on empty when we're actually about to index.
    if scan.files.is_empty() && !cli.check {
        eprintln!(
            "  {} No source files found.{}",
            style("—").dim(),
            if scan.skipped_unknown > 0 {
                format!(
                    "  ({} files with unrecognised extensions skipped)",
                    scan.skipped_unknown
                )
            } else {
                String::new()
            }
        );
        eprintln!();
        return Ok(());
    }

    print_scan_summary(&scan);

    // ── HTTP client (shared by every worker) ──────────────────────────────────
    let http = Arc::new(build_http_client(&cfg).context("failed to build HTTP client")?);

    let total = scan.files.len();

    // ── Drift check: hash the tree, ask the server what diverged, report, exit.
    // No uploads, no warm-up (read-only against an existing project).
    if cli.check {
        let actionable = run_check(
            &http,
            &cfg.server_url,
            &scope.project,
            scan.files,
            &cancel,
            cli.json,
        )
        .await?;
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
            &scope.project,
            IndexRequest {
                files: HashMap::new(),
                force: cli.force,
                symbols_only: cli.symbols_only,
            },
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
        let project = scope.project.clone();
        let batch_size = cfg.batch_size_files;
        let verbose = cli.verbose;
        let (force, symbols_only) = (cli.force, cli.symbols_only);
        handles.push(tokio::spawn(async move {
            run_worker(
                shard,
                bar,
                http,
                server,
                protocol,
                project,
                batch_size,
                verbose,
                force,
                symbols_only,
                cancel,
                shared,
            )
            .await
        }));
    }

    // ── Drive the bar from the shared counters. Position updates every tick.
    // The chunk counter advances per embed batch (SSE `embedded` events), so the
    // speed line can be an honest **windowed** rate — what the GPU is doing right
    // now — instead of the old cumulative average that a long run flattened into
    // meaninglessness. Until the window fills (and against an old JSON-only
    // server, whose counter still jumps per batch) the oldest retained sample is
    // t=0, which makes the very same formula the cumulative average — the stable
    // fallback, not a special case. ETA keeps the cumulative file rate. ────────
    let total_files = total as u64;
    let tick_stop = CancellationToken::new();
    let ticker = {
        let bar = bar.clone();
        let shared = shared.clone();
        let stop = tick_stop.clone();
        tokio::spawn(async move {
            const RATE_WINDOW_SECS: f64 = 20.0;
            let mut samples: std::collections::VecDeque<(f64, u64)> =
                std::collections::VecDeque::from([(0.0, 0u64)]);
            loop {
                let done = shared.files_done.load(Ordering::Relaxed);
                let chunks = shared.chunks.load(Ordering::Relaxed);
                let active = shared.active.load(Ordering::Relaxed);
                let errs = shared.errors.load(Ordering::Relaxed);
                let current = shared.current.lock().unwrap().clone();

                bar.set_position(done);

                let elapsed = t0.elapsed().as_secs_f64();
                samples.push_back((elapsed, chunks));
                while samples.len() > 2
                    && elapsed - samples.front().map(|s| s.0).unwrap_or(0.0) > RATE_WINDOW_SECS
                {
                    samples.pop_front();
                }
                let (t_old, chunks_old) = *samples.front().unwrap_or(&(0.0, 0));
                let dt = elapsed - t_old;
                let chunks_per_s = if dt > 0.0 {
                    chunks.saturating_sub(chunks_old) as f64 / dt
                } else {
                    0.0
                };
                let files_per_s = if elapsed > 0.0 {
                    done as f64 / elapsed
                } else {
                    0.0
                };
                let remaining = total_files.saturating_sub(done);
                let eta = if files_per_s > 0.0 {
                    remaining as f64 / files_per_s
                } else {
                    f64::INFINITY
                };
                bar.set_message(format!(
                    "{chunks_per_s:.0} chunks/s · ETA {} · {chunks} chunks · {active} active{}{}",
                    fmt_eta(eta),
                    if errs > 0 {
                        format!(" · {errs} err")
                    } else {
                        String::new()
                    },
                    if current.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", path_tail(&current, 42))
                    },
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
        // The join is awaited into a binding rather than tested inline: under edition
        // 2024 an `if let` scrutinee holds its temporaries for the whole block, and the
        // awaitee owns a custom destructor.
        let joined = h.await;
        if let Ok(s) = joined {
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
        cli.symbols_only,
    );

    // ── Git history: the second content channel, opt-in and best-effort ───────
    // Runs after the files because it is secondary to them and costs no GPU. A
    // failure here is a WARN, never a failed run: history is an addition to what
    // the working tree already says, and refusing to index a repository because
    // `git` is missing or the root is not a checkout would be the wrong trade —
    // the same reasoning that makes a failed symbol extraction degrade to "no
    // symbols" rather than failing the file.
    if cfg.history
        && !cancel.is_cancelled()
        && let Err(e) = reconcile_history(&http, &cfg, &scope, &root, &cancel).await
    {
        eprintln!(
            "{} git history skipped: {e:#}",
            style("warning:").yellow().bold()
        );
    }

    if cancel.is_cancelled() || totals.errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Walk the tracked refs and reconcile the result against the server.
///
/// One request, not a diff negotiation: a sha is the hash of its own content, so
/// re-posting a commit the server already holds is free, and the server's reply
/// says how many were new. That is also why a force-push needs nothing special —
/// the walk simply reaches a different set, and the server drops what this run
/// did not name.
async fn reconcile_history(
    http: &reqwest::Client,
    cfg: &config::IndexerConfig,
    scope: &Scope,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    // `.mindex` first, the config file's list as the fallback — the same
    // precedence the path scope follows, one level down.
    let patterns: Vec<String> = if scope.git_refs.is_empty() {
        cfg.git_refs.clone()
    } else {
        scope.git_refs.clone()
    };

    let refs = git::resolve_refs(root, &patterns)?;
    if refs.is_empty() {
        anyhow::bail!(
            "none of the configured git_refs ({}) matched a local branch",
            patterns.join(", ")
        );
    }

    let (includes, excludes) = mindexfile::build_globsets(&scope.includes, &scope.excludes)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let walk = git::walk(
        root,
        &refs,
        cfg.history_max_age_days,
        cfg.history_max_commits,
        cfg.history_min_message_bytes,
        includes.as_ref(),
        excludes.as_ref(),
        now,
    )?;

    // Every drop is announced. A channel that quietly indexes a third of what it
    // walked is indistinguishable from a repository that small, and the first
    // question anyone asks of a thin history is which of the two happened.
    let dropped =
        walk.skipped_short_message + walk.skipped_generated_merge + walk.skipped_out_of_scope;
    if dropped > 0 || walk.truncated_messages > 0 {
        eprintln!(
            "history: dropped {dropped} commits ({} too short, {} generated merges, {} out of scope){}",
            walk.skipped_short_message,
            walk.skipped_generated_merge,
            walk.skipped_out_of_scope,
            if walk.truncated_messages > 0 {
                format!("; truncated {} oversized messages", walk.truncated_messages)
            } else {
                String::new()
            }
        );
    }

    let posted = walk.commits.len();
    let res = post_history(
        http,
        &cfg.server_url,
        &cfg.protocol,
        &scope.project,
        HistoryRequest {
            since: walk.since,
            commits: walk.commits,
        },
        cancel,
    )
    .await?;

    println!(
        "{} {} refs · {posted} commits · {} new · {} unchanged · {} removed",
        style("history").cyan().bold(),
        refs.len(),
        res.indexed,
        res.unchanged,
        res.removed,
    );
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
    force: bool,
    symbols_only: bool,
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

        // Per-event progress: files advance the bar as the server settles them,
        // embed batches advance the chunk counter (the honest chunks-per-second
        // source), and `counted` remembers how many files the events already
        // moved so the post-request catch-up below never double-counts.
        let mut counted: u64 = 0;
        let mut on_event = |ev: IndexStreamEvent| match ev {
            IndexStreamEvent::Started { .. } => {}
            IndexStreamEvent::Prepared { path, .. } => {
                *shared.current.lock().unwrap() = path;
            }
            IndexStreamEvent::Skipped { path, reason } => {
                counted += 1;
                shared.files_done.fetch_add(1, Ordering::Relaxed);
                // Unchanged files are the silent common case (they are absent
                // from the JSON response too); the rarer reasons are worth a line.
                if verbose && reason != "unchanged" {
                    bar.println(format!(
                        "  {} {}  {}",
                        style("!").yellow(),
                        path,
                        style(reason).yellow().dim(),
                    ));
                }
            }
            IndexStreamEvent::Embedded { batch_chunks, .. } => {
                shared.chunks.fetch_add(batch_chunks, Ordering::Relaxed);
            }
            IndexStreamEvent::Indexed { path, count } => {
                counted += 1;
                shared.files_done.fetch_add(1, Ordering::Relaxed);
                // The embed pass never runs under --symbols-only, so the live
                // counter advances on the per-file symbol counts instead.
                if symbols_only {
                    shared.chunks.fetch_add(count, Ordering::Relaxed);
                }
                if verbose {
                    print_verbose_line(&bar, &path, count);
                }
                *shared.current.lock().unwrap() = path;
            }
        };

        match upload_batch_streaming(
            &http,
            &server,
            &protocol,
            &project,
            IndexRequest {
                files: req_files,
                force,
                symbols_only,
            },
            &cancel,
            &mut on_event,
        )
        .await
        {
            Ok(outcome) => {
                let (chunks, too_short) = tally_response(&outcome.response);
                stats.new_chunks += chunks;
                stats.too_short += too_short;
                if !outcome.streamed {
                    // Plain-JSON fallback (an older server): the callback saw
                    // nothing, so the counters and verbose lines come from the
                    // response, batch-granular — exactly the old behaviour.
                    shared.chunks.fetch_add(chunks, Ordering::Relaxed);
                    if verbose {
                        print_verbose(&bar, &outcome.response);
                    }
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

        // Catch up the bar for whatever the events did not report per file:
        // everything in the JSON fallback, and the tail of a failed stream.
        shared
            .files_done
            .fetch_add(readable.saturating_sub(counted), Ordering::Relaxed);
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

    let drift = check_drift(
        http,
        server,
        project,
        DriftRequest { files: manifest },
        cancel,
    )
    .await
    .context("drift check request failed (server unreachable, bad project GUID, or TLS?)")?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "stale": drift.stale,
                "missing": drift.missing,
                "orphaned": drift.orphaned,
                "indexing": drift.indexing,
            }))?
        );
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
        eprintln!(
            "  {}",
            style(format!("{label} ({})", paths.len())).fg(color).bold()
        );
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
        eprintln!(
            "  {} index in sync{}",
            style("✓").green(),
            style(note).dim()
        );
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

/// The trailing `max_chars` of a path for the bar's one-line status, `…`-prefixed
/// when truncated — the file name end is the informative half.
fn path_tail(path: &str, max_chars: usize) -> String {
    let count = path.chars().count();
    if count <= max_chars {
        return path.to_string();
    }
    let tail: String = path
        .chars()
        .skip(count.saturating_sub(max_chars.saturating_sub(1)))
        .collect();
    format!("…{tail}")
}

/// The one HTTP client every request goes through.
///
/// TLS trusts the OS store (reqwest's `rustls-tls-native-roots`), plus `ca_cert`
/// when the server's CA is not installed there. `no_verify` remains the escape
/// hatch for the self-signed certificate `scripts/entrypoint.sh` generates, which
/// no store can vouch for.
///
/// `api_key`, when set, rides on every request as `X-Api-Key`. It is for a proxy
/// in front of mindex; the server ignores the header.
fn build_http_client(cfg: &config::IndexerConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::ClientBuilder::new().danger_accept_invalid_certs(cfg.no_verify);
    if let Some(path) = &cfg.ca_cert {
        let pem = std::fs::read(path)
            .with_context(|| format!("cannot read ca_cert {}", path.display()))?;
        for cert in reqwest::Certificate::from_pem_bundle(&pem)
            .with_context(|| format!("cannot parse ca_cert {} as PEM", path.display()))?
        {
            builder = builder.add_root_certificate(cert);
        }
    }
    if let Some(key) = &cfg.api_key {
        let mut value = reqwest::header::HeaderValue::from_str(key)
            .context("api_key contains characters that are not valid in an HTTP header")?;
        // Keeps the key out of reqwest's Debug output, which error paths print.
        value.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-api-key", value);
        builder = builder.default_headers(headers);
    }
    Ok(builder.build()?)
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
    // Named, not counted: an over-cap file is invisible to both indexing and drift,
    // and silence about it reads as "that file is fine".
    for rel in &scan.skipped_too_large {
        eprintln!(
            "  {} {}  {}",
            style("!").yellow(),
            rel,
            style(format!(
                "larger than {} MiB — not indexed",
                mindexfile::MAX_CODE_BYTES / (1024 * 1024)
            ))
            .yellow()
            .dim(),
        );
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
        print_verbose_line(pb, &path, count);
    }
}

/// One file's verbose line — shared by the batch printer above (JSON fallback)
/// and the per-`indexed`-event streaming path, so both modes read identically.
fn print_verbose_line(pb: &ProgressBar, path: &str, count: u64) {
    if count == 0 {
        pb.println(format!(
            "  {} {}  {}",
            style("⊘").dim(),
            style(path).dim(),
            style("0 chunks (too short)").dim(),
        ));
    } else {
        pb.println(format!(
            "  {} {}  {}",
            style("✓").green(),
            path,
            style(format!(
                "{count} chunk{}",
                if count == 1 { "" } else { "s" }
            ))
            .green(),
        ));
    }
}

fn print_summary(
    elapsed: Duration,
    total: usize,
    new_chunks: u64,
    n_no_chunks: u64,
    n_errors: usize,
    cancelled: bool,
    symbols_only: bool,
) {
    let secs = elapsed.as_secs_f64();
    // Under --symbols-only the server returns symbol-row counts, not chunk counts,
    // and nothing is ever "too short" (that is a slicer verdict).
    let unit = if symbols_only {
        "symbols"
    } else {
        "new chunks"
    };
    let short = if symbols_only {
        String::new()
    } else {
        format!(" · {n_no_chunks} too short")
    };

    if cancelled {
        eprintln!(
            "  {} Cancelled after {secs:.1}s — {total} files queued · {} {unit}{short} · {} errors",
            style("⚠").yellow(),
            style(new_chunks).green(),
            style(n_errors).red(),
        );
    } else if n_errors > 0 {
        eprintln!(
            "  {} {secs:.1}s · {} files · {} {unit}{short} · {} errors",
            style("⚠").yellow(),
            style(total).bold(),
            style(new_chunks).green(),
            style(n_errors).red(),
        );
    } else {
        eprintln!(
            "  {} {secs:.1}s · {} files · {} {unit}{short}",
            style("✓").green(),
            style(total).bold(),
            style(new_chunks).green(),
        );
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUID: &str = "123e4567-e89b-42d3-a456-426614174000";

    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["mindex-index"];
        argv.extend_from_slice(args);
        Cli::parse_from(argv)
    }

    fn root_with(mindex: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(text) = mindex {
            std::fs::write(dir.path().join(mindexfile::FILE_NAME), text).unwrap();
        }
        dir
    }

    #[test]
    fn scope_comes_from_the_mindex_file_when_no_flags_are_passed() {
        let dir = root_with(Some(&format!(
            "guid: {GUID}\nexclude_paths:\n  - tools/**\nlanguages:\n  - rust\n"
        )));
        let s = resolve_scope(&cli(&[]), dir.path()).unwrap();
        assert_eq!(s.project, GUID);
        assert_eq!(s.excludes, vec!["tools/**"]);
        assert_eq!(s.languages, vec!["rust"]);
        assert!(s.includes.is_empty());
    }

    #[test]
    fn flags_replace_the_files_lists_rather_than_adding_to_them() {
        let dir = root_with(Some(&format!(
            "guid: {GUID}\nexclude_paths:\n  - tools/**\n"
        )));
        let s = resolve_scope(&cli(&["--exclude", "perf/**"]), dir.path()).unwrap();
        assert_eq!(
            s.excludes,
            vec!["perf/**"],
            "a one-off scope must not inherit"
        );
    }

    #[test]
    fn project_flag_wins_over_the_file_and_is_normalized() {
        let dir = root_with(Some(&format!("guid: {GUID}\n")));
        let s = resolve_scope(
            &cli(&["--project", "0000000000004000800000000000FFFF"]),
            dir.path(),
        )
        .unwrap();
        assert_eq!(s.project, "00000000-0000-4000-8000-00000000ffff");
    }

    #[test]
    fn missing_file_needs_a_project_flag() {
        let dir = root_with(None);
        let err = resolve_scope(&cli(&[]), dir.path()).unwrap_err();
        assert!(err.to_string().contains("--project"), "{err}");

        let s = resolve_scope(&cli(&["--project", GUID]), dir.path()).unwrap();
        assert_eq!(s.project, GUID);
        assert!(s.excludes.is_empty());
    }

    #[test]
    fn a_malformed_file_is_an_error_even_with_a_project_flag() {
        // Falling back to "no scope" here would index the tree the file excludes.
        let dir = root_with(Some("guid: {{{\n"));
        assert!(resolve_scope(&cli(&["--project", GUID]), dir.path()).is_err());
    }
}
