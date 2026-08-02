import * as vscode from "vscode";
import { humanize } from "./problem";
import { BRAND } from "./brand";

export type { ProblemDetails, Humanized } from "./problem";
export {
    humanize,
    isCancellation,
    MalformedResponseError,
    ProblemError,
    TimeoutError,
    UnreachableError,
} from "./problem";

let channel: vscode.OutputChannel | undefined;

/**
 * The raw failure, in full, where a person can go and read it.
 *
 * This is what makes "no raw errors reach the user" affordable rather than
 * destructive. A notification saying "the server hit an internal error" is the
 * right thing to *show*; it is the wrong thing to be the only surviving record,
 * because then a bug report contains no error at all. So the sentence goes to
 * the notification and the stack goes here, one click away.
 *
 * Created lazily: a session that never fails never grows an output channel, and
 * an empty one in the picker reads as something having gone wrong.
 */
export function logError(what: string, e: unknown): void {
    channel ??= vscode.window.createOutputChannel(BRAND);
    const code = humanize(e).code;
    const raw = e instanceof Error ? (e.stack ?? e.message) : String(e);
    channel.appendLine(
        `[${new Date().toISOString()}] ${what}${code !== undefined ? ` (${code})` : ""}\n${raw}`
    );
}

/** Dispose the output channel, if one was ever needed. For `deactivate`. */
export function disposeErrorLog(): void {
    channel?.dispose();
    channel = undefined;
}

/**
 * Show an operation failure to the user: one sentence, never the error's own
 * words. Retryable failures get a Retry button; cancellations are silent.
 *
 * `what` is the operation, in the user's terms ("Delete failed"). The rest comes
 * from `humanize`, so this notification and a panel's inline banner cannot
 * disagree about what a 404 means.
 */
export async function reportError(
    what: string,
    e: unknown,
    retry?: () => Promise<void>
): Promise<void> {
    const h = humanize(e);
    if (h.cancelled) {
        return;
    }
    logError(what, e);
    const message = `${what}: ${h.text}`;
    if (h.retryable && retry) {
        const pick = await vscode.window.showErrorMessage(message, "Retry");
        if (pick === "Retry") {
            await retry();
        }
    } else {
        await vscode.window.showErrorMessage(message);
    }
}
