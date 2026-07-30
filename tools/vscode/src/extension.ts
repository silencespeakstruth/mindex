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
import { ResearchPanel, ResearchSubmission } from "./researchView";
import { AskSubmission, AskViewProvider } from "./askView";
import { createProjectFile } from "./createProject";
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

    // ── Project marker: one .mindex at a workspace root, cached and watched ──────
    // The file is the extension's whole reason to be active, so it is read once and
    // re-read on change rather than per command: a scope edit must reach the next
    // drift check without a window reload, and a delete must visibly disable the UI.
    let project: Project | undefined;
    let projectError: string | undefined;

    const reloadProject = async (): Promise<void> => {
        project = undefined;
        projectError = undefined;
        for (const folder of vscode.workspace.workspaceFolders ?? []) {
            const file = path.join(folder.uri.fsPath, ".mindex");
            let text: string;
            try {
                text = await fs.readFile(file, "utf8");
            } catch (e) {
                if ((e as NodeJS.ErrnoException).code === "ENOENT") {
                    continue;
                }
                projectError = `cannot read ${file}: ${e instanceof Error ? e.message : String(e)}`;
                break;
            }
            // The first folder that *has* the file wins, valid or not — falling
            // through to another folder would silently index the wrong project.
            try {
                project = { root: folder.uri.fsPath, mindex: parseMindexFile(text) };
            } catch (e) {
                projectError = e instanceof Error ? e.message : String(e);
            }
            break;
        }
        if (project === undefined && projectError === undefined) {
            projectError =
                "no .mindex file found at a workspace root — run " +
                "“mindex: Create a .mindex Project File” to generate one";
        }
        await vscode.commands.executeCommand(
            "setContext",
            "mindex.hasProject",
            project !== undefined
        );
    };

    // Async purely to keep every call site unchanged; the read now happens in
    // reloadProject, driven by the file watcher.
    const loadProject = (): Promise<Project> => {
        if (project === undefined) {
            return Promise.reject(new Error(projectError ?? "no usable .mindex file"));
        }
        return Promise.resolve(project);
    };

    // For the handlers that have no try/catch of their own: without a marker, the
    // raw rejection reads as "command failed", swallowing the one message that says
    // how to fix it.
    const currentProject = async (): Promise<Project | undefined> => {
        try {
            return await loadProject();
        } catch (e) {
            await reportError("mindex", e);
            return undefined;
        }
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
        statusBar,
        // The health refresh is the only thing that knows the server's optional
        // Ollama is down; the Ask view is where that costs the user something.
        (available) => askProvider.setResearchAvailable(available),
        // Same shape, and for the same reason: the refresh is what knows what the
        // index holds, the Ask view is what has to stop offering the rest.
        (languages) => askProvider.setLanguageInventory(languages),
        // `GET /config` is read here on every refresh rather than once at
        // activation, because `research.models` is no longer static: a model pulled
        // after the window opened has to reach the picker without a reload.
        (cfg) => {
            if (cfg.research !== undefined) {
                askProvider.setResearchConfig(cfg.research);
            }
        }
    );
    context.subscriptions.push(
        vscode.window.createTreeView("mindexStatus", { treeDataProvider: statusProvider })
    );

    // ── Research: sidebar form → SSE stream → output tab ─────────────────────
    let researchAbort: AbortController | undefined;
    let researchPanel: ResearchPanel | undefined;

    const cancelResearch = (): void => {
        researchAbort?.abort(); // closing the connection IS the server-side cancel
        researchAbort = undefined;
        askProvider.setRunning(false);
        researchPanel?.cancelled();
        researchPanel = undefined;
    };

    const startResearch = async (s: ResearchSubmission): Promise<void> => {
        if (researchAbort !== undefined) {
            void vscode.window.showInformationMessage(
                "mindex: a research run is already in progress — cancel it first."
            );
            return;
        }
        let proj: Project;
        try {
            proj = await loadProject();
        } catch (e) {
            await reportError("Research failed", e);
            return;
        }

        // A fresh tab per run: reports are documents worth keeping side by side, and
        // reusing one would destroy the previous answer on the next question. Only the
        // *live* panel is tracked, so closing an old report cancels nothing.
        const panel = new ResearchPanel(
            context.extensionUri,
            s.question,
            () => {
                if (researchPanel === panel) {
                    // Closing the live tab abandons the run: abort so the server cancels.
                    cancelResearch();
                }
            },
            { include: s.include, exclude: s.exclude }
        );
        researchPanel = panel;

        const abort = new AbortController();
        researchAbort = abort;
        askProvider.setRunning(true);
        let failure: unknown;
        try {
            await api.research(
                proj.mindex.guid,
                {
                    question: s.question,
                    effort: s.effort,
                    model: s.model === "" ? undefined : s.model,
                    budget: s.budget,
                    include: s.include,
                    exclude: s.exclude,
                },
                {
                    onThinking: (text) => panel.thinking(text),
                    onStep: (step) => panel.step(step),
                    onProgress: (progress) => panel.progress(progress),
                    onSummary: (text) => panel.summary(text),
                    onCitations: (citations) => panel.citations(citations),
                    onDone: (info) => panel.done(info),
                    onError: (code, detail) => {
                        panel.error(`${code}: ${detail}`, code);
                        // The run just proved Ollama is unreachable — re-read health
                        // so the status tree and the Ask notice say so too.
                        if (code === "ollama.unavailable") {
                            void statusProvider.refresh();
                        }
                    },
                },
                abort.signal
            );
        } catch (e) {
            if (!isCancellation(e)) {
                panel.error(e instanceof Error ? e.message : String(e));
                failure = e;
            }
        } finally {
            // Only clear if this run is still the active one (not a newer restart).
            if (researchAbort === abort) {
                researchAbort = undefined;
                askProvider.setRunning(false);
                // The tab stays open with its finished report, but it is no longer the
                // live run — closing it must not fire a cancel for work already done.
                if (researchPanel === panel) {
                    researchPanel = undefined;
                }
            }
        }
        // Reported *after* the form is released, never inside the `catch`: an error
        // notification's thenable resolves only when the user dismisses it, and VS Code
        // error notifications do not auto-hide — awaiting it in `catch` delays `finally`,
        // so a failed run left Research disabled and `researchAbort` set (refusing every
        // later question) until the toast was clicked away.
        if (failure !== undefined) {
            await reportError("Research failed", failure);
        }
    };

    /**
     * The submission with `include` narrowed to the folder of the active editor.
     *
     * `dir/*` rather than `dir/**`: the server evaluates SQLite `GLOB`, where `*`
     * already crosses `/`, so the double form buys nothing and reads as if it were
     * doing something. Falls back to the submission unchanged when no editor is open
     * or the file lies outside the project — silently narrowing to the wrong subtree
     * would be worse than not narrowing.
     */
    const scopeToCurrentFolder = (s: AskSubmission): AskSubmission => {
        const doc = vscode.window.activeTextEditor?.document;
        if (doc === undefined || project === undefined || doc.uri.scheme !== "file") {
            void vscode.window.showInformationMessage(
                "mindex: open a file in this project first — there is no folder to scope to."
            );
            return s;
        }
        const rel = path
            .relative(project.root, path.dirname(doc.uri.fsPath))
            .split(path.sep)
            .join("/");
        if (rel === "" || rel.startsWith("..")) {
            void vscode.window.showInformationMessage(
                "mindex: that file is not inside the project, or is at its root."
            );
            return s;
        }
        return { ...s, include: { ...s.include, paths: [`${rel}/*`] } };
    };

    const onAsk = async (s: AskSubmission): Promise<void> => {
        if (s.mode === "research") {
            // "this folder" is resolved here, not in the webview: the webview has no
            // editor API, and mirroring the active editor into it would be a second
            // channel to keep fresh for one button.
            const scoped = s.scopeCurrentFolder === true ? scopeToCurrentFolder(s) : s;
            if (s.scopeCurrentFolder === true) {
                askProvider.setScope(scoped.include, scoped.exclude);
                return;
            }
            await startResearch({
                question: s.text,
                effort: s.effort,
                model: s.model,
                budget: s.budget,
                include: scoped.include,
                exclude: scoped.exclude,
            });
            return;
        }
        try {
            const proj = await loadProject();
            await runSearch(
                api,
                proj.mindex.guid,
                proj.root,
                s.topK,
                s.text,
                s.language === "" ? undefined : s.language
            );
        } catch (e) {
            if (!isCancellation(e)) {
                await reportError("Search failed", e);
            }
        }
    };

    const askProvider = new AskViewProvider(
        () => config().get<string>("researchModel", ""),
        () => config().get<number>("topK", 10),
        // The project's standing scope, for prefilling the Scope panel. Read live
        // rather than captured: `.mindex` is watched and reloaded, and a stale
        // prefill is a boundary the user did not choose.
        () => ({
            include: project?.mindex.includePaths ?? [],
            exclude: project?.mindex.excludePaths ?? [],
            languages: project?.mindex.languages ?? [],
        }),
        (s) => void onAsk(s),
        cancelResearch
    );
    context.subscriptions.push(
        vscode.window.registerWebviewViewProvider(AskViewProvider.viewId, askProvider),
        new vscode.Disposable(cancelResearch)
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

    const reindex = async (
        paths: string[],
        noneMessage: string,
        force = false
    ): Promise<void> => {
        if (paths.length === 0) {
            void vscode.window.showInformationMessage(noneMessage);
            return;
        }
        const proj = await currentProject();
        if (proj === undefined) {
            return;
        }
        const batch = config().get<number>("batchSize", 100);
        const summary = await reindexPaths(
            api,
            proj.mindex.guid,
            proj.root,
            paths,
            batch,
            force
        );
        if (summary !== undefined) {
            showReindexSummary(summary);
            await checkDrift();
            // The inventory just changed: a language may have gained its first
            // searchable chunk, which is what the Ask view's pickers offer.
            await statusProvider.refresh();
        }
    };

    context.subscriptions.push(
        // The one command that works *without* a project: it is how you get one.
        vscode.commands.registerCommand("mindex.createProjectFile", () =>
            createProjectFile(reloadProject)
        ),

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
            const proj = await currentProject();
            if (proj === undefined) {
                return;
            }
            if (doc === undefined || !doc.uri.fsPath.startsWith(proj.root)) {
                void vscode.window.showInformationMessage(
                    "mindex: no project file is active."
                );
                return;
            }
            const rel = path.relative(proj.root, doc.uri.fsPath).replaceAll("\\", "/");
            await reindex([rel], "");
        }),

        // ── Force variants. The server normally skips a file whose content hash and
        //    derivation versions both match, so these exist for what that cannot see:
        //    a grammar-crate bump with the version constant untouched, a suspected-
        //    corrupt index, or debugging one file.
        vscode.commands.registerCommand("mindex.forceReindexCurrentFile", async () => {
            const doc = vscode.window.activeTextEditor?.document;
            const proj = await currentProject();
            if (proj === undefined) {
                return;
            }
            if (doc === undefined || !doc.uri.fsPath.startsWith(proj.root)) {
                void vscode.window.showInformationMessage(
                    "mindex: no project file is active."
                );
                return;
            }
            const rel = path.relative(proj.root, doc.uri.fsPath).replaceAll("\\", "/");
            await reindex([rel], "", true);
        }),

        vscode.commands.registerCommand("mindex.forceReindexProject", async () => {
            const proj = await currentProject();
            if (proj === undefined) {
                return;
            }
            const files = await scanWorkspace(proj.root, proj.mindex);
            if (files.length === 0) {
                void vscode.window.showInformationMessage("mindex: nothing to index.");
                return;
            }
            // Modal: this re-slices and re-embeds every file in the project, which is
            // GPU-bound and can take a long time on a large tree.
            const confirm = await vscode.window.showWarningMessage(
                `Force reindex all ${files.length} file(s)? Every file is re-sliced and re-embedded, ignoring the unchanged-skip. An ordinary reindex already picks up slicer and tags-query changes.`,
                { modal: true },
                "Force reindex"
            );
            if (confirm !== "Force reindex") {
                return;
            }
            await reindex(
                files.map((f) => f.relPath),
                "",
                true
            );
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
                // Same reason as after a reindex, in the other direction: a language
                // may have just lost its last searchable chunk.
                await statusProvider.refresh();
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
                        : "mindex: no failed files in this project to retry."
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

        vscode.commands.registerCommand("mindex.research", () =>
            askProvider.focus("research")
        ),

        vscode.commands.registerCommand("mindex.ask", () => askProvider.focus()),

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

    // ── Watch .mindex itself ────────────────────────────────────────────────────
    // One watcher per workspace folder, rebuilt when the folder set changes. A
    // scope edit invalidates the drift results it was computed from, so they are
    // cleared rather than left to look current.
    let markerWatchers: vscode.FileSystemWatcher[] = [];
    const onMarkerChanged = async (): Promise<void> => {
        await reloadProject();
        driftProvider.clear();
        await statusProvider.refresh();
    };
    const rewatchMarkers = (): void => {
        for (const w of markerWatchers) {
            w.dispose();
        }
        markerWatchers = (vscode.workspace.workspaceFolders ?? []).map((folder) => {
            const w = vscode.workspace.createFileSystemWatcher(
                new vscode.RelativePattern(folder, ".mindex")
            );
            w.onDidCreate(() => void onMarkerChanged());
            w.onDidChange(() => void onMarkerChanged());
            w.onDidDelete(() => void onMarkerChanged());
            return w;
        });
    };
    rewatchMarkers();
    context.subscriptions.push(
        new vscode.Disposable(() => {
            for (const w of markerWatchers) {
                w.dispose();
            }
        }),
        vscode.workspace.onDidChangeWorkspaceFolders(() => {
            rewatchMarkers();
            void onMarkerChanged();
        })
    );

    // Initial, non-blocking load + status refresh so the views and the status bar
    // reflect reality as soon as the extension activates.
    void reloadProject().then(() => statusProvider.refresh());
}

export function deactivate(): void {}

function config(): vscode.WorkspaceConfiguration {
    return vscode.workspace.getConfiguration("mindex");
}

function createApi(): MindexApi {
    const cfg = config();
    const caCert = cfg.get<string>("caCert", "").trim();
    const apiKey = cfg.get<string>("apiKey", "").trim();
    return new MindexApi({
        serverUrl: cfg.get<string>("serverUrl", "https://127.0.0.1:11111"),
        noVerify: cfg.get<boolean>("noVerify", false),
        caCertPath: caCert === "" ? undefined : caCert,
        apiKey: apiKey === "" ? undefined : apiKey,
    });
}
