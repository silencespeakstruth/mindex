/**
 * The bearer token: what it says about itself, and when to say something about it.
 *
 * Two halves live here and neither touches `vscode`, so both are testable: reading
 * a token's claims (`describeToken`) and deciding what an indicator should show
 * (`expiryNotice`). The half that owns a `StatusBarItem` is `tokenStatusBar.ts`.
 *
 * WHAT THIS DOES NOT DO: verify anything. The signature is checked by the server
 * and by nothing else — a client that "validated" a token would be asserting a
 * fact it cannot establish, and the only honest reading of the payload here is as
 * a hint about a credential the *server* will judge. So a malformed token is not
 * an error, it is simply a token this file can say nothing about: the request goes
 * out, and the 401 that comes back is the authority.
 */

/** Claims worth showing a human. Everything is optional — see the file's note. */
export interface TokenFacts {
    /** `sub`: the label the token was minted under. */
    subject?: string;
    /** `exp` as epoch milliseconds. Absent for a non-expiring token — a real and
     *  deliberate state (`mint-token --days 0`), never an error. */
    expiresAtMs?: number;
    /** `prj`: the project GUIDs it reaches, or `["*"]`. */
    projects?: string[];
    /** `act`: the actions it permits. */
    actions?: string[];
    /**
     * `aud`: which kinds of holder it was minted for. Absent means **every** kind
     * — an unlabelled token must keep working everywhere, or adding the claim
     * would have locked out every holder on the day it shipped.
     */
    audiences?: string[];
}

/** What this client calls itself in an `aud` claim. */
export const AUDIENCE_VSCODE = "vscode";

/** Decodes one base64url segment. Returns undefined rather than throwing. */
function decodeSegment(segment: string): unknown {
    try {
        const padded = segment.replace(/-/g, "+").replace(/_/g, "/");
        const json = Buffer.from(padded, "base64").toString("utf8");
        return JSON.parse(json);
    } catch {
        return undefined;
    }
}

function stringList(v: unknown): string[] | undefined {
    if (!Array.isArray(v)) {
        return undefined;
    }
    const out = v.filter((x): x is string => typeof x === "string");
    return out.length > 0 ? out : undefined;
}

/**
 * What a token says about itself, or an empty object when it says nothing this
 * code can read.
 *
 * An empty result covers three cases that must stay indistinguishable to callers:
 * not a JWT at all, a JWT whose payload will not parse, and a JWT carrying none of
 * these claims. All three mean the same thing here — show no expiry indicator —
 * and separating them would invite a client-side verdict on a credential only the
 * server can judge.
 */
export function describeToken(token: string | undefined): TokenFacts {
    const parts = (token ?? "").trim().split(".");
    if (parts.length !== 3) {
        return {};
    }
    const payload = decodeSegment(parts[1]);
    if (typeof payload !== "object" || payload === null) {
        return {};
    }
    const claims = payload as Record<string, unknown>;
    const exp = claims.exp;
    return {
        subject: typeof claims.sub === "string" ? claims.sub : undefined,
        // Seconds on the wire, milliseconds everywhere in this extension. A
        // non-finite or negative `exp` is dropped rather than rendered as 1970.
        expiresAtMs:
            typeof exp === "number" && Number.isFinite(exp) && exp > 0
                ? exp * 1000
                : undefined,
        projects: stringList(claims.prj),
        actions: stringList(claims.act),
        // RFC 7519 allows `aud` to be a bare string as well as an array, and a
        // token minted by something other than this server is still one this
        // extension may legitimately be holding.
        audiences: typeof claims.aud === "string" ? [claims.aud] : stringList(claims.aud),
    };
}

/**
 * Why this extension should not use the token, as a sentence, or `undefined`.
 *
 * The claim it reads is a **label the server does not check** — nothing about an
 * HTTP request identifies the process behind it — so refusing here is the whole
 * mechanism rather than a second line of defence. That is also why it refuses
 * rather than warns: the request would succeed, and a warning followed by success
 * is read once and then never again.
 *
 * What it catches is one specific, likely mistake: an agent's short-lived token,
 * or a CLI credential, pasted into the extension's keychain entry. What it cannot
 * catch is anything adversarial — a holder simply does not run this check. So the
 * sentence names both audiences and points at the remedy, which is what a person
 * who has just mixed up two credentials actually needs.
 */
export function audienceRefusal(facts: TokenFacts): string | undefined {
    const aud = facts.audiences;
    // Unreadable, unlabelled, or labelled with an empty list: all three mean "no
    // reason visible from here", and separating them would invite a client-side
    // verdict on a credential only the server can judge.
    if (aud === undefined || aud.length === 0 || aud.includes(AUDIENCE_VSCODE)) {
        return undefined;
    }
    return (
        `This token was minted for ${aud.join(" + ")}, not for ${AUDIENCE_VSCODE}. ` +
        "The server does not check that label, so the token would probably work — which is " +
        "why it is refused here instead: a credential in the wrong place is usually the " +
        "wrong credential."
    );
}

/**
 * What the token says this extension may ask for, and why not when it may not.
 *
 * # Offering is a hint; validating is a contract
 *
 * This reads the payload of an unverified credential, so it is authoritative
 * about nothing. It decides what to *offer*; the server decides what to serve,
 * and a request that slips through gets a 403 the error funnel explains. The
 * asymmetry is the same one the language pickers already run on, and it is what
 * keeps a wrong reading here from ever being a refusal the server would not make.
 *
 * # Why a read-only extension must work rather than refuse to start
 *
 * A search-and-research client is exactly what a narrow token is *for*, and it is
 * the most valuable thing to hand somebody who should not be able to reindex. So
 * a missing action disables the tab's controls and states the reason, the way a
 * missing Ollama already does; it does not hide the tab (the explanation lives
 * behind it) and it does not stop the extension (which would delete the feature
 * in the surface that serves it best).
 *
 * # The project axis, which is where the sharp case lives
 *
 * A brand-new project's GUID is minted locally by the Drift view's welcome
 * button, so no token names it yet. Every request then answers 404
 * `project.not_found`, deliberately byte-identical to a project that never
 * existed — the server must not confirm which GUIDs exist. That is unanswerable
 * from the response, and perfectly answerable from here: this extension holds the
 * token and just wrote the GUID. Saying so is the only way the user learns that
 * the empty result is a scope decision and not an empty index.
 */
export interface TokenAvailability {
    /** Whether Search may be offered. */
    ask: boolean;
    /** Whether Research may be offered. */
    research: boolean;
    /** Set only when something is unavailable; already a full sentence. */
    reason?: string;
}

/** The wildcard `prj` entry. Must be spelled; an empty list reaches nothing. */
const WILDCARD_PROJECT = "*";

/**
 * Whether the token reaches `guid`, comparing the dashless form on both sides —
 * the server treats the two spellings as one project, and a client that did not
 * would report a scope problem that does not exist.
 */
export function tokenCovers(facts: TokenFacts, guid: string | undefined): boolean {
    if (facts.projects === undefined || guid === undefined) {
        return true;
    }
    const simple = guid.replace(/-/g, "").toLowerCase();
    return facts.projects.some(
        (p) => p === WILDCARD_PROJECT || p.replace(/-/g, "").toLowerCase() === simple
    );
}

/** Whether the token permits `action`; `true` when it names no actions at all. */
export function tokenPermits(facts: TokenFacts, action: string): boolean {
    return facts.actions === undefined || facts.actions.includes(action);
}

export function tokenAvailability(
    facts: TokenFacts,
    projectGuid: string | undefined
): TokenAvailability {
    if (!tokenCovers(facts, projectGuid)) {
        return {
            ask: false,
            research: false,
            reason:
                "this project is not in your token, so the server answers as though it did " +
                "not exist — which is what it answers for a GUID nobody has ever indexed, " +
                "deliberately, so the two cannot be told apart from the outside",
        };
    }
    const ask = tokenPermits(facts, "search");
    const research = tokenPermits(facts, "research");
    if (ask && research) {
        return { ask: true, research: true };
    }
    // One sentence names the missing action, not the whole grant: the user is
    // reading it because one control is dead, and the remedy is a token carrying
    // that action.
    const missing = [!ask ? "search" : undefined, !research ? "research" : undefined].filter(
        (x): x is string => x !== undefined
    );
    return {
        ask,
        research,
        reason: `your token does not carry ${missing.join(" or ")}`,
    };
}

/**
 * Fold the token's restrictions into what the server's health already said.
 *
 * Kept as a pure merge rather than being computed inside the status fetch,
 * because the two have different lifetimes: health is re-read on a timer and the
 * token changes only when someone stores one. Merging at the point of use is what
 * lets a token change repaint the form without waiting for the next poll.
 *
 * A token reason **wins** over a health reason for the mode it kills, and that
 * ordering is the useful one: a dependency that is down will come back by itself,
 * and a token that lacks an action will not.
 */
export function mergeAvailability<
    T extends { ask: boolean; research: boolean; reason?: string },
>(health: T, token: TokenAvailability): T {
    if (token.ask && token.research) {
        return health;
    }
    const ask = health.ask && token.ask;
    const research = health.research && token.research;
    return {
        ...health,
        ask,
        research,
        reason: token.reason ?? health.reason,
    };
}

/** How loudly the indicator should speak. */
export type ExpirySeverity = "expired" | "urgent" | "soon";

export interface ExpiryNotice {
    severity: ExpirySeverity;
    /** Milliseconds until `exp`; negative once it has passed. */
    remainingMs: number;
    /** Short, for a status bar: `42m`, `7h`, `3d`. Empty when expired. */
    short: string;
}

/** Under this much time left, the indicator turns into a warning. */
export const URGENT_MS = 60 * 60 * 1000;

/**
 * Whether to show anything, and how urgently.
 *
 * `undefined` — the common case — means say nothing: no token, a non-expiring one,
 * or one with more than `quietBeforeMs` left. A credential that is fine must cost
 * the user no screen space, which is what makes the indicator's mere presence
 * informative; an always-visible "token OK" is read once and then never again.
 *
 * The urgent threshold is a constant rather than a second setting: one knob for
 * "how early do I want to know" is a preference, two are a puzzle.
 */
export function expiryNotice(
    facts: TokenFacts,
    nowMs: number,
    quietBeforeMs: number
): ExpiryNotice | undefined {
    const { expiresAtMs } = facts;
    if (expiresAtMs === undefined) {
        return undefined;
    }
    const remainingMs = expiresAtMs - nowMs;
    if (remainingMs <= 0) {
        return { severity: "expired", remainingMs, short: "" };
    }
    if (remainingMs <= URGENT_MS) {
        return { severity: "urgent", remainingMs, short: humanizeRemaining(remainingMs) };
    }
    // `0` disables the early notice entirely, and must not be read as "warn always".
    if (quietBeforeMs <= 0 || remainingMs > quietBeforeMs) {
        return undefined;
    }
    return { severity: "soon", remainingMs, short: humanizeRemaining(remainingMs) };
}

/**
 * `42m` / `7h` / `3d`, rounded **down** so the indicator never claims more time
 * than there is. One unit only: a status bar entry is read at a glance, and
 * `2h 51m` is not read faster than `2h`.
 */
export function humanizeRemaining(ms: number): string {
    const minutes = Math.floor(ms / 60_000);
    if (minutes < 60) {
        return `${Math.max(minutes, 0)}m`;
    }
    const hours = Math.floor(minutes / 60);
    return hours < 48 ? `${hours}h` : `${Math.floor(hours / 24)}d`;
}

/**
 * How long until the indicator could next change, capped so a long-lived window
 * still re-checks occasionally.
 *
 * Polling on a fixed short interval to watch a clock the process already knows is
 * waste; a fixed long one lets the warning arrive after the token is dead. So the
 * delay is derived from the distance to the next boundary that would change what
 * is on screen, and clamped: never under `MIN_TICK_MS` (a boundary reached exactly
 * would otherwise re-arm at 0 and spin), never over `MAX_TICK_MS` (the machine may
 * have been asleep, so a wake-up must not be able to sit on a stale reading for
 * hours).
 */
export const MIN_TICK_MS = 15_000;
export const MAX_TICK_MS = 15 * 60 * 1000;

export function nextTickMs(facts: TokenFacts, nowMs: number, quietBeforeMs: number): number {
    const { expiresAtMs } = facts;
    if (expiresAtMs === undefined) {
        return MAX_TICK_MS;
    }
    const remainingMs = expiresAtMs - nowMs;
    if (remainingMs <= 0) {
        // Expired is terminal: nothing further changes until the token does, and
        // SecretStorage's change event is what will say so.
        return MAX_TICK_MS;
    }
    const boundaries = [remainingMs, remainingMs - URGENT_MS, remainingMs - quietBeforeMs]
        .filter((d) => d > 0)
        .concat(
            // Inside the urgent window the label counts minutes, so it has to be
            // redrawn every minute or it silently freezes at the value it had.
            remainingMs <= URGENT_MS ? [remainingMs % 60_000 || 60_000] : []
        );
    const soonest = Math.min(...boundaries, MAX_TICK_MS);
    return Math.max(soonest, MIN_TICK_MS);
}
