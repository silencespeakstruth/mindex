/**
 * The Server Status snapshot, as it travels from the host to the panel's webview.
 *
 * It lives under `src/shared` because both halves compile it: the host builds it in
 * `statusMonitor.ts` and the browser renders it in `webview/status.ts`. Putting the
 * type next to the fetching would have meant importing `statusMonitor.ts` — and with
 * it `vscode` and `node:https` — into a browser bundle.
 *
 * The three payload shapes below are structural restatements of `api.ts`'s
 * `StatusResponse`, `LanguageStats` and `FileEntry`, not copies with a life of their
 * own: `statusMonitor.ts` assigns the real ones straight into these fields, so a
 * server shape change that `api.ts` picks up fails to compile here rather than going
 * quiet. They are restated at all only because `api.ts` is host-only.
 */

/** How the server answered its last health check. */
export type ServerState = "ok" | "degraded" | "unreachable";

/**
 * A section that could not be fetched. Distinct from *absent* (an older server does
 * not publish it) and from an empty value (the server answered, and the answer is
 * nothing) — three states the old status tree collapsed into one dim leaf.
 */
export const UNAVAILABLE = "unavailable";
export type Unavailable = typeof UNAVAILABLE;

export interface RuntimeInfo {
    indexing_claims: number;
    gc_running: boolean;
    pool_available: number;
    pool_size: number;
}

export interface LanguageInventory {
    files: number;
    indexed_files: number;
    chunks_active: number;
    chunks_deleted: number;
}

export interface FailedFile {
    path: string;
    programming_language: string;
    retry_count: number;
    status_updated_at: number;
}

export interface StatusSnapshot {
    /** Unix milliseconds — when this refresh completed. */
    at: number;
    serverUrl: string;
    state: ServerState;
    /** Why the server is unreachable. Only set when `state === "unreachable"`. */
    detail?: string;
    version?: string;
    /** `GET /health`'s per-dependency verdicts, open-ended so a new check renders. */
    checks?: Record<string, string>;
    /**
     * Whether `/research` can work at all. The server's Ollama is *optional*, so it
     * never makes `state` anything but "ok" — it only costs Research.
     */
    researchAvailable: boolean;
    runtime?: RuntimeInfo | Unavailable;
    /**
     * What this project holds, per language. **Absent** = the server publishes no
     * inventory (too old) or there is no project; `"unavailable"` = the fetch failed;
     * `{}` = the project genuinely holds nothing.
     */
    inventory?: Record<string, LanguageInventory> | Unavailable;
    failed?: FailedFile[] | Unavailable;
}

/** How many files are in the dead-letter list, or 0 when that is not known. */
export function failedCount(snapshot?: StatusSnapshot): number {
    return Array.isArray(snapshot?.failed) ? snapshot.failed.length : 0;
}
