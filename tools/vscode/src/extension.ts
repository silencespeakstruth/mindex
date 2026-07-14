import * as vscode from "vscode";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { MindexApi } from "./api";
import { MindexFile, parseMindexFile } from "./mindexFile";
import { buildManifest, scanWorkspace } from "./scanner";
import { DriftTreeProvider } from "./driftView";
import { StatusTreeProvider, failedFilePath } from "./statusView";
import { reindexPaths, showReindexSummary } from "./indexer";
import { runSearch } from "./search";
import { isCancellation, reportError } from "./errors";

interface Project {
    root: string;
    mindex: MindexFile;
}

export function activate(context: vscode.ExtensionContext): void {
    let api = createApi();
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration("mindex")) {
                api.dispose();
                api = createApi();
            }
        }),
        new vscode.Disposable(() => api.dispose())
    );

    const statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 90);
    statusBar.command = "mindex.refreshStatus";
    statusBar.tooltip = "mindex server health — click to refresh";
    context.subscriptions.push(statusBar);

    let project: Project | undefined;
    const loadProject = async (): Promise<Project> => {
        const folders = vscode.workspace.workspaceFolders ?? [];
        for (const folder of folders) {
            const file = path.join(folder.uri.fsPath, ".mindex");
            try {
                const text = await fs.readFile(file, "utf8");
                project = { root: folder.uri.fsPath, mindex: parseMindexFile(text) };
                return project;
            } catch (e) {
                if ((e as NodeJS.ErrnoException).code !== "ENOENT") {
                    throw e;
                }
            }
        }
        throw new Error(
            "no .mindex file found at a workspace root — create one with the project GUID on the first line"
        );
    };

    const driftProvider = new DriftTreeProvider(
        vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? ""
    );
    const driftView = vscode.window.createTreeView("mindexDrift", {
        treeDataProvider: driftProvider,
        showCollapseAll: true,
    });
    driftView.onDidChangeCheckboxState((e) => driftProvider.applyCheckboxChanges(e.items));
    context.subscriptions.push(driftView);

    const statusProvider = new StatusTreeProvider(
        () => api,
        () => project?.mindex.guid,
        statusBar
    );
    context.subscriptions.push(
        vscode.window.createTreeView("mindexStatus", { treeDataProvider: statusProvider })
    );

    const checkDrift = async (): Promise<void> => {
        try {
            const proj = await loadProject();
            await vscode.window.withProgress(
                { location: { viewId: "mindexDrift" } },
                async () => {
                    const files = await scanWorkspace(proj.root, proj.mindex);
                    const manifest = await buildManifest(files);
                    const drift = await api.drift(proj.mindex.guid, manifest.hashes);
                    driftProvider.setDrift(drift);
                    driftView.description = `checked ${new Date().toLocaleTimeString()} — ${
                        files.length
                    } files`;
                    const actionable =
                        drift.stale.length + drift.missing.length + drift.orphaned.length;
                    if (actionable === 0) {
                        void vscode.window.setStatusBarMessage(
                            "mindex: index is in sync with the working tree",
                            5000
                        );
                    }
                }
            );
        } catch (e) {
            await reportError("Drift check failed", e, checkDrift);
        }
    };

    const reindex = async (paths: string[], noneMessage: string): Promise<void> => {
        if (paths.length === 0) {
            void vscode.window.showInformationMessage(noneMessage);
            return;
        }
        const proj = await loadProject();
        const batch = config().get<number>("batchSize", 100);
        const summary = await reindexPaths(api, proj.mindex.guid, proj.root, paths, batch);
        if (summary !== undefined) {
            showReindexSummary(summary);
            await checkDrift();
        }
    };

    context.subscriptions.push(
        vscode.commands.registerCommand("mindex.checkDrift", checkDrift),

        vscode.commands.registerCommand("mindex.reindexSelected", () =>
            reindex(
                driftProvider.selectedPaths("stale", "missing"),
                "mindex: nothing selected in Stale/Missing — tick checkboxes first (or run Check Drift)."
            )
        ),

        vscode.commands.registerCommand("mindex.reindexAllDrift", () =>
            reindex(
                driftProvider.allPaths("stale", "missing"),
                "mindex: no stale or missing files — index is in sync."
            )
        ),

        vscode.commands.registerCommand("mindex.reindexCurrentFile", async () => {
            const doc = vscode.window.activeTextEditor?.document;
            const proj = await loadProject();
            if (doc === undefined || !doc.uri.fsPath.startsWith(proj.root)) {
                void vscode.window.showInformationMessage(
                    "mindex: no project file is active."
                );
                return;
            }
            const rel = path.relative(proj.root, doc.uri.fsPath).replaceAll("\\", "/");
            await reindex([rel], "");
        }),

        vscode.commands.registerCommand("mindex.deleteOrphanedSelected", async () => {
            const paths = driftProvider.selectedPaths("orphaned");
            if (paths.length === 0) {
                void vscode.window.showInformationMessage(
                    "mindex: nothing selected in Orphaned."
                );
                return;
            }
            const confirm = await vscode.window.showWarningMessage(
                `Delete ${paths.length} orphaned file(s) from the index? (Soft delete; GC removes vectors later.)`,
                { modal: true },
                "Delete"
            );
            if (confirm !== "Delete") {
                return;
            }
            try {
                const proj = await loadProject();
                const n = await api.deleteFiles(proj.mindex.guid, { include: { paths } });
                void vscode.window.showInformationMessage(
                    `mindex: ${n} file(s) deleted from the index.`
                );
                await checkDrift();
            } catch (e) {
                await reportError("Delete from index failed", e);
            }
        }),

        vscode.commands.registerCommand("mindex.cancelIndexing", async () => {
            const paths = driftProvider.allPaths("indexing");
            if (paths.length === 0) {
                void vscode.window.showInformationMessage("mindex: nothing is in flight.");
                return;
            }
            try {
                const proj = await loadProject();
                const n = await api.cancel(proj.mindex.guid, { include: { paths } });
                void vscode.window.showInformationMessage(
                    `mindex: cancelled ${n} in-flight file(s) (best-effort).`
                );
                await checkDrift();
            } catch (e) {
                await reportError("Cancel failed", e);
            }
        }),

        vscode.commands.registerCommand("mindex.refreshStatus", () =>
            statusProvider.refresh()
        ),

        vscode.commands.registerCommand("mindex.retryAllFailed", async () => {
            try {
                const proj = await loadProject();
                const n = await api.retry(proj.mindex.guid);
                void vscode.window.showInformationMessage(
                    n > 0
                        ? `mindex: requeued ${n} failed file(s) — the retry worker picks them up within ~60 s.`
                        : "mindex: no failed files to retry."
                );
                await statusProvider.refresh();
            } catch (e) {
                await reportError("Retry failed", e);
            }
        }),

        vscode.commands.registerCommand("mindex.retryFile", async (node: unknown) => {
            const filePath = failedFilePath(node);
            if (filePath === undefined) {
                return;
            }
            try {
                const proj = await loadProject();
                const n = await api.retry(proj.mindex.guid, {
                    include: { paths: [filePath] },
                });
                void vscode.window.showInformationMessage(
                    n > 0
                        ? `mindex: requeued ${filePath}.`
                        : `mindex: ${filePath} is not failed anymore.`
                );
                await statusProvider.refresh();
            } catch (e) {
                await reportError("Retry failed", e);
            }
        }),

        vscode.commands.registerCommand("mindex.search", async () => {
            try {
                const proj = await loadProject();
                const topK = config().get<number>("topK", 10);
                await runSearch(api, proj.mindex.guid, proj.root, topK);
            } catch (e) {
                if (!isCancellation(e)) {
                    await reportError("Search failed", e);
                }
            }
        })
    );

    // Initial, non-blocking status refresh so the status bar appears on activation.
    void statusProvider.refresh();
}

export function deactivate(): void {}

function config(): vscode.WorkspaceConfiguration {
    return vscode.workspace.getConfiguration("mindex");
}

function createApi(): MindexApi {
    const cfg = config();
    const caCert = cfg.get<string>("caCert", "").trim();
    return new MindexApi({
        serverUrl: cfg.get<string>("serverUrl", "https://127.0.0.1:11111"),
        noVerify: cfg.get<boolean>("noVerify", false),
        caCertPath: caCert === "" ? undefined : caCert,
    });
}
