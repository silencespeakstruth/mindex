import { deepStrictEqual, strictEqual } from "node:assert";
import { describe, it } from "node:test";
import { AskFormState } from "./askState";

describe("AskFormState", () => {
    it("replays nothing when nothing was happening", () => {
        deepStrictEqual(new AskFormState().replay(), []);
    });

    /**
     * The dead end this exists for. The Ask sidebar is destroyed when it is
     * collapsed; a page rebuilt during a run used to draw Submit enabled and Stop
     * hidden, so the only control that could end the run was the one missing, and
     * pressing Submit earned a toast telling the user to cancel it first.
     */
    it("replays a run that is still in flight", () => {
        const s = new AskFormState();
        s.setRunning(true);
        deepStrictEqual(s.replay(), [{ type: "running", running: true }]);
    });

    it("stops replaying a run once it has finished", () => {
        const s = new AskFormState();
        s.setRunning(true);
        s.setRunning(false);
        deepStrictEqual(s.replay(), []);
    });

    it("replays the keys the host is still refusing", () => {
        const s = new AskFormState();
        s.setBusy("submit", true);
        deepStrictEqual(s.replay(), [{ type: "busy", key: "submit", busy: true }]);
    });

    it("does not replay a key that was released", () => {
        const s = new AskFormState();
        s.setBusy("submit", true);
        s.setBusy("submit", false);
        deepStrictEqual(s.held, []);
        deepStrictEqual(s.replay(), []);
    });

    it("replays the mode once, and never again", () => {
        const s = new AskFormState();
        s.requestMode("research");
        deepStrictEqual(s.replay(), [{ type: "mode", mode: "research" }]);
        // A request, not a state: replaying it would drag the user back out of a
        // tab they had since switched to by hand.
        deepStrictEqual(s.replay(), []);
    });

    it("replays every live fact together, mode first", () => {
        const s = new AskFormState();
        s.requestMode("search");
        s.setRunning(true);
        s.setBusy("submit", true);
        deepStrictEqual(s.replay(), [
            { type: "mode", mode: "search" },
            { type: "running", running: true },
            { type: "busy", key: "submit", busy: true },
        ]);
    });

    it("reports the run to the host, not only to the page", () => {
        const s = new AskFormState();
        strictEqual(s.running, false);
        s.setRunning(true);
        strictEqual(s.running, true);
    });
});
