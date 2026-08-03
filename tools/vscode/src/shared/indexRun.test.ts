import { describe, it } from "node:test";
import * as assert from "node:assert";
import { IndexRun, runUnit } from "./indexRun";

const prepared = (path: string, language: string, chunks: number, symbols: number) => ({
    path,
    language,
    chunks,
    symbols,
});
const done = (files_indexed: number, chunks: number, elapsed_ms: number) => ({
    files: {},
    files_indexed,
    chunks,
    elapsed_ms,
});
const embedded = (chunks_done: number, chunks_total: number) => ({
    batch_chunks: 0,
    chunks_done,
    chunks_total,
    elapsed_ms: 0,
});

describe("IndexRun", () => {
    it("builds the per-language table from prepared and indexed together", () => {
        const run = new IndexRun(3, { now: 0 });
        run.beginBatch(0, 3, 0);
        run.prepared(prepared("src/a.rs", "rust", 12, 30), 10);
        run.prepared(prepared("src/b.rs", "rust", 4, 7), 11);
        run.prepared(prepared("doc.md", "markdown", 5, 0), 12);
        run.indexed({ path: "src/a.rs", language: "rust", count: 12 }, 20);
        run.indexed({ path: "src/b.rs", language: "rust", count: 4 }, 21);
        run.indexed({ path: "doc.md", language: "markdown", count: 5 }, 22);

        const s = run.snapshot();
        assert.deepStrictEqual(s.languages, [
            { language: "rust", filesIndexed: 2, filesSkipped: 0, chunks: 16, symbols: 37 },
            { language: "markdown", filesIndexed: 1, filesSkipped: 0, chunks: 5, symbols: 0 },
        ]);
        assert.strictEqual(s.symbols, 37, "symbols come from prepared, chunks from indexed");
    });

    it("does not count a file's symbols twice if indexed arrives twice", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 0);
        run.prepared(prepared("a.rs", "rust", 3, 9), 1);
        run.indexed({ path: "a.rs", language: "rust", count: 3 }, 2);
        run.indexed({ path: "a.rs", language: "rust", count: 3 }, 3);
        // The stash is consumed by the first settle, so a duplicate event cannot
        // re-add symbols the file only ever had once.
        assert.strictEqual(run.snapshot().symbols, 9);
    });

    it("puts a skip in its language's row and in the reason tally", () => {
        const run = new IndexRun(2, { now: 0 });
        run.beginBatch(0, 2, 0);
        run.skipped({ path: "a.rs", language: "rust", reason: "unchanged" }, 1);
        run.skipped({ path: "b.py", language: "python", reason: "in_flight" }, 2);
        const s = run.snapshot();
        assert.deepStrictEqual(s.skipReasons, { unchanged: 1, in_flight: 1 });
        assert.deepStrictEqual(
            s.languages.map((l) => [l.language, l.filesSkipped]),
            [
                ["rust", 1],
                ["python", 1],
            ]
        );
    });

    it("keeps run-level chunks monotonic while the batch pair stays the server's", () => {
        const run = new IndexRun(300, { now: 0, batchCount: 3 });
        let t = 0;
        for (let b = 0; b < 3; b += 1) {
            run.beginBatch(b, 100, t);
            for (const d of [40, 100]) {
                t += 1000;
                run.embedded(embedded(d, 100), t);
            }
            run.batchDone(done(100, 100, 500), t);
        }
        const s = run.snapshot();
        assert.strictEqual(
            s.chunksEmbedded,
            300,
            "run-level embedded total is the sum of the batches"
        );
        assert.strictEqual(s.batchChunks, 100, "the pair on screen is the last request's");
        assert.strictEqual(s.batchChunksTotal, 100);
        assert.strictEqual(s.batchCount, 3);
        assert.strictEqual(s.serverElapsedMs, 1500);
        assert.ok((s.chunksPerSecond ?? -1) >= 0, "no negative rate across boundaries");
    });

    it("reports peak and average as different numbers, both honest", () => {
        const run = new IndexRun(10, { now: 0 });
        run.beginBatch(0, 10, 0);
        // A slow first second, then a fast one: 100 chunks in 1 s, then 400 in 1 s.
        run.embedded(embedded(0, 500), 0);
        run.embedded(embedded(100, 500), 1000);
        run.embedded(embedded(500, 500), 2000);
        run.finish(2000, "done");
        const s = run.snapshot();
        assert.strictEqual(s.averageChunksPerSecond, 250, "500 chunks over 2 s of wall clock");
        assert.ok(
            (s.peakChunksPerSecond ?? 0) >= (s.averageChunksPerSecond ?? 0),
            "the peak is the embedder's figure and cannot be below the average"
        );
    });

    it("records a batch answered without a live stream", () => {
        const run = new IndexRun(2, { now: 0, batchCount: 2 });
        run.beginBatch(0, 1, 0);
        run.batchDone(done(1, 5, 10), 100);
        assert.strictEqual(run.snapshot().streamed, true);

        run.beginBatch(1, 1, 200);
        run.batchDone(undefined, 300, false);
        const s = run.snapshot();
        assert.strictEqual(s.streamed, false, "one JSON batch makes the whole run unstreamed");
        assert.deepStrictEqual(
            s.batches.map((b) => b.streamed),
            [true, false],
            "and each batch still says which it was"
        );
    });

    it("flips the unit label for a symbols-only run", () => {
        const run = new IndexRun(1, { now: 0 });
        assert.strictEqual(run.snapshot().symbolsOnly, false);
        assert.strictEqual(runUnit(false), "chunks");

        run.started({ files: 1, symbols_only: true }, 5);
        assert.strictEqual(run.snapshot().symbolsOnly, true);
        assert.strictEqual(runUnit(true), "symbol rows");
    });

    it("halves the rate series at the cap, keeping the first and last samples", () => {
        const run = new IndexRun(1, { now: 0, maxSamples: 6 });
        run.beginBatch(0, 1, 0);
        let chunks = 0;
        for (let i = 0; i <= 20; i += 1) {
            chunks += 10;
            run.embedded(embedded(chunks, 1000), i * 1000);
        }
        const s = run.snapshot();
        assert.ok(s.rateSamples.length <= 6, "the series stays bounded");
        assert.ok(s.rateSamples.length >= 3, "and is not emptied");
        assert.strictEqual(
            s.rateSamples[s.rateSamples.length - 1].t,
            20_000,
            "the newest sample survives the halving"
        );
        assert.ok(
            s.rateSamples[0].t < 6000,
            "and so does the start of the run — a shift would have hidden it"
        );
    });

    it("counts what the run set out to send apart from what it posted", () => {
        const run = new IndexRun(5, { now: 0 });
        run.beginBatch(0, 3, 0);
        run.droppedLocally("logo.png", undefined, 1);
        run.droppedLocally("huge.bin", undefined, 2);
        const s = run.snapshot();
        assert.strictEqual(s.files, 5);
        assert.strictEqual(s.filesPosted, 3);
        assert.strictEqual(s.skipped, 2);
        assert.deepStrictEqual(s.skipReasons, { unsupported: 2 });
    });

    it("carries the drift check's in-flight correction into the summary", () => {
        const run = new IndexRun(1, { now: 0 });
        run.finish(10, "done");
        assert.strictEqual(run.snapshot().inFlight, undefined);
        run.applyInFlight(2);
        assert.strictEqual(run.snapshot().inFlight, 2);
    });

    it("mirrors the feed's one line so both surfaces read the same", () => {
        const run = new IndexRun(10, { now: 0 });
        run.beginBatch(0, 10, 0);
        run.indexed({ path: "a.rs", language: "rust", count: 3 }, 1);
        assert.strictEqual(run.snapshot().line, run.feedSnapshot().line);
        assert.strictEqual(run.snapshot().line, "preparing 1/10 files");
    });
});

/**
 * The phase machine, against the timing the live server was measured to produce:
 * every `prepared` inside the first few hundred milliseconds, then one long silent
 * embed pass, then every `indexed` at once. Naming the phase is the only thing
 * that makes that middle stretch legible, so the transitions are worth pinning.
 */
describe("IndexRun phases", () => {
    it("enters embedding when the server has accounted for every announced file", () => {
        const run = new IndexRun(2, { now: 0, batchCount: 1 });
        run.beginBatch(0, 2, 1);
        assert.strictEqual(run.snapshot().phase, "reading");

        run.started({ files: 2, symbols_only: false }, 2);
        assert.strictEqual(run.snapshot().phase, "preparing");

        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        assert.strictEqual(
            run.snapshot().phase,
            "preparing",
            "one file still unaccounted for"
        );

        run.prepared(prepared("b.rs", "rust", 7, 4), 4);
        const s = run.snapshot();
        assert.strictEqual(s.phase, "embedding", "the prepare phase is over");
        assert.strictEqual(s.phaseSince, 4, "and it is timed from the transition");
    });

    it("counts a skipped file toward the prepare phase ending", () => {
        const run = new IndexRun(2, { now: 0 });
        run.beginBatch(0, 2, 1);
        run.started({ files: 2, symbols_only: false }, 2);
        run.skipped({ path: "a.rs", language: "rust", reason: "unchanged" }, 3);
        run.prepared(prepared("b.rs", "rust", 7, 4), 4);
        assert.strictEqual(run.snapshot().phase, "embedding");
    });

    it("never calls a wholly skipped batch an embed pass", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.started({ files: 1, symbols_only: false }, 2);
        run.skipped({ path: "a.rs", language: "rust", reason: "unchanged" }, 3);
        assert.notStrictEqual(run.snapshot().phase, "embedding", "nothing went to the GPU");
    });

    it("labels the embed pass even when the server never sent started", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.prepared(prepared("a.rs", "rust", 5, 3), 2);
        run.embedded({ batch_chunks: 5, chunks_done: 5, chunks_total: 5, elapsed_ms: 900 }, 3);
        assert.strictEqual(run.snapshot().phase, "embedding");
    });

    it("goes idle when the batch and the run end", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.started({ files: 1, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        run.indexed({ path: "a.rs", language: "rust", count: 5 }, 4);
        assert.strictEqual(run.snapshot().phase, "settling");
        run.batchDone(done(1, 5, 100), 5);
        assert.strictEqual(run.snapshot().phase, "idle");
    });
});

/**
 * The one list. A file appears on it once, where the server first mentioned it,
 * and afterwards only its state moves — which is what lets the page change a mark
 * in place instead of writing the same path twice, once looking busy and once
 * looking finished.
 */
describe("IndexRun file list", () => {
    it("puts every prepared file into the GPU pass together, then settles them in place", () => {
        const run = new IndexRun(3, { now: 0 });
        run.beginBatch(0, 3, 1);
        run.started({ files: 3, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        run.prepared(prepared("b.rs", "rust", 7, 4), 4);
        run.prepared(prepared("c.rs", "rust", 2, 1), 5);

        let rows = run.snapshot().rows;
        assert.deepStrictEqual(
            rows.map((f) => f.state),
            ["embedding", "embedding", "embedding"],
            "one /encode call holds all three"
        );
        assert.deepStrictEqual(
            rows.map((f) => f.path),
            ["c.rs", "b.rs", "a.rs"],
            "newest first, and that order is insertion order — nothing re-sorts"
        );

        run.indexed({ path: "b.rs", language: "rust", count: 7 }, 20);
        rows = run.snapshot().rows;
        assert.deepStrictEqual(
            rows.map((f) => f.path),
            ["c.rs", "b.rs", "a.rs"],
            "a file that settled stays exactly where it was"
        );
        assert.strictEqual(rows[1].state, "indexed");
        assert.strictEqual(rows[1].chunks, 7, "with the server's own count on it");
        assert.strictEqual(rows[1].symbols, 4, "and the symbols prepared reported");
    });

    it("writes one row per path, however many events the file gets", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.started({ files: 1, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        assert.strictEqual(run.snapshot().rows.length, 1);
        run.indexed({ path: "a.rs", language: "rust", count: 5 }, 4);
        const rows = run.snapshot().rows;
        assert.strictEqual(rows.length, 1, "settling advances the row, it does not add one");
        assert.strictEqual(rows[0].state, "indexed");
    });

    it("keeps earlier batches on the list", () => {
        const run = new IndexRun(2, { now: 0, batchCount: 2 });
        run.beginBatch(0, 1, 1);
        run.started({ files: 1, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        run.indexed({ path: "a.rs", language: "rust", count: 5 }, 4);
        run.batchDone(done(1, 5, 100), 5);

        run.beginBatch(1, 1, 6);
        run.started({ files: 1, symbols_only: false }, 7);
        run.prepared(prepared("b.rs", "rust", 3, 2), 8);
        assert.deepStrictEqual(
            run.snapshot().rows.map((f) => f.path),
            ["b.rs", "a.rs"],
            "the list is the run, not the batch"
        );
    });

    it("drops the oldest settled rows at the cap, never one still in flight", () => {
        const run = new IndexRun(6, { now: 0, maxRows: 3 });
        run.beginBatch(0, 6, 1);
        run.started({ files: 6, symbols_only: false }, 2);
        for (const p of ["a", "b", "c"]) {
            run.prepared(prepared(`${p}.rs`, "rust", 1, 1), 3);
            run.indexed({ path: `${p}.rs`, language: "rust", count: 1 }, 4);
        }
        for (const p of ["d", "e", "f"]) {
            run.prepared(prepared(`${p}.rs`, "rust", 1, 1), 5);
        }
        const rows = run.snapshot().rows;
        assert.strictEqual(rows.length, 3);
        assert.deepStrictEqual(
            rows.map((f) => f.path),
            ["f.rs", "e.rs", "d.rs"],
            "the cap evicts settled rows, never a file still in flight"
        );
    });

    it("puts a file this client refused to post on the list too", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 0, 1);
        run.droppedLocally("logo.png", undefined, 2);
        const rows = run.snapshot().rows;
        assert.strictEqual(rows[0].state, "skipped");
        assert.strictEqual(rows[0].note, "unsupported");
        assert.strictEqual(rows[0].language, undefined, "and it has no language to draw");
    });
});

/**
 * A file posted into a run that then died used to keep its in-progress state for
 * good — a page still drawing work on a request that had already failed.
 */
describe("IndexRun answers for every file when the run ends", () => {
    it("marks what never settled as failed, and says why", () => {
        const run = new IndexRun(2, { now: 0 });
        run.beginBatch(0, 2, 1);
        run.started({ files: 2, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        run.prepared(prepared("b.rs", "rust", 7, 4), 4);
        run.indexed({ path: "a.rs", language: "rust", count: 5 }, 5);
        run.finish(6, "error", { code: "index.failed", detail: "socket hang up" });

        const s = run.snapshot();
        const byPath = new Map(s.rows.map((f) => [f.path, f]));
        assert.strictEqual(
            byPath.get("a.rs")?.state,
            "indexed",
            "a settled file is left alone"
        );
        assert.strictEqual(byPath.get("b.rs")?.state, "failed");
        assert.strictEqual(byPath.get("b.rs")?.note, "index.failed");
        assert.strictEqual(s.failed, 1);
    });

    it("calls a cancelled run's leftovers cancelled, not failed", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.started({ files: 1, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        run.finish(4, "cancelled");
        const s = run.snapshot();
        assert.strictEqual(s.rows[0].state, "cancelled");
        assert.strictEqual(s.rows[0].note, "cancelled");
        assert.strictEqual(s.failed, 1, "unfinished either way — the count does not lie");
    });

    it("does not quietly leave a file in flight on a run that reported done", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.started({ files: 1, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        run.finish(4, "done");
        assert.strictEqual(run.snapshot().rows[0].state, "failed");
        assert.strictEqual(run.snapshot().rows[0].note, "no result reported");
    });
});

/**
 * Counting at `prepared` is what makes the metric block move at all: the server
 * sends every `prepared` inside the first few hundred milliseconds and then goes
 * silent for the whole embed pass, so a counter that waits for `indexed` is a
 * counter that reads zero for 96% of the run and then lands complete.
 */
describe("IndexRun counts what the server has already reported", () => {
    it("moves chunks and symbols at prepared, long before indexed", () => {
        const run = new IndexRun(2, { now: 0 });
        run.beginBatch(0, 2, 1);
        run.started({ files: 2, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 12, 30), 3);

        let s = run.snapshot();
        assert.strictEqual(s.chunks, 12, "the file is sliced and its rows are written");
        assert.strictEqual(s.symbols, 30);
        assert.strictEqual(s.indexed, 0, "but nothing has settled yet");
        assert.strictEqual(s.chunksEmbedded, 0, "and the GPU has not seen it");
        assert.deepStrictEqual(s.languages[0], {
            language: "rust",
            filesIndexed: 0,
            filesSkipped: 0,
            chunks: 12,
            symbols: 30,
        });

        run.prepared(prepared("b.rs", "rust", 5, 7), 4);
        s = run.snapshot();
        assert.strictEqual(s.chunks, 17);
        assert.strictEqual(s.symbols, 37);
    });

    it("reconciles against indexed rather than adding it on top", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.started({ files: 1, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 12, 30), 3);
        run.indexed({ path: "a.rs", language: "rust", count: 12 }, 4);

        const s = run.snapshot();
        assert.strictEqual(s.chunks, 12, "counted once, not twice");
        assert.strictEqual(s.symbols, 30, "symbols never come from count");
        assert.strictEqual(s.languages[0].chunks, 12);
        assert.strictEqual(s.languages[0].symbols, 30);
    });

    it("takes the server's number when indexed disagrees with prepared", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.prepared(prepared("a.rs", "rust", 12, 30), 2);
        run.indexed({ path: "a.rs", language: "rust", count: 9 }, 3);
        const s = run.snapshot();
        assert.strictEqual(s.chunks, 9, "indexed.count is authoritative");
        assert.strictEqual(s.languages[0].chunks, 9);
    });

    it("counts a file the stream never prepared, so the JSON fallback still tallies", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.indexed({ path: "a.rs", language: "rust", count: 9 }, 2);
        assert.strictEqual(run.snapshot().chunks, 9);
    });

    it("takes back a provisional count when the file turns out to be skipped", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 1);
        run.prepared(prepared("a.rs", "rust", 12, 30), 2);
        run.skipped({ path: "a.rs", language: "rust", reason: "unchanged" }, 3);
        const s = run.snapshot();
        assert.strictEqual(s.chunks, 0, "a skipped file contributes nothing");
        assert.strictEqual(s.symbols, 0);
        assert.strictEqual(s.languages[0].chunks, 0);
    });

    it("keeps the average over embedded chunks, not over reported ones", () => {
        const run = new IndexRun(1, { now: 0 });
        run.beginBatch(0, 1, 0);
        run.prepared(prepared("a.rs", "rust", 500, 10), 100);
        // A tenth of a second in, 500 chunks are *reported* but none embedded. A
        // wall-clock average over the reported total would read 5000 chunks/s here
        // and then collapse — the run has done no embedding at all yet.
        assert.strictEqual(run.snapshot().averageChunksPerSecond, undefined);
        run.embedded(embedded(0, 500), 100);
        run.embedded(embedded(500, 500), 2100);
        run.finish(2100, "done");
        assert.strictEqual(run.snapshot().averageChunksPerSecond, (500 * 1000) / 2100);
    });
});

describe("IndexRun keeps one fact on one surface", () => {
    it("survives the run it describes, so the summary reads against the same list", () => {
        const run = new IndexRun(2, { now: 0 });
        run.beginBatch(0, 2, 1);
        run.started({ files: 2, symbols_only: false }, 2);
        run.prepared(prepared("a.rs", "rust", 5, 3), 3);
        run.skipped({ path: "b.rs", language: "rust", reason: "unchanged" }, 4);
        run.indexed({ path: "a.rs", language: "rust", count: 5 }, 5);
        run.batchDone(done(1, 5, 100), 6);
        run.finish(7, "done");

        const s = run.snapshot();
        assert.deepStrictEqual(
            s.rows.map((f) => [f.path, f.state]),
            [
                ["b.rs", "skipped"],
                ["a.rs", "indexed"],
            ],
            "a finished run still says what happened to each file, once"
        );
        assert.strictEqual(s.failed, 0);
        assert.strictEqual(s.indexed + s.skipped, 2, "and the counters agree with it");
    });
});
