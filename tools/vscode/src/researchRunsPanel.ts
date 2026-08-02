import * as vscode from "vscode";
import { BRAND } from "./brand";
import type {
    ConfigResponse,
    MindexApi,
    ResearchCorpusTotals,
    ResearchRunDetail,
    ResearchRunListResponse,
    ResearchRunSummary,
} from "./api";
import { asString, mediaRoots, readMedia, renderPage } from "./webview";
import { BusyKeys } from "./busy";
import { humanize, logError, ProblemError } from "./errors";
import { debounce, type Debounced } from "./shared/debounce";
import {
    asVerdict,
    bulkSelectionNote,
    challengeGuard,
    gcRowReasons,
    recheckOptions,
    standingChallenge,
    type ChallengeState,
    type ChallengeStatePresent,
} from "./shared/runsFormat";

/** What the panel asks the extension to do with the runs the user picked. */
export interface ResearchRunsActions {
    /**
     * Hand the selected runs to the Ask form as context for the next question. The
     * panel does not know the form exists — `extension.ts` wires the two, the same
     * way `StatusActions` keeps `StatusPanel` from reaching into the retry logic.
     */
    useAsContext(runs: ResearchRunSummary[]): void;
    /** Open one stored report as its own Markdown tab. */
    openReport(run: ResearchRunSummary): void;
    /**
     * Put a stored run's question back in the Ask form — same scope, model and
     * effort — with the run itself attached as context. Following a report up is
     * the common next move, and retyping the question is how it gets skipped.
     */
    reAsk(run: ResearchRunDetail): void;
    /**
     * Launch a challenge against this run's report. The panel only pre-checks
     * (webview-side, via `challengeGuard`) and forwards; the QuickPick chain,
     * the stream and the error mapping live with `startResearch` in
     * `extension.ts`, which owns the single-flight handles.
     */
    challenge(run: ResearchRunSummary): void;
}

/** How long the box waits after the last keystroke before it asks the server. */
const SEARCH_DEBOUNCE_MS = 250;

/** What the server's own defaults are, when `/config` has not been read yet. */
const FALLBACK_PAGE_LIMIT = 50;
const FALLBACK_MAX_DELETE = 500;

/**
 * A hard stop on any paging loop, over and above the id cap.
 *
 * The loops below follow `next_before_seq` until the server stops offering one.
 * That is the server's contract and it holds — but a bug on either side (a cursor
 * that fails to advance, a page that reports itself full when it is not) would
 * turn "follow the cursor" into an unbounded request storm against a local
 * service, from a UI thread, with no way for the user to stop it. The cap is not
 * expected to bind; it is there so a wrong answer is a wrong answer and not a
 * hang.
 */
const MAX_PAGES = 64;

/**
 * Research History: a two-pane reader over the runs this project has stored — a
 * searchable list on the left, the selected report rendered on the right, and a
 * multi-select that feeds the Ask form.
 *
 * **An editor tab, not a third sidebar view.** Same argument `icons.test.ts` already
 * records for moving Server Status out of the sidebar: a permanent third of the
 * sidebar is the wrong price for something consulted deliberately, and a
 * Markdown report needs width the sidebar has never had.
 *
 * A **singleton** — two tabs of the same corpus is clutter — and `retainContextWhenHidden`
 * unlike `StatusPanel`, because this one holds user input: a half-typed query and a
 * selection that took several clicks to build.
 *
 * The panel owns exactly one `AbortController`. Every keystroke aborts the request
 * before it, which is the only thing that keeps a slow query from painting its
 * results over a newer one.
 */
export class ResearchRunsPanel {
    private static current?: ResearchRunsPanel;

    private readonly panel: vscode.WebviewPanel;
    private readonly disposables: vscode.Disposable[] = [];
    /** Supersedes the in-flight list request. */
    private inFlight?: AbortController;
    /**
     * Single-flight per button, and the wire that greys them.
     *
     * Reads that a keystroke supersedes (`list`) keep their `AbortController` and
     * take a key only so the button can echo them; everything else — paging and
     * every write — is *refused* while it is in flight. See `src/busy.ts`.
     */
    private readonly busy = new BusyKeys((m) => this.post(m));
    private readonly search: Debounced<[string, boolean]>;
    /** The rows currently RENDERED, so the list can be reconciled in place. */
    private rows = new Map<string, ResearchRunSummary>();
    /**
     * Every summary this panel has ever seen, including ones fetched by a
     * select-all or a garbage-collection pass and never rendered.
     *
     * Separate from `rows` because the delete confirmation reads it: a bulk
     * selection is mostly off-screen, and resolving it through `rows` would report
     * `0` dependants for every row the user never scrolled to — quietly stating a
     * smaller number than the truth in a delete dialog, which is the one way
     * `remove()` must never be wrong.
     */
    private summaries = new Map<string, ResearchRunSummary>();
    private selected = new Set<string>();
    /** Whether the current selection was built by a filter rather than by clicking. */
    private bulkSelection = false;
    private query = "";
    private freshness: "all" | "fresh" | "stale" = "all";
    private validity: "all" | "valid" | "invalid" = "all";
    private kind: "all" | "research" | "challenge" = "all";
    private completeness: "all" | "finalized" | "partial" = "all";
    private totals?: ResearchCorpusTotals;
    /** The challenge state of the previewed run, for the Re-check fork. */
    private challengeState?: { runId: string; state: ChallengeState };
    /**
     * A server too old for the query parameters added here.
     *
     * `ResearchListQuery` is `deny_unknown_fields`, so sending `completeness` or
     * `challenged_run_id` to a 1.0.1 server is a 400 on *every* request and the
     * whole panel dies rather than degrading. One 400 flips this and the panel
     * drops back to what that server understands.
     */
    private legacyServer = false;

    static showOrReveal(
        extensionUri: vscode.Uri,
        api: () => MindexApi,
        guid: () => string | undefined,
        actions: ResearchRunsActions,
        config: () => ConfigResponse | undefined
    ): void {
        if (ResearchRunsPanel.current !== undefined) {
            ResearchRunsPanel.current.panel.reveal(undefined, false);
            return;
        }
        ResearchRunsPanel.current = new ResearchRunsPanel(
            extensionUri,
            api,
            guid,
            actions,
            config
        );
    }

    private constructor(
        extensionUri: vscode.Uri,
        private readonly api: () => MindexApi,
        private readonly guid: () => string | undefined,
        actions: ResearchRunsActions,
        private readonly config: () => ConfigResponse | undefined
    ) {
        this.panel = vscode.window.createWebviewPanel(
            "mindexResearchRuns",
            `${BRAND} — Research History`,
            vscode.ViewColumn.Active,
            {
                enableScripts: true,
                // Unlike StatusPanel: this panel holds a half-typed query and a
                // multi-click selection, and rebuilding from the default HTML would
                // discard both every time the tab lost focus.
                retainContextWhenHidden: true,
                localResourceRoots: mediaRoots(extensionUri),
            }
        );
        this.panel.webview.html = renderPage(this.panel.webview, extensionUri, {
            body: readMedia(extensionUri, "runs.html"),
            styles: ["common.css", "runs.css"],
            modules: ["js/runs.js"],
            codicons: true,
        });

        this.search = debounce(SEARCH_DEBOUNCE_MS, (q: string, reset: boolean) => {
            void this.load(q, reset ? undefined : this.lastSeq());
        });

        this.disposables.push(
            this.panel.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
                switch (msg.type) {
                    case "ready":
                        void this.load("", undefined);
                        break;
                    case "search":
                        this.query = asString(msg.q);
                        this.freshness = readFreshness(msg.freshness);
                        this.validity = readValidity(msg.validity);
                        this.kind = readKind(msg.kind);
                        this.completeness = readCompleteness(msg.completeness);
                        this.dropBulkSelection();
                        // Debounced: the first keystroke of an identifier is one
                        // letter, and its results would be wrong by the time they
                        // arrived.
                        this.search(this.query, true);
                        break;
                    case "preset":
                        this.applyPreset(asString(msg.preset));
                        break;
                    case "selectAllMatching":
                        void this.busy.run("list", () => this.selectAllMatching());
                        break;
                    case "gcPropose":
                        void this.busy.run("gc", () => this.proposeGarbage());
                        break;
                    // One key for all three destructive paths: they are the same
                    // act, and "two confirmation modals for one row" is what
                    // separate keys would still allow.
                    case "gcDelete":
                        void this.busy.run("delete", () => this.remove(readIds(msg.ids)));
                        break;
                    case "recheck":
                        void this.busy.run("preview", () =>
                            this.recheck(asString(msg.id), actions)
                        );
                        break;
                    case "more":
                        // Refused, never superseded. A second press used to abort
                        // the page the first had asked for, and `load` returned
                        // early without advancing the cursor or clearing the
                        // spinner — so holding the key made paging stop dead.
                        void this.busy.run("more", () =>
                            this.load(this.query, this.lastSeq())
                        );
                        break;
                    case "refresh": {
                        // Not debounced either, and it supersedes a pending
                        // keystroke: the user asked for the current query, now.
                        this.search.cancel();
                        this.dropBulkSelection();
                        void this.load(this.query, undefined);
                        // The open report is re-fetched too — its staleness and
                        // trust are exactly the numbers that move under the panel.
                        const active = asString(msg.activeId);
                        if (active !== "") {
                            void this.preview(active);
                        }
                        break;
                    }
                    case "verify":
                        void this.busy.run("verify", () => this.verify(asString(msg.id)));
                        break;
                    case "challenge": {
                        const run = this.summaries.get(asString(msg.id));
                        if (run !== undefined) {
                            void this.busy.run("preview", () =>
                                this.confirmAndChallenge(run, actions)
                            );
                        }
                        break;
                    }
                    case "select":
                        void this.busy.run("preview", () => this.preview(asString(msg.id)));
                        break;
                    case "toggle":
                        this.toggle(asString(msg.id), msg.checked === true);
                        break;
                    case "pin": {
                        // Per row, not one `pin` key: pinning one report must not
                        // freeze the pin button on every other row on screen.
                        const id = asString(msg.id);
                        void this.busy.run(`row:${id}`, () =>
                            this.pin(id, msg.pinned === true)
                        );
                        break;
                    }
                    case "delete":
                        void this.busy.run("delete", () => this.remove([asString(msg.id)]));
                        break;
                    case "deleteSelected":
                        void this.busy.run("delete", () => this.remove([...this.selected]));
                        break;
                    case "openRun": {
                        const run = this.rows.get(asString(msg.id));
                        if (run !== undefined) {
                            actions.openReport(run);
                        }
                        break;
                    }
                    case "reAsk":
                        void this.busy.run("preview", () =>
                            this.reAsk(asString(msg.id), actions)
                        );
                        break;
                    case "useAsContext":
                        actions.useAsContext(
                            [...this.selected]
                                .map((id) => this.rows.get(id))
                                .filter((r): r is ResearchRunSummary => r !== undefined)
                        );
                        break;
                    case "openFile":
                        void vscode.commands.executeCommand(
                            "vscode.open",
                            vscode.Uri.joinPath(
                                vscode.workspace.workspaceFolders?.[0]?.uri ??
                                    vscode.Uri.file("/"),
                                asString(msg.path)
                            )
                        );
                        break;
                }
            })
        );

        this.panel.onDidDispose(() => {
            ResearchRunsPanel.current = undefined;
            // Order matters: drop the pending timer before the panel is gone, or it
            // fires into a disposed webview and surfaces as an error the user can do
            // nothing about.
            this.search.cancel();
            this.inFlight?.abort();
            this.busy.reset();
            for (const d of this.disposables) {
                d.dispose();
            }
        });
    }

    /** The cursor for the next page: the oldest row currently shown. */
    private lastSeq(): number | undefined {
        let min: number | undefined;
        for (const r of this.rows.values()) {
            if (min === undefined || r.seq < min) {
                min = r.seq;
            }
        }
        return min;
    }

    /** The server's own page ceiling, and the batch-delete cap, from `/config`. */
    private pageLimit(): number {
        return this.config()?.research?.list_page_limit ?? FALLBACK_PAGE_LIMIT;
    }

    private maxDelete(): number {
        return this.config()?.research?.max_delete_ids ?? FALLBACK_MAX_DELETE;
    }

    /** The filters as the API wants them — the one place they are translated. */
    private filterQuery(): {
        q?: string;
        freshness: "all" | "fresh" | "stale";
        valid?: boolean;
        kind?: "research" | "challenge";
        completeness?: "all" | "finalized" | "partial";
    } {
        return {
            q: this.query || undefined,
            freshness: this.freshness,
            valid: this.validity === "all" ? undefined : this.validity === "valid",
            kind: this.kind === "all" ? undefined : this.kind,
            // Withheld from a server that would 400 on it; the panel then filters
            // nothing by completeness rather than showing nothing at all.
            completeness: this.legacyServer ? undefined : this.completeness,
        };
    }

    /**
     * A bulk selection is defined by the filters that built it, so a filter change
     * invalidates it wholesale rather than pruning it row by row.
     *
     * Keeping it would leave the user holding several hundred ids they can no
     * longer see, chosen by a query that is no longer on screen — and the delete
     * button would still offer to act on them.
     */
    private dropBulkSelection(): void {
        if (!this.bulkSelection) {
            return;
        }
        this.bulkSelection = false;
        this.selected.clear();
        this.post({ type: "selected", selected: [] });
    }

    /**
     * `true` if this error is the version skew — a 400 from a server whose
     * `deny_unknown_fields` query struct has never heard of these parameters.
     *
     * Matched on the shape of the failure rather than on a code, because the
     * extractor's answer to an unknown query key is a malformed-query 400 whose
     * detail is axum's, not a code this client can pin.
     */
    private noteIfLegacy(e: unknown): boolean {
        if (this.legacyServer) {
            return false;
        }
        // A shape test against fields that will not move, rather than against a
        // rendered message: the message is now humanized prose, and matching
        // English for a version check would break the day the wording improves.
        if (
            e instanceof ProblemError &&
            e.status === 400 &&
            /unknown field|malformed/i.test(e.detail)
        ) {
            this.legacyServer = true;
            return true;
        }
        return false;
    }

    /** Hand the list spinner and key back, but only if this request still owns them. */
    private releaseList(controller: AbortController): void {
        if (this.inFlight === controller) {
            this.inFlight = undefined;
            this.post({ type: "busy", key: "list", busy: false });
        }
    }

    /**
     * The panel's one error surface.
     *
     * Eight catch sites used to post `e.message` straight into the banner, which
     * for a `ProblemError` is `code (status): detail` — so users read
     * `research.not_found (404)` and `connect ECONNREFUSED 127.0.0.1:11111`. The
     * sentence goes to the banner and the raw error to the output channel, and
     * `where` is what makes the log line worth having.
     */
    private fail(where: string, e: unknown): void {
        if (isAbort(e)) {
            return;
        }
        const humanized = humanize(e);
        if (humanized.cancelled) {
            return;
        }
        logError(`Research History: ${where}`, e);
        this.post({
            type: "error",
            message: humanized.text,
            retryable: humanized.retryable,
        });
    }

    private async load(q: string, beforeSeq: number | undefined): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            this.post({ type: "runs", runs: [], nextBeforeSeq: null, reset: true });
            return;
        }
        this.inFlight?.abort();
        const controller = new AbortController();
        this.inFlight = controller;
        const reset = beforeSeq === undefined;
        this.post({ type: "busy", key: "list", busy: true });

        let page: ResearchRunListResponse;
        try {
            page = await this.api().listResearchRuns(
                guid,
                { ...this.filterQuery(), beforeSeq },
                controller.signal
            );
        } catch (e) {
            // `MindexApi.request` REJECTS on abort (unlike `research`, which
            // resolves), so a superseded keystroke lands here on every search. It is
            // not a failure and must not be reported as one.
            if (isAbort(e)) {
                return;
            }
            // One retry, once, with the parameters an older server cannot parse
            // stripped. Without it a single unknown key takes the whole panel down.
            if (this.noteIfLegacy(e)) {
                this.releaseList(controller);
                await this.load(q, beforeSeq);
                return;
            }
            this.fail("loading the list", e);
            return;
        } finally {
            // Only the current owner clears the spinner and the key. A superseded
            // request turning them off while its successor is still running is how
            // the panel came to look idle mid-fetch — and clearing `inFlight` from a
            // stale controller would leave the next keystroke aborting nothing.
            this.releaseList(controller);
        }
        if (controller.signal.aborted) {
            return;
        }

        if (reset) {
            this.rows.clear();
        }
        for (const r of page.runs) {
            this.rows.set(r.id, r);
            this.summaries.set(r.id, r);
        }
        // A hand-built selection that is no longer offered is dropped, the way the
        // pills widget already does: keeping an invisible id would submit context
        // the user cannot see and cannot remove. A BULK selection is exempt — it is
        // deliberately larger than the page, and `dropBulkSelection` (on any filter
        // change) is what bounds it instead.
        if (!this.bulkSelection) {
            for (const id of [...this.selected]) {
                if (!this.rows.has(id)) {
                    this.selected.delete(id);
                }
            }
        }
        this.totals = page.totals;
        this.post({ type: "totals", totals: page.totals ?? null, legacy: this.legacyServer });
        this.post({
            type: "runs",
            runs: page.runs,
            nextBeforeSeq: page.next_before_seq,
            reset,
            selected: [...this.selected],
            bulk: this.bulkSelection,
        });
    }

    /**
     * Every run matching the current filters, by following the keyset cursor.
     *
     * Bounded three ways, and each one is announced rather than silent: the
     * server's own batch-delete cap (`stopped` below), [`MAX_PAGES`], and
     * exhaustion. Callers get `truncated` so the UI can say it stopped short —
     * a screen that quietly shows a sample of a corpus reads as the whole of it.
     */
    private async pageAll(
        guid: string,
        query: Parameters<MindexApi["listResearchRuns"]>[1],
        cap: number,
        signal: AbortSignal
    ): Promise<{ rows: ResearchRunSummary[]; truncated: boolean }> {
        const rows: ResearchRunSummary[] = [];
        let beforeSeq: number | undefined;
        for (let page = 0; page < MAX_PAGES; page += 1) {
            const res = await this.api().listResearchRuns(
                guid,
                { ...query, beforeSeq, limit: this.pageLimit() },
                signal
            );
            for (const r of res.runs) {
                this.summaries.set(r.id, r);
                rows.push(r);
            }
            this.totals = res.totals ?? this.totals;
            if (rows.length >= cap) {
                return { rows: rows.slice(0, cap), truncated: true };
            }
            if (res.next_before_seq === null) {
                return { rows, truncated: false };
            }
            beforeSeq = res.next_before_seq;
        }
        return { rows, truncated: true };
    }

    /**
     * Select every run matching the filters, not just the loaded page.
     *
     * Capped at the server's batch-delete limit, because the *point* of the
     * selection is to be deletable: selecting more than one call accepts would
     * hand the user a number the delete then silently cannot honour.
     */
    private async selectAllMatching(): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            return;
        }
        this.inFlight?.abort();
        const controller = new AbortController();
        this.inFlight = controller;
        this.post({ type: "busy", key: "list", busy: true });
        try {
            const { rows, truncated } = await this.pageAll(
                guid,
                this.filterQuery(),
                this.maxDelete(),
                controller.signal
            );
            if (controller.signal.aborted) {
                return;
            }
            this.selected = new Set(rows.map((r) => r.id));
            this.bulkSelection = true;
            this.post({
                type: "selected",
                selected: [...this.selected],
                bulk: true,
                truncated,
                cap: this.maxDelete(),
            });
        } catch (e) {
            this.fail("selecting everything that matches", e);
        } finally {
            this.inFlight = undefined;
            this.post({ type: "busy", key: "list", busy: false });
        }
    }

    /** One-click filter shortcuts. The selects stay the general mechanism. */
    private applyPreset(preset: string): void {
        this.dropBulkSelection();
        this.freshness = "all";
        this.validity = "all";
        this.completeness = "all";
        this.kind = "all";
        if (preset === "outdated") {
            this.freshness = "stale";
        } else if (preset === "partial") {
            this.completeness = "partial";
        }
        // Echoed so the four selects move with the preset: a shortcut that left the
        // controls disagreeing with the list would be a second, invisible filter.
        this.post({
            type: "filters",
            freshness: this.freshness,
            validity: this.validity,
            kind: this.kind,
            completeness: this.completeness,
        });
        this.search.cancel();
        void this.load(this.query, undefined);
    }

    /**
     * Build the garbage-collection proposal: every **unpinned** run with something
     * wrong with it, classified into the four buckets the review then groups by.
     *
     * The pinned exemption is the *server's* `pinned: false`, not a client-side
     * test, so it cannot leak. The classification is client-side and that is safe
     * here specifically because this pass runs to exhaustion — no inference is
     * being drawn from a page's length, which is the thing a client-side filter
     * would break.
     */
    private async proposeGarbage(): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            return;
        }
        this.inFlight?.abort();
        const controller = new AbortController();
        this.inFlight = controller;
        this.post({ type: "busy", key: "list", busy: true });
        try {
            const { rows } = await this.pageAll(
                guid,
                { pinned: false },
                this.maxDelete(),
                controller.signal
            );
            if (controller.signal.aborted) {
                return;
            }
            const proposed = rows
                .map((run) => ({ run, buckets: gcRowReasons({ ...run, pinned: run.pinned }) }))
                .filter((r) => r.buckets.length > 0);
            this.post({
                type: "gc",
                rows: proposed.map(({ run, buckets }) => ({
                    id: run.id,
                    seq: run.seq,
                    title: run.title,
                    referenced_by_count: run.referenced_by_count,
                    buckets,
                })),
                expected: this.totals?.gc_candidates ?? null,
            });
        } catch (e) {
            this.fail("proposing garbage", e);
        } finally {
            this.inFlight = undefined;
            this.post({ type: "busy", key: "list", busy: false });
        }
    }

    private async preview(id: string): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            return;
        }
        let detail: ResearchRunDetail;
        try {
            detail = await this.api().getResearchRun(guid, id);
        } catch (e) {
            // `fail` drops aborts itself — one place, not eight.
            this.fail("opening the report", e);
            return;
        }
        this.summaries.set(detail.id, detail);
        this.challengeState = undefined;
        this.post({ type: "preview", run: detail });
        void this.loadChallengeState(guid, detail);
    }

    /**
     * What has been said about the previewed run — one indexed request against
     * `challenged_run_id`, and it fires for **every** research run.
     *
     * The old version asked the server for a page of *all* challenges and picked
     * out the matching ones itself, and skipped the request entirely when trust
     * read `unchallenged`. Both halves were bugs, and they compounded: trust is
     * correctly silent about an inconclusive challenge and about one whose own
     * evidence has moved, so a report that had been challenged and refuted could
     * show nothing at all about it; and a challenge past the first unfiltered page
     * was simply never found.
     *
     * Best-effort: a failure costs the line, never the preview.
     */
    private async loadChallengeState(guid: string, run: ResearchRunDetail): Promise<void> {
        if (run.kind !== "research") {
            return;
        }
        let state: ChallengeState = { state: "none" };
        try {
            const page = await this.api().listResearchRuns(guid, {
                kind: "challenge",
                challengedRunId: run.id,
                // Two, not one: the replace rule is gated on a verdict, so an
                // inconclusive re-check leaves the standing verdict in place and a
                // subject can legitimately carry two rows. Asking for one would
                // silently pick between them; asking for two lets the line say so.
                limit: 2,
            });
            // Newest first (`ORDER BY seq DESC`), so the first row is the standing one.
            const found = page.runs.map(toChallengeState);
            const latest = found[0];
            if (latest !== undefined) {
                state =
                    found.length > 1
                        ? { state: "several", count: found.length, latest }
                        : latest;
            }
        } catch (e) {
            if (this.noteIfLegacy(e)) {
                // An older server cannot answer this query at all. Say nothing
                // rather than guessing — the trust badge still carries the verdict.
                return;
            }
            return;
        }
        this.challengeState = { runId: run.id, state };
        this.post({ type: "challengeState", runId: run.id, state });
    }

    /**
     * Re-check the standing challenge: offline, or by spending a research slot.
     *
     * Both are offered and neither runs automatically. They are not variants of
     * one action — the first is free and changes nothing, the second can delete
     * the verdict currently on the record — so choosing between them is the user's.
     */
    private async recheck(id: string, actions: ResearchRunsActions): Promise<void> {
        const run = this.summaries.get(id);
        const state =
            this.challengeState?.runId === id ? this.challengeState.state : undefined;
        const standing = state === undefined ? undefined : standingChallenge(state);
        if (run === undefined || standing === undefined) {
            return;
        }
        const opts = recheckOptions(standing);
        const picked = await vscode.window.showQuickPick(
            [
                { label: opts.links.label, detail: opts.links.detail, mode: "links" as const },
                { label: opts.fresh.label, detail: opts.fresh.detail, mode: "fresh" as const },
            ],
            {
                title: `Re-check challenge #${standing.seq}`,
                placeHolder: "Offline, or a fresh run on the GPU",
                matchOnDetail: true,
            }
        );
        if (picked === undefined) {
            return;
        }
        if (picked.mode === "links") {
            // The CHALLENGE run's own citations, not the subject's — which is why
            // the rendered result says so. Reading one as the other would be a
            // worse confusion than the one this whole surface exists to fix.
            await this.verify(standing.id, { subjectId: id, of: "challenge" });
            return;
        }
        await this.confirmAndChallenge(run, actions);
    }

    /**
     * The one place a fresh challenge is launched from this panel — first or
     * repeat — so the replacement warning cannot be skipped by taking a different
     * button.
     */
    private async confirmAndChallenge(
        run: ResearchRunSummary,
        actions: ResearchRunsActions
    ): Promise<void> {
        const state =
            this.challengeState?.runId === run.id ? this.challengeState.state : undefined;
        const standing = state === undefined ? undefined : standingChallenge(state);
        if (standing !== undefined) {
            const guard = challengeGuard(run, state);
            if (guard.ok && guard.mode === "recheck") {
                const yes = await vscode.window.showWarningMessage(
                    guard.replaceWarning,
                    { modal: true },
                    "Run it"
                );
                if (yes !== "Run it") {
                    return;
                }
            }
        }
        actions.challenge(run);
    }

    /**
     * Offline re-verification, rendered under the Verify button.
     *
     * `into` exists because the Re-check fork verifies the **challenge** run while
     * the preview showing the result belongs to its subject. The webview keys the
     * render on the run it is displaying (`runId`), and `of` is what makes the
     * caption say whose citations were checked — a challenge's provenance read as
     * the subject's would be a worse confusion than the one being fixed.
     */
    private async verify(
        id: string,
        into?: { subjectId: string; of: "challenge" }
    ): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            return;
        }
        try {
            const v = await this.api().getResearchVerification(guid, id);
            this.post({
                type: "verification",
                runId: into?.subjectId ?? id,
                of: into?.of ?? "self",
                verifiedRunId: id,
                v,
            });
        } catch (e) {
            this.fail("re-verifying the citations", e);
        }
    }

    private toggle(id: string, checked: boolean): void {
        if (checked) {
            this.selected.add(id);
        } else {
            this.selected.delete(id);
        }
        this.post({ type: "selected", selected: [...this.selected] });
    }

    private async pin(id: string, pinned: boolean): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            return;
        }
        try {
            const updated = await this.api().pinResearchRun(guid, id, pinned);
            this.rows.set(updated.id, updated);
            // Re-post the server's answer rather than the state we guessed: pinning
            // rewrites `expires_at`, and unpinning an old run can make it eligible at
            // the very next sweep.
            this.post({ type: "updated", run: updated });
        } catch (e) {
            this.fail("pinning the report", e);
        }
    }

    /**
     * Delete one run or a batch, through the one path.
     *
     * Batched server-side rather than looped here: a corpus is pruned in handfuls,
     * and N requests is N chances to fail halfway and leave the user guessing which
     * half went. The confirmation names the **dependants**, because deleting a run
     * silently invalidates every later report built on it — the caller is owed that
     * number before they agree, and `referenced_by_count` is why it is on the wire.
     */
    private async remove(ids: string[]): Promise<void> {
        const guid = this.guid();
        if (guid === undefined || ids.length === 0) {
            return;
        }
        // Through `summaries`, not `rows`: a bulk selection is mostly off-screen,
        // and resolving it through the rendered page would report `0` dependants
        // for every row the user never scrolled to.
        const picked = ids
            .map((id) => this.summaries.get(id))
            .filter((r): r is ResearchRunSummary => r !== undefined);
        const label =
            ids.length === 1
                ? picked[0] === undefined
                    ? "this run"
                    : `#${picked[0].seq} — ${picked[0].title}`
                : `${ids.length} reports`;
        const onScreen = ids.filter((id) => this.rows.has(id)).length;
        const bulkNote = bulkSelectionNote(ids.length, onScreen);
        // The count is stated, not netted against the selection: a summary carries
        // its *ancestors* (`context`), never its dependants, so which of them are
        // also being deleted is not knowable here — and quietly reporting a smaller
        // number than the truth is the wrong way to be wrong in a delete dialog.
        const dependants = picked.reduce((n, r) => n + r.referenced_by_count, 0);
        const warning =
            dependants > 0
                ? `\n\n${dependants} later report(s) were built on ` +
                  `${ids.length === 1 ? "it" : "these"} and will become invalid.`
                : "";
        const yes = await vscode.window.showWarningMessage(
            `Delete ${label}? The report${ids.length === 1 ? "" : "s"} cannot be recovered.` +
                `${bulkNote === undefined ? "" : `\n\n${bulkNote}`}${warning}`,
            { modal: true },
            "Delete"
        );
        if (yes !== "Delete") {
            return;
        }
        try {
            if (ids.length === 1) {
                await this.api().deleteResearchRun(guid, ids[0]);
            } else {
                await this.api().deleteResearchRuns(guid, ids);
            }
        } catch (e) {
            this.fail("deleting reports", e);
            return;
        }
        for (const id of ids) {
            this.rows.delete(id);
            this.summaries.delete(id);
            this.selected.delete(id);
        }
        this.bulkSelection = false;
        this.post({ type: "removed", ids, selected: [...this.selected] });
        // The corpus just shrank, so the counts line and the GC button's number
        // are both stale. Re-reading the current page is what refreshes them.
        void this.load(this.query, undefined);
    }

    /** Send a stored run's question and settings back to the Ask form. */
    private async reAsk(id: string, actions: ResearchRunsActions): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            return;
        }
        try {
            actions.reAsk(await this.api().getResearchRun(guid, id));
        } catch (e) {
            this.fail("re-asking the question", e);
        }
    }

    private post(message: unknown): void {
        void this.panel.webview.postMessage(message);
    }
}

function readFreshness(v: unknown): "all" | "fresh" | "stale" {
    return v === "fresh" || v === "stale" ? v : "all";
}

function readValidity(v: unknown): "all" | "valid" | "invalid" {
    return v === "valid" || v === "invalid" ? v : "all";
}

function readKind(v: unknown): "all" | "research" | "challenge" {
    return v === "research" || v === "challenge" ? v : "all";
}

function readCompleteness(v: unknown): "all" | "finalized" | "partial" {
    return v === "finalized" || v === "partial" ? v : "all";
}

function readIds(v: unknown): string[] {
    return Array.isArray(v) ? v.map(String) : [];
}

/** One challenge summary as the state the wording module reads. */
function toChallengeState(r: ResearchRunSummary): ChallengeStatePresent {
    return {
        state: "present",
        id: r.id,
        seq: r.seq,
        title: r.title,
        // Narrowed here rather than carried as a bare string: the wording depends
        // on the distinction between a verdict and the absence of one, and an
        // unknown future value must read as "no verdict", not as a fourth one.
        verdict: asVerdict(r.challenge_verdict) ?? null,
        valid: r.valid,
    };
}

function isAbort(e: unknown): boolean {
    return e instanceof Error && e.name === "AbortError";
}
