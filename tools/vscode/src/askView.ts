import * as vscode from "vscode";
import {
    ConfigResponse,
    ResearchBudget,
    ResearchEffort,
    ResearchRunSummary,
    SearchFilter,
} from "./api";
import { say } from "./brand";
import { ALL_LANGUAGES } from "./languages";
import { AskMode } from "./shared/askFields";
import { Availability } from "./statusFetch";
import { asString, mediaRoots, readMedia, renderPage } from "./webview";

export type { AskMode } from "./shared/askFields";

/** What the sidebar form submits. `mode` decides which half of the fields matter. */
export interface AskSubmission {
    mode: AskMode;
    /** The query (search) or question (research) — the one field both modes share. */
    text: string;
    /** Search only. */
    topK: number;
    /** Research only. */
    effort: ResearchEffort;
    /** Research only; empty means the server's default. */
    model: string;
    /** Research only; only the axes the user overrode. Absent = the effort preset. */
    budget?: ResearchBudget;
    /**
     * The files this query may see, in **both** modes. `/search` and `/research` take
     * the same selector, and Research enforces it server-side on every lookup — so
     * for a research run it bounds the answer and not just the ranking, which is why
     * a scoped report may not say "nowhere in this project".
     */
    include?: SearchFilter;
    exclude?: SearchFilter;
    /**
     * The user asked to scope to the folder of the file they are looking at. Resolved
     * by the extension, which has the editor API this webview does not.
     */
    scopeCurrentFolder?: boolean;
    /**
     * Research only: stored runs whose reports the server hands the model as
     * background before it plans. Picked in Research History.
     *
     * Read from the provider's own cache rather than from the webview message: these
     * are ids the user never typed, and the form is an input surface for the question,
     * not an authority on which runs exist.
     */
    contextRunIds?: string[];
}

/**
 * The **Ask** sidebar: one query box under a Search/Research switch, with a shared
 * Scope panel and a mode-specific option row.
 *
 * Purely an input surface. Results deliberately do *not* render here — search results
 * stay in the QuickPick, which live-previews each hit in the editor and restores the
 * prior state on Esc, and research streams into its own panel. A list crammed into a
 * 300px column would be a downgrade of both.
 *
 * The form itself is built in the webview from `ASK_FIELDS` (`shared/askFields.ts`);
 * this class is host wiring only — post the config, decode the submission, forward a
 * handful of message types. It used to be 938 lines, most of them an inline
 * HTML/CSS/JS template in which the same ten fields were enumerated by hand five
 * times.
 */
export class AskViewProvider implements vscode.WebviewViewProvider {
    public static readonly viewId = "mindexAsk";

    private view?: vscode.WebviewView;
    /** A mode requested before the view existed; applied once it resolves. */
    private pendingMode?: AskMode;
    /**
     * What the server can currently be asked for; replayed into a new view.
     *
     * Optimistic until the first health refresh lands: a form that starts disabled and
     * enables a second later reads as broken, while one that starts enabled and
     * disables costs at most a request the server answers with its own error.
     */
    private availability: Availability = { ask: true, research: true };
    /** Last known `GET /config`; replayed into a new view. */
    private serverConfig?: ConfigResponse;
    /**
     * Languages this project has something searchable in (`GET /projects/{guid}`).
     * `undefined` = not known yet, which is *not* the same as `[]`; see
     * [`pickerLanguages`].
     */
    private inventory?: string[];
    /**
     * Stored runs the user picked in Research History, to be handed to the next
     * question as background. Cached here and replayed on resolve for the same reason
     * the languages and the model list are: a reopened sidebar starts from the default
     * HTML, and a selection that took several clicks to build must survive that.
     */
    private contextRuns: ResearchRunSummary[] = [];

    constructor(
        private readonly extensionUri: vscode.Uri,
        private readonly defaultModel: () => string,
        private readonly defaultTopK: () => number,
        /**
         * The project's standing scope from `.mindex`, used to prefill the Scope
         * panel. Prefilled and not silently applied: a boundary the user cannot see
         * is one they will blame the report for.
         */
        private readonly defaultScope: () => {
            include: string[];
            exclude: string[];
            languages: string[];
        },
        private readonly onSubmit: (s: AskSubmission) => void,
        private readonly onCancel: () => void,
        private readonly onOpenStatus: () => void,
        /**
         * Open the context picker. Lives in the extension because it needs the API
         * client and the project guid, neither of which a view provider owns — and
         * because the same picker is reachable from a command.
         */
        private readonly onPickContext: () => void
    ) {}

    resolveWebviewView(view: vscode.WebviewView): void {
        this.view = view;
        view.webview.options = {
            enableScripts: true,
            // Without these every asset 404s and the sidebar is silently blank.
            localResourceRoots: mediaRoots(this.extensionUri),
        };
        view.webview.html = renderPage(view.webview, this.extensionUri, {
            body: readMedia(this.extensionUri, "ask.html"),
            styles: ["common.css", "lang.css", "ask.css"],
            modules: ["js/ask.js"],
            codicons: true,
            data: {
                defaultModel: this.defaultModel(),
                defaultTopK: this.defaultTopK(),
                languages: this.pickerLanguages,
            },
        });

        // A view resolved after the last health refresh (reopened sidebar, window
        // reload) starts from the default HTML, so the known state is replayed.
        if (this.pendingMode !== undefined) {
            void view.webview.postMessage({ type: "mode", mode: this.pendingMode });
            this.pendingMode = undefined;
        }
        if (!this.availability.ask || !this.availability.research) {
            this.postAvailability();
        }
        if (this.serverConfig !== undefined) {
            this.postConfig(this.serverConfig);
        }
        if (this.contextRuns.length > 0) {
            this.postContextRuns();
        }

        view.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
            switch (msg.type) {
                case "submit":
                    this.handleSubmit(msg);
                    break;
                case "cancel":
                    this.onCancel();
                    break;
                case "openStatus":
                    this.onOpenStatus();
                    break;
                case "scopeDefaults":
                    this.postScopeDefaults();
                    break;
                case "pickContext":
                    this.onPickContext();
                    break;
                case "contextRuns": {
                    // The form can only ever *remove* runs — they are chosen in the
                    // Research History panel — so this filters the cache rather than
                    // trusting the message to name them.
                    const keep = new Set(
                        Array.isArray(msg.ids) ? (msg.ids as unknown[]).map(String) : []
                    );
                    this.contextRuns = this.contextRuns.filter((r) => keep.has(r.id));
                    break;
                }
            }
        });
    }

    private handleSubmit(msg: Record<string, unknown>): void {
        const mode: AskMode = msg.mode === "search" ? "search" : "research";
        const text = asString(msg.text).trim();
        // "Folder" is a scope action, not a query, so it must work with an empty box —
        // it fills the form in rather than running anything.
        if (text === "" && msg.scopeCurrentFolder !== true) {
            void vscode.window.showInformationMessage(
                mode === "search"
                    ? say("enter a search query first.")
                    : say("enter a research question first.")
            );
            return;
        }
        const effortRaw = asString(msg.effort, "medium");
        const effort: ResearchEffort =
            effortRaw === "low" || effortRaw === "high" ? effortRaw : "medium";
        const topK = Number(msg.topk);
        this.onSubmit({
            mode,
            text,
            topK: Number.isFinite(topK) && topK > 0 ? Math.floor(topK) : this.defaultTopK(),
            effort,
            model: asString(msg.model).trim(),
            budget: readBudget(msg),
            ...readScope(msg),
            scopeCurrentFolder: msg.scopeCurrentFolder === true,
            contextRunIds:
                this.contextRuns.length > 0 ? this.contextRuns.map((r) => r.id) : undefined,
        });
    }

    /**
     * Write a resolved scope back into the form.
     *
     * Used by the "Folder" button, which the host resolves: the form is the source of
     * truth for what the next query will be given, so a scope computed elsewhere has
     * to land back in the fields the user can see and edit.
     */
    setScope(include?: SearchFilter, exclude?: SearchFilter): void {
        void this.view?.webview.postMessage({
            type: "scope",
            include: (include?.paths ?? []).join(", "),
            exclude: (exclude?.paths ?? []).join(", "),
            languages: (include?.programming_languages ?? []).join(","),
        });
    }

    /**
     * Put a question back in the form, with the settings it was asked under.
     *
     * Scope is **not** restored, and that is a limitation rather than an oversight:
     * a stored run carries its scope only as the sentence it was described to the
     * model with (`ResearchRunDetail.scope`), not as the selector that produced it,
     * so re-applying it would mean guessing globs from prose. The caller states the
     * original scope instead, and the user re-enters it if it mattered.
     */
    prefill(question: string, effort: ResearchEffort, model: string): void {
        void this.view?.webview.postMessage({
            type: "prefill",
            question,
            effort,
            model,
        });
    }

    /** Push the project's `.mindex` scope into the form's Scope panel. */
    private postScopeDefaults(): void {
        const scope = this.defaultScope();
        void this.view?.webview.postMessage({
            type: "scope",
            include: scope.include.join(", "),
            exclude: scope.exclude.join(", "),
            languages: scope.languages.join(","),
        });
    }

    focus(mode?: AskMode): void {
        // The first `mindex.research` of a session may run before the view has ever
        // been resolved, so remember the mode instead of posting into the void.
        if (mode !== undefined) {
            this.pendingMode = mode;
        }
        void vscode.commands.executeCommand(`${AskViewProvider.viewId}.focus`);
        if (mode !== undefined && this.view !== undefined) {
            void this.view.webview.postMessage({ type: "mode", mode });
            this.pendingMode = undefined;
        }
    }

    /** Research only: disables submit and enables Stop while a run is in flight. */
    setRunning(running: boolean): void {
        void this.view?.webview.postMessage({ type: "running", running });
    }

    /**
     * What the server can currently be asked for.
     *
     * This *does* disable controls, which the Ollama notice deliberately did not. The
     * argument for a notice-only warning was that health is a snapshot and a stale one
     * should not block a run — but that only holds while the snapshot is stale by
     * minutes. It is now refreshed on a timer, and a form whose Submit is a round-trip
     * to a 503 is not offering a choice, it is offering a delay.
     */
    setAvailability(availability: Availability): void {
        this.availability = availability;
        this.postAvailability();
    }

    private postAvailability(): void {
        void this.view?.webview.postMessage({
            type: "availability",
            ...this.availability,
        });
    }

    /**
     * `GET /config`: the effort ladder, the override ceilings, the model list and the
     * search bounds. The form's sliders and labels are built from it rather than from
     * numbers written here — three separate hard-coded copies of the ladder had each
     * drifted from the server's before it was published, and the result field's
     * `max="50"` disagreed with a real ceiling of 100.
     */
    setServerConfig(config: ConfigResponse): void {
        this.serverConfig = config;
        this.postConfig(config);
    }

    private postConfig(config: ConfigResponse): void {
        void this.view?.webview.postMessage({
            type: "config",
            research: config.research,
            search: config.search,
        });
    }

    /**
     * The languages this project actually has searchable content in, so the pickers
     * offer what the index holds instead of every language the server supports.
     * `undefined` = unknown (server down, no project, older server).
     *
     * Pushed as a message rather than re-rendering the HTML: a re-render would throw
     * away the half-typed question, the restored form state and a running run's Stop
     * button — and this fires on every status refresh.
     */
    /**
     * Offer these stored runs as context for the next question.
     *
     * Replaces rather than appends: the panel sends its whole selection every time, so
     * appending would make unchecking a run in the panel leave it silently attached
     * here.
     */
    setContextRuns(runs: readonly ResearchRunSummary[]): void {
        this.contextRuns = [...runs];
        this.postContextRuns();
    }

    /** What is currently attached — the picker seeds its selection from this. */
    get currentContextRuns(): readonly ResearchRunSummary[] {
        return this.contextRuns;
    }

    /**
     * Only the fields the chips need. The webview is sent a projection rather than
     * the summaries: it renders a label and two state marks, and shipping the whole
     * record (report-less but still ~20 fields each) through `postMessage` on every
     * status refresh buys nothing.
     */
    private postContextRuns(): void {
        void this.view?.webview.postMessage({
            type: "contextRuns",
            runs: this.contextRuns.map((r) => ({
                id: r.id,
                seq: r.seq,
                title: r.title,
                stale: r.stale,
                valid: r.valid,
            })),
        });
    }

    setLanguageInventory(languages: string[] | undefined): void {
        this.inventory = languages;
        void this.view?.webview.postMessage({
            type: "languages",
            languages: this.pickerLanguages,
        });
    }

    /**
     * What the pickers offer. Falls back to the full supported list when the inventory
     * is unknown **or empty**: an empty picker is a dead form, while a superset merely
     * lets a filter match nothing — and the server answers that with a 404 either way.
     */
    private get pickerLanguages(): readonly string[] {
        return this.inventory === undefined || this.inventory.length === 0
            ? ALL_LANGUAGES
            : this.inventory;
    }
}

/**
 * The budget overrides the user actually set. An axis left on its effort preset is
 * left out entirely rather than sent as 0 — absent means "the effort preset", while 0
 * is a value the server rejects. Out-of-range values are *not* clamped here: the
 * server owns the ceilings, and clamping would silently run something other than what
 * was asked for.
 *
 * The slider expresses "unset" as an empty string, which is byte-for-byte what the
 * blank number field it replaced sent — so this function did not have to change when
 * the control did.
 */
function readBudget(msg: Record<string, unknown>): ResearchBudget | undefined {
    const num = (v: unknown, scale = 1): number | undefined => {
        const s = asString(v).trim();
        if (s === "") {
            return undefined;
        }
        const n = Number(s);
        return Number.isFinite(n) && n > 0 ? Math.floor(n) * scale : undefined;
    };
    const budget: ResearchBudget = {
        max_seconds: num(msg.bseconds),
        max_tokens: num(msg.btokens, 1000),
        max_steps: num(msg.bsteps),
    };
    const named = Object.fromEntries(
        Object.entries(budget).filter(([, v]) => v !== undefined)
    );
    return Object.keys(named).length === 0 ? undefined : named;
}

/**
 * The scope the user set, as the server's selector shape. Applies to **both** modes.
 *
 * Languages are checked against `ALL_LANGUAGES` — a stale value from restored webview
 * state is dropped here rather than becoming a 400.
 *
 * Deliberately **not** checked against the project's live inventory, even though that
 * is what the chips now offer: the inventory is an availability hint, not a validity
 * contract. A language indexed one second after the last stats fetch is a legitimate
 * value, and dropping the user's explicit selection would silently run a different
 * query than the one they asked for. Offering is inventory-driven; validating is not.
 * That held when only Research had a language filter, and it still holds now that
 * Search shares this one.
 *
 * Globs are passed through unchanged: they are evaluated by SQLite `GLOB`
 * server-side, and translating them to `.mindex`'s stricter dialect would make this
 * the fifth glob dialect in the project.
 */
function readScope(msg: Record<string, unknown>): {
    include?: SearchFilter;
    exclude?: SearchFilter;
} {
    const globs = (v: unknown): string[] =>
        asString(v)
            .split(/[,\n]/)
            .map((g) => g.trim())
            .filter((g) => g !== "");
    const langs = asString(msg.slangs)
        .split(",")
        .map((l) => l.trim())
        .filter((l) => ALL_LANGUAGES.includes(l));
    const filter = (
        paths: string[],
        programming_languages: string[]
    ): SearchFilter | undefined => {
        const f: SearchFilter = {};
        if (paths.length > 0) {
            f.paths = paths;
        }
        if (programming_languages.length > 0) {
            f.programming_languages = programming_languages;
        }
        return Object.keys(f).length === 0 ? undefined : f;
    };
    // Languages ride on `include` only: `exclude`'s language list would mean "drop
    // these languages", which the UI does not offer and which is a different question.
    const include = filter(globs(msg.sinclude), langs);
    const exclude = filter(globs(msg.sexclude), []);
    return {
        ...(include === undefined ? {} : { include }),
        ...(exclude === undefined ? {} : { exclude }),
    };
}
