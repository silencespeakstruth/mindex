import { describe, it } from "node:test";
import * as assert from "node:assert";
import { IndexFeed } from "./indexFeed";

describe("IndexFeed", () => {
    it("keeps one line per path as it advances", () => {
        const feed = new IndexFeed(1);
        feed.prepared("src/a.rs");
        feed.indexed("src/a.rs", 14);
        const s = feed.snapshot();
        assert.strictEqual(s.recent.length, 1, "prepared+indexed is one file, not two");
        assert.deepStrictEqual(s.recent[0], {
            kind: "indexed",
            path: "src/a.rs",
            note: "14 chunks",
        });
        assert.strictEqual(s.indexed, 1);
    });

    it("caps the stream and keeps the newest, newest first", () => {
        const feed = new IndexFeed(9, 3);
        for (const p of ["a", "b", "c", "d", "e"]) {
            feed.prepared(p);
        }
        const s = feed.snapshot();
        assert.deepStrictEqual(
            s.recent.map((e) => e.path),
            ["e", "d", "c"]
        );
    });

    it("tallies skips by reason, local drops included", () => {
        const feed = new IndexFeed(4);
        feed.skipped("a.rs", "unchanged");
        feed.skipped("b.rs", "unchanged");
        feed.skipped("c.rs", "in_flight");
        feed.droppedLocally("logo.png");
        const s = feed.snapshot();
        assert.strictEqual(s.skipped, 4);
        assert.deepStrictEqual(s.skipReasons, {
            unchanged: 2,
            in_flight: 1,
            unsupported: 1,
        });
        // The reason is shown to a human, so the wire spelling is not.
        assert.strictEqual(s.recent[1].note, "in flight");
    });

    it("says what it is doing until a rate exists, then reports the rate", () => {
        const feed = new IndexFeed(10);
        feed.indexed("a.rs", 3);
        assert.strictEqual(feed.snapshot().chunksPerSecond, undefined);
        assert.strictEqual(feed.snapshot().line, "preparing 1/10 files");

        feed.embedded(100, 1000, 0);
        assert.strictEqual(
            feed.snapshot().chunksPerSecond,
            undefined,
            "one sample is no rate"
        );

        feed.embedded(300, 1000, 2000);
        const s = feed.snapshot();
        assert.strictEqual(s.chunksPerSecond, 100);
        assert.strictEqual(s.chunks, 300);
        assert.strictEqual(s.chunksTotal, 1000);
        assert.strictEqual(s.line, "100 chunks/s · 1 indexed · 0 skipped");
    });

    it("takes the server's cumulative chunk counts across batches", () => {
        const feed = new IndexFeed(2);
        feed.embedded(50, 50, 0);
        // The next batch is a second request, so its totals restart — the feed
        // shows what the server last said rather than a sum of its own.
        feed.beginBatch();
        feed.embedded(80, 200, 1000);
        assert.strictEqual(feed.snapshot().chunks, 80);
        assert.strictEqual(feed.snapshot().chunksTotal, 200);
        // …while the run-level count is the sum, which is what the rate reads.
        assert.strictEqual(feed.snapshot().runChunks, 130);
    });

    it("keeps the rate positive across a batch boundary", () => {
        // The regression: `chunks_done` restarts per request, so feeding it
        // straight to the RateWindow made `perSecond()` negative for a whole
        // window on any run longer than one batch.
        const feed = new IndexFeed(200);
        feed.embedded(50, 400, 0);
        feed.embedded(200, 400, 1000);
        feed.beginBatch();
        feed.embedded(40, 300, 2000);
        const s = feed.snapshot();
        assert.strictEqual(s.chunks, 40, "the batch pair stays the server's");
        assert.strictEqual(s.runChunks, 240);
        // (240 - 50) chunks over 2000 ms, the window's oldest sample being 50.
        assert.strictEqual(s.chunksPerSecond, 95);
    });

    it("never reports a negative rate, however many batches a run takes", () => {
        const feed = new IndexFeed(500);
        let t = 0;
        for (let batch = 0; batch < 4; batch += 1) {
            feed.beginBatch();
            for (const done of [30, 90, 150]) {
                t += 500;
                feed.embedded(done, 150, t);
                const rate = feed.snapshot().chunksPerSecond;
                assert.ok(
                    rate === undefined || rate >= 0,
                    `batch ${batch}: rate ${String(rate)} went backwards`
                );
            }
        }
        assert.strictEqual(feed.snapshot().runChunks, 600);
    });

    it("never lets the JSON-fallback catch-up double-count a streamed run", () => {
        const feed = new IndexFeed(3);
        feed.indexed("a.rs", 1);
        feed.indexed("b.rs", 1);
        feed.skipped("c.rs", "unchanged");
        feed.settledAtLeast(2, 1);
        const s = feed.snapshot();
        assert.strictEqual(s.indexed, 2);
        assert.strictEqual(s.skipped, 1);
    });
});
