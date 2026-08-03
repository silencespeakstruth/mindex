import { describe, it } from "node:test";
import { deepStrictEqual, strictEqual } from "node:assert";

import { debounce, throttle } from "./shared/debounce";

/** `setTimeout` resolution is not exact; give each wait room without slowing the suite. */
const wait = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

describe("debounce", () => {
    it("fires once, after the pause, with the last arguments", async () => {
        const calls: string[] = [];
        const d = debounce(20, (q: string) => calls.push(q));

        // A user typing "gc" — the prefix must never reach the server, because its
        // results would be wrong by the time they arrived.
        d("g");
        d("gc");
        d("gc ");
        strictEqual(calls.length, 0, "nothing fires during the wait");

        await wait(60);
        deepStrictEqual(calls, ["gc "], "the finished query, once");
    });

    it("starts a fresh wait on every call", async () => {
        const calls: number[] = [];
        const d = debounce(40, (n: number) => calls.push(n));

        d(1);
        await wait(25);
        d(2); // still inside the first wait, so it resets rather than fires
        await wait(25);
        strictEqual(calls.length, 0, "the second call restarted the timer");

        await wait(40);
        deepStrictEqual(calls, [2]);
    });

    it("cancel drops the pending call", async () => {
        const calls: number[] = [];
        const d = debounce(20, (n: number) => calls.push(n));
        d(1);
        d.cancel();
        await wait(60);
        // The disposal case: a timer firing into a torn-down webview is an error the
        // user sees and cannot act on.
        deepStrictEqual(calls, [], "a cancelled call must never arrive");
    });

    it("flush runs the pending call immediately, and only once", async () => {
        const calls: number[] = [];
        const d = debounce(1000, (n: number) => calls.push(n));
        d(7);
        d.flush();
        deepStrictEqual(calls, [7], "flush does not wait out the delay");
        await wait(20);
        deepStrictEqual(calls, [7], "the timer must not fire again after a flush");
    });

    it("flush with nothing pending does nothing", () => {
        const calls: number[] = [];
        const d = debounce(20, (n: number) => calls.push(n));
        d.flush();
        deepStrictEqual(calls, []);
    });
});

describe("throttle", () => {
    it("fires the first call of a burst at once", () => {
        const calls: number[] = [];
        const t = throttle(50, (n: number) => calls.push(n));
        t(1);
        deepStrictEqual(calls, [1], "leading edge — the opposite of debounce");
    });

    it("delivers the LAST call of a burst", async () => {
        // The regression this exists for. Indexing arrives in bursts: a whole batch
        // prepares in milliseconds and the run then goes quiet for a GPU pass.
        // Leading-only, the burst's final numbers never reach the screen and every
        // surface freezes one event short — for exactly the stretch it explains.
        const calls: number[] = [];
        const t = throttle(50, (n: number) => calls.push(n));
        for (let i = 1; i <= 20; i += 1) {
            t(i);
        }
        deepStrictEqual(calls, [1], "the rest are still inside the window");
        await wait(120);
        deepStrictEqual(calls, [1, 20], "the newest value lands when the window opens");
    });

    it("caps the rate without starving a steady stream", async () => {
        const calls: number[] = [];
        const t = throttle(30, (n: number) => calls.push(n));
        for (let i = 0; i < 6; i += 1) {
            t(i);
            await wait(20);
        }
        await wait(60);
        strictEqual(calls.length >= 2, true, "some calls got through");
        strictEqual(calls.length < 6, true, "and the rate was capped");
        strictEqual(calls[calls.length - 1], 5, "the newest value is never lost");
    });

    it("cancel drops the pending trailing call", async () => {
        const calls: number[] = [];
        const t = throttle(30, (n: number) => calls.push(n));
        t(1);
        t(2);
        t.cancel();
        await wait(80);
        deepStrictEqual(calls, [1], "the leading call happened, the trailing one did not");
    });

    it("flush runs the pending call immediately", () => {
        const calls: number[] = [];
        const t = throttle(50, (n: number) => calls.push(n));
        t(1);
        t(2);
        t.flush();
        deepStrictEqual(calls, [1, 2]);
    });
});
