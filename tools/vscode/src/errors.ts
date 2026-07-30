import * as vscode from "vscode";
import { isCancellation, ProblemError, UnreachableError } from "./problem";
import { BRAND } from "./brand";

export type { ProblemDetails } from "./problem";
export { isCancellation, ProblemError, UnreachableError } from "./problem";

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
        message =
            `${what}: ${e.message}. Is the ${BRAND} server running? ` +
            "Check mindex.serverUrl / mindex.noVerify.";
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
