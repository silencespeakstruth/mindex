import { ok, strictEqual } from "node:assert";
import { describe, it } from "node:test";
import { Supersedable } from "./supersede";

describe("Supersedable", () => {
    it("aborts the previous read when a newer one starts", () => {
        const s = new Supersedable();
        const first = s.begin();
        strictEqual(first.signal.aborted, false);
        const second = s.begin();
        ok(first.signal.aborted, "the superseded read must be cancelled");
        strictEqual(second.signal.aborted, false);
    });

    /**
     * The whole reason this is a class. The Research History panel had this check in
     * one of three writers of the same handle and a bare `finally` in the other two,
     * so a superseded pass disowned its own successor on the way out: the spinner
     * went off mid-fetch, and the next keystroke then aborted nothing.
     */
    it("lets only the current owner release the handle", () => {
        const s = new Supersedable();
        const first = s.begin();
        const second = s.begin();

        strictEqual(s.end(first), false, "a superseded read must not report back");
        ok(s.busy, "its successor is still running");
        strictEqual(s.controller, second);

        strictEqual(s.end(second), true);
        strictEqual(s.busy, false);
    });

    it("reports not-busy only once nothing is running", () => {
        const s = new Supersedable();
        strictEqual(s.busy, false);
        const c = s.begin();
        ok(s.busy);
        s.end(c);
        strictEqual(s.busy, false);
    });

    it("releasing twice is not a second release", () => {
        const s = new Supersedable();
        const c = s.begin();
        strictEqual(s.end(c), true);
        strictEqual(s.end(c), false, "the second call owns nothing");
    });

    it("abort() cancels and gives the handle up without a reply", () => {
        const s = new Supersedable();
        const c = s.begin();
        s.abort();
        ok(c.signal.aborted);
        strictEqual(s.busy, false);
        strictEqual(s.end(c), false, "a disposed handle has no owner to report");
    });
});
