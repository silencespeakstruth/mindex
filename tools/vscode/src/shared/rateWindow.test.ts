import { describe, it } from "node:test";
import * as assert from "node:assert";
import { RateWindow } from "./rateWindow";

describe("RateWindow", () => {
    it("is undefined until two samples span time", () => {
        const w = new RateWindow(10_000);
        assert.strictEqual(w.perSecond(), undefined);
        w.push(1000, 0);
        assert.strictEqual(w.perSecond(), undefined);
        w.push(1000, 50);
        assert.strictEqual(w.perSecond(), undefined, "zero elapsed time is no rate");
    });

    it("computes units per second across the window", () => {
        const w = new RateWindow(10_000);
        w.push(0, 0);
        w.push(2000, 100);
        assert.strictEqual(w.perSecond(), 50);
    });

    it("forgets samples older than the window, so the rate is current", () => {
        const w = new RateWindow(10_000);
        // A fast early burst...
        w.push(0, 0);
        w.push(1000, 1000);
        // ...then a slow stretch far outside the window.
        w.push(30_000, 1100);
        w.push(40_000, 1200);
        const rate = w.perSecond();
        assert.notStrictEqual(rate, undefined);
        // The burst (1000/s) must no longer dominate: the window covers the slow tail.
        assert.ok((rate as number) < 100, `rate should reflect the tail, got ${rate}`);
    });
});
