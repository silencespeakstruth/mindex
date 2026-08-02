import * as https from "node:https";
import * as http from "node:http";
import {
    MalformedResponseError,
    ProblemDetails,
    ProblemError,
    TimeoutError,
    UnreachableError,
} from "./problem";

// ---- wire types (src/backend/v0/models.rs) ----

export type IndexFiles = Record<string, Record<string, { code: string }>>;
export interface IndexResponse {
    files: Record<string, Record<string, number>>;
}

// ---- streaming /index (`?stream=yes`) wire events ----
// They live in `shared/indexEvents.ts` so the run aggregate and the webview page
// can be typed against them without importing this module's `node:https`; every
// name is re-exported here so no call site has to know that.

import type {
    IndexStartedEvent,
    IndexPreparedEvent,
    IndexSkippedEvent,
    IndexEmbeddedEvent,
    IndexIndexedEvent,
    IndexDoneEvent,
    IndexStreamCallbacks,
} from "./shared/indexEvents";

export type {
    IndexStartedEvent,
    IndexPreparedEvent,
    IndexSkippedEvent,
    IndexEmbeddedEvent,
    IndexIndexedEvent,
    IndexDoneEvent,
    IndexStreamCallbacks,
};

export interface SearchFilter {
    paths?: string[];
    programming_languages?: string[];
}
export interface SearchRequest {
    query: string;
    top_k?: number;
    include?: SearchFilter;
    exclude?: SearchFilter;
}
export interface SearchResult {
    score: number;
    path: string;
    code: string;
    start_line: number;
    end_line: number;
    start_column: number;
    end_column: number;
}
export interface SearchResponse {
    results: SearchResult[];
}

export interface DriftResponse {
    stale: string[];
    missing: string[];
    orphaned: string[];
    indexing: string[];
}

export interface Selector {
    include?: SearchFilter;
    exclude?: SearchFilter;
}

export interface HealthResponse {
    /**
     * The server's own verdict. `degraded` = only the optional Ollama is down,
     * so search and indexing still work; `unhealthy` = a required dependency
     * failed, or a research run is wedged.
     *
     * A server older than this vocabulary says `degraded` for the *required*
     * case, which is why nothing may key behaviour on this field alone — see
     * `readHealth` in `statusFetch.ts`, which reads it together with `checks`.
     */
    status: "ok" | "degraded" | "unhealthy";
    version: string;
    indexing_files: number;
    /**
     * Per-dependency liveness: `"ok"` or `"error"` — and `"error: <reason>"` from
     * an older server, which is why a reader must test `=== "ok"` and never
     * `startsWith("error")`. Rendered generically, so a check added server-side
     * shows up without a client change. `ollama` is optional (only `/research`
     * needs it) and absent on servers before it existed — hence `| undefined`.
     */
    checks: Record<string, string | undefined> & { ollama?: string };
    /**
     * Research admission. Dependencies being alive says nothing about whether a run
     * can start: with `slots_total: 1` a single occupied slot is a total outage of
     * research, and health used to report `"ok"` right through it.
     *
     * A *busy* slot is not a degradation — that is the service working. Only
     * `oldest_inflight_age_ms` past a run's own worst case is, and that is what
     * moves `status` here. Absent on older servers.
     */
    research?: {
        slots_total: number;
        slots_busy: number;
        /** `null` when nothing is running. */
        oldest_inflight_age_ms: number | null;
    };
}
/**
 * `GET /status` — a live runtime snapshot. Its file counts (`indexing_files`,
 * `files_by_status`) are **server-wide**, summing every project this server has
 * indexed, so the status view deliberately does not render them: next to the
 * per-project Failed list they only invite reading another project's failures as
 * this workspace's.
 */
export interface StatusResponse {
    indexing_claims: number;
    gc_running: boolean;
    pool_available: number;
    pool_size: number;
    indexing_files: number;
    files_by_status: Record<string, number>;
}
export interface FileEntry {
    path: string;
    programming_language: string;
    status: string;
    sha256: string;
    chunk_count: number;
    retry_count: number;
    status_updated_at: number;
}
/** One effort level's budgets, as served by `GET /config`. */
export interface ResearchEffortInfo {
    max_seconds: number;
    max_tokens: number;
    max_steps: number;
    context_fraction: number;
    /** Chunks one `search` call returns to the model — the evidence width. */
    search_top_k?: number;
    /** Word ceiling announced for the report (`0` = none). Absent on older servers. */
    max_report_words?: number;
    /** Report sections the run may write. Absent on older servers. */
    max_report_sections?: number;
    /** Multiplier on the per-call evidence tool widths. Absent on older servers. */
    evidence_width?: number;
    /**
     * `max_seconds + report_timeout_ms / 1000` — the longest a run at this level may
     * take, derived by the server because the two bound different phases and reading
     * `max_seconds` as the whole wait understates `high` by five minutes.
     */
    worst_case_seconds?: number;
}
/** What a `(model, effort)` pair has actually cost lately, from `GET /config`. */
export interface ResearchObservedEffort {
    model: string;
    effort: string;
    runs: number;
    p50_seconds: number;
    p90_seconds: number;
}
/** `[research].temperature`/`top_p`/`seed`; `null` means the model's own default. */
export interface ResearchSamplingInfo {
    temperature: number | null;
    top_p: number | null;
    seed: number | null;
}
export interface ResearchConfigInfo {
    default_model: string;
    /**
     * The models the server's Ollama has locally, refreshed server-side on an
     * interval. Optional because an older server does not publish it — and absent
     * is not the same as empty: with no list to offer, the model field stays free
     * text. Empty *with* a `models_refreshed_at` means Ollama really has none.
     */
    models?: string[];
    /** Unix seconds of the last successful registry read; `null` = never reached. */
    models_refreshed_at?: number | null;
    effort: { low: ResearchEffortInfo; medium: ResearchEffortInfo; high: ResearchEffortInfo };
    max_request_seconds: number;
    max_request_tokens: number;
    max_request_steps: number;
    /** Ceilings on the report-shape overrides. Absent on older servers. */
    max_request_report_sections?: number;
    max_request_report_words?: number;
    max_evidence_width?: number;
    /** Steps between draft-banking turns (`0` = off). Absent on older servers. */
    checkpoint_every_steps?: number;
    /**
     * How long the report phase gets after the investigation deadline. The other
     * half of what a caller waits: `effort.*.max_seconds` bounds the investigation,
     * and the longest a request can take is that plus this.
     */
    report_timeout_ms?: number;
    /**
     * How many runs the server admits at once. A second request while they are all
     * busy is refused with 429, not queued — so this is what says whether two
     * investigations can be started together. Absent on older servers.
     */
    max_concurrent?: number;
    /** Caps on `context_run_ids` and the injected prior-report block. */
    max_context_runs?: number;
    max_context_chars?: number;
    /**
     * Default and maximum page size of the research list. Published so a client
     * paging the corpus sizes its loop instead of guessing — guessing low doubles
     * the request count, guessing high is a 400. Absent on older servers.
     */
    list_page_limit?: number;
    /**
     * How many run ids one batch delete accepts (`[limits]`, not `[research]`).
     * What bounds "select everything matching this filter" — and what lets the
     * panel say honestly that it stopped short. Absent on older servers.
     */
    max_delete_ids?: number;
    sampling?: ResearchSamplingInfo;
    /**
     * Measured cost per `(model, effort)`, as opposed to what the ladder *grants*.
     * A pair with too few runs to be meaningful is simply absent.
     */
    observed?: { refreshed_at: number | null; efforts: ResearchObservedEffort[] };
}
/**
 * What `/search` accepts, from `GET /config`.
 *
 * The bounds the server's edge validator enforces, so a form built from them cannot
 * offer an input that comes back a 400. Optional for the same reason `research` is:
 * an older server does not publish it, and the fallbacks in `SEARCH_LIMITS` stand in.
 */
export interface SearchConfigInfo {
    default_top_k: number;
    max_top_k: number;
    max_query_bytes: number;
}

/**
 * What to assume when the server does not publish its search bounds. These are the
 * server's own compiled defaults; a server that disagrees publishes the real ones,
 * which is the whole reason the block exists.
 */
export const SEARCH_LIMITS: SearchConfigInfo = {
    default_top_k: 5,
    max_top_k: 100,
    max_query_bytes: 32768,
};

export interface ConfigResponse {
    version: string;
    model_id: string;
    /**
     * Every language this server *supports* — the canonical list, not what any one
     * project contains. What a project actually holds is `ProjectStatsResponse`.
     */
    languages: string[];
    /** Optional: an older server does not publish its search bounds. */
    search?: SearchConfigInfo;
    /** Optional: an older server does not publish its research budgets. */
    research?: ResearchConfigInfo;
}
/** One language's inventory in a project, from `GET /projects/{guid}`. */
export interface LanguageStats {
    files: number;
    indexed_files: number;
    chunks_active: number;
    chunks_deleted: number;
}
/**
 * `GET /projects/{guid}` — what this project actually contains.
 *
 * `languages` is optional because an older server publishes a `chunks` map instead;
 * absent therefore means *unknown*, which is a different state from an empty object
 * (a project with nothing in it). The pickers fall back on the first and not the
 * second.
 */
export interface ProjectStatsResponse {
    project_guid: string;
    files: Record<string, number>;
    languages?: Record<string, LanguageStats>;
}

export type ResearchEffort = "low" | "medium" | "high";
/**
 * Per-request budget overriding the `effort` preset, axis by axis. Absent fields
 * keep the preset. `context_fraction` is deliberately not overridable — it guards
 * against silent transcript truncation and is not a quality lever; `search_top_k`
 * stays TOML-only for the same class of reason.
 *
 * An older server rejects the shape axes with a 400 (`deny_unknown_fields`) —
 * loud, which is the point of them riding inside `budget`.
 */
export interface ResearchBudget {
    max_seconds?: number;
    max_tokens?: number;
    max_steps?: number;
    /** `0` = announce no length; otherwise 150..=max_request_report_words. */
    max_report_words?: number;
    /** 3..=max_request_report_sections. */
    max_report_sections?: number;
    /** `0` = no checkpoints this run; otherwise 2..=max_request_steps. */
    checkpoint_every_steps?: number;
    /** 1..=max_evidence_width. */
    evidence_width?: number;
}
export interface ResearchRequest {
    question: string;
    model?: string;
    effort: ResearchEffort;
    budget?: ResearchBudget;
    /**
     * The files the run may see. Enforced by the server on EVERY lookup the model
     * makes, not only on search — so a scoped run cannot read its way out, and its
     * report can only speak about the scope it was given.
     */
    include?: SearchFilter;
    exclude?: SearchFilter;
    /** Pins sampling for a repeatable run; omit for the server's configured default. */
    seed?: number;
    /**
     * Earlier runs of this project whose reports are handed to the model as
     * background before it plans. Not evidence: the model is told it may not cite
     * them, and anything copied from one comes back `unverified`.
     */
    context_run_ids?: string[];
}
export interface ResearchStep {
    n: number;
    action:
        | "search"
        | "grep"
        | "symbols"
        | "outline"
        | "callers"
        | "list_files"
        | "read_chunks"
        | "file_history"
        | "note"
        | "revise_plan";
    /** Exactly one of these is present, per action: see the SSE contract. */
    query?: string;
    pattern?: string;
    name?: string;
    path?: string;
    glob?: string;
    text?: string;
    plan?: string;
    hits: number;
    /**
     * Where the call actually landed, as `path:start-end` — the same locations the
     * server scores citations against. `hits` alone says how many rows came back
     * and nothing about where, which is the difference between a trace you can
     * judge coverage from and a list of verbs. Absent on an older server; empty for
     * calls that read nothing (`note`, `revise_plan`) or return paths without spans
     * (`list_files`).
     */
    spans?: string[];
    /** The span list hit the server's per-frame cap; some were dropped. */
    spans_truncated?: boolean;
}
/**
 * Budget consumption of a live run. Emitted once before the first turn (limits,
 * nothing spent), then after every executed step and every completed turn — not on
 * a timer, so `elapsed_ms` is interpolated locally between events.
 */
export interface ResearchProgress {
    steps: number;
    max_steps: number;
    elapsed_ms: number;
    max_ms: number;
    /** prompt + eval, summed over turns: what the run cost the local GPU. */
    tokens: number;
    max_tokens: number;
    prompt_tokens: number;
    eval_tokens: number;
    peak_prompt_tokens: number;
    num_ctx: number;
    context_pct: number;
    turns: number;
    /**
     * Where the elapsed time went. A slow model and a busy GPU produce the same
     * `elapsed_ms` and want opposite remedies, so these four are what tell them
     * apart: `eval_tokens_per_second` is measured over `generation_ms` (Ollama's own
     * generation time), so a queued run still reports its true rate and the waiting
     * lands in `unaccounted_ms` instead of being averaged in. A non-zero
     * `model_load_ms` after the first turn means the model was evicted and reloaded
     * mid-run — something else wanted the device.
     *
     * All optional and all `0` when the Ollama in use reports no durations; an older
     * server omits them entirely.
     */
    generation_ms?: number;
    model_load_ms?: number;
    unaccounted_ms?: number;
    eval_tokens_per_second?: number;
    /**
     * The axis with the largest **share spent** — a maximum, not a warning. What
     * stopped a run is `done.reason`; read this with `shares` beside it, since a run
     * 12% into its clock and less into everything else also reports `"time"`.
     */
    binding?: "time" | "tokens" | "steps" | "context";
    /** The four shares `binding` is the maximum of, as percentages. */
    shares?: { time: number; tokens: number; steps: number; context: number };
}
/** `done` repeats every `progress` field and adds `reason`. */
export interface ResearchDone extends Partial<ResearchProgress> {
    steps: number;
    elapsed_ms: number;
    /**
     * Why the loop stopped: "finalized" (the model was satisfied) or one of
     * "time_exhausted" / "tokens_exhausted" / "budget_exhausted" /
     * "context_exhausted" / "unparseable" / "repeated_calls", meaning the report
     * was written on partial evidence. Optional — an older server omits it.
     */
    reason?: string;
    /**
     * Which generation of the server's research instructions drove this run.
     * Reports written under different prompts are not comparable; without this
     * nothing on the stream says which was in force. Optional — an older server
     * omits it.
     */
    prompt_version?: string;
    /**
     * The stored run this became — how to fetch the report later, and what to pass
     * as `context_run_ids` on a follow-up question.
     *
     * `null` when the server's best-effort journal write failed, and absent on a
     * server older than the field. Both mean the same thing to a client: the run
     * cannot be referenced, so do not offer to reuse it.
     */
    run_id?: string | null;
    /** Short per-project ordinal for display. Null/absent alongside `run_id`. */
    seq?: number | null;
}
/** One stored research run, as the list returns it — without its report. */
export interface ResearchRunSummary {
    /** Stable identity: what every per-run call keys on, and what goes in a URL. */
    id: string;
    /**
     * Per-project ordinal — short enough to show, and the keyset cursor. **Not**
     * identity: it is renumbered if a project's runs are ever wiped entirely.
     */
    seq: number;
    /**
     * The report's own stored heading when the run journalled one, else derived
     * server-side from the question. Never null.
     */
    title: string;
    question: string;
    created_at: number;
    /** When GC may reap it; `null` = pinned, never reaped. */
    expires_at: number | null;
    pinned: boolean;
    model: string;
    effort: string;
    done_reason: string;
    citations_total: number;
    citations_verified: number;
    citations_unverified: number;
    steps: number;
    elapsed_ms: number;
    /** Files the run read and recorded a baseline for. */
    files_total: number;
    /** How many of those have changed or left the index since. */
    files_moved: number;
    /** `files_moved > 0` — the report describes code that has since moved. */
    stale: boolean;
    /**
     * Derived validity: the run itself is fresh AND every run in its transitive
     * context still exists and is itself valid. The server refuses an invalid run
     * as context, so an unchecked-able row should not offer the checkbox.
     */
    valid: boolean;
    /** "stale" | "context_deleted" | "context_invalid"; null when valid. */
    invalid_reason: string | null;
    /**
     * How many runs this one was launched **on** — direct edges out. Not
     * `context.length`, which is the transitive ancestry: a run built on one
     * report that was itself built on three has `references_count = 1` and four
     * entries in `context`.
     */
    references_count: number;
    /**
     * How many other runs name this one in their context — direct edges in,
     * across the whole corpus rather than the loaded page. What makes a delete
     * confirmation honest: every one of them is invalidated by the delete.
     */
    referenced_by_count: number;
    /** Flat transitive context ancestry — every report this one leaned on. */
    context: ResearchRunDependency[];
    /** `research` or `challenge` — whether this run answered a question or attacked another run's report. */
    kind: string;
    /** For a challenge: the run it attacked; `null` on research runs. */
    challenged_run_id: string | null;
    /**
     * For a challenge: its overall verdict (`confirmed`/`disputed`/`refuted`),
     * or `null` — inconclusive, which is NOT an acquittal. `null` on research runs.
     */
    challenge_verdict: string | null;
    /**
     * Derived trust status of THIS run from valid challenges aimed at it:
     * `refuted` > `disputed` > `confirmed` > `unchallenged`. A stale challenge
     * stops counting automatically; an inconclusive one counts toward none.
     */
    trust: string;
    /**
     * For a challenge: the SUBJECT's `seq`, resolved server-side. `null` on a
     * research run, and on a challenge whose subject has been deleted — which is
     * now the only thing null means.
     *
     * Optional on the type because a 1.0.1 server does not send it: `undefined`
     * is "this server cannot say", `null` is "there is nothing to say".
     */
    challenged_seq?: number | null;
    /** For a challenge: the subject's title, by the same rule as `title`. */
    challenged_title?: string | null;
}

/**
 * Corpus-wide counts for one project, from the list endpoint.
 *
 * **No filter on the request affects these** — they are a fixed denominator, so
 * "74 of 128 current" keeps answering a question the visible page cannot while
 * the user types into the search box.
 */
export interface ResearchCorpusTotals {
    /** Every stored run of the project, of either kind. */
    total: number;
    /** How many are valid — the same predicate the server enforces on context. */
    current: number;
    /** The UNION of the four buckets below, unpinned only. Never their sum. */
    gc_candidates: number;
    gc_invalid: number;
    gc_stale: number;
    gc_partial: number;
    gc_inconclusive: number;
}

/** One run in another run's context chain (direct or transitive). */
export interface ResearchRunDependency {
    /** The id as recorded at launch — present even when the run is gone. */
    id: string;
    /** `null` when the run no longer exists. */
    seq: number | null;
    /** `null` when the run no longer exists — render a "deleted report" marker. */
    title: string | null;
    state: "valid" | "invalid" | "deleted";
}

export interface ResearchRunListResponse {
    runs: ResearchRunSummary[];
    /**
     * Pass as `beforeSeq` for the next page. `null` when the page came back short,
     * which is how the client knows to hide "Load more" without another request.
     */
    next_before_seq: number | null;
    /**
     * Corpus-wide counts. Optional: a 1.0.1 server omits the field entirely, and
     * the panel renders the counts line as "—" rather than inventing numbers.
     */
    totals?: ResearchCorpusTotals;
}

/** What became of one file a run read. */
export interface ResearchRunFile {
    path: string;
    sha256: string;
    current_sha256: string | null;
    /** "fresh" | "changed" | "removed" — an edit and a deletion read differently. */
    state: string;
}

export interface ResearchRunDetail extends ResearchRunSummary {
    /** The report, as Markdown. */
    report: string;
    prompt_version: string;
    context_run_ids: string[];
    scope: string | null;
    files: ResearchRunFile[];
}

/**
 * `POST /v0/{guid}/research/{run_id}/challenge` body. Deliberately minimal, and
 * the server refuses unknown fields: the question comes from the stored run, the
 * scope is the subject's own, and the context is the subject report itself —
 * prior reports are hearsay and must not feed a refutation. The caller only
 * chooses how hard the challenge tries.
 */
export interface ChallengeRequest {
    model?: string;
    effort: ResearchEffort;
    budget?: ResearchBudget;
    /** Pins sampling for a repeatable run; omit for the server's configured default. */
    seed?: number;
}

/** One set of citation-provenance counts, as the offline re-verification reports them. */
export interface CitationCounts {
    total: number;
    verified: number;
    path_only: number;
    unverified: number;
    stale: number;
}

/**
 * `GET /projects/{guid}/research/{run_id}/verification` — `check_citations`
 * re-run offline over the journalled evidence. No model, no GPU. Two separate
 * answers by design: **provenance** is immutable (`provenance_matches: false`
 * means the journal is wrong, never the code), while **staleness** is computed
 * against the index as it is now and is the number that moves between calls.
 */
export interface ResearchVerification {
    run_id: string;
    seq: number;
    valid: boolean;
    invalid_reason: string | null;
    /**
     * False for runs stored before evidence spans were journalled (pre-v1.3.0):
     * provenance cannot be recomputed for those, only staleness.
     */
    spans_available: boolean;
    /** The counters the run recorded when it finished. */
    recorded: CitationCounts;
    /** Recomputed from the journal; `null` when `spans_available` is false. */
    recomputed: CitationCounts | null;
    /** `null` when provenance could not be recomputed. */
    provenance_matches: boolean | null;
    stale_citations_now: number;
    stale_paths_now: string[];
    files_total: number;
    files_moved: number;
}

/** One in-flight research run, as `GET /research/active` lists it. */
export interface ActiveResearchRun {
    run_id: string;
    project_guid: string;
    /** A challenge appears with the server-synthesized "Challenge research #N: …". */
    question: string;
    model: string;
    effort: string;
    started_at: number;
    age_ms: number;
    granted_seconds: number;
    /** Past this, health calls the run wedged and the watchdog may cancel it. */
    worst_case_ms: number;
}

export interface ActiveResearchResponse {
    /** Oldest first. */
    runs: ActiveResearchRun[];
    slots_total: number;
    slots_busy: number;
}

/**
 * The server's provenance check on the report's `path:start-end` references,
 * emitted once between the report and `done`. Not a spell-check: it says how much
 * of the report is anchored in locations the investigation actually retrieved.
 */
export interface ResearchCitations {
    /**
     * The report was written by the *server*, not the model: the report window
     * expired first. Read this before any of the counts. A server-written report
     * contains no `path:start-end`, so it always scores `total: 0, verified: 0,
     * unverified: 0` — byte-for-byte what a flawless report scores, and
     * indistinguishable from one without this flag.
     *
     * Optional because a server older than the flag simply omits it; treat a
     * missing value as false, which is what it was before.
     */
    server_written?: boolean;
    /**
     * How many files this run's tools actually returned — the denominator the
     * counts never had. `verified: 0` over a non-zero `shown_paths` is a report
     * that cited none of what it read; over zero it is the honest "nothing in this
     * scope was shown to me", which the server's own grounding gate exempts and
     * which therefore arrives looking exactly like a clean run.
     *
     * Optional for the same reason as `server_written`: an older server omits it.
     */
    shown_paths?: number;
    /**
     * No tool returned a single path this run, *and* the run was holding somebody
     * else's report — prior context, or a challenge subject. This is the one case
     * where `shown_paths: 0` is not the honest empty-scope answer it looks like: the
     * report is that earlier prose restated with no evidence of its own. Refuse it
     * rather than re-asking with a wider scope.
     *
     * Not the same as `shown_paths === 0`: a run that called only `list_files` shows
     * nothing inside and is not hearsay. Optional; an older server omits it.
     */
    hearsay_only?: boolean;
    total: number;
    /** Path and an overlapping line range were both shown to the model. */
    verified: number;
    /** The file was shown, that line range was not. */
    path_only: number;
    /** No tool returned that path during the run — the model invented it. */
    unverified: number;
    unverified_paths: string[];
    /**
     * Citations scored against a path they did not spell — a bare filename that
     * named exactly one shown file. `verified` therefore means "a path a tool
     * returned, identified unambiguously from what the report wrote", and this says
     * how many leaned on the second half. Optional; an older server omits it.
     */
    path_resolved?: number;
    /**
     * Citations pointing into a file the index rewrote (or dropped) after the run
     * had read it. Independent of the three counts above: indexing is never blocked
     * by a research run, so a location the model really was shown can still describe
     * code that has been replaced.
     */
    stale: number;
    stale_paths: string[];
    /**
     * Set only when the first draft failed this same check and was sent back for
     * correction; null otherwise. The counts above always describe the report that
     * was actually shown, so these are the only way to tell a report that was
     * right the first time from one that was repaired.
     */
    draft_unverified: number | null;
    draft_path_only: number | null;
    draft_stale: number | null;
    /** Tool calls the correction pass spent re-reading what the draft cited. */
    revalidation_steps: number | null;
}

/** One location's indexed code, shipped verbatim beside the report that cites it. */
export interface ResearchExcerpt {
    path: string;
    start_line: number;
    end_line: number;
    code: string;
}

/**
 * The indexed code at every verified citation, emitted once between `citations`
 * and `done` — and only when the report has at least one.
 *
 * The server already holds these bytes, so quoting them costs one query and no
 * model tokens. That is the point: asking a report to reproduce a file is the most
 * reliable way to make a run fail, so the report cites and the server quotes.
 */
/** One claim's verdict inside a challenge's `verdict` event. */
export interface ResearchClaimVerdict {
    claim: string;
    verdict: "confirmed" | "disputed" | "refuted";
}

/**
 * A challenge run's conclusion about the stored report it attacked. `overall`
 * is `null` when the verdict turn parsed to nothing — challenged, inconclusive,
 * never an acquittal. `grounded: false` means the challenge's own report
 * verified no citations, which capped `overall` at `disputed`.
 */
export interface ResearchVerdict {
    challenged_run_id: string;
    overall: "confirmed" | "disputed" | "refuted" | null;
    grounded: boolean;
    claims: ResearchClaimVerdict[];
}

export interface ResearchExcerpts {
    excerpts: ResearchExcerpt[];
    /** Verified citations found, before the server's caps. */
    total: number;
    /** Some code did not fit the caps; whole chunks were dropped, never cut. */
    truncated: boolean;
}

/**
 * The run's identity, streamed before any work — always the first frame.
 *
 * `run_id` names the run for its whole life: it is what `GET /research/active`
 * lists and what `DELETE /research/active/{run_id}` cancels. Before it existed an
 * id arrived only with `done`, so a *running* job could not be named at all.
 */
export interface ResearchStarted {
    run_id: string;
    model: string;
    effort: string;
    granted_seconds: number;
    /**
     * `granted_seconds * 1000 + report_timeout_ms`. The two bound different phases,
     * so this sum — not `granted_seconds` — is how long the run may take.
     */
    worst_case_ms: number;
}

/** One-way SSE events of a research stream (wire contract of POST /research). */
export interface ResearchCallbacks {
    /** Optional: a server older than the started frame never fires it. */
    onStarted?(started: ResearchStarted): void;
    onThinking(text: string): void;
    onStep(step: ResearchStep): void;
    onProgress(progress: ResearchProgress): void;
    onSummary(text: string): void;
    onCitations(citations: ResearchCitations): void;
    /** Optional: a server older than the excerpt channel never fires it. */
    onExcerpts?(excerpts: ResearchExcerpts): void;
    /**
     * A challenge stream's conclusion about its subject, after `excerpts` and
     * before `done`. Ordinary research streams never emit it. Optional on both
     * ends, like `excerpts`.
     */
    onVerdict?(verdict: ResearchVerdict): void;
    onDone(info: ResearchDone): void;
    /** A server-side failure after the stream started (HTTP status was already 200). */
    onError(code: string, detail: string): void;
}

export interface ApiOptions {
    serverUrl: string;
    noVerify: boolean;
    /**
     * Extra CA (PEM contents, already read from disk) to trust for the server.
     * Reading it is the *caller's* job on purpose: an unreadable file must degrade
     * to a warning, and a constructor that throws on it takes the whole extension
     * down — including `noVerify`, the one setting that would have got past it.
     */
    ca?: Buffer;
    protocol?: string;
    /**
     * Sent as `X-Api-Key` on every request. mindex has no authentication of its
     * own and ignores the header; it is for a reverse proxy in front of it that
     * refuses requests without a known key. Undefined sends no header, which is
     * what a direct `https://127.0.0.1:11111` connection wants.
     */
    apiKey?: string;
    /**
     * Deadline for an ordinary request, in ms. `0` disables it.
     *
     * Not optional decoration: without a deadline a half-open socket leaves the
     * promise pending forever, and since the status poll re-arms itself in a
     * `.finally()`, one such socket stopped health polling for the rest of the
     * session and froze the indicator at whatever colour it happened to be.
     */
    timeoutMs?: number;
    /**
     * How long a stream may say nothing before it is treated as dead, in ms.
     * `0` disables it. See `streamRequest` for why this is an *idle* clock and
     * never a total one.
     */
    streamIdleMs?: number;
}

/** A request that has answered nothing at all. */
export const DEFAULT_TIMEOUT_MS = 15_000;
/**
 * The status poll's own, deliberately under every poll interval — a poll that
 * outlives its period stacks, and the shortest period is the 3 s busy poll.
 */
export const HEALTH_TIMEOUT_MS = 5_000;
/**
 * A stream that has gone quiet. Derived, not guessed: the longest *legitimate*
 * silence on the research path is a turn that produces nothing, which the server
 * already bounds at `[research].first_token_timeout_ms` and `report_timeout_ms`
 * (120 s each), plus a minute of slack for a model being loaded on busy hardware.
 */
export const STREAM_IDLE_TIMEOUT_MS = 180_000;

function asString(v: unknown, fallback = ""): string {
    return typeof v === "string" ? v : fallback;
}

/**
 * Parse one SSE frame (`event:` + `data:` lines) into its event name and decoded
 * JSON payload. Returns undefined for keep-alive comments, empty frames and
 * malformed JSON — skipping one frame beats killing a live stream.
 */
export function parseSseFrame(frame: string): { event: string; data: unknown } | undefined {
    let event = "message";
    const dataLines: string[] = [];
    for (const line of frame.split("\n")) {
        if (line.startsWith("event:")) {
            event = line.slice("event:".length).trim();
        } else if (line.startsWith("data:")) {
            dataLines.push(line.slice("data:".length).trimStart());
        }
        // `:keep-alive` comments and unknown fields are ignored per the SSE spec.
    }
    if (dataLines.length === 0) {
        return undefined;
    }
    try {
        return { event, data: JSON.parse(dataLines.join("\n")) };
    } catch {
        return undefined; // malformed frame — skip rather than kill the stream
    }
}

/** Parse one SSE frame (`event:` + `data:` lines) and route it to the callbacks. */
function dispatchSseFrame(frame: string, cb: ResearchCallbacks): void {
    const parsed = parseSseFrame(frame);
    if (parsed === undefined) {
        return;
    }
    const { event, data } = parsed;
    const d = data as Record<string, unknown>;
    switch (event) {
        case "started":
            // Optional on both ends: an older server never sends it, and a view
            // that does not render the run id need not implement it.
            cb.onStarted?.(d as unknown as ResearchStarted);
            break;
        case "thinking":
            cb.onThinking(asString(d.text));
            break;
        case "step":
            cb.onStep(d as unknown as ResearchStep);
            break;
        case "progress":
            cb.onProgress(d as unknown as ResearchProgress);
            break;
        case "summary":
            cb.onSummary(asString(d.text));
            break;
        case "citations":
            cb.onCitations(d as unknown as ResearchCitations);
            break;
        case "excerpts":
            // Optional on both ends: an older server never sends it, and a view
            // that does not render it need not implement it.
            cb.onExcerpts?.(d as unknown as ResearchExcerpts);
            break;
        case "verdict":
            // Challenge streams only — see ResearchVerdict.
            cb.onVerdict?.(d as unknown as ResearchVerdict);
            break;
        case "done":
            cb.onDone(d as unknown as ResearchDone);
            break;
        case "error":
            cb.onError(asString(d.code, "unknown"), asString(d.detail));
            break;
        default:
            break;
    }
}

/**
 * Route one streaming-/index SSE frame: progress events go to the callbacks,
 * terminals (`done`/`error`) are returned so the caller can settle its promise.
 * Unknown events and malformed frames yield undefined. Exported for tests.
 */
export function routeIndexFrame(
    frame: string,
    cb: IndexStreamCallbacks
): { done?: IndexDoneEvent; error?: { code: string; detail: string } } | undefined {
    const parsed = parseSseFrame(frame);
    if (parsed === undefined) {
        return undefined;
    }
    const d = parsed.data as Record<string, unknown>;
    switch (parsed.event) {
        case "started":
            cb.onStarted?.(d as unknown as IndexStartedEvent);
            break;
        case "prepared":
            cb.onPrepared?.(d as unknown as IndexPreparedEvent);
            break;
        case "skipped":
            cb.onSkipped?.(d as unknown as IndexSkippedEvent);
            break;
        case "embedded":
            cb.onEmbedded?.(d as unknown as IndexEmbeddedEvent);
            break;
        case "indexed":
            cb.onIndexed?.(d as unknown as IndexIndexedEvent);
            break;
        case "done":
            return { done: d as unknown as IndexDoneEvent };
        case "error":
            return {
                error: { code: asString(d.code, "unknown"), detail: asString(d.detail) },
            };
        default:
            break;
    }
    return undefined;
}

export class MindexApi {
    private readonly base: string;
    private readonly protocol: string;
    private readonly agent: https.Agent;
    private readonly apiKey?: string;
    /**
     * The same TLS settings the agent carries, repeated on every request.
     * Redundant against a plain Node `https`, and not against VS Code's: with
     * `http.proxySupport` at its default `"override"` the extension host patches
     * `https.request` and may substitute its own proxy agent — which silently
     * discards ours, and with it `rejectUnauthorized`. Options on the request
     * itself are forwarded, so this is what makes `noVerify` mean something
     * behind a corporate proxy.
     */
    private readonly tls: { rejectUnauthorized: boolean; ca?: Buffer };
    /** Not `readonly`: `withTimeout` shadows it on a derived view. */
    private timeoutMs: number;
    private readonly streamIdleMs: number;

    constructor(opts: ApiOptions) {
        this.base = opts.serverUrl.replace(/\/+$/, "");
        this.protocol = opts.protocol ?? "v0";
        this.apiKey = opts.apiKey && opts.apiKey !== "" ? opts.apiKey : undefined;
        this.timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
        this.streamIdleMs = opts.streamIdleMs ?? STREAM_IDLE_TIMEOUT_MS;
        // `noVerify` wins over `ca`: with verification off the extra CA can only
        // confuse the picture, and a user who turned it on is asking to connect
        // regardless of what the certificate says.
        this.tls = {
            rejectUnauthorized: !opts.noVerify,
            ca: opts.noVerify ? undefined : opts.ca,
        };
        this.agent = new https.Agent({ ...this.tls, keepAlive: true });
    }

    dispose(): void {
        this.agent.destroy();
    }

    /**
     * The same client with a stricter clock.
     *
     * Shares the agent, the TLS settings and the key — it is the same connection
     * pool, differing only in how long it will wait. That is what the status
     * poll needs and what a per-call `timeoutMs` parameter on nine methods would
     * have cost: the poll calls five different endpoints and every one of them
     * must be bounded by the *poll's* deadline rather than by the default.
     *
     * A view must not be disposed — it would destroy the parent's agent.
     */
    withTimeout(timeoutMs: number): MindexApi {
        const view = Object.create(this) as MindexApi;
        view.timeoutMs = timeoutMs;
        return view;
    }

    // ---- data plane ----

    search(guid: string, req: SearchRequest, signal?: AbortSignal): Promise<SearchResponse> {
        return this.request(
            "POST",
            `/${this.protocol}/${guid}/search`,
            req,
            signal
        ) as Promise<SearchResponse>;
    }

    // ---- management ----

    drift(
        guid: string,
        manifest: Record<string, string>,
        signal?: AbortSignal
    ): Promise<DriftResponse> {
        return this.request(
            "POST",
            `/projects/${guid}/drift`,
            { files: manifest },
            signal
        ) as Promise<DriftResponse>;
    }

    /** Empty selector = requeue every failed file. Returns requeued count (204 → 0). */
    async retry(guid: string, selector?: Selector): Promise<number> {
        const body = (await this.request(
            "POST",
            `/projects/${guid}/retry`,
            selector ?? {}
        )) as {
            requeued_files: number;
        } | null;
        return body?.requeued_files ?? 0;
    }

    /** Cancels in-flight indexing for the selector. Returns cancelled count (204 → 0). */
    async cancel(guid: string, selector: Selector): Promise<number> {
        const body = (await this.request("POST", `/projects/${guid}/cancel`, selector)) as {
            cancelled_files: number;
        } | null;
        return body?.cancelled_files ?? 0;
    }

    /** Soft-deletes files matching the selector. Returns deleted count (204 → 0). */
    async deleteFiles(guid: string, selector: Selector): Promise<number> {
        const body = (await this.request("DELETE", `/projects/${guid}/files`, selector)) as {
            deleted_files: number;
        } | null;
        return body?.deleted_files ?? 0;
    }

    listFiles(
        guid: string,
        filter?: { status?: string; language?: string },
        signal?: AbortSignal
    ): Promise<{ files: FileEntry[] }> {
        const params = new URLSearchParams();
        if (filter?.status) {
            params.set("status", filter.status);
        }
        if (filter?.language) {
            params.set("language", filter.language);
        }
        const qs = params.size > 0 ? `?${params.toString()}` : "";
        return this.request(
            "GET",
            `/projects/${guid}/files${qs}`,
            undefined,
            signal
        ) as Promise<{ files: FileEntry[] }>;
    }

    /**
     * One keyset page of stored research runs, newest first, without their reports.
     *
     * **Keyset, not offset**: pass the previous page's `next_before_seq` as
     * `beforeSeq`. A run written or reaped between two pages then cannot make the
     * reader skip or repeat a row, which is exactly what `OFFSET` would do over a
     * table that GC prunes and every run appends to.
     *
     * Takes a `signal` — unlike `listFiles` — because this is typed into: every
     * keystroke supersedes the request before it. Note `request` **rejects** on
     * abort, so the caller must swallow `AbortError` itself.
     */
    listResearchRuns(
        guid: string,
        query?: {
            q?: string;
            beforeSeq?: number;
            limit?: number;
            freshness?: "all" | "fresh" | "stale";
            pinned?: boolean;
            /** Restrict to fully-valid (`true`) or invalid (`false`) runs. */
            valid?: boolean;
            /** Restrict to ordinary research runs or to challenges. */
            kind?: "research" | "challenge";
            /**
             * Restrict to challenges aimed at this run — "what was said about
             * *that* report". Finds the stale and inconclusive challenges that
             * `trust` deliberately stops counting, which is why the panel can no
             * longer skip the lookup when trust reads `unchallenged`.
             */
            challengedRunId?: string;
            /** Whether the run reached its own conclusion or a budget stopped it. */
            completeness?: "all" | "finalized" | "partial";
        },
        signal?: AbortSignal
    ): Promise<ResearchRunListResponse> {
        const params = new URLSearchParams();
        if (query?.q) {
            params.set("q", query.q);
        }
        if (query?.beforeSeq !== undefined) {
            params.set("before_seq", String(query.beforeSeq));
        }
        if (query?.limit !== undefined) {
            params.set("limit", String(query.limit));
        }
        if (query?.freshness && query.freshness !== "all") {
            params.set("freshness", query.freshness);
        }
        if (query?.pinned !== undefined) {
            params.set("pinned", String(query.pinned));
        }
        if (query?.valid !== undefined) {
            params.set("valid", String(query.valid));
        }
        if (query?.kind !== undefined) {
            params.set("kind", query.kind);
        }
        if (query?.challengedRunId !== undefined) {
            params.set("challenged_run_id", query.challengedRunId);
        }
        if (query?.completeness && query.completeness !== "all") {
            params.set("completeness", query.completeness);
        }
        const qs = params.size > 0 ? `?${params.toString()}` : "";
        return this.request(
            "GET",
            `/projects/${guid}/research${qs}`,
            undefined,
            signal
        ) as Promise<ResearchRunListResponse>;
    }

    /** One stored run in full, including its Markdown report and per-file freshness. */
    getResearchRun(
        guid: string,
        runId: string,
        signal?: AbortSignal
    ): Promise<ResearchRunDetail> {
        return this.request(
            "GET",
            `/projects/${guid}/research/${encodeURIComponent(runId)}`,
            undefined,
            signal
        ) as Promise<ResearchRunDetail>;
    }

    /**
     * Exempt a run from the retention sweep, or return it to it. Returns the updated
     * summary, so the caller renders the server's answer rather than its own guess.
     */
    pinResearchRun(guid: string, runId: string, pinned: boolean): Promise<ResearchRunSummary> {
        return this.request(
            "POST",
            `/projects/${guid}/research/${encodeURIComponent(runId)}/pin`,
            { pinned }
        ) as Promise<ResearchRunSummary>;
    }

    /** Drop one stored run. Idempotent — deleting one that is already gone is a 204. */
    async deleteResearchRun(guid: string, runId: string): Promise<void> {
        await this.request(
            "DELETE",
            `/projects/${guid}/research/${encodeURIComponent(runId)}`
        );
    }

    /**
     * Drop a batch of stored runs in one transaction. Returns how many rows
     * actually went — never more than `ids.length`, and fewer when some were
     * already gone (unknown ids are ignored, like the single-run delete).
     *
     * An **empty** `ids` is a 400 server-side, not a whole-corpus wipe, so the
     * caller must not "helpfully" send one for "delete everything".
     */
    async deleteResearchRuns(guid: string, ids: string[]): Promise<number> {
        const body = (await this.request("DELETE", `/projects/${guid}/research`, {
            ids,
        })) as { deleted_runs: number } | null;
        return body?.deleted_runs ?? 0;
    }

    /**
     * The project's language inventory + file counts by status. A project the server
     * has never seen is a 404 (a `ProblemError`), which callers must read as *not
     * known yet* rather than as an empty index.
     */
    projectStats(guid: string, signal?: AbortSignal): Promise<ProjectStatsResponse> {
        return this.request(
            "GET",
            `/projects/${guid}`,
            undefined,
            signal
        ) as Promise<ProjectStatsResponse>;
    }

    /**
     * POST /research — a long-lived one-way SSE stream. Resolves when the server
     * closes the stream (after `done`/`error`), rejects on transport failure or a
     * non-2xx response. Aborting `signal` closes the connection, which IS the
     * cancellation interface: the server cancels the job on disconnect. No
     * reconnects, by contract.
     */
    research(
        guid: string,
        req: ResearchRequest,
        cb: ResearchCallbacks,
        signal: AbortSignal
    ): Promise<void> {
        return this.streamRequest(`/${this.protocol}/${guid}/research`, req, signal, {
            onFrame: (frame) => dispatchSseFrame(frame, cb),
            // An abort is the user's cancel, not a failure.
            abortResolves: true,
        });
    }

    /**
     * POST /research/{run_id}/challenge — the same one-way SSE stream as
     * [`research`], pointed at a stored report. One extra frame arrives on this
     * stream only: `verdict`, after `excerpts` and before `done`. Same abort
     * semantics: aborting `signal` is the user's cancel and resolves.
     */
    challenge(
        guid: string,
        runId: string,
        req: ChallengeRequest,
        cb: ResearchCallbacks,
        signal: AbortSignal
    ): Promise<void> {
        return this.streamRequest(
            `/${this.protocol}/${guid}/research/${encodeURIComponent(runId)}/challenge`,
            req,
            signal,
            {
                onFrame: (frame) => dispatchSseFrame(frame, cb),
                abortResolves: true,
            }
        );
    }

    /**
     * The offline re-verification of one stored run. Cheap and side-effect-free;
     * staleness is recomputed against the index *now*, so calling it again after a
     * reindex is the point, not a waste.
     */
    getResearchVerification(
        guid: string,
        runId: string,
        signal?: AbortSignal
    ): Promise<ResearchVerification> {
        return this.request(
            "GET",
            `/projects/${guid}/research/${encodeURIComponent(runId)}/verification`,
            undefined,
            signal
        ) as Promise<ResearchVerification>;
    }

    /**
     * The research runs holding a semaphore slot right now — global, not per
     * project, because the semaphore is. What the 429 `research.busy` points at.
     */
    activeResearch(signal?: AbortSignal): Promise<ActiveResearchResponse> {
        return this.request(
            "GET",
            "/research/active",
            undefined,
            signal
        ) as Promise<ActiveResearchResponse>;
    }

    /**
     * Cancel a live run by id — the hand that reaches a run whose caller
     * abandoned it while its socket stayed open. 204 always, idempotent; the slot
     * may take a moment to free as the job unwinds.
     */
    async cancelActiveResearch(runId: string): Promise<void> {
        await this.request("DELETE", `/research/active/${encodeURIComponent(runId)}`);
    }

    /**
     * `POST /index?stream=yes` — an upload reported live: per-file
     * `prepared`/`skipped`/`indexed` and per-embed-batch `embedded` events reach
     * `cb` as the server works, and the resolved value is the `IndexResponse` the
     * JSON mode would have returned (`done.files`). `force` bypasses the server's
     * unchanged-skip (content hash *and* derivation versions), so every posted
     * file is re-sliced and re-embedded; routine reindexing leaves it off.
     *
     * An older server that does not know the query answers plain JSON. That
     * degrades transparently — the body is parsed as the response — but not
     * silently: `cb.onJsonFallback` fires, because a run whose numbers came from
     * two batch responses instead of a live stream is a different thing to read.
     * Aborting `signal` rejects (unlike [`research`]) so a caller can tell the
     * user's cancel from an empty result; the disconnect is what cancels the
     * server-side work.
     */
    async indexStream(
        guid: string,
        files: IndexFiles,
        cb: IndexStreamCallbacks,
        signal: AbortSignal,
        force = false
    ): Promise<IndexResponse> {
        let done: IndexDoneEvent | undefined;
        let streamError: { code: string; detail: string } | undefined;
        let jsonBody: IndexResponse | undefined;
        await this.streamRequest(
            `/${this.protocol}/${guid}/index?stream=yes`,
            { files, force },
            signal,
            {
                onFrame: (frame) => {
                    const out = routeIndexFrame(frame, cb);
                    if (out?.done !== undefined) {
                        done = out.done;
                    }
                    if (out?.error !== undefined) {
                        streamError = out.error;
                    }
                },
                onJson: (text) => {
                    // Fired before the parse: a body this client cannot read is
                    // still a request that produced no events, and the caller has
                    // to know that before it decides what its counters mean.
                    cb.onJsonFallback?.();
                    try {
                        jsonBody = JSON.parse(text) as IndexResponse;
                    } catch {
                        // fall through to the "ended without done" error below
                    }
                },
                abortResolves: false,
            }
        );
        if (streamError !== undefined) {
            // The HTTP status was already 200 when the failure happened.
            throw new ProblemError(200, streamError.code, streamError.detail);
        }
        if (done !== undefined) {
            // The three fields the response body does not carry — `files_indexed`,
            // `chunks` and the server's own `elapsed_ms` — reach the caller only
            // here. The promise still settles on the same terminal.
            cb.onDone?.(done);
            return { files: done.files };
        }
        if (jsonBody !== undefined) {
            return jsonBody;
        }
        throw new UnreachableError(
            new Error("index stream ended without a terminal done/error event")
        );
    }

    /**
     * Shared SSE plumbing for the two streaming endpoints. Resolves when the
     * server closes the stream, rejects on transport failure or a non-2xx
     * response. When the server answers with a non-SSE content type the whole
     * body is buffered into `onJson` instead (how an older server that ignores
     * `?stream=yes` degrades). `abortResolves` picks the abort contract: research
     * resolves (a cancel is not a failure), index rejects (its caller tells a
     * cancel apart from an empty result) — both callers depend on their side, so
     * this cannot be unified further.
     *
     * **The clock here is idle-only, never total.** A legitimate `high` research
     * run lives up to the server's 70-minute ceiling, so any total deadline the
     * client could pick would eventually kill a run that was working. What is
     * *not* legitimate is silence: the server bounds every turn's silent prefix
     * itself, so nothing arriving for `streamIdleMs` means the far end is gone.
     * The clock starts at the ordinary response timeout — before any frame there
     * is nothing to be patient about, admission being immediate or a 429 — and
     * relaxes to `streamIdleMs` once the stream is live.
     *
     * A timeout must not take the `abortResolves` path: a silent stream is a
     * failure and has to be reported as one, where the user's Stop is not.
     */
    private streamRequest(
        path: string,
        body: unknown,
        signal: AbortSignal,
        handlers: {
            onFrame: (frame: string) => void;
            onJson?: (text: string) => void;
            abortResolves: boolean;
        }
    ): Promise<void> {
        const url = new URL(this.base + path);
        const payload = Buffer.from(JSON.stringify(body), "utf8");
        const firstFrameMs = this.timeoutMs;
        const idleMs = this.streamIdleMs;

        return new Promise((resolve, reject) => {
            let clock: NodeJS.Timeout | undefined;
            const disarm = (): void => {
                if (clock !== undefined) {
                    clearTimeout(clock);
                    clock = undefined;
                }
            };
            /** Re-arm at `ms`; `0` disarms for good (see the legacy path below). */
            const arm = (ms: number, phase: "response" | "idle"): void => {
                disarm();
                if (ms > 0) {
                    clock = setTimeout(() => request.destroy(new TimeoutError(ms, phase)), ms);
                }
            };
            const ok = (): void => {
                disarm();
                resolve();
            };
            const fail = (e: Error): void => {
                disarm();
                reject(e);
            };

            const request = https.request(
                url,
                {
                    method: "POST",
                    headers: {
                        "Content-Type": "application/json",
                        "Content-Length": payload.length,
                        Accept: "text/event-stream",
                        ...(this.apiKey ? { "X-Api-Key": this.apiKey } : {}),
                    },
                    agent: this.agent,
                    ...this.tls,
                    signal,
                },
                (res) => {
                    const status = res.statusCode ?? 0;
                    if (status < 200 || status >= 300) {
                        const chunks: Buffer[] = [];
                        res.on("data", (c: Buffer) => chunks.push(c));
                        res.on("end", () => {
                            const text = Buffer.concat(chunks).toString("utf8");
                            let problem: ProblemDetails = {};
                            try {
                                problem = JSON.parse(text) as ProblemDetails;
                            } catch {
                                // non-problem+json body — keep the raw text
                            }
                            fail(
                                new ProblemError(
                                    status,
                                    problem.code ?? `http.${status}`,
                                    problem.detail ?? problem.title ?? text.slice(0, 200)
                                )
                            );
                        });
                        return;
                    }

                    const contentType = res.headers["content-type"] ?? "";
                    if (
                        handlers.onJson !== undefined &&
                        !contentType.startsWith("text/event-stream")
                    ) {
                        // The legacy degradation: an older server ignored
                        // `?stream=yes` and is buffering a whole synchronous
                        // index. There is no progress to measure and no bound to
                        // pick — a full pass over a large repo is legitimately
                        // minutes of silence — so this path keeps its historical
                        // behaviour of waiting.
                        disarm();
                        const chunks: Buffer[] = [];
                        res.on("data", (c: Buffer) => chunks.push(c));
                        res.on("end", () => {
                            handlers.onJson?.(Buffer.concat(chunks).toString("utf8"));
                            ok();
                        });
                        res.on("error", (e) => fail(new UnreachableError(e)));
                        return;
                    }

                    res.setEncoding("utf8");
                    let buf = "";
                    res.on("data", (chunk: string) => {
                        arm(idleMs, "idle");
                        buf += chunk;
                        // SSE frames are separated by a blank line.
                        let sep;
                        while ((sep = buf.indexOf("\n\n")) !== -1) {
                            const frame = buf.slice(0, sep);
                            buf = buf.slice(sep + 2);
                            handlers.onFrame(frame);
                        }
                    });
                    res.on("end", () => ok());
                    res.on("error", (e) => fail(new UnreachableError(e)));
                }
            );
            arm(firstFrameMs, "response");
            request.on("error", (e) => {
                // A timeout is a failure even on the paths where an abort is
                // not: the run stopped answering, which the caller has to be
                // told, where a cancel is something the caller asked for.
                if (e instanceof TimeoutError) {
                    fail(e);
                } else if (e.name === "AbortError" && handlers.abortResolves) {
                    ok();
                } else if (e.name === "AbortError") {
                    fail(e);
                } else {
                    fail(new UnreachableError(e));
                }
            });
            request.write(payload);
            request.end();
        });
    }

    // ---- observability ----

    /**
     * Bounded tighter than everything else, and unconditionally: this is the
     * call the poll loop makes, and the poll re-arms only after it settles.
     */
    health(signal?: AbortSignal): Promise<HealthResponse> {
        const clock = Math.min(this.timeoutMs || HEALTH_TIMEOUT_MS, HEALTH_TIMEOUT_MS);
        return this.withTimeout(clock).request(
            "GET",
            "/health",
            undefined,
            signal
        ) as Promise<HealthResponse>;
    }

    status(signal?: AbortSignal): Promise<StatusResponse> {
        return this.request("GET", "/status", undefined, signal) as Promise<StatusResponse>;
    }

    config(signal?: AbortSignal): Promise<ConfigResponse> {
        return this.request("GET", "/config", undefined, signal) as Promise<ConfigResponse>;
    }

    // ---- plumbing ----

    private request(
        method: string,
        path: string,
        body?: unknown,
        signal?: AbortSignal
    ): Promise<unknown> {
        const url = new URL(this.base + path);
        const payload =
            body === undefined ? undefined : Buffer.from(JSON.stringify(body), "utf8");

        const timeoutMs = this.timeoutMs;

        return new Promise((resolve, reject) => {
            const headers: http.OutgoingHttpHeaders = { Accept: "application/json" };
            if (this.apiKey) {
                headers["X-Api-Key"] = this.apiKey;
            }
            if (payload) {
                headers["Content-Type"] = "application/json";
                headers["Content-Length"] = payload.length;
            }
            /**
             * The second clock. `req.setTimeout` below only measures socket
             * *inactivity*, which a peer dribbling one byte at a time resets
             * forever — so the deadline that actually bounds the call is this
             * one. Twice the budget, because a slow-but-progressing transfer is
             * a different (and legitimate) thing from a stalled one.
             */
            let total: NodeJS.Timeout | undefined;
            const done = <T>(settle: (v: T) => void) => {
                return (v: T) => {
                    if (total !== undefined) {
                        clearTimeout(total);
                        total = undefined;
                    }
                    settle(v);
                };
            };
            const ok = done(resolve);
            const fail = done(reject);

            const req = https.request(
                url,
                { method, headers, agent: this.agent, ...this.tls, signal },
                (res) => {
                    const chunks: Buffer[] = [];
                    res.on("data", (c: Buffer) => chunks.push(c));
                    res.on("end", () => {
                        const status = res.statusCode ?? 0;
                        const text = Buffer.concat(chunks).toString("utf8");
                        if (status === 204) {
                            ok(null);
                            return;
                        }
                        if (status >= 200 && status < 300) {
                            try {
                                ok(JSON.parse(text));
                            } catch (e) {
                                // Something answered and it was not JSON. Not
                                // "unreachable": the remedy is about what is
                                // listening on that URL, not about starting it.
                                fail(new MalformedResponseError(e));
                            }
                            return;
                        }
                        let problem: ProblemDetails = {};
                        try {
                            problem = JSON.parse(text) as ProblemDetails;
                        } catch {
                            // non-problem+json body (proxy, hard crash) — keep the raw text
                        }
                        fail(
                            new ProblemError(
                                status,
                                problem.code ?? `http.${status}`,
                                problem.detail ?? problem.title ?? text.slice(0, 200)
                            )
                        );
                    });
                    res.on("error", (e) => fail(new UnreachableError(e)));
                }
            );
            if (timeoutMs > 0) {
                req.setTimeout(timeoutMs, () =>
                    req.destroy(new TimeoutError(timeoutMs, "response"))
                );
                total = setTimeout(
                    () => req.destroy(new TimeoutError(timeoutMs, "response")),
                    timeoutMs * 2
                );
            }
            req.on("error", (e) => {
                // Order matters: a timeout wrapped as `UnreachableError` reaches
                // the user as "is the server running?", which is both wrong and
                // the first thing they have already checked.
                if (e instanceof TimeoutError || e.name === "AbortError") {
                    fail(e);
                } else {
                    fail(new UnreachableError(e));
                }
            });
            if (payload) {
                req.write(payload);
            }
            req.end();
        });
    }
}
