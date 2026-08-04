import * as vscode from "vscode";
import { BRAND } from "./brand";
import { themeIconRef } from "./icons";
import {
    describeToken,
    expiryNotice,
    ExpirySeverity,
    humanizeRemaining,
    nextTickMs,
    TokenFacts,
} from "./token";

/**
 * The token's own indicator, shown **only when there is something to say**.
 *
 * A credential that dies mid-session is the failure this exists for: every
 * request starts answering 401, the extension reports each one honestly, and
 * nothing anywhere connects them to a token that expired eleven minutes ago. The
 * remedy has to arrive *before* the failure, which means an indicator that is
 * absent while the token is healthy and appears on its own.
 *
 * Why the status bar and not a notification: a notification is dismissed once and
 * then gone, and the state it described persists for hours. This is the opposite
 * shape — it cannot be dismissed and it costs nothing to ignore, so it can stay up
 * for the whole last day of a token's life without ever interrupting.
 *
 * Three severities and, unlike the health indicator beside it, `backgroundColor`
 * rather than `color`. That is deliberate: VS Code offers exactly two background
 * treatments, warning and error, and this indicator needs exactly two loud states
 * plus one quiet one — the shape `backgroundColor` fits and the health indicator's
 * three-hue state does not. It also makes "your token is about to die" visually
 * *louder* than "a dependency is down", which is the correct ordering: one of them
 * stops everything.
 */
export class TokenStatusBar {
    private readonly item: vscode.StatusBarItem;
    private facts: TokenFacts = {};
    private timer: NodeJS.Timeout | undefined;

    constructor(
        item: vscode.StatusBarItem,
        /** Milliseconds before expiry at which to start showing anything. */
        private quietBeforeMs: number,
        /** Injected so the tests can move the clock without moving the machine's. */
        private readonly now: () => number = Date.now
    ) {
        this.item = item;
        this.item.command = "mindex.setToken";
    }

    /** The token changed (or was cleared). Re-reads it and repaints immediately. */
    setToken(token: string | undefined): void {
        this.facts = describeToken(token);
        this.refresh();
    }

    /** The setting changed. */
    setQuietBefore(quietBeforeMs: number): void {
        this.quietBeforeMs = quietBeforeMs;
        this.refresh();
    }

    /**
     * Repaints and re-arms. Public because the caller re-runs it on window focus:
     * a laptop that slept through the last scheduled tick must not show yesterday's
     * reading until the next one comes due.
     */
    refresh(): void {
        const nowMs = this.now();
        const notice = expiryNotice(this.facts, nowMs, this.quietBeforeMs);
        if (notice === undefined) {
            this.item.hide();
        } else {
            this.paint(notice.severity, notice.short);
            this.item.show();
        }
        this.arm(nextTickMs(this.facts, nowMs, this.quietBeforeMs));
    }

    private paint(severity: ExpirySeverity, short: string): void {
        // The word is on screen in every state, not only in the tooltip: colour
        // alone cannot distinguish "expiring" from "expired", and those two differ
        // in whether anything currently works.
        this.item.text =
            severity === "expired"
                ? `${themeIconRef("invalid")} ${BRAND} token expired`
                : `${themeIconRef("outOfDate")} ${BRAND} token ${short}`;
        this.item.backgroundColor = new vscode.ThemeColor(
            severity === "soon"
                ? "statusBarItem.warningBackground"
                : "statusBarItem.errorBackground"
        );
        this.item.tooltip = this.tooltip(severity);
    }

    private tooltip(severity: ExpirySeverity): vscode.MarkdownString {
        const md = new vscode.MarkdownString();
        md.supportThemeIcons = true;
        // Required for the `command:` links below, and deliberately *not* the
        // blanket `true`: parts of this tooltip are interpolated from the token's
        // own claims, which is arbitrary text from whatever was pasted in. A
        // wide-open trusted string would let a crafted `sub` render a link to any
        // command in the window. Both commands named here open a dialog and take
        // no arguments, so the worst an injected copy can do is what the button
        // beside it already does.
        md.isTrusted = { enabledCommands: ["mindex.setToken", "mindex.mintAgentToken"] };
        const { subject, projects, actions, expiresAtMs } = this.facts;

        if (severity === "expired") {
            md.appendMarkdown(
                `**${BRAND}** — this token has expired. Every request will be ` +
                    "refused until it is replaced.\n\n"
            );
        } else {
            const left =
                expiresAtMs === undefined
                    ? ""
                    : ` — ${humanizeRemaining(expiresAtMs - this.now())} left`;
            md.appendMarkdown(`**${BRAND}** — this token is about to expire${left}.\n\n`);
        }
        if (expiresAtMs !== undefined) {
            md.appendMarkdown(`Expires ${new Date(expiresAtMs).toLocaleString()}.\n\n`);
        }
        if (subject !== undefined) {
            md.appendMarkdown(`Issued to \`${subject}\`.\n\n`);
        }
        // What it reached is what a replacement has to reach too, and it is the one
        // thing the person minting the next one has to get right.
        if (projects !== undefined) {
            md.appendMarkdown(`Projects: \`${projects.join("`, `")}\`\n\n`);
        }
        if (actions !== undefined) {
            md.appendMarkdown(`Actions: \`${actions.join("`, `")}\`\n\n`);
        }
        // Two remedies, and they are not the same act. Replacing *this* credential
        // has to happen on the host: a token whose own life is nearly over cannot
        // mint itself a longer-lived successor, because `may_mint` refuses a later
        // `exp` than the minter's. Issuing a token for an agent is a different
        // credential with a different holder, is not bound by this one's remaining
        // life in the same way, and is reachable from here — so it is offered as a
        // link rather than as a sentence about a terminal.
        md.appendMarkdown(
            "_Mint a replacement with_ `mindex mint-token` _on the server's host, " +
                "then_ [paste it here](command:mindex.setToken)_._\n\n"
        );
        md.appendMarkdown("[Issue a token for an agent](command:mindex.mintAgentToken)");
        return md;
    }

    private arm(delayMs: number): void {
        this.clearTimer();
        this.timer = setTimeout(() => this.refresh(), delayMs);
        // Without this a pending timer keeps the extension host's event loop alive
        // on shutdown. `unref` is available because this runs in Node, not a webview.
        this.timer.unref?.();
    }

    private clearTimer(): void {
        if (this.timer !== undefined) {
            clearTimeout(this.timer);
            this.timer = undefined;
        }
    }

    dispose(): void {
        this.clearTimer();
        this.item.dispose();
    }
}
