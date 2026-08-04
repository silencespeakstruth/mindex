import { strict as assert } from "node:assert";
import { describe, it } from "node:test";
import {
    ConfigurationError,
    humanize,
    MalformedResponseError,
    ProblemError,
    TimeoutError,
    UnreachableError,
} from "./problem";

function abort(): Error {
    const e = new Error("The operation was aborted");
    e.name = "AbortError";
    return e;
}

describe("humanize", () => {
    /**
     * The rule the whole error funnel rests on. `ProblemError.message` is
     * `code (status): detail` — a machine string — and every surface used to
     * render it, which is how users came to see `research.not_found (404)`. The
     * code has to survive somewhere, so it survives on its own field.
     */
    it("never puts the machine code in the sentence, and never loses it", () => {
        for (const status of [400, 404, 409, 413, 429, 500, 503, 418, 502]) {
            const h = humanize(new ProblemError(status, "some.machine_code", "Detail here."));
            assert.equal(h.cancelled, false);
            assert.ok(!h.text.includes("some.machine_code"), `${status}: ${h.text}`);
            assert.ok(!h.text.includes(String(status)), `${status}: ${h.text}`);
            assert.equal(h.code, "some.machine_code");
            assert.ok(h.text.length > 0);
        }
    });

    /**
     * Three ways of saying "the user did this on purpose", and all three must
     * render nothing: a notification for a Stop the user pressed reads as a
     * failure of the Stop.
     */
    it("treats every spelling of a cancellation as silent", () => {
        for (const e of [
            abort(),
            new ProblemError(499, "request.cancelled", "Client went away."),
            new ProblemError(499, "something.else", "Client went away."),
        ]) {
            assert.equal(humanize(e).cancelled, true);
        }
    });

    /**
     * A timeout must not be reported as an unreachable server: "is it running?"
     * is unhelpful advice about a server that is plainly running and stuck, and
     * it is the first thing the user has already checked.
     */
    it("separates a timeout from an unreachable server, and the two clocks", () => {
        const response = humanize(new TimeoutError(5000, "response"));
        assert.match(response.text, /did not answer within 5s/);
        assert.equal(response.retryable, true);

        const idle = humanize(new TimeoutError(180_000, "idle"));
        assert.match(idle.text, /went silent for 180s/);
        assert.equal(idle.retryable, true);

        const dead = humanize(
            new UnreachableError(new Error("connect ECONNREFUSED 127.0.0.1"))
        );
        assert.ok(!dead.text.includes("ECONNREFUSED"), dead.text);
        assert.match(dead.text, /mindex\.serverUrl/);
        assert.equal(dead.retryable, true);
    });

    it("reads a malformed answer as something listening, not something missing", () => {
        const h = humanize(new MalformedResponseError(new SyntaxError("Unexpected token <")));
        assert.ok(!h.text.includes("Unexpected token"), h.text);
        assert.equal(h.retryable, true);
    });

    /**
     * `retryable` is what a surface colours on and what decides whether a Retry
     * button appears, so a wrong value is either a dead button or a missing one.
     */
    it("marks exactly the failures a second press could survive", () => {
        const retryable = (status: number): boolean =>
            humanize(new ProblemError(status, "c", "d")).retryable;
        for (const status of [409, 429, 500, 503, 502]) {
            assert.equal(retryable(status), true, `${status} should be retryable`);
        }
        for (const status of [400, 404, 413, 418]) {
            assert.equal(retryable(status), false, `${status} should not be retryable`);
        }
    });

    /**
     * A 400's detail is server-authored English naming the offending field —
     * strictly better than anything the client could say about a request it
     * cannot see. Passed through; an empty one falls back rather than rendering
     * a blank notification.
     */
    it("passes a validation detail through and survives an empty one", () => {
        assert.equal(
            humanize(
                new ProblemError(400, "validation.top_k_out_of_range", "top_k must be 1..50.")
            ).text,
            "top_k must be 1..50."
        );
        assert.ok(humanize(new ProblemError(400, "c", "   ")).text.length > 0);
    });

    it("names the remedy for a busy server", () => {
        assert.match(
            humanize(new ProblemError(429, "research.busy", "No slot is free.")).text,
            /Active Research Runs/
        );
    });

    /**
     * The two credential refusals. They used to fall through to the generic `<500`
     * branch: the server's raw detail, not retryable, and nothing to press — which
     * left an expired token showing up only as requests that quietly stop working,
     * exactly the state the token indicator exists to prevent and cannot detect for
     * an opaque (non-JWT) token.
     */
    it("offers a way to replace a token the server would not accept", () => {
        const h = humanize(new ProblemError(401, "auth.token_expired", "expired"));
        assert.equal(h.retryable, false);
        assert.equal(h.action?.command, "mindex.setToken");
        assert.ok(h.text.length > 0);
        assert.ok(!h.text.includes("auth.token_expired"), "the code stays off the sentence");
        assert.equal(h.code, "auth.token_expired");
    });

    /**
     * 403 must not read as 401. The token is valid and replacing it is the wrong
     * advice; the missing *action* is what the server names, so that leads.
     */
    it("distinguishes a token that is not accepted from one that is too narrow", () => {
        const h = humanize(
            new ProblemError(
                403,
                "auth.action_not_permitted",
                "This token does not carry `mint`."
            )
        );
        assert.match(h.text, /does not carry `mint`/);
        assert.equal(h.retryable, false);
        assert.notEqual(h.action?.command, "mindex.setToken");
        assert.ok(h.action !== undefined, "a refusal with no way forward is the bug");
    });

    /**
     * A wrong `mindex.serverUrl` is the one failure whose remedy the client knows
     * exactly, and it was the one that reached "Something went wrong." — the throw
     * happens synchronously inside the request Promise's executor, so it escaped the
     * UnreachableError wrap entirely.
     */
    it("names the setting when the server address cannot be used", () => {
        const h = humanize(
            new ConfigurationError("mindex.serverUrl", "MINDex is served over HTTPS only.")
        );
        assert.match(h.text, /mindex\.serverUrl/);
        assert.equal(
            h.retryable,
            false,
            "a wrong setting does not fix itself on a second press"
        );
        assert.equal(h.action?.command, "mindex.openSettings");
        assert.notEqual(h.text, "Something went wrong.");
    });

    it("says something bounded about anything it has never seen", () => {
        for (const e of ["a string", 42, null, undefined, { nope: true }]) {
            const h = humanize(e);
            assert.equal(h.text, "Something went wrong.");
            assert.equal(h.cancelled, false);
        }
    });
});
