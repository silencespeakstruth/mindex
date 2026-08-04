import * as vscode from "vscode";
import { ConfigResponse, MindexApi } from "./api";
import { Availability, fetchStatus } from "./statusFetch";
import { StatusSnapshot, UNAVAILABLE } from "./shared/status";

export type { Availability } from "./statusFetch";

/** How fast to re-check while the server has files in flight. */
const BUSY_POLL_SECONDS = 3;

/**
 * The hard bound on one whole refresh — health, status, config, stats, failed.
 *
 * Every one of those calls is individually bounded now, so this is a backstop
 * rather than the mechanism. It is a backstop worth having because the failure it
 * catches is silent and permanent: `reschedule` re-arms in a `.finally()`, so a
 * refresh that never settles does not just miss a tick, it ends polling for the
 * life of the window and freezes the indicator at whatever colour it last had.
 */
const REFRESH_DEADLINE_MS = 20_000;

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

    /**
     * Whether a refresh is in flight, and a signal when that changes.
     *
     * The Status panel's refresh buttons echo this rather than only the press
     * that started it. Without that they read as broken during a *background*
     * poll: the panel is visibly re-rendering and the button that supposedly
     * causes it is idle and clickable, which invites the second press the whole
     * busy discipline exists to refuse.
     */
    private readonly refreshingChanged = new vscode.EventEmitter<boolean>();
    readonly onDidChangeRefreshing = this.refreshingChanged.event;
    private inFlight?: AbortController;
    private disposed = false;

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

    /** Whether a refresh is running right now. */
    get refreshing(): boolean {
        return this.inFlight !== undefined;
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
        this.disposed = true;
        this.setPollInterval(0);
        // An in-flight poll outliving the emitter it fires into is a leak with a
        // visible end: `changed.fire` on a disposed emitter throws out of a
        // promise nobody awaits.
        this.inFlight?.abort();
        this.changed.dispose();
        this.refreshingChanged.dispose();
    }

    /**
     * Fetch and publish. Never throws.
     *
     * Called from activation, the explicit refresh command, a `.mindex` edit, and
     * every reindex/delete/retry — which is why the Ask form's inventories are read
     * here and nowhere else.
     *
     * Single-flight, and the refusal is deliberate rather than a supersede: a
     * tick landing on a slow refresh would otherwise start a second chain of five
     * requests against a connection pool of four, which is how a slow refresh
     * becomes a stuck one.
     */
    async refresh(): Promise<void> {
        if (this.inFlight !== undefined || this.disposed) {
            return;
        }
        this.pending = this.runRefresh();
        await this.pending;
    }

    /**
     * A refresh whose result is guaranteed to be *newer than this call*.
     *
     * `refresh` refuses while one is in flight, which is right for a tick and wrong
     * for the callers that await it after a write — a retry, a delete, a reindex.
     * Those got an instant return carrying a snapshot taken before their own change,
     * so the panel could re-render still showing the file they had just requeued.
     * Shortening `mindex.statusPollSeconds` makes that collision the common case
     * rather than a rare one.
     *
     * It waits out the in-flight read rather than aborting it — that one has its own
     * awaiting caller — and then takes its turn. At most one extra pass per write,
     * which is why this is a separate method and not a change to `refresh`: the
     * chaining the single-flight exists to prevent is a *tick* colliding with a slow
     * read, and a tick still refuses.
     */
    async refreshNow(): Promise<void> {
        if (this.disposed) {
            return;
        }
        await this.pending;
        await this.refresh();
    }

    private pending?: Promise<void>;

    private async runRefresh(): Promise<void> {
        const controller = new AbortController();
        this.inFlight = controller;
        this.refreshingChanged.fire(true);
        const deadline = setTimeout(() => controller.abort(), REFRESH_DEADLINE_MS);
        try {
            const snapshot = await fetchStatus(
                this.api(),
                this.guid(),
                this.serverUrl(),
                this.fan,
                controller.signal
            );
            this.snapshot = snapshot;
            if (!this.disposed) {
                this.changed.fire(snapshot);
            }
        } finally {
            clearTimeout(deadline);
            this.inFlight = undefined;
            if (!this.disposed) {
                this.refreshingChanged.fire(false);
            }
        }
    }
}
