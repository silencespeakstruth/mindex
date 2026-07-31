import { RateWindow } from "./rateWindow";

/**
 * What a reindex looks like while it runs, folded from the server's `/index` SSE
 * events into one snapshot two surfaces render.
 *
 * It replaced a file-granular percentage, which indexing's own shape makes
 * useless: the server prepares a whole batch, embeds it in a single GPU pass and
 * settles it, so a per-file bar moves in two bursts with the long stretch it
 * exists to explain sitting frozen between them. What moves continuously is the
 * chunk counter, and what a waiting reader actually wants is the stream — which
 * paths are going through, how fast chunks are landing, how much has settled.
 *
 * vscode-free on purpose, like `rateWindow.ts` and `debounce.ts`, so
 * `node --test` can reach it.
 */

/** How far a path has got. `preparing` is sliced-and-inserted, not yet embedded. */
export type FeedEntryKind = "preparing" | "indexed" | "skipped";

export interface FeedEntry {
    kind: FeedEntryKind;
    path: string;
    /** Chunk count for `indexed`, the skip reason for `skipped`. */
    note?: string;
}

export interface IndexFeedSnapshot {
    /** The one line both the status bar and the notification render. */
    line: string;
    /** Newest first, capped — the stream. */
    recent: FeedEntry[];
    chunksPerSecond?: number;
    indexed: number;
    skipped: number;
    /** Skips by reason: the server's `unchanged`/`in_flight`/`cancelled`, plus
     *  `unsupported` for the files this client never posted. */
    skipReasons: Record<string, number>;
    chunks: number;
    chunksTotal: number;
    /** How many files the run set out to send. */
    files: number;
}

/** Files this client dropped before posting (binary, over-cap, unreadable, unknown ext). */
export const LOCAL_SKIP_REASON = "unsupported";

export class IndexFeed {
    private readonly rate: RateWindow;
    private readonly ring: FeedEntry[] = [];
    private indexedCount = 0;
    private skippedCount = 0;
    private readonly reasons = new Map<string, number>();
    private chunks = 0;
    private chunksTotal = 0;

    constructor(
        private readonly files: number,
        private readonly recentLimit = 5,
        windowMs = 20_000
    ) {
        this.rate = new RateWindow(windowMs);
    }

    prepared(path: string): void {
        this.push({ kind: "preparing", path });
    }

    indexed(path: string, chunks: number): void {
        this.indexedCount += 1;
        this.push({ kind: "indexed", path, note: `${chunks} chunks` });
    }

    skipped(path: string, reason: string): void {
        this.skippedCount += 1;
        this.reasons.set(reason, (this.reasons.get(reason) ?? 0) + 1);
        this.push({ kind: "skipped", path, note: reason.replace(/_/g, " ") });
    }

    /** A file this client refused to post — counted where the summary counts it. */
    droppedLocally(path: string): void {
        this.skipped(path, LOCAL_SKIP_REASON);
    }

    /**
     * One embed batch landed. `chunksDone`/`chunksTotal` are the server's own
     * cumulative counts — taken rather than re-accumulated locally, so a retry or
     * a batch boundary cannot make the client's total disagree with the server's.
     */
    embedded(chunksDone: number, chunksTotal: number, now: number): void {
        this.chunks = chunksDone;
        this.chunksTotal = chunksTotal;
        this.rate.push(now, chunksDone);
    }

    /**
     * The per-batch catch-up, for a server that ignored `?stream=yes`: no event
     * ever fires, so the counters would otherwise sit at zero for the whole run.
     * Idempotent against the streaming path, which has already counted these
     * files one by one — hence the `Math.max`, never a `+=`.
     */
    settledAtLeast(indexed: number, skipped: number): void {
        this.indexedCount = Math.max(this.indexedCount, indexed);
        this.skippedCount = Math.max(this.skippedCount, skipped);
    }

    snapshot(): IndexFeedSnapshot {
        const perSecond = this.rate.perSecond();
        return {
            line: this.line(perSecond),
            recent: [...this.ring].reverse(),
            chunksPerSecond: perSecond,
            indexed: this.indexedCount,
            skipped: this.skippedCount,
            skipReasons: Object.fromEntries(this.reasons),
            chunks: this.chunks,
            chunksTotal: this.chunksTotal,
            files: this.files,
        };
    }

    /**
     * Before the first two embed batches there is no rate to show, and a `0` there
     * would read as "stalled" during exactly the stretch the run is busiest —
     * reading files and slicing them. So the line says what it is doing instead.
     */
    private line(perSecond: number | undefined): string {
        const settled = this.indexedCount + this.skippedCount;
        if (perSecond === undefined) {
            return `preparing ${settled}/${this.files} files`;
        }
        return [
            `${Math.round(perSecond)} chunks/s`,
            `${this.indexedCount} indexed`,
            `${this.skippedCount} skipped`,
        ].join(" · ");
    }

    /**
     * One entry per path, replaced in place as it advances: a file that appeared
     * as `preparing` and then as `indexed` would otherwise take two of the five
     * lines to say one thing, and a batch of five would fill the stream with its
     * own first half.
     */
    private push(entry: FeedEntry): void {
        const at = this.ring.findIndex((e) => e.path === entry.path);
        if (at !== -1) {
            this.ring.splice(at, 1);
        }
        this.ring.push(entry);
        while (this.ring.length > this.recentLimit) {
            this.ring.shift();
        }
    }
}
