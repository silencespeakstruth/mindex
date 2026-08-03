/**
 * Wording and derivation for the challenge/trust/verification surfaces —
 * vscode-free and DOM-free so `node --test` reaches it.
 *
 * The wire types keep these fields as bare `string` on purpose (an unknown
 * future server value must not become a type lie — `done_reason` set the
 * precedent); the unions live here, behind narrowing guards, and every consumer
 * of the *meaning* of a value goes through them.
 */

/** Derived trust of a run, from valid challenges aimed at it. */
export type Trust = "refuted" | "disputed" | "confirmed" | "unchallenged";

/** A challenge's overall verdict about its subject. */
export type Verdict = "confirmed" | "disputed" | "refuted";

export type RunKind = "research" | "challenge";

export function asTrust(v: string): Trust | undefined {
    return v === "refuted" || v === "disputed" || v === "confirmed" || v === "unchallenged"
        ? v
        : undefined;
}

export function asVerdict(v: string | null): Verdict | undefined {
    return v === "confirmed" || v === "disputed" || v === "refuted" ? v : undefined;
}

/** The slice of a run summary this module reads. Structural on purpose: the host
 * imports the api.ts shape, the webview its own copy, and this file neither. */
export interface RunLike {
    kind: string;
    valid: boolean;
    invalid_reason: string | null;
    challenge_verdict: string | null;
    trust: string;
}

/** One badge: the text, a CSS class family, and the hover explanation. */
export interface BadgeSpec {
    label: string;
    kind: string;
    title: string;
}

/**
 * The badge a challenge row wears: what it concluded. `null` verdict is spelled
 * out as inconclusive — challenged-and-unparseable is NOT an acquittal, and a
 * blank badge would read as one.
 */
export function challengeBadge(run: RunLike): BadgeSpec | undefined {
    if (run.kind !== "challenge") {
        return undefined;
    }
    const verdict = asVerdict(run.challenge_verdict);
    return {
        label: verdict === undefined ? "challenge: inconclusive" : `challenge: ${verdict}`,
        kind: "deps",
        title:
            "This run attacked another report's claims. Inconclusive means its " +
            "verdict turn produced nothing parseable — not an acquittal.",
    };
}

/**
 * The badge a run wears for what valid challenges concluded about IT.
 * `unchallenged` is silent — it merely means untested, and a badge on every row
 * is a badge on none.
 */
export function trustBadge(run: RunLike): BadgeSpec | undefined {
    switch (asTrust(run.trust)) {
        case "refuted":
            return {
                label: "refuted",
                kind: "invalid",
                title: "A valid challenge run refuted this report's claims. Treat it as likely wrong.",
            };
        case "disputed":
            return {
                label: "disputed",
                kind: "incomplete",
                title: "A valid challenge run disputed some of this report's claims.",
            };
        case "confirmed":
            return {
                label: "confirmed",
                kind: "pinned",
                title: "A valid challenge run confirmed this report's claims.",
            };
        default:
            return undefined;
    }
}

/**
 * What the corpus holds about challenges aimed at one report.
 *
 * `none` is a first-class value, not the absence of one: the preview must be able
 * to say "never challenged" out loud. `several` exists because the server's
 * replace rule is gated on a *verdict* — an inconclusive re-check leaves the
 * standing verdict in place, so a subject can legitimately carry two rows, and
 * rows written before that rule can carry more.
 */
export type ChallengeState =
    | { state: "none" }
    | ChallengeStatePresent
    | { state: "several"; count: number; latest: ChallengeStatePresent };

/** One challenge aimed at the report, as the panel resolves it. */
export interface ChallengeStatePresent {
    state: "present";
    id: string;
    seq: number;
    title: string;
    verdict: Verdict | null;
    /** Whether this challenge's own evidence still stands. */
    valid: boolean;
}

/** The standing challenge — the one a re-check acts on. */
export function standingChallenge(s: ChallengeState): ChallengeStatePresent | undefined {
    if (s.state === "present") {
        return s;
    }
    return s.state === "several" ? s.latest : undefined;
}

/**
 * What the preview says about a report's challenge history — **always a
 * sentence, for every state**.
 *
 * This is the fix for a real gap: the panel used to render only a trust badge,
 * and trust is silent for exactly the two cases a reader most needs told. A
 * challenge whose verdict turn parsed to nothing counts toward no trust value,
 * and one whose own evidence has moved stops counting the moment it goes stale —
 * so a report that had been challenged and refuted could read as untouched.
 * Badges may stay quiet on a list (a badge on every row is a badge on none); the
 * preview is the place that answers outright.
 */
export function challengeStateLine(s: ChallengeState): string {
    if (s.state === "none") {
        return "Never challenged — no run has tried to re-derive this report's claims.";
    }
    if (s.state === "several") {
        return (
            `Challenged ${s.count} times; the standing verdict is #${s.latest.seq}. ` +
            `${verdictSentence(s.latest)} A challenge that reaches no verdict does not ` +
            "replace one that did, which is why more than one can stand."
        );
    }
    return `Challenged by #${s.seq}. ${verdictSentence(s)}`;
}

function verdictSentence(c: ChallengeStatePresent): string {
    if (c.verdict === null) {
        return (
            "Verdict: inconclusive — its verdict turn produced nothing parseable. " +
            "That is not an acquittal, and it counts toward no trust status."
        );
    }
    if (!c.valid) {
        return (
            `Verdict: ${c.verdict}, but this challenge's own evidence has since ` +
            "moved, so it no longer counts toward trust. Re-check it before acting on it."
        );
    }
    return `Verdict: ${c.verdict}.`;
}

/**
 * How a challenge row names the report it attacked, from the server-resolved
 * `challenged_seq`/`challenged_title`.
 *
 * The client used to look for the subject among the rows it happened to hold and
 * fall back to an anonymous link — which, on a list filtered to challenges, was
 * always. `null` now means the subject is genuinely gone; `undefined` means an
 * older server that cannot resolve it.
 */
export function subjectLabel(run: {
    challenged_seq?: number | null;
    challenged_title?: string | null;
}): string {
    if (run.challenged_seq === undefined) {
        return "⚔ open subject";
    }
    if (run.challenged_seq === null) {
        return "⚔ subject deleted";
    }
    const title = run.challenged_title ?? "";
    return title === ""
        ? `⚔ challenges #${run.challenged_seq}`
        : `⚔ challenges #${run.challenged_seq} — ${title}`;
}

/**
 * Client-side mirror of the server's two refusals plus the re-check fork, so the
 * button can explain itself instead of collecting a 400. The server stays the
 * authority — `research.challenge_subject_is_challenge` and
 * `research.challenge_subject_invalid` still land when a stale summary let a
 * click through.
 *
 * `mode` is the third answer, and the reason this returns more than a boolean: a
 * report that already carries a challenge must not offer "Challenge" as though
 * nothing had happened, because launching one now *replaces* the standing
 * verdict. `existing` undefined means not resolved yet (or an older server) —
 * treated as `first`, the behaviour before any of this existed.
 */
export function challengeGuard(
    run: RunLike,
    existing?: ChallengeState
):
    | { ok: true; mode: "first" }
    | { ok: true; mode: "recheck"; current: string; replaceWarning: string }
    | { ok: false; reason: string } {
    if (run.kind === "challenge") {
        return {
            ok: false,
            reason:
                "A challenge cannot itself be challenged — trust aggregation is " +
                "single-level. Challenge the original report instead.",
        };
    }
    if (!run.valid) {
        return {
            ok: false,
            reason:
                "This report is out of date, and staleness must not be spendable " +
                "as refutation. Re-run the question first, then challenge the fresh report.",
        };
    }
    const standing = existing === undefined ? undefined : standingChallenge(existing);
    if (standing === undefined) {
        return { ok: true, mode: "first" };
    }
    const verdict = standing.verdict ?? "inconclusive";
    return {
        ok: true,
        mode: "recheck",
        current: `#${standing.seq} — ${verdict}`,
        replaceWarning:
            `This report already carries a ${verdict} challenge (#${standing.seq}). ` +
            "A fresh run replaces it if — and only if — the new run reaches a verdict; " +
            "one that comes back inconclusive leaves the current verdict standing. " +
            "A replaced challenge and its report cannot be recovered.",
    };
}

/**
 * The two ways to re-check a standing challenge. Neither runs automatically —
 * one is free and instant, the other spends a research slot and can delete the
 * verdict now on the record, and that is not a choice to make for the user.
 */
export function recheckOptions(s: ChallengeStatePresent): {
    links: { label: string; detail: string };
    fresh: { label: string; detail: string };
} {
    const verdict = s.verdict ?? "inconclusive";
    return {
        links: {
            label: "Links only",
            detail:
                `Re-check challenge #${s.seq} offline: its own citations against the ` +
                "index as it stands now. No model, no GPU, nothing replaced.",
        },
        fresh: {
            label: "Fresh run",
            detail:
                "Run a new challenge on the GPU. If it reaches a verdict it replaces " +
                `the current ${verdict} one (#${s.seq}); if it comes back inconclusive, ` +
                "the current verdict stands.",
        },
    };
}

// ── Corpus counts and the garbage-collection proposal ──────────────────────

/** The totals shape this module reads (mirrors api.ts). */
export interface CorpusTotalsLike {
    total: number;
    current: number;
    challenges?: number;
    stale?: number;
    gc_candidates: number;
    gc_invalid: number;
    gc_stale: number;
    gc_partial: number;
    gc_inconclusive: number;
}

/** Which of the four things is wrong with a run a GC pass proposes deleting. */
export type GcBucket = "invalid" | "stale" | "partial" | "inconclusive";

export const GC_BUCKETS: readonly GcBucket[] = [
    "invalid",
    "stale",
    "partial",
    "inconclusive",
] as const;

/**
 * The corpus line in the panel head. `current` is the transitive validity
 * verdict, so this reads as "how many of these could be handed to the next
 * question" rather than as a softer notion of freshness.
 *
 * `undefined` totals is a server too old to send them — rendered as a dash
 * rather than a guess, because a wrong denominator is worse than none.
 */
export function corpusCountsLine(totals: CorpusTotalsLike | undefined): string {
    // Silent when there is nothing to count, and silent when the server cannot
    // count: the empty list already says so, in the middle of the panel and in
    // words, and a header note repeating it is one line of chrome saying what the
    // whole surface already said.
    if (totals === undefined || totals.total === 0) {
        return "";
    }
    // Four numbers, because a corpus is not one population: challenges are
    // *about* other reports rather than answers to questions, validity is what
    // the server will accept as context, and staleness is what has moved since.
    // "128 reports · 74 current" made the first two invisible.
    const parts = [`${totals.total} report${totals.total === 1 ? "" : "s"}`];
    if (totals.challenges !== undefined && totals.challenges > 0) {
        parts.push(`${totals.challenges} challenge${totals.challenges === 1 ? "" : "s"}`);
    }
    parts.push(`${totals.current} valid`);
    if (totals.stale !== undefined && totals.stale > 0) {
        parts.push(`${totals.stale} outdated`);
    }
    return parts.join(" · ");
}

/** The Collect-garbage button's label. `gc_candidates` is the union, never a sum. */
export function gcButtonLabel(totals: CorpusTotalsLike | undefined): string {
    const n = totals?.gc_candidates ?? 0;
    return n === 0 ? "Collect garbage" : `Collect garbage (${n})`;
}

/** The heading one group of proposed deletions gets, and why it is proposed. */
export function gcBucketLabel(bucket: GcBucket): { title: string; why: string } {
    switch (bucket) {
        case "invalid":
            return {
                title: "Out of date",
                why: "Their own files moved, or a report in their context chain did. The server already refuses these as context for a new question.",
            };
        case "stale":
            return {
                title: "Files changed",
                why: "At least one file they were written against has been edited or removed since. Their specifics may no longer hold.",
            };
        case "partial":
            return {
                title: "Stopped early",
                why: "A budget ended the run before it finished, so the report rests on partial evidence.",
            };
        case "inconclusive":
            return {
                title: "Inconclusive challenges",
                why: "Their verdict turn produced nothing parseable, so they carry no finding and count toward no trust status.",
            };
    }
}

/** The slice of a run the GC classification reads. */
export interface GcRunLike {
    valid: boolean;
    files_moved: number;
    done_reason: string;
    kind: string;
    challenge_verdict: string | null;
    pinned: boolean;
}

/**
 * Why this run is in the proposal — every reason, not the first.
 *
 * Empty means it is not a candidate at all. **Pinned always wins**: pinning is
 * the one action that takes a report off the table, and it must not be
 * overridable by a bucket, or the button's count and the proposal it builds
 * would disagree.
 *
 * Mirrors the server's `gc_*` predicates exactly. The two are separate on
 * purpose — the server counts the whole corpus in SQL, the client classifies the
 * rows it fetched — so this list is where they are kept in step.
 */
export function gcRowReasons(run: GcRunLike): GcBucket[] {
    if (run.pinned) {
        return [];
    }
    const out: GcBucket[] = [];
    if (!run.valid) {
        out.push("invalid");
    }
    if (run.files_moved > 0) {
        out.push("stale");
    }
    if (run.done_reason !== "finalized") {
        out.push("partial");
    }
    if (run.kind === "challenge" && run.challenge_verdict === null) {
        out.push("inconclusive");
    }
    return out;
}

/**
 * What the GC review says it is proposing, including whether the rows shown are
 * all of them.
 *
 * `shown < expected` means the paging loop stopped short, and saying so is not
 * optional: a review screen that silently truncates reads as "this is
 * everything" while being a sample.
 */
export function gcProposalNote(shown: number, expected: number | undefined): string {
    const head =
        shown === 0
            ? "Nothing to collect — every stored report is current, finished and unpinned."
            : `Proposing ${shown} report${shown === 1 ? "" : "s"} for deletion.`;
    if (expected !== undefined && shown < expected) {
        return (
            `${head} ${expected - shown} more match but were not loaded; run this ` +
            "again after deleting these."
        );
    }
    return `${head} Pinned reports are never proposed.`;
}

/**
 * What the delete confirmation adds when the selection was built by a filter
 * rather than by clicking rows. Deleting what you have not seen is the blind
 * spot of a bulk selection, and the count is the honest minimum.
 */
export function bulkSelectionNote(selected: number, onScreen: number): string | undefined {
    return selected > onScreen
        ? `Selected by filter — ${selected} reports, ${onScreen} of them on screen.`
        : undefined;
}

/** The verification response shape this module reads (mirrors api.ts). */
export interface VerificationLike {
    spans_available: boolean;
    recorded: CitationCountsLike;
    recomputed: CitationCountsLike | null;
    provenance_matches: boolean | null;
    stale_citations_now: number;
    stale_paths_now: string[];
    files_total: number;
    files_moved: number;
}

export interface CitationCountsLike {
    total: number;
    verified: number;
    path_only: number;
    unverified: number;
    stale: number;
}

/** The rendering model of one verification result: plain lines, no DOM. */
export interface VerificationView {
    /** Present only when provenance could be recomputed. */
    provenanceLine?: string;
    /** Present when it could not, saying why. */
    spansNote?: string;
    stalenessLine: string;
    /** A `provenance_matches: false` — a journal bug, never news about the code. */
    warning?: string;
}

function countsLine(c: CitationCountsLike): string {
    return `${c.verified}/${c.total} verified, ${c.path_only} path-only, ${c.unverified} unverified`;
}

/**
 * The two halves of an offline re-verification, kept deliberately separate:
 * provenance is immutable (a mismatch impeaches the journal, not the code),
 * staleness is measured against the index now and moves with it.
 */
export function verificationView(v: VerificationLike): VerificationView {
    const out: VerificationView = {
        stalenessLine:
            v.stale_citations_now === 0
                ? `Staleness now: 0 stale citations; ${v.files_moved}/${v.files_total} baseline files moved.`
                : `Staleness now: ${v.stale_citations_now} citation(s) point into rewritten code ` +
                  `(${v.stale_paths_now.join(", ")}); ${v.files_moved}/${v.files_total} baseline files moved.`,
    };
    if (!v.spans_available) {
        out.spansNote =
            "Stored before evidence spans were journalled — provenance cannot be " +
            "re-checked, only staleness.";
        return out;
    }
    if (v.recomputed !== null) {
        out.provenanceLine = `Provenance re-checked: ${countsLine(v.recomputed)} (recorded: ${countsLine(v.recorded)}).`;
    }
    if (v.provenance_matches === false) {
        out.warning =
            "Recomputed provenance disagrees with the recorded counters. That is a " +
            "journal bug in the server, never news about the code — report it.";
    }
    return out;
}

/**
 * The provenance-header lines for a stored report document. Returned as plain
 * Markdown blockquote lines; the caller joins them into the header.
 *
 * `subjectLabel` names the challenged report when the caller resolved it
 * (best-effort — the subject may be deleted); undefined falls back to the id.
 */
export function provenanceExtras(
    run: RunLike & { challenged_run_id: string | null },
    subjectLabel?: string
): string[] {
    const lines: string[] = [];
    if (run.kind === "challenge") {
        const subject =
            subjectLabel ?? run.challenged_run_id ?? "an unknown report (journal gap)";
        const verdict = asVerdict(run.challenge_verdict);
        lines.push(`> ⚔ **Challenge** of ${subject}.`);
        lines.push(
            verdict === undefined
                ? "> Verdict: **inconclusive** — the verdict turn produced nothing parseable. Not an acquittal."
                : `> Verdict: **${verdict}**.`
        );
        return lines;
    }
    switch (asTrust(run.trust)) {
        case "refuted":
            lines.push(
                "> ⚠️ **Refuted by a valid challenge** — do not read this report as settled. " +
                    "Its claims failed re-derivation through the tools."
            );
            break;
        case "disputed":
            lines.push(
                "> ⚠️ **Disputed by a valid challenge** — some of this report's claims " +
                    "did not survive re-derivation. Read it critically."
            );
            break;
        case "confirmed":
            lines.push("> Confirmed by a valid challenge run.");
            break;
        default:
            break;
    }
    return lines;
}
