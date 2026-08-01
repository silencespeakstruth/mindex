import { describe, it } from "node:test";
import * as assert from "node:assert";
import {
    asTrust,
    asVerdict,
    challengeBadge,
    challengeGuard,
    provenanceExtras,
    trustBadge,
    verificationView,
    RunLike,
    VerificationLike,
} from "./runsFormat";

function run(
    overrides: Partial<RunLike & { challenged_run_id: string | null }> = {}
): RunLike & {
    challenged_run_id: string | null;
} {
    return {
        kind: "research",
        valid: true,
        invalid_reason: null,
        challenge_verdict: null,
        challenged_run_id: null,
        trust: "unchallenged",
        ...overrides,
    };
}

describe("narrowing guards", () => {
    it("accept the wire spellings and nothing else", () => {
        assert.strictEqual(asTrust("refuted"), "refuted");
        assert.strictEqual(asTrust("unchallenged"), "unchallenged");
        assert.strictEqual(asTrust("Refuted"), undefined);
        assert.strictEqual(asVerdict("disputed"), "disputed");
        assert.strictEqual(asVerdict(null), undefined);
        assert.strictEqual(asVerdict("inconclusive"), undefined);
    });
});

describe("challengeBadge", () => {
    it("is absent on research runs", () => {
        assert.strictEqual(challengeBadge(run()), undefined);
    });

    it("spells a null verdict as inconclusive — never an acquittal", () => {
        const b = challengeBadge(run({ kind: "challenge" }));
        assert.ok(b);
        assert.strictEqual(b.label, "challenge: inconclusive");
        assert.match(b.title, /not an acquittal/);
    });

    it("names the verdict when there is one", () => {
        const b = challengeBadge(run({ kind: "challenge", challenge_verdict: "refuted" }));
        assert.strictEqual(b?.label, "challenge: refuted");
    });
});

describe("trustBadge", () => {
    it("is silent for unchallenged — a badge on every row is a badge on none", () => {
        assert.strictEqual(trustBadge(run()), undefined);
        assert.strictEqual(trustBadge(run({ trust: "something-new" })), undefined);
    });

    it("says refuted must be treated as likely wrong", () => {
        const b = trustBadge(run({ trust: "refuted" }));
        assert.strictEqual(b?.kind, "invalid");
        assert.match(b.title, /likely wrong/);
    });
});

describe("challengeGuard", () => {
    it("lets a valid research run through", () => {
        assert.deepStrictEqual(challengeGuard(run()), { ok: true });
    });

    it("refuses a challenge — trust aggregation is single-level", () => {
        const g = challengeGuard(run({ kind: "challenge" }));
        assert.strictEqual(g.ok, false);
        assert.match((g as { reason: string }).reason, /single-level/);
    });

    it("refuses an invalid subject — staleness is not spendable as refutation", () => {
        const g = challengeGuard(run({ valid: false, invalid_reason: "stale" }));
        assert.strictEqual(g.ok, false);
        assert.match((g as { reason: string }).reason, /out of date/);
    });
});

describe("verificationView", () => {
    const counts = { total: 4, verified: 3, path_only: 1, unverified: 0, stale: 0 };
    function v(overrides: Partial<VerificationLike> = {}): VerificationLike {
        return {
            spans_available: true,
            recorded: counts,
            recomputed: counts,
            provenance_matches: true,
            stale_citations_now: 0,
            stale_paths_now: [],
            files_total: 5,
            files_moved: 0,
            ...overrides,
        };
    }

    it("renders both halves when spans are available", () => {
        const view = verificationView(v());
        assert.match(view.provenanceLine ?? "", /3\/4 verified/);
        assert.match(view.stalenessLine, /0 stale citations/);
        assert.strictEqual(view.warning, undefined);
        assert.strictEqual(view.spansNote, undefined);
    });

    it("pre-v1.3.0: staleness only, and says why", () => {
        const view = verificationView(v({ spans_available: false, recomputed: null }));
        assert.strictEqual(view.provenanceLine, undefined);
        assert.match(view.spansNote ?? "", /only staleness/);
    });

    it("a provenance mismatch is a journal bug, never news about the code", () => {
        const view = verificationView(v({ provenance_matches: false }));
        assert.match(view.warning ?? "", /journal bug/);
    });

    it("staleness names the paths that moved", () => {
        const view = verificationView(
            v({ stale_citations_now: 2, stale_paths_now: ["src/a.rs"], files_moved: 1 })
        );
        assert.match(view.stalenessLine, /src\/a\.rs/);
        assert.match(view.stalenessLine, /1\/5 baseline files moved/);
    });
});

describe("provenanceExtras", () => {
    it("a challenge names its subject and its verdict", () => {
        const lines = provenanceExtras(
            run({
                kind: "challenge",
                challenged_run_id: "abc",
                challenge_verdict: "disputed",
            }),
            "#7 How GC works"
        );
        assert.match(lines[0] ?? "", /Challenge.*#7 How GC works/);
        assert.match(lines[1] ?? "", /\*\*disputed\*\*/);
    });

    it("an inconclusive challenge says so — not an acquittal", () => {
        const lines = provenanceExtras(run({ kind: "challenge", challenged_run_id: "abc" }));
        assert.match(lines[1] ?? "", /inconclusive/);
        assert.match(lines[1] ?? "", /Not an acquittal/);
    });

    it("a refuted report must not read as settled", () => {
        const lines = provenanceExtras(run({ trust: "refuted" }));
        assert.match(lines[0] ?? "", /do not read this report as settled/);
    });

    it("an unchallenged report gets no extra lines", () => {
        assert.deepStrictEqual(provenanceExtras(run()), []);
    });
});
