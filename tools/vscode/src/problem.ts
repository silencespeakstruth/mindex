/**
 * The error *shapes* the API client raises, free of `vscode`.
 *
 * Split out of `errors.ts` so the code that classifies a failure can be exercised by
 * `node --test`, which has no extension host to import `vscode` from. `errors.ts`
 * keeps `reportError` — the half that actually shows a notification — and re-exports
 * these, so no call site had to change.
 */

/** RFC 7807 problem+json body every MINDex non-2xx response carries. */
export interface ProblemDetails {
    type?: string;
    title?: string;
    status?: number;
    detail?: string;
    code?: string;
    field?: string;
    meta?: Record<string, unknown>;
}

/** A non-2xx MINDex response, keyed by the stable machine `code`. */
export class ProblemError extends Error {
    constructor(
        public readonly status: number,
        public readonly code: string,
        public readonly detail: string
    ) {
        super(`${code} (${status}): ${detail}`);
        this.name = "ProblemError";
    }
}

/** The server could not be reached at all (connection refused, TLS failure, timeout). */
export class UnreachableError extends Error {
    constructor(public readonly cause_: Error) {
        super(`MINDex server unreachable: ${cause_.message}`);
        this.name = "UnreachableError";
    }
}

export function isCancellation(e: unknown): boolean {
    return e instanceof Error && e.name === "AbortError";
}
