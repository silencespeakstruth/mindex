/**
 * The Ask form: one declarative table in, a working form out.
 *
 * The five hand-kept field lists this replaces — the submit handler, the "this
 * folder" handler (a verbatim copy of it plus one flag), the state writer, the state
 * reader and the change-listener loop — are now four lines each derived from
 * `ASK_FIELDS`. Adding a field is one table entry; nothing else can be forgotten.
 *
 * The one thing this file still owns by hand is *arrangement*: which group goes
 * where, what the disclosure summaries say, and what the mode switch swaps. That is
 * layout, and layout is not data.
 */
import {
    AskField,
    ASK_FIELDS,
    AskMode,
    ConfigBound,
    FieldGroup,
    groupApplies,
    PresetAxis,
} from "../shared/askFields.js";
import { formatValue } from "../shared/scale.js";
import { el, pageData, vscodeApi, VsCodeApi } from "./host.js";
import { applyBusy, setEnabled } from "./ui/busy.js";
import { makePills } from "./ui/pills.js";
import { makeSegmented } from "./ui/segmented.js";
import { makeSlider, Slider } from "./ui/slider.js";

interface EffortBudget {
    max_seconds: number;
    max_tokens: number;
    max_steps: number;
    /** Absent on servers older than the report-shape knobs. */
    max_report_sections?: number;
    max_report_words?: number;
    evidence_width?: number;
}
interface ObservedEffort {
    model: string;
    effort: string;
    runs: number;
    p50_seconds: number;
    p90_seconds: number;
}
interface ResearchConfig {
    default_model: string;
    models?: string[];
    effort: Record<string, EffortBudget>;
    /** Measured cost per (model, effort) — what a level *takes*, not what it grants. */
    observed?: { efforts?: ObservedEffort[] };
    max_request_seconds: number;
    max_request_tokens: number;
    max_request_steps: number;
    /** Absent on servers older than the report-shape knobs. */
    max_request_report_sections?: number;
    max_request_report_words?: number;
    max_evidence_width?: number;
    checkpoint_every_steps?: number;
}
interface SearchConfig {
    max_top_k: number;
    max_query_bytes: number;
}
interface PageData {
    defaultModel: string;
    defaultTopK: number;
    languages: string[];
}

/** A mounted control: how to read it, how to write it, what to listen to. */
interface Control {
    read(): string;
    write(value: string): void;
    nodes: HTMLElement[];
}

// `v` in the persisted record gates the v1 → v2 migration in `restore`.
const api: VsCodeApi<Record<string, string>> = vscodeApi();
const data = pageData<PageData>() ?? { defaultModel: "", defaultTopK: 10, languages: [] };

const controls = new Map<string, Control>();
const sliders = new Map<string, Slider>();
/** The model field is two nodes with one value; see `mountModel`. */
let modelSelect: HTMLSelectElement;
let modelText: HTMLInputElement;
let languagePills: ReturnType<typeof makePills>;

let mode: AskMode = "research";
let running = false;
/**
 * A search round trip is in flight.
 *
 * Separate from `running`, which is Research's: a search has no Stop, no panel
 * and no hint line, and it is short enough that the whole affordance is a
 * disabled button that says what it is doing. What it *does* share is the need
 * for one — five fast clicks used to be five concurrent searches and five quick
 * picks racing to open.
 */
let searching = false;
/** What the server can currently be asked for; optimistic until health first lands. */
let available = { ask: true, research: true, reason: "" };
let researchConfig: ResearchConfig | undefined;
/**
 * Stored runs picked in Research History, to be handed to the next question as
 * background. Owned by the host — the panel is the only place they can be chosen —
 * so this is a render cache, and the submit payload takes them from the host too.
 */
let contextRuns: {
    id: string;
    seq: number;
    title: string;
    stale: boolean;
    valid: boolean;
}[] = [];
let searchConfig: SearchConfig | undefined;

const COPY = {
    search: {
        placeholder: "Semantic code search — e.g. where are collection names derived?",
        hint: "Opens a quick pick that previews each hit in the editor. <kbd>Enter</kbd> to search.",
        label: "Search",
        glyph: "search",
    },
    research: {
        placeholder: "What do you want researched in this codebase?",
        hint:
            "A local model investigates and streams a cited report into its own tab. " +
            "<kbd>Enter</kbd> to start, <kbd>Shift</kbd>+<kbd>Enter</kbd> for a newline.",
        label: "Research",
        glyph: "beaker",
    },
} as const;

// ── mounting ─────────────────────────────────────────────────────────────────

function mount(field: AskField): Control {
    switch (field.kind.k) {
        case "textarea":
            return mountTextarea(field, field.kind.rows);
        case "text":
            return mountText(field);
        case "segmented":
            return mountSegmented(field, field.kind.options);
        case "model":
            return mountModel(field);
        case "languages":
            return mountLanguages(field);
        case "slider":
            return mountSlider(field, field.kind);
    }
}

/** A labelled row on the shared 58px gutter. */
function labelled(field: AskField, control: HTMLElement): HTMLElement {
    const row = document.createElement("div");
    row.className = "field";
    const label = document.createElement("label");
    label.htmlFor = field.id;
    label.textContent = field.label;
    label.title = field.title;
    control.classList.add("control");
    row.append(label, control);
    return row;
}

function mountTextarea(field: AskField, rows: number): Control {
    const node = document.createElement("textarea");
    node.id = field.id;
    node.rows = rows;
    node.title = field.title;
    node.setAttribute("aria-label", field.label);
    container(field.group).appendChild(node);
    return { read: () => node.value, write: (v) => (node.value = v), nodes: [node] };
}

function mountText(field: AskField): Control {
    const node = document.createElement("input");
    node.type = "text";
    node.id = field.id;
    node.title = field.title;
    node.placeholder = field.placeholder ?? "";
    container(field.group).appendChild(labelled(field, node));
    return { read: () => node.value, write: (v) => (node.value = v), nodes: [node] };
}

function mountSegmented(
    field: AskField,
    options: { value: string; label: string; icon?: string }[]
): Control {
    const seg = makeSegmented(
        field.id,
        options.map((o) => ({ value: o.value, label: o.label, glyph: o.icon })),
        true,
        () => {
            persist();
            // Effort decides what an *unset* budget axis parks on, so the sliders and
            // the panel summaries have to follow it.
            applyPresets();
            renderPanels();
        }
    );
    container(field.group).appendChild(labelled(field, seg.root));
    return seg;
}

/**
 * The model field is a closed list once the server has said what its Ollama holds,
 * and free text until then — an older server, an unreachable Ollama or the first
 * refresh not yet done all leave nothing to offer, and an empty dropdown is worse
 * than a text box. Two nodes, one visible, one reader: `read()` asks whichever is
 * live, so submit, persist and restore cannot disagree about which one that is.
 */
function mountModel(field: AskField): Control {
    modelSelect = document.createElement("select");
    modelSelect.id = field.id;
    modelSelect.title = field.title;
    modelSelect.hidden = true;

    modelText = document.createElement("input");
    modelText.type = "text";
    modelText.id = `${field.id}-text`;
    modelText.title = field.title;
    modelText.placeholder = field.placeholder ?? "";
    modelText.value = data.defaultModel;

    const wrap = document.createElement("div");
    wrap.className = "grow";
    wrap.append(modelSelect, modelText);
    container(field.group).appendChild(labelled(field, wrap));

    return {
        read: () => (modelSelect.hidden ? modelText.value : modelSelect.value),
        write: (v) => {
            modelText.value = v;
            // Only if the server actually has it; otherwise the select keeps "server
            // default" and the value is carried by the hidden text input until
            // `applyModels` decides.
            if ([...modelSelect.options].some((o) => o.value === v)) {
                modelSelect.value = v;
            }
        },
        nodes: [modelSelect, modelText],
    };
}

function mountLanguages(field: AskField): Control {
    languagePills = makePills(field.id, field.title, persist);
    languagePills.setOptions(data.languages);
    container(field.group).appendChild(labelled(field, languagePills.root));
    return languagePills;
}

function mountSlider(
    field: AskField,
    kind: Extract<AskField["kind"], { k: "slider" }>
): Control {
    const slider = makeSlider({
        id: field.id,
        label: field.label,
        title: field.title,
        unit: kind.unit,
        min: kind.min,
        max: kind.fallbackMax,
        preset: kind.preset === undefined ? undefined : presetValue(kind.preset),
        initial: kind.seed === "topK" ? data.defaultTopK : undefined,
        onChange: () => {
            persist();
            renderPanels();
        },
    });
    sliders.set(field.id, slider);
    container(field.group).appendChild(slider.root);
    return slider;
}

function container(group: FieldGroup): HTMLElement {
    return el(`group-${group}`);
}

// ── server-published numbers ─────────────────────────────────────────────────

/** The current effort's value for one preset axis, or a sane default before /config. */
function presetValue(axis: PresetAxis): number {
    // Not an effort axis: the preset is the `[research]` scalar published beside
    // the ladder, same for every effort level.
    if (axis === "checkpoint_every_steps") {
        return researchConfig?.checkpoint_every_steps ?? 6;
    }
    const budget = researchConfig?.effort[effortValue()];
    const fallbacks = {
        max_seconds: 900,
        max_tokens_k: 1200,
        max_steps: 20,
        max_report_sections: 6,
        max_report_words: 900,
        evidence_width: 1,
    };
    if (budget === undefined) {
        return fallbacks[axis];
    }
    switch (axis) {
        case "max_tokens_k":
            return Math.round(budget.max_tokens / 1000);
        case "max_seconds":
            return budget.max_seconds;
        case "max_steps":
            return budget.max_steps;
        // Optional: an older server publishes a ladder without the shape axes.
        case "max_report_sections":
        case "max_report_words":
        case "evidence_width":
            return budget[axis] ?? fallbacks[axis];
    }
}

function effortValue(): string {
    return controls.get("effort")?.read() ?? "medium";
}

/** Resolve a field's named ceiling against what the server published. */
function boundValue(bound: ConfigBound, fallback: number): number {
    switch (bound) {
        case "search.max_top_k":
            return searchConfig?.max_top_k ?? fallback;
        case "research.max_request_seconds":
            return researchConfig?.max_request_seconds ?? fallback;
        case "research.max_request_tokens_k":
            return researchConfig === undefined
                ? fallback
                : Math.round(researchConfig.max_request_tokens / 1000);
        case "research.max_request_steps":
            return researchConfig?.max_request_steps ?? fallback;
        case "research.max_request_report_sections":
            return researchConfig?.max_request_report_sections ?? fallback;
        case "research.max_request_report_words":
            return researchConfig?.max_request_report_words ?? fallback;
        case "research.max_evidence_width":
            return researchConfig?.max_evidence_width ?? fallback;
    }
}

function applyBounds(): void {
    for (const field of ASK_FIELDS) {
        if (field.kind.k !== "slider") {
            continue;
        }
        sliders.get(field.id)?.setMax(boundValue(field.kind.max, field.kind.fallbackMax));
    }
}

function applyPresets(): void {
    for (const field of ASK_FIELDS) {
        if (field.kind.k === "slider" && field.kind.preset !== undefined) {
            sliders.get(field.id)?.setPreset(presetValue(field.kind.preset));
        }
    }
}

/**
 * The model list, and the default option's value.
 *
 * The default option's value is `""` rather than the model's own name on purpose:
 * sending nothing keeps whatever the *server* considers default, which stays right if
 * the operator changes `[research].default_model`.
 */
function applyModels(models: string[] | undefined, defaultModel: string): void {
    if (models === undefined || models.length === 0) {
        modelSelect.hidden = true;
        modelText.hidden = false;
        return;
    }
    const keep = controls.get("model")?.read() ?? "";
    modelSelect.replaceChildren(
        new Option(
            defaultModel === "" ? "server default" : `server default (${defaultModel})`,
            ""
        )
    );
    for (const m of models) {
        modelSelect.add(new Option(m, m));
    }
    modelSelect.value = models.includes(keep) ? keep : "";
    modelSelect.hidden = false;
    modelText.hidden = true;
    persist();
}

// ── rendering ────────────────────────────────────────────────────────────────

/**
 * The picked runs, as removable chips.
 *
 * Shown only in Research: Search takes no prior context, and a control that does
 * nothing in the active mode is worse than an absent one. Staleness is marked on the
 * chip because it is the one thing that changes what a report is worth, and the user
 * chose these before they could see whether the tree had moved under them.
 */
/** A message field that must be a string; anything else is the fallback. */
function text(value: unknown, fallback = ""): string {
    return typeof value === "string" ? value : fallback;
}

function renderContextRuns(): void {
    const box = el("context-runs");
    // Shown in Research **whether or not anything is picked**. Chaining a question
    // onto an earlier report is the common path, not an advanced one, and while the
    // was hidden until it had contents the only way to discover it was to find the
    // History panel first — a feature reachable only by already knowing about it.
    box.hidden = mode !== "research";
    if (box.hidden) {
        return;
    }
    el("context-runs-label").textContent =
        contextRuns.length === 0
            ? "No earlier reports as context"
            : contextRuns.length === 1
              ? "1 earlier report as context"
              : `${contextRuns.length} earlier reports as context`;
    el("context-runs-clear").hidden = contextRuns.length === 0;
    const pills = el("context-runs-pills");
    pills.replaceChildren();
    for (const run of contextRuns) {
        const chip = document.createElement("button");
        chip.type = "button";
        // Invalidity outranks staleness: the server will refuse the submit.
        chip.className = `pill${!run.valid ? " pill-invalid" : run.stale ? " pill-stale" : ""}`;
        chip.textContent = `#${run.seq} ${run.title}`;
        chip.title = !run.valid
            ? "This report is no longer valid (its files or a report it built on " +
              "moved or was deleted); the server will refuse it as context. Remove it."
            : run.stale
              ? "Files this report was written against have changed since. It is still " +
                "useful for names, and its specifics may not hold. Click to remove."
              : "Click to remove from the next question's context.";
        chip.addEventListener("click", () => {
            contextRuns = contextRuns.filter((r) => r.id !== run.id);
            api.postMessage({ type: "contextRuns", ids: contextRuns.map((r) => r.id) });
            renderContextRuns();
        });
        pills.appendChild(chip);
    }
    // The chips were just rebuilt; without this a chip created while the form is
    // frozen is the one live control on it.
    setComposingEnabled(modeUsable());
}

function renderPanels(): void {
    const budget = researchConfig?.effort[effortValue()];

    // Each disclosure states its own contents in its header, so collapsing hides the
    // controls and never the decision.
    const axes: [string, "seconds" | "ktokens" | "steps" | "count"][] = [
        ["bseconds", "seconds"],
        ["btokens", "ktokens"],
        ["bsteps", "steps"],
        ["bsections", "count"],
        ["bwords", "count"],
        ["bwidth", "count"],
        ["bcheckpoint", "steps"],
    ];
    const overridden = axes
        .filter(([id]) => (controls.get(id)?.read() ?? "") !== "")
        .map(([id, unit]) => formatValue(unit, Number(controls.get(id)?.read())));
    el("budget-summary").textContent =
        overridden.length === 0 ? `${effortValue()} preset` : overridden.join(" · ");

    el("budget-hint").textContent =
        budget === undefined
            ? "Untouched axes use the effort preset."
            : `Untouched axes use the ${effortValue()} preset ` +
              `(${formatValue("seconds", budget.max_seconds)} · ` +
              `${formatValue("ktokens", Math.round(budget.max_tokens / 1000))} · ` +
              `${budget.max_steps} steps).`;

    const langs = controls.get("slangs")?.read() ?? "";
    const include = (controls.get("sinclude")?.read() ?? "").trim();
    const exclude = (controls.get("sexclude")?.read() ?? "").trim();
    const globCount = [include, exclude].filter((g) => g !== "").length;
    const parts: string[] = [];
    if (globCount > 0) {
        parts.push(`${globCount} glob${globCount === 1 ? "" : "s"}`);
    }
    if (langs !== "") {
        parts.push(langs.split(",").join(", "));
    }
    el("scope-summary").textContent = parts.length === 0 ? "whole project" : parts.join(" · ");
}

function render(): void {
    const copy = COPY[mode];
    for (const button of el("mode").querySelectorAll("button")) {
        button.setAttribute("aria-selected", String(button.dataset.mode === mode));
    }

    const text = el<HTMLTextAreaElement>("text");
    text.placeholder = copy.placeholder;
    // The button says what it is doing while it does it. A search is too short
    // for a progress surface and too long to look like nothing happened.
    el("submit-label").textContent = searching ? "Searching…" : copy.label;
    el("submit-icon").className = searching
        ? "codicon codicon-loading codicon-modifier-spin codicon-sm"
        : `codicon codicon-${copy.glyph} codicon-sm`;

    // Group visibility falls straight out of the table: a group is shown when any of
    // its fields belongs to this mode.
    el("group-search").hidden = !groupApplies("search", mode);
    el("group-research").hidden = !groupApplies("research", mode);
    el("panel-budget").hidden = !groupApplies("budget", mode);
    el("panel-scope").hidden = !groupApplies("scope", mode);
    renderContextRuns();

    // Stop is a Research affordance: a search round-trip is short, and the quick
    // pick's own Esc already dismisses it.
    const cancel = el<HTMLButtonElement>("cancel");
    cancel.hidden = mode !== "research";
    cancel.disabled = !running;

    // The tabs stay live in every state, including the one they lead into. A
    // disabled tab is a dead end that explains nothing: the user learns that
    // Research is unavailable and not that the server's Ollama is down, because
    // the sentence saying so lives *behind* the tab they cannot press. So the tab
    // opens, and the notice inside it is the answer.
    const researchTab = el("mode").querySelector<HTMLButtonElement>('[data-mode="research"]');
    if (researchTab !== null) {
        researchTab.disabled = false;
        researchTab.removeAttribute("aria-disabled");
    }

    const usable = modeUsable();
    setEnabled(el<HTMLButtonElement>("submit"), usable && !running && !searching);
    setComposingEnabled(usable);

    // Only in the Research tab: in Search the server's Ollama is irrelevant. Suppressed
    // entirely when the server itself is down — two notices saying the same thing at
    // two severities is worse than the one that names the real cause.
    el("ollama-notice").hidden = mode !== "research" || available.research || !available.ask;
    if (!available.research && available.ask) {
        // The reason travels from the host because the two causes have different
        // remedies: a dependency is restarted, a token is replaced. Falling back
        // to Ollama's wording is right for an older host that sends none — that
        // was the only cause this notice used to have.
        el("ollama-reason").textContent = `${capitalise(
            available.reason === "" ? "the server's Ollama is not answering" : available.reason
        )}, so ${copy.label} is unavailable.`;
    }
    el("degraded-notice").hidden = available.ask;
    if (!available.ask) {
        // What is missing, and what it costs *in this tab*. "The server is
        // degraded" is not an answer to "why is my Search button dead".
        el("degraded-reason").textContent = `${capitalise(
            available.reason === "" ? "the server is not answering" : available.reason
        )}, so ${copy.label} is unavailable until it recovers.`;
    }

    el("hint").innerHTML = running
        ? '<span class="running"><span class="spinner"></span>Researching — Stop drops the ' +
          "connection, which frees the model.</span>"
        : !usable
          ? "The form is frozen while what it needs is down; Stop still works."
          : copy.hint;

    renderPanels();
}

/**
 * Freeze the form when the mode it is showing cannot be served.
 *
 * The gate moved outward on purpose. It used to be "disable only what reaches
 * the server", which left every field live under a red notice — and a form that
 * accepts text, globs and budget changes while stating that nothing can be asked
 * is telling the user two different things at once. A dead-looking field is the
 * honest rendering: the notice says what is missing, and the controls say they
 * are not currently worth filling in.
 *
 * Three exemptions, each for a reason:
 *
 * - **The mode switch** — always live. It is how the user reaches the notice
 *   that explains the other tab, and a disabled tab explains nothing.
 * - **Stop** — a run in flight when the server went down still has a connection
 *   to drop, and the one control that ends it must not be the one that dies.
 * - **The notices' own links** — "Open Server Status" is the remedy being
 *   offered; disabling it would disable the way out.
 *
 * Submit is gated by `render` (it also has `running`/`searching` to account for)
 * and skipped here so the two cannot fight over it.
 */
const ALWAYS_LIVE = new Set(["cancel", "submit", "notice-status", "degraded-status"]);

/** Whether the mode currently on screen can actually be served right now. */
function modeUsable(): boolean {
    return available.ask && (mode === "search" || available.research);
}

function setComposingEnabled(usable: boolean): void {
    for (const node of el("form").querySelectorAll<HTMLElement & { disabled?: boolean }>(
        "input, textarea, select, button"
    )) {
        if (ALWAYS_LIVE.has(node.id) || node.closest("#mode") !== null) {
            continue;
        }
        setEnabled(node, usable);
    }
}

function capitalise(s: string): string {
    return s.charAt(0).toUpperCase() + s.slice(1);
}

// ── state ────────────────────────────────────────────────────────────────────

/** Every field's value, keyed by id. The one reader submit and persist both use. */
function values(): Record<string, string> {
    return Object.fromEntries([...controls].map(([id, c]) => [id, c.read()]));
}

function persist(): void {
    api.setState({ v: "2", mode, ...values() });
}

function restore(): void {
    const saved = api.getState();
    if (saved === undefined) {
        return;
    }
    mode = saved.mode === "search" ? "search" : "research";
    for (const [id, control] of controls) {
        const value = saved[id];
        if (value !== undefined) {
            control.write(value);
        }
    }
    // v1 → v2. The pre-redesign form persisted two keys this one does not read, and
    // both would otherwise be dropped in silence on upgrade — which for a filter is
    // the worst way for it to change.
    if (saved.v === undefined) {
        // `lang`: the Search tab's single-select language filter, folded into the
        // chips that both modes now share.
        if (typeof saved.lang === "string" && saved.lang !== "") {
            if ((controls.get("slangs")?.read() ?? "") === "") {
                controls.get("slangs")?.write(saved.lang);
            }
        }
        // `topK`: the result count, whose key is now the field id (`topk`) like every
        // other. Benign if missed — it would fall back to the `mindex.topK` setting —
        // but a per-window value the user chose is still theirs.
        if (typeof saved.topK === "string" && saved.topK !== "") {
            controls.get("topk")?.write(saved.topK);
        }
    }
    persist();
}

/**
 * Whether a submit is allowed right now — asked at **every** entry point.
 *
 * There are two, and they used to disagree: the button checked its own
 * `disabled`, and Enter in the question box called `submit()` blind. With Ollama
 * down that meant the Research tab was disabled, Submit was disabled, the notice
 * said so — and Enter still fired a research run that round-tripped to a 503.
 *
 * Reading the button's own `disabled` rather than recomputing the condition is
 * the point: one predicate, decided in `render`, so a keyboard path can never
 * drift from what the user can see.
 */
function canSubmit(): boolean {
    return !running && !el<HTMLButtonElement>("submit").disabled;
}

function submit(extra: Record<string, unknown> = {}): void {
    api.postMessage({ type: "submit", mode, ...values(), ...extra });
}

// ── wiring ───────────────────────────────────────────────────────────────────

for (const field of ASK_FIELDS) {
    controls.set(field.id, mount(field));
}
for (const control of controls.values()) {
    for (const node of control.nodes) {
        node.addEventListener("input", persist);
        node.addEventListener("change", persist);
    }
}
// The measured-cost line in the effort tooltips is per model, so it follows the
// model picker rather than freezing at whatever was selected when config arrived.
for (const node of controls.get("model")?.nodes ?? []) {
    node.addEventListener("change", refreshEffortTitles);
}

restore();
render();

for (const button of el("mode").querySelectorAll("button")) {
    button.addEventListener("click", () => {
        // Don't strand an in-flight research run behind the other tab, and don't
        // switch into a mode the server cannot serve.
        if (running || button.disabled) {
            return;
        }
        mode = button.dataset.mode === "search" ? "search" : "research";
        persist();
        render();
    });
}

el("submit").addEventListener("click", () => {
    if (canSubmit()) {
        submit();
    }
});
el("cancel").addEventListener("click", () => api.postMessage({ type: "cancel" }));
for (const id of ["notice-status", "degraded-status"]) {
    el(id).addEventListener("click", () => api.postMessage({ type: "openStatus" }));
}

// The webview has no editor API, so "this folder" is resolved host-side: the form asks
// for it by name rather than keeping a copy of the active editor's folder in sync here.
//
// Its own message, not a `submit` the host early-returns on. A control that fills
// in a text field has no business travelling on the channel that launches
// research runs — and while it did, it was the one unguarded path into that
// channel, live during an in-flight run and against a server that was down.
el("scope-folder").addEventListener("click", () =>
    api.postMessage({ type: "scopeFolder", ...values() })
);
el("scope-mindex").addEventListener("click", () => api.postMessage({ type: "scopeDefaults" }));
el("scope-clear").addEventListener("click", () => {
    controls.get("sinclude")?.write("");
    controls.get("sexclude")?.write("");
    controls.get("slangs")?.write("");
    persist();
    renderPanels();
});

el("context-runs-clear").addEventListener("click", () => {
    contextRuns = [];
    api.postMessage({ type: "contextRuns", ids: [] });
    renderContextRuns();
});

// The host opens a QuickPick and pushes the result back as `contextRuns`, so
// nothing is rendered optimistically here: the picker is cancellable, and a chip
// that appeared before the user confirmed would have to be taken away again.
el("context-runs-pick").addEventListener("click", () => {
    api.postMessage({ type: "pickContext" });
});

// Enter submits; Shift+Enter keeps the newline (the question box is multiline).
// It goes through the same gate as the button — see `canSubmit`.
el("text").addEventListener("keydown", (key) => {
    if (key.key === "Enter" && !key.shiftKey) {
        key.preventDefault();
        if (canSubmit()) {
            submit();
        }
    }
});

window.addEventListener("message", (e: MessageEvent<Record<string, unknown>>) => {
    const msg = e.data;
    switch (msg.type) {
        case "running":
            running = msg.running === true;
            render();
            break;
        case "busy":
            // Search's in-flight state. Research keeps `running`, which does
            // strictly more — Stop, the hint line, blocking the tab switch — and
            // folding the two would lose all three.
            if (typeof msg.key === "string") {
                if (msg.key === "submit") {
                    searching = msg.busy === true;
                    render();
                }
                applyBusy(msg.key, msg.busy === true);
            }
            break;
        case "availability":
            available = {
                ask: msg.ask === true,
                research: msg.research === true,
                reason: asText(msg.reason),
            };
            render();
            break;
        case "config": {
            researchConfig = msg.research as ResearchConfig | undefined;
            searchConfig = msg.search as SearchConfig | undefined;
            applyBounds();
            applyPresets();
            if (researchConfig !== undefined) {
                applyModels(researchConfig.models, researchConfig.default_model);
                // The ladder's numbers live in the tooltip rather than the button
                // label: three labels reading "medium · 900s · 1.2M tok · 20 steps"
                // do not fit a sidebar, and the Budget summary already carries the
                // active one.
                refreshEffortTitles();
            }
            renderPanels();
            break;
        }
        case "languages":
            languagePills.setOptions((msg.languages as string[] | undefined) ?? []);
            persist();
            // `render`, not `renderPanels`: the pill buttons were just replaced and
            // must inherit whatever the form's frozen/live state currently is.
            render();
            break;
        case "contextRuns":
            contextRuns = (msg.runs ?? []) as typeof contextRuns;
            renderContextRuns();
            break;
        case "prefill":
            // Never over a live run: the form is disabled then, and replacing the
            // question under a running stream would leave the two disagreeing about
            // what is being answered.
            if (!running) {
                controls.get("text")?.write(text(msg.question));
                controls.get("effort")?.write(text(msg.effort, "medium"));
                controls.get("model")?.write(text(msg.model));
                persist();
                applyPresets();
                render();
            }
            break;
        case "mode":
            if (!running) {
                mode = msg.mode === "search" ? "search" : "research";
                persist();
                render();
            }
            break;
        case "scope":
            // Always strings on this channel — `setScope`/`postScopeDefaults` join
            // their arrays host-side, so the form never has to know the shape.
            controls.get("sinclude")?.write(asText(msg.include));
            controls.get("sexclude")?.write(asText(msg.exclude));
            controls.get("slangs")?.write(asText(msg.languages));
            el<HTMLDetailsElement>("panel-scope").open = true;
            persist();
            renderPanels();
            break;
    }
});

function asText(v: unknown): string {
    return typeof v === "string" ? v : "";
}

function refreshEffortTitles(): void {
    if (researchConfig === undefined) {
        return;
    }
    for (const level of ["low", "medium", "high"]) {
        const b = researchConfig.effort[level];
        if (b !== undefined) {
            modeSegmentTitle(level, b);
        }
    }
}

function modeSegmentTitle(level: string, b: EffortBudget): void {
    const button = el("effort").querySelector<HTMLButtonElement>(`[data-value="${level}"]`);
    if (button === null) {
        return;
    }
    let title =
        `${formatValue("seconds", b.max_seconds)} · ` +
        `${formatValue("ktokens", Math.round(b.max_tokens / 1000))} tokens · ` +
        `${b.max_steps} steps`;
    // The grant says what the level allows; the measured line says what it takes,
    // which is the number a user waiting on a run actually wants. Per model,
    // because a 31B model and a 3B model at the same level are different waits;
    // absent (no row for this model+level yet) the grant stands alone.
    const model = controls.get("model")?.read() || researchConfig?.default_model || "";
    const seen = researchConfig?.observed?.efforts?.find(
        (o) => o.effort === level && o.model === model
    );
    if (seen !== undefined) {
        title += ` · measured ~${formatValue("seconds", seen.p50_seconds)} (p50 of ${seen.runs})`;
    }
    button.title = title;
}
