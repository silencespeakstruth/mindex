import * as vscode from "vscode";
import { BRAND } from "./brand";
import { asString, mediaRoots, readMedia, renderPage } from "./webview";
import { StatusMonitor } from "./statusMonitor";

/** What the panel's buttons ask the extension to do. */
export interface StatusActions {
    refresh(): void;
    retryAll(): void;
    retryFile(path: string): void;
    openFile(path: string): void;
    openSettings(): void;
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
            this.panel.onDidChangeViewState((e) => {
                if (e.webviewPanel.visible) {
                    this.post();
                }
            }),
            this.panel.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
                switch (msg.type) {
                    case "ready":
                        this.post();
                        break;
                    case "refresh":
                        actions.refresh();
                        break;
                    case "retryAll":
                        actions.retryAll();
                        break;
                    case "retryFile":
                        actions.retryFile(asString(msg.path));
                        break;
                    case "openFile":
                        actions.openFile(asString(msg.path));
                        break;
                    case "openSettings":
                        actions.openSettings();
                        break;
                }
            })
        );

        this.panel.onDidDispose(() => {
            StatusPanel.current = undefined;
            for (const d of this.disposables) {
                d.dispose();
            }
        });
        this.post();
    }

    private post(): void {
        void this.panel.webview.postMessage({ snapshot: this.monitor.latest });
    }
}
