import { strict as assert } from "node:assert";
import { describe, it } from "node:test";
import { BusyKeys } from "./busy";

function collector(): { posted: unknown[]; post: (m: unknown) => void } {
    const posted: unknown[] = [];
    return { posted, post: (m) => posted.push(m) };
}

/** A promise plus the handles to settle it, so a call can be held open. */
function deferred<T>(): {
    promise: Promise<T>;
    resolve: (v: T) => void;
    reject: (e: unknown) => void;
} {
    let resolve!: (v: T) => void;
    let reject!: (e: unknown) => void;
    const promise = new Promise<T>((res, rej) => {
        resolve = res;
        reject = rej;
    });
    return { promise, resolve, reject };
}

describe("BusyKeys", () => {
    it("admits the first caller and refuses the second while it runs", async () => {
        const { posted, post } = collector();
        const keys = new BusyKeys(post);
        const gate = deferred<string>();

        const first = keys.run("delete", () => gate.promise);
        assert.equal(keys.isBusy("delete"), true);

        let secondRan = false;
        const second = await keys.run("delete", () => {
            secondRan = true;
            return Promise.resolve("nope");
        });
        assert.equal(secondRan, false, "the refused call must not run");
        assert.equal(second, undefined);

        gate.resolve("done");
        assert.equal(await first, "done");
        assert.equal(keys.isBusy("delete"), false);

        // Exactly one pair for the admitted call and nothing for the refused
        // one: a stray `busy:false` would re-enable the button mid-call.
        assert.deepEqual(posted, [
            { type: "busy", key: "delete", busy: true },
            { type: "busy", key: "delete", busy: false },
        ]);
    });

    it("keeps different keys independent", async () => {
        const { post } = collector();
        const keys = new BusyKeys(post);
        const gate = deferred<void>();

        const held = keys.run("list", () => gate.promise);
        assert.equal(await keys.run("more", () => Promise.resolve("ran")), "ran");
        gate.resolve();
        await held;
    });

    /**
     * The failure mode this guards is permanent: a key left held by a throw
     * disables its button for the life of the panel, and the user's only remedy
     * is to close and reopen it.
     */
    it("releases the key when the work throws, and lets the error out", async () => {
        const { posted, post } = collector();
        const keys = new BusyKeys(post);

        await assert.rejects(
            keys.run("gc", () => Promise.reject(new Error("boom"))),
            /boom/
        );
        assert.equal(keys.isBusy("gc"), false);
        assert.deepEqual(posted, [
            { type: "busy", key: "gc", busy: true },
            { type: "busy", key: "gc", busy: false },
        ]);
        assert.equal(await keys.run("gc", () => Promise.resolve("again")), "again");
    });

    it("reset frees everything a disposed webview was holding", () => {
        const { post } = collector();
        const keys = new BusyKeys(post);
        const gate = deferred<void>();
        void keys.run("list", () => gate.promise);

        assert.equal(keys.isBusy("list"), true);
        keys.reset();
        assert.equal(keys.isBusy("list"), false);
        gate.resolve();
    });
});
