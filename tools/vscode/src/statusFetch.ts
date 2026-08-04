import { ConfigResponse, HealthResponse, MindexApi } from "./api";
import { humanize } from "./problem";
import { SectionErrors, ServerState, StatusSnapshot, UNAVAILABLE } from "./shared/status";

/**
 * Who a refresh tells what, besides producing a snapshot.
 *
 * These three exist because the Ask form is built from server-published values — the
 * language inventory, the model list, the effort ladder and the search bounds — and
 * the refresh is the only thing that reads them. They are **not** derived from the
 * snapshot by a subscriber: a subscriber can be absent, and the status surface is now
 * an editor panel that is closed almost all the time.
 */
/**
 * What the server can currently be asked to do.
 *
 * Two flags rather than one because the server has two classes of dependency and they
 * fail differently. A required one (SQLite, Qdrant, the embedder) takes *everything*
 * down and the server says so by reporting itself degraded; Ollama takes only Research
 * down and leaves health at `"ok"` deliberately. Collapsing them would either disable
 * Search whenever a local model was not running, or keep offering Research against a
 * server that cannot do it — which is what this used to do.
 */
export interface Availability {
    /** Anything at all: Search and Research both need the required dependencies. */
    ask: boolean;
    /** Additionally needs the server's optional Ollama. */
    research: boolean;
    /** Why not, in the user's words. Only set when something is unavailable. */
    reason?: string;
}

export interface StatusFanOut {
    /**
     * What the surfaces that *offer* work may currently offer.
     *
     * Forwarded from the fetch rather than derived by a snapshot subscriber, for the
     * same reason as the rest of this interface: the status panel is closed almost all
     * the time, and the Ask form must still learn that the server stopped answering.
     */
    onAvailability(availability: Availability): void;
    /**
     * The languages a search can actually match something in. `undefined` means
     * *unknown* (server unreachable, no project, a 404, or a server too old to
     * publish it), which a consumer must treat differently from `[]`: unknown falls
     * back to the full supported list, empty means the index really holds nothing.
     */
    onInventory(languages: string[] | undefined): void;
    /**
     * `GET /config`. Called only when there *is* one: a failed read leaves the
     * consumer's last known config standing, since stale budgets beat unlabelled ones.
     */
    onServerConfig(config: ConfigResponse): void;
}

/**
 * One refresh: `/health`, then `/status`, `/config`, the project's stats and its
 * failed-file list, folded into a snapshot. Never throws.
 *
 * Free of `vscode` on purpose — that is what makes it testable, and `StatusMonitor`
 * is then a thin `EventEmitter` around it rather than a class with untestable
 * internals. Everything below health is best-effort: health already answered, and a
 * section that fails degrades to `"unavailable"` rather than failing the refresh.
 */
/**
 * Which dependencies are down, as a sentence.
 *
 * Names them rather than saying "the server is degraded": the fix is different for
 * each, and the one thing the user needs from this message is which process to go and
 * start. Ollama is excluded — it never makes the server unusable, so listing it here
 * would blame the optional dependency for the required one's failure.
 */
function describeDegradation(requiredDown: string[]): string {
    return requiredDown.length === 0
        ? "the server reports itself unhealthy"
        : `${requiredDown.join(" and ")} ${requiredDown.length === 1 ? "is" : "are"} not answering`;
}

/**
 * The client's reading of `/health`, from `status` **and** `checks` together.
 *
 * Reading `status` alone would be wrong across versions, and wrong in the
 * dangerous direction: a server older than the tri-state verdict says `degraded`
 * when a *required* dependency is down, which under the new vocabulary paints
 * yellow and leaves the form armed against a server that cannot answer it. So a
 * failing required check is authoritative on its own, and `unhealthy` covers the
 * one failure with no failing check behind it — a wedged research slot.
 *
 * The complement of `ollama` is not "a second list of check names in the client":
 * it is one name, the only optional one, and it was already hard-coded here and
 * in the panel's `CHECK_META` before this.
 */
export function readHealth(health: HealthResponse): {
    state: Exclude<ServerState, "unreachable">;
    requiredDown: string[];
    researchAvailable: boolean;
} {
    const requiredDown = Object.entries(health.checks)
        .filter(([name, state]) => name !== "ollama" && state !== undefined && state !== "ok")
        .map(([name]) => name);
    // Absent is not down: an older server omits the check entirely, and that
    // must not read as a failure anywhere.
    const ollama = health.checks.ollama;
    const researchAvailable = ollama === undefined || ollama === "ok";

    const state =
        requiredDown.length > 0 || health.status === "unhealthy"
            ? "unhealthy"
            : health.status === "degraded" || !researchAvailable
              ? "degraded"
              : "ok";
    return { state, requiredDown, researchAvailable };
}

/**
 * Why a section could not be read, as a sentence that is never empty.
 *
 * `humanize` renders a cancellation as `""` — correct everywhere else, because the
 * user's own Stop deserves no notification. Here the cancellation is the refresh
 * *deadline* firing, which is news: with the empty string the panel drew three dim
 * "unavailable" rows carrying no tooltip and no cause, and the one thing that had
 * actually happened — the whole refresh ran out of time — was said nowhere.
 */
function sectionReason(e: unknown): string {
    const humanized = humanize(e);
    return humanized.cancelled
        ? "The refresh ran out of time before this section was read."
        : humanized.text;
}

export async function fetchStatus(
    api: MindexApi,
    guid: string | undefined,
    serverUrl: string,
    fan: StatusFanOut,
    signal?: AbortSignal
): Promise<StatusSnapshot> {
    let health: HealthResponse;
    try {
        health = await api.health(signal);
    } catch (e) {
        // A cancellation is silent everywhere else, but here it is the refresh
        // deadline firing and the snapshot must still say something: an empty
        // reason renders as a red indicator whose tooltip explains nothing.
        const humanized = humanize(e);
        const detail = humanized.cancelled
            ? "The health check did not finish in time."
            : humanized.text;
        fan.onAvailability({ ask: false, research: false, reason: detail });
        // Unknown, not empty: an unreachable server must leave the pickers at their
        // last known contents rather than blanking them. The config is simply not
        // re-pushed, for the same reason.
        fan.onInventory(undefined);
        return {
            at: Date.now(),
            serverUrl,
            state: "unreachable",
            detail,
            researchAvailable: false,
        };
    }

    const { state, requiredDown, researchAvailable } = readHealth(health);
    // Two flags, not three: a third would need a control that is live under
    // `unhealthy` and dead under Ollama-degradation, and there is none.
    fan.onAvailability({
        ask: state !== "unhealthy",
        research: state !== "unhealthy" && researchAvailable,
        ...(state === "unhealthy"
            ? { reason: describeDegradation(requiredDown) }
            : researchAvailable
              ? {}
              : { reason: "the server's Ollama is not answering" }),
    });

    const sectionErrors: SectionErrors = {};
    const next: StatusSnapshot = {
        at: Date.now(),
        serverUrl,
        state,
        version: health.version,
        checks: Object.fromEntries(
            Object.entries(health.checks).map(([k, v]) => [k, v ?? "unknown"])
        ),
        researchAvailable,
        sectionErrors,
    };

    try {
        next.runtime = await api.status(signal);
    } catch (e) {
        next.runtime = UNAVAILABLE;
        sectionErrors.runtime = sectionReason(e);
    }

    try {
        fan.onServerConfig(await api.config(signal));
    } catch (e) {
        // Decoration on the way in; the server validates on the way out — so this
        // never fails the refresh. It is still recorded: a stale model list that
        // nothing admits is stale is how a picker comes to offer a model the
        // server would refuse.
        sectionErrors.config = sectionReason(e);
    }

    if (guid === undefined) {
        fan.onInventory(undefined);
        return next;
    }

    try {
        const languages = (await api.projectStats(guid, signal)).languages;
        if (languages === undefined) {
            fan.onInventory(undefined); // a server too old to publish one
        } else {
            next.inventory = languages;
            // Only languages with live chunks: a filter on a language whose files all
            // failed (or sliced to nothing) returns a 404 that reads to the user as
            // "your query matched nothing".
            fan.onInventory(
                Object.entries(languages)
                    .filter(([, v]) => v.chunks_active > 0)
                    .map(([name]) => name)
                    .sort()
            );
        }
    } catch (e) {
        // Includes the 404 of a project that has never been indexed — which is
        // unknown, not empty, and not worth a reason line either.
        fan.onInventory(undefined);
        next.inventory = UNAVAILABLE;
        sectionErrors.inventory = sectionReason(e);
    }

    try {
        next.failed = (await api.listFiles(guid, { status: "failed" }, signal)).files;
    } catch (e) {
        next.failed = UNAVAILABLE;
        sectionErrors.failed = sectionReason(e);
    }

    return next;
}
