import * as vscode from "vscode";
import { IndexFiles, IndexStreamCallbacks, MindexApi } from "./api";
import { detectLanguage } from "./languages";
import { readUtf8 } from "./scanner";
import { humanize, isCancellation, logError, reportError } from "./errors";
import { say } from "./brand";
import { IndexStatusBar } from "./indexStatusBar";
import { IndexingPanel, IndexingPanelPlacement } from "./indexingPanel";
import { IndexRun } from "./shared/indexRun";
import { throttle } from "./shared/debounce";

export interface ReindexSummary {
    /** Files the server actually (re)indexed (present in the /index response). */
    indexed: number;
    /** Files posted but absent from the response — hash-unchanged, skipped server-side. */
    unchanged: number;
    /** Files not posted at all: binary, over-cap, unreadable, unsupported extension. */
    skipped: string[];
}

/** Everything a run needs that is not the file list itself. */
export interface ReindexOptions {
    statusBar: IndexStatusBar;
    /** Where the live panel opens; `manual` means the run does not open one. */
    placement: IndexingPanelPlacement;
    extensionUri: vscode.Uri;
    openFile(path: string): void;
    force?: boolean;
    batchSize: number;
}

/**
 * Reads the given repo-relative paths and POSTs them to /index in sequential batches
 * (the server's pool is small; parallel batches just contend). Shows progress and
 * honours the user's cancel. Returns undefined if it failed and the user declined retry.
 *
 * Three surfaces, rendered from one aggregate. The **panel** holds what the server
 * actually streams — the paths, the languages, the chunk and symbol counts, the
 * rate, the batch position — because it is the only one of the three that is not
 * structurally a single line. The **status bar** holds that one line, and the
 * **notification** holds the same line plus the Cancel button, which is the one
 * thing it can hold that the others cannot.
 */
export async function reindexPaths(
    api: MindexApi,
    guid: string,
    root: string,
    relPaths: string[],
    opts: ReindexOptions
): Promise<ReindexSummary | undefined> {
    const run = () =>
        vscode.window.withProgress(
            {
                location: vscode.ProgressLocation.Notification,
                title: opts.force === true ? say("force reindexing") : say("reindexing"),
                cancellable: true,
            },
            (progress, token) => doReindex(api, guid, root, relPaths, opts, progress, token)
        );
    try {
        return await run();
    } catch (e) {
        if (isCancellation(e)) {
            return undefined;
        }
        let retried: ReindexSummary | undefined;
        await reportError(`Reindex of ${relPaths.length} file(s) failed`, e, async () => {
            retried = await reindexPaths(api, guid, root, relPaths, opts);
        });
        return retried;
    }
}

async function doReindex(
    api: MindexApi,
    guid: string,
    root: string,
    relPaths: string[],
    opts: ReindexOptions,
    progress: vscode.Progress<{ message?: string; increment?: number }>,
    token: vscode.CancellationToken
): Promise<ReindexSummary> {
    const abort = new AbortController();
    const sub = token.onCancellationRequested(() => abort.abort());
    const summary: ReindexSummary = { indexed: 0, unchanged: 0, skipped: [] };

    const total = relPaths.length;
    const batchSize = Math.max(1, opts.batchSize);
    // One aggregate across every batch, folded from the server's SSE events. Against
    // an older JSON-only server no callback ever fires and the per-batch catch-up
    // below keeps the counters moving, batch by batch.
    const run = new IndexRun(total, {
        force: opts.force ?? false,
        now: Date.now(),
        batchCount: Math.ceil(total / batchSize),
    });
    // Opened before anything is read: the long wait is the request, and a surface
    // that only appeared afterwards would be absent for exactly the stretch it
    // exists to explain. `beginRun` honours the `manual` placement by declining.
    IndexingPanel.beginRun(
        opts.extensionUri,
        { cancel: () => abort.abort(), openFile: (p) => opts.openFile(p) },
        opts.placement,
        run.snapshot()
    );

    // Leading *and* trailing: events arrive in bursts — a whole batch prepares at
    // once — and a leading-only cap drops the burst's last event, freezing every
    // surface one file short of the truth for the length of the embed pass.
    const render = throttle(200, () => {
        const feed = run.feedSnapshot();
        // No `increment`: the toast's own bar was a file-granular percentage, which
        // is exactly the thing batched indexing makes meaningless — it jumps in two
        // bursts and stands still through the embed pass it exists to explain.
        progress.report({ message: feed.line });
        opts.statusBar.render(feed);
        IndexingPanel.current?.update(run.snapshot());
    });
    /** Render this instant, whether or not the throttle window happens to be open. */
    const renderNow = (): void => {
        render();
        render.flush();
    };
    // A heartbeat, because `render` only ever fires when an event arrives and the
    // embed pass sends none. Measured against the live server: between the last
    // `prepared` and the single `embedded` there were **7.8 seconds without one
    // render** — every surface frozen on numbers from before the wait. The panel has
    // its own clock, but the toast and the status bar do not, and neither can be
    // given one; this is the only place all three can be kept alive.
    const beat = setInterval(renderNow, 1000);
    const callbacks: IndexStreamCallbacks = {
        onStarted: (e) => {
            run.started(e, Date.now());
            render();
        },
        onPrepared: (e) => {
            run.prepared(e, Date.now());
            render();
        },
        onSkipped: (e) => {
            run.skipped(e, Date.now());
            render();
        },
        onEmbedded: (e) => {
            run.embedded(e, Date.now());
            render();
        },
        onIndexed: (e) => {
            run.indexed(e, Date.now());
            render();
        },
        onDone: (e) => {
            run.batchDone(e, Date.now());
            render();
        },
        onJsonFallback: () => {
            // Recorded, not merely tolerated: a batch answered without a stream has
            // no per-file reasons, so the summary must not claim any.
            run.batchDone(undefined, Date.now(), false);
            render();
        },
    };

    let failed = false;
    try {
        for (let i = 0; i < relPaths.length; i += batchSize) {
            const batchPaths = relPaths.slice(i, i + batchSize);

            const files: IndexFiles = {};
            let posted = 0;
            const readAt = Date.now();
            const dropped: Array<[string, string | undefined]> = [];
            for (const rel of batchPaths) {
                const language = detectLanguage(rel);
                if (language === undefined) {
                    summary.skipped.push(rel);
                    dropped.push([rel, undefined]);
                    continue;
                }
                const code = await readUtf8(`${root}/${rel}`);
                if (code === undefined) {
                    summary.skipped.push(rel);
                    dropped.push([rel, language]);
                    continue;
                }
                (files[language] ??= {})[rel] = { code };
                posted += 1;
            }
            // The batch record opens once `posted` is known, so its file count is
            // what actually went out rather than what was selected.
            run.beginBatch(i / batchSize, posted, readAt);
            for (const [rel, language] of dropped) {
                run.droppedLocally(rel, language, readAt);
            }
            if (posted === 0) {
                // Still closed, or the panel shows a batch that never ends.
                run.batchDone(undefined, Date.now());
                renderNow();
                continue;
            }
            // Shown *before* the request goes out: the long wait is the request
            // itself, so a surface that only appeared afterwards would be absent
            // for exactly the stretch it exists to explain.
            renderNow();

            const resp = await api.indexStream(
                guid,
                files,
                callbacks,
                abort.signal,
                opts.force ?? false
            );
            let indexed = 0;
            for (const byPath of Object.values(resp.files)) {
                indexed += Object.keys(byPath).length;
            }
            summary.indexed += indexed;
            summary.unchanged += posted - indexed;
            // Idempotent catch-up: with SSE the events have already counted every
            // posted file and this changes nothing; on the JSON fallback it is the
            // only thing that moves the counters at all.
            run.settledAtLeast(summary.indexed, summary.unchanged + summary.skipped.length);
            renderNow();
        }
    } catch (e) {
        failed = true;
        run.finish(
            Date.now(),
            isCancellation(e) ? "cancelled" : "error",
            isCancellation(e) ? undefined : { code: "index.failed", detail: describe(e) }
        );
        throw e;
    } finally {
        if (!failed) {
            run.finish(Date.now(), token.isCancellationRequested ? "cancelled" : "done");
        }
        clearInterval(beat);
        sub.dispose();
        // The panel is the one surface that outlives the run — its summary is what
        // the user reads afterwards — so it gets a last full render before the
        // throttle is silenced and the status bar goes away.
        IndexingPanel.current?.update(run.snapshot());
        IndexingPanel.endRun();
        render.cancel();
        opts.statusBar.clear();
    }
    return summary;
}

/**
 * One line for the run's own log, in the user's terms.
 *
 * Through the shared humanizer, so the sentence in the indexing panel and the
 * one in a notification about the same failure cannot disagree — and so a raw
 * `ECONNREFUSED` never reaches either. The full error goes to the output channel.
 */
function describe(e: unknown): string {
    logError("Indexing", e);
    return humanize(e).text;
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
