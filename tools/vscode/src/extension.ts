import * as vscode from "vscode";
import * as fs from "node:fs/promises";
import * as fsSync from "node:fs";
import * as path from "node:path";
import { MindexApi } from "./api";
import { MindexFile, parseMindexFile } from "./mindexFile";
import { buildManifest, scanWorkspace } from "./scanner";
import { DRIFT_MESSAGE, DriftTreeProvider } from "./driftView";
import { StatusMonitor, UNAVAILABLE } from "./statusMonitor";
import { StatusPanel } from "./statusPanel";
import { ResearchRunsPanel } from "./researchRunsPanel";
import { browseResearchRuns, pickContextRuns } from "./researchContextPick";
import { openResearchReport, RESEARCH_SCHEME, ResearchDocumentProvider } from "./researchDocs";
import { paintStatusBar } from "./statusBar";
import { IndexStatusBar } from "./indexStatusBar";
import { IndexingPanel, IndexingPanelPlacement } from "./indexingPanel";
import { reindexPaths, showReindexSummary } from "./indexer";
import { runSearch } from "./search";
import { ResearchPanel, ResearchSubmission } from "./researchView";
import { AskSubmission, AskViewProvider } from "./askView";
import { createProjectFile } from "./createProject";
import { isCancellation, reportError } from "./errors";
import { BRAND, say } from "./brand";

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
    // Clicking opens the panel rather than refreshing invisibly: a click that changes
    // nothing on screen reads as a dead control, and the refresh happens on the way
    // to the panel anyway.
    statusBar.command = "mindex.openStatus";
    context.subscriptions.push(statusBar);
    paintStatusBar(statusBar);

    // The live indexing feed, shown only while a reindex runs. A priority above the
    // health indicator's puts it immediately to its left, so the two read as one
    // group; it is a separate item because it is transient and the other is not.
    const indexStatusItem = vscode.window.createStatusBarItem(
        vscode.StatusBarAlignment.Right,
        91
    );
    context.subscriptions.push(indexStatusItem);
    const indexStatusBar = new IndexStatusBar(indexStatusItem);

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
                `“${BRAND}: Create a .mindex Project File” to generate one`;
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
            await reportError(BRAND, e);
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

    const statusProvider = new StatusMonitor(
        () => api,
        () => project?.mindex.guid,
        () => config().get<string>("serverUrl", "https://127.0.0.1:11111"),
        // The health refresh is the only thing that knows what the server can still
        // do; the Ask view is where that costs the user something.
        (availability) => {
            askProvider.setAvailability(availability);
            if (!availability.ask) {
                abortRunsForDegradation(availability.reason);
            }
        },
        // Same shape, and for the same reason: the refresh is what knows what the
        // index holds, the Ask view is what has to stop offering the rest.
        (languages) => askProvider.setLanguageInventory(languages),
        // `GET /config` is read here on every refresh rather than once at
        // activation, because `research.models` is no longer static: a model pulled
        // after the window opened has to reach the picker without a reload.
        (cfg) => askProvider.setServerConfig(cfg)
    );
    context.subscriptions.push(
        statusProvider,
        // The status bar is the monitor's only unconditional subscriber; the panel is
        // optional and subscribes for itself when it exists.
        statusProvider.onDidChangeSnapshot((snapshot) => paintStatusBar(statusBar, snapshot)),
        // Drift is the second: the server's claim count is what decides whether a
        // reindex would do anything, and it is the only place the user can see that
        // work is in flight that this window did not start.
        statusProvider.onDidChangeSnapshot((snapshot) =>
            driftProvider.setServerClaims(
                snapshot.runtime !== undefined && snapshot.runtime !== UNAVAILABLE
                    ? snapshot.runtime.indexing_claims
                    : 0
            )
        )
    );

    /**
     * What the Status panel's buttons do. Every one of them is an existing command,
     * so the panel adds a surface and not a second implementation — and the palette
     * keeps working for anyone who prefers it.
     */
    const statusActions = {
        refresh: () => void statusProvider.refresh(),
        retryAll: () => void vscode.commands.executeCommand("mindex.retryAllFailed"),
        retryFile: (p: string) => void vscode.commands.executeCommand("mindex.retryFile", p),
        openFile: (p: string) => {
            if (project !== undefined) {
                void vscode.window.showTextDocument(
                    vscode.Uri.file(path.join(project.root, p))
                );
            }
        },
        openSettings: () => openSettings(),
    };

    // ── Research: sidebar form → SSE stream → output tab ─────────────────────
    let researchAbort: AbortController | undefined;
    let researchPanel: ResearchPanel | undefined;

    /**
     * Searches currently in flight, so a health collapse can end them.
     *
     * Research is tracked separately above because it owns a panel and a running flag;
     * a search owns only its request and the quick pick it is about to open, which is
     * what this abstracts to. Entries remove themselves — a `finally` in `runSearch` —
     * so a completed search cannot be aborted twice or hold a reference forever.
     */
    const liveSearches = new Set<AbortController>();

    const cancelResearch = (): void => {
        researchAbort?.abort(); // closing the connection IS the server-side cancel
        researchAbort = undefined;
        askProvider.setRunning(false);
        researchPanel?.cancelled();
        researchPanel = undefined;
    };

    /**
     * A required dependency went away while something was running.
     *
     * Order matters and is the whole of this function. **Reset first**: the form is
     * released and the handles cleared before anything is reported, because a
     * notification's thenable resolves only when the user dismisses it — the same trap
     * that once left Research disabled behind an un-clicked toast (see `startResearch`).
     * Then abort, then report it as a *failure* rather than as a cancellation: the user
     * did not stop this, and a run that ends looking like their own Stop is a run they
     * will assume produced nothing worth keeping.
     */
    const abortRunsForDegradation = (reason?: string): void => {
        const panel = researchPanel;
        const abort = researchAbort;
        const searches = [...liveSearches];
        if (abort === undefined && searches.length === 0) {
            return;
        }

        researchAbort = undefined;
        researchPanel = undefined;
        askProvider.setRunning(false);
        liveSearches.clear();

        abort?.abort();
        for (const search of searches) {
            search.abort();
        }

        const detail = reason ?? "a required dependency stopped answering";
        panel?.error(`mindex.degraded: ${detail} — the run was aborted.`);
        void vscode.window.showErrorMessage(
            say(`server health degraded — ${detail}. The run in progress was aborted.`)
        );
    };

    const startResearch = async (s: ResearchSubmission): Promise<void> => {
        if (researchAbort !== undefined) {
            void vscode.window.showInformationMessage(
                say("a research run is already in progress — cancel it first.")
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
            { include: s.include, exclude: s.exclude },
            // Titles come from the form's own cache — `contextRunIds` on the
            // submission is ids only, and a header reading "#7, #9" would name the
            // provenance without saying what it is.
            askProvider.currentContextRuns.filter((r) =>
                (s.contextRunIds ?? []).includes(r.id)
            )
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
                    context_run_ids:
                        s.contextRunIds !== undefined && s.contextRunIds.length > 0
                            ? s.contextRunIds
                            : undefined,
                },
                {
                    onThinking: (text) => panel.thinking(text),
                    onStep: (step) => panel.step(step),
                    onProgress: (progress) => panel.progress(progress),
                    onSummary: (text) => panel.summary(text),
                    onCitations: (citations) => panel.citations(citations),
                    onExcerpts: (excerpts) => panel.excerpts(excerpts),
                    onDone: (info) => panel.done(info),
                    onError: (code, detail) => {
                        panel.error(`${code}: ${detail}`, code);
                        // The run just proved Ollama is unreachable — re-read health
                        // so the status bar and the Ask notice say so too.
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
                say("open a file in this project first — there is no folder to scope to.")
            );
            return s;
        }
        const rel = path
            .relative(project.root, path.dirname(doc.uri.fsPath))
            .split(path.sep)
            .join("/");
        if (rel === "" || rel.startsWith("..")) {
            void vscode.window.showInformationMessage(
                say("that file is not inside the project, or is at its root.")
            );
            return s;
        }
        return { ...s, include: { ...s.include, paths: [`${rel}/*`] } };
    };

    const onAsk = async (s: AskSubmission): Promise<void> => {
        // "Folder" is resolved here, not in the webview: the webview has no editor
        // API, and mirroring the active editor into it would be a second channel to
        // keep fresh for one button. It is checked before the mode split because the
        // Scope panel now serves both modes — it fills the form in and runs nothing.
        if (s.scopeCurrentFolder === true) {
            const scoped = scopeToCurrentFolder(s);
            askProvider.setScope(scoped.include, scoped.exclude);
            return;
        }
        if (s.mode === "research") {
            // Spread the whole submission minus the fields a research run has no
            // use for, rather than naming the ones it does: `ResearchSubmission` is
            // `AskSubmission` less exactly these four, so a field the form grows
            // arrives here by construction. Copied field by field, `contextRunIds`
            // was simply never listed — the picked reports reached the panel header
            // and the request went out without them, so every run the user gave
            // context to ran with none and said so in its report.
            const {
                mode: _mode,
                text,
                topK: _topK,
                scopeCurrentFolder: _folder,
                ...research
            } = s;
            await startResearch({ question: text, ...research });
            return;
        }
        try {
            const proj = await loadProject();
            await runSearch(api, proj.mindex.guid, proj.root, {
                topK: s.topK,
                query: s.text,
                include: s.include,
                exclude: s.exclude,
                registry: liveSearches,
            });
        } catch (e) {
            if (!isCancellation(e)) {
                await reportError("Search failed", e);
            }
        }
    };

    const askProvider = new AskViewProvider(
        context.extensionUri,
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
        (s: AskSubmission) => void onAsk(s),
        cancelResearch,
        () => void vscode.commands.executeCommand("mindex.openStatus"),
        () => void vscode.commands.executeCommand("mindex.pickResearchContext")
    );
    // Stored reports are served as read-only Markdown documents, so a report can be
    // opened in a tab from anywhere that knows its id — the picker, the History
    // rows, a dependency chip in a live run's header.
    const researchDocs = new ResearchDocumentProvider(() => api);
    context.subscriptions.push(
        vscode.window.registerWebviewViewProvider(AskViewProvider.viewId, askProvider),
        vscode.workspace.registerTextDocumentContentProvider(RESEARCH_SCHEME, researchDocs),
        researchDocs,
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
                    // Only while there is something to say something about: VS Code
                    // shows `message` *instead of* the welcome view when the tree is
                    // empty, and the welcome text is what explains drift in the first
                    // place. Setting this unconditionally would replace it.
                    driftView.message = actionable === 0 ? undefined : DRIFT_MESSAGE;
                    if (actionable === 0) {
                        void vscode.window.setStatusBarMessage(
                            say("index is in sync with the working tree"),
                            5000
                        );
                    }
                }
            );
        } catch (e) {
            await reportError("Drift check failed", e, checkDrift);
        }
    };

    /**
     * The one confirm for dropping files from the index, shared by the two commands
     * that do it. Worth stating that files on disk are untouched: "delete" beside a
     * list of paths reads as a filesystem operation until it says otherwise.
     */
    const confirmOrphanDelete = async (count: number): Promise<boolean> => {
        const confirm = await vscode.window.showWarningMessage(
            `Delete ${count} orphaned file(s) from the index? They are already gone from the working tree; nothing on disk is touched. (Soft delete; GC removes vectors later.)`,
            { modal: true },
            "Delete"
        );
        return confirm === "Delete";
    };

    /** Drop paths from the index. Assumes the caller has already confirmed. */
    const deleteOrphans = async (paths: string[]): Promise<void> => {
        try {
            const proj = await loadProject();
            const n = await api.deleteFiles(proj.mindex.guid, { include: { paths } });
            void vscode.window.showInformationMessage(
                say(`${n} file(s) deleted from the index.`)
            );
            await checkDrift();
            // Same reason as after a reindex, in the other direction: a language may
            // have just lost its last searchable chunk.
            await statusProvider.refresh();
        } catch (e) {
            await reportError("Delete from index failed", e);
        }
    };

    /** Whether *this* window is mid-upload, so a second run can be refused. */
    let reindexRunning = false;

    const reindex = async (
        paths: string[],
        noneMessage: string,
        force = false
    ): Promise<void> => {
        // Re-entry guard. Every reindex entry point funnels through here, and without
        // it a second press starts a *concurrent* run over the same paths: both post
        // the same files, the server's keyed claim answers the loser with a conflict it
        // swallows, and the drift check each one queues at the end races the other's
        // uploads — so the view can settle showing files as still stale that were in
        // fact just indexed. Pressing again looked like the only recourse, which
        // started a third.
        if (reindexRunning) {
            void vscode.window.showInformationMessage(
                say("a reindex is already running — watch the status bar for progress.")
            );
            return;
        }
        // The server is still holding claims. Posting now is not merely redundant: the
        // handler answers 200 with every claimed file *absent* from the response, so the
        // upload finishes instantly, the files are counted "unchanged", and the user is
        // told nothing happened — which is exactly what it looks like, because nothing
        // did. Refusing up front and pointing at the progress row is the honest version.
        if (driftProvider.busyClaims > 0) {
            void vscode.window.showInformationMessage(
                say(
                    `the server is still indexing ${driftProvider.busyClaims} file(s) — ` +
                        "they would be refused as in-flight. Watch the Drift view; it " +
                        "clears when they settle."
                )
            );
            return;
        }
        if (paths.length === 0) {
            if (noneMessage !== "") {
                void vscode.window.showInformationMessage(noneMessage);
            }
            return;
        }
        const proj = await currentProject();
        if (proj === undefined) {
            return;
        }
        const batch = config().get<number>("batchSize", 100);
        reindexRunning = true;
        let summary;
        try {
            summary = await reindexPaths(api, proj.mindex.guid, proj.root, paths, {
                statusBar: indexStatusBar,
                extensionUri: context.extensionUri,
                placement: config().get<IndexingPanelPlacement>("indexingPanel", "beside"),
                openFile: statusActions.openFile,
                batchSize: batch,
                force,
            });
        } finally {
            // Cleared before the drift check below, which draws its own progress in
            // the view title — two indicators for one operation reads as two
            // operations.
            reindexRunning = false;
        }
        if (summary !== undefined) {
            // The drift check runs *before* the summary, not after: it is the only
            // thing that can tell a hash-skipped file from one the server refused as
            // in-flight, and the summary is a claim about which of the two happened.
            await checkDrift();
            const inFlight = driftProvider.allPaths("indexing").length;
            showReindexSummary(summary, inFlight);
            // The same number, from the same source, so the panel's summary and the
            // toast cannot disagree about what the server actually refused.
            IndexingPanel.current?.finishedWithInFlight(inFlight);
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
                say(
                    "nothing selected in Stale/Missing — tick checkboxes first (or run Check Drift)."
                )
            )
        ),

        vscode.commands.registerCommand("mindex.reindexAllDrift", () =>
            reindex(
                driftProvider.allPaths("stale", "missing"),
                say("no stale or missing files — index is in sync.")
            )
        ),

        vscode.commands.registerCommand("mindex.reindexCurrentFile", async () => {
            const doc = vscode.window.activeTextEditor?.document;
            const proj = await currentProject();
            if (proj === undefined) {
                return;
            }
            if (doc === undefined || !doc.uri.fsPath.startsWith(proj.root)) {
                void vscode.window.showInformationMessage(say("no project file is active."));
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
                void vscode.window.showInformationMessage(say("no project file is active."));
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
                void vscode.window.showInformationMessage(say("nothing to index."));
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
                    say("nothing selected in Orphaned.")
                );
                return;
            }
            if (!(await confirmOrphanDelete(paths.length))) {
                return;
            }
            await deleteOrphans(paths);
        }),

        /**
         * Everything Check Drift found, in one press: reindex what is stale or
         * missing, then drop what is orphaned.
         *
         * The order is not arbitrary. Reindexing first means that if the delete half
         * fails (or the user cancels at the confirm), the run still left the index
         * strictly better off — whereas deleting first and then failing to reindex
         * would leave the project with *less* indexed than before it was pressed.
         */
        vscode.commands.registerCommand("mindex.syncAll", async () => {
            const toReindex = driftProvider.allPaths("stale", "missing");
            const orphans = driftProvider.allPaths("orphaned");
            if (toReindex.length === 0 && orphans.length === 0) {
                void vscode.window.showInformationMessage(
                    say("nothing to sync — the index matches the working tree.")
                );
                return;
            }
            // Confirmed only for the destructive half. A reindex is idempotent and
            // costs time; a delete removes what the server holds, and that is the one
            // thing worth a modal — asking about the whole operation would train the
            // user to click through the dialog that actually matters.
            const deleting = orphans.length > 0 && (await confirmOrphanDelete(orphans.length));
            if (toReindex.length > 0) {
                await reindex(toReindex, "");
            }
            if (deleting) {
                await deleteOrphans(orphans);
            }
        }),

        vscode.commands.registerCommand("mindex.cancelIndexing", async () => {
            const paths = driftProvider.allPaths("indexing");
            if (paths.length === 0) {
                void vscode.window.showInformationMessage(say("nothing is in flight."));
                return;
            }
            try {
                const proj = await loadProject();
                const n = await api.cancel(proj.mindex.guid, { include: { paths } });
                void vscode.window.showInformationMessage(
                    say(`cancelled ${n} in-flight file(s) (best-effort).`)
                );
                await checkDrift();
            } catch (e) {
                await reportError("Cancel failed", e);
            }
        }),

        vscode.commands.registerCommand("mindex.refreshStatus", () =>
            statusProvider.refresh()
        ),

        vscode.commands.registerCommand("mindex.openStatus", () => {
            // Revealed first, so the panel shows the previous snapshot while the fetch
            // is in flight rather than a blank tab.
            StatusPanel.showOrReveal(context.extensionUri, statusProvider, statusActions);
            void statusProvider.refresh();
        }),

        // How a panel closed mid-run is brought back, and the only way in at all
        // when `mindex.indexingPanel` is set to `manual`. It renders whatever the
        // last run left, which is the summary until the next one starts.
        vscode.commands.registerCommand("mindex.openIndexing", () =>
            IndexingPanel.showOrReveal(context.extensionUri, statusActions.openFile)
        ),

        vscode.commands.registerCommand("mindex.openResearchHistory", () => {
            ResearchRunsPanel.showOrReveal(
                context.extensionUri,
                () => api,
                () => project?.mindex.guid,
                {
                    useAsContext: (runs) => {
                        askProvider.setContextRuns(runs);
                        // Reveal the form and switch it to Research: a selection made
                        // here is only useful to the mode that can spend it.
                        askProvider.focus("research");
                    },
                    openReport: (run) => {
                        const guid = project?.mindex.guid;
                        if (guid !== undefined) {
                            void openResearchReport(guid, run);
                        }
                    },
                    reAsk: (run) => {
                        const effort =
                            run.effort === "low" || run.effort === "high"
                                ? run.effort
                                : "medium";
                        askProvider.prefill(run.question, effort, run.model);
                        // Attach the report being followed up, which is the point of
                        // re-asking: the next run starts from what this one found
                        // instead of rediscovering it.
                        askProvider.setContextRuns(run.valid ? [run] : []);
                        askProvider.focus("research");
                        if (run.scope !== null) {
                            // See `AskViewProvider.prefill`: the stored scope is prose,
                            // not a selector, so it cannot be restored — say so rather
                            // than silently widening the question.
                            void vscode.window.showInformationMessage(
                                say(
                                    `report #${run.seq} was scoped to ${run.scope}. ` +
                                        "Set the scope again if it mattered."
                                )
                            );
                        }
                    },
                }
            );
        }),

        vscode.commands.registerCommand("mindex.pickResearchContext", async () => {
            const guid = project?.mindex.guid;
            if (guid === undefined) {
                await vscode.window.showInformationMessage(
                    say("no project here yet — create a .mindex file first.")
                );
                return;
            }
            const picked = await pickContextRuns(
                api,
                guid,
                askProvider.currentContextRuns,
                () => void vscode.commands.executeCommand("mindex.openResearchHistory")
            );
            // `undefined` is a dismissed picker and must leave the form alone; an
            // empty array is a deliberate "no context" and must clear it.
            if (picked !== undefined) {
                askProvider.setContextRuns(picked);
                askProvider.focus("research");
            }
        }),

        vscode.commands.registerCommand("mindex.browseResearch", async () => {
            const guid = project?.mindex.guid;
            if (guid === undefined) {
                await vscode.window.showInformationMessage(
                    say("no project here yet — create a .mindex file first.")
                );
                return;
            }
            await browseResearchRuns(api, guid, () => {
                void vscode.commands.executeCommand("mindex.openResearchHistory");
            });
        }),

        vscode.commands.registerCommand("mindex.openResearchReport", async (arg: unknown) => {
            const guid = project?.mindex.guid;
            const run = arg as { id?: unknown; seq?: unknown; title?: unknown } | undefined;
            if (guid === undefined || typeof run?.id !== "string") {
                return;
            }
            await openResearchReport(guid, {
                id: run.id,
                seq: typeof run.seq === "number" ? run.seq : 0,
                title: typeof run.title === "string" ? run.title : "report",
            });
        }),

        vscode.commands.registerCommand("mindex.openSettings", () => openSettings()),

        vscode.commands.registerCommand("mindex.retryAllFailed", async () => {
            try {
                const proj = await loadProject();
                const n = await api.retry(proj.mindex.guid);
                void vscode.window.showInformationMessage(
                    n > 0
                        ? say(
                              `requeued ${n} failed file(s) — the retry worker picks them up within ~60 s.`
                          )
                        : say("no failed files in this project to retry.")
                );
                await statusProvider.refresh();
            } catch (e) {
                await reportError("Retry failed", e);
            }
        }),

        vscode.commands.registerCommand("mindex.retryFile", async (arg: unknown) => {
            // A plain path now: the tree node that used to carry it is gone, and the
            // Status panel posts the string it rendered.
            const filePath = typeof arg === "string" ? arg : undefined;
            if (filePath === undefined || filePath === "") {
                return;
            }
            try {
                const proj = await loadProject();
                const n = await api.retry(proj.mindex.guid, {
                    include: { paths: [filePath] },
                });
                void vscode.window.showInformationMessage(
                    n > 0
                        ? say(`requeued ${filePath}.`)
                        : say(`${filePath} is not failed anymore.`)
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
                // The palette entry point stays deliberately scope-free: it prompts
                // for a query and has no form to read a scope from.
                const topK = config().get<number>("topK", 10);
                await runSearch(api, proj.mindex.guid, proj.root, { topK });
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

    // The background health poll. Everything that already triggers a refresh still
    // does; this only covers the idle window between them, which is precisely when a
    // dependency dies unobserved and the Ask form goes on offering work against it.
    const applyPollInterval = (): void =>
        statusProvider.setPollInterval(config().get<number>("statusPollSeconds", 30));
    applyPollInterval();
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration("mindex.statusPollSeconds")) {
                applyPollInterval();
            }
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

/**
 * The extension's own page in VS Code's Settings editor.
 *
 * The `@ext:` filter is `publisher.name` from `package.json` — the *identifier*, so
 * it stays lowercase while everything the user reads says MINDex. Everything the
 * server needs to be reachable (`serverUrl`, `noVerify`, `caCert`, `apiKey`) lives
 * there, and the panel that reports "unreachable" is exactly where a link to it
 * belongs.
 */
function openSettings(): void {
    void vscode.commands.executeCommand(
        "workbench.action.openSettings",
        "@ext:mindex.mindex-vscode"
    );
}

/**
 * Build the client from the settings.
 *
 * The CA is read here, and a failure is a *warning* — never a throw. This runs at
 * activation and again on every settings change, so a `caCert` naming a file that
 * is not on this machine (a settings profile synced from another one is the way it
 * happens) used to abort activation with a bare ENOENT: every command dead, and
 * `noVerify` unable to help, since the read that failed came first. Skipping the
 * unreadable CA leaves the connection to succeed by whatever other means is
 * configured, and says out loud which path was ignored.
 */
function createApi(): MindexApi {
    const cfg = config();
    const caCert = cfg.get<string>("caCert", "").trim();
    const apiKey = cfg.get<string>("apiKey", "").trim();
    const noVerify = cfg.get<boolean>("noVerify", false);
    let ca: Buffer | undefined;
    if (caCert !== "" && !noVerify) {
        try {
            ca = fsSync.readFileSync(caCert);
        } catch (e) {
            void vscode.window.showWarningMessage(
                say(
                    `cannot read the CA certificate at ${caCert} — ignoring it. ` +
                        `${e instanceof Error ? e.message : String(e)}. ` +
                        "Clear mindex.caCert, point it at a file that exists on this " +
                        "machine, or turn on mindex.noVerify."
                )
            );
        }
    }
    return new MindexApi({
        serverUrl: cfg.get<string>("serverUrl", "https://127.0.0.1:11111"),
        noVerify,
        ca,
        apiKey: apiKey === "" ? undefined : apiKey,
    });
}
