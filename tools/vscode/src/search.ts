import * as vscode from "vscode";
import * as path from "node:path";
import { MindexApi, SearchResult } from "./api";
import { ProblemError, isCancellation, reportError } from "./errors";

/**
 * Prompt for a query, POST /search, and show every result in a QuickPick in
 * server rank order (score descending): each item carries `#rank score path`,
 * the line span, and a one-line code snippet. Moving through the list live
 * previews the location in the editor; Enter opens it, Esc restores the
 * editor state from before the search.
 */
export async function runSearch(
    api: MindexApi,
    guid: string,
    workspaceRoot: string,
    topK: number
): Promise<void> {
    const query = await vscode.window.showInputBox({
        title: "mindex search",
        prompt: "Semantic code search query",
        placeHolder: "e.g. where are Qdrant collection names derived?",
        ignoreFocusOut: true,
    });
    if (query === undefined || query.trim() === "") {
        return;
    }

    let results: SearchResult[];
    try {
        results = (
            await vscode.window.withProgress(
                {
                    location: vscode.ProgressLocation.Notification,
                    title: "mindex: searching…",
                    cancellable: true,
                },
                (_p, token) => {
                    const abort = new AbortController();
                    token.onCancellationRequested(() => abort.abort());
                    return api.search(guid, { query, top_k: topK }, abort.signal);
                }
            )
        ).results;
    } catch (e) {
        if (isCancellation(e)) {
            return;
        }
        if (e instanceof ProblemError && e.code === "search.no_match") {
            void vscode.window.showInformationMessage("mindex: no matches.");
            return;
        }
        await reportError("Search failed", e, () => runSearch(api, guid, workspaceRoot, topK));
        return;
    }

    if (results.length === 0) {
        void vscode.window.showInformationMessage("mindex: no matches.");
        return;
    }

    showResultsPicker(workspaceRoot, query, results);
}

interface ResultItem extends vscode.QuickPickItem {
    result: SearchResult;
}

/**
 * QuickPick over the results in server order (= rank order, score descending).
 * Returns as soon as the picker is shown — everything after that is driven by its
 * events, so there is nothing for the caller to await.
 */
function showResultsPicker(
    workspaceRoot: string,
    query: string,
    results: SearchResult[]
): void {
    // Remember where the user was so Esc puts them back.
    const before = vscode.window.activeTextEditor;
    const beforeUri = before?.document.uri;
    const beforeSelection = before?.selection;

    const items: ResultItem[] = results.map((r, i) => ({
        label: `#${i + 1}  ${r.score.toFixed(2)}  ${r.path}`,
        description: `:${r.start_line}-${r.end_line}`,
        detail: snippet(r.code),
        result: r,
    }));

    const picker = vscode.window.createQuickPick<ResultItem>();
    picker.title = `mindex: ${results.length} result(s) for “${query}”`;
    picker.placeholder = "↑/↓ preview · Enter open · Esc back";
    picker.matchOnDescription = true;
    picker.matchOnDetail = true;
    picker.ignoreFocusOut = true;
    picker.items = items;
    picker.activeItems = [items[0]];

    let accepted = false;
    picker.onDidChangeActive(async (active) => {
        if (active.length > 0) {
            // Preview silently: a stale-index miss here would spam warnings on scroll.
            await openResult(workspaceRoot, active[0].result, { preview: true, quiet: true });
        }
    });
    picker.onDidAccept(async () => {
        const chosen = picker.selectedItems[0] ?? picker.activeItems[0];
        accepted = true;
        picker.hide();
        if (chosen !== undefined) {
            await openResult(workspaceRoot, chosen.result, { preview: false, quiet: false });
        }
    });
    picker.onDidHide(async () => {
        picker.dispose();
        if (!accepted && beforeUri !== undefined) {
            try {
                await vscode.window.showTextDocument(beforeUri, {
                    selection: beforeSelection,
                });
            } catch {
                // The original document may be gone; nothing to restore.
            }
        }
    });
    picker.show();
}

/** First non-empty line of the chunk, trimmed and capped, as the item detail. */
function snippet(code: string): string {
    const line =
        code
            .split("\n")
            .find((l) => l.trim() !== "")
            ?.trim() ?? "";
    return line.length > 100 ? `${line.slice(0, 100)}…` : line;
}

function resultRange(r: SearchResult): vscode.Range {
    // Server lines/columns are 1-based lines from the slicer; VS Code is 0-based.
    const start = new vscode.Position(Math.max(0, r.start_line - 1), r.start_column);
    const end = new vscode.Position(Math.max(0, r.end_line - 1), r.end_column);
    return new vscode.Range(start, end);
}

async function openResult(
    workspaceRoot: string,
    r: SearchResult,
    opts: { preview: boolean; quiet: boolean }
): Promise<void> {
    const uri = vscode.Uri.file(path.join(workspaceRoot, r.path));
    try {
        await vscode.window.showTextDocument(uri, {
            preview: opts.preview,
            preserveFocus: opts.preview,
            selection: resultRange(r),
        });
    } catch {
        if (!opts.quiet) {
            void vscode.window.showWarningMessage(
                `mindex: ${r.path} not found in the working tree (index may be stale — run Check Drift).`
            );
        }
    }
}
