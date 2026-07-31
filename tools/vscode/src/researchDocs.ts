import * as vscode from "vscode";
import { MindexApi, ResearchRunDetail, ResearchRunSummary } from "./api";
import { ProblemError } from "./errors";

/**
 * URI scheme for a stored report opened as a document.
 *
 * A report is Markdown that someone wants to read, search, fold and keep open
 * beside the code it talks about — which is what an editor tab is for. Serving it
 * through a `TextDocumentContentProvider` rather than a fourth webview buys the
 * whole markdown preview (outline, find, "open source", the user's own theme and
 * font) for a few dozen lines, and costs nothing: the content is text and the
 * document is read-only by construction, since the provider is the only writer.
 */
export const RESEARCH_SCHEME = "mindex-research";

/**
 * Build the URI a report is served at.
 *
 * The path ends in `.md` so VS Code picks the Markdown language mode, and carries
 * a slug of the title so a row of open tabs stays readable — the tab label is the
 * basename, and `#12` alone tells the reader nothing. Identity is the `runId`
 * segment, never the slug: two reports may share a title.
 *
 * The project guid rides in the authority rather than the path so a workspace with
 * two projects cannot collide, and so `provideTextDocumentContent` can serve the
 * document from the URI alone — a provider is called again after a window reload,
 * when whatever cache produced the URI is long gone.
 */
export function researchUri(
    guid: string,
    runId: string,
    seq: number,
    title: string
): vscode.Uri {
    const slug =
        title
            .toLowerCase()
            .replace(/[^a-z0-9]+/g, "-")
            .replace(/^-+|-+$/g, "")
            .slice(0, 48) || "report";
    return vscode.Uri.parse(
        `${RESEARCH_SCHEME}://${guid}/${encodeURIComponent(runId)}/${seq}-${slug}.md`
    );
}

/** The `runId` the URI names. */
function runIdOf(uri: vscode.Uri): string {
    // path is `/<runId>/<seq>-<slug>.md`.
    return decodeURIComponent(uri.path.split("/")[1] ?? "");
}

function stamp(unixSeconds: number): string {
    return new Date(unixSeconds * 1000).toLocaleString();
}

/**
 * The provenance block prepended to the report body.
 *
 * The stored Markdown says what the run concluded and nothing about what it is
 * entitled to claim — which run it was, what it could see, whether its evidence
 * still matches the index, and which earlier reports it leaned on. All of that is
 * on the summary already and none of it is in the report, so a report read months
 * later out of a tab would be a confident document with no provenance at all.
 *
 * Written as Markdown rather than YAML front matter: front matter renders as a
 * table in some previews and as literal text in others, and this must be readable
 * in the preview *and* in the source.
 */
function header(detail: ResearchRunDetail): string {
    const lines: string[] = [];
    lines.push(`> **Research #${detail.seq}** · ${stamp(detail.created_at)}`);
    lines.push(`> `);
    lines.push(`> ${detail.question.trim().replace(/\s+/g, " ")}`);
    lines.push(`> `);

    const facts = [
        `\`${detail.model}\``,
        detail.effort,
        `${detail.steps} steps`,
        `${Math.round(detail.elapsed_ms / 1000)}s`,
        detail.done_reason === "finalized" ? null : `stopped: ${detail.done_reason}`,
        detail.scope ? `scope: ${detail.scope}` : null,
        detail.pinned ? "pinned" : null,
    ].filter((f): f is string => f !== null);
    lines.push(`> ${facts.join(" · ")}`);

    const cites = detail.citations_total;
    if (cites > 0) {
        const bad = detail.citations_unverified;
        lines.push(
            `> ` +
                `${detail.citations_verified}/${cites} citations verified` +
                (bad > 0 ? ` — **${bad} unverified**` : "")
        );
    } else {
        lines.push(`> No checkable citations.`);
    }

    if (!detail.valid) {
        const why =
            detail.invalid_reason === "stale"
                ? `${detail.files_moved} of ${detail.files_total} files it read have changed since`
                : detail.invalid_reason === "context_deleted"
                  ? "a report it was built on has been deleted"
                  : "a report it was built on is itself out of date";
        lines.push(`> `);
        lines.push(`> ⚠️ **Out of date** — ${why}.`);
    } else if (detail.stale) {
        lines.push(`> `);
        lines.push(
            `> ⚠️ ${detail.files_moved} of ${detail.files_total} files it read have changed.`
        );
    }

    if (detail.context.length > 0) {
        lines.push(`> `);
        lines.push(`> Built on: ` + detail.context.map(describeDependency).join(", "));
    }
    lines.push("");
    return lines.join("\n");
}

function describeDependency(d: ResearchRunSummary["context"][number]): string {
    if (d.state === "deleted") {
        return `~~#? (deleted)~~`;
    }
    const label = `#${d.seq} ${d.title ?? ""}`.trim();
    return d.state === "invalid" ? `${label} (out of date)` : label;
}

/**
 * Serves stored reports as read-only Markdown documents.
 *
 * Registered once for the window. `onDidChange` is fired by [`refresh`] so a
 * report already open picks up a pin, or a validity change, without the user
 * closing the tab.
 */
export class ResearchDocumentProvider implements vscode.TextDocumentContentProvider {
    private readonly changed = new vscode.EventEmitter<vscode.Uri>();
    readonly onDidChange = this.changed.event;

    constructor(private readonly api: () => MindexApi | undefined) {}

    async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
        const api = this.api();
        if (!api) {
            return "# Report unavailable\n\nThe server is not configured.\n";
        }
        const guid = uri.authority;
        const runId = runIdOf(uri);
        try {
            const detail = await api.getResearchRun(guid, runId);
            return header(detail) + detail.report;
        } catch (e) {
            // A deleted run is the common case here — the tab outlives the row,
            // and saying so is better than an empty document or a modal.
            if (e instanceof ProblemError && e.code === "research.run_not_found") {
                return "# Report deleted\n\nThis report is no longer stored.\n";
            }
            return `# Report unavailable\n\n${e instanceof Error ? e.message : String(e)}\n`;
        }
    }

    /** Re-fetch an open report (after a pin, a delete, or an index move). */
    refresh(uri: vscode.Uri): void {
        this.changed.fire(uri);
    }

    dispose(): void {
        this.changed.dispose();
    }
}

/**
 * Open a stored report in its own tab, as a rendered Markdown preview.
 *
 * `markdown.showPreview` rather than `showTextDocument`: the report is prose to be
 * read, not source to be edited, and the preview is what makes its links, headings
 * and code fences behave. The source stays one click away in the preview's own
 * toolbar for anyone who wants to copy from it.
 */
export async function openResearchReport(
    guid: string,
    run: { id: string; seq: number; title: string }
): Promise<void> {
    const uri = researchUri(guid, run.id, run.seq, run.title);
    await vscode.commands.executeCommand("markdown.showPreview", uri);
}
