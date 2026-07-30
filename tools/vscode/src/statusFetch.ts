import { ConfigResponse, HealthResponse, MindexApi } from "./api";
import { UnreachableError } from "./problem";
import { StatusSnapshot, UNAVAILABLE } from "./shared/status";

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
 * start. Ollama is excluded — it never causes degradation, so listing it here would
 * blame the optional dependency for the required one's failure.
 */
function describeDegradation(checks: Record<string, string | undefined>): string {
    const down = Object.entries(checks)
        .filter(([name, state]) => name !== "ollama" && state !== "ok")
        .map(([name]) => name);
    return down.length === 0
        ? "the server reports itself degraded"
        : `${down.join(" and ")} ${down.length === 1 ? "is" : "are"} not answering`;
}

export async function fetchStatus(
    api: MindexApi,
    guid: string | undefined,
    serverUrl: string,
    fan: StatusFanOut
): Promise<StatusSnapshot> {
    let health: HealthResponse;
    try {
        health = await api.health();
    } catch (e) {
        const detail = e instanceof UnreachableError ? e.cause_.message : String(e);
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

    // Ollama is the server's *optional* dependency: `status` stays "ok" without it,
    // and only Research stops working. Older servers omit the check entirely — absent
    // is not down, so it must not read as a failure anywhere.
    const ollama = health.checks.ollama;
    const researchAvailable = ollama === undefined || ollama === "ok";
    // A required dependency is down. The server is the authority on which checks are
    // required — that is exactly what `status` reports — so this reads its verdict
    // rather than keeping a second list of check names in the client.
    const degraded = health.status !== "ok";
    fan.onAvailability({
        ask: !degraded,
        research: !degraded && researchAvailable,
        ...(degraded
            ? { reason: describeDegradation(health.checks) }
            : researchAvailable
              ? {}
              : { reason: "the server's Ollama is not answering" }),
    });

    const next: StatusSnapshot = {
        at: Date.now(),
        serverUrl,
        state: health.status === "ok" ? "ok" : "degraded",
        version: health.version,
        checks: Object.fromEntries(
            Object.entries(health.checks).map(([k, v]) => [k, v ?? "unknown"])
        ),
        researchAvailable,
    };

    try {
        next.runtime = await api.status();
    } catch {
        next.runtime = UNAVAILABLE;
    }

    try {
        fan.onServerConfig(await api.config());
    } catch {
        // Decoration on the way in; the server validates on the way out.
    }

    if (guid === undefined) {
        fan.onInventory(undefined);
        return next;
    }

    try {
        const languages = (await api.projectStats(guid)).languages;
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
    } catch {
        // Includes the 404 of a project that has never been indexed — which is
        // unknown, not empty.
        fan.onInventory(undefined);
        next.inventory = UNAVAILABLE;
    }

    try {
        next.failed = (await api.listFiles(guid, { status: "failed" })).files;
    } catch {
        next.failed = UNAVAILABLE;
    }

    return next;
}
