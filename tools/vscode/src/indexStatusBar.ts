import * as vscode from "vscode";
import { BRAND } from "./brand";
import { ICON, IconName, themeIconRef } from "./icons";
import { FeedEntry, FeedEntryKind, IndexFeedSnapshot } from "./shared/indexFeed";

/**
 * The live indexing feed, in the corner of the window.
 *
 * It exists because the surface it replaces could not hold a stream. A
 * `withProgress` notification renders its message as **one line** — VS Code's
 * `.notification-list-item-message` is `white-space: normal`, so a `\n` collapses
 * to a space and there is no multi-line message API — and a `TreeView` row is one
 * label plus one description. A status bar item is also one line, but its
 * `MarkdownString` tooltip is not, and that is where the paths go.
 *
 * The toast is still shown by [`reindexPaths`], carrying this same line: it is the
 * only one of the two that can hold a Cancel button.
 */
export class IndexStatusBar {
    constructor(private readonly item: vscode.StatusBarItem) {
        // The click lands where the Cancel button is. Cancelling from the status
        // bar itself was considered and dropped: a one-click abort of a long run,
        // on an item the pointer crosses on its way elsewhere, is the wrong
        // affordance for a destructive-feeling action.
        item.command = "notifications.toggleList";
    }

    render(s: IndexFeedSnapshot): void {
        // `~spin` animates the codicon, which is what says "still working" through
        // the long silent stretch while a batch is on the GPU — the stretch during
        // which every number below it legitimately stands still.
        this.item.text = `$(${ICON.sync}~spin) ${s.line}`;
        this.item.tooltip = tooltipFor(s);
        this.item.show();
    }

    clear(): void {
        this.item.hide();
    }
}

function tooltipFor(s: IndexFeedSnapshot): vscode.MarkdownString {
    const md = new vscode.MarkdownString();
    md.supportThemeIcons = true;
    md.appendMarkdown(`**${BRAND}** — indexing ${s.files} file(s)\n\n`);

    for (const e of s.recent) {
        md.appendMarkdown(
            `${glyph(e)} \`${e.path}\`${e.note === undefined ? "" : ` — ${e.note}`}\n\n`
        );
    }

    const rate =
        s.chunksPerSecond === undefined
            ? "reading and slicing"
            : `${Math.round(s.chunksPerSecond)} chunks/s`;
    const chunks =
        s.chunksTotal > 0 ? ` · ${s.chunks} of ${s.chunksTotal} chunks in this batch` : "";
    md.appendMarkdown(`${rate}${chunks}\n\n`);

    // The breakdown, not just the total: `in_flight` is the one skip that means
    // the file was *refused* rather than found unchanged, and it is invisible in
    // the response body — the server answers 200 with the file simply absent.
    const reasons = Object.entries(s.skipReasons)
        .map(([reason, n]) => `${n} ${reason.replace(/_/g, " ")}`)
        .join(", ");
    md.appendMarkdown(
        `${s.indexed} indexed · ${s.skipped} skipped${reasons === "" ? "" : ` (${reasons})`}\n\n`
    );
    md.appendMarkdown("_Cancel from the progress notification._");
    return md;
}

const MARK: Record<FeedEntryKind, IconName> = {
    preparing: "sync",
    indexed: "feedIndexed",
    skipped: "feedSkipped",
};

function glyph(e: FeedEntry): string {
    return themeIconRef(MARK[e.kind]);
}
