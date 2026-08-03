import * as vscode from "vscode";
import * as path from "node:path";
import { MindexApi, SearchResult } from "./api";
import { ProblemError, isCancellation, reportError } from "./errors";
import { BRAND, say } from "./brand";
import { describeScope, isScoped, Scope } from "./scope";

/**
 * Where an in-flight search registers itself so something else can end it.
 *
 * A `Set<AbortController>` satisfies this structurally; the interface exists so this
 * module does not take a whole collection type as an argument for two method calls.
 * Registration covers the **request only** — once results are in hand the search is no
 * longer something that can be interrupted, and leaving it registered would make a
 * later health collapse claim it aborted a run that had already finished.
 */
export interface RunRegistry {
    add(run: AbortController): void;
    delete(run: AbortController): void;
}

/** What a search run is given. `query` preset = no prompt (the Ask form typed it). */
export interface SearchOptions extends Scope {
    topK: number;
    query?: string;
    /** Registered for the duration of the request; see [`RunRegistry`]. */
    registry?: RunRegistry;
}

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
    opts: SearchOptions
): Promise<void> {
    const { topK, query: presetQuery, registry, ...scope } = opts;
    // Invoked from the Ask view the query is already typed; from the command palette
    // there is nothing to type into, so prompt.
    const query =
        presetQuery ??
        (await vscode.window.showInputBox({
            title: `${BRAND} search`,
            prompt: "Semantic code search query",
            placeHolder: "e.g. where are Qdrant collection names derived?",
            ignoreFocusOut: true,
        }));
    if (query === undefined || query.trim() === "") {
        return;
    }

    let results: SearchResult[];
    try {
        results = (
            await vscode.window.withProgress(
                {
                    location: vscode.ProgressLocation.Notification,
                    title: say("searching…"),
                    cancellable: true,
                },
                async (_p, token) => {
                    const abort = new AbortController();
                    token.onCancellationRequested(() => abort.abort());
                    registry?.add(abort);
                    try {
                        return await api.search(
                            guid,
                            {
                                query,
                                top_k: topK,
                                ...(scope.include === undefined
                                    ? {}
                                    : { include: scope.include }),
                                ...(scope.exclude === undefined
                                    ? {}
                                    : { exclude: scope.exclude }),
                            },
                            abort.signal
                        );
                    } finally {
                        registry?.delete(abort);
                    }
                }
            )
        ).results;
    } catch (e) {
        if (isCancellation(e)) {
            return;
        }
        if (e instanceof ProblemError && e.code === "search.no_match") {
            void vscode.window.showInformationMessage(say("no matches."));
            return;
        }
        await reportError("Search failed", e, () =>
            runSearch(api, guid, workspaceRoot, { ...opts, query })
        );
        return;
    }

    if (results.length === 0) {
        void vscode.window.showInformationMessage(say("no matches."));
        return;
    }

    showResultsPicker(workspaceRoot, query, results, scope);
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
    results: SearchResult[],
    scope: Scope
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
    // The scope rides in the title because a scoped search that returns three hits
    // and an unscoped one that returns three hits look identical, and only one of them
    // means "there are three".
    picker.title = say(
        `${results.length} result(s) for “${query}”` +
            (isScoped(scope) ? ` — ${describeScope(scope)}` : "")
    );
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
                say(
                    `${r.path} not found in the working tree (index may be stale — run Check Drift).`
                )
            );
        }
    }
}
