import * as vscode from "vscode";
import { IndexFiles, IndexStreamCallbacks, MindexApi } from "./api";
import { detectLanguage } from "./languages";
import { readUtf8 } from "./scanner";
import { isCancellation, reportError } from "./errors";
import { say } from "./brand";
import { IndexStatusBar } from "./indexStatusBar";
import { IndexFeed } from "./shared/indexFeed";

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
 *
 * Two surfaces, rendered from the one snapshot: the status bar holds the live feed
 * — the paths going through, the rate, the counters — and the notification holds
 * the Cancel button, which is the only thing it can hold that the status bar
 * cannot. Its message is the feed's one line, because a notification message is
 * structurally single-line (see [`IndexStatusBar`]).
 */
export async function reindexPaths(
    api: MindexApi,
    guid: string,
    root: string,
    relPaths: string[],
    batchSize: number,
    statusBar: IndexStatusBar,
    force = false
): Promise<ReindexSummary | undefined> {
    const run = () =>
        vscode.window.withProgress(
            {
                location: vscode.ProgressLocation.Notification,
                title: force ? say("force reindexing") : say("reindexing"),
                cancellable: true,
            },
            (progress, token) =>
                doReindex(
                    api,
                    guid,
                    root,
                    relPaths,
                    batchSize,
                    progress,
                    token,
                    force,
                    statusBar
                )
        );
    try {
        return await run();
    } catch (e) {
        if (isCancellation(e)) {
            return undefined;
        }
        let retried: ReindexSummary | undefined;
        await reportError(`Reindex of ${relPaths.length} file(s) failed`, e, async () => {
            retried = await reindexPaths(
                api,
                guid,
                root,
                relPaths,
                batchSize,
                statusBar,
                force
            );
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
    token: vscode.CancellationToken,
    force: boolean,
    statusBar: IndexStatusBar
): Promise<ReindexSummary> {
    const abort = new AbortController();
    const sub = token.onCancellationRequested(() => abort.abort());
    const summary: ReindexSummary = { indexed: 0, unchanged: 0, skipped: [] };

    // One feed across every batch, folded from the server's SSE events. Against an
    // older JSON-only server no callback ever fires and the per-batch catch-up
    // below keeps the counters moving, batch by batch.
    const total = relPaths.length;
    const feed = new IndexFeed(total);
    let lastRender = 0;
    const render = (forceRender = false): void => {
        const now = Date.now();
        // Events arrive in bursts — a whole batch prepares at once — and each
        // render rebuilds a Markdown tooltip. Cap the rate.
        if (!forceRender && now - lastRender < 200) {
            return;
        }
        lastRender = now;
        const snapshot = feed.snapshot();
        // No `increment`: the toast's own bar was a file-granular percentage, which
        // is exactly the thing batched indexing makes meaningless — it jumps in two
        // bursts and stands still through the embed pass it exists to explain.
        progress.report({ message: snapshot.line });
        statusBar.render(snapshot);
    };
    const callbacks: IndexStreamCallbacks = {
        onPrepared: (e) => {
            feed.prepared(e.path);
            render();
        },
        onSkipped: (e) => {
            feed.skipped(e.path, e.reason);
            render();
        },
        onEmbedded: (e) => {
            feed.embedded(e.chunks_done, e.chunks_total, Date.now());
            render();
        },
        onIndexed: (e) => {
            feed.indexed(e.path, e.count);
            render();
        },
    };

    try {
        for (let i = 0; i < relPaths.length; i += batchSize) {
            const batchPaths = relPaths.slice(i, i + batchSize);
            // Shown *before* the batch is read and sent: the long wait is the
            // request, so a surface that only appeared afterwards would be absent
            // for exactly the stretch it exists to explain.
            render(true);

            const files: IndexFiles = {};
            let posted = 0;
            for (const rel of batchPaths) {
                const language = detectLanguage(rel);
                if (language === undefined) {
                    summary.skipped.push(rel);
                    feed.droppedLocally(rel);
                    continue;
                }
                const code = await readUtf8(`${root}/${rel}`);
                if (code === undefined) {
                    summary.skipped.push(rel);
                    feed.droppedLocally(rel);
                    continue;
                }
                (files[language] ??= {})[rel] = { code };
                posted += 1;
            }
            if (posted === 0) {
                render(true);
                continue;
            }
            const resp = await api.indexStream(guid, files, callbacks, abort.signal, force);
            let indexed = 0;
            for (const byPath of Object.values(resp.files)) {
                indexed += Object.keys(byPath).length;
            }
            summary.indexed += indexed;
            summary.unchanged += posted - indexed;
            // Idempotent catch-up: with SSE the events have already counted every
            // posted file and this changes nothing; on the JSON fallback it is the
            // only thing that moves the counters at all.
            feed.settledAtLeast(summary.indexed, summary.unchanged + summary.skipped.length);
            render(true);
        }
    } finally {
        sub.dispose();
        statusBar.clear();
    }
    return summary;
}

/**
 * @param inFlight files the follow-up drift check found still `indexing`.
 *
 * That count is the correction to an otherwise dishonest sentence. A file the server
 * has a claim on comes back **absent** from the `/index` response — the conflict is
 * swallowed and the request still answers 200 — which is byte-for-byte how a
 * hash-skipped file comes back. Reporting both as "unchanged" told the user their
 * reindex was unnecessary at the exact moment it had been refused, and only the drift
 * check that follows can tell the two apart.
 */
export function showReindexSummary(s: ReindexSummary, inFlight = 0): void {
    const unchanged = Math.max(0, s.unchanged - inFlight);
    const parts = [`${s.indexed} reindexed`, `${unchanged} unchanged (hash-skipped)`];
    if (inFlight > 0) {
        parts.push(`${inFlight} still indexing on the server`);
    }
    if (s.skipped.length > 0) {
        parts.push(`${s.skipped.length} skipped (binary/unsupported)`);
    }
    void vscode.window.showInformationMessage(say(`${parts.join(", ")}`));
}
