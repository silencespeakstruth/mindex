import * as vscode from "vscode";
import { BRAND } from "./brand";
import { asString, mediaRoots, readMedia, renderPage } from "./webview";
import type { IndexRunSnapshot } from "./shared/indexRun";

/** What the panel's controls ask the run to do. */
export interface IndexingActions {
    /** The user pressed Cancel. Same path as the toast's — an abort of the upload. */
    cancel(): void;
    openFile(path: string): void;
}

/** Where the panel opens when a reindex starts. `manual` means "never on its own". */
export type IndexingPanelPlacement = "beside" | "active" | "manual";

/**
 * The live indexing panel.
 *
 * It exists because a reindex was, in practice, invisible. The two surfaces it
 * joins are structurally one line each — a `withProgress` notification message is
 * single-line (VS Code renders it `white-space: normal`, so a `\n` collapses) and
 * a status-bar item is a strip in a corner that a crowded bar truncates. Neither
 * can hold what the server actually streams: per-file paths and languages, chunk
 * and symbol counts, an embed rate, the skip reasons, the batch position. That is
 * what this renders, and it is why it opens by itself — a progress surface the
 * user has to know about and go and find does not answer "is anything happening".
 *
 * A **singleton**, like [`StatusPanel`]: two tabs of one run is clutter, not a
 * comparison. It holds no user input, so `retainContextWhenHidden` stays off and
 * every path back to visibility re-posts the whole run — the host owns the state,
 * so a torn-down page loses nothing.
 */
export class IndexingPanel {
    private static instance?: IndexingPanel;
    /**
     * The run in flight, if any.
     *
     * Static rather than passed in, because a panel can be opened *after* the run
     * started — from the command palette, or under the `manual` placement where
     * nothing opens on its own — and a Cancel button that does nothing because the
     * panel missed the handshake is worse than no button.
     */
    private static runActions?: IndexingActions;
    /**
     * Opening a file outlives the run that mentioned it: the summary and the feed
     * stay on screen afterwards, and a path that stops being clickable the moment
     * the run ends is a dead link on the one surface the user reads at leisure.
     */
    private static fileOpener?: (path: string) => void;

    private readonly panel: vscode.WebviewPanel;
    private readonly disposables: vscode.Disposable[] = [];
    private snapshot?: IndexRunSnapshot;

    /** The open panel, if any. Consulted through this rather than captured: a panel
     *  closed mid-run must leave the run itself untouched. */
    static get current(): IndexingPanel | undefined {
        return IndexingPanel.instance;
    }

    /**
     * Open (or reveal) the panel and reset it for a run that is about to start.
     *
     * `initial` is the run's zero snapshot, and passing it is what makes the metrics
     * block exist from the first frame. Opening with no snapshot at all put the page
     * into its "nothing has run in this window" state for the whole read-and-post
     * stretch — so the panel a reindex had just opened by itself said that no reindex
     * had happened, which is the exact silence it exists to end.
     */
    static beginRun(
        extensionUri: vscode.Uri,
        actions: IndexingActions,
        placement: IndexingPanelPlacement,
        initial: IndexRunSnapshot
    ): IndexingPanel | undefined {
        IndexingPanel.runActions = actions;
        IndexingPanel.fileOpener = (p) => actions.openFile(p);
        if (IndexingPanel.instance === undefined) {
            if (placement === "manual") {
                return undefined;
            }
            IndexingPanel.instance = new IndexingPanel(extensionUri, placement);
        }
        const panel = IndexingPanel.instance;
        panel.snapshot = initial;
        // `preserveFocus` on a reveal too: the run did not ask for the caret.
        panel.panel.reveal(undefined, true);
        // A `run` message is what clears the page: the file rows are keyed by path,
        // and a second run over the same paths would otherwise light up the first
        // run's rows — with the previous summary still sitting under them.
        panel.post("run");
        return panel;
    }

    /** The run ended; its Cancel button no longer has anything to abort. */
    static endRun(): void {
        IndexingPanel.runActions = undefined;
    }

    /** Open the panel from the command palette, with whatever the last run left. */
    static showOrReveal(extensionUri: vscode.Uri, openFile: (path: string) => void): void {
        IndexingPanel.fileOpener = openFile;
        if (IndexingPanel.instance !== undefined) {
            IndexingPanel.instance.panel.reveal(undefined, false);
            IndexingPanel.instance.post("run");
            return;
        }
        IndexingPanel.instance = new IndexingPanel(extensionUri, "active");
    }

    private constructor(extensionUri: vscode.Uri, placement: IndexingPanelPlacement) {
        this.panel = vscode.window.createWebviewPanel(
            "mindexIndexing",
            `${BRAND} — Indexing`,
            // The object form is the only one that takes `preserveFocus` at
            // creation: a panel that opens by itself must never take the caret out
            // of the file the user is editing.
            {
                viewColumn:
                    placement === "beside"
                        ? vscode.ViewColumn.Beside
                        : vscode.ViewColumn.Active,
                preserveFocus: true,
            },
            {
                enableScripts: true,
                retainContextWhenHidden: false,
                localResourceRoots: mediaRoots(extensionUri),
            }
        );
        this.panel.webview.html = renderPage(this.panel.webview, extensionUri, {
            body: readMedia(extensionUri, "indexing.html"),
            styles: ["common.css", "lang.css", "indexing.css"],
            modules: ["js/indexing.js"],
            codicons: true,
        });

        this.disposables.push(
            this.panel.onDidChangeViewState((e) => {
                if (e.webviewPanel.visible) {
                    // A hidden panel's DOM is discarded, so this is a full resync
                    // from the retained log rather than a delta.
                    this.post("run");
                }
            }),
            this.panel.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
                switch (msg.type) {
                    case "ready":
                        this.post("run");
                        break;
                    case "cancel":
                        IndexingPanel.runActions?.cancel();
                        break;
                    case "openFile":
                        IndexingPanel.fileOpener?.(asString(msg.path));
                        break;
                }
            })
        );

        this.panel.onDidDispose(() => {
            IndexingPanel.instance = undefined;
            for (const d of this.disposables) {
                d.dispose();
            }
        });
    }

    /**
     * One tick of a running reindex: the whole snapshot, every time.
     *
     * It used to be a snapshot plus an event delta, because the page kept an
     * append-only log the host could not afford to resend. There is no log any
     * more — one bounded, path-keyed list of files *is* the state — so the delta
     * channel bought nothing but a way for the two sides to disagree after a
     * resync. The list is capped by `IndexRun`, so the message stays small however
     * many files the run posts.
     */
    update(snapshot: IndexRunSnapshot): void {
        this.snapshot = snapshot;
        this.postTick();
    }

    /**
     * Re-render the finished run with the follow-up drift check's `indexing` count.
     *
     * It arrives after the run returns, because only that check can tell a file the
     * server refused as in-flight from one it hash-skipped — both come back absent
     * from a 200. The panel is given the same number the toast gets, from the same
     * source, so the two surfaces cannot disagree about what just happened.
     */
    finishedWithInFlight(inFlight: number): void {
        if (this.snapshot === undefined) {
            return;
        }
        this.snapshot = { ...this.snapshot, inFlight };
        this.postTick();
    }

    private postTick(): void {
        if (this.snapshot === undefined) {
            return;
        }
        void this.panel.webview.postMessage({ type: "tick", snapshot: this.snapshot });
    }

    private post(type: "run"): void {
        void this.panel.webview.postMessage({ type, snapshot: this.snapshot });
    }
}
