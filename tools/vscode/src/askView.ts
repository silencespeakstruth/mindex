import * as vscode from "vscode";
import { ResearchBudget, ResearchConfigInfo, ResearchEffort, SearchFilter } from "./api";
import { ALL_LANGUAGES } from "./languages";

export type AskMode = "search" | "research";

/** What the sidebar form submits. `mode` decides which half of the fields matter. */
export interface AskSubmission {
    mode: AskMode;
    /** The query (search) or question (research) — the one field both modes share. */
    text: string;
    /** Search only. */
    topK: number;
    /** Search only; empty means no language filter. */
    language: string;
    /** Research only. */
    effort: ResearchEffort;
    /** Research only; empty means the server's default. */
    model: string;
    /** Research only; only the axes the user overrode. Absent = the effort preset. */
    budget?: ResearchBudget;
    /**
     * Research only: the files the run may see. The server enforces it on every
     * lookup, so this is a hard boundary rather than a ranking hint. Absent = the
     * whole project.
     */
    include?: SearchFilter;
    exclude?: SearchFilter;
    /**
     * Research only: the user asked to scope the run to the folder of the file they
     * are looking at. Resolved by the extension, which has the editor API this
     * webview does not.
     */
    scopeCurrentFolder?: boolean;
}

/**
 * The "Ask mindex" sidebar section: one query box under a Search/Research segmented
 * control, with the option row swapping to match the mode. Purely an input surface —
 * the extension wires `onSubmit`/`onCancel` and drives `setRunning`.
 *
 * Results deliberately do *not* render here. Search results stay in the QuickPick,
 * which live-previews each hit in the editor and restores the prior state on Esc;
 * a list crammed into a narrow sidebar would be a downgrade. Research streams into
 * its own panel, as before. The unification is of the *entry point*, which is what
 * was inconsistent — Research was a form, Search was a command with no visible home.
 */
export class AskViewProvider implements vscode.WebviewViewProvider {
    public static readonly viewId = "mindexAsk";

    private view?: vscode.WebviewView;
    /** A mode requested before the view existed; applied once it resolves. */
    private pendingMode?: AskMode;
    /** Last known state of the server's optional Ollama; replayed into a new view. */
    private researchAvailable = true;
    /** Last known research budgets from `GET /config`; replayed into a new view. */
    private researchConfig?: ResearchConfigInfo;
    /**
     * Languages this project has something searchable in (`GET /projects/{guid}`).
     * `undefined` = not known yet, which is *not* the same as `[]`; see
     * [`pickerLanguages`].
     */
    private inventory?: string[];

    constructor(
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
        private readonly onCancel: () => void
    ) {}

    resolveWebviewView(view: vscode.WebviewView): void {
        this.view = view;
        view.webview.options = { enableScripts: true };
        view.webview.html = this.html();
        if (this.pendingMode !== undefined) {
            void view.webview.postMessage({ type: "mode", mode: this.pendingMode });
            this.pendingMode = undefined;
        }
        // A view resolved after the last health refresh (reopened sidebar, window
        // reload) starts from the default HTML, so the known state is replayed.
        if (!this.researchAvailable) {
            void view.webview.postMessage({ type: "researchAvailable", available: false });
        }
        if (this.researchConfig !== undefined) {
            void view.webview.postMessage({
                type: "researchConfig",
                info: this.researchConfig,
            });
        }
        if (this.inventory !== undefined) {
            void view.webview.postMessage({
                type: "languages",
                languages: this.pickerLanguages,
            });
        }
        view.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
            if (msg.type === "submit") {
                const mode: AskMode = msg.mode === "search" ? "search" : "research";
                const text = asString(msg.text).trim();
                if (text === "") {
                    void vscode.window.showInformationMessage(
                        mode === "search"
                            ? "mindex: enter a search query first."
                            : "mindex: enter a research question first."
                    );
                    return;
                }
                const effortRaw = asString(msg.effort, "medium");
                const effort: ResearchEffort =
                    effortRaw === "low" || effortRaw === "high" ? effortRaw : "medium";
                const topK = Number(msg.topK);
                const language = asString(msg.lang).trim();
                this.onSubmit({
                    mode,
                    text,
                    language: ALL_LANGUAGES.includes(language) ? language : "",
                    topK:
                        Number.isFinite(topK) && topK > 0
                            ? Math.floor(topK)
                            : this.defaultTopK(),
                    effort,
                    model: asString(msg.model).trim(),
                    budget: readBudget(msg),
                    ...readScope(msg),
                    scopeCurrentFolder: msg.scopeCurrentFolder === true,
                });
            } else if (msg.type === "cancel") {
                this.onCancel();
            } else if (msg.type === "scopeDefaults") {
                this.postScopeDefaults();
            }
        });
    }

    /**
     * Write a resolved scope back into the form.
     *
     * Used by the "this folder" button, which the host resolves: the form is the
     * source of truth for what the next run will be given, so a scope computed
     * elsewhere has to land back in the fields the user can see and edit.
     */
    setScope(include?: SearchFilter, exclude?: SearchFilter): void {
        void this.view?.webview.postMessage({
            type: "scope",
            include: (include?.paths ?? []).join(", "),
            exclude: (exclude?.paths ?? []).join(", "),
            languages: (include?.programming_languages ?? []).join(","),
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

    /** Research only: disables submit and enables Cancel while a run is in flight. */
    setRunning(running: boolean): void {
        void this.view?.webview.postMessage({ type: "running", running });
    }

    /**
     * Research only: whether the server's (optional) Ollama answered its last health
     * check. Shown as a notice rather than a disabled button — health is a snapshot,
     * possibly seconds stale, and blocking a run on it would be worse than letting it
     * fail with the server's own error.
     */
    setResearchAvailable(available: boolean): void {
        this.researchAvailable = available;
        void this.view?.webview.postMessage({ type: "researchAvailable", available });
    }

    /**
     * The server's effort ladder and override ceilings (`GET /config`). The labels
     * are built from it rather than written here: three separate copies of these
     * numbers had drifted from the server's before it was published.
     */
    setResearchConfig(info: ResearchConfigInfo): void {
        this.researchConfig = info;
        void this.view?.webview.postMessage({ type: "researchConfig", info });
    }

    /**
     * The languages this project actually has searchable content in, so the pickers
     * offer what the index holds instead of every language the server supports.
     * `undefined` = unknown (server down, no project, older server).
     *
     * Pushed as a message rather than re-rendering the HTML: a re-render would throw
     * away the half-typed question, the restored form state and a running run's
     * Cancel button — and this now fires on every status refresh.
     */
    setLanguageInventory(languages: string[] | undefined): void {
        this.inventory = languages;
        void this.view?.webview.postMessage({
            type: "languages",
            languages: this.pickerLanguages,
        });
    }

    /**
     * What the pickers offer. Falls back to the full supported list when the
     * inventory is unknown **or empty**: an empty picker is a dead form, while a
     * superset merely lets a filter match nothing — and the server answers that with
     * a 404 either way.
     */
    private get pickerLanguages(): readonly string[] {
        return this.inventory === undefined || this.inventory.length === 0
            ? ALL_LANGUAGES
            : this.inventory;
    }

    private html(): string {
        const nonce = makeNonce();
        const langOptions = this.pickerLanguages
            .map((l) => `<option value="${escapeHtml(l)}">${escapeHtml(l)}</option>`)
            .join("");
        // Theme-aware via VS Code's CSS variables; no external resources.
        return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
<style>
    body {
        padding: 12px 12px 16px;
        font-family: var(--vscode-font-family);
        font-size: var(--vscode-font-size);
        color: var(--vscode-foreground);
    }
    .stack { display: flex; flex-direction: column; gap: 9px; }
    svg { flex: none; }

    /* ── mode switch ────────────────────────────────────────────────────────
       Two halves of one pill. The border falls back through three tokens: some
       themes (Catppuccin among them) leave --vscode-input-border unset, and a
       control with no border on a same-coloured background is invisible — which
       is exactly how this view first read as "there is no textbox". */
    .segmented {
        display: flex;
        gap: 2px;
        padding: 2px;
        border-radius: 6px;
        background: var(--vscode-editorWidget-background,
                    var(--vscode-input-background, rgba(128,128,128,0.12)));
        border: 1px solid var(--vscode-widget-border,
                var(--vscode-input-border, rgba(128,128,128,0.35)));
    }
    .segmented button {
        flex: 1;
        display: flex;
        align-items: center;
        justify-content: center;
        gap: 5px;
        padding: 5px 6px;
        border: none;
        border-radius: 4px;
        cursor: pointer;
        font-family: inherit;
        font-size: 12px;
        background: transparent;
        color: var(--vscode-foreground);
        opacity: 0.7;
        transition: background 90ms ease, opacity 90ms ease;
    }
    .segmented button:hover { background: var(--vscode-toolbar-hoverBackground); opacity: 1; }
    .segmented button[aria-selected="true"] {
        background: var(--vscode-button-background);
        color: var(--vscode-button-foreground);
        opacity: 1;
        font-weight: 600;
    }

    /* ── inputs ─────────────────────────────────────────────────────────── */
    textarea, input, select {
        width: 100%;
        box-sizing: border-box;
        font-family: inherit;
        font-size: var(--vscode-font-size);
        background: var(--vscode-input-background);
        color: var(--vscode-input-foreground);
        border: 1px solid var(--vscode-input-border, rgba(128,128,128,0.4));
        border-radius: 4px;
        padding: 6px 7px;
    }
    textarea { resize: vertical; min-height: 68px; line-height: 1.45; }
    textarea:focus-visible, input:focus-visible, select:focus-visible,
    button:focus-visible {
        outline: 1px solid var(--vscode-focusBorder);
        outline-offset: -1px;
        border-color: var(--vscode-focusBorder);
    }
    ::placeholder { color: var(--vscode-input-placeholderForeground); opacity: 1; }

    /* ── option row ─────────────────────────────────────────────────────── */
    .options { display: flex; gap: 7px; align-items: center; }
    .options label {
        font-size: 11px;
        color: var(--vscode-descriptionForeground, var(--vscode-foreground));
        white-space: nowrap;
    }
    .options .grow { flex: 1; min-width: 0; }
    .options input[type="number"] { width: 54px; }
    .options select { width: auto; flex: 1; min-width: 0; }
    [hidden] { display: none !important; }

    /* ── budget overrides (collapsed: the presets are the normal path) ────── */
    .budget { display: block; }
    .budget summary { cursor: pointer; font-size: 11px; opacity: 0.8; }
    .budget .row { display: flex; gap: 7px; align-items: center; margin-top: 6px; }
    .budget .row input[type="number"] { width: 100%; min-width: 0; }
    .budget .hint {
        font-size: 11px;
        margin-top: 5px;
        color: var(--vscode-descriptionForeground, var(--vscode-foreground));
    }

    /* ── actions ────────────────────────────────────────────────────────── */
    .actions { display: flex; gap: 7px; margin-top: 1px; }
    .actions button {
        flex: 1;
        display: flex;
        align-items: center;
        justify-content: center;
        gap: 6px;
        padding: 7px 10px;
        border: 1px solid transparent;
        border-radius: 4px;
        cursor: pointer;
        font-family: inherit;
        font-size: 13px;
        font-weight: 500;
        background: var(--vscode-button-background);
        color: var(--vscode-button-foreground);
        transition: background 90ms ease, opacity 90ms ease;
    }
    .actions button:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); }
    .actions button:active:not(:disabled) { transform: translateY(0.5px); }
    .actions button.secondary {
        flex: 0 0 auto;
        background: var(--vscode-button-secondaryBackground);
        color: var(--vscode-button-secondaryForeground);
        border-color: var(--vscode-widget-border,
                      var(--vscode-input-border, rgba(128,128,128,0.35)));
    }
    .actions button.secondary:hover:not(:disabled) {
        background: var(--vscode-button-secondaryHoverBackground);
    }
    .actions button:disabled { opacity: 0.4; cursor: default; }

    .hint {
        font-size: 11px;
        line-height: 1.5;
        color: var(--vscode-descriptionForeground, var(--vscode-foreground));
        opacity: 0.85;
    }
    /* Ollama down: a notice, not an error — search in the other tab still works. */
    .notice {
        display: flex;
        gap: 6px;
        align-items: flex-start;
        font-size: 11px;
        line-height: 1.45;
        padding: 6px 7px;
        border-radius: 4px;
        color: var(--vscode-inputValidation-warningForeground, var(--vscode-foreground));
        background: var(--vscode-inputValidation-warningBackground,
                    rgba(190, 140, 0, 0.15));
        border: 1px solid var(--vscode-inputValidation-warningBorder,
                rgba(190, 140, 0, 0.5));
    }
    kbd {
        font-family: var(--vscode-editor-font-family, monospace);
        font-size: 10px;
        padding: 0 4px;
        border-radius: 3px;
        border: 1px solid var(--vscode-widget-border, rgba(128,128,128,0.4));
        background: var(--vscode-keybindingLabel-background, rgba(128,128,128,0.17));
    }
    .running { display: flex; align-items: center; gap: 6px; font-size: 11px; }
    .spinner {
        width: 11px; height: 11px; border-radius: 50%;
        border: 1.5px solid var(--vscode-descriptionForeground, currentColor);
        border-top-color: transparent;
        animation: spin 0.8s linear infinite;
    }
    @keyframes spin { to { transform: rotate(360deg); } }
</style>
</head>
<body>
<svg width="0" height="0" style="position:absolute" aria-hidden="true">
  <defs>
    <g id="i-search" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round">
      <circle cx="6.6" cy="6.6" r="4.4"/><path d="M9.9 9.9 13.6 13.6"/>
    </g>
    <g id="i-research" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round">
      <path d="M6.2 2v3.6L2.8 11.6a1.6 1.6 0 0 0 1.4 2.4h7.6a1.6 1.6 0 0 0 1.4-2.4L9.8 5.6V2"/>
      <path d="M5.2 2h5.6"/><path d="M4.7 9.9h6.6"/>
    </g>
    <g id="i-cancel" fill="none" stroke="currentColor" stroke-width="1.6"
       stroke-linecap="round">
      <path d="M4.2 4.2 11.8 11.8"/><path d="M11.8 4.2 4.2 11.8"/>
    </g>
    <g id="i-warning" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round">
      <path d="M8 1.9 15 14H1z"/><path d="M8 6.2v3.6"/><path d="M8 11.8h.01"/>
    </g>
  </defs>
</svg>

<div class="stack">
    <div class="segmented" role="tablist">
        <button id="tab-search" role="tab" aria-selected="false" title="Semantic code search">
            <svg width="14" height="14" viewBox="0 0 16 16"><use href="#i-search"/></svg>Search
        </button>
        <button id="tab-research" role="tab" aria-selected="true"
                title="Local model investigates and writes a cited report">
            <svg width="14" height="14" viewBox="0 0 16 16"><use href="#i-research"/></svg>Research
        </button>
    </div>

    <div class="notice" id="ollama-notice" hidden>
        <svg width="13" height="13" viewBox="0 0 16 16" aria-hidden="true"><use href="#i-warning"/></svg>
        <span>The server's Ollama is not answering — Research will fail until it is
        back. Search is unaffected; see <b>Server Status → Health → ollama</b>.</span>
    </div>

    <textarea id="text" rows="4"></textarea>

    <div class="options" id="opts-search" hidden>
        <label for="topk">results</label>
        <input id="topk" type="number" min="1" max="50" value="${this.defaultTopK()}"
               title="How many results to request (top-k)">
        <label for="lang">lang</label>
        <select id="lang" title="Restrict the search to one language">
            <option value="">any</option>
            ${langOptions}
        </select>
    </div>

    <div class="options" id="opts-research">
        <label for="effort">effort</label>
        <select id="effort" title="Preset budget: time, local tokens and tool calls">
            <option value="low">low</option>
            <option value="medium" selected>medium</option>
            <option value="high">high</option>
        </select>
        <!-- Two nodes, one visible. The select is the real control once the server
             has told us what Ollama actually has; until then (older server, Ollama
             unreachable, first tick not yet done) the text input is the honest
             fallback rather than an empty dropdown. -->
        <select id="model-select" class="grow" hidden
                title="Ollama model for this run"></select>
        <input id="model" class="grow" type="text" placeholder="model (optional)"
               title="Ollama model override; empty uses the server default"
               value="${escapeHtml(this.defaultModel())}">
    </div>

    <details class="options budget" id="opts-budget">
        <summary title="Override individual budget axes for this run">budget</summary>
        <div class="row">
            <label for="bseconds">sec</label>
            <input id="bseconds" type="number" min="1" placeholder="preset"
                   title="Wall-clock for the investigation. The budget you actually wait for.">
            <label for="btokens">k tok</label>
            <input id="btokens" type="number" min="1" placeholder="preset"
                   title="Local tokens (thousands) the run may spend — prompt + generated, summed over turns. What it costs the GPU.">
            <label for="bsteps">steps</label>
            <input id="bsteps" type="number" min="1" placeholder="preset"
                   title="Executed tool calls. A backstop, not a measure of work.">
        </div>
        <div class="hint" id="budget-hint"></div>
    </details>

    <details class="options budget" id="opts-scope">
        <summary title="Restrict what this run may read. Enforced by the server on every lookup.">scope</summary>
        <div class="row">
            <label for="sinclude">only</label>
            <input id="sinclude" class="grow" type="text" placeholder="whole project"
                   title="Comma-separated globs; only matching files are visible to any tool">
        </div>
        <div class="row">
            <label for="sexclude">never</label>
            <input id="sexclude" class="grow" type="text" placeholder="nothing excluded"
                   title="Comma-separated globs; matching files are hidden from every tool">
        </div>
        <div class="row">
            <label for="slangs">langs</label>
            <select id="slangs" class="grow" multiple size="4"
                    title="Restrict to these languages; select none for all">
                ${langOptions}
            </select>
        </div>
        <div class="row">
            <button id="scope-folder" class="secondary" type="button"
                    title="Set the include globs to the folder of the file you are looking at">this folder</button>
            <button id="scope-mindex" class="secondary" type="button"
                    title="Reset to the project scope from .mindex">from .mindex</button>
            <button id="scope-clear" class="secondary" type="button"
                    title="Clear the scope — the run sees the whole project">clear</button>
        </div>
        <div class="hint">Globs are root-relative and <code>*</code> crosses
        <code>/</code> here, so <code>src/*</code> also matches <code>src/db/x.rs</code>
        — wider than the same pattern in <code>.mindex</code>. A scoped report can only
        speak about its scope.</div>
    </details>

    <div class="actions">
        <button id="submit">
            <svg width="14" height="14" viewBox="0 0 16 16"><use id="submit-icon" href="#i-research"/></svg>
            <span id="submit-label">Research</span>
        </button>
        <button id="cancel" class="secondary" disabled hidden title="Drop the connection — that is the server-side cancel">
            <svg width="13" height="13" viewBox="0 0 16 16"><use href="#i-cancel"/></svg>Cancel
        </button>
    </div>

    <div class="hint" id="hint"></div>
</div>

<script nonce="${nonce}">
const vscodeApi = acquireVsCodeApi();
const el = (id) => document.getElementById(id);
const tabs = { search: el("tab-search"), research: el("tab-research") };
const text = el("text"), hint = el("hint");
const submit = el("submit"), submitIcon = el("submit-icon"), submitLabel = el("submit-label");
const cancel = el("cancel");
const optsSearch = el("opts-search"), optsResearch = el("opts-research");
const ollamaNotice = el("ollama-notice");
const topk = el("topk"), lang = el("lang"), effort = el("effort"), model = el("model");
const modelSelect = el("model-select");
// Whichever of the two model nodes is currently visible owns the value. One reader,
// so submit / persist / restore cannot disagree about which control is live.
const modelValue = () => (modelSelect.hidden ? model.value : modelSelect.value);
const optsBudget = el("opts-budget"), budgetHint = el("budget-hint");
const bseconds = el("bseconds"), btokens = el("btokens"), bsteps = el("bsteps");

const COPY = {
    search: {
        placeholder: "Semantic code search — e.g. where are collection names derived?",
        hint: 'Opens a quick pick that previews each hit in the editor. <kbd>Enter</kbd> to search.',
        label: "Search",
        icon: "#i-search",
    },
    research: {
        placeholder: "What do you want researched in this codebase?",
        hint: 'A local model investigates and streams a cited report into its own tab. <kbd>Enter</kbd> to start, <kbd>Shift</kbd>+<kbd>Enter</kbd> for a newline.',
        label: "Research",
        icon: "#i-research",
    },
};

let mode = "research";
let running = false;
let researchAvailable = true;
// The server's effort ladder (GET /config). Until it arrives the levels are shown
// unlabelled — a guessed number here is how the old "3 / 8 / 16" survived three
// server changes.
let researchConfig = null;

const fmtTokens = (n) => (n >= 1000 ? Math.round(n / 1000) + "k" : String(n));

// Rebuilds both language pickers in place. One function so the option list the HTML
// is seeded with and the one a later refresh installs cannot drift apart.
//
// Selections survive where they still exist: the single-select falls back to "any"
// and the multi-select keeps the intersection, because a filter that silently
// changes is worse than one that visibly resets.
function applyLanguages(list) {
    const keepOne = lang.value;
    const keepMany = new Set(Array.from(slangs.selectedOptions).map((o) => o.value));

    lang.replaceChildren(new Option("any", ""));
    slangs.replaceChildren();
    for (const l of list) {
        lang.add(new Option(l, l));
        slangs.add(new Option(l, l, false, keepMany.has(l)));
    }
    lang.value = list.includes(keepOne) ? keepOne : "";
    persist();
}

// The model field is a closed list once the server has told us what its Ollama
// actually has; with nothing to offer it stays the free-text input it was.
//
// The default option's value is "" rather than the model's name on purpose: sending
// nothing keeps whatever the *server* considers default, which stays right if the
// operator changes [research].default_model.
function applyModels(models, defaultModel) {
    if (!Array.isArray(models) || models.length === 0) {
        modelSelect.hidden = true;
        model.hidden = false;
        return;
    }
    const keep = modelValue();
    modelSelect.replaceChildren(
        new Option(defaultModel ? "server default (" + defaultModel + ")" : "server default", "")
    );
    for (const m of models) modelSelect.add(new Option(m, m));
    modelSelect.value = models.includes(keep) ? keep : "";
    modelSelect.hidden = false;
    model.hidden = true;
    persist();
}

function applyResearchConfig(info) {
    researchConfig = info;
    applyModels(info.models, info.default_model);
    for (const level of ["low", "medium", "high"]) {
        const b = info.effort[level];
        const option = effort.querySelector('option[value="' + level + '"]');
        if (b && option) {
            option.textContent =
                level + " · " + b.max_seconds + "s · " + fmtTokens(b.max_tokens) +
                " tok · " + b.max_steps + " steps";
        }
    }
    bseconds.max = info.max_request_seconds;
    btokens.max = Math.floor(info.max_request_tokens / 1000);
    bsteps.max = info.max_request_steps;
    renderBudgetHint();
}

function renderBudgetHint() {
    if (researchConfig === null) {
        budgetHint.textContent = "Empty fields use the effort preset.";
        return;
    }
    const b = researchConfig.effort[effort.value] ?? researchConfig.effort.medium;
    budgetHint.textContent =
        "Empty = the " + effort.value + " preset (" + b.max_seconds + "s / " +
        fmtTokens(b.max_tokens) + " tok / " + b.max_steps + " steps). Max " +
        researchConfig.max_request_seconds + "s / " +
        fmtTokens(researchConfig.max_request_tokens) + " tok / " +
        researchConfig.max_request_steps + " steps.";
}

function render() {
    for (const [m, tab] of Object.entries(tabs)) {
        tab.setAttribute("aria-selected", String(m === mode));
    }
    const copy = COPY[mode];
    text.placeholder = copy.placeholder;
    submitLabel.textContent = copy.label;
    submitIcon.setAttribute("href", copy.icon);
    optsSearch.hidden = mode !== "search";
    optsResearch.hidden = mode !== "research";
    optsBudget.hidden = mode !== "research";
    optsScope.hidden = mode !== "research";
    // Cancel is a research affordance: a search round-trip is short, and the quick
    // pick's own Esc already dismisses it.
    cancel.hidden = mode !== "research";
    // Only in the Research tab: in Search the server's Ollama is irrelevant.
    ollamaNotice.hidden = mode !== "research" || researchAvailable;
    submit.disabled = running;
    cancel.disabled = !running;
    hint.innerHTML = running
        ? '<span class="running"><span class="spinner"></span>Researching — Cancel drops the connection, which frees the model.</span>'
        : copy.hint;
}

function setMode(next) {
    if (running) return; // don't strand an in-flight research run behind the other tab
    mode = next;
    persist();
    render();
}
tabs.search.addEventListener("click", () => setMode("search"));
tabs.research.addEventListener("click", () => setMode("research"));

const sinclude = el("sinclude");
const sexclude = el("sexclude");
const slangs = el("slangs");
const optsScope = el("opts-scope");
const selectedLangs = () => Array.from(slangs.selectedOptions).map((o) => o.value).join(",");
const selectLangs = (csv) => {
    const want = new Set(String(csv ?? "").split(",").map((s) => s.trim()).filter(Boolean));
    for (const o of slangs.options) o.selected = want.has(o.value);
};

const persist = () =>
    vscodeApi.setState({
        mode,
        text: text.value,
        topK: topk.value,
        lang: lang.value,
        effort: effort.value,
        model: modelValue(),
        bseconds: bseconds.value,
        btokens: btokens.value,
        bsteps: bsteps.value,
        sinclude: sinclude.value,
        sexclude: sexclude.value,
        slangs: selectedLangs(),
    });

const saved = vscodeApi.getState();
if (saved) {
    mode = saved.mode === "search" ? "search" : "research";
    text.value = saved.text ?? "";
    if (saved.topK) topk.value = saved.topK;
    if (saved.lang !== undefined) lang.value = saved.lang;
    effort.value = saved.effort ?? "medium";
    // Restored into the text input, which is what is visible until /config arrives;
    // applyModels then carries the value over if the server actually has it.
    if (saved.model !== undefined) model.value = saved.model;
    if (saved.bseconds !== undefined) bseconds.value = saved.bseconds;
    if (saved.btokens !== undefined) btokens.value = saved.btokens;
    if (saved.bsteps !== undefined) bsteps.value = saved.bsteps;
    if (saved.sinclude !== undefined) sinclude.value = saved.sinclude;
    if (saved.sexclude !== undefined) sexclude.value = saved.sexclude;
    if (saved.slangs !== undefined) selectLangs(saved.slangs);
}
render();
renderBudgetHint();

for (const node of [
    text, topk, lang, effort, model, modelSelect, bseconds, btokens, bsteps,
    sinclude, sexclude, slangs,
]) {
    node.addEventListener("input", persist);
    node.addEventListener("change", persist);
}
effort.addEventListener("change", renderBudgetHint);

function doSubmit() {
    if (submit.disabled) return;
    vscodeApi.postMessage({
        type: "submit",
        mode,
        text: text.value,
        topK: topk.value,
        lang: lang.value,
        effort: effort.value,
        model: modelValue(),
        // Blank stays blank: the extension only sends the axes actually named, so
        // an untouched field means "use the preset", not "use 0".
        bseconds: bseconds.value,
        btokens: btokens.value,
        bsteps: bsteps.value,
        sinclude: sinclude.value,
        sexclude: sexclude.value,
        slangs: selectedLangs(),
        scopeCurrentFolder: false,
    });
}
submit.addEventListener("click", doSubmit);
// The webview has no editor API, so the resolution happens host-side: this asks for
// it by name rather than keeping a copy of the active editor's folder in sync here.
el("scope-folder").addEventListener("click", () => {
    vscodeApi.postMessage({
        type: "submit", mode, text: text.value, topK: topk.value, lang: lang.value,
        effort: effort.value, model: modelValue(),
        bseconds: bseconds.value, btokens: btokens.value, bsteps: bsteps.value,
        sinclude: sinclude.value, sexclude: sexclude.value, slangs: selectedLangs(),
        scopeCurrentFolder: true,
    });
});
el("scope-mindex").addEventListener("click", () => {
    vscodeApi.postMessage({ type: "scopeDefaults" });
});
el("scope-clear").addEventListener("click", () => {
    sinclude.value = "";
    sexclude.value = "";
    selectLangs("");
    persist();
});
cancel.addEventListener("click", () => vscodeApi.postMessage({ type: "cancel" }));

// Enter submits; Shift+Enter keeps the newline (the question box is multiline).
text.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        doSubmit();
    }
});

window.addEventListener("message", (e) => {
    if (e.data.type === "running") {
        running = e.data.running;
        render();
    } else if (e.data.type === "researchAvailable") {
        researchAvailable = e.data.available;
        render();
    } else if (e.data.type === "researchConfig") {
        applyResearchConfig(e.data.info);
    } else if (e.data.type === "languages") {
        applyLanguages(e.data.languages ?? []);
    } else if (e.data.type === "mode") {
        setMode(e.data.mode);
    } else if (e.data.type === "scope") {
        sinclude.value = e.data.include ?? "";
        sexclude.value = e.data.exclude ?? "";
        selectLangs(e.data.languages ?? "");
        optsScope.open = true;
        persist();
    }
});
</script>
</body>
</html>`;
    }
}

function asString(v: unknown, fallback = ""): string {
    return typeof v === "string" ? v : fallback;
}

/**
 * The budget overrides the user actually filled in. A blank field is left out
 * entirely rather than sent as 0 — absent means "the effort preset", while 0 is a
 * value the server rejects. Out-of-range values are *not* clamped here: the server
 * owns the ceilings, and clamping would silently run something other than what was
 * asked for.
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
 * The scope the user typed, as the server's selector shape.
 *
 * Languages are checked against `ALL_LANGUAGES` — the same whitelist the Search
 * half applies — so a stale value from restored webview state is dropped here
 * rather than becoming a 400.
 *
 * Deliberately **not** checked against the project's live inventory, even though
 * that is what the picker now offers: the inventory is an availability hint, not a
 * validity contract. A language indexed one second after the last stats fetch is a
 * legitimate value, and dropping the user's explicit selection would silently run a
 * different query than the one they asked for. Offering is inventory-driven;
 * validating is not.
 *
 * Globs are passed through unchanged: they are
 * evaluated by SQLite `GLOB` server-side, and translating them to `.mindex`'s
 * stricter dialect would make this the fifth glob dialect in the project.
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

function escapeHtml(s: string): string {
    return s.replace(
        /[&<>"']/g,
        (c) =>
            ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
                c
            ] as string
    );
}

function makeNonce(): string {
    const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let out = "";
    for (let i = 0; i < 32; i++) {
        out += chars.charAt(Math.floor(Math.random() * chars.length));
    }
    return out;
}
