import { IndexFeed, IndexFeedSnapshot, LOCAL_SKIP_REASON } from "./indexFeed";
import type {
    IndexDoneEvent,
    IndexEmbeddedEvent,
    IndexIndexedEvent,
    IndexPreparedEvent,
    IndexSkippedEvent,
    IndexStartedEvent,
} from "./indexEvents";

/**
 * A whole reindex run, folded from the server's `/index` SSE events.
 *
 * [`IndexFeed`] answers "what is happening right now" in one line for the status
 * bar and the toast; this answers "what has this run done so far and what did it
 * cost", which is what a panel renders and what a summary is written from. It
 * *owns* a feed and delegates to it (`feedSnapshot`), so the two surfaces cannot
 * disagree and neither `IndexStatusBar` nor its tests had to change.
 *
 * Every method takes `now` rather than reading the clock, for the reason
 * `RateWindow` does: a rate and an average are the things worth testing here, and
 * they are untestable against a clock the test cannot move.
 *
 * vscode-free on purpose, like `indexFeed.ts` — `node --test` reaches it, and the
 * webview page is typed against the same snapshot the host builds.
 */

export interface LangTally {
    language: string;
    filesIndexed: number;
    filesSkipped: number;
    chunks: number;
    symbols: number;
}

export interface BatchRecord {
    index: number;
    filesPosted: number;
    startedAt: number;
    endedAt?: number;
    /** The server's own totals for this request, once it finished. */
    chunks: number;
    filesIndexed?: number;
    serverElapsedMs?: number;
    /** False when this batch was answered as plain JSON rather than as a stream. */
    streamed: boolean;
}

/**
 * Where the batch in flight is, measured against what the server actually streams.
 *
 * A run is **not** a uniform trickle of events, and rendering it as one is what
 * made a reindex look like a single hyperjump. Measured against the live server,
 * 14 files / 173 chunks: `prepared` arrived for every file inside 700 ms (3.6% of
 * the run), then **18.5 seconds of total silence**, then one `embedded` carrying
 * `chunks_done == chunks_total` — the embed pass reports once, on completion,
 * because the server calls `/encode` per `embed_batch` (256) chunks and 173 is one
 * call — and then every `indexed` plus `done` inside 2 ms.
 *
 * So 96% of a small run is one phase that emits nothing at all. The panel can
 * only be honest about that by naming the phase and timing it, rather than by
 * drawing a bar that does not move.
 */
export type RunPhase = "idle" | "reading" | "preparing" | "embedding" | "settling";

/**
 * How far one file got.
 *
 * The three terminal states are kept apart because they call for three different
 * marks and three different readings: `indexed` is the good outcome, `skipped` is
 * a decision the server (or this client) made and explains in its `note`, and
 * `failed`/`cancelled` are what a file gets when the run ended underneath it. A
 * file that was posted and never settled used to keep its in-progress state
 * forever on a run that had already failed — a spinner over a dead request.
 */
export type FileState =
    "prepared" | "embedding" | "indexed" | "skipped" | "failed" | "cancelled";

/**
 * One file of the run, **for the whole run**, updated in place.
 *
 * This is deliberately one list and not two. A file used to appear on an
 * "in flight" list while it worked and again in a "feed" once it settled, so
 * every path in the run was on screen twice, once looking busy and once looking
 * finished, and the same counts were printed beside both. A row is created when
 * the server first mentions the file and never moves again; what changes is its
 * `state`, which is what the mark at the end of the row draws.
 */
export interface RunFile {
    path: string;
    language?: string;
    chunks: number;
    symbols: number;
    state: FileState;
    /** When it last changed state — what makes "nothing has moved for 8s" sayable. */
    at: number;
    /** The skip reason, or why the file never settled. */
    note?: string;
}

export interface RateSample {
    t: number;
    v: number;
}

export type RunOutcome = "done" | "cancelled" | "error";

export interface IndexRunSnapshot {
    startedAt: number;
    endedAt?: number;
    /** When the last server event arrived — what tells a quiet run from a wedged one. */
    lastEventAt: number;
    force: boolean;
    symbolsOnly: boolean;

    files: number;
    filesPosted: number;
    indexed: number;
    skipped: number;
    /** Files the run posted that never settled; see [`FileState`]. */
    failed: number;
    skipReasons: Record<string, number>;

    /**
     * Run-level inventory: chunks and symbols the server has reported, counted once
     * per file and moved by `prepared` — so they climb during the slice phase rather
     * than staying at zero until the run is over.
     */
    chunks: number;
    symbols: number;
    /** Chunks the embedder has actually got through — the throughput denominator. */
    chunksEmbedded: number;
    /** The server's per-request pair for the batch in flight. */
    batchChunks: number;
    batchChunksTotal: number;
    /** 1-based position of the batch in flight, and how many the run will send. */
    batchIndex: number;
    batchCount: number;

    chunksPerSecond?: number;
    peakChunksPerSecond?: number;
    averageChunksPerSecond?: number;
    rateSamples: RateSample[];

    languages: LangTally[];
    batches: BatchRecord[];

    /** False once any batch was answered without a live stream. */
    streamed: boolean;
    outcome?: RunOutcome;
    error?: { code: string; detail: string };
    /** Summed `done.elapsed_ms` — the server's own clock, not the wall clock. */
    serverElapsedMs: number;
    /** Files the follow-up drift check found still `indexing`; see `applyInFlight`. */
    inFlight?: number;

    /** Which phase the batch in flight is in, and since when. See [`RunPhase`]. */
    phase: RunPhase;
    phaseSince: number;
    /** Every file the run has touched, newest first. See [`RunFile`]. */
    rows: RunFile[];

    /** The line the status bar and the toast render, mirrored so the panel can echo it. */
    line: string;
}

export interface IndexRunOptions {
    force?: boolean;
    now?: number;
    /** How many `/index` requests the run will send, known up front from the batch size. */
    batchCount?: number;
    /** How many file rows the page keeps; the oldest **settled** ones go first. */
    maxRows?: number;
    recentLimit?: number;
    maxSamples?: number;
}

/** What `count` means on this run — `symbols_only` flips it for every event. */
export function runUnit(symbolsOnly: boolean): string {
    return symbolsOnly ? "symbol rows" : "chunks";
}

/** A row nothing more will happen to. The rest are still the server's to move. */
export function isSettled(state: FileState): boolean {
    return state !== "prepared" && state !== "embedding";
}

export class IndexRun {
    private readonly feed: IndexFeed;
    private readonly startedAt: number;
    private endedAt?: number;
    private lastEventAt: number;
    private readonly force: boolean;
    private symbolsOnly = false;

    private filesPosted = 0;
    /**
     * Chunks and symbols the server has **reported**, counted once per file and
     * moved by `prepared` rather than by `indexed`.
     *
     * This is the difference between a metric block that moves and one that sits at
     * zero for 96% of the run. `prepared` already carries the file's final chunk and
     * symbol counts — both are written in the same prepare transaction — so waiting
     * for `indexed` to count them meant every counter stayed at zero through the
     * whole silent embed pass and then landed complete, which is exactly the jump
     * that made a working run look like it did nothing. `indexed.count` is still
     * authoritative and reconciles the difference when it lands.
     */
    private chunks = 0;
    private symbols = 0;

    private readonly langs = new Map<string, LangTally>();
    /** Per-path `prepared` detail, dropped the moment the file settles. */
    private readonly pending = new Map<
        string,
        {
            language: string;
            chunks: number;
            symbols: number;
        }
    >();

    private phase: RunPhase = "idle";
    private phaseSince: number;
    /**
     * The run's files, insertion-ordered.
     *
     * Insertion order and nothing else: a row that re-sorted itself as it advanced
     * would read as a *new* row appearing, which is the duplication this list was
     * merged to remove. A file lands once, where the server first mentioned it, and
     * afterwards only its state moves.
     */
    private readonly rows = new Map<string, RunFile>();
    private readonly maxRows: number;
    /** `started.files` for the batch in flight — what says the prepare phase is over. */
    private batchFiles = 0;
    private preparedInBatch = 0;
    private skippedInBatch = 0;

    private readonly batches: BatchRecord[] = [];
    private streamed = true;

    private readonly samples: RateSample[] = [];
    private peak = 0;
    private readonly maxSamples: number;
    private readonly batchCount: number;

    private outcome?: RunOutcome;
    private error?: { code: string; detail: string };
    private serverElapsedMs = 0;
    private inFlight?: number;

    constructor(
        private readonly files: number,
        opts: IndexRunOptions = {}
    ) {
        this.feed = new IndexFeed(files, opts.recentLimit);
        this.startedAt = opts.now ?? 0;
        this.lastEventAt = this.startedAt;
        this.force = opts.force ?? false;
        this.maxSamples = opts.maxSamples ?? 240;
        this.batchCount = opts.batchCount ?? 1;
        this.maxRows = opts.maxRows ?? 400;
        this.phaseSince = this.startedAt;
    }

    /** Phase changes are timed, so a silent stretch can be reported as one. */
    private enter(phase: RunPhase, now: number): void {
        if (this.phase === phase) {
            return;
        }
        this.phase = phase;
        this.phaseSince = now;
        if (phase === "embedding") {
            // Every prepared file of this batch is in the same `/encode` call.
            for (const f of this.rows.values()) {
                if (f.state === "prepared") {
                    f.state = "embedding";
                    f.at = now;
                }
            }
        }
    }

    private track(path: string, patch: Partial<RunFile> & { state: FileState }): void {
        const existing = this.rows.get(path);
        if (existing !== undefined) {
            Object.assign(existing, patch);
            return;
        }
        this.rows.set(path, {
            path,
            chunks: 0,
            symbols: 0,
            at: 0,
            ...patch,
        });
        // Bounded over rows that have already settled, oldest first: dropping a file
        // still on the GPU would make the list disagree with the counters beside it,
        // and dropping the newest would hide the part of the run being watched.
        while (this.rows.size > this.maxRows) {
            const stale = [...this.rows.values()].find((f) => isSettled(f.state));
            if (stale === undefined) {
                break;
            }
            this.rows.delete(stale.path);
        }
    }

    // ---- the run's own boundaries ----

    /** A new `/index` request is about to go out. `filesPosted` is what it carries. */
    beginBatch(index: number, filesPosted: number, now: number): void {
        this.feed.beginBatch();
        this.filesPosted += filesPosted;
        this.batchFiles = 0;
        this.preparedInBatch = 0;
        this.skippedInBatch = 0;
        this.enter("reading", now);
        this.batches.push({
            index,
            filesPosted,
            startedAt: now,
            chunks: 0,
            streamed: true,
        });
    }

    /**
     * The request settled. `done` is absent when the server answered plain JSON
     * (or when the batch had nothing to post), which is the one case where this
     * run's numbers do not come from the stream — recorded rather than guessed at,
     * because a summary written from two batch responses says less than it looks
     * like it says.
     */
    batchDone(done: IndexDoneEvent | undefined, now: number, streamed = true): void {
        const batch = this.batches[this.batches.length - 1];
        if (batch === undefined) {
            return;
        }
        batch.endedAt = now;
        batch.streamed = streamed;
        this.enter("idle", now);
        if (!streamed) {
            this.streamed = false;
        }
        if (done !== undefined) {
            batch.chunks = done.chunks;
            batch.filesIndexed = done.files_indexed;
            batch.serverElapsedMs = done.elapsed_ms;
            this.serverElapsedMs += done.elapsed_ms;
        }
    }

    /**
     * The run is over — and every file it posted gets an answer.
     *
     * A row still `prepared` or `embedding` when the run ends is a file whose
     * result never arrived, and leaving it in an in-progress state was a lie the
     * page then drew as work still happening. What it becomes is the run's own
     * ending: a failed run marks them `failed` (the red cross), a cancelled one
     * `cancelled`, and a run that reported `done` without settling them is a defect
     * worth showing rather than hiding — the server said it was finished and this
     * file has no result.
     */
    finish(now: number, outcome: RunOutcome, error?: { code: string; detail: string }): void {
        this.enter("idle", now);
        this.endedAt = now;
        this.outcome = outcome;
        this.error = error;
        const state: FileState = outcome === "cancelled" ? "cancelled" : "failed";
        const note =
            outcome === "cancelled"
                ? "cancelled"
                : outcome === "error"
                  ? (error?.code ?? "failed")
                  : "no result reported";
        for (const f of this.rows.values()) {
            if (!isSettled(f.state)) {
                f.state = state;
                f.note = note;
                f.at = now;
            }
        }
    }

    /**
     * The follow-up drift check's `indexing` count.
     *
     * A file the server holds a claim on comes back absent from the response, the
     * request still answers 200, and only that later check can tell it from a
     * hash-skipped file. The panel is told the same number the toast is, from the
     * same source, so the two surfaces cannot disagree about what happened.
     */
    applyInFlight(n: number): void {
        this.inFlight = n;
    }

    // ---- server events ----

    started(e: IndexStartedEvent, now: number): void {
        this.symbolsOnly = e.symbols_only;
        this.batchFiles = e.files;
        this.enter("preparing", now);
        this.lastEventAt = now;
    }

    prepared(e: IndexPreparedEvent, now: number): void {
        this.feed.prepared(e.path);
        this.pending.set(e.path, {
            language: e.language,
            chunks: e.chunks,
            symbols: e.symbols,
        });
        // Counted here, not at `indexed` — see the `chunks` field. The file is
        // sliced and its rows are written by the time this event is sent.
        const tally = this.tally(e.language);
        this.chunks += e.chunks;
        this.symbols += e.symbols;
        tally.chunks += e.chunks;
        tally.symbols += e.symbols;
        this.preparedInBatch += 1;
        this.enter("preparing", now);
        this.track(e.path, {
            state: "prepared",
            language: e.language,
            chunks: e.chunks,
            symbols: e.symbols,
            at: now,
        });
        // The prepare phase is over the moment the server has accounted for every
        // file it announced — which is the *only* signal that the silent embed pass
        // has begun, since that pass emits nothing until it completes.
        if (
            this.batchFiles > 0 &&
            this.preparedInBatch > 0 &&
            this.preparedInBatch + this.skippedInBatch >= this.batchFiles
        ) {
            this.enter("embedding", now);
        }
        this.lastEventAt = now;
    }

    indexed(e: IndexIndexedEvent, now: number): void {
        this.feed.indexed(e.path, e.count);
        const stashed = this.pending.get(e.path);
        this.pending.delete(e.path);
        const tally = this.tally(e.language);
        tally.filesIndexed += 1;
        // `indexed.count` is authoritative, but `prepared` has usually already
        // counted this file — so what lands here is the *difference*, not the whole
        // number. Adding it outright would double every chunk on the page. With no
        // stash (an older server, or the JSON fallback) the delta is the full count,
        // which is the same arithmetic.
        const delta = e.count - (stashed?.chunks ?? 0);
        this.chunks += delta;
        tally.chunks += delta;
        // Symbols come from `prepared`, never from `count`: under `symbols_only`
        // that field *is* the symbol row count, and taking both from it would
        // report every file twice. They are already counted, so nothing lands here.
        this.enter("settling", now);
        this.track(e.path, {
            state: "indexed",
            language: e.language,
            chunks: e.count,
            symbols: stashed?.symbols ?? this.rows.get(e.path)?.symbols ?? 0,
            at: now,
        });
        this.lastEventAt = now;
    }

    skipped(e: IndexSkippedEvent, now: number): void {
        this.feed.skipped(e.path, e.reason);
        const stashed = this.pending.get(e.path);
        this.pending.delete(e.path);
        const tally = this.tally(e.language);
        tally.filesSkipped += 1;
        // A file counted at `prepared` and then skipped contributes nothing, so its
        // provisional chunks and symbols are taken back rather than left standing.
        if (stashed !== undefined) {
            this.chunks -= stashed.chunks;
            this.symbols -= stashed.symbols;
            tally.chunks -= stashed.chunks;
            tally.symbols -= stashed.symbols;
        }
        this.skippedInBatch += 1;
        this.track(e.path, {
            state: "skipped",
            language: e.language,
            chunks: 0,
            symbols: 0,
            at: now,
            note: e.reason.replace(/_/g, " "),
        });
        this.lastEventAt = now;
    }

    embedded(e: IndexEmbeddedEvent, now: number): void {
        // Defensive: the phase is normally entered from the prepare count, which
        // needs `started`. An older server that omits it would otherwise leave the
        // whole embed pass labelled "preparing".
        this.enter("embedding", now);
        this.feed.embedded(e.chunks_done, e.chunks_total, now);
        this.lastEventAt = now;
        const rate = this.feed.perSecond();
        if (rate !== undefined) {
            this.peak = Math.max(this.peak, rate);
            this.sample(now, rate);
        }
    }

    /** A file this client refused to post — counted where the summary counts it. */
    droppedLocally(path: string, language: string | undefined, now: number): void {
        this.feed.droppedLocally(path);
        if (language !== undefined) {
            this.tally(language).filesSkipped += 1;
        }
        this.track(path, {
            state: "skipped",
            language,
            at: now,
            note: LOCAL_SKIP_REASON,
        });
    }

    /** The JSON-fallback catch-up; see [`IndexFeed.settledAtLeast`]. */
    settledAtLeast(indexed: number, skipped: number): void {
        this.feed.settledAtLeast(indexed, skipped);
    }

    // ---- rendering ----

    feedSnapshot(): IndexFeedSnapshot {
        return this.feed.snapshot();
    }

    snapshot(): IndexRunSnapshot {
        const feed = this.feed.snapshot();
        const until = this.endedAt ?? this.lastEventAt;
        const elapsed = Math.max(0, until - this.startedAt);
        // The average is over what the **embedder** got through, not over what the
        // server has reported: chunks are counted at `prepared`, which lands long
        // before the GPU has seen them, so dividing the reported total by elapsed
        // time would read as a few hundred chunks a second during the slice phase
        // and then collapse. `chunks` is the inventory; this is the throughput.
        const embedded = feed.runChunks;
        const rows = [...this.rows.values()];
        return {
            startedAt: this.startedAt,
            endedAt: this.endedAt,
            lastEventAt: this.lastEventAt,
            force: this.force,
            symbolsOnly: this.symbolsOnly,

            files: this.files,
            filesPosted: this.filesPosted,
            indexed: feed.indexed,
            skipped: feed.skipped,
            failed: rows.filter((f) => f.state === "failed" || f.state === "cancelled").length,
            skipReasons: feed.skipReasons,

            chunks: this.chunks,
            chunksEmbedded: embedded,
            symbols: this.symbols,
            batchChunks: feed.chunks,
            batchChunksTotal: feed.chunksTotal,
            batchIndex: this.batches.length,
            batchCount: Math.max(this.batchCount, this.batches.length),

            chunksPerSecond: feed.chunksPerSecond,
            peakChunksPerSecond: this.peak > 0 ? this.peak : undefined,
            // Wall clock, so it includes reading and slicing — the number the user
            // actually waited through. The peak is the embedder's own figure, and
            // the two are labelled apart wherever they are shown.
            averageChunksPerSecond:
                elapsed > 0 && embedded > 0 ? (embedded * 1000) / elapsed : undefined,
            rateSamples: [...this.samples],

            languages: [...this.langs.values()].sort((a, b) => b.chunks - a.chunks),
            batches: this.batches.map((b) => ({ ...b })),

            streamed: this.streamed,
            outcome: this.outcome,
            error: this.error,
            serverElapsedMs: this.serverElapsedMs,
            inFlight: this.inFlight,

            phase: this.phase,
            phaseSince: this.phaseSince,
            // Newest first: the run is read from the top, and the rows themselves
            // never move — reversing the insertion order puts the file the server
            // has just mentioned above the ones it finished with.
            rows: rows.reverse().map((f) => ({ ...f })),

            line: feed.line,
        };
    }

    // ---- internals ----

    private tally(language: string): LangTally {
        let t = this.langs.get(language);
        if (t === undefined) {
            t = { language, filesIndexed: 0, filesSkipped: 0, chunks: 0, symbols: 0 };
            this.langs.set(language, t);
        }
        return t;
    }

    /**
     * Rate samples for the sparkline.
     *
     * At the cap the series is **halved** rather than shifted. A shift turns the
     * chart into a scrolling window that hides the start of the run, which is
     * exactly where the interesting shape is (the cold first batch against the
     * warm ones); halving keeps the whole run at lower resolution, and keeps the
     * first and last samples, which are the two a reader compares.
     */
    private sample(t: number, v: number): void {
        this.samples.push({ t, v });
        if (this.samples.length > this.maxSamples) {
            const kept = this.samples.filter((_, i) => i % 2 === 0);
            const last = this.samples[this.samples.length - 1];
            if (kept[kept.length - 1] !== last) {
                kept.push(last);
            }
            this.samples.length = 0;
            this.samples.push(...kept);
        }
    }
}
