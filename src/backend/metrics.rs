//! Prometheus/OpenMetrics instrumentation.
//!
//! One owned [`Registry`] threaded as `Arc<Metrics>` through constructors — the
//! same rule config follows (never a global). That is also what lets two unit
//! tests in one binary each own an independent metric set, which a process-wide
//! recorder could not.
//!
//! Two things about this crate that shape the code below and the tests:
//! `prometheus-client` appends `_total` to counter names itself (a family
//! registered as `http_requests` is exposed as `mindex_http_requests_total`), and
//! `encoding::text::encode` writes OpenMetrics, `# EOF` terminator included — so
//! the body must be served as [`CONTENT_TYPE`], not `text/plain`.
//!
//! **Cardinality rule.** Every label value comes from a set the *server* defines:
//! `MatchedPath` (router-owned), [`ApiError::code`](crate::backend::error::ApiError::code),
//! `ProgrammingLanguage::name`, `DoneReason::as_str`, the tool names, the file
//! statuses. `project_guid` is the sole open-ended label; it is UUID-validated
//! before it becomes one, and on the HTTP families it is off by default. Never a
//! raw URI, path, query, or model-supplied string without a bound.

use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets, linear_buckets};
use prometheus_client::registry::Registry;

/// The exposition content type. OpenMetrics, because `encode` emits `# EOF`;
/// serving that as `text/plain` is a parse error at a strict scraper and a
/// success at build time, which is the worst possible ordering.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

// ─── Bucket sets ─────────────────────────────────────────────────────────────
//
// Chosen per shape rather than shared: a SQLite transaction and a research run
// differ by six orders of magnitude, and one bucket set spanning both resolves
// neither.

/// 1 ms → ~33 s. HTTP requests, index phases, Qdrant ops, search stages.
fn request_hist() -> Histogram {
    Histogram::new(exponential_buckets(0.001, 2.0, 16))
}

/// 100 µs → ~6 s. SQLite transactions, which are meant to be short.
fn db_hist() -> Histogram {
    Histogram::new(exponential_buckets(0.0001, 3.0, 10))
}

/// 0.5 s → ~34 min. Research runs, GC passes and embed calls, which are not.
fn long_hist() -> Histogram {
    Histogram::new(exponential_buckets(0.5, 2.0, 12))
}

/// 256 B → ~16 MiB, the `[limits].max_code_bytes` neighbourhood.
fn size_hist() -> Histogram {
    Histogram::new(exponential_buckets(256.0, 4.0, 9))
}

/// 1 → 8192. Chunks per file, embed batch sizes, candidates, results, steps.
fn count_hist() -> Histogram {
    Histogram::new(exponential_buckets(1.0, 2.0, 14))
}

/// 0.0 → 1.0 in tenths.
fn ratio_hist() -> Histogram {
    Histogram::new(linear_buckets(0.0, 0.1, 11))
}

/// Shorthand for a histogram family. The constructor is a plain function
/// pointer (the `Family` default) rather than a boxed closure, because a boxed
/// closure is not `Clone` and every group here has to be.
type HistFamily<S> = Family<S, Histogram>;

fn hist_family<S: Clone + std::hash::Hash + Eq>(ctor: fn() -> Histogram) -> HistFamily<S> {
    Family::new_with_constructor(ctor)
}

// ─── Label sets ──────────────────────────────────────────────────────────────
//
// This block *is* the cardinality audit: every label in the system is on one
// screen. Keep it that way.

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RequestLabels {
    pub route: String,
    pub method: &'static str,
    pub status: u16,
    /// The `ApiError` code, empty on success.
    pub code: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RouteLabels {
    pub route: String,
    pub method: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RouteProjectLabels {
    pub route: String,
    pub project_guid: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProtoLabels {
    pub proto: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProjectLabels {
    pub project_guid: String,
}

/// The worker's own name. Bounded by the supervised set in `main.rs`, which is a
/// literal list — the cardinality rule holds by construction.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct WorkerLabels {
    pub worker: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct WorkerOutcomeLabels {
    pub worker: &'static str,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProjectLangLabels {
    pub project_guid: String,
    pub language: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProjectLangOutcomeLabels {
    pub project_guid: String,
    pub language: &'static str,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct LangLabels {
    pub language: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct PhaseLabels {
    pub phase: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct EmbedderLabels {
    /// `index` or `query` — the two BGE-M3 instances, which may be one server.
    pub embedder: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct EmbedderOutcomeLabels {
    pub embedder: &'static str,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OpLabels {
    pub op: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OpOutcomeLabels {
    pub op: &'static str,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct StageLabels {
    pub stage: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProjectOutcomeLabels {
    pub project_guid: String,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProjectClassLabels {
    pub project_guid: String,
    pub class: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ProjectStatusLabels {
    pub project_guid: String,
    pub status: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModelLabels {
    pub model: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModelReasonLabels {
    pub model: String,
    pub done_reason: &'static str,
}

/// A research run that produced no journal row, and why. Deliberately not folded
/// into `ModelReasonLabels`: `done_reason` describes how a run that *finished*
/// finished, and these are the runs that never got one.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModelOutcomeLabels {
    pub model: String,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModelEffortLabels {
    pub model: String,
    pub effort: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModelKindLabels {
    pub model: String,
    /// `prompt` or `eval`.
    pub kind: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ToolOutcomeLabels {
    pub tool: &'static str,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ClassLabels {
    pub class: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TriggerOutcomeLabels {
    pub trigger: &'static str,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OutcomeLabels {
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DependencyLabels {
    pub dependency: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildLabels {
    pub version: &'static str,
    pub db_schema_version: String,
    pub model_id: String,
}

// ─── Metric groups ───────────────────────────────────────────────────────────
//
// Field names carry no metric-name prefix: the registered name is the contract,
// and prefixed fields trip clippy's `struct_field_names`. Every group is `Clone`
// (the handles are internally `Arc`-backed), so a consumer can take just its own.

#[derive(Clone)]
pub struct HttpMetrics {
    pub requests: Family<RequestLabels, Counter>,
    pub duration: HistFamily<RouteLabels>,
    pub in_flight: Family<RouteLabels, Gauge>,
    /// Only written when `[metrics].per_project_http_labels` is on. Deliberately
    /// carries neither status nor code, so projects never multiply the expensive
    /// dimensions.
    pub by_project: Family<RouteProjectLabels, Counter>,
    pub by_proto: Family<ProtoLabels, Counter>,
}

#[derive(Clone)]
pub struct IndexMetrics {
    pub files: Family<ProjectLangOutcomeLabels, Counter>,
    pub chunks: Family<ProjectLangLabels, Counter>,
    pub symbols: Family<ProjectLangLabels, Counter>,
    pub code_bytes: Family<ProjectLangLabels, Counter>,
    /// Language only, no project: a histogram is 13-19 exposition lines and
    /// multiplying that by the project count buys a breakdown nobody reads.
    pub file_size: HistFamily<LangLabels>,
    pub file_chunks: HistFamily<LangLabels>,
    pub phase_duration: HistFamily<PhaseLabels>,
    /// `IndexClaim` contention. Counted where the error is swallowed — the HTTP
    /// middleware can never see it, since the request still 200s.
    pub claim_conflicts: Counter,
    pub drift_checks: Family<ProjectLabels, Counter>,
    /// Counters, not gauges: `/drift` compares against a *client-posted* manifest,
    /// so there is no server-side drift level to gauge. See the module docs in
    /// `worker/metrics.rs`.
    pub drift_files: Family<ProjectClassLabels, Counter>,
}

#[derive(Clone)]
pub struct EmbedMetrics {
    pub requests: Family<EmbedderOutcomeLabels, Counter>,
    pub duration: HistFamily<EmbedderLabels>,
    pub batch_size: HistFamily<EmbedderLabels>,
    pub texts: Family<EmbedderLabels, Counter>,
    /// Incremented inside `BGEm3HttpClient::encode`: from outside the client,
    /// three retries then a success is indistinguishable from one success.
    pub retries: Family<EmbedderLabels, Counter>,
}

#[derive(Clone)]
pub struct QdrantMetrics {
    pub ops: Family<OpOutcomeLabels, Counter>,
    pub duration: HistFamily<OpLabels>,
    pub points: Family<OpLabels, Counter>,
}

#[derive(Clone)]
pub struct SearchMetrics {
    pub requests: Family<ProjectOutcomeLabels, Counter>,
    pub stage_duration: HistFamily<StageLabels>,
    pub candidates: Histogram,
    pub results: Histogram,
    /// Winners Qdrant scored whose SQLite chunk row was gone by the time the second
    /// query ran. The response just got shorter, silently, and if *every* winner was
    /// one the caller got a 200 with an empty list — the opposite spelling of the
    /// "nothing" an over-narrow filter gets (404 `search.no_match`), and the one that
    /// actually means the two stores disagree. Benign in small numbers (a reindex
    /// soft-deleted the chunk between this request's two queries); a sustained rate is
    /// divergence.
    pub orphaned_winners: Counter,
    /// Winners the reranker scored `NaN`. `total_cmp` orders `+NaN` above every
    /// finite value, so before they were ranked *last* these took the top result
    /// slot — the one an agent reads and a human trusts. The producer is not
    /// hypothetical: the embedder's XPU backend returns NaN for padded fp16 rows on
    /// its default attention kernel and still answers 200, and so does a split
    /// deployment whose two instances differ in precision. Expected to stay at zero;
    /// any value at all means the embedding path is misconfigured, and the symptom
    /// without this counter reads as a ranking-quality complaint.
    pub unscorable_winners: Counter,
}

#[derive(Clone)]
pub struct ResearchMetrics {
    /// Split from `runs_by_effort` rather than crossed with it: `model` is a
    /// client-supplied string, and effort × done_reason × model is the one
    /// genuine cardinality hazard in the set.
    pub runs: Family<ModelReasonLabels, Counter>,
    /// Runs that ended without a journal row, by why: `cancelled` (client hung up,
    /// `DELETE /research/active`, or the watchdog), `failed` (an `error` event went
    /// out instead of a report) and `report_rejected` (a report that failed the
    /// markdown gate — streamed to the caller, deliberately not stored).
    ///
    /// Every per-run research metric lives in the `MeteredJournal` decorator, so a run
    /// that never journals is absent from **all** of them: `runs`, `duration`, `steps`,
    /// `tokens`, `citations`. The GPU hour was spent and nothing on the dashboard knew
    /// it happened, which also makes every success rate computed from `runs` a rate
    /// with no denominator. Rare per label set — chart with `increase()`.
    pub unjournalled: Family<ModelOutcomeLabels, Counter>,
    pub runs_by_effort: Family<ModelEffortLabels, Counter>,
    pub duration: HistFamily<ModelLabels>,
    pub steps: HistFamily<ModelLabels>,
    pub turns: HistFamily<ModelLabels>,
    /// Per-turn generation rate, tokens per second of *generation* time.
    ///
    /// The pair below is here to answer one question the service could not answer
    /// at all: when a run crawls, is the model slow or is the GPU busy? On this host
    /// the device is shared, so the second is the common case and looked exactly
    /// like the first — a measured run spent 985 s at ~1.5 tok/s and read as a
    /// wedged model. Rate alone still cannot distinguish them, which is why
    /// `turn_load_seconds` ships beside it: a non-zero load after the run's first
    /// turn means Ollama evicted and reloaded the model, i.e. something else wanted
    /// the device.
    ///
    /// Per *turn*, so unlike the per-run families these are not rare — `rate()` is
    /// fine here.
    pub turn_tokens_per_second: HistFamily<ModelLabels>,
    /// Seconds Ollama spent loading the model into the device for one turn.
    pub turn_load_seconds: HistFamily<ModelLabels>,
    /// Seconds of one turn's wall clock that Ollama did not account for.
    ///
    /// The third of the set, and the one that closes it. The two above are read from
    /// Ollama's own timings, which are taken *inside* its handler — so a request
    /// queued in front of a busy GPU is absent from both, and during exactly the
    /// contention they were built to find they report a healthy model. Measured on
    /// this host: a 912-second turn while every one of the preceding week's 220
    /// turns sat between 32 and 128 tok/s. This is where those 890 seconds were, and
    /// until it existed they were in no metric at all.
    ///
    /// Small values are transport (HTTP, TLS, NDJSON) and mean nothing. A large one
    /// means the request waited.
    pub turn_unaccounted_seconds: HistFamily<ModelLabels>,
    pub tokens: Family<ModelKindLabels, Counter>,
    pub context_used: HistFamily<ModelLabels>,
    pub tool_calls: Family<ToolOutcomeLabels, Counter>,
    pub tool_duration: HistFamily<ToolOutcomeLabels>,
    pub citations: Family<ClassLabels, Counter>,
    /// Challenge runs by their overall verdict (`confirmed`/`disputed`/
    /// `refuted`/`inconclusive`) — a server-defined closed set, so the
    /// cardinality rule holds. Rare by nature: chart with `increase()`, never
    /// `rate()` (the rare-counter rule in CLAUDE.md).
    pub challenges: Family<OutcomeLabels, Counter>,
    /// Challenge verdicts the grounding cap downgraded — an ungrounded `refuted`
    /// resolved to `disputed`, or an ungrounded `confirmed` to inconclusive.
    ///
    /// It exists because the cap is the one safety property of the whole challenge
    /// mechanism that leaves no trace: the verdict it *would* have returned is not
    /// stored, so a run whose accusation was capped is indistinguishable in the
    /// journal from one that genuinely disputed. Unlabelled — the two directions
    /// share a counter deliberately, since the question this answers is "does the
    /// cap ever fire on this hardware", not which way. Rarer than
    /// `research_challenges_total`, so the same rule applies twice over:
    /// `increase()`, never `rate()`.
    pub challenge_verdict_caps: Counter,
    /// Earlier challenges evicted because a newer challenge of the same report
    /// reached a verdict (the "one challenge per report, newest verdict wins"
    /// rule in `db::research::insert_run`).
    ///
    /// The eviction is destructive and leaves no other trace: the deleted row and
    /// its verdict are gone, and a subject that was `refuted` and is now
    /// `confirmed` looks exactly like one challenged once. This is the only answer
    /// to "how often is a standing verdict being overwritten here". Unlabelled —
    /// the subject id would be unbounded cardinality, and the question is a rate,
    /// not a list. Rare: `increase()`, never `rate()`.
    pub challenges_replaced: Counter,
    pub revalidations: Counter,
    /// Reports the *server* wrote because the report window expired before the model
    /// produced one. The operational symptom of `[research].report_timeout_ms` set too
    /// tight, and the one thing a dashboard needs from this generation's changes: the
    /// rest of what a run did with the new tools is a per-run question and lives in
    /// the journal.
    pub forced_syntheses: Counter,
    pub parse_retries: Counter,
    /// Ollama trimmed an over-long prompt and streamed on. Today the only other
    /// symptom is one `warn!`.
    pub truncations: Counter,
    /// Turns abandoned because the model ran away in its thinking channel. Counted
    /// here because the abandonment is returned as an ordinary empty reply — the one
    /// shape every caller already recovers from — so nothing downstream can tell it
    /// apart from a model that simply said nothing.
    pub runaway_thinking_turns: Counter,
    /// Turns abandoned for passing `[research].max_turn_seconds` while still
    /// streaming — i.e. generating far too slowly to finish anything.
    ///
    /// Unlike its neighbours this is **not** expected to stay at zero on a host whose
    /// GPU is shared: it counts the event that twice cost a whole run here. A rise
    /// means contention, not a code defect; read it against
    /// `research_turn_unaccounted_seconds`.
    pub stalled_turns: Counter,
    /// Runs that were given at least one earlier report as context, by model.
    ///
    /// The question this answers is "does prior-research context get used, and with
    /// which models" — the only thing that can justify keeping the feature or tuning
    /// its caps. Labelled by model rather than crossed with anything: a run either had
    /// context or did not, and the model is the axis the caps are felt through.
    pub runs_with_context: Family<ModelLabels, Counter>,
    /// Earlier reports injected, summed over runs. Unlabelled — the interesting
    /// number is the total against `runs_with_context`, i.e. how many reports a
    /// typical run is given, and a label would only split a sum nobody reads apart.
    pub context_runs_used: Counter,
    /// Context blocks that hit `[research].max_context_chars` and had their last
    /// report truncated. Without it that cap is untunable from evidence — the same
    /// argument `report_window_ms` granted-vs-taken makes.
    pub context_truncations: Counter,
    /// Report turns whose reply reached `num_predict` and was therefore cut rather
    /// than finished.
    ///
    /// Expected to stay at **zero**: the ceiling is sized ~3x the honest prose
    /// ratio so a report that merely overshoots its word budget never meets it.
    /// Any non-zero value means `REPORT_WORDS_TO_TOKENS` or the model is wrong, and
    /// that a cut landed mid-token — which can sever a code fence and cost a
    /// full-volume rewrite. Unlabelled: the actionable fact is that it happened at
    /// all.
    pub report_length_caps: Counter,
    /// Report turns whose assembled prompt was over the context ceiling, so the
    /// server dropped old tool output to fit.
    ///
    /// The alternative is Ollama trimming the same transcript in silence. Measured
    /// on this host a run's peak prompt was ~12k against a 65k window, so this may
    /// legitimately stay at zero forever — in which case the shed path is insurance
    /// and should be described as such rather than as a mechanism.
    pub report_context_sheds: Counter,
    /// Words in the report a run actually shipped, by model.
    ///
    /// The measurement `max_report_words` exists to produce: granted-versus-actual
    /// is the only thing that says whether announcing a length ceiling changes what
    /// a model writes. If the two turn out uncorrelated, the prompt half of that
    /// knob is dead weight and only `num_predict` earns its place. Labelled by
    /// model because the answer is certainly per-model.
    pub report_words: HistFamily<ModelLabels>,
    /// Sections of a sectioned report, by what became of each one:
    /// `written` / `empty` / `timed_out` / `skipped` — a set the server defines, so
    /// the cardinality rule holds.
    ///
    /// The point of writing in sections is that one failing costs a section rather
    /// than the document; this is what says how often that trade is actually being
    /// made. A rising `empty` share means the per-section word budget or the model
    /// is wrong; `timed_out`/`skipped` mean `report_timeout_ms` is too tight for the
    /// number of sections the plans are producing.
    pub report_sections: Family<OutcomeLabels, Counter>,
    /// Runs the watchdog cancelled because they outlived
    /// `max_seconds + report_timeout_ms` and were therefore holding a
    /// `max_concurrent` slot no deadline of their own was going to free.
    ///
    /// Expected to stay at **zero**, like `report_length_caps`: every phase of a run
    /// is already bounded by a token, so a non-zero value means one of the awaits
    /// that is *not* under a token has wedged (Ollama's `/api/show`, its error-body
    /// read, or the journal write) and names the day it started happening.
    /// Unlabelled — the actionable fact is that it happened at all.
    pub watchdog_cancels: Counter,
}

#[derive(Clone)]
pub struct GcMetrics {
    pub runs: Family<TriggerOutcomeLabels, Counter>,
    pub duration: Histogram,
    pub chunks_removed: Counter,
    pub files_pruned: Counter,
    pub status_log_pruned: Counter,
    /// Stored research runs reaped for having passed their `expires_at`. Pinned runs
    /// (`expires_at IS NULL`) are unreachable by the sweep and never counted here.
    pub research_pruned: Counter,
    pub running: Gauge,
}

/// Liveness of the background workers, which are otherwise unobservable: each is a
/// detached `tokio::spawn` whose `JoinHandle` used to be dropped, so a panic stopped
/// GC or the retry sweep permanently and in total silence — the only symptom was
/// some *other* gauge slowly ceasing to move, months later.
///
/// Not part of [`StateMetrics`]: that family set is cleared and repopulated whole by
/// the metrics worker, and a liveness gauge belonging to the metrics worker itself
/// would be erased by the very tick that proves it is alive.
#[derive(Clone)]
pub struct SupervisorMetrics {
    /// `1` while the worker's task is running, `0` once it has left. Every supervised
    /// worker publishes `0` before it starts, so a series exists to alert on from the
    /// first scrape — a worker that panicked on its first tick is otherwise absent
    /// rather than zero, and absence is what a dashboard cannot see.
    pub running: Family<WorkerLabels, Gauge>,
    /// Task exits, by how they ended. A clean shutdown ends every worker, so this is
    /// read together with `running`: `outcome="panic"` at any time, or `outcome="ok"`
    /// while the process is still serving, is the defect.
    pub exits: Family<WorkerOutcomeLabels, Counter>,
}

/// The collection-layout check ([`crate::worker::stale`]).
///
/// Not part of [`StateMetrics`] for the same reason as [`SupervisorMetrics`]: that set
/// is cleared and repopulated whole by the metrics worker, and these are written by a
/// different worker on a different cadence — a tick of one would erase the other's
/// findings and read as "all clear".
///
/// Both are `-1` until the first successful check, never `0`. Zero is the healthy
/// value here, so a check that could not run must not be able to spell it: an
/// unreachable Qdrant would otherwise publish the all-clear.
#[derive(Clone)]
pub struct CollectionMetrics {
    /// Projects holding active chunks whose current-version collection is missing or
    /// empty — i.e. projects whose search silently answers nothing. The alarm for a
    /// `COLLECTION_SCHEMA_VERSION` bump that was never followed by a reindex, and for
    /// a lost Qdrant volume.
    pub stale: Gauge,
    /// Collections present under a *previous* schema version. Not an error — they are
    /// the pre-bump store, still holding every byte of it — but nothing else can see
    /// them: SQLite records no layout, so this is the only number that says how much
    /// disk is waiting to be reclaimed by hand.
    pub orphaned: Gauge,
}

#[derive(Clone)]
pub struct RetryMetrics {
    pub sweeps: Counter,
    pub files: Family<OutcomeLabels, Counter>,
}

#[derive(Clone)]
pub struct DbMetrics {
    pub transaction_duration: Histogram,
    pub transactions: Family<OutcomeLabels, Counter>,
    /// `PoolEmpty`, which collapses into an opaque `ApiError::Internal` on the
    /// wire and is otherwise invisible.
    pub pool_acquire_failures: Counter,
    pub pool_size: Gauge,
    pub pool_available: Gauge,
}

/// Gauges describing *current state*, owned exclusively by
/// [`crate::worker::metrics`].
///
/// Nothing in a handler writes to this group, and every family in it is a gauge.
/// Both facts are load-bearing: the collector `clear()`s each family every tick
/// and repopulates it from a fresh query, because a `Family` retains a label set
/// for the life of the process — a deleted project would otherwise report its
/// last known file count forever. Clearing a *counter* would read as a process
/// restart to Prometheus and permanently re-baseline every `rate()` over it,
/// which is why counters never live here.
#[derive(Clone)]
pub struct StateMetrics {
    pub project_files: Family<ProjectStatusLabels, Gauge>,
    pub project_files_by_language: Family<ProjectLangLabels, Gauge>,
    pub project_chunks_active: Family<ProjectLangLabels, Gauge>,
    /// The GC backlog. Language is cut deliberately — nobody dashboards a
    /// backlog by language, and it halves the family.
    pub project_chunks_deleted: Family<ProjectLabels, Gauge>,
    pub project_symbols: Family<ProjectLabels, Gauge>,
    pub project_last_indexed: Family<ProjectLabels, Gauge>,
    pub project_files_permanently_failed: Family<ProjectLabels, Gauge>,
    pub projects: Gauge,
    pub db_size_bytes: Gauge,
    pub status_log_rows: Gauge,
    /// Stored research runs per project, and the two cuts a reader actually asks for:
    /// how many are pinned (exempt from the retention sweep) and how many describe a
    /// tree that has since moved. Split into three families rather than crossed with a
    /// `state` label because a run can be pinned *and* stale, so one labelled family
    /// would have to double-count or pick a winner.
    pub project_research_runs: Family<ProjectLabels, Gauge>,
    pub project_research_pinned: Family<ProjectLabels, Gauge>,
    pub project_research_stale: Family<ProjectLabels, Gauge>,
    pub dependency_up: Family<DependencyLabels, Gauge>,
    /// Points Qdrant holds for the project, against `project_chunks_active`'s sum from
    /// SQLite. The pair is the only detector for the failure `db/qdrant.rs` documents
    /// and nothing catches: lose Qdrant's volume (or bump `COLLECTION_SCHEMA_VERSION`)
    /// and SQLite still calls every file `indexed` while search answers
    /// `404 search.no_match` for ever, with no error logged anywhere. Written only when
    /// `[metrics].probe_dependencies` is on — it costs one Qdrant round-trip per
    /// project per tick — and absent, not zero, when the store cannot answer.
    pub project_vectors: Family<ProjectLabels, Gauge>,
    /// Unix time of the last **successful** state snapshot. A failed read keeps the
    /// previous gauges, deliberately, but that used to be indistinguishable from a
    /// healthy tick: every `StateMetrics` value would sit frozen at its last good
    /// number with nothing saying so. `time() - this` is the staleness a dashboard
    /// alerts on.
    pub state_refreshed_at: Gauge,
    // ── Process state, refreshed on scrape rather than on tick (all free reads) ──
    pub indexing_claims: Gauge,
    pub research_active: Gauge,
    pub research_permits_available: Gauge,
    /// Age of the longest-running live research run, `0` when none is running.
    ///
    /// `research_active` says a slot is taken; only this says whether it has been
    /// taken for four minutes or four hours — the difference between a queue and a
    /// wedge, and the number the `/health` verdict and the watchdog both act on.
    pub research_inflight_oldest_age_seconds: Gauge,
    pub research_worker_threads: Gauge,
    pub build_info: Family<BuildLabels, Gauge>,
    pub start_time: Gauge,
}

// ─── The registry ────────────────────────────────────────────────────────────

/// Every metric mindex exposes, plus the registry that renders them.
///
/// Always constructed and always written into: `[metrics].enabled` gates whether
/// the endpoint is *served* and whether the collector runs, never whether a
/// counter is incremented. A counter increment is a relaxed atomic add, and the
/// alternative is an `Option` check at sixty call sites.
pub struct Metrics {
    registry: Registry,
    pub http: HttpMetrics,
    pub index: IndexMetrics,
    pub embed: EmbedMetrics,
    pub qdrant: QdrantMetrics,
    pub search: SearchMetrics,
    pub research: ResearchMetrics,
    pub gc: GcMetrics,
    pub supervisor: SupervisorMetrics,
    pub collections: CollectionMetrics,
    pub retry: RetryMetrics,
    pub db: DbMetrics,
    pub state: StateMetrics,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Build and register everything. The one place a metric name is chosen.
    #[allow(
        clippy::too_many_lines,
        reason = "one flat registration block is the point: every name, help text \
                  and bucket set is readable in one pass, which is what makes the \
                  cardinality audit and the stability test reviewable."
    )]
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("mindex");

        // ── HTTP ──
        let http = HttpMetrics {
            requests: Family::default(),
            duration: hist_family(request_hist),
            in_flight: Family::default(),
            by_project: Family::default(),
            by_proto: Family::default(),
        };
        registry.register(
            "http_requests",
            "HTTP requests completed, by matched route, method, status and error code",
            http.requests.clone(),
        );
        registry.register(
            "http_request_duration_seconds",
            "HTTP request latency by matched route",
            http.duration.clone(),
        );
        registry.register(
            "http_requests_in_flight",
            "HTTP requests currently being served",
            http.in_flight.clone(),
        );
        registry.register(
            "http_requests_by_project",
            "HTTP requests completed, by matched route and project (opt-in)",
            http.by_project.clone(),
        );
        registry.register(
            "http_requests_by_proto",
            "HTTP requests completed, by transport version",
            http.by_proto.clone(),
        );

        // ── Indexing ──
        let index = IndexMetrics {
            files: Family::default(),
            chunks: Family::default(),
            symbols: Family::default(),
            code_bytes: Family::default(),
            file_size: hist_family(size_hist),
            file_chunks: hist_family(count_hist),
            phase_duration: hist_family(request_hist),
            claim_conflicts: Counter::default(),
            drift_checks: Family::default(),
            drift_files: Family::default(),
        };
        registry.register(
            "index_files",
            "Files seen by an index pass, by outcome",
            index.files.clone(),
        );
        registry.register(
            "index_chunks",
            "Chunks produced by indexing",
            index.chunks.clone(),
        );
        registry.register(
            "index_symbols",
            "Symbol rows produced by indexing",
            index.symbols.clone(),
        );
        registry.register(
            "index_code_bytes",
            "Source bytes accepted by indexing",
            index.code_bytes.clone(),
        );
        registry.register(
            "index_file_size_bytes",
            "Size distribution of files submitted for indexing",
            index.file_size.clone(),
        );
        registry.register(
            "index_file_chunks",
            "Distribution of chunks produced per file",
            index.file_chunks.clone(),
        );
        registry.register(
            "index_phase_duration_seconds",
            "Duration of an index request's prepare, embed and mark phases",
            index.phase_duration.clone(),
        );
        registry.register(
            "index_claim_conflicts",
            "Files skipped because another writer held the in-process claim",
            index.claim_conflicts.clone(),
        );
        registry.register(
            "drift_checks",
            "Working-tree drift comparisons requested by a client",
            index.drift_checks.clone(),
        );
        registry.register(
            "drift_files_reported",
            "Files a drift check classified as stale, missing, orphaned or indexing",
            index.drift_files.clone(),
        );

        // ── Embedder ──
        let embed = EmbedMetrics {
            requests: Family::default(),
            duration: hist_family(long_hist),
            batch_size: hist_family(count_hist),
            texts: Family::default(),
            retries: Family::default(),
        };
        registry.register(
            "embed_requests",
            "Calls to a BGE-M3 /encode endpoint, by outcome",
            embed.requests.clone(),
        );
        registry.register(
            "embed_duration_seconds",
            "Latency of a BGE-M3 /encode call",
            embed.duration.clone(),
        );
        registry.register(
            "embed_batch_texts",
            "Texts per /encode call",
            embed.batch_size.clone(),
        );
        registry.register("embed_texts", "Texts embedded", embed.texts.clone());
        registry.register(
            "embed_retries",
            "Embed calls resent after an HTTP 429 from the embedder",
            embed.retries.clone(),
        );

        // ── Qdrant ──
        let qdrant = QdrantMetrics {
            ops: Family::default(),
            duration: hist_family(request_hist),
            points: Family::default(),
        };
        registry.register(
            "qdrant_ops",
            "Vector-store operations, by op and outcome",
            qdrant.ops.clone(),
        );
        registry.register(
            "qdrant_op_duration_seconds",
            "Vector-store operation latency",
            qdrant.duration.clone(),
        );
        registry.register(
            "qdrant_points",
            "Points upserted into or deleted from Qdrant",
            qdrant.points.clone(),
        );

        // ── Search ──
        let search = SearchMetrics {
            requests: Family::default(),
            stage_duration: hist_family(request_hist),
            candidates: count_hist(),
            results: count_hist(),
            orphaned_winners: Counter::default(),
            unscorable_winners: Counter::default(),
        };
        registry.register(
            "search_requests",
            "Search requests, by outcome",
            search.requests.clone(),
        );
        registry.register(
            "search_stage_duration_seconds",
            "Search latency split by stage: embed, candidates, qdrant, fetch",
            search.stage_duration.clone(),
        );
        registry.register(
            "search_candidates",
            "Candidate chunks the SQLite filter handed to Qdrant",
            search.candidates.clone(),
        );
        registry.register(
            "search_results",
            "Results returned to the caller",
            search.results.clone(),
        );
        registry.register(
            "search_orphaned_winners",
            "Qdrant winners dropped because their SQLite chunk row was gone",
            search.orphaned_winners.clone(),
        );
        registry.register(
            "search_unscorable_winners",
            "Winners the reranker scored NaN, ranked last instead of first",
            search.unscorable_winners.clone(),
        );

        // ── Research ──
        let research = ResearchMetrics {
            runs: Family::default(),
            unjournalled: Family::default(),
            runs_by_effort: Family::default(),
            duration: hist_family(long_hist),
            steps: hist_family(count_hist),
            turns: hist_family(count_hist),
            // 1 → 8192 tok/s. A healthy local model sits in the tens; the low
            // buckets are where contention shows, and they are the point.
            turn_tokens_per_second: hist_family(count_hist),
            turn_load_seconds: hist_family(request_hist),
            turn_unaccounted_seconds: hist_family(long_hist),
            tokens: Family::default(),
            context_used: hist_family(ratio_hist),
            tool_calls: Family::default(),
            tool_duration: hist_family(request_hist),
            citations: Family::default(),
            challenges: Family::default(),
            challenge_verdict_caps: Counter::default(),
            challenges_replaced: Counter::default(),
            revalidations: Counter::default(),
            forced_syntheses: Counter::default(),
            parse_retries: Counter::default(),
            truncations: Counter::default(),
            runaway_thinking_turns: Counter::default(),
            stalled_turns: Counter::default(),
            runs_with_context: Family::default(),
            context_runs_used: Counter::default(),
            context_truncations: Counter::default(),
            report_length_caps: Counter::default(),
            report_context_sheds: Counter::default(),
            // 1 → 8192 words. The granted ladder is 400/900/1800, so the ceiling
            // sits three buckets above the deepest grant: a report landing in +Inf
            // is itself the finding.
            report_words: hist_family(count_hist),
            report_sections: Family::default(),
            watchdog_cancels: Counter::default(),
        };
        registry.register(
            "research_runs",
            "Research runs finished, by model and why they stopped",
            research.runs.clone(),
        );
        registry.register(
            "research_unjournalled_runs",
            "Research runs that produced no stored row, by model and why",
            research.unjournalled.clone(),
        );
        registry.register(
            "research_runs_by_effort",
            "Research runs finished, by model and effort preset",
            research.runs_by_effort.clone(),
        );
        registry.register(
            "research_duration_seconds",
            "Wall-clock duration of a research run",
            research.duration.clone(),
        );
        registry.register(
            "research_steps",
            "Executed tool steps per research run",
            research.steps.clone(),
        );
        registry.register(
            "research_turns",
            "Model turns per research run",
            research.turns.clone(),
        );
        registry.register(
            "research_tokens",
            "Tokens a research run made the model process, by prompt or eval",
            research.tokens.clone(),
        );
        registry.register(
            "research_turn_tokens_per_second",
            "Generation rate of one model turn, over its own generation time",
            research.turn_tokens_per_second.clone(),
        );
        registry.register(
            "research_turn_load_seconds",
            "Time Ollama spent loading the model for one turn",
            research.turn_load_seconds.clone(),
        );
        registry.register(
            "research_turn_unaccounted_seconds",
            "Wall clock of one model turn that Ollama did not account for (queueing)",
            research.turn_unaccounted_seconds.clone(),
        );
        registry.register(
            "research_context_used_ratio",
            "Peak prompt tokens as a fraction of the run's num_ctx",
            research.context_used.clone(),
        );
        registry.register(
            "research_tool_calls",
            "Tool calls executed by a research run, by tool and outcome",
            research.tool_calls.clone(),
        );
        registry.register(
            "research_tool_duration_seconds",
            "Latency of a research tool call",
            research.tool_duration.clone(),
        );
        registry.register(
            "research_citations",
            "Citations in a finished report, by provenance verdict",
            research.citations.clone(),
        );
        registry.register(
            "research_challenges",
            "Challenge runs recorded, by overall verdict",
            research.challenges.clone(),
        );
        registry.register(
            "research_challenge_verdict_caps",
            // No trailing period: the encoder appends one.
            "Challenge verdicts downgraded because the challenge's own report was ungrounded",
            research.challenge_verdict_caps.clone(),
        );
        registry.register(
            "research_challenges_replaced",
            // No trailing period: the encoder appends one.
            "Standing challenges deleted because a newer challenge of the same report reached a verdict",
            research.challenges_replaced.clone(),
        );
        registry.register(
            "research_revalidations",
            "Reports sent back to the model because their citations did not check out",
            research.revalidations.clone(),
        );
        registry.register(
            "research_forced_syntheses",
            "Reports written by the server because the report window expired first",
            research.forced_syntheses.clone(),
        );
        registry.register(
            "research_tool_call_parse_retries",
            "Turns resent at a new seed after Ollama failed to parse a tool call",
            research.parse_retries.clone(),
        );
        registry.register(
            "research_transcript_truncations",
            "Turns whose prompt reached num_ctx, so Ollama silently trimmed the transcript",
            research.truncations.clone(),
        );
        registry.register(
            "research_runaway_thinking_turns",
            "Turns abandoned after the model streamed past the thinking-volume guard",
            research.runaway_thinking_turns.clone(),
        );
        registry.register(
            "research_stalled_turns",
            "Turns abandoned for passing the per-turn ceiling while still streaming",
            research.stalled_turns.clone(),
        );
        registry.register(
            "research_runs_with_context",
            "Research runs given at least one earlier report as context, by model",
            research.runs_with_context.clone(),
        );
        registry.register(
            "research_context_runs_used",
            "Earlier reports injected into a research run, summed over runs",
            research.context_runs_used.clone(),
        );
        registry.register(
            "research_context_truncations",
            "Context blocks whose last report was truncated to fit max_context_chars",
            research.context_truncations.clone(),
        );
        registry.register(
            "research_report_length_caps",
            "Report turns cut off by num_predict instead of finishing",
            research.report_length_caps.clone(),
        );
        registry.register(
            "research_report_context_sheds",
            "Report turns whose prompt was over the context ceiling, so old tool output was dropped",
            research.report_context_sheds.clone(),
        );
        registry.register(
            "research_report_words",
            "Words in the report a research run shipped, by model",
            research.report_words.clone(),
        );
        registry.register(
            "research_report_sections",
            "Sections of a sectioned report, by what became of each one",
            research.report_sections.clone(),
        );
        registry.register(
            "research_watchdog_cancels",
            "Research runs cancelled by the watchdog after outliving their worst case",
            research.watchdog_cancels.clone(),
        );

        // ── GC ──
        let gc = GcMetrics {
            runs: Family::default(),
            duration: long_hist(),
            chunks_removed: Counter::default(),
            files_pruned: Counter::default(),
            status_log_pruned: Counter::default(),
            research_pruned: Counter::default(),
            running: Gauge::default(),
        };
        registry.register(
            "gc_runs",
            "Garbage-collection passes, by trigger and outcome",
            gc.runs.clone(),
        );
        registry.register(
            "gc_duration_seconds",
            "Duration of a garbage-collection pass",
            gc.duration.clone(),
        );
        registry.register(
            "gc_chunks_removed",
            "Chunk rows hard-deleted after Qdrant confirmed the vector delete",
            gc.chunks_removed.clone(),
        );
        registry.register(
            "gc_files_pruned",
            "Deleted file rows dropped once their chunks were gone",
            gc.files_pruned.clone(),
        );
        registry.register(
            "gc_status_log_pruned",
            "Status-log rows dropped past the retention window",
            gc.status_log_pruned.clone(),
        );
        registry.register(
            "gc_research_pruned",
            "Stored research runs reaped for having passed their expiry",
            gc.research_pruned.clone(),
        );
        registry.register(
            "gc_running",
            "1 while a garbage-collection pass holds the process-wide guard",
            gc.running.clone(),
        );

        // ── Worker supervision ──
        let supervisor = SupervisorMetrics {
            running: Family::default(),
            exits: Family::default(),
        };
        registry.register(
            "worker_running",
            "1 while a background worker's task is alive, 0 once it has exited",
            supervisor.running.clone(),
        );
        registry.register(
            "worker_exits",
            "Background worker task exits, by worker and how it ended",
            supervisor.exits.clone(),
        );

        // ── Collection layout ──
        //
        // Seeded at -1, the "not checked yet" value: 0 is the healthy reading and must
        // never be published by a check that could not run.
        let collections = CollectionMetrics {
            stale: Gauge::default(),
            orphaned: Gauge::default(),
        };
        collections.stale.set(-1);
        collections.orphaned.set(-1);
        registry.register(
            "stale_collections",
            "Projects with active chunks whose current-schema-version Qdrant collection \
             is missing or empty (-1 = not yet checked)",
            collections.stale.clone(),
        );
        registry.register(
            "orphaned_collections",
            "Qdrant collections left behind at a previous schema version (-1 = not yet \
             checked)",
            collections.orphaned.clone(),
        );

        // ── Retry worker ──
        let retry = RetryMetrics {
            sweeps: Counter::default(),
            files: Family::default(),
        };
        registry.register(
            "retry_sweeps",
            "Retry-worker sweeps that found at least one candidate",
            retry.sweeps.clone(),
        );
        registry.register(
            "retry_files",
            "Files the retry worker handled, by outcome",
            retry.files.clone(),
        );

        // ── SQLite pool ──
        let db = DbMetrics {
            transaction_duration: db_hist(),
            transactions: Family::default(),
            pool_acquire_failures: Counter::default(),
            pool_size: Gauge::default(),
            pool_available: Gauge::default(),
        };
        registry.register(
            "db_transaction_duration_seconds",
            "Time a SQLite transaction held its connection",
            db.transaction_duration.clone(),
        );
        registry.register(
            "db_transactions",
            "SQLite transactions, by outcome",
            db.transactions.clone(),
        );
        registry.register(
            "db_pool_acquire_failures",
            "Transactions refused because the connection pool was empty",
            db.pool_acquire_failures.clone(),
        );
        registry.register(
            "db_pool_size",
            "Connections in the SQLite pool",
            db.pool_size.clone(),
        );
        registry.register(
            "db_pool_available",
            "Idle connections in the SQLite pool",
            db.pool_available.clone(),
        );

        // ── State (collector-owned; see `StateMetrics`) ──
        let state = StateMetrics {
            project_files: Family::default(),
            project_files_by_language: Family::default(),
            project_chunks_active: Family::default(),
            project_chunks_deleted: Family::default(),
            project_symbols: Family::default(),
            project_last_indexed: Family::default(),
            project_files_permanently_failed: Family::default(),
            projects: Gauge::default(),
            db_size_bytes: Gauge::default(),
            status_log_rows: Gauge::default(),
            project_research_runs: Family::default(),
            project_research_pinned: Family::default(),
            project_research_stale: Family::default(),
            dependency_up: Family::default(),
            project_vectors: Family::default(),
            state_refreshed_at: Gauge::default(),
            indexing_claims: Gauge::default(),
            research_active: Gauge::default(),
            research_permits_available: Gauge::default(),
            research_inflight_oldest_age_seconds: Gauge::default(),
            research_worker_threads: Gauge::default(),
            build_info: Family::default(),
            start_time: Gauge::default(),
        };
        registry.register(
            "project_files",
            "Files known to a project, by status",
            state.project_files.clone(),
        );
        registry.register(
            "project_files_by_language",
            "Files known to a project, by language",
            state.project_files_by_language.clone(),
        );
        registry.register(
            "project_chunks_active",
            "Active chunks in a project, by language",
            state.project_chunks_active.clone(),
        );
        registry.register(
            "project_chunks_deleted",
            "Soft-deleted chunks in a project awaiting garbage collection",
            state.project_chunks_deleted.clone(),
        );
        registry.register(
            "project_symbols",
            "Definition rows in a project",
            state.project_symbols.clone(),
        );
        registry.register(
            "project_last_indexed_timestamp_seconds",
            "Unix time of the most recent file to reach 'indexed' in a project",
            state.project_last_indexed.clone(),
        );
        registry.register(
            "project_files_permanently_failed",
            "Files that exhausted their retries and will not be retried again",
            state.project_files_permanently_failed.clone(),
        );
        registry.register(
            "projects",
            "Projects in the database",
            state.projects.clone(),
        );
        registry.register(
            "db_size_bytes",
            "Size of the SQLite database file",
            state.db_size_bytes.clone(),
        );
        registry.register(
            "status_log_rows",
            "Rows in the file status-transition log",
            state.status_log_rows.clone(),
        );
        registry.register(
            "project_research_runs",
            "Stored research runs per project",
            state.project_research_runs.clone(),
        );
        registry.register(
            "project_research_pinned",
            "Stored research runs exempt from the retention sweep",
            state.project_research_pinned.clone(),
        );
        registry.register(
            "project_research_stale",
            "Stored research runs at least one of whose files has changed since",
            state.project_research_stale.clone(),
        );
        registry.register(
            "dependency_up",
            "1 when a dependency answered its health probe",
            state.dependency_up.clone(),
        );
        registry.register(
            "project_vectors",
            "Points Qdrant holds for the project, to compare against project_chunks_active",
            state.project_vectors.clone(),
        );
        registry.register(
            "state_refreshed_timestamp_seconds",
            "Unix time of the last successful state-aggregate snapshot",
            state.state_refreshed_at.clone(),
        );
        registry.register(
            "indexing_claims",
            "Files currently held by an in-process indexing claim",
            state.indexing_claims.clone(),
        );
        registry.register(
            "research_active",
            "Research runs holding a concurrency permit",
            state.research_active.clone(),
        );
        registry.register(
            "research_permits_available",
            "Free research concurrency permits",
            state.research_permits_available.clone(),
        );
        registry.register(
            "research_inflight_oldest_age_seconds",
            "Age of the longest-running live research run, 0 when none is running",
            state.research_inflight_oldest_age_seconds.clone(),
        );
        registry.register(
            "research_worker_threads",
            "Worker threads on the dedicated research runtime",
            state.research_worker_threads.clone(),
        );
        registry.register(
            "build_info",
            "Build and schema identity of this process",
            state.build_info.clone(),
        );
        registry.register(
            "start_time_seconds",
            "Unix time this process started serving",
            state.start_time.clone(),
        );

        Self {
            registry,
            http,
            index,
            embed,
            qdrant,
            search,
            research,
            gc,
            supervisor,
            collections,
            retry,
            db,
            state,
        }
    }

    /// Render the exposition body. Serve it as [`CONTENT_TYPE`].
    pub fn render(&self) -> Result<String, std::fmt::Error> {
        let mut out = String::new();
        encode(&mut out, &self.registry)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touch every family once, so `render()` emits every name. A family with no
    /// observations still emits its `# TYPE` line, but touching them also proves
    /// each label set encodes.
    fn touched() -> Metrics {
        let m = Metrics::new();

        m.http
            .requests
            .get_or_create(&RequestLabels {
                route: "/status".into(),
                method: "GET",
                status: 200,
                code: "",
            })
            .inc();
        m.http
            .duration
            .get_or_create(&RouteLabels {
                route: "/status".into(),
                method: "GET",
            })
            .observe(0.01);
        m.http
            .in_flight
            .get_or_create(&RouteLabels {
                route: "/status".into(),
                method: "GET",
            })
            .set(0);
        m.http
            .by_project
            .get_or_create(&RouteProjectLabels {
                route: "/v0/{project_guid}/search".into(),
                project_guid: "p".into(),
            })
            .inc();
        m.http
            .by_proto
            .get_or_create(&ProtoLabels { proto: "HTTP/1.1" })
            .inc();

        let pl = ProjectLangLabels {
            project_guid: "p".into(),
            language: "rust",
        };
        m.index
            .files
            .get_or_create(&ProjectLangOutcomeLabels {
                project_guid: "p".into(),
                language: "rust",
                outcome: "indexed",
            })
            .inc();
        m.index.chunks.get_or_create(&pl).inc();
        m.index.symbols.get_or_create(&pl).inc();
        m.index.code_bytes.get_or_create(&pl).inc();
        m.index
            .file_size
            .get_or_create(&LangLabels { language: "rust" })
            .observe(1024.0);
        m.index
            .file_chunks
            .get_or_create(&LangLabels { language: "rust" })
            .observe(4.0);
        m.index
            .phase_duration
            .get_or_create(&PhaseLabels { phase: "prepare" })
            .observe(0.1);
        m.index.claim_conflicts.inc();
        m.index
            .drift_checks
            .get_or_create(&ProjectLabels {
                project_guid: "p".into(),
            })
            .inc();
        m.index
            .drift_files
            .get_or_create(&ProjectClassLabels {
                project_guid: "p".into(),
                class: "stale",
            })
            .inc();

        let emb = EmbedderLabels { embedder: "index" };
        m.embed
            .requests
            .get_or_create(&EmbedderOutcomeLabels {
                embedder: "index",
                outcome: "ok",
            })
            .inc();
        m.embed.duration.get_or_create(&emb).observe(1.0);
        m.embed.batch_size.get_or_create(&emb).observe(256.0);
        m.embed.texts.get_or_create(&emb).inc();
        m.embed.retries.get_or_create(&emb).inc();

        m.qdrant
            .ops
            .get_or_create(&OpOutcomeLabels {
                op: "search",
                outcome: "ok",
            })
            .inc();
        m.qdrant
            .duration
            .get_or_create(&OpLabels { op: "search" })
            .observe(0.02);
        m.qdrant
            .points
            .get_or_create(&OpLabels { op: "insert_batch" })
            .inc();

        m.search
            .requests
            .get_or_create(&ProjectOutcomeLabels {
                project_guid: "p".into(),
                outcome: "hit",
            })
            .inc();
        m.search
            .stage_duration
            .get_or_create(&StageLabels { stage: "qdrant" })
            .observe(0.03);
        m.search.candidates.observe(900.0);
        m.search.results.observe(5.0);

        let model = ModelLabels {
            model: "glm".into(),
        };
        m.research
            .runs
            .get_or_create(&ModelReasonLabels {
                model: "glm".into(),
                done_reason: "finalized",
            })
            .inc();
        m.research
            .runs_by_effort
            .get_or_create(&ModelEffortLabels {
                model: "glm".into(),
                effort: "medium",
            })
            .inc();
        m.research.duration.get_or_create(&model).observe(120.0);
        m.research.steps.get_or_create(&model).observe(8.0);
        m.research.turns.get_or_create(&model).observe(10.0);
        m.research
            .turn_tokens_per_second
            .get_or_create(&model)
            .observe(22.5);
        m.research
            .turn_load_seconds
            .get_or_create(&model)
            .observe(0.5);
        m.research
            .turn_unaccounted_seconds
            .get_or_create(&model)
            .observe(0.05);
        m.research
            .tokens
            .get_or_create(&ModelKindLabels {
                model: "glm".into(),
                kind: "prompt",
            })
            .inc();
        m.research.context_used.get_or_create(&model).observe(0.2);
        m.research
            .tool_calls
            .get_or_create(&ToolOutcomeLabels {
                tool: "search",
                outcome: "ok",
            })
            .inc();
        m.research
            .tool_duration
            .get_or_create(&ToolOutcomeLabels {
                tool: "search",
                outcome: "ok",
            })
            .observe(0.5);
        m.research
            .citations
            .get_or_create(&ClassLabels { class: "verified" })
            .inc();
        m.research
            .challenges
            .get_or_create(&OutcomeLabels { outcome: "refuted" })
            .inc();
        m.research.challenge_verdict_caps.inc();
        m.research.revalidations.inc();
        m.research.parse_retries.inc();
        m.gc.research_pruned.inc();
        for f in [
            &m.state.project_research_runs,
            &m.state.project_research_pinned,
            &m.state.project_research_stale,
        ] {
            f.get_or_create(&ProjectLabels {
                project_guid: "p".into(),
            })
            .set(1);
        }
        m.research.truncations.inc();
        m.research.runaway_thinking_turns.inc();
        m.research.stalled_turns.inc();
        m.research
            .runs_with_context
            .get_or_create(&ModelLabels { model: "m".into() })
            .inc();
        m.research.context_runs_used.inc();
        m.research.context_truncations.inc();
        m.research.report_length_caps.inc();
        m.research.report_context_sheds.inc();
        m.research.report_words.get_or_create(&model).observe(742.0);

        m.gc.runs
            .get_or_create(&TriggerOutcomeLabels {
                trigger: "worker",
                outcome: "ok",
            })
            .inc();
        m.gc.duration.observe(2.0);
        m.gc.chunks_removed.inc();
        m.gc.files_pruned.inc();
        m.gc.status_log_pruned.inc();
        m.gc.running.set(0);

        m.research
            .unjournalled
            .get_or_create(&ModelOutcomeLabels {
                model: "m".into(),
                outcome: "cancelled",
            })
            .inc();
        m.search.orphaned_winners.inc();
        m.search.unscorable_winners.inc();
        m.state
            .project_vectors
            .get_or_create(&ProjectLabels {
                project_guid: "p".into(),
            })
            .set(1);
        m.state.state_refreshed_at.set(1);

        m.supervisor
            .running
            .get_or_create(&WorkerLabels { worker: "gc" })
            .set(1);
        m.supervisor
            .exits
            .get_or_create(&WorkerOutcomeLabels {
                worker: "gc",
                outcome: "panic",
            })
            .inc();

        m.retry.sweeps.inc();
        m.retry
            .files
            .get_or_create(&OutcomeLabels { outcome: "indexed" })
            .inc();

        m.db.transaction_duration.observe(0.001);
        m.db.transactions
            .get_or_create(&OutcomeLabels { outcome: "ok" })
            .inc();
        m.db.pool_acquire_failures.inc();
        m.db.pool_size.set(4);
        m.db.pool_available.set(4);

        let s = &m.state;
        s.project_files
            .get_or_create(&ProjectStatusLabels {
                project_guid: "p".into(),
                status: "indexed".into(),
            })
            .set(1);
        s.project_files_by_language.get_or_create(&pl).set(1);
        s.project_chunks_active.get_or_create(&pl).set(1);
        s.project_chunks_deleted
            .get_or_create(&ProjectLabels {
                project_guid: "p".into(),
            })
            .set(0);
        s.project_symbols
            .get_or_create(&ProjectLabels {
                project_guid: "p".into(),
            })
            .set(1);
        s.project_last_indexed
            .get_or_create(&ProjectLabels {
                project_guid: "p".into(),
            })
            .set(1);
        s.project_files_permanently_failed
            .get_or_create(&ProjectLabels {
                project_guid: "p".into(),
            })
            .set(0);
        s.projects.set(1);
        s.db_size_bytes.set(1);
        s.status_log_rows.set(1);
        s.dependency_up
            .get_or_create(&DependencyLabels {
                dependency: "qdrant",
            })
            .set(1);
        s.indexing_claims.set(0);
        s.research_active.set(0);
        s.research_permits_available.set(2);
        s.research_worker_threads.set(2);
        s.build_info
            .get_or_create(&BuildLabels {
                version: "1.0.0",
                db_schema_version: "1".into(),
                model_id: "qwen3-embedding-0.6b".into(),
            })
            .set(1);
        s.start_time.set(1);

        m
    }

    /// Every family as a dashboard would *query* it, with its type, sorted.
    ///
    /// OpenMetrics puts the `_total` suffix on a counter's sample lines, not on
    /// its `# TYPE` line — the family is `mindex_gc_runs`, the series Prometheus
    /// stores is `mindex_gc_runs_total`. The series name is the contract, so it
    /// is what this reconstructs and what the list below pins.
    fn rendered_families(text: &str) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let name = it.next()?;
                let ty = it.next()?;
                let queried = if ty == "counter" {
                    format!("{name}_total")
                } else {
                    name.to_string()
                };
                Some((queried, ty.to_string()))
            })
            .collect();
        v.sort();
        v
    }

    /// The dashboard is a client and a metric name is as much a contract as an
    /// `ApiError` code — this is the `codes_are_stable` mirror. The *type* is
    /// pinned alongside the name because a counter→gauge flip renames nothing
    /// and silently breaks every `rate()` built on it.
    #[test]
    fn metric_names_are_stable() {
        let text = touched().render().expect("registry renders");
        let got = rendered_families(&text);

        let mut want: Vec<(String, String)> = [
            ("mindex_build_info", "gauge"),
            ("mindex_db_pool_acquire_failures_total", "counter"),
            ("mindex_db_pool_available", "gauge"),
            ("mindex_db_pool_size", "gauge"),
            ("mindex_db_size_bytes", "gauge"),
            ("mindex_db_transaction_duration_seconds", "histogram"),
            ("mindex_db_transactions_total", "counter"),
            ("mindex_dependency_up", "gauge"),
            ("mindex_drift_checks_total", "counter"),
            ("mindex_drift_files_reported_total", "counter"),
            ("mindex_embed_batch_texts", "histogram"),
            ("mindex_embed_duration_seconds", "histogram"),
            ("mindex_embed_requests_total", "counter"),
            ("mindex_embed_retries_total", "counter"),
            ("mindex_embed_texts_total", "counter"),
            ("mindex_gc_chunks_removed_total", "counter"),
            ("mindex_gc_duration_seconds", "histogram"),
            ("mindex_gc_files_pruned_total", "counter"),
            ("mindex_gc_running", "gauge"),
            ("mindex_gc_runs_total", "counter"),
            ("mindex_gc_research_pruned_total", "counter"),
            ("mindex_project_research_pinned", "gauge"),
            ("mindex_project_research_runs", "gauge"),
            ("mindex_project_research_stale", "gauge"),
            ("mindex_gc_status_log_pruned_total", "counter"),
            ("mindex_http_request_duration_seconds", "histogram"),
            ("mindex_http_requests_by_project_total", "counter"),
            ("mindex_http_requests_by_proto_total", "counter"),
            ("mindex_http_requests_in_flight", "gauge"),
            ("mindex_http_requests_total", "counter"),
            ("mindex_index_chunks_total", "counter"),
            ("mindex_index_claim_conflicts_total", "counter"),
            ("mindex_index_code_bytes_total", "counter"),
            ("mindex_index_file_chunks", "histogram"),
            ("mindex_index_file_size_bytes", "histogram"),
            ("mindex_index_files_total", "counter"),
            ("mindex_index_phase_duration_seconds", "histogram"),
            ("mindex_index_symbols_total", "counter"),
            ("mindex_indexing_claims", "gauge"),
            ("mindex_orphaned_collections", "gauge"),
            ("mindex_stale_collections", "gauge"),
            ("mindex_project_chunks_active", "gauge"),
            ("mindex_project_chunks_deleted", "gauge"),
            ("mindex_project_files", "gauge"),
            ("mindex_project_files_by_language", "gauge"),
            ("mindex_project_files_permanently_failed", "gauge"),
            ("mindex_project_last_indexed_timestamp_seconds", "gauge"),
            ("mindex_project_symbols", "gauge"),
            ("mindex_project_vectors", "gauge"),
            ("mindex_projects", "gauge"),
            ("mindex_qdrant_op_duration_seconds", "histogram"),
            ("mindex_qdrant_ops_total", "counter"),
            ("mindex_qdrant_points_total", "counter"),
            ("mindex_research_active", "gauge"),
            ("mindex_research_citations_total", "counter"),
            ("mindex_research_challenges_total", "counter"),
            ("mindex_research_challenge_verdict_caps_total", "counter"),
            ("mindex_research_challenges_replaced_total", "counter"),
            ("mindex_research_context_used_ratio", "histogram"),
            ("mindex_research_duration_seconds", "histogram"),
            ("mindex_research_inflight_oldest_age_seconds", "gauge"),
            ("mindex_research_permits_available", "gauge"),
            ("mindex_research_revalidations_total", "counter"),
            ("mindex_research_forced_syntheses_total", "counter"),
            ("mindex_research_runs_by_effort_total", "counter"),
            ("mindex_research_runs_total", "counter"),
            ("mindex_research_steps", "histogram"),
            ("mindex_research_turn_load_seconds", "histogram"),
            ("mindex_research_turn_unaccounted_seconds", "histogram"),
            ("mindex_research_turn_tokens_per_second", "histogram"),
            ("mindex_research_tokens_total", "counter"),
            ("mindex_research_tool_call_parse_retries_total", "counter"),
            ("mindex_research_tool_calls_total", "counter"),
            ("mindex_research_tool_duration_seconds", "histogram"),
            ("mindex_research_runaway_thinking_turns_total", "counter"),
            ("mindex_research_stalled_turns_total", "counter"),
            ("mindex_research_runs_with_context_total", "counter"),
            ("mindex_research_context_runs_used_total", "counter"),
            ("mindex_research_context_truncations_total", "counter"),
            ("mindex_research_report_context_sheds_total", "counter"),
            ("mindex_research_report_length_caps_total", "counter"),
            ("mindex_research_report_words", "histogram"),
            ("mindex_research_transcript_truncations_total", "counter"),
            ("mindex_research_turns", "histogram"),
            ("mindex_research_unjournalled_runs_total", "counter"),
            ("mindex_research_watchdog_cancels_total", "counter"),
            ("mindex_research_worker_threads", "gauge"),
            ("mindex_retry_files_total", "counter"),
            ("mindex_retry_sweeps_total", "counter"),
            ("mindex_search_candidates", "histogram"),
            ("mindex_search_orphaned_winners_total", "counter"),
            ("mindex_search_requests_total", "counter"),
            ("mindex_search_unscorable_winners_total", "counter"),
            ("mindex_search_results", "histogram"),
            ("mindex_search_stage_duration_seconds", "histogram"),
            ("mindex_start_time_seconds", "gauge"),
            ("mindex_state_refreshed_timestamp_seconds", "gauge"),
            ("mindex_status_log_rows", "gauge"),
            ("mindex_worker_exits_total", "counter"),
            ("mindex_worker_running", "gauge"),
        ]
        .iter()
        .map(|(n, t)| ((*n).to_string(), (*t).to_string()))
        .collect();
        want.sort();

        assert_eq!(
            got, want,
            "metric names/types are a contract with the Grafana dashboard — \
             update the dashboard in the same change, then this list"
        );
    }

    /// Every `mindex_*{...}` sample line in the exposition, as a set.
    fn rendered_series(text: &str) -> std::collections::HashSet<&str> {
        text.lines()
            .filter(|l| l.starts_with("mindex_"))
            .map(|l| l.split_whitespace().next().unwrap_or(l))
            .collect()
    }

    /// **The clear-and-repopulate rule, enforced rather than described.**
    ///
    /// The metrics collector rebuilds `StateMetrics` from scratch every tick, and
    /// two structural guards keep that safe: only `StateMetrics` is ever cleared,
    /// and `StateMetrics` holds gauges only. The second is the one with teeth —
    /// clearing a *counter* makes its series disappear and reappear at zero, which
    /// Prometheus reads as a process restart and which permanently re-baselines
    /// every `rate()` and `increase()` over it. A counter accidentally placed in
    /// `StateMetrics` (or a `clear()` added to the wrong family) would produce a
    /// dashboard that is quietly, unfixably wrong rather than blank.
    ///
    /// This checks it without a hardcoded list of what the collector clears: touch
    /// every family, run a real tick against an empty database, and see what stopped
    /// being reported. Gauges legitimately vanish that way; counters must not.
    #[tokio::test]
    async fn a_collector_tick_never_clears_a_counter() {
        let metrics = touched();
        let before = metrics.render().expect("registry renders");
        let types: std::collections::HashMap<&str, &str> = before
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?, it.next()?))
            })
            .collect();
        let series_before = rendered_series(&before);

        // An empty, migrated database: every state aggregate reads back as nothing,
        // so a tick clears each family it owns and repopulates none of it — the
        // harshest version of the real thing.
        let pool = crate::db::sqlite3::SQLite3Pool::new(
            std::path::Path::new(":memory:"),
            1,
            16384,
            "NORMAL",
        );
        pool.transaction(tokio_util::sync::CancellationToken::new(), |tx| {
            crate::apply_pending_migrations(tx).map(|_| ())
        })
        .await
        .expect("migrations apply");

        crate::worker::metrics::collect_once(
            &pool,
            &metrics,
            &crate::worker::metrics::MetricsTuning {
                refresh_interval_seconds: 60,
                probe_dependencies: false,
                max_retries: 3,
                model_id: "qwen3-embedding-0.6b".to_string(),
            },
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        let after = metrics.render().expect("registry renders");
        let series_after = rendered_series(&after);

        let mut lost_counters: Vec<&str> = series_before
            .difference(&series_after)
            .filter(|s| {
                // Map a sample line back to its family: OpenMetrics puts `_total` on a
                // counter's samples but not on its `# TYPE` line, and histograms
                // suffix `_bucket`/`_sum`/`_count`.
                let family = s.split('{').next().unwrap_or(s);
                let base = family.strip_suffix("_total").unwrap_or(family);
                types.get(base).is_some_and(|t| *t == "counter")
            })
            .copied()
            .collect();
        lost_counters.sort_unstable();

        assert!(
            lost_counters.is_empty(),
            "a collector tick cleared these counter series: {lost_counters:?}. \
             Prometheus reads a counter that disappears and returns at zero as a \
             process restart, which permanently re-baselines every rate() over it — \
             move them out of StateMetrics, or stop clearing their family."
        );

        // And the rule has to be worth enforcing: the tick must genuinely clear
        // something, or this test would pass on a collector that does nothing.
        assert!(
            series_before.len() > series_after.len(),
            "the tick cleared no series at all, so this test proved nothing"
        );
    }

    /// Mirrors the config convention that a key carries its unit. A metric whose
    /// name does not say what it measures is a panel someone will mislabel.
    #[test]
    fn every_metric_name_carries_its_unit() {
        // Bare gauges: a count of things right now, where a unit suffix would lie.
        const BARE: &[&str] = &[
            "mindex_build_info",
            "mindex_db_pool_available",
            "mindex_db_pool_size",
            "mindex_dependency_up",
            "mindex_embed_batch_texts",
            "mindex_gc_running",
            "mindex_http_requests_in_flight",
            "mindex_index_file_chunks",
            "mindex_indexing_claims",
            // Both are counts of collections, and both use -1 for "not yet checked" —
            // a unit suffix on either would promise a measurement they do not make.
            "mindex_orphaned_collections",
            "mindex_stale_collections",
            "mindex_project_chunks_active",
            "mindex_project_chunks_deleted",
            "mindex_project_files",
            "mindex_project_files_by_language",
            "mindex_project_files_permanently_failed",
            "mindex_project_research_pinned",
            "mindex_project_research_runs",
            "mindex_project_research_stale",
            "mindex_project_symbols",
            "mindex_project_vectors",
            "mindex_projects",
            "mindex_research_active",
            "mindex_research_context_used_ratio",
            "mindex_research_permits_available",
            // A liveness flag, like `gc_running` and `dependency_up` above it.
            "mindex_worker_running",
            // A count of words, like `steps` and `turns` beside it: the unit is the
            // thing being counted, and `_words` would only restate the name.
            "mindex_research_report_words",
            "mindex_research_steps",
            // A rate, and the unit is in the name — but `_per_second` is not
            // `_seconds`, so the suffix rule cannot see it. Renaming it to satisfy
            // the rule would name it after the wrong quantity.
            "mindex_research_turn_tokens_per_second",
            "mindex_research_turns",
            "mindex_research_worker_threads",
            "mindex_search_candidates",
            "mindex_search_results",
            "mindex_status_log_rows",
        ];

        let text = touched().render().expect("registry renders");
        for (name, _) in rendered_families(&text) {
            let ok = name.ends_with("_total")
                || name.ends_with("_seconds")
                || name.ends_with("_bytes")
                || BARE.contains(&name.as_str());
            assert!(ok, "{name} carries no unit and is not a listed bare gauge");
        }
    }

    /// The body is OpenMetrics, not Prometheus text — `# EOF` is the tell, and a
    /// content-type that disagrees fails at the scraper, never at build time.
    #[test]
    fn the_registry_renders_openmetrics() {
        let text = touched().render().expect("registry renders");
        assert!(text.ends_with("# EOF\n"), "missing OpenMetrics terminator");
        assert!(CONTENT_TYPE.starts_with("application/openmetrics-text"));
        assert!(text.contains("mindex_http_requests_total{"));
    }
}
