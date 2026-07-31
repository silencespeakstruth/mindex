import * as vscode from "vscode";
import { IndexFiles, IndexStreamCallbacks, MindexApi } from "./api";
import { detectLanguage } from "./languages";
import { readUtf8 } from "./scanner";
import { isCancellation, reportError } from "./errors";
import { say } from "./brand";
import { RateWindow } from "./shared/rateWindow";

export interface ReindexSummary {
    /** Files the server actually (re)indexed (present in the /index response). */
    indexed: number;
    /** Files posted but absent from the response — hash-unchanged, skipped server-side. */
    unchanged: number;
    /** Files not posted at all: binary, over-cap, unreadable, unsupported extension. */
    skipped: string[];
}

/** Where a run reports itself besides the notification — see [`reindexPaths`]. */
export type ReindexProgress = (done: number, total: number, label: string) => void;

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
    batchSize: number,
    force = false,
    /**
     * Mirrors the notification's progress into the Drift view.
     *
     * Both, not one: the notification is the only one of the two that can carry a
     * Cancel button, and it appears in the corner of the window rather than in the
     * view the user just pressed a button in — which is how a running reindex came to
     * read as nothing happening at all.
     */
    onProgress?: ReindexProgress
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
                    onProgress
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
                force,
                onProgress
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
    onProgress?: ReindexProgress
): Promise<ReindexSummary> {
    const abort = new AbortController();
    const sub = token.onCancellationRequested(() => abort.abort());
    const summary: ReindexSummary = { indexed: 0, unchanged: 0, skipped: [] };

    // Live progress across all batches, fed by the server's SSE events: files
    // settle one by one (`skipped`/`indexed`), and each embed batch advances the
    // chunk counter, from which a sliding window computes an honest live
    // chunks-per-second (a cumulative average flattens a long run). Against an
    // older JSON-only server the callbacks never fire and the per-batch
    // catch-up below keeps the old batch-granular behaviour.
    const total = relPaths.length;
    let settled = 0;
    let lastPct = 0;
    let chunks = 0;
    const rate = new RateWindow(20_000);
    let lastReport = 0;
    const report = (label: string, forceReport = false): void => {
        const now = Date.now();
        // The Drift view redraws its whole tree per update; per-file events can
        // arrive in bursts, so cap the redraw rate.
        if (!forceReport && now - lastReport < 200) {
            return;
        }
        lastReport = now;
        const perSecond = rate.perSecond();
        const rateLabel = perSecond === undefined ? "" : `${Math.round(perSecond)} chunks/s`;
        const detail = [rateLabel, label].filter((s) => s !== "").join(" · ");
        const pct = total > 0 ? (settled / total) * 100 : 100;
        progress.report({
            message: `${settled}/${total} files${detail === "" ? "" : ` · ${detail}`}`,
            increment: pct - lastPct,
        });
        lastPct = pct;
        onProgress?.(settled, total, detail);
    };
    const callbacks: IndexStreamCallbacks = {
        onPrepared: (e) => report(e.path),
        onSkipped: (e) => {
            settled += 1;
            report(e.path);
        },
        onEmbedded: (e) => {
            chunks += e.batch_chunks;
            rate.push(Date.now(), chunks);
            report("embedding");
        },
        onIndexed: (e) => {
            settled += 1;
            report(e.path);
        },
    };

    try {
        for (let i = 0; i < relPaths.length; i += batchSize) {
            const batchPaths = relPaths.slice(i, i + batchSize);
            // Reported *before* the batch, naming what is about to be read and sent:
            // the long wait is the request, so a counter that only moved afterwards
            // would sit still for exactly the stretch it exists to explain.
            report(
                batchPaths.length === 1 ? batchPaths[0] : `${batchPaths.length} files`,
                true
            );

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
                settled = Math.min(i + batchPaths.length, total);
                report("skipped", true);
                continue;
            }
            const resp = await api.indexStream(guid, files, callbacks, abort.signal, force);
            let indexed = 0;
            for (const byPath of Object.values(resp.files)) {
                indexed += Object.keys(byPath).length;
            }
            summary.indexed += indexed;
            summary.unchanged += posted - indexed;
            // Idempotent catch-up: with SSE the events already settled every
            // posted file and this only accounts the client-side skips; on the
            // JSON fallback it advances the whole batch at once.
            settled = Math.min(i + batchPaths.length, total);
            report("posted", true);
        }
    } finally {
        sub.dispose();
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
