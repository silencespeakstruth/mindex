import * as vscode from "vscode";

/** RFC 7807 problem+json body every mindex non-2xx response carries. */
export interface ProblemDetails {
    type?: string;
    title?: string;
    status?: number;
    detail?: string;
    code?: string;
    field?: string;
    meta?: Record<string, unknown>;
}

/** A non-2xx mindex response, keyed by the stable machine `code`. */
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
        super(`mindex server unreachable: ${cause_.message}`);
        this.name = "UnreachableError";
    }
}

export function isCancellation(e: unknown): boolean {
    return e instanceof Error && e.name === "AbortError";
}

/**
 * Show an operation failure to the user. Infra failures (unreachable, 503) get a Retry
 * button; cancellations are silent. `retry` re-runs the operation when the user asks.
 */
export async function reportError(
    what: string,
    e: unknown,
    retry?: () => Promise<void>
): Promise<void> {
    if (isCancellation(e)) {
        return;
    }
    let message: string;
    let retriable = false;
    if (e instanceof ProblemError) {
        if (e.code === "request.cancelled") {
            return;
        }
        message = `${what}: ${e.code} — ${e.detail}`;
        retriable = e.status === 503 || e.status === 500 || e.status === 409;
    } else if (e instanceof UnreachableError) {
        message = `${what}: ${e.message}. Is the mindex server running? Check mindex.serverUrl / mindex.noVerify.`;
        retriable = true;
    } else {
        message = `${what}: ${e instanceof Error ? e.message : String(e)}`;
    }
    if (retriable && retry) {
        const pick = await vscode.window.showErrorMessage(message, "Retry");
        if (pick === "Retry") {
            await retry();
        }
    } else {
        await vscode.window.showErrorMessage(message);
    }
}
