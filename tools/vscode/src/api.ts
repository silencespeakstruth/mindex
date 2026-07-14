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
    status: "ok" | "degraded";
    version: string;
    indexing_files: number;
    checks: Record<string, string>;
}
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
export interface ConfigResponse {
    version: string;
    model_id: string;
    languages: string[];
}

export interface ApiOptions {
    serverUrl: string;
    noVerify: boolean;
    caCertPath?: string;
    protocol?: string;
}

export class MindexApi {
    private readonly base: string;
    private readonly protocol: string;
    private readonly agent: https.Agent;

    constructor(opts: ApiOptions) {
        this.base = opts.serverUrl.replace(/\/+$/, "");
        this.protocol = opts.protocol ?? "v0";
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

    index(guid: string, files: IndexFiles, signal?: AbortSignal): Promise<IndexResponse> {
        return this.request("POST", `/${this.protocol}/${guid}/index`, { files }, signal) as Promise<IndexResponse>;
    }

    search(guid: string, req: SearchRequest, signal?: AbortSignal): Promise<SearchResponse> {
        return this.request("POST", `/${this.protocol}/${guid}/search`, req, signal) as Promise<SearchResponse>;
    }

    // ---- management ----

    drift(guid: string, manifest: Record<string, string>, signal?: AbortSignal): Promise<DriftResponse> {
        return this.request("POST", `/projects/${guid}/drift`, { files: manifest }, signal) as Promise<DriftResponse>;
    }

    /** Empty selector = requeue every failed file. Returns requeued count (204 → 0). */
    async retry(guid: string, selector?: Selector): Promise<number> {
        const body = (await this.request("POST", `/projects/${guid}/retry`, selector ?? {})) as {
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

    listFiles(guid: string, filter?: { status?: string; language?: string }): Promise<{ files: FileEntry[] }> {
        const params = new URLSearchParams();
        if (filter?.status) {
            params.set("status", filter.status);
        }
        if (filter?.language) {
            params.set("language", filter.language);
        }
        const qs = params.size > 0 ? `?${params}` : "";
        return this.request("GET", `/projects/${guid}/files${qs}`) as Promise<{ files: FileEntry[] }>;
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
        const payload = body === undefined ? undefined : Buffer.from(JSON.stringify(body), "utf8");

        return new Promise((resolve, reject) => {
            const headers: http.OutgoingHttpHeaders = { Accept: "application/json" };
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
