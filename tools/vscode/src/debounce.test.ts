import { describe, it } from "node:test";
import { deepStrictEqual, strictEqual } from "node:assert";

import { debounce } from "./shared/debounce";

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
