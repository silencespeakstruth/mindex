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
} from "../shared/askFields.js";
import { formatValue } from "../shared/scale.js";
import { el, pageData, vscodeApi, VsCodeApi } from "./host.js";
import { makePills } from "./ui/pills.js";
import { makeSegmented } from "./ui/segmented.js";
import { makeSlider, Slider } from "./ui/slider.js";

interface EffortBudget {
    max_seconds: number;
    max_tokens: number;
    max_steps: number;
}
interface ResearchConfig {
    default_model: string;
    models?: string[];
    effort: Record<string, EffortBudget>;
    max_request_seconds: number;
    max_request_tokens: number;
    max_request_steps: number;
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
function presetValue(axis: "max_seconds" | "max_tokens_k" | "max_steps"): number {
    const budget = researchConfig?.effort[effortValue()];
    if (budget === undefined) {
        return { max_seconds: 900, max_tokens_k: 1200, max_steps: 20 }[axis];
    }
    return axis === "max_tokens_k"
        ? Math.round(budget.max_tokens / 1000)
        : budget[axis === "max_seconds" ? "max_seconds" : "max_steps"];
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
function renderContextRuns(): void {
    const box = el("context-runs");
    box.hidden = mode !== "research" || contextRuns.length === 0;
    if (box.hidden) {
        return;
    }
    el("context-runs-label").textContent =
        contextRuns.length === 1
            ? "1 earlier report as context"
            : `${contextRuns.length} earlier reports as context`;
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
}

function renderPanels(): void {
    const budget = researchConfig?.effort[effortValue()];

    // Each disclosure states its own contents in its header, so collapsing hides the
    // controls and never the decision.
    const axes: [string, "seconds" | "ktokens" | "steps"][] = [
        ["bseconds", "seconds"],
        ["btokens", "ktokens"],
        ["bsteps", "steps"],
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
    el("submit-label").textContent = copy.label;
    el("submit-icon").className = `codicon codicon-${copy.glyph} codicon-sm`;

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

    // The mode is never switched out from under the user when Research goes away: a
    // tab that changes what Enter does while they are typing is worse than a tab that
    // is visibly unavailable. The button disables, the notice explains, the question
    // they were writing survives.
    const researchTab = el("mode").querySelector<HTMLButtonElement>('[data-mode="research"]');
    if (researchTab !== null) {
        researchTab.disabled = !available.research;
        researchTab.setAttribute("aria-disabled", String(!available.research));
    }

    const usable = available.ask && (mode === "search" || available.research);
    el<HTMLButtonElement>("submit").disabled = running || !usable;
    setFormEnabled(available.ask);

    // Only in the Research tab: in Search the server's Ollama is irrelevant. Suppressed
    // entirely when the server itself is down — two notices saying the same thing at
    // two severities is worse than the one that names the real cause.
    el("ollama-notice").hidden = mode !== "research" || available.research || !available.ask;
    el("degraded-notice").hidden = available.ask;
    if (!available.ask) {
        el("degraded-reason").textContent = capitalise(
            available.reason === "" ? "the server is not answering" : available.reason
        );
    }

    el("hint").innerHTML = running
        ? '<span class="running"><span class="spinner"></span>Researching — Stop drops the ' +
          "connection, which frees the model.</span>"
        : !available.ask
          ? "Nothing can be asked until the server is healthy again."
          : copy.hint;

    renderPanels();
}

/**
 * Disable (or restore) everything that composes a query.
 *
 * The Stop button is deliberately exempt: a run that was in flight when the server
 * went down still has a connection to drop, and the one control that ends it must not
 * be the one that disappears. Everything else is left visible and inert rather than
 * hidden — a form that collapses loses the question the user had typed into it, which
 * is the thing they are least willing to retype.
 */
function setFormEnabled(enabled: boolean): void {
    const nodes = [
        el<HTMLTextAreaElement>("text"),
        ...[...controls.values()].flatMap((c) => c.nodes),
        ...["scope-folder", "scope-mindex", "scope-clear"].map((id) => el(id)),
    ];
    for (const node of nodes) {
        for (const control of node.matches("input, textarea, select, button")
            ? [node]
            : node.querySelectorAll<HTMLElement>("input, textarea, select, button")) {
            (control as HTMLInputElement).disabled = !enabled;
        }
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
    if (!el<HTMLButtonElement>("submit").disabled) {
        submit();
    }
});
el("cancel").addEventListener("click", () => api.postMessage({ type: "cancel" }));
for (const id of ["notice-status", "degraded-status"]) {
    el(id).addEventListener("click", () => api.postMessage({ type: "openStatus" }));
}

// The webview has no editor API, so "this folder" is resolved host-side: the form asks
// for it by name rather than keeping a copy of the active editor's folder in sync here.
el("scope-folder").addEventListener("click", () => submit({ scopeCurrentFolder: true }));
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

// Enter submits; Shift+Enter keeps the newline (the question box is multiline).
el("text").addEventListener("keydown", (key) => {
    if (key.key === "Enter" && !key.shiftKey) {
        key.preventDefault();
        submit();
    }
});

window.addEventListener("message", (e: MessageEvent<Record<string, unknown>>) => {
    const msg = e.data;
    switch (msg.type) {
        case "running":
            running = msg.running === true;
            render();
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
                for (const level of ["low", "medium", "high"]) {
                    const b = researchConfig.effort[level];
                    if (b !== undefined) {
                        modeSegmentTitle(level, b);
                    }
                }
            }
            renderPanels();
            break;
        }
        case "languages":
            languagePills.setOptions((msg.languages as string[] | undefined) ?? []);
            persist();
            renderPanels();
            break;
        case "contextRuns":
            contextRuns = (msg.runs ?? []) as typeof contextRuns;
            renderContextRuns();
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

function modeSegmentTitle(level: string, b: EffortBudget): void {
    const button = el("effort").querySelector<HTMLButtonElement>(`[data-value="${level}"]`);
    if (button !== null) {
        button.title =
            `${formatValue("seconds", b.max_seconds)} · ` +
            `${formatValue("ktokens", Math.round(b.max_tokens / 1000))} tokens · ` +
            `${b.max_steps} steps`;
    }
}
