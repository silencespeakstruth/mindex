import * as vscode from "vscode";
import { IndexFiles, MindexApi } from "./api";
import { detectLanguage } from "./languages";
import { readUtf8 } from "./scanner";
import { isCancellation, reportError } from "./errors";

export interface ReindexSummary {
    /** Files the server actually (re)indexed (present in the /index response). */
    indexed: number;
    /** Files posted but absent from the response — hash-unchanged, skipped server-side. */
    unchanged: number;
    /** Files not posted at all: binary, over-cap, unreadable, unsupported extension. */
    skipped: string[];
}

/**
 * Reads the given repo-relative paths and POSTs them to /index in sequential batches
 * (the server's pool is small; parallel batches just contend). Shows progress and
 * honours the user's cancel. Returns undefined if it failed and the user declined retry.
 */
export async function reindexPaths(
    api: MindexApi,
    guid: string,
    root: string,
    relPaths: string[],
    batchSize: number
): Promise<ReindexSummary | undefined> {
    const run = () =>
        vscode.window.withProgress(
            {
                location: vscode.ProgressLocation.Notification,
                title: "mindex: reindexing",
                cancellable: true,
            },
            (progress, token) =>
                doReindex(api, guid, root, relPaths, batchSize, progress, token)
        );
    try {
        return await run();
    } catch (e) {
        if (isCancellation(e)) {
            return undefined;
        }
        let retried: ReindexSummary | undefined;
        await reportError(`Reindex of ${relPaths.length} file(s) failed`, e, async () => {
            retried = await reindexPaths(api, guid, root, relPaths, batchSize);
        });
        return retried;
    }
}

async function doReindex(
    api: MindexApi,
    guid: string,
    root: string,
    relPaths: string[],
    batchSize: number,
    progress: vscode.Progress<{ message?: string; increment?: number }>,
    token: vscode.CancellationToken
): Promise<ReindexSummary> {
    const abort = new AbortController();
    const sub = token.onCancellationRequested(() => abort.abort());
    const summary: ReindexSummary = { indexed: 0, unchanged: 0, skipped: [] };
    try {
        for (let i = 0; i < relPaths.length; i += batchSize) {
            const batchPaths = relPaths.slice(i, i + batchSize);
            progress.report({
                message: `${Math.min(i + batchSize, relPaths.length)}/${relPaths.length} files`,
                increment: (batchPaths.length / relPaths.length) * 100,
            });

            const files: IndexFiles = {};
            let posted = 0;
            for (const rel of batchPaths) {
                const language = detectLanguage(rel);
                if (language === undefined) {
                    summary.skipped.push(rel);
                    continue;
                }
                const code = await readUtf8(`${root}/${rel}`);
                if (code === undefined) {
                    summary.skipped.push(rel);
                    continue;
                }
                (files[language] ??= {})[rel] = { code };
                posted += 1;
            }
            if (posted === 0) {
                continue;
            }
            const resp = await api.index(guid, files, abort.signal);
            let indexed = 0;
            for (const byPath of Object.values(resp.files)) {
                indexed += Object.keys(byPath).length;
            }
            summary.indexed += indexed;
            summary.unchanged += posted - indexed;
        }
    } finally {
        sub.dispose();
    }
    return summary;
}

export function showReindexSummary(s: ReindexSummary): void {
    const parts = [`${s.indexed} reindexed`, `${s.unchanged} unchanged (hash-skipped)`];
    if (s.skipped.length > 0) {
        parts.push(`${s.skipped.length} skipped (binary/unsupported)`);
    }
    void vscode.window.showInformationMessage(`mindex: ${parts.join(", ")}`);
}
