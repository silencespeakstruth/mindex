import { describe, it } from "node:test";
import * as assert from "node:assert";
import {
    asTrust,
    asVerdict,
    bulkSelectionNote,
    challengeBadge,
    challengeGuard,
    challengeStateLine,
    corpusCountsLine,
    gcButtonLabel,
    gcProposalNote,
    gcRowReasons,
    provenanceExtras,
    recheckOptions,
    standingChallenge,
    subjectLabel,
    trustBadge,
    verificationView,
    ChallengeStatePresent,
    CorpusTotalsLike,
    GcRunLike,
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
    it("lets a valid research run through as a first challenge", () => {
        assert.deepStrictEqual(challengeGuard(run()), { ok: true, mode: "first" });
        // An unresolved challenge state is `first` too — the behaviour before any
        // of this existed, which is what an older server falls back to.
        assert.deepStrictEqual(challengeGuard(run(), undefined), {
            ok: true,
            mode: "first",
        });
        assert.deepStrictEqual(challengeGuard(run(), { state: "none" }), {
            ok: true,
            mode: "first",
        });
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

// ── The challenge state: every case gets a sentence ────────────────────────

function present(overrides: Partial<ChallengeStatePresent> = {}): ChallengeStatePresent {
    return {
        state: "present",
        id: "c-uuid",
        seq: 14,
        title: "Challenge research #12",
        verdict: "refuted",
        valid: true,
        ...overrides,
    };
}

describe("challengeStateLine", () => {
    it("says a report was never challenged, rather than saying nothing", () => {
        assert.match(challengeStateLine({ state: "none" }), /Never challenged/);
    });

    for (const verdict of ["confirmed", "disputed", "refuted"] as const) {
        it(`states a ${verdict} verdict outright`, () => {
            const line = challengeStateLine(present({ verdict }));
            assert.match(line, /Challenged by #14/);
            assert.match(line, new RegExp(`Verdict: ${verdict}\\.`));
        });
    }

    // The two cases trust is silent about, and the reported bug: a report that
    // HAD been challenged read as untouched, because `trust` correctly counts
    // neither of these.
    it("spells out an inconclusive verdict as not an acquittal", () => {
        const line = challengeStateLine(present({ verdict: null }));
        assert.match(line, /inconclusive/);
        assert.match(line, /not an acquittal/);
        assert.match(line, /counts toward no trust status/);
    });

    it("says a stale challenge stopped counting, instead of hiding it", () => {
        const line = challengeStateLine(present({ verdict: "refuted", valid: false }));
        assert.match(line, /Verdict: refuted/);
        assert.match(line, /no longer counts toward trust/);
    });

    it("names the standing verdict when more than one challenge stands", () => {
        const line = challengeStateLine({
            state: "several",
            count: 2,
            latest: present({ seq: 19, verdict: "confirmed" }),
        });
        assert.match(line, /Challenged 2 times/);
        assert.match(line, /standing verdict is #19/);
        assert.match(line, /reaches no verdict does not\s+replace one that did/);
    });
});

describe("standingChallenge", () => {
    it("is the row itself, the latest of several, or nothing", () => {
        assert.strictEqual(standingChallenge({ state: "none" }), undefined);
        assert.strictEqual(standingChallenge(present())?.seq, 14);
        assert.strictEqual(
            standingChallenge({ state: "several", count: 3, latest: present({ seq: 21 }) })
                ?.seq,
            21
        );
    });
});

describe("subjectLabel", () => {
    it("names the subject by seq and title when the server resolved it", () => {
        assert.strictEqual(
            subjectLabel({ challenged_seq: 12, challenged_title: "How GC works" }),
            "⚔ challenges #12 — How GC works"
        );
    });

    it("tells a deleted subject apart from a server that cannot resolve one", () => {
        // null: the server looked and there is nothing there.
        assert.strictEqual(
            subjectLabel({ challenged_seq: null, challenged_title: null }),
            "⚔ subject deleted"
        );
        // undefined: a 1.0.1 server, which sends neither field.
        assert.strictEqual(subjectLabel({}), "⚔ open subject");
    });
});

describe("challengeGuard, re-check mode", () => {
    it("offers a re-check rather than a second challenge", () => {
        const g = challengeGuard(run(), present({ verdict: "refuted" }));
        assert.strictEqual(g.ok, true);
        assert.strictEqual((g as { mode: string }).mode, "recheck");
        assert.match((g as { current: string }).current, /#14 — refuted/);
    });

    // The whole point of the verdict gate, and what the user must be told before
    // spending a slot: a losing re-run does not cost the standing verdict.
    it("warns that only a verdict-reaching run replaces the current one", () => {
        const g = challengeGuard(run(), present({ verdict: "refuted" }));
        const w = (g as { replaceWarning: string }).replaceWarning;
        assert.match(w, /already carries a refuted challenge \(#14\)/);
        assert.match(w, /if — and only if — the new run reaches a verdict/);
        assert.match(w, /inconclusive leaves the current verdict standing/);
    });

    it("calls an inconclusive standing challenge by that name", () => {
        const g = challengeGuard(run(), present({ verdict: null }));
        assert.match((g as { current: string }).current, /inconclusive/);
    });

    // The two refusals outrank the fork: an invalid subject is refused whether or
    // not it already carries a challenge.
    it("still refuses an invalid subject that already has a challenge", () => {
        const g = challengeGuard(run({ valid: false, invalid_reason: "stale" }), present());
        assert.strictEqual(g.ok, false);
    });
});

describe("recheckOptions", () => {
    it("distinguishes the free offline check from the one that spends a slot", () => {
        const o = recheckOptions(present({ seq: 14, verdict: "refuted" }));
        assert.match(o.links.detail, /No model, no GPU, nothing replaced/);
        assert.match(o.fresh.detail, /replaces\s+the current refuted one \(#14\)/);
        assert.match(o.fresh.detail, /inconclusive, \s*the current verdict stands/);
    });
});

// ── Corpus counts and the GC proposal ──────────────────────────────────────

function totals(overrides: Partial<CorpusTotalsLike> = {}): CorpusTotalsLike {
    return {
        total: 128,
        current: 74,
        gc_candidates: 31,
        gc_invalid: 20,
        gc_stale: 22,
        gc_partial: 9,
        gc_inconclusive: 2,
        ...overrides,
    };
}

describe("corpusCountsLine", () => {
    it("reports the corpus size and how much of it is usable", () => {
        assert.strictEqual(corpusCountsLine(totals()), "128 reports · 74 current");
    });

    it("says nothing rather than guessing when the server cannot count", () => {
        assert.strictEqual(corpusCountsLine(undefined), "— reports");
    });

    it("has a first-run wording", () => {
        assert.strictEqual(
            corpusCountsLine(totals({ total: 0, current: 0 })),
            "No stored reports yet"
        );
    });
});

describe("gcButtonLabel", () => {
    it("carries the candidate count, and drops it at zero", () => {
        assert.strictEqual(gcButtonLabel(totals()), "Collect garbage (31)");
        assert.strictEqual(gcButtonLabel(totals({ gc_candidates: 0 })), "Collect garbage");
        assert.strictEqual(gcButtonLabel(undefined), "Collect garbage");
    });
});

describe("gcRowReasons", () => {
    const gcRun = (o: Partial<GcRunLike> = {}): GcRunLike => ({
        valid: true,
        files_moved: 0,
        done_reason: "finalized",
        kind: "research",
        challenge_verdict: null,
        pinned: false,
        ...o,
    });

    it("proposes nothing for a clean finished report", () => {
        assert.deepStrictEqual(gcRowReasons(gcRun()), []);
    });

    it("lists every reason, not the first", () => {
        assert.deepStrictEqual(
            gcRowReasons(
                gcRun({ valid: false, files_moved: 3, done_reason: "time_exhausted" })
            ),
            ["invalid", "stale", "partial"]
        );
    });

    it("catches an inconclusive challenge, which carries no finding", () => {
        assert.deepStrictEqual(
            gcRowReasons(gcRun({ kind: "challenge", challenge_verdict: null })),
            ["inconclusive"]
        );
        assert.deepStrictEqual(
            gcRowReasons(gcRun({ kind: "challenge", challenge_verdict: "refuted" })),
            []
        );
    });

    // Pinning is the one action that takes a report off the table. If a bucket
    // could override it, the button's count and the proposal would disagree.
    it("never proposes a pinned report, whatever else is wrong with it", () => {
        assert.deepStrictEqual(
            gcRowReasons(gcRun({ pinned: true, valid: false, done_reason: "time_exhausted" })),
            []
        );
    });
});

describe("gcProposalNote", () => {
    it("says when it is showing fewer rows than match", () => {
        const note = gcProposalNote(50, 130);
        assert.match(note, /Proposing 50 reports/);
        assert.match(note, /80 more match but were not loaded/);
    });

    it("states the pinned exemption when the proposal is complete", () => {
        assert.match(gcProposalNote(31, 31), /Pinned reports are never proposed/);
    });

    it("has an empty-corpus wording", () => {
        assert.match(gcProposalNote(0, 0), /Nothing to collect/);
    });
});

describe("bulkSelectionNote", () => {
    it("admits how much of a filter selection is off screen", () => {
        assert.strictEqual(
            bulkSelectionNote(500, 50),
            "Selected by filter — 500 reports, 50 of them on screen."
        );
    });

    it("stays quiet when everything selected is visible", () => {
        assert.strictEqual(bulkSelectionNote(3, 50), undefined);
    });
});
