import * as vscode from "vscode";
import * as fs from "node:fs/promises";
import * as fsSync from "node:fs";
import * as path from "node:path";
import { ConfigResponse, MindexApi, ResearchRunSummary, SearchFilter } from "./api";
import { BusyKeys } from "./busy";
import { pickChallengeOptions } from "./challengeFlow";
import { showActiveResearchRuns } from "./activeRunsPick";
import { challengeGuard } from "./shared/runsFormat";
import { MindexFile, parseMindexFile } from "./mindexFile";
import { buildManifest, scanWorkspace } from "./scanner";
import { DRIFT_MESSAGE, DriftTreeProvider } from "./driftView";
import { Availability, StatusMonitor, UNAVAILABLE } from "./statusMonitor";
import { StatusPanel } from "./statusPanel";
import { ResearchRunsPanel } from "./researchRunsPanel";
import { pickContextRuns } from "./researchContextPick";
import {
    openResearchReport,
    RESEARCH_SCHEME,
    ResearchDocumentProvider,
    runIdOf,
} from "./researchDocs";
import { paintStatusBar } from "./statusBar";
import { mintAgentToken } from "./agentToken";
import { forgetIfToken, TOKEN_SCHEME, TokenDocumentProvider } from "./tokenDoc";
import {
    audienceRefusal,
    describeToken,
    mergeAvailability,
    tokenAvailability,
    tokenCovers,
    tokenPermits,
} from "./token";
import { TokenStatusBar } from "./tokenStatusBar";
import { IndexStatusBar } from "./indexStatusBar";
import { IndexingPanel, IndexingPanelPlacement } from "./indexingPanel";
import { reindexPaths, showReindexSummary } from "./indexer";
import { promptForQuery, runSearch, SearchOptions } from "./search";
import { ResearchPanel, ResearchSubmission } from "./researchView";
import { AskSubmission, AskViewProvider } from "./askView";
import { createProjectFile } from "./createProject";
import {
    disposeErrorLog,
    humanize,
    isCancellation,
    logError,
    ProblemError,
    reportError,
} from "./errors";
import { BRAND, say } from "./brand";

interface Project {
    root: string;
    mindex: MindexFile;
}

export function activate(context: vscode.ExtensionContext): void {
    // The bearer token, cached here because `SecretStorage` is async and
    // `createApi` is called from a dozen synchronous places. The cache is the only
    // copy the extension keeps in memory, and it is kept in step by exactly two
    // things: the load below and `onDidChange`, which fires for writes made in
    // *this* window and in every other one sharing the store.
    let token: string | undefined;
    let api = createApi(token);
    const rebuildApi = (): void => {
        api.dispose();
        api = createApi(token);
    };

    const tokenStatus = new TokenStatusBar(
        vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 89),
        config().get<number>("tokenWarningHours", 24) * 60 * 60 * 1000
    );
    context.subscriptions.push(tokenStatus);

    // What the last health refresh said, kept so the token layer can be re-applied
    // without waiting for the next poll. A token stored at 12:00 must not leave the
    // form frozen until 12:00:30.
    let healthAvailability: Availability = { ask: true, research: true };
    const pushAvailability = (): void => {
        const merged = mergeAvailability(
            healthAvailability,
            tokenAvailability(describeToken(token), project?.mindex.guid)
        );
        askProvider.setAvailability(merged);
        if (!merged.ask) {
            abortRunsForDegradation(merged.reason);
        }
    };

    const reloadToken = async (): Promise<void> => {
        try {
            token = await context.secrets.get(TOKEN_SECRET_KEY);
        } catch (e) {
            // A keychain that cannot be read is not a reason to have no extension:
            // an unauthenticated server needs no token at all, so this degrades to
            // "no credential" with a line in the log rather than a failed activation.
            logError("read the stored token", e);
            token = undefined;
        }
        rebuildApi();
        tokenStatus.setToken(token);
        pushAvailability();
    };
    void reloadToken();

    context.subscriptions.push(
        context.secrets.onDidChange((e) => {
            if (e.key === TOKEN_SECRET_KEY) {
                void reloadToken();
            }
        }),
        vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration("mindex")) {
                rebuildApi();
            }
            if (e.affectsConfiguration("mindex.tokenWarningHours")) {
                tokenStatus.setQuietBefore(
                    config().get<number>("tokenWarningHours", 24) * 60 * 60 * 1000
                );
            }
        }),
        // A machine that slept through the scheduled tick wakes with a stale
        // reading; regaining focus is the moment someone is about to look at it.
        vscode.window.onDidChangeWindowState((st) => {
            if (st.focused) {
                tokenStatus.refresh();
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
        // A different GUID can be a different answer to "does your token reach
        // this?" — most sharply right after the welcome button writes a fresh one.
        pushAvailability();
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

    // The last `GET /config` the monitor saw — what the challenge QuickPick reads
    // its effort/model inventory from. Same offering-vs-validating stance as the
    // Ask form: undefined degrades the pick to bare labels, never blocks it.
    let serverConfig: ConfigResponse | undefined;

    const statusProvider = new StatusMonitor(
        () => api,
        () => project?.mindex.guid,
        () => config().get<string>("serverUrl", "https://127.0.0.1:11111"),
        // The health refresh is the only thing that knows what the server can still
        // do; the Ask view is where that costs the user something.
        // The token's own grant is folded in here rather than inside the fetch: the
        // two have different lifetimes (health is polled, a token changes when
        // someone stores one), and merging at the point of use is what lets either
        // one repaint the form on its own.
        (availability) => {
            healthAvailability = availability;
            pushAvailability();
        },
        // Same shape, and for the same reason: the refresh is what knows what the
        // index holds, the Ask view is what has to stop offering the rest.
        (languages) => askProvider.setLanguageInventory(languages),
        // `GET /config` is read here on every refresh rather than once at
        // activation, because `research.models` is no longer static: a model pulled
        // after the window opened has to reach the picker without a reload.
        (cfg) => {
            serverConfig = cfg;
            askProvider.setServerConfig(cfg);
        }
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
        // Awaited, like `mintAgentToken` below and for the same reason: these are
        // writes, the panel greys the button for as long as they are pending, and a
        // `void` released it on the first frame — which is not a slower spinner but
        // no single-flight at all.
        retryAll: async (): Promise<void> => {
            await vscode.commands.executeCommand("mindex.retryAllFailed");
        },
        retryFile: async (p: string): Promise<void> => {
            await vscode.commands.executeCommand("mindex.retryFile", p);
        },
        openFile: (p: string) => {
            if (project !== undefined) {
                void vscode.window.showTextDocument(
                    vscode.Uri.file(path.join(project.root, p))
                );
            }
        },
        openSettings: () => openSettings(),
        // Awaited, unlike its neighbours: the panel greys the button for as long
        // as this is pending, and the dialog behind it is a four-step chain the
        // user is walking. `void`ing it here would release the button on the
        // first frame and invite a second press mid-dialog.
        mintAgentToken: async (): Promise<void> => {
            await vscode.commands.executeCommand("mindex.mintAgentToken");
        },
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

    /**
     * The form's Stop button and the `mindex.researchRunning` context key are the
     * same fact, so they are set in one place.
     *
     * The key exists because Stop is not always reachable. The Ask sidebar is a
     * `WebviewView` without `retainContextWhenHidden`: collapsing it destroys the
     * page, and while `AskFormState` now replays the run into the rebuilt one, a
     * user who has closed the sidebar entirely still has a run and no button. The
     * key is what lets `mindex.cancelResearch` appear in the palette exactly while
     * there is something to cancel, and disappear when there is not.
     */
    /**
     * Resolves once the previous run's connection has finished unwinding.
     *
     * `cancelResearch` clears `researchAbort` synchronously — it must, or Stop would
     * not release the form until the socket closed. But the *server* frees the
     * research slot on disconnect, and that happens some milliseconds later, so a
     * question asked immediately after a Stop could reach a server that still
     * believes its one slot is taken and answer 429 `research.busy`. Awaiting this
     * before launching costs nothing in the ordinary case (it is already resolved)
     * and closes the window in the one case it is not.
     */
    let researchSettled: Promise<void> = Promise.resolve();

    const setResearchRunning = (running: boolean): void => {
        askProvider.setRunning(running);
        void vscode.commands.executeCommand("setContext", "mindex.researchRunning", running);
    };

    const cancelResearch = (): void => {
        researchAbort?.abort(); // closing the connection IS the server-side cancel
        researchAbort = undefined;
        setResearchRunning(false);
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
        setResearchRunning(false);
        liveSearches.clear();

        abort?.abort();
        for (const search of searches) {
            search.abort();
        }

        const detail = reason ?? "a required dependency stopped answering";
        panel?.error(`${detail} — the run was aborted.`, "mindex.degraded");
        void vscode.window.showErrorMessage(
            say(`server health degraded — ${detail}. The run in progress was aborted.`)
        );
    };

    /**
     * The one callback block for both research and challenge streams — extracted
     * so the two entrances cannot drift: a challenge is the same stream with one
     * extra `verdict` frame, which `panel.verdict` already renders.
     */
    const researchCallbacks = (panel: ResearchPanel) => ({
        onThinking: (text: string) => panel.thinking(text),
        onStep: (step: Parameters<ResearchPanel["step"]>[0]) => panel.step(step),
        onProgress: (progress: Parameters<ResearchPanel["progress"]>[0]) =>
            panel.progress(progress),
        onSummary: (text: string) => panel.summary(text),
        onCitations: (citations: Parameters<ResearchPanel["citations"]>[0]) =>
            panel.citations(citations),
        onExcerpts: (excerpts: Parameters<ResearchPanel["excerpts"]>[0]) =>
            panel.excerpts(excerpts),
        onVerdict: (verdict: Parameters<ResearchPanel["verdict"]>[0]) =>
            panel.verdict(verdict),
        onDone: (info: Parameters<ResearchPanel["done"]>[0]) => panel.done(info),
        onError: (code: string, detail: string) => {
            // Passed apart, not concatenated. `ResearchPanel.error` has taken the
            // code as its own argument all along and renders only the detail; this
            // one call site glued them together, so the panel was the last place in
            // the extension showing a user `ollama.unavailable: …` — the exact
            // shape `humanize` exists to prevent. The code still travels, and the
            // webview hangs it on the block's tooltip.
            panel.error(detail, code);
            // The run just proved Ollama is unreachable — re-read health
            // so the status bar and the Ask notice say so too. Deliberately not
            // `ollama.error`, which is Ollama answering *with* an error (usually a
            // model that is not pulled): health comes back green every time, so the
            // refresh would only replace the useful message with a reassuring one.
            if (code === "ollama.unavailable") {
                void statusProvider.refresh();
            }
        },
    });

    const startResearch = async (s: ResearchSubmission): Promise<void> => {
        await researchSettled;
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
        let settle: () => void = () => {};
        researchSettled = new Promise<void>((resolve) => {
            settle = resolve;
        });
        setResearchRunning(true);
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
                researchCallbacks(panel),
                abort.signal
            );
        } catch (e) {
            if (!isCancellation(e)) {
                logError("Research run", e);
                panel.error(humanize(e).text);
                failure = e;
            }
        } finally {
            // Only clear if this run is still the active one (not a newer restart).
            if (researchAbort === abort) {
                researchAbort = undefined;
                setResearchRunning(false);
                // The tab stays open with its finished report, but it is no longer the
                // live run — closing it must not fire a cancel for work already done.
                if (researchPanel === panel) {
                    researchPanel = undefined;
                }
                // The corpus gained a report (or did not, if this run failed — in
                // which case re-reading costs one request and says so honestly).
                // Either way the History panel is describing the corpus as it was.
                ResearchRunsPanel.notifyRunFinished();
            }
            // Unconditional, unlike everything above it: this says the connection
            // is done, which is true whether or not this run is still the current
            // one — and a handle left unresolved would block the next launch for
            // the life of the window.
            settle();
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
     * Launch a challenge against a stored run's report. Shares everything with
     * `startResearch` deliberately: the same single-flight handles (so
     * degradation-abort and Cancel cover challenges for free), the same panel,
     * the same callback block. What differs is the request — the server takes
     * only effort/model/budget/seed; question, scope and context come from the
     * subject.
     */
    const startChallenge = async (subject: ResearchRunSummary): Promise<void> => {
        await researchSettled;
        if (researchAbort !== undefined) {
            void vscode.window.showInformationMessage(
                say("a research run is already in progress — cancel it first.")
            );
            return;
        }
        // The server would refuse both of these with a 400; refusing here spares
        // the QuickPick chain. The summary can be stale, so the server's answer
        // still lands in the panel when it disagrees.
        const guard = challengeGuard(subject);
        if (!guard.ok) {
            void vscode.window.showInformationMessage(say(guard.reason));
            return;
        }
        let proj: Project;
        try {
            proj = await loadProject();
        } catch (e) {
            await reportError("Challenge failed", e);
            return;
        }
        const req = await pickChallengeOptions(subject, serverConfig);
        if (req === undefined) {
            return;
        }

        const panel = new ResearchPanel(
            context.extensionUri,
            `Challenge research #${subject.seq}: ${subject.question}`,
            () => {
                if (researchPanel === panel) {
                    cancelResearch();
                }
            },
            undefined,
            [],
            {
                tabTitle: `Challenge #${subject.seq}: ${subject.title}`,
                isChallenge: true,
            }
        );
        researchPanel = panel;

        const abort = new AbortController();
        researchAbort = abort;
        let settle: () => void = () => {};
        researchSettled = new Promise<void>((resolve) => {
            settle = resolve;
        });
        setResearchRunning(true);
        let failure: unknown;
        try {
            await api.challenge(
                proj.mindex.guid,
                subject.id,
                req,
                researchCallbacks(panel),
                abort.signal
            );
        } catch (e) {
            if (!isCancellation(e)) {
                logError("Research run", e);
                panel.error(humanize(e).text);
                failure = e;
            }
        } finally {
            if (researchAbort === abort) {
                researchAbort = undefined;
                setResearchRunning(false);
                if (researchPanel === panel) {
                    researchPanel = undefined;
                }
                // More than a new row here: the subject's trust badge and its
                // Challenge/Re-check button are exactly what this run just decided,
                // and they are the two things a reader would go and check.
                ResearchRunsPanel.notifyRunFinished();
            }
            // Unconditional, unlike everything above it: this says the connection
            // is done, which is true whether or not this run is still the current
            // one — and a handle left unresolved would block the next launch for
            // the life of the window.
            settle();
        }
        // After the handles are released, like `startResearch` — see the comment
        // there for the un-clicked-toast trap.
        if (failure !== undefined) {
            if (failure instanceof ProblemError && failure.code === "research.busy") {
                await vscode.window.showErrorMessage(
                    say(
                        "all research slots are busy — “MINDex: Active Research Runs” " +
                            "lists them and can cancel one."
                    )
                );
                return;
            }
            await reportError("Challenge failed", failure);
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
    const scopeToCurrentFolder = (s: {
        include?: SearchFilter;
        exclude?: SearchFilter;
    }): { include?: SearchFilter; exclude?: SearchFilter } => {
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

    /**
     * One search at a time, refused rather than queued.
     *
     * The button greys itself the moment this takes the key, but the refusal has
     * to live here: five fast clicks used to be five concurrent requests, five
     * entries in `liveSearches` and five quick picks racing to be the one on
     * screen. Shared with the palette command, so the form and the palette cannot
     * start two either.
     */
    const askKeys = new BusyKeys((m) => {
        const msg = m as { key?: string; busy?: boolean };
        if (typeof msg.key === "string") {
            askProvider.setBusy(msg.key, msg.busy === true);
        }
    });

    /**
     * Run a search under the `submit` key and report a failure only once the key is
     * back.
     *
     * The order is the whole point, and it is `startResearch`'s: an error
     * notification's thenable resolves when the user *dismisses* it, so awaiting one
     * inside `askKeys.run` held `submit` for as long as the toast sat on screen —
     * Submit greyed and spinning over a search that had already failed. Retry
     * re-enters here, so the retried search is single-flighted like any other.
     */
    const guardedSearch = async (opts: Omit<SearchOptions, "registry">): Promise<void> => {
        let failure: unknown;
        await askKeys.run("submit", async () => {
            try {
                const proj = await loadProject();
                await runSearch(api, proj.mindex.guid, proj.root, {
                    ...opts,
                    registry: liveSearches,
                });
            } catch (e) {
                failure = e;
            }
        });
        if (failure === undefined || isCancellation(failure)) {
            return;
        }
        await reportError("Search failed", failure, () => guardedSearch(opts));
    };

    const onAsk = async (s: AskSubmission): Promise<void> => {
        if (s.mode === "research") {
            // Spread the whole submission minus the fields a research run has no
            // use for, rather than naming the ones it does: `ResearchSubmission` is
            // `AskSubmission` less exactly these three, so a field the form grows
            // arrives here by construction. Copied field by field, `contextRunIds`
            // was simply never listed — the picked reports reached the panel header
            // and the request went out without them, so every run the user gave
            // context to ran with none and said so in its report.
            const { mode: _mode, text, topK: _topK, ...research } = s;
            await startResearch({ question: text, ...research });
            return;
        }
        await guardedSearch({
            topK: s.topK,
            query: s.text,
            include: s.include,
            exclude: s.exclude,
        });
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
        () => void vscode.commands.executeCommand("mindex.pickResearchContext"),
        (current) => {
            const scoped = scopeToCurrentFolder(current);
            askProvider.setScope(scoped.include, scoped.exclude);
        }
    );
    // Stored reports are served as read-only Markdown documents, so a report can be
    // opened in a tab from anywhere that knows its id — the picker, the History
    // rows, a dependency chip in a live run's header.
    const researchDocs = new ResearchDocumentProvider(() => api);
    // A freshly minted token is served the same way, and for the opposite reason:
    // not so it can be kept, but so it can be looked at without ever becoming a
    // file. See `tokenDoc.ts`.
    const tokenDocs = new TokenDocumentProvider();
    context.subscriptions.push(
        vscode.window.registerWebviewViewProvider(AskViewProvider.viewId, askProvider),
        vscode.workspace.registerTextDocumentContentProvider(RESEARCH_SCHEME, researchDocs),
        researchDocs,
        vscode.workspace.registerTextDocumentContentProvider(TOKEN_SCHEME, tokenDocs),
        tokenDocs,
        vscode.workspace.onDidCloseTextDocument(forgetIfToken),
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
            await statusProvider.refreshNow();
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
        // A read-only token is a legitimate way to run this extension, and the
        // cost of not saying so here is a batch of uploads that each 403 halfway
        // through — a partial reindex reported as a failure per file, when the
        // single true sentence is that this credential does not index. The button
        // stays live: the explanation lives behind it, and a dead button with no
        // reason is what this replaces.
        if (!tokenPermits(describeToken(token), "index")) {
            void vscode.window.showWarningMessage(
                say(
                    "your token does not carry `index`, so the server would refuse these " +
                        "uploads. Search and Research are unaffected."
                )
            );
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
            await statusProvider.refreshNow();
        }
    };

    context.subscriptions.push(
        // The one command that works *without* a project: it is how you get one.
        vscode.commands.registerCommand("mindex.createProjectFile", () =>
            createProjectFile(reloadProject, (guid) => {
                // A GUID nobody has ever indexed and one this token may not reach
                // are the same 404 by design, so nothing the server says later can
                // tell them apart. Here they can be: the token is in hand and the
                // GUID was written a line ago.
                //
                // No offer to mint a covering token, because there is never one to
                // make: a token that already reaches everything covers this GUID
                // too and never gets here, and one scoped to named projects is
                // refused by `may_mint` when it asks for a project it does not
                // hold. The remedy is genuinely on the server's host.
                if (tokenCovers(describeToken(token), guid)) {
                    return;
                }
                void vscode.window.showWarningMessage(
                    say(
                        `this project's GUID is not in your token, so the server will answer ` +
                            `every request about it as though it did not exist — which is what ` +
                            `it answers for a project nobody has indexed, deliberately, so the ` +
                            `two cannot be told apart. Mint a token naming ${guid} (or one ` +
                            `covering "*") on the server's host.`
                    )
                );
            })
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
                // Reindexing runs its own drift check, which rebuilds this view —
                // so `orphans` now describes the tree as it was two round trips
                // ago. Delete only what the fresh check still calls orphaned: the
                // confirmation bounds the set, and the later check decides.
                const stillOrphaned = new Set(driftProvider.allPaths("orphaned"));
                const condemned = orphans.filter((p) => stillOrphaned.has(p));
                if (condemned.length === 0) {
                    void vscode.window.showInformationMessage(
                        say("nothing left to delete — the reindex accounted for it.")
                    );
                } else {
                    await deleteOrphans(condemned);
                }
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
                // The claim count comes from the status poll, not from the drift
                // check, and `reindex` refuses on it — so without this the user
                // cancels and then cannot reindex until the next tick, which is the
                // one thing they cancelled in order to do.
                await statusProvider.refreshNow();
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
                    challenge: (run) => {
                        void startChallenge(run);
                    },
                    runsDeleted: (ids) => {
                        askProvider.dropContextRuns(ids);
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
                },
                // The panel needs the server's own page ceiling and batch-delete
                // cap to size its paging loops, and reads it live: `/config` is
                // re-read on every status poll, so a restart with new limits
                // reaches the panel without one here.
                () => serverConfig
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

        vscode.commands.registerCommand(
            "mindex.challengeResearchRun",
            async (arg: unknown) => {
                const guid = project?.mindex.guid;
                if (guid === undefined) {
                    await vscode.window.showInformationMessage(
                        say("no project here yet — create a .mindex file first.")
                    );
                    return;
                }
                // Three entry points, three argument shapes: the History panel passes a
                // summary, the streaming panel a run id, the report tab's editor/title
                // button nothing (the id is in the active editor's URI).
                let runId: string | undefined;
                if (typeof arg === "string" && arg !== "") {
                    runId = arg;
                } else if (typeof (arg as { id?: unknown } | undefined)?.id === "string") {
                    runId = (arg as { id: string }).id;
                } else {
                    const uri = vscode.window.activeTextEditor?.document.uri;
                    if (uri?.scheme === RESEARCH_SCHEME) {
                        runId = runIdOf(uri);
                    }
                }
                if (runId === undefined) {
                    return;
                }
                try {
                    // The detail refreshes kind/valid/trust — the pre-check must not
                    // run on whatever stale shape the caller happened to hold.
                    await startChallenge(await api.getResearchRun(guid, runId));
                } catch (e) {
                    await reportError("Challenge failed", e);
                }
            }
        ),

        vscode.commands.registerCommand("mindex.activeResearchRuns", () =>
            showActiveResearchRuns(api)
        ),

        vscode.commands.registerCommand("mindex.openSettings", () => openSettings()),

        vscode.commands.registerCommand("mindex.mintAgentToken", async () => {
            const proj = await currentProject();
            if (proj === undefined) {
                return;
            }
            try {
                await mintAgentToken(api, proj.mindex.guid, path.basename(proj.root));
            } catch (e) {
                // `what` is the operation, and `reportError` appends the sentence —
                // so it must not be a sentence itself. Naming the likely cause here
                // produced "cannot issue a token: the stored token does not carry
                // `mint`: The server refused the request." The cause now lives where
                // it belongs: `humanize`'s 403 branch says what a 403 means for any
                // request, and offers the button that issues a wider credential.
                await reportError(say("could not issue a token"), e);
            }
        }),

        vscode.commands.registerCommand("mindex.setToken", async () => {
            const entered = await promptForToken(token);
            // `undefined` is a dismissed box and must leave the stored token alone;
            // an empty string is a deliberate clear. The same distinction the
            // research-context picker makes, for the same reason.
            if (entered === undefined) {
                return;
            }
            if (entered === "") {
                await context.secrets.delete(TOKEN_SECRET_KEY);
                void vscode.window.showInformationMessage(say("stored token cleared."));
                return;
            }
            const facts = describeToken(entered);
            // Refused before it is stored, not after: the server does not check
            // `aud`, so this token would work, and the mistake it catches — the
            // agent's credential pasted into the editor's keychain — is invisible
            // from every other surface. Overridable, because the label is a hint
            // and the person holding the token may know better than it does.
            const wrongAudience = audienceRefusal(facts);
            if (wrongAudience !== undefined) {
                const STORE = "Store it anyway";
                const choice = await vscode.window.showWarningMessage(
                    say(wrongAudience),
                    { modal: true },
                    STORE
                );
                if (choice !== STORE) {
                    return;
                }
            }
            await context.secrets.store(TOKEN_SECRET_KEY, entered);
            // `onDidChange` refreshes the cache and the indicator; this only says
            // what the token turned out to be, which is the half a paste can get
            // wrong without any error — the wrong token is a perfectly valid one.
            const scope =
                facts.projects === undefined
                    ? ""
                    : ` — ${facts.projects.join(", ")} / ${(facts.actions ?? []).join(", ")}`;
            const life =
                facts.expiresAtMs === undefined
                    ? " It does not expire."
                    : ` It expires ${new Date(facts.expiresAtMs).toLocaleString()}.`;
            void vscode.window.showInformationMessage(say(`token stored${scope}.${life}`));
        }),

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
                await statusProvider.refreshNow();
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
                await statusProvider.refreshNow();
            } catch (e) {
                await reportError("Retry failed", e);
            }
        }),

        vscode.commands.registerCommand("mindex.research", () =>
            askProvider.focus("research")
        ),

        // The palette's copy of the form's Stop button, gated by
        // `mindex.researchRunning` so it is offered exactly while there is a run.
        // Stop is the primary control and stays so; this is the way out for a user
        // whose sidebar is closed, which is a state the form cannot draw itself out
        // of. It reports rather than staying silent when there is nothing to stop:
        // a palette command that does nothing at all reads as broken.
        vscode.commands.registerCommand("mindex.cancelResearch", () => {
            if (researchAbort === undefined) {
                void vscode.window.showInformationMessage(
                    say("no research run is in progress.")
                );
                return;
            }
            cancelResearch();
        }),

        vscode.commands.registerCommand("mindex.ask", () => askProvider.focus()),

        vscode.commands.registerCommand("mindex.search", async () => {
            // Prompted before the key is taken, not inside `runSearch`: the box is
            // open for as long as the user is typing, and holding `submit` across it
            // would make the form's Submit dead for that whole time — for a search
            // that has not started and may never be asked for.
            const query = await promptForQuery();
            if (query === undefined) {
                return;
            }
            // `guardedSearch` takes the same key the form's Submit does: the palette
            // and the form are two doors into one search, and a keybinding pressed
            // while the form's search is in flight must not open a second quick pick.
            // The palette entry point stays deliberately scope-free — it has no form
            // to read a scope from.
            await guardedSearch({ topK: config().get<number>("topK", 10), query });
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

export function deactivate(): void {
    // Everything else is a `context.subscriptions` entry; the error log is not,
    // because it is created lazily by the first failure and most sessions never
    // have one.
    disposeErrorLog();
}

function config(): vscode.WorkspaceConfiguration {
    return vscode.workspace.getConfiguration("mindex");
}

/**
 * The extension's own page in VS Code's Settings editor.
 *
 * The `@ext:` filter is `publisher.name` from `package.json` — the *identifier*, so
 * it stays lowercase while everything the user reads says MINDex. Everything the
 * server needs to be reachable (`serverUrl`, `noVerify`, `caCert`) lives there,
 * and the panel that reports "unreachable" is exactly where a link to it belongs.
 * The bearer token is the one exception and is deliberately not a setting — see
 * `mindex.setToken`.
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
/**
 * Where the bearer token lives, and the reason it is not a setting.
 *
 * `SecretStorage` is the platform keychain. A `mindex.token` setting would sit in
 * a plaintext `settings.json` and — being a string setting — would be carried to
 * every other machine by Settings Sync, which for a credential is a copy nobody
 * decided to make. That is what `mindex.apiKey` used to do, and removing it is
 * half of why this key exists.
 *
 * It is deliberately **not** the only home for a credential on this machine: the
 * CLI tools read `~/.config/mindex/credentials.toml`, which no extension's
 * keychain can be, since a store one application can read is not an answer to
 * "who holds the credential". The two are separate copies on purpose, and the
 * runbook says to mint one token per holder rather than share one.
 */
const TOKEN_SECRET_KEY = "mindex.token";

/**
 * The paste box. Returns `undefined` when dismissed, `""` to clear.
 *
 * `password: true` keeps the value out of the screen and out of the input's own
 * history. The validation is deliberately shallow — shape, never validity: this
 * process cannot check a signature, and a box that refused a token the server
 * would have accepted is a worse failure than one that lets a bad paste through
 * to a 401 that says exactly what is wrong.
 */
async function promptForToken(current: string | undefined): Promise<string | undefined> {
    const facts = describeToken(current);
    const held =
        current === undefined
            ? "none stored"
            : facts.subject !== undefined
              ? `replacing the one issued to ${facts.subject}`
              : "replacing the stored one";
    const entered = await vscode.window.showInputBox({
        title: say("set the bearer token"),
        prompt: `Mint one on the server's host with \`mindex mint-token\` (${held}). Leave empty to clear.`,
        password: true,
        ignoreFocusOut: true,
        validateInput: (value) => {
            const v = value.trim();
            if (v === "" || v.split(".").length === 3) {
                return undefined;
            }
            return "that does not look like a token — mindex mint-token prints one line of three dot-separated parts";
        },
    });
    return entered?.trim();
}

function createApi(token: string | undefined): MindexApi {
    const cfg = config();
    const caCert = cfg.get<string>("caCert", "").trim();
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
        token,
        timeoutMs: cfg.get<number>("requestTimeoutSeconds", 15) * 1000,
        streamIdleMs: cfg.get<number>("streamIdleTimeoutSeconds", 180) * 1000,
    });
}
