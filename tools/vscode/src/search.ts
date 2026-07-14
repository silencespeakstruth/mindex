import * as vscode from "vscode";
import * as path from "node:path";
import { MindexApi, SearchResult } from "./api";
import { ProblemError, isCancellation, reportError } from "./errors";

/**
 * Prompt for a query, POST /search, jump to the best match, and show every
 * result in the native peek widget (editor.action.peekLocations): full file
 * code on the left, the result list on the right; click / F4 / Shift+F4
 * navigate, Esc closes the peek and leaves the cursor on the current result.
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
                { location: vscode.ProgressLocation.Notification, title: "mindex: searching…", cancellable: true },
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

    // Server returns score-descending; keep that order (locations[0] = best).
    const locations = results.map(
        (r) => new vscode.Location(vscode.Uri.file(path.join(workspaceRoot, r.path)), resultRange(r))
    );

    // Jump straight to the best match, then peek every result anchored there.
    await openResult(workspaceRoot, results[0]);
    await vscode.commands.executeCommand(
        "editor.action.peekLocations",
        locations[0].uri,
        locations[0].range.start,
        locations,
        "peek"
    );
    vscode.window.setStatusBarMessage(
        `mindex: ${results.length} result(s) for “${query}”, top score ${results[0].score.toFixed(2)}`,
        5000
    );
}

function resultRange(r: SearchResult): vscode.Range {
    // Server lines/columns are 1-based lines from the slicer; VS Code is 0-based.
    const start = new vscode.Position(Math.max(0, r.start_line - 1), r.start_column);
    const end = new vscode.Position(Math.max(0, r.end_line - 1), r.end_column);
    return new vscode.Range(start, end);
}

async function openResult(workspaceRoot: string, r: SearchResult): Promise<void> {
    const uri = vscode.Uri.file(path.join(workspaceRoot, r.path));
    try {
        await vscode.window.showTextDocument(uri, {
            preview: true,
            selection: resultRange(r),
        });
    } catch {
        void vscode.window.showWarningMessage(
            `mindex: ${r.path} not found in the working tree (index may be stale — run Check Drift).`
        );
    }
}
