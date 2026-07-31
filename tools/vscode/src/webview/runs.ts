/**
 * Research History, webview side.
 *
 * The host owns every request and all the state that matters; this module renders
 * what it is sent and reports what the user did. Nothing here re-renders the page
 * wholesale — the search box holds a half-typed query and the list holds a
 * multi-click selection, and both would be discarded.
 */

import { marked } from "marked";
import { el, icon, vscodeApi } from "./host.js";

interface RunSummary {
    id: string;
    seq: number;
    title: string;
    question: string;
    created_at: number;
    expires_at: number | null;
    pinned: boolean;
    model: string;
    effort: string;
    done_reason: string;
    citations_total: number;
    citations_verified: number;
    citations_unverified: number;
    steps: number;
    elapsed_ms: number;
    files_total: number;
    files_moved: number;
    stale: boolean;
    valid: boolean;
    invalid_reason: string | null;
    context: RunDependency[];
}

interface RunDependency {
    id: string;
    seq: number | null;
    title: string | null;
    state: "valid" | "invalid" | "deleted";
}

interface RunFile {
    path: string;
    sha256: string;
    current_sha256: string | null;
    state: string;
}

interface RunDetail extends RunSummary {
    report: string;
    prompt_version: string;
    context_run_ids: string[];
    scope: string | null;
    files: RunFile[];
}

/** Survives a hidden tab; restored on reload so a half-built selection is not lost. */
interface State {
    v: string;
    query: string;
    freshness: string;
    validity: string;
    selected: string[];
}

const api = vscodeApi<State>();

const searchBox = el<HTMLInputElement>("runs-search");
const freshnessBox = el<HTMLSelectElement>("runs-freshness");
const validityBox = el<HTMLSelectElement>("runs-validity");
const list = el<HTMLUListElement>("runs-items");
const empty = el("runs-empty");
const errorBox = el("runs-error");
const loading = el("runs-loading");
const moreBtn = el<HTMLButtonElement>("runs-more");
const useBtn = el<HTMLButtonElement>("runs-use");
const useLabel = el("runs-use-label");
const preview = el("runs-preview");

let selected = new Set<string>();
let activeId: string | undefined;

const restored = api.getState();
if (restored?.v === "2") {
    searchBox.value = restored.query;
    freshnessBox.value = restored.freshness;
    validityBox.value = restored.validity;
    selected = new Set(restored.selected);
}

function save(): void {
    api.setState({
        v: "2",
        query: searchBox.value,
        freshness: freshnessBox.value,
        validity: validityBox.value,
        selected: [...selected],
    });
}

function sendSearch(): void {
    save();
    api.postMessage({
        type: "search",
        q: searchBox.value,
        freshness: freshnessBox.value,
        validity: validityBox.value,
    });
}

// The host debounces. Doing it on both sides would compound into a wait the user
// notices, and the host is where the AbortController lives.
searchBox.addEventListener("input", sendSearch);
freshnessBox.addEventListener("change", sendSearch);
validityBox.addEventListener("change", sendSearch);
moreBtn.addEventListener("click", () => api.postMessage({ type: "more" }));
useBtn.addEventListener("click", () => api.postMessage({ type: "useAsContext" }));

function relative(unixSeconds: number): string {
    const secs = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
    if (secs < 90) {
        return "just now";
    }
    for (const [limit, div, unit] of [
        [3600, 60, "min"],
        [86400, 3600, "h"],
        [2592000, 86400, "d"],
    ] as const) {
        if (secs < limit) {
            return `${Math.floor(secs / div)} ${unit} ago`;
        }
    }
    return `${Math.floor(secs / 2592000)} mo ago`;
}

function badge(text: string, kind: string, title: string): HTMLSpanElement {
    const span = document.createElement("span");
    span.className = `runs-badge ${kind}`;
    span.textContent = text;
    span.title = title;
    return span;
}

function ghostButton(glyph: string, title: string, onClick: () => void): HTMLButtonElement {
    const b = document.createElement("button");
    b.className = "ghost";
    b.title = title;
    b.appendChild(icon(glyph));
    b.addEventListener("click", (e) => {
        // The row itself opens the report; an action button must not also do that.
        e.stopPropagation();
        onClick();
    });
    return b;
}

function renderRow(run: RunSummary): HTMLLIElement {
    const li = document.createElement("li");
    li.className = "runs-item";
    li.dataset.id = run.id;
    if (run.id === activeId) {
        li.classList.add("selected-row");
    }

    const check = document.createElement("input");
    check.type = "checkbox";
    check.checked = selected.has(run.id) && run.valid;
    // The server refuses an invalid run as context (400), so offering the
    // checkbox would only defer the same refusal to submit time.
    check.disabled = !run.valid;
    check.title = run.valid
        ? "Use this report as context for the next question"
        : "This report is no longer valid and cannot be used as context.";
    check.addEventListener("click", (e) => e.stopPropagation());
    check.addEventListener("change", () => {
        if (check.checked) {
            selected.add(run.id);
        } else {
            selected.delete(run.id);
        }
        save();
        api.postMessage({ type: "toggle", id: run.id, checked: check.checked });
    });

    const body = document.createElement("div");
    body.className = "runs-item-body";
    const title = document.createElement("span");
    title.className = "runs-title";
    title.textContent = run.title;
    title.title = run.question;
    body.appendChild(title);

    const meta = document.createElement("div");
    meta.className = "runs-meta dim";
    const seq = document.createElement("span");
    seq.className = "runs-seq";
    seq.textContent = `#${run.seq}`;
    meta.appendChild(seq);
    const when = document.createElement("span");
    when.textContent = relative(run.created_at);
    meta.appendChild(when);
    if (run.stale) {
        meta.appendChild(
            badge(
                `${run.files_moved}/${run.files_total} moved`,
                "stale",
                "Files this report was written against have changed or been removed " +
                    "since. Its specifics may no longer hold."
            )
        );
    }
    if (run.done_reason !== "finalized") {
        meta.appendChild(
            badge(
                "partial",
                "incomplete",
                `The run was stopped (${run.done_reason}), so the report rests on partial evidence.`
            )
        );
    }
    if (!run.valid) {
        meta.appendChild(
            badge(
                "invalid",
                "invalid",
                run.invalid_reason === "stale"
                    ? "The files this report read have moved; it no longer describes the tree."
                    : run.invalid_reason === "context_deleted"
                      ? "A report in this one's context chain was deleted."
                      : "A report in this one's context chain is no longer valid."
            )
        );
    }
    if (run.context.length > 0) {
        meta.appendChild(
            badge(
                `⤷${run.context.length}`,
                "deps",
                "Built on earlier reports:\n" +
                    run.context
                        .map((d) =>
                            d.state === "deleted"
                                ? "— deleted report"
                                : `#${d.seq} ${d.title} (${d.state})`
                        )
                        .join("\n")
            )
        );
    }
    body.appendChild(meta);

    const actions = document.createElement("div");
    actions.className = "runs-actions";
    actions.appendChild(
        ghostButton(
            run.pinned ? "pinned" : "pin",
            run.pinned
                ? "Pinned — never reaped. Click to let it age normally."
                : "Pin: keep this report past the retention window.",
            () => api.postMessage({ type: "pin", id: run.id, pinned: !run.pinned })
        )
    );
    actions.appendChild(
        ghostButton("trash", "Delete this report", () =>
            api.postMessage({ type: "delete", id: run.id })
        )
    );

    li.append(check, body, actions);
    li.addEventListener("click", () => {
        activeId = run.id;
        for (const other of list.querySelectorAll(".runs-item")) {
            other.classList.toggle(
                "selected-row",
                (other as HTMLElement).dataset.id === run.id
            );
        }
        api.postMessage({ type: "select", id: run.id });
    });
    return li;
}

function refreshUseButton(): void {
    useBtn.disabled = selected.size === 0;
    useLabel.textContent =
        selected.size === 0 ? "Use as context" : `Use ${selected.size} as context`;
}

function renderDetail(run: RunDetail): void {
    preview.replaceChildren();

    const head = document.createElement("div");
    head.className = "runs-detail-head";
    const h = document.createElement("h3");
    h.textContent = `#${run.seq} — ${run.question}`;
    head.appendChild(h);

    const meta = document.createElement("div");
    meta.className = "dim";
    const parts = [
        run.model,
        run.effort,
        `${run.steps} steps`,
        `${(run.elapsed_ms / 1000).toFixed(1)}s`,
        `${run.citations_verified}/${run.citations_total} citations verified`,
    ];
    if (run.scope !== null) {
        parts.push(`scope: ${run.scope}`);
    }
    meta.textContent = parts.join(" · ");
    head.appendChild(meta);

    if (run.done_reason !== "finalized") {
        const warn = document.createElement("p");
        warn.className = "dim";
        warn.textContent =
            `This run was stopped (${run.done_reason}) rather than finishing, so the ` +
            "report rests on partial evidence.";
        head.appendChild(warn);
    }

    // The context ancestry: every report this one leaned on, so the reader knows
    // whose claims it inherited — and which of those have since gone bad.
    if (run.context.length > 0) {
        const p = document.createElement("p");
        p.textContent = `Built on ${run.context.length} earlier report(s):`;
        head.appendChild(p);
        const ul = document.createElement("ul");
        ul.className = "runs-deps";
        for (const d of run.context) {
            const li = document.createElement("li");
            li.className = "runs-dep";
            const state = document.createElement("span");
            state.className = `dim dep-${d.state}`;
            state.textContent = d.state;
            const label = document.createElement("span");
            label.textContent =
                d.state === "deleted" ? "deleted report" : `#${d.seq} ${d.title}`;
            li.append(state, label);
            ul.appendChild(li);
        }
        head.appendChild(ul);
    }

    // The per-file freshness, which is the honest form of the list's badge: an edited
    // file and a deleted one call for different reading, and one flag cannot say
    // which happened.
    const moved = run.files.filter((f) => f.state !== "fresh");
    if (moved.length > 0) {
        const p = document.createElement("p");
        p.textContent = `${moved.length} of the ${run.files.length} files this report was written against have moved:`;
        head.appendChild(p);
        const ul = document.createElement("ul");
        ul.className = "runs-files";
        for (const f of moved) {
            const li = document.createElement("li");
            li.className = "runs-file";
            const state = document.createElement("span");
            state.className = `dim state-${f.state}`;
            state.textContent = f.state;
            const link = document.createElement("button");
            link.textContent = f.path;
            link.addEventListener("click", () =>
                api.postMessage({ type: "openFile", path: f.path })
            );
            li.append(state, link);
            ul.appendChild(li);
        }
        head.appendChild(ul);
    }
    preview.appendChild(head);

    const report = document.createElement("div");
    report.className = "runs-report";
    report.innerHTML = marked.parse(run.report) as string;
    preview.appendChild(report);
}

window.addEventListener("message", (event: MessageEvent<Record<string, unknown>>) => {
    const msg = event.data;
    switch (msg.type) {
        case "runs": {
            const runs = (msg.runs ?? []) as RunSummary[];
            if (msg.reset === true) {
                list.replaceChildren();
            }
            if (Array.isArray(msg.selected)) {
                selected = new Set(msg.selected as string[]);
            }
            for (const run of runs) {
                list.appendChild(renderRow(run));
            }
            const total = list.childElementCount;
            empty.hidden = total > 0;
            moreBtn.hidden = msg.nextBeforeSeq === null || msg.nextBeforeSeq === undefined;
            refreshUseButton();
            save();
            break;
        }
        case "preview":
            renderDetail(msg.run as RunDetail);
            break;
        case "updated": {
            // Replace the one row in place. Re-rendering the list would scroll it back
            // to the top for a change to a single button.
            const run = msg.run as RunSummary;
            const row = list.querySelector(`[data-id="${CSS.escape(run.id)}"]`);
            row?.replaceWith(renderRow(run));
            break;
        }
        case "removed": {
            const id = String(msg.id);
            list.querySelector(`[data-id="${CSS.escape(id)}"]`)?.remove();
            selected.delete(id);
            empty.hidden = list.childElementCount > 0;
            refreshUseButton();
            save();
            break;
        }
        case "selected":
            selected = new Set((msg.selected ?? []) as string[]);
            refreshUseButton();
            save();
            break;
        case "loading":
            loading.hidden = msg.loading !== true;
            break;
        case "error":
            errorBox.textContent = typeof msg.message === "string" ? msg.message : "";
            errorBox.hidden = errorBox.textContent === "";
            break;
    }
});

refreshUseButton();
api.postMessage({ type: "ready" });
