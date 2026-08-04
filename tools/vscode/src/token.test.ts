import { strict as assert } from "node:assert";
import { describe, it } from "node:test";
import {
    audienceRefusal,
    describeToken,
    expiryNotice,
    humanizeRemaining,
    MAX_TICK_MS,
    MIN_TICK_MS,
    mergeAvailability,
    nextTickMs,
    tokenAvailability,
    URGENT_MS,
} from "./token";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** Builds a token the way the server does: three base64url segments. */
function jwt(claims: Record<string, unknown>): string {
    const seg = (o: unknown) =>
        Buffer.from(JSON.stringify(o), "utf8")
            .toString("base64")
            .replace(/\+/g, "-")
            .replace(/\//g, "_")
            .replace(/=+$/, "");
    return `${seg({ alg: "HS256", kid: "default" })}.${seg(claims)}.c2lnbmF0dXJl`;
}

describe("describeToken", () => {
    it("reads what mint-token puts in a token", () => {
        const facts = describeToken(
            jwt({
                sub: "vscode@wonderlandx",
                exp: 1_800_000_000,
                prj: ["c2d7e2c1316542f593660ff1492b4bab"],
                act: ["search", "research"],
            })
        );
        assert.equal(facts.subject, "vscode@wonderlandx");
        assert.equal(facts.expiresAtMs, 1_800_000_000_000);
        assert.deepEqual(facts.projects, ["c2d7e2c1316542f593660ff1492b4bab"]);
        assert.deepEqual(facts.actions, ["search", "research"]);
    });

    /**
     * `mint-token --days 0` is a real and deliberate state — it is what the host's
     * own admin token and the metrics scraper's use. Reading a missing `exp` as
     * "expired" would put a permanent red warning on a token that is fine.
     */
    it("reads a non-expiring token as having no expiry, not as expired", () => {
        const facts = describeToken(jwt({ sub: "admin", prj: ["*"], act: ["admin"] }));
        assert.equal(facts.expiresAtMs, undefined);
        assert.equal(expiryNotice(facts, Date.now(), DAY), undefined);
    });

    /**
     * The extension cannot verify a signature, so anything it cannot parse must
     * degrade to "nothing to say" rather than to a verdict. The request still goes
     * out and the server's 401 is the authority.
     */
    it("says nothing about a token it cannot read", () => {
        for (const bad of [
            undefined,
            "",
            "not-a-token",
            "a.b",
            "a.!!!.c",
            `a.${btoa("7")}.c`,
        ]) {
            assert.deepEqual(describeToken(bad), {}, `must say nothing about ${String(bad)}`);
        }
    });

    it("drops an exp that is not a usable number", () => {
        for (const exp of ["soon", 0, -5, Number.NaN]) {
            assert.equal(describeToken(jwt({ exp })).expiresAtMs, undefined, `exp=${exp}`);
        }
    });
});

describe("expiryNotice", () => {
    const now = 1_000_000_000_000;
    const at = (msLeft: number) => ({ expiresAtMs: now + msLeft });

    it("stays silent while the token is healthy", () => {
        assert.equal(expiryNotice(at(5 * DAY), now, DAY), undefined);
    });

    it("speaks quietly inside the configured window", () => {
        const n = expiryNotice(at(6 * HOUR), now, DAY);
        assert.equal(n?.severity, "soon");
        assert.equal(n?.short, "6h");
    });

    it("turns urgent under an hour regardless of the setting", () => {
        // The setting says "do not warn early"; it must not be able to suppress the
        // warning that arrives when the credential is about to stop working.
        const n = expiryNotice(at(42 * MINUTE), now, 0);
        assert.equal(n?.severity, "urgent");
        assert.equal(n?.short, "42m");
    });

    it("reports an expired token as expired, not as urgent", () => {
        const n = expiryNotice(at(-1), now, DAY);
        assert.equal(n?.severity, "expired");
        assert.ok(n !== undefined && n.remainingMs < 0);
    });

    /** `0` means "no early notice", which is not "warn always". */
    it("treats a zero window as disabling only the early notice", () => {
        assert.equal(expiryNotice(at(6 * HOUR), now, 0), undefined);
        assert.equal(expiryNotice(at(30 * MINUTE), now, 0)?.severity, "urgent");
    });

    it("is exactly at the boundary rather than one tick past it", () => {
        assert.equal(expiryNotice(at(URGENT_MS), now, DAY)?.severity, "urgent");
        assert.equal(expiryNotice(at(URGENT_MS + 1), now, DAY)?.severity, "soon");
    });
});

describe("humanizeRemaining", () => {
    it("rounds down so it never claims more time than there is", () => {
        assert.equal(humanizeRemaining(119 * MINUTE), "1h");
        assert.equal(humanizeRemaining(59_999), "0m");
    });

    it("uses one unit, switching at two days", () => {
        assert.equal(humanizeRemaining(47 * HOUR), "47h");
        assert.equal(humanizeRemaining(49 * HOUR), "2d");
    });
});

describe("nextTickMs", () => {
    const now = 1_000_000_000_000;
    const at = (msLeft: number) => ({ expiresAtMs: now + msLeft });

    it("never re-arms at zero, which would spin", () => {
        // The boundary reached exactly is the case: the distance to it is 0, and a
        // 0 ms timeout re-entering the same computation is a busy loop.
        assert.ok(nextTickMs(at(URGENT_MS), now, DAY) >= MIN_TICK_MS);
        assert.ok(nextTickMs(at(1), now, DAY) >= MIN_TICK_MS);
    });

    it("still re-checks a distant expiry, because the machine may have slept", () => {
        assert.equal(nextTickMs(at(30 * DAY), now, DAY), MAX_TICK_MS);
        assert.equal(nextTickMs({}, now, DAY), MAX_TICK_MS);
    });

    it("ticks every minute inside the urgent window, where the label counts minutes", () => {
        // 42m30s left: the label reads "42m" and must be redrawn in 30s, not in 15m.
        assert.ok(nextTickMs(at(42 * MINUTE + 30_000), now, DAY) <= MINUTE);
    });

    it("wakes up for the boundary it is approaching", () => {
        // 10 minutes before the early-notice window opens, nothing else is nearer —
        // and it is under MAX_TICK_MS, which would otherwise be the answer.
        assert.equal(nextTickMs(at(DAY + 10 * MINUTE), now, DAY), 10 * MINUTE);
    });

    /** The cap is a floor on how *late* a check may be, never a target. */
    it("never waits longer than the cap even for a nearer boundary", () => {
        assert.equal(nextTickMs(at(DAY + 40 * MINUTE), now, DAY), MAX_TICK_MS);
    });
});

describe("audienceRefusal", () => {
    /**
     * The backwards-compatibility half, and the one that must not regress: a
     * token nobody labelled is for everybody. Reading an absent `aud` as an empty
     * allow-list locks out every existing holder at once, with a message about a
     * concept they have never met.
     */
    it("accepts a token that names no audience at all", () => {
        assert.equal(audienceRefusal(describeToken(jwt({ sub: "x" }))), undefined);
        assert.equal(audienceRefusal(describeToken(jwt({ sub: "x", aud: [] }))), undefined);
    });

    it("accepts a token that names this client among others", () => {
        assert.equal(
            audienceRefusal(describeToken(jwt({ aud: ["cli", "vscode"] }))),
            undefined
        );
    });

    it("refuses a token minted for another client, naming both", () => {
        const refusal = audienceRefusal(describeToken(jwt({ aud: ["agent"] })));
        assert.ok(refusal !== undefined);
        assert.match(refusal, /agent/);
        assert.match(refusal, /vscode/);
    });

    /**
     * A client that refused what it could not read would break the day a server
     * started issuing an opaque credential — and would break with a message about
     * audiences, which is not what happened.
     */
    it("says nothing about a token it cannot read", () => {
        for (const bad of ["", "not-a-token", "a.b", "a.b.c.d"]) {
            assert.equal(audienceRefusal(describeToken(bad)), undefined, bad);
        }
    });

    /** RFC 7519 allows the scalar spelling, and mindex is not the only minter. */
    it("reads the scalar spelling as a one-element list", () => {
        assert.equal(audienceRefusal(describeToken(jwt({ aud: "vscode" }))), undefined);
        assert.ok(audienceRefusal(describeToken(jwt({ aud: "agent" }))) !== undefined);
    });
});

describe("tokenAvailability", () => {
    const guid = "c2d7e2c1316542f593660ff1492b4bab";

    /** Authorization off, or a credential this code cannot read: offer everything. */
    it("restricts nothing when the token says nothing", () => {
        assert.deepEqual(tokenAvailability(describeToken(undefined), guid), {
            ask: true,
            research: true,
        });
    });

    it("offers both when the token carries both", () => {
        const facts = describeToken(jwt({ prj: [guid], act: ["search", "research"] }));
        assert.deepEqual(tokenAvailability(facts, guid), { ask: true, research: true });
    });

    /**
     * The read-only case this whole mechanism exists to serve. Search must stay
     * offered: a token with `search` alone is a perfectly good credential, and
     * disabling the tab it serves would be the extension deciding that a narrow
     * grant is no grant.
     */
    it("keeps Search when only Research is missing, and names what is missing", () => {
        const facts = describeToken(jwt({ prj: [guid], act: ["search"] }));
        const out = tokenAvailability(facts, guid);
        assert.equal(out.ask, true);
        assert.equal(out.research, false);
        // Not `/search/` — "research" contains it. The point is that the sentence
        // names only what is missing, so a token whose Search works must not be
        // described as missing search.
        assert.equal(out.reason, "your token does not carry research");
    });

    it("closes both when the token carries neither", () => {
        const facts = describeToken(jwt({ prj: [guid], act: ["index"] }));
        const out = tokenAvailability(facts, guid);
        assert.equal(out.ask, false);
        assert.equal(out.research, false);
        assert.match(out.reason ?? "", /search or research/);
    });

    /**
     * The sharp case: a GUID the Drift view just wrote into `.mindex` that no
     * token names. The server answers 404 for it and for a GUID that never
     * existed, byte-identically and on purpose, so this is the only place the
     * difference can be stated.
     */
    it("explains an out-of-scope project rather than letting it read as an empty index", () => {
        const facts = describeToken(
            jwt({ prj: ["4bc4d6f0d2e94f7fa9b6c2e4f8a1b3d5"], act: ["search"] })
        );
        const out = tokenAvailability(facts, guid);
        assert.equal(out.ask, false);
        assert.equal(out.research, false);
        assert.match(out.reason ?? "", /not in your token/);
    });

    it("treats a wildcard token as covering a project it never names", () => {
        const facts = describeToken(jwt({ prj: ["*"], act: ["search", "research"] }));
        assert.deepEqual(tokenAvailability(facts, guid), { ask: true, research: true });
    });

    /**
     * The server treats the dashed and dashless spellings as one project, so a
     * client that told them apart would report a scope problem that does not
     * exist — and would report it as "this project is not in your token", which
     * is the most alarming sentence available.
     */
    it("does not care how either side spelled the GUID", () => {
        const dashed = "c2d7e2c1-3165-42f5-9366-0ff1492b4bab";
        for (const inToken of [dashed, guid, dashed.toUpperCase()]) {
            for (const asked of [dashed, guid]) {
                const facts = describeToken(jwt({ prj: [inToken], act: ["search"] }));
                assert.equal(
                    tokenAvailability(facts, asked).ask,
                    true,
                    `${inToken} vs ${asked}`
                );
            }
        }
    });

    /** No project open: there is nothing to be out of scope of. */
    it("says nothing about scope when no project is selected", () => {
        const facts = describeToken(jwt({ prj: [guid], act: ["search", "research"] }));
        assert.deepEqual(tokenAvailability(facts, undefined), { ask: true, research: true });
    });
});

describe("mergeAvailability", () => {
    const ok = { ask: true, research: true };

    it("leaves an unrestricted token's health verdict byte-for-byte alone", () => {
        const health = { ask: false, research: false, reason: "qdrant is not answering" };
        assert.equal(mergeAvailability(health, ok), health);
    });

    /**
     * A dependency comes back by itself; a missing action does not. So when both
     * have something to say about a mode, the permanent one is the one worth the
     * user's attention.
     */
    it("prefers the token's reason, which is the one that will not fix itself", () => {
        const merged = mergeAvailability(
            { ask: true, research: false, reason: "the server's Ollama is not answering" },
            { ask: true, research: false, reason: "your token does not carry research" }
        );
        assert.match(merged.reason ?? "", /token/);
    });

    /**
     * When the server itself is down, that is the whole story and the token's
     * complaint is noise on top of it.
     *
     * The naive rule — "the token reason always wins, because it will not fix
     * itself" — is right for a mode the server could still serve and wrong here:
     * it reports "your token does not carry research" to somebody whose Search is
     * also dead, sending them to re-mint a credential that was never the problem.
     */
    it("yields to the health reason when the server can serve nothing at all", () => {
        const merged = mergeAvailability(
            { ask: false, research: false, reason: "qdrant is not answering" },
            { ask: true, research: false, reason: "your token does not carry research" }
        );
        assert.equal(merged.reason, "qdrant is not answering");
    });

    /** Neither side may re-enable what the other closed. */
    it("closes a mode either side closes", () => {
        assert.deepEqual(
            mergeAvailability({ ask: false, research: false, reason: "down" }, ok),
            { ask: false, research: false, reason: "down" }
        );
        const merged = mergeAvailability(ok, {
            ask: true,
            research: false,
            reason: "your token does not carry research",
        });
        assert.equal(merged.ask, true);
        assert.equal(merged.research, false);
    });
});
