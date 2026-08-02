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

/** The server could not be reached at all (connection refused, TLS failure). */
export class UnreachableError extends Error {
    constructor(public readonly cause_: Error) {
        super(`MINDex server unreachable: ${cause_.message}`);
        this.name = "UnreachableError";
    }
}

/**
 * A request that was answered too slowly, or not at all.
 *
 * Kept apart from `UnreachableError` because the two have different remedies and
 * the wrong one wastes the user's time: "is the server running?" is unhelpful
 * advice about a server that is plainly running and merely stuck, and it is the
 * first thing anyone checks. `phase` separates the two clocks — `"response"` is
 * a request that never completed, `"idle"` is a stream that went silent
 * mid-flight, which on the research path means the run, not the connection.
 */
export class TimeoutError extends Error {
    constructor(
        public readonly ms: number,
        public readonly phase: "response" | "idle"
    ) {
        super(`MINDex request timed out after ${ms}ms (${phase})`);
        this.name = "TimeoutError";
    }
}

/**
 * The server answered and the answer was not what a MINDex server sends.
 *
 * Also not `UnreachableError`, and for a sharper reason: something *did* answer,
 * so the remedy is about what is listening on that URL — a captive portal, a
 * proxy, another service on the port — not about starting anything.
 */
export class MalformedResponseError extends Error {
    constructor(public readonly cause_: unknown) {
        super("MINDex response could not be parsed");
        this.name = "MalformedResponseError";
    }
}

export function isCancellation(e: unknown): boolean {
    return e instanceof Error && e.name === "AbortError";
}

/**
 * One failure, rendered for a person.
 *
 * Every surface renders this and nothing else. `text` is a sentence; the machine
 * `code` never appears in it. That is the whole rule, and it is worth stating
 * because the obvious shortcut — `e.message`, which for a `ProblemError` is
 * `` `${code} (${status}): ${detail}` `` — is how `research.not_found (404)` and
 * `connect ECONNREFUSED 127.0.0.1:11111` came to be shown to users at eight
 * different places. The code survives on `code` for a tooltip and for the log,
 * which is what the localization-key rule actually asks of it.
 */
export interface Humanized {
    /** One sentence, for a human. Never contains a machine code or a stack. */
    text: string;
    /** Whether pressing the same button again could plausibly work. */
    retryable: boolean;
    /** The user's own Stop, or a superseded request: show nothing at all. */
    cancelled: boolean;
    /** The stable machine code, for a `title=` and for the log. Never rendered as the message. */
    code?: string;
}

const CANCELLED: Humanized = { text: "", retryable: false, cancelled: true };

/**
 * Classify anything thrown by the API client.
 *
 * | input | text | retryable |
 * | --- | --- | --- |
 * | abort / `request.cancelled` / 499 | — | `cancelled` |
 * | timeout `response` | did not answer within Ns | yes |
 * | timeout `idle` | went silent for Ns | yes |
 * | unreachable | not answering, check the two settings | yes |
 * | malformed | answer could not be read | yes |
 * | 400 / 409 / 429 | the server's own `detail` | 409, 429 |
 * | 404 | not found, may already be gone | no |
 * | 413 | larger than the server accepts | no |
 * | 500 | internal error, its log has the detail | yes |
 * | 503 | a dependency is not answering | yes |
 * | other 4xx / 5xx / unknown | refused / failed / something went wrong | 5xx |
 *
 * A 400's `detail` is passed through deliberately: it is server-authored English
 * written for a human and naming the offending field, which is strictly better
 * than anything this function could say about a request it cannot see.
 */
export function humanize(e: unknown): Humanized {
    if (isCancellation(e)) {
        return CANCELLED;
    }
    if (e instanceof TimeoutError) {
        const s = Math.round(e.ms / 1000);
        return {
            text:
                e.phase === "response"
                    ? `The server did not answer within ${s}s.`
                    : `The run went silent for ${s}s and the connection was dropped.`,
            retryable: true,
            cancelled: false,
        };
    }
    if (e instanceof UnreachableError) {
        return {
            text:
                "The MINDex server is not answering. Check mindex.serverUrl and " +
                "mindex.noVerify, and that the server is running.",
            retryable: true,
            cancelled: false,
        };
    }
    if (e instanceof MalformedResponseError) {
        return {
            text:
                "The server's answer could not be read. It may not be a MINDex " +
                "server, or something is in the way.",
            retryable: true,
            cancelled: false,
        };
    }
    if (e instanceof ProblemError) {
        return problem(e);
    }
    return { text: "Something went wrong.", retryable: false, cancelled: false };
}

function problem(e: ProblemError): Humanized {
    const code = e.code;
    if (e.code === "request.cancelled" || e.status === 499) {
        return CANCELLED;
    }
    const detail = e.detail.trim();
    switch (e.status) {
        case 400:
            return {
                text: detail || "The server refused the request.",
                retryable: false,
                cancelled: false,
                code,
            };
        case 404:
            return {
                text: "Not found — it may already have been deleted.",
                retryable: false,
                cancelled: false,
                code,
            };
        case 409:
            return {
                text: detail || "Something else is already doing that. Try again shortly.",
                retryable: true,
                cancelled: false,
                code,
            };
        case 413:
            return {
                text: "That is larger than the server accepts.",
                retryable: false,
                cancelled: false,
                code,
            };
        case 429:
            return {
                text:
                    (detail || "The server is busy.") +
                    " Run “MINDex: Active Research Runs” to see what is holding the slot.",
                retryable: true,
                cancelled: false,
                code,
            };
        case 500:
            return {
                text: "The server hit an internal error. Its log has the detail.",
                retryable: true,
                cancelled: false,
                code,
            };
        case 503:
            return {
                text:
                    "A dependency the server needs is not answering — open Server " +
                    "Status to see which.",
                retryable: true,
                cancelled: false,
                code,
            };
        default:
            return e.status >= 500
                ? {
                      text: "The server failed to answer.",
                      retryable: true,
                      cancelled: false,
                      code,
                  }
                : {
                      text: detail || "The server refused the request.",
                      retryable: false,
                      cancelled: false,
                      code,
                  };
    }
}
