use crate::backend::http3::{EmbeddingModel, RouterState};
use crate::config::Cli;
use crate::db::qdrant::{QdrantStore, VectorStore};
use crate::db::qdrant_metrics::MeteredVectorStore;
use crate::db::sqlite3::SQLite3Pool;
use crate::embed::EmbedTuning;
use crate::models::bge_m3::{BGEm3HttpClient, BGEm3Model, BGEm3Tuning, MeteredEmbedder};
use crate::models::ollama::{OllamaHttpClient, OllamaModel, OllamaTuning};
use crate::worker::retry::RetryTuning;
use clap::Parser;
use qdrant_client::Qdrant;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokenizers::Tokenizer;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Spawn a background worker under supervision.
///
/// Every worker used to be a bare `tokio::spawn` with its `JoinHandle` dropped, which
/// makes a panic inside one **silent and permanent**: the task dies, nothing joins it,
/// nothing restarts it, and the only trace is that whatever the worker maintained
/// quietly stops changing. GC ceasing to reclaim vectors and the retry sweep ceasing
/// to rescue stuck files both look, from outside, exactly like a healthy idle system.
///
/// This does not restart anything — a worker that panicked once will panic again, and
/// a restart loop would bury the bug under its own noise. It makes the death *visible*:
/// an `error!` naming the worker, a counter, and a gauge that drops to 0 and stays
/// there, which is the alertable signal. Restarting is a decision for after there is
/// evidence any of these ever die.
fn supervise<F>(name: &'static str, metrics: &Arc<backend::metrics::Metrics>, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    use backend::metrics::{WorkerLabels, WorkerOutcomeLabels};

    let running = metrics
        .supervisor
        .running
        .get_or_create(&WorkerLabels { worker: name })
        .clone();
    // Published before the task starts, so the series exists from the first scrape.
    // A worker that dies immediately would otherwise be *absent* rather than zero,
    // and no alert can fire on a series that was never written.
    running.set(1);

    let metrics = Arc::clone(metrics);
    tokio::spawn(async move {
        let outcome = if tokio::spawn(fut).await.is_ok() {
            info!(worker = name, "Background worker exited.");
            "ok"
        } else {
            error!(
                worker = name,
                "Background worker task died and will not be restarted; the work it \
                 does is not happening any more. Sysadmin: find the panic above this \
                 line and restart the service — mindex_worker_running{{worker}} stays \
                 at 0 until it is."
            );
            "panic"
        };
        running.set(0);
        metrics
            .supervisor
            .exits
            .get_or_create(&WorkerOutcomeLabels {
                worker: name,
                outcome,
            })
            .inc();
    });
}

mod backend;
mod config;
mod db;
mod embed;
mod models;
mod research;
mod slicing;
mod worker;

type BoxError = Box<dyn Error + Send + Sync>;

/// How long a signalled shutdown waits for in-flight work before exiting anyway.
///
/// Not configurable: it is a floor on politeness, not a tuning knob, and the real
/// ceiling is whatever the supervisor allows before SIGKILL (systemd's default
/// `TimeoutStopSec` is 90 s, Docker's `--time` is 10 s). Sized under both.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(8);

// Applied in order on startup: only migrations whose version exceeds the
// current `PRAGMA user_version` are run, inside one transaction. `pub(crate)`
// so test modules build a schema-identical `:memory:` pool from the same source.
pub(crate) const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("db/migrations/v1.0.0_schema.sql")),
    (2, include_str!("db/migrations/v1.1.0_git_history.sql")),
    (
        3,
        include_str!("db/migrations/v1.1.0_toml_yaml_languages.sql"),
    ),
    (4, include_str!("db/migrations/v1.2.0_research_context.sql")),
    (
        5,
        include_str!("db/migrations/v1.3.0_research_verification.sql"),
    ),
    (
        6,
        include_str!("db/migrations/v1.4.0_symbol_definitions.sql"),
    ),
];

/// Applies every migration whose version exceeds the DB's `PRAGMA user_version`,
/// then stamps `user_version` to the highest applied version. Returns the resulting
/// schema version and whether anything was applied. Extracted from the startup
/// transaction so the versioning logic is unit-testable.
pub(crate) fn apply_pending_migrations(
    tx: &rusqlite::Transaction,
) -> Result<(i32, bool), db::sqlite3::SQLite3PoolError> {
    apply_migrations_from(tx, MIGRATIONS)
}

/// [`apply_pending_migrations`] over a given list.
///
/// The list is a parameter for one reason: the `pragma_foreign_key_check` below is
/// the guard that stops a migration which orphaned a row from reaching a running
/// server, where the damage is silent — and there is no way to exercise a *failing*
/// migration while the only list is the real one, which by construction passes.
fn apply_migrations_from(
    tx: &rusqlite::Transaction,
    migrations: &[(i32, &str)],
) -> Result<(i32, bool), db::sqlite3::SQLite3PoolError> {
    let current: i32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let pending: Vec<_> = migrations.iter().filter(|(v, _)| *v > current).collect();
    for (_, sql) in &pending {
        tx.execute_batch(sql)?;
    }
    if let Some((max_v, _)) = pending.last() {
        // Migrations run with foreign-key enforcement off (see
        // `SQLite3Pool::migration_transaction`), so this is the check that would
        // otherwise have run statement by statement. Failing here rolls the whole
        // batch back — a migration that orphaned a chunk or a symbol row must not
        // reach a running server, where the damage is silent.
        let violations: i64 =
            tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if violations > 0 {
            error!(
                violations,
                schema_version = *max_v,
                "Schema migration left dangling foreign-key references; rolling back."
            );
            return Err(db::sqlite3::SQLite3PoolError::Sql(
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                    Some(format!(
                        "migration to schema version {max_v} left {violations} \
                         dangling foreign-key reference(s)"
                    )),
                ),
            ));
        }
        tx.pragma_update(None, "user_version", max_v)?;
    }
    let applied = !pending.is_empty();
    Ok((pending.last().map_or(current, |(v, _)| *v), applied))
}

/// Seconds since the Unix epoch, saturating at 0 before it. Used for the
/// `start_time_seconds` gauge, which Grafana renders as uptime via `time() - x`,
/// and for the Ollama catalog's `refreshed_at`.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::registry()
        // `EnvFilter`'s own default when `RUST_LOG` is unset is ERROR, which
        // silently discards every INFO/WARN this service emits — the permanently
        // -failed-files warning, the config-resolution trace, the per-research
        // -run record. The containers set `RUST_LOG=info` explicitly, so that
        // silence only ever hit bare/systemd runs, where nobody was looking.
        // Default to INFO instead; `RUST_LOG` still overrides.
        .with(
            EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with(
            fmt::layer()
                .json()
                .with_file(true)
                .with_line_number(true)
                .with_current_span(true)
                .with_span_list(true)
                .flatten_event(true)
                .with_ansi(std::env::var("RUST_ENV") == Ok("dev".into()))
                .pretty(),
        )
        .init();

    let token = CancellationToken::new();

    // The one startup panic that used to be a bare `unwrap`. Registering a signal
    // handler fails only if the process cannot install one at all, and without it
    // `docker stop` / `systemctl stop` would go unheard — so panicking is right, but
    // it has to say which of the startup steps failed, like every other one here.
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("cannot install a SIGTERM handler; the process would ignore every stop request");
    let sigterm_token = token.child_token();

    let provider = rustls::crypto::ring::default_provider();
    let _ = provider.install_default();

    let cli = Cli::parse();

    // Two-level config: TOML file (XDG-resolved) → CLI overrides → built-in defaults.
    // `resolve` logs where it looked, what it loaded, and every flag override; on a
    // fatal config / validation error it returns the already-formatted message and we
    // refuse to start.
    let (cfg, config_source) = match config::resolve(&cli) {
        Ok(v) => v,
        Err(e) => {
            // Log the (already-formatted, multi-line) message and exit non-zero
            // directly — returning `Err` would make the runtime *also* dump the
            // error via Debug, double-printing it with escaped newlines.
            error!(error = %e, "Invalid configuration; refusing to start.");
            std::process::exit(1);
        }
    };
    info!(source = %config_source, "Configuration resolved.");

    // Built before anything it measures, and unconditionally: `[metrics].enabled`
    // gates the endpoint and the collector, never the recording. Everything that
    // measures itself takes a handle from here — there is no global recorder.
    let metrics = Arc::new(backend::metrics::Metrics::new());
    metrics.state.start_time.set(unix_now());

    let db_pool = Arc::new(
        SQLite3Pool::new(
            cfg.database.path.as_path(),
            cfg.database.pool_size,
            cfg.database.page_size_bytes,
            &cfg.sqlite_synchronous(),
        )
        // Not a decorator: the pool is deliberately not a trait, and
        // `transaction` is the one choke point every DB call passes through.
        .with_metrics(&metrics),
    );

    let db_schema_version = match db_pool
        .migration_transaction(token, apply_pending_migrations)
        .await
    {
        Ok((v, true)) => {
            info!(db_path = ?cfg.database.path, schema_version = v, "Schema migration completed.");
            v
        }
        Ok((v, false)) => {
            info!(db_path = ?cfg.database.path, schema_version = v, "Schema already up to date; no migrations run.");
            v
        }
        Err(err) => {
            error!(
                error = ?err,
                db_path = ?cfg.database.path,
                "Schema migration failed; cannot start. \
                 Check the DB file is writable and not from an incompatible older schema \
                 (no upgrade path is maintained — drop and recreate if so)."
            );
            return Err(err.into());
        }
    };

    let model_id = cfg.model.name.as_str(); // For now, only one model is supported.

    // Identity is only knowable now: the schema version is whatever the startup
    // migration left behind.
    metrics
        .state
        .build_info
        .get_or_create(&backend::metrics::BuildLabels {
            version: env!("CARGO_PKG_VERSION"),
            db_schema_version: db_schema_version.to_string(),
            model_id: model_id.to_string(),
        })
        .set(1);
    metrics
        .state
        .research_worker_threads
        .set(cfg.research.worker_threads as i64);

    // Embed/upsert tuning shared by the indexing handler and the retry worker.
    let embed_tuning = EmbedTuning {
        embed_batch: cfg.indexing.embed_batch_chunks,
        upsert_batch: cfg.qdrant.upsert_batch_points,
        sparse_min_weight: cfg.indexing.sparse_min_weight,
    };

    // Surface files that have exhausted their retries — the retry worker stops
    // touching them, so without this they are silently stuck in 'failed'.
    worker::retry::warn_permanently_failed(
        &db_pool,
        cfg.workers.max_retries,
        sigterm_token.child_token(),
    )
    .await;

    // Surface files left mid-indexing by a previous run (crash / unclean shutdown).
    // They are not force-failed — the retry worker re-embeds them back to 'indexed'.
    worker::retry::warn_orphaned_indexing(&db_pool, sigterm_token.child_token()).await;

    // The per-file indexing claim table, shared by the HTTP handlers (in `RouterState`)
    // and the retry worker so a file held by a live `/index` is never raced by a sweep.
    let indexing_locks = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // Process-wide GC flag, shared by the GC worker and the `POST /gc` handler so a
    // manual sweep and the hourly tick never run concurrently (GC is global).
    let gc_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Wrapped once, here: every consumer — handlers, GC sweep, retry worker —
    // reaches Qdrant through this one `Arc`, so the decorator cannot miss a
    // caller the way an edited call site could.
    let qdrant_client: Arc<dyn VectorStore> = Arc::new(MeteredVectorStore::new(
        Arc::new(QdrantStore::new(
            // Both timeouts set explicitly. The client's own defaults are 5 s for
            // each, and nothing here could override them: a project big enough that
            // fusion + ColBERT rerank ran past five seconds failed every single search
            // with `qdrant.unavailable`, with no knob to reach for.
            Qdrant::from_url(cfg.qdrant.server_url.as_str())
                .timeout(Duration::from_millis(cfg.qdrant.timeout_ms))
                .connect_timeout(Duration::from_millis(cfg.qdrant.connect_timeout_ms))
                .build()?,
            cfg.qdrant.dense_prefetch_limit,
            cfg.qdrant.sparse_prefetch_limit,
            cfg.qdrant.fusion_limit,
            cfg.qdrant.search_hnsw_ef,
        )),
        metrics.clone(),
    ));

    // One embedding client, shared (as a trait object) by the retry worker and the
    // HTTP handlers — built once rather than per consumer.
    let embed_client: Arc<dyn BGEm3Model> = Arc::new(MeteredEmbedder::new(
        Arc::new(
            BGEm3HttpClient::new(
                cfg.model.server_url.clone(),
                BGEm3Tuning {
                    max_429_retries: cfg.model.max_429_retries,
                    backoff_base_ms: cfg.model.backoff_base_ms,
                    health_timeout_ms: cfg.model.health_timeout_ms,
                    encode_timeout_ms: cfg.model.encode_timeout_ms,
                },
            )
            // The 429 retry loop is inside `encode`, so the decorator outside it
            // cannot see a retry — only the client can count them.
            .with_metrics(&metrics, "index"),
        ),
        metrics.clone(),
        "index",
    ));

    // The query path: a second instance when the operator split the workloads,
    // otherwise literally the same `Arc`. Same tuning — the retry/backoff and
    // timeout semantics do not change with the device.
    let query_embed_client: Arc<dyn BGEm3Model> = match &cfg.model.query_server_url {
        Some(url) => {
            info!(
                index_server = %cfg.model.server_url,
                query_server = %url,
                "Serving the query path from a separate embedder instance. It must run \
                 the same model at the same precision as the indexing instance, or \
                 query and index vectors disagree."
            );
            Arc::new(MeteredEmbedder::new(
                Arc::new(
                    BGEm3HttpClient::new(
                        url.clone(),
                        BGEm3Tuning {
                            max_429_retries: cfg.model.max_429_retries,
                            backoff_base_ms: cfg.model.backoff_base_ms,
                            health_timeout_ms: cfg.model.health_timeout_ms,
                            encode_timeout_ms: cfg.model.encode_timeout_ms,
                        },
                    )
                    .with_metrics(&metrics, "query"),
                ),
                metrics.clone(),
                "query",
            ))
        }
        // Unsplit: literally the same instance, already wrapped as `index`. The
        // `embedder` label therefore reads `index` for query traffic too, which is
        // the honest answer — there is one server doing both.
        None => embed_client.clone(),
    };

    // Dedicated small runtime for /research jobs: research is rare but long-lived
    // (minutes of local-LLM turns), so it gets its own threads instead of tying up
    // the main runtime's. Leaked deliberately — it must outlive `main`'s async
    // context (dropping a runtime from async code panics), and it lives for the
    // whole process anyway.
    let research_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.research.worker_threads)
        .thread_name("mindex-research")
        .enable_all()
        .build()?;
    let research_handle = research_runtime.handle().clone();
    std::mem::forget(research_runtime);

    let research_ollama: Arc<dyn OllamaModel> = Arc::new(
        OllamaHttpClient::new(
            cfg.research.ollama_url.clone(),
            OllamaTuning {
                max_num_ctx_tokens: cfg.research.max_num_ctx_tokens,
                turn_timeout_ms: cfg.research.turn_timeout_ms,
                first_token_timeout_ms: cfg.research.first_token_timeout_ms,
                slow_turn_tokens_per_second: cfg.research.slow_turn_tokens_per_second,
                slow_turn_unaccounted_ms: cfg.research.slow_turn_unaccounted_ms,
                health_timeout_ms: cfg.research.health_timeout_ms,
            },
        )
        // Not a decorator: both counters fire inside one `chat_stream` call and
        // are invisible in its return value.
        .with_metrics(metrics.clone()),
    );

    // Hoisted out of the `RouterState` literal so the metrics collector can derive
    // `research_active` from it. Deriving beats an inc/dec pair around the spawn:
    // a run's normal exit is a dropped SSE stream, which no `dec()` would survive.
    let research_semaphore = Arc::new(tokio::sync::Semaphore::new(cfg.research.max_concurrent));

    // The identity half of admission: the semaphore says how many slots are gone,
    // this says which runs took them and holds the tokens that stop them. Hoisted
    // because three things share it — the handlers, `GET /health`, and the watchdog.
    let research_registry = backend::inflight::ResearchRegistry::new();

    // Hoisted for the same reason: the catalog worker writes it, `GET /config` reads
    // it. Starts empty and unstamped — nothing blocks startup on Ollama, which is an
    // optional dependency, and `interval`'s first tick fires immediately anyway.
    let research_models: worker::ollama_catalog::SharedCatalog = Arc::new(
        tokio::sync::RwLock::new(worker::ollama_catalog::ModelCatalog::default()),
    );

    // Say at once whether any project's vectors are missing from the collection layout
    // this build looks in. Run here rather than only on the worker's tick because the
    // condition it reports is worst immediately after a deploy — a
    // COLLECTION_SCHEMA_VERSION bump breaks every search the moment the new binary
    // starts, and does it without failing anything.
    worker::stale::check_and_publish(
        &db_pool,
        &qdrant_client,
        &metrics,
        &sigterm_token.child_token(),
    )
    .await;

    let gc_token = sigterm_token.child_token();
    let retry_token = sigterm_token.child_token();

    supervise(
        "collection_check",
        &metrics,
        worker::stale::run(
            db_pool.clone(),
            qdrant_client.clone(),
            metrics.clone(),
            sigterm_token.child_token(),
        ),
    );

    supervise(
        "gc",
        &metrics,
        worker::gc::run(
            db_pool.clone(),
            qdrant_client.clone(),
            gc_flag.clone(),
            cfg.workers.gc_interval_seconds,
            cfg.workers.status_log_retention_days,
            metrics.clone(),
            gc_token,
        ),
    );

    supervise(
        "retry",
        &metrics,
        worker::retry::run(
            db_pool.clone(),
            qdrant_client.clone(),
            embed_client.clone(),
            model_id.to_string(),
            RetryTuning {
                embed: embed_tuning,
                retry_interval_seconds: cfg.workers.retry_interval_seconds,
                failed_warn_interval_seconds: cfg.workers.failed_warn_interval_seconds,
                max_retries: cfg.workers.max_retries,
                stuck_grace_secs: cfg.indexing.stuck_grace_minutes * 60,
            },
            indexing_locks.clone(),
            metrics.clone(),
            retry_token,
        ),
    );

    if cfg.metrics.enabled {
        supervise(
            "metrics",
            &metrics,
            worker::metrics::run(
                db_pool.clone(),
                metrics.clone(),
                worker::metrics::MetricsTuning {
                    refresh_interval_seconds: cfg.metrics.refresh_interval_seconds,
                    probe_dependencies: cfg.metrics.probe_dependencies,
                    max_retries: cfg.workers.max_retries,
                    model_id: cfg.model.name.clone(),
                },
                cfg.metrics.probe_dependencies.then(|| {
                    worker::metrics::ProbeTargets {
                        store: qdrant_client.clone(),
                        embedder: embed_client.clone(),
                        // Probed separately only when it *is* separate — the same
                        // `Arc::ptr_eq` rule `GET /health` uses, because comparing URLs
                        // would call one instance two things.
                        query_embedder: (!Arc::ptr_eq(&embed_client, &query_embed_client))
                            .then(|| query_embed_client.clone()),
                        ollama: research_ollama.clone(),
                    }
                }),
                research_semaphore.clone(),
                cfg.research.max_concurrent,
                sigterm_token.child_token(),
            ),
        );
    }

    // What a run at each level has actually cost, for `GET /config`. Shares the
    // model catalog's interval deliberately: both are the parts of `/config` that
    // are not static, and a second interval key would be a knob nobody tunes.
    let research_stats: worker::research_stats::SharedRunStats = Arc::new(
        tokio::sync::RwLock::new(worker::research_stats::RunStats::default()),
    );
    supervise(
        "research_stats",
        &metrics,
        worker::research_stats::run(
            db_pool.clone(),
            research_stats.clone(),
            cfg.research.models_refresh_interval_seconds,
            sigterm_token.child_token(),
        ),
    );

    // Unconditional on purpose, unlike the collector above: this is the backstop
    // that keeps a research slot from being held forever, and gating a safety
    // mechanism on `[metrics].enabled` would make an observability switch decide
    // whether the service can recover.
    supervise(
        "research_watchdog",
        &metrics,
        worker::research_watchdog::run(
            research_registry.clone(),
            metrics.clone(),
            sigterm_token.child_token(),
        ),
    );

    // Unconditional: an Ollama that comes up an hour from now must still be picked
    // up, so there is nothing to gate this on. A failed tick keeps the last list.
    supervise(
        "ollama_catalog",
        &metrics,
        worker::ollama_catalog::run(
            research_ollama.clone(),
            research_models.clone(),
            cfg.research.models_refresh_interval_seconds,
            sigterm_token.child_token(),
        ),
    );

    // Whichever arm fires first wins and we proceed to shutdown — there is no
    // looping (a server exit, SIGINT, or SIGTERM all end the process).
    //
    // The server future is pinned rather than consumed by the `select!` so a signal
    // can cancel it and then **wait for it**. Before, the signal arms cancelled the
    // token and the `select!` returned immediately: "Shutdown complete." was logged
    // while in-flight requests, the HTTP/3 acceptor and live research runs were still
    // being dropped mid-flight, which for an indexing batch means files left
    // `indexing` and for a research run means a client's stream cut without a
    // terminal event.
    let server = backend::http3::run(
        cfg.server.bind,
        (
            cfg.server.cert_path.as_path(),
            cfg.server.key_path.as_path(),
        ),
        RouterState {
            tokenizer: Arc::new(Tokenizer::from_pretrained(model_id, None)?),
            db_pool: db_pool.clone(),
            qdrant: qdrant_client.clone(),
            model: EmbeddingModel::BGEm3 {
                model_id: model_id.to_string(),
                client: embed_client.clone(),
            },
            query_model: query_embed_client.clone(),
            embed_tuning,
            min_chunk_tokens: cfg.slicer.min_chunk_tokens,
            max_chunk_tokens: cfg.slicer.max_chunk_tokens,
            fill_gaps: cfg.slicer.fill_gaps,
            max_doc_chunk_tokens: cfg.slicer.max_doc_chunk_tokens,
            doc_semantic_weight: cfg.slicer.doc_semantic_weight,
            default_top_k: cfg.search.default_top_k,
            max_top_k: cfg.search.max_top_k,
            max_query_bytes: cfg.search.max_query_bytes,
            max_code_bytes: cfg.limits.max_code_bytes,
            max_files_per_request: cfg.limits.max_files_per_request,
            max_drift_files: cfg.limits.max_drift_files,
            max_history_commits: cfg.limits.max_history_commits,
            max_commit_message_bytes: cfg.limits.max_commit_message_bytes,
            max_research_delete_ids: cfg.limits.max_research_delete_ids,
            max_selector_patterns: cfg.limits.max_selector_patterns,
            max_symbol_name_bytes: cfg.limits.max_symbol_name_bytes,
            max_symbol_results: cfg.limits.max_symbol_results,
            path_batch_size: cfg.indexing.path_batch_size,
            status_log_retention_days: cfg.workers.status_log_retention_days,
            max_retries: cfg.workers.max_retries,
            indexing_locks: indexing_locks.clone(),
            gc_flag: gc_flag.clone(),
            stuck_grace_mins: cfg.indexing.stuck_grace_minutes,
            db_pool_size: cfg.database.pool_size,
            db_schema_version,
            research_handle,
            research_semaphore: research_semaphore.clone(),
            research_max_concurrent: cfg.research.max_concurrent,
            research_registry: research_registry.clone(),
            research_stats: research_stats.clone(),
            research_ollama,
            research_default_model: cfg.research.default_model.clone(),
            research_allowed_models: config::AllowedModels::compile(&cfg.research.allowed_models)
                .expect("validated at startup"),
            research_effort: cfg.research.effort.clone(),
            research_max_request_seconds: cfg.research.max_request_seconds,
            research_max_request_tokens: cfg.research.max_request_tokens,
            research_max_request_steps: cfg.research.max_request_steps,
            research_max_request_report_sections: cfg.research.max_request_report_sections,
            research_max_request_report_words: cfg.research.max_request_report_words,
            research_max_evidence_width: cfg.research.max_evidence_width,
            research_report_timeout_ms: cfg.research.report_timeout_ms,
            research_checkpoint_every_steps: cfg.research.checkpoint_every_steps,
            research_max_turn_thinking_chars: cfg.research.max_turn_thinking_chars,
            research_max_turn_seconds: cfg.research.max_turn_seconds,
            research_retention_days: cfg.research.retention_days,
            research_max_context_runs: cfg.research.max_context_runs,
            research_max_context_chars: cfg.research.max_context_chars,
            research_list_page_limit: cfg.research.list_page_limit,
            research_sampling: models::ollama::Sampling {
                temperature: cfg.research.temperature,
                top_p: cfg.research.top_p,
                seed: cfg.research.seed,
                // Not config: `write_report` arms it from the effort level's
                // `max_report_words`, and only for the turn that writes the
                // report. Every other turn sends no `num_predict` at all.
                num_predict: None,
            },
            research_models: research_models.clone(),
            metrics: metrics.clone(),
        },
        cfg.server.max_body_mib * 1024 * 1024,
        cfg.server.http3,
        backend::http3::MetricsRouting {
            enabled: cfg.metrics.enabled,
            per_project_http_labels: cfg.metrics.per_project_http_labels,
        },
        sigterm_token.child_token(),
    );
    tokio::pin!(server);

    let signalled = tokio::select! {
        res = &mut server => {
            if let Err(err) = res {
                error!(
                    error = ?err,
                    bind = %cfg.server.bind,
                    "HTTP server exited with an error. \
                     Check the bind address is free and the TLS cert/key paths are valid."
                );
            }
            false
        }
        _ = signal::ctrl_c() => {
            info!("Received SIGINT. Shutting down...");
            sigterm_token.cancel();
            true
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM. Shutting down...");
            sigterm_token.cancel();
            true
        }
    };

    if signalled {
        // Bounded: a `high` research run may legitimately have an hour left, and a
        // supervisor that sent SIGTERM will send SIGKILL long before that. The point is
        // that the common case — a few in-flight requests and an indexing batch — gets
        // to finish and land its rows, instead of the process claiming to be done while
        // they are torn out from under it.
        match tokio::time::timeout(SHUTDOWN_DRAIN, &mut server).await {
            Ok(Ok(())) => info!("In-flight work drained."),
            Ok(Err(err)) => error!(error = ?err, "HTTP server errored while draining."),
            Err(_) => warn!(
                drain = ?SHUTDOWN_DRAIN,
                "Shutdown drain expired with work still in flight; exiting anyway. \
                 Sysadmin: an indexing batch may be left 'indexing' — the retry worker \
                 recovers it on the next start."
            ),
        }
    }

    info!("Shutdown complete.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite3::SQLite3PoolError;
    use std::path::Path;

    fn pool() -> SQLite3Pool {
        SQLite3Pool::new(Path::new(":memory:"), 1, 16384, "NORMAL")
    }

    /// Wait for `worker_running{worker}` to reach `want`, or give up. The supervisor
    /// bookkeeping happens in a spawned task, so there is no handle to await — but a
    /// bounded poll keeps the test from hanging if the gauge never moves.
    async fn await_running(
        metrics: &Arc<backend::metrics::Metrics>,
        worker: &'static str,
        want: i64,
    ) -> i64 {
        use backend::metrics::WorkerLabels;
        let gauge = metrics
            .supervisor
            .running
            .get_or_create(&WorkerLabels { worker })
            .clone();
        for _ in 0..200 {
            if gauge.get() == want {
                return want;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        gauge.get()
    }

    fn exits(
        metrics: &Arc<backend::metrics::Metrics>,
        worker: &'static str,
        outcome: &'static str,
    ) -> u64 {
        use backend::metrics::WorkerOutcomeLabels;
        metrics
            .supervisor
            .exits
            .get_or_create(&WorkerOutcomeLabels { worker, outcome })
            .get()
    }

    /// The liveness gauge must exist from the moment the worker is launched, not
    /// from its first successful tick. A worker that panics on its first tick would
    /// otherwise leave a series that was never written — and no alert can fire on a
    /// series that does not exist. All six workers were bare `tokio::spawn` with the
    /// `JoinHandle` dropped, so a panic stopped GC or the retry sweep permanently and
    /// in silence.
    #[tokio::test]
    async fn a_supervised_worker_is_marked_running_before_it_does_anything() {
        let metrics = Arc::new(backend::metrics::Metrics::new());
        let gate = Arc::new(tokio::sync::Notify::new());

        let held = Arc::clone(&gate);
        supervise("gate_keeper", &metrics, async move {
            held.notified().await;
        });

        assert_eq!(
            await_running(&metrics, "gate_keeper", 1).await,
            1,
            "a launched worker must publish worker_running = 1 immediately"
        );
        assert!(
            metrics
                .render()
                .expect("renders")
                .contains(r#"mindex_worker_running{worker="gate_keeper"} 1"#),
            "the gauge must be scrapeable, not merely set"
        );

        gate.notify_one();
        assert_eq!(await_running(&metrics, "gate_keeper", 0).await, 0);
        assert_eq!(exits(&metrics, "gate_keeper", "ok"), 1);
        assert_eq!(
            exits(&metrics, "gate_keeper", "panic"),
            0,
            "a clean exit must not be counted as a death"
        );
    }

    /// A panicking worker is the case this exists for: the gauge must fall to zero
    /// and the death must be counted, so "the retry sweep stopped happening" is
    /// visible from outside instead of looking like a healthy idle system.
    #[tokio::test]
    async fn a_worker_that_panics_falls_to_zero_and_is_counted_as_a_death() {
        let metrics = Arc::new(backend::metrics::Metrics::new());

        supervise("doomed", &metrics, async {
            panic!("a worker fell over");
        });

        assert_eq!(
            await_running(&metrics, "doomed", 0).await,
            0,
            "a dead worker still reports itself alive"
        );
        assert_eq!(
            exits(&metrics, "doomed", "panic"),
            1,
            "the death was not counted"
        );
        assert_eq!(
            exits(&metrics, "doomed", "ok"),
            0,
            "a panic was recorded as a clean exit"
        );
    }

    /// Supervision must not restart: a worker that panicked once panics again, and
    /// a restart loop buries the backtrace under its own noise. One death, one
    /// count, and the process keeps serving.
    #[tokio::test]
    async fn a_dead_worker_is_not_restarted() {
        let metrics = Arc::new(backend::metrics::Metrics::new());
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let counter = Arc::clone(&runs);
        supervise("never_again", &metrics, async move {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("down");
        });

        await_running(&metrics, "never_again", 0).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the supervisor restarted a worker it is documented never to restart"
        );
        assert_eq!(exits(&metrics, "never_again", "panic"), 1);
    }

    /// One worker dying must not touch any other worker's series — the gauge is
    /// per-worker, and a shared one would make the whole set unreadable the moment
    /// any single worker fell over.
    #[tokio::test]
    async fn one_worker_dying_leaves_the_others_alone() {
        let metrics = Arc::new(backend::metrics::Metrics::new());
        let gate = Arc::new(tokio::sync::Notify::new());

        let held = Arc::clone(&gate);
        supervise("survivor", &metrics, async move { held.notified().await });
        supervise("casualty", &metrics, async { panic!("down") });

        assert_eq!(await_running(&metrics, "casualty", 0).await, 0);
        assert_eq!(
            await_running(&metrics, "survivor", 1).await,
            1,
            "a healthy worker was marked dead by its neighbour's panic"
        );
        assert_eq!(exits(&metrics, "survivor", "panic"), 0);

        gate.notify_one();
    }

    /// `SupervisorMetrics` is deliberately **not** part of `StateMetrics`: the
    /// metrics worker clears and repopulates that whole group every tick, so a
    /// liveness gauge living there would be erased by the very tick that proves the
    /// worker alive — and its own death would then be indistinguishable from any
    /// other tick.
    #[tokio::test]
    async fn the_liveness_gauge_survives_a_state_metrics_tick() {
        let metrics = Arc::new(backend::metrics::Metrics::new());
        let gate = Arc::new(tokio::sync::Notify::new());

        let held = Arc::clone(&gate);
        supervise("metrics", &metrics, async move { held.notified().await });
        await_running(&metrics, "metrics", 1).await;

        let pool = pool();
        pool.transaction(CancellationToken::new(), |tx| {
            apply_pending_migrations(tx).map(|_| ())
        })
        .await
        .expect("migrations apply");

        worker::metrics::collect_once(
            &pool,
            &metrics,
            &worker::metrics::MetricsTuning {
                refresh_interval_seconds: 60,
                probe_dependencies: false,
                max_retries: 3,
                model_id: "BAAI/bge-m3".to_string(),
            },
            &CancellationToken::new(),
        )
        .await;

        assert!(
            metrics
                .render()
                .expect("renders")
                .contains(r#"mindex_worker_running{worker="metrics"} 1"#),
            "a state-metrics tick erased the gauge that says the collector is alive"
        );

        gate.notify_one();
    }

    async fn user_version(pool: &SQLite3Pool) -> i32 {
        pool.transaction(CancellationToken::new(), |tx| {
            tx.pragma_query_value(None, "user_version", |r| r.get(0))
                .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
    }

    async fn column_exists(pool: &SQLite3Pool, table: &'static str, column: &'static str) -> bool {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                rusqlite::params![table, column],
                |r| r.get::<_, i64>(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
            > 0
    }

    async fn object_exists(pool: &SQLite3Pool, kind: &'static str, name: &'static str) -> bool {
        pool.transaction(CancellationToken::new(), move |tx| {
            tx.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                rusqlite::params![kind, name],
                |r| r.get::<_, i64>(0),
            )
            .map_err(SQLite3PoolError::from)
        })
        .await
        .unwrap()
            > 0
    }

    #[tokio::test]
    async fn fresh_db_applies_all_migrations_and_stamps_user_version() {
        let p = pool();
        let (v, applied) = p
            .transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();

        let (max_v, _) = MIGRATIONS.last().unwrap();
        assert_eq!((v, applied), (*max_v, true));
        assert_eq!(user_version(&p).await, *max_v);

        // Every table the schema defines, plus one trigger from each of the two
        // trigger families. A statement silently dropped from the batch — the way a
        // bad merge loses one — shows up here and nowhere else.
        for table in [
            "projects",
            "project_files",
            "project_file_chunks",
            "project_file_status_log",
            "project_file_symbols",
            "research_runs",
            "research_run_files",
            "project_commits",
            "project_commit_paths",
        ] {
            assert!(
                object_exists(&p, "table", table).await,
                "table {table} missing from the schema"
            );
        }
        // Columns that used to be 1:1 side tables. A dropped table is loud (every
        // query naming it fails); a dropped column is quiet — the reads that would
        // catch it live behind an indexing run, not a unit test.
        for (table, column) in [
            ("project_files", "chunks_version"),
            ("project_files", "symbols_version"),
            ("research_runs", "changed_files"),
            ("research_runs", "notes_written"),
            ("research_runs", "scope_json"),
            // Migration 4 rebuilt this table; a rebuild that lost a column would be
            // silent until the first list request 500s with `no such column`.
            ("research_runs", "seq"),
            ("research_runs", "expires_at"),
            ("research_runs", "context_run_ids_json"),
            ("research_runs", "title"),
        ] {
            assert!(
                column_exists(&p, table, column).await,
                "{table}.{column} missing from the schema"
            );
        }
        assert!(
            object_exists(&p, "trigger", "project_files_status_update_guard").await,
            "the status state machine is missing"
        );
        assert!(
            object_exists(&p, "trigger", "project_files_sha256_insert_guard").await,
            "the shape-validation triggers are missing"
        );
    }

    #[tokio::test]
    async fn second_run_is_a_noop() {
        let p = pool();
        p.transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();
        let (v, applied) = p
            .transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();

        let (max_v, _) = MIGRATIONS.last().unwrap();
        assert_eq!(
            (v, applied),
            (*max_v, false),
            "an up-to-date DB must apply nothing"
        );
    }

    #[tokio::test]
    async fn db_already_at_max_version_is_trusted_and_untouched() {
        // The filter trusts user_version: a DB stamped at the max version gets no
        // migrations even if (hypothetically) its schema is empty.
        let p = pool();
        let (max_v, _) = *MIGRATIONS.last().unwrap();
        p.transaction(CancellationToken::new(), move |tx| {
            tx.pragma_update(None, "user_version", max_v)?;
            Ok(())
        })
        .await
        .unwrap();

        let (v, applied) = p
            .transaction(CancellationToken::new(), apply_pending_migrations)
            .await
            .unwrap();
        assert_eq!((v, applied), (max_v, false));
        assert!(
            !object_exists(&p, "table", "project_files").await,
            "nothing must be applied"
        );
    }

    #[tokio::test]
    async fn every_migration_sql_is_idempotent() {
        // The cold-start guarantee: re-running any batch on a DB that already has it
        // must be a no-op (all SQL uses IF NOT EXISTS), never an error.
        let p = pool();
        p.transaction(CancellationToken::new(), |tx| {
            for (_, sql) in MIGRATIONS {
                tx.execute_batch(sql)?;
            }
            for (v, sql) in MIGRATIONS {
                tx.execute_batch(sql).unwrap_or_else(|e| {
                    panic!("migration {v} is not idempotent (re-run failed): {e}")
                });
            }
            Ok(())
        })
        .await
        .unwrap();
    }

    /// Migration 4 rebuilds `research_runs` while `research_run_files` already
    /// references it, which the test above cannot exercise: it starts from an empty
    /// database, where a rebuild that dropped every child row would still pass.
    ///
    /// The rebuild is safe only because migrations run with foreign keys suspended
    /// (`SQLite3Pool::migration_transaction`) — which suspends the child's
    /// `ON DELETE CASCADE` too, so the `DROP TABLE` does not take the baselines with
    /// it, and `id` is preserved by the copy so they still point at their run. That
    /// is a load-bearing accident of the FK suspension, and this is the test that
    /// notices if it stops being true.
    #[tokio::test]
    async fn rebuilding_research_runs_keeps_the_baselines_that_reference_it() {
        let p = pool();
        // `migration_transaction`, not `transaction`: the FK suspension is the whole
        // mechanism under test. Under an ordinary transaction the `DROP TABLE` fires
        // the child's `ON DELETE CASCADE` and every baseline is silently erased —
        // which is precisely the failure this pins, and why a migration must never be
        // applied through the ordinary path.
        p.migration_transaction(CancellationToken::new(), |tx| {
            for (_, sql) in MIGRATIONS {
                tx.execute_batch(sql)?;
            }
            tx.execute_batch(
                "INSERT INTO research_runs (
                     id, project_guid, seq, question, model, prompt_version, effort,
                     granted_seconds, granted_tokens, granted_steps, granted_search_top_k,
                     done_reason, steps, turns, elapsed_ms,
                     prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx,
                     citations_total, citations_verified, citations_path_only,
                     citations_unverified, cited_paths_json, unverified_paths_json,
                     changed_files, removed_files, stale_citations, stale_paths_json,
                     notes_written, notes_rejected, plan_revisions, grep_calls, grep_hits,
                     out_of_scope_refusals, out_of_scope_rows, scoped,
                     forced_synthesis, report_window_ms, report_elapsed_ms, report
                 ) VALUES (
                     'run-1', 'p1', 1, 'q', 'm', '1.2', 'medium',
                     1, 1, 1, 1,
                     'finalized', 1, 1, 1,
                     1, 1, 1, 1,
                     0, 0, 0,
                     0, '[]', '[]',
                     0, 0, 0, '[]',
                     0, 0, 0, 0, 0,
                     0, 0, 0,
                     0, 0, 0, 'report'
                 );
                 INSERT INTO research_run_files (run_id, path, sha256) VALUES
                     ('run-1', 'src/a.rs', '\
                      0000000000000000000000000000000000000000000000000000000000000001');
                 INSERT INTO research_run_evidence (run_id, path, spans_json) VALUES
                     ('run-1', 'src/a.rs', '[[1,2]]');
                 INSERT INTO research_run_citations
                     (run_id, ord, path, start_line, end_line, verdict, stale)
                 VALUES ('run-1', 0, 'src/a.rs', 1, 2, 'verified', 0);
                 INSERT INTO research_run_steps
                     (run_id, n, phase, action, argument, hits, spans_json,
                      spans_truncated, at_ms)
                 VALUES ('run-1', 1, 'main', 'grep', 'x', 1, '[]', 0, 5);",
            )?;

            // The whole batch again, as a cold re-run would apply it.
            for (v, sql) in MIGRATIONS {
                tx.execute_batch(sql)
                    .unwrap_or_else(|e| panic!("migration {v} failed on a populated DB: {e}"));
            }

            let runs: i64 = tx.query_row("SELECT COUNT(*) FROM research_runs", [], |r| r.get(0))?;
            let files: i64 =
                tx.query_row("SELECT COUNT(*) FROM research_run_files", [], |r| r.get(0))?;
            assert_eq!(runs, 1, "the rebuild lost the run");
            assert_eq!(files, 1, "the rebuild orphaned or dropped the baselines");
            // Migration 5's own children survive its rebuild the same way the
            // baselines survive migration 4's — via the FK suspension.
            for child in [
                "research_run_evidence",
                "research_run_citations",
                "research_run_steps",
            ] {
                let n: i64 =
                    tx.query_row(&format!("SELECT COUNT(*) FROM {child}"), [], |r| r.get(0))?;
                assert_eq!(n, 1, "the rebuild dropped {child} rows");
            }

            // And they still join: `id` survives the copy, so the child still resolves.
            let joined: i64 = tx.query_row(
                "SELECT COUNT(*) FROM research_run_files f
                   JOIN research_runs r ON r.id = f.run_id",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(joined, 1, "baselines no longer point at their run");

            let violations: i64 =
                tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(violations, 0, "the rebuild left dangling references");
            Ok(())
        })
        .await
        .unwrap();
    }

    /// The payback for suspending foreign keys during a rebuild. A migration that
    /// orphaned a chunk or a symbol row must **roll the whole batch back** and leave
    /// `user_version` alone, so the next start retries it — reaching a running
    /// server, the damage is silent: the FK is `RESTRICT` precisely because nothing
    /// else notices a chunk whose file is gone.
    ///
    /// There is no way to exercise this with the real list, which by construction
    /// passes, so `apply_migrations_from` takes the list.
    #[tokio::test]
    async fn a_migration_that_orphans_a_row_is_rolled_back_and_not_stamped() {
        let pool = pool();

        // A first migration builds a parent/child pair; a second deletes the parent
        // out from under the child. With enforcement suspended the DELETE succeeds,
        // and only the check at the end can catch it.
        const BROKEN: &[(i32, &str)] = &[
            (
                1,
                "CREATE TABLE IF NOT EXISTS parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE IF NOT EXISTS child (
                     id        INTEGER PRIMARY KEY,
                     parent_id INTEGER NOT NULL
                         REFERENCES parent(id) ON DELETE RESTRICT
                 );
                 INSERT INTO parent (id) SELECT 1 WHERE NOT EXISTS
                     (SELECT 1 FROM parent WHERE id = 1);
                 INSERT INTO child (id, parent_id) SELECT 1, 1 WHERE NOT EXISTS
                     (SELECT 1 FROM child WHERE id = 1);",
            ),
            (2, "DELETE FROM parent WHERE id = 1;"),
        ];

        let res = pool
            .migration_transaction(CancellationToken::new(), |tx| {
                apply_migrations_from(tx, BROKEN).map(|_| ())
            })
            .await;

        assert!(
            res.is_err(),
            "a migration that orphaned a row was allowed to commit"
        );
        assert_eq!(
            user_version(&pool).await,
            0,
            "the schema version was stamped for a batch that rolled back, so the \
             next start would skip the migration that never applied"
        );
        // The rollback is total: not even the first migration's tables survive.
        let tables: i64 = pool
            .transaction(CancellationToken::new(), |tx| {
                tx.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                      WHERE type = 'table' AND name IN ('parent', 'child')",
                    [],
                    |r| r.get(0),
                )
                .map_err(SQLite3PoolError::from)
            })
            .await
            .expect("read");
        assert_eq!(tables, 0, "the batch did not roll back as one");
    }

    /// The same list, without the migration that breaks it, must commit and stamp —
    /// so the test above is showing a refusal rather than a batch that could never
    /// have worked.
    #[tokio::test]
    async fn the_same_batch_without_the_breaking_step_commits_and_stamps() {
        let pool = pool();
        const SOUND: &[(i32, &str)] = &[(
            1,
            "CREATE TABLE IF NOT EXISTS parent (id INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS child (
                 id        INTEGER PRIMARY KEY,
                 parent_id INTEGER NOT NULL
                     REFERENCES parent(id) ON DELETE RESTRICT
             );
             INSERT INTO parent (id) SELECT 1 WHERE NOT EXISTS
                 (SELECT 1 FROM parent WHERE id = 1);
             INSERT INTO child (id, parent_id) SELECT 1, 1 WHERE NOT EXISTS
                 (SELECT 1 FROM child WHERE id = 1);",
        )];

        pool.migration_transaction(CancellationToken::new(), |tx| {
            apply_migrations_from(tx, SOUND).map(|_| ())
        })
        .await
        .expect("a sound migration commits");

        assert_eq!(user_version(&pool).await, 1);
    }
}
