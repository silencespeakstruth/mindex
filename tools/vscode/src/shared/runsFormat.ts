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
 * Client-side mirror of the server's two refusals, so the Challenge button can
 * explain itself instead of collecting a 400. The server stays the authority —
 * `research.challenge_subject_is_challenge` and
 * `research.challenge_subject_invalid` still land when a stale summary let a
 * click through.
 */
export function challengeGuard(run: RunLike): { ok: true } | { ok: false; reason: string } {
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
    return { ok: true };
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
