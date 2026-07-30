import * as https from "node:https";
import * as http from "node:http";
import * as fs from "node:fs";
import { ProblemDetails, ProblemError, UnreachableError } from "./errors";

// ---- wire types (src/backend/v0/models.rs) ----

export type IndexFiles = Record<string, Record<string, { code: string }>>;
export interface IndexResponse {
    files: Record<string, Record<string, number>>;
}

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
    /** Required dependencies only (SQLite, Qdrant, embedder); `checks.ollama` never
     *  degrades it — see below. */
    status: "ok" | "degraded";
    version: string;
    indexing_files: number;
    /**
     * Per-dependency liveness, `"ok"` or `"error: <reason>"`. Rendered generically,
     * so a check added server-side shows up without a client change. `ollama` is
     * optional (only `/research` needs it) and absent on servers before it existed —
     * hence the `| undefined`.
     */
    checks: Record<string, string | undefined> & { ollama?: string };
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
    /**
     * How long the report phase gets after the investigation deadline. The other
     * half of what a caller waits: `effort.*.max_seconds` bounds the investigation,
     * and the longest a request can take is that plus this.
     */
    report_timeout_ms?: number;
    sampling?: ResearchSamplingInfo;
}
export interface ConfigResponse {
    version: string;
    model_id: string;
    /**
     * Every language this server *supports* — the canonical list, not what any one
     * project contains. What a project actually holds is `ProjectStatsResponse`.
     */
    languages: string[];
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
 * against silent transcript truncation and is not a quality lever.
 */
export interface ResearchBudget {
    max_seconds?: number;
    max_tokens?: number;
    max_steps?: number;
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
    /** The axis closest to exhaustion — what this run will run out of. */
    binding?: "time" | "tokens" | "steps" | "context";
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
}
/**
 * The server's provenance check on the report's `path:start-end` references,
 * emitted once between the report and `done`. Not a spell-check: it says how much
 * of the report is anchored in locations the investigation actually retrieved.
 */
export interface ResearchCitations {
    total: number;
    /** Path and an overlapping line range were both shown to the model. */
    verified: number;
    /** The file was shown, that line range was not. */
    path_only: number;
    /** No tool returned that path during the run — the model invented it. */
    unverified: number;
    unverified_paths: string[];
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
/** One-way SSE events of a research stream (wire contract of POST /research). */
export interface ResearchCallbacks {
    onThinking(text: string): void;
    onStep(step: ResearchStep): void;
    onProgress(progress: ResearchProgress): void;
    onSummary(text: string): void;
    onCitations(citations: ResearchCitations): void;
    onDone(info: ResearchDone): void;
    /** A server-side failure after the stream started (HTTP status was already 200). */
    onError(code: string, detail: string): void;
}

export interface ApiOptions {
    serverUrl: string;
    noVerify: boolean;
    caCertPath?: string;
    protocol?: string;
    /**
     * Sent as `X-Api-Key` on every request. mindex has no authentication of its
     * own and ignores the header; it is for a reverse proxy in front of it that
     * refuses requests without a known key. Undefined sends no header, which is
     * what a direct `https://127.0.0.1:11111` connection wants.
     */
    apiKey?: string;
}

function asString(v: unknown, fallback = ""): string {
    return typeof v === "string" ? v : fallback;
}

/** Parse one SSE frame (`event:` + `data:` lines) and route it to the callbacks. */
function dispatchSseFrame(frame: string, cb: ResearchCallbacks): void {
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
        return;
    }
    let data: unknown;
    try {
        data = JSON.parse(dataLines.join("\n"));
    } catch {
        return; // malformed frame — skip rather than kill the stream
    }
    const d = data as Record<string, unknown>;
    switch (event) {
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

export class MindexApi {
    private readonly base: string;
    private readonly protocol: string;
    private readonly agent: https.Agent;
    private readonly apiKey?: string;

    constructor(opts: ApiOptions) {
        this.base = opts.serverUrl.replace(/\/+$/, "");
        this.protocol = opts.protocol ?? "v0";
        this.apiKey = opts.apiKey && opts.apiKey !== "" ? opts.apiKey : undefined;
        this.agent = new https.Agent({
            rejectUnauthorized: !opts.noVerify,
            ca: opts.caCertPath ? fs.readFileSync(opts.caCertPath) : undefined,
            keepAlive: true,
        });
    }

    dispose(): void {
        this.agent.destroy();
    }

    // ---- data plane ----

    /**
     * `force` bypasses the server's unchanged-skip (content hash *and* derivation
     * versions), so every posted file is re-sliced and re-embedded. Routine reindexing
     * leaves it off — an ordinary pass already picks up slicer/tags-query changes.
     */
    index(
        guid: string,
        files: IndexFiles,
        signal?: AbortSignal,
        force = false
    ): Promise<IndexResponse> {
        return this.request(
            "POST",
            `/${this.protocol}/${guid}/index`,
            { files, force },
            signal
        ) as Promise<IndexResponse>;
    }

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
        filter?: { status?: string; language?: string }
    ): Promise<{ files: FileEntry[] }> {
        const params = new URLSearchParams();
        if (filter?.status) {
            params.set("status", filter.status);
        }
        if (filter?.language) {
            params.set("language", filter.language);
        }
        const qs = params.size > 0 ? `?${params.toString()}` : "";
        return this.request("GET", `/projects/${guid}/files${qs}`) as Promise<{
            files: FileEntry[];
        }>;
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
        const url = new URL(`${this.base}/${this.protocol}/${guid}/research`);
        const payload = Buffer.from(JSON.stringify(req), "utf8");

        return new Promise((resolve, reject) => {
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
                            reject(
                                new ProblemError(
                                    status,
                                    problem.code ?? `http.${status}`,
                                    problem.detail ?? problem.title ?? text.slice(0, 200)
                                )
                            );
                        });
                        return;
                    }

                    res.setEncoding("utf8");
                    let buf = "";
                    res.on("data", (chunk: string) => {
                        buf += chunk;
                        // SSE frames are separated by a blank line.
                        let sep;
                        while ((sep = buf.indexOf("\n\n")) !== -1) {
                            const frame = buf.slice(0, sep);
                            buf = buf.slice(sep + 2);
                            dispatchSseFrame(frame, cb);
                        }
                    });
                    res.on("end", () => resolve());
                    res.on("error", (e) => reject(new UnreachableError(e)));
                }
            );
            request.on("error", (e) => {
                if (e.name === "AbortError") {
                    resolve(); // an abort is the user's cancel, not a failure
                } else {
                    reject(new UnreachableError(e));
                }
            });
            request.write(payload);
            request.end();
        });
    }

    // ---- observability ----

    health(signal?: AbortSignal): Promise<HealthResponse> {
        return this.request("GET", "/health", undefined, signal) as Promise<HealthResponse>;
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

        return new Promise((resolve, reject) => {
            const headers: http.OutgoingHttpHeaders = { Accept: "application/json" };
            if (this.apiKey) {
                headers["X-Api-Key"] = this.apiKey;
            }
            if (payload) {
                headers["Content-Type"] = "application/json";
                headers["Content-Length"] = payload.length;
            }
            const req = https.request(
                url,
                { method, headers, agent: this.agent, signal },
                (res) => {
                    const chunks: Buffer[] = [];
                    res.on("data", (c: Buffer) => chunks.push(c));
                    res.on("end", () => {
                        const status = res.statusCode ?? 0;
                        const text = Buffer.concat(chunks).toString("utf8");
                        if (status === 204) {
                            resolve(null);
                            return;
                        }
                        if (status >= 200 && status < 300) {
                            try {
                                resolve(JSON.parse(text));
                            } catch (e) {
                                reject(new UnreachableError(e as Error));
                            }
                            return;
                        }
                        let problem: ProblemDetails = {};
                        try {
                            problem = JSON.parse(text) as ProblemDetails;
                        } catch {
                            // non-problem+json body (proxy, hard crash) — keep the raw text
                        }
                        reject(
                            new ProblemError(
                                status,
                                problem.code ?? `http.${status}`,
                                problem.detail ?? problem.title ?? text.slice(0, 200)
                            )
                        );
                    });
                    res.on("error", (e) => reject(new UnreachableError(e)));
                }
            );
            req.on("error", (e) => {
                if (e.name === "AbortError") {
                    reject(e);
                } else {
                    reject(new UnreachableError(e));
                }
            });
            if (payload) {
                req.write(payload);
            }
            req.end();
        });
    }
}
