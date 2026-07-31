import * as vscode from "vscode";
import { BRAND } from "./brand";
import type {
    MindexApi,
    ResearchRunDetail,
    ResearchRunListResponse,
    ResearchRunSummary,
} from "./api";
import { asString, mediaRoots, readMedia, renderPage } from "./webview";
import { debounce, type Debounced } from "./shared/debounce";

/** What the panel asks the extension to do with the runs the user picked. */
export interface ResearchRunsActions {
    /**
     * Hand the selected runs to the Ask form as context for the next question. The
     * panel does not know the form exists — `extension.ts` wires the two, the same
     * way `StatusActions` keeps `StatusPanel` from reaching into the retry logic.
     */
    useAsContext(runs: ResearchRunSummary[]): void;
}

/** How long the box waits after the last keystroke before it asks the server. */
const SEARCH_DEBOUNCE_MS = 250;

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
    private readonly search: Debounced<[string, boolean]>;
    /** The last page's rows, so a selection can be resolved to summaries. */
    private rows = new Map<string, ResearchRunSummary>();
    private selected = new Set<string>();
    private query = "";
    private freshness: "all" | "fresh" | "stale" = "all";
    private validity: "all" | "valid" | "invalid" = "all";

    static showOrReveal(
        extensionUri: vscode.Uri,
        api: () => MindexApi,
        guid: () => string | undefined,
        actions: ResearchRunsActions
    ): void {
        if (ResearchRunsPanel.current !== undefined) {
            ResearchRunsPanel.current.panel.reveal(undefined, false);
            return;
        }
        ResearchRunsPanel.current = new ResearchRunsPanel(extensionUri, api, guid, actions);
    }

    private constructor(
        extensionUri: vscode.Uri,
        private readonly api: () => MindexApi,
        private readonly guid: () => string | undefined,
        actions: ResearchRunsActions
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
                        // Debounced: the first keystroke of an identifier is one
                        // letter, and its results would be wrong by the time they
                        // arrived.
                        this.search(this.query, true);
                        break;
                    case "more":
                        // Not debounced — a button press is already deliberate.
                        void this.load(this.query, this.lastSeq());
                        break;
                    case "select":
                        void this.preview(asString(msg.id));
                        break;
                    case "toggle":
                        this.toggle(asString(msg.id), msg.checked === true);
                        break;
                    case "pin":
                        void this.pin(asString(msg.id), msg.pinned === true);
                        break;
                    case "delete":
                        void this.remove(asString(msg.id));
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
        this.post({ type: "loading", loading: true });

        let page: ResearchRunListResponse;
        try {
            page = await this.api().listResearchRuns(
                guid,
                {
                    q: q || undefined,
                    beforeSeq,
                    freshness: this.freshness,
                    valid: this.validity === "all" ? undefined : this.validity === "valid",
                },
                controller.signal
            );
        } catch (e) {
            // `MindexApi.request` REJECTS on abort (unlike `research`, which
            // resolves), so a superseded keystroke lands here on every search. It is
            // not a failure and must not be reported as one.
            if (isAbort(e)) {
                return;
            }
            this.post({ type: "loading", loading: false });
            this.post({ type: "error", message: messageOf(e) });
            return;
        }
        if (controller.signal.aborted) {
            return;
        }
        this.inFlight = undefined;

        if (reset) {
            this.rows.clear();
        }
        for (const r of page.runs) {
            this.rows.set(r.id, r);
        }
        // A selection that is no longer offered is dropped, the way the pills widget
        // already does: keeping an invisible id would submit context the user cannot
        // see and cannot remove.
        for (const id of [...this.selected]) {
            if (!this.rows.has(id)) {
                this.selected.delete(id);
            }
        }
        this.post({ type: "loading", loading: false });
        this.post({
            type: "runs",
            runs: page.runs,
            nextBeforeSeq: page.next_before_seq,
            reset,
            selected: [...this.selected],
        });
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
            if (!isAbort(e)) {
                this.post({ type: "error", message: messageOf(e) });
            }
            return;
        }
        this.post({ type: "preview", run: detail });
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
            this.post({ type: "error", message: messageOf(e) });
        }
    }

    private async remove(id: string): Promise<void> {
        const guid = this.guid();
        if (guid === undefined) {
            return;
        }
        const row = this.rows.get(id);
        const label = row === undefined ? "this run" : `#${row.seq} — ${row.title}`;
        const yes = await vscode.window.showWarningMessage(
            `Delete ${label}? The report cannot be recovered.`,
            { modal: true },
            "Delete"
        );
        if (yes !== "Delete") {
            return;
        }
        try {
            await this.api().deleteResearchRun(guid, id);
        } catch (e) {
            this.post({ type: "error", message: messageOf(e) });
            return;
        }
        this.rows.delete(id);
        this.selected.delete(id);
        this.post({ type: "removed", id, selected: [...this.selected] });
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

function isAbort(e: unknown): boolean {
    return e instanceof Error && e.name === "AbortError";
}

function messageOf(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
}
