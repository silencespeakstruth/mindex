import * as vscode from "vscode";
import { ConfigResponse, MindexApi } from "./api";
import { Availability, fetchStatus } from "./statusFetch";
import { StatusSnapshot, UNAVAILABLE } from "./shared/status";

export type { Availability } from "./statusFetch";

/** How fast to re-check while the server has files in flight. */
const BUSY_POLL_SECONDS = 3;

export { failedCount, UNAVAILABLE } from "./shared/status";
export type { ServerState, StatusSnapshot, Unavailable } from "./shared/status";

/**
 * Polls the server and tells everyone who needs to know.
 *
 * This used to be `StatusTreeProvider`, and the rename is the point: the fetching was
 * never the tree's job, and it is not the panel's either. Three consumers depend on a
 * refresh happening whether or not any status surface is open — the Ask form's
 * Research notice, its language pickers, and the budget ladder, model list and search
 * bounds its sliders are built from — so the callbacks are invoked by
 * [`fetchStatus`] directly and the monitor holds **no** reference to a panel.
 * `onDidChangeSnapshot` is how a panel subscribes; a missing subscriber changes
 * nothing about the fetch, which `statusFetch.test.ts` pins.
 *
 * The class itself is deliberately thin: an `EventEmitter`, a cached snapshot and a
 * call. Everything with a decision in it lives in `statusFetch.ts`, where it is
 * reachable by `node --test` without an extension host.
 */
export class StatusMonitor {
    private readonly changed = new vscode.EventEmitter<StatusSnapshot>();
    readonly onDidChangeSnapshot = this.changed.event;

    private snapshot?: StatusSnapshot;
    private timer?: ReturnType<typeof setTimeout>;
    /** The user's configured interval — the ceiling; see [`reschedule`]. */
    private baseSeconds = 0;

    constructor(
        private readonly api: () => MindexApi,
        private readonly guid: () => string | undefined,
        private readonly serverUrl: () => string,
        onAvailability: (availability: Availability) => void = () => {},
        onInventory: (languages: string[] | undefined) => void = () => {},
        /**
         * `GET /config` is read on every refresh rather than once at activation:
         * `research.models` is refreshed server-side, so a model pulled after the
         * window opened must appear without a reload. Reading it here and not at each
         * call site is the point — a future refresh site cannot forget it.
         */
        onServerConfig: (config: ConfigResponse) => void = () => {}
    ) {
        this.fan = { onAvailability, onInventory, onServerConfig };
    }

    private readonly fan: Parameters<typeof fetchStatus>[3];

    /** The last completed refresh, for a surface that opens between refreshes. */
    get latest(): StatusSnapshot | undefined {
        return this.snapshot;
    }

    /**
     * Re-check every `seconds`, or stop polling at `0`.
     *
     * A poll is what makes the Ask form's gate mean anything. Without one the only
     * refreshes are activation, an explicit command, a `.mindex` edit and the tail of
     * a reindex — so a dependency could die and the form would keep offering work
     * against it until the user happened to do something else. The events that
     * *already* refresh are unaffected; this only covers the idle window between them.
     *
     * Safe to call repeatedly: the previous timer is always cleared first, which is
     * what lets it double as the `onDidChangeConfiguration` handler.
     */
    setPollInterval(seconds: number): void {
        this.baseSeconds = seconds;
        this.reschedule();
    }

    /**
     * A self-rescheduling timeout rather than an interval, so the delay can depend on
     * what the last answer said.
     *
     * While the server holds indexing claims the Drift view is drawing a live count
     * from them, and a row that only moves every 30 s is a row that reads as stuck —
     * the very impression this whole mechanism exists to remove. Idle, the slow rate is
     * the right one: nothing is changing and each tick is six requests against a
     * connection pool of four. The rate is therefore a property of the answer, not a
     * setting; the user's interval stays the ceiling and `0` still means off.
     */
    private reschedule(): void {
        if (this.timer !== undefined) {
            clearTimeout(this.timer);
            this.timer = undefined;
        }
        if (this.baseSeconds <= 0) {
            return;
        }
        const runtime = this.snapshot?.runtime;
        const busy =
            runtime !== undefined && runtime !== UNAVAILABLE && runtime.indexing_claims > 0;
        const delay = busy ? Math.min(BUSY_POLL_SECONDS, this.baseSeconds) : this.baseSeconds;
        this.timer = setTimeout(() => {
            void this.refresh().finally(() => this.reschedule());
        }, delay * 1000);
    }

    dispose(): void {
        this.setPollInterval(0);
        this.changed.dispose();
    }

    /**
     * Fetch and publish. Never throws.
     *
     * Called from activation, the explicit refresh command, a `.mindex` edit, and
     * every reindex/delete/retry — which is why the Ask form's inventories are read
     * here and nowhere else.
     */
    async refresh(): Promise<void> {
        this.snapshot = await fetchStatus(this.api(), this.guid(), this.serverUrl(), this.fan);
        this.changed.fire(this.snapshot);
    }
}
