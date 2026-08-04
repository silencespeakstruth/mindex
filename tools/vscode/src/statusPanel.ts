import * as vscode from "vscode";
import { BRAND } from "./brand";
import { asString, mediaRoots, readMedia, renderPage } from "./webview";
import { StatusMonitor } from "./statusMonitor";
import { BusyKeys } from "./busy";

/** What the panel's buttons ask the extension to do. */
export interface StatusActions {
    /**
     * Returns when the re-check has finished — the panel greys its refresh
     * buttons until it does, and a `void` here would grey them for one frame.
     * The monitor's own `refreshing` signal covers a *background* poll; this
     * covers the press.
     */
    refresh(): void | Promise<void>;
    retryAll(): void;
    retryFile(path: string): void;
    openFile(path: string): void;
    openSettings(): void;
    /**
     * Issue a short-lived token for an agent.
     *
     * Returns when the dialog has finished, so the button stays greyed for the
     * whole chain rather than for one frame — the same reason `refresh` above is
     * awaitable. It delegates to the `mindex.mintAgentToken` command rather than
     * calling the flow directly: the command owns the project lookup and the
     * `auth.action_not_permitted` sentence, and a second copy of either would be
     * a second thing to keep in step.
     */
    mintAgentToken(): void | Promise<void>;
}

/**
 * The Server Status panel: health, runtime, the project's inventory and its dead
 * letters, in an editor tab opened from the status-bar indicator.
 *
 * It used to be a permanent sidebar tree, which was a third of the sidebar spent on
 * something consulted only when the indicator is not green. Moving it here also gives
 * it the width a four-column inventory table needs, which the sidebar never had.
 *
 * A **singleton**: two tabs of the same snapshot is clutter, not a comparison. It is
 * also a pure subscriber — it reads `monitor.latest` on open and listens for changes,
 * and the monitor does not know it exists. That is what keeps the Ask form's pickers
 * updating while this panel is closed.
 */
export class StatusPanel {
    private static current?: StatusPanel;

    private readonly panel: vscode.WebviewPanel;
    private readonly disposables: vscode.Disposable[] = [];

    static showOrReveal(
        extensionUri: vscode.Uri,
        monitor: StatusMonitor,
        actions: StatusActions
    ): void {
        if (StatusPanel.current !== undefined) {
            StatusPanel.current.panel.reveal(undefined, false);
            StatusPanel.current.post();
            return;
        }
        StatusPanel.current = new StatusPanel(extensionUri, monitor, actions);
    }

    private constructor(
        extensionUri: vscode.Uri,
        private readonly monitor: StatusMonitor,
        actions: StatusActions
    ) {
        this.panel = vscode.window.createWebviewPanel(
            "mindexStatus",
            `${BRAND} — Server Status`,
            vscode.ViewColumn.Active,
            {
                enableScripts: true,
                // Cheap to rebuild and never holds unsaved input, so there is no
                // reason to keep a hidden panel's DOM alive. The cost is that a
                // restored panel starts blank — which is why every path back to
                // visibility re-posts the snapshot.
                retainContextWhenHidden: false,
                localResourceRoots: mediaRoots(extensionUri),
            }
        );
        this.panel.webview.html = renderPage(this.panel.webview, extensionUri, {
            body: readMedia(extensionUri, "status.html"),
            styles: ["common.css", "lang.css", "status.css"],
            modules: ["js/status.js"],
            codicons: true,
        });

        this.disposables.push(
            monitor.onDidChangeSnapshot(() => this.post()),
            // The refresh buttons echo the *monitor*, not the press, so a
            // background poll greys them too. Otherwise the panel visibly
            // re-renders while the button that supposedly caused it sits idle and
            // clickable, inviting exactly the second press this prevents.
            monitor.onDidChangeRefreshing((busy) => this.postBusy("refresh", busy)),
            this.panel.onDidChangeViewState((e) => {
                if (e.webviewPanel.visible) {
                    this.post();
                    this.postBusy("refresh", monitor.refreshing);
                }
            }),
            this.panel.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
                switch (msg.type) {
                    case "ready":
                        this.post();
                        this.postBusy("refresh", monitor.refreshing);
                        break;
                    case "refresh":
                        // No guard here: the monitor refuses a concurrent refresh
                        // itself, and its `refreshing` signal is what disables the
                        // buttons — one authority, not two that can disagree.
                        void actions.refresh();
                        break;
                    case "retryAll":
                        void this.busy("retry", () => Promise.resolve(actions.retryAll()));
                        break;
                    case "retryFile": {
                        const path = asString(msg.path);
                        void this.busy(`row:${path}`, () =>
                            Promise.resolve(actions.retryFile(path))
                        );
                        break;
                    }
                    case "openFile":
                        actions.openFile(asString(msg.path));
                        break;
                    case "openSettings":
                        actions.openSettings();
                        break;
                    case "mintAgentToken":
                        void this.busy("mint", () =>
                            Promise.resolve(actions.mintAgentToken())
                        );
                        break;
                }
            })
        );

        this.panel.onDidDispose(() => {
            StatusPanel.current = undefined;
            this.keys.reset();
            for (const d of this.disposables) {
                d.dispose();
            }
        });
        this.post();
    }

    private post(): void {
        void this.panel.webview.postMessage({ snapshot: this.monitor.latest });
    }

    private postBusy(key: string, busy: boolean): void {
        void this.panel.webview.postMessage({ type: "busy", key, busy });
    }

    private readonly keys = new BusyKeys((m) => void this.panel.webview.postMessage(m));

    private busy(key: string, run: () => Promise<void>): Promise<void | undefined> {
        return this.keys.run(key, run);
    }
}
