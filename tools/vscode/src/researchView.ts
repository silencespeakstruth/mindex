import * as vscode from "vscode";
import {
    ResearchBudget,
    ResearchCitations,
    ResearchDone,
    ResearchEffort,
    ResearchProgress,
    ResearchStep,
    SearchFilter,
} from "./api";

/** What the sidebar form submits. */
export interface ResearchSubmission {
    question: string;
    effort: ResearchEffort;
    model: string;
    /** Only the axes the user filled in; absent ones keep the effort preset. */
    budget?: ResearchBudget;
    /**
     * The files the run may see. Enforced server-side on every lookup, so it bounds
     * the answer and not just the ranking — which is why the panel renders it: a
     * scoped report and an unscoped one are otherwise the same document, and only one
     * of them is entitled to say "nowhere in this project".
     */
    include?: SearchFilter;
    exclude?: SearchFilter;
}

/**
 * The streaming output tab: a webview panel showing the step feed (with the
 * model's live thinking under the current step, collapsed once the step lands)
 * and the incrementally rendered Markdown report.
 *
 * One panel per run, never reused. A finished report is a document you keep —
 * reusing the tab would silently destroy the previous answer the moment you asked
 * the next question, and two runs are rarely about the same thing. The tab title
 * carries a slug of the question so a row of them stays navigable.
 */
export class ResearchPanel {
    private panel: vscode.WebviewPanel;
    private disposed = false;

    constructor(
        private readonly extensionUri: vscode.Uri,
        question: string,
        readonly onDispose: () => void,
        /**
         * The scope the run was given, rendered in the header. Without it a scoped
         * report is indistinguishable from an unscoped one after the fact — and a
         * report that could only see `docs/**` saying "this is not in the project" is
         * misleading unless the reader can see why.
         */
        scope?: { include?: SearchFilter; exclude?: SearchFilter }
    ) {
        this.panel = vscode.window.createWebviewPanel(
            "mindexResearch",
            titleFor(question),
            { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
            {
                enableScripts: true,
                retainContextWhenHidden: true,
                localResourceRoots: [
                    vscode.Uri.joinPath(extensionUri, "node_modules", "marked"),
                ],
            }
        );
        this.panel.onDidDispose(() => {
            this.disposed = true;
            onDispose();
        });
        this.panel.webview.html = this.html(question, describeScope(scope));
        this.panel.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
            if (msg.type === "copy") {
                void vscode.env.clipboard
                    .writeText(asString(msg.text))
                    .then(() =>
                        vscode.window.showInformationMessage(
                            "mindex: report copied as Markdown."
                        )
                    );
            }
        });
    }

    get isDisposed(): boolean {
        return this.disposed;
    }

    reveal(): void {
        this.panel.reveal(undefined, true);
    }

    thinking(text: string): void {
        this.post({ type: "thinking", text });
    }
    step(step: ResearchStep): void {
        this.post({ type: "step", step });
    }
    progress(progress: ResearchProgress): void {
        this.post({ type: "progress", progress });
    }
    summary(text: string): void {
        this.post({ type: "summary", text });
    }
    citations(citations: ResearchCitations): void {
        this.post({ type: "citations", citations });
    }
    done(info: ResearchDone): void {
        this.post({ type: "done", info });
    }
    /** `code` lets the view decide whether the streamed summary is salvageable. */
    error(detail: string, code?: string): void {
        this.post({ type: "error", detail, code });
    }
    cancelled(): void {
        this.post({ type: "cancelled" });
    }

    private post(msg: unknown): void {
        if (!this.disposed) {
            void this.panel.webview.postMessage(msg);
        }
    }

    private html(question: string, scope: string): string {
        const nonce = makeNonce();
        const markedUri = this.panel.webview.asWebviewUri(
            vscode.Uri.joinPath(
                this.extensionUri,
                "node_modules",
                "marked",
                "lib",
                "marked.umd.js"
            )
        );
        return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline';
               script-src 'nonce-${nonce}' ${this.panel.webview.cspSource};">
<style>
    body { font-family: var(--vscode-font-family); padding: 0 16px 24px; line-height: 1.5; }
    .question { white-space: pre-wrap; opacity: 0.85; border-left: 3px solid
                var(--vscode-textBlockQuote-border); padding: 4px 8px; margin: 12px 0; }
    .step { margin: 6px 0 2px; }
    .step .hits { opacity: 0.6; }
    details.thinking { margin: 2px 0 8px 12px; opacity: 0.65; font-size: 0.92em; }
    details.thinking pre { white-space: pre-wrap; margin: 4px 0; }
    .status { font-style: italic; opacity: 0.7; margin: 8px 0; }
    .error { color: var(--vscode-errorForeground); white-space: pre-wrap; margin: 8px 0; }
    .cutshort { color: var(--vscode-editorWarning-foreground, #cca700); margin: 8px 0; }
    #report { border-top: 1px solid var(--vscode-widget-border, #8884); margin-top: 12px;
              padding-top: 4px; }
    #report code { background: var(--vscode-textCodeBlock-background); padding: 0 3px; }
    #report pre { background: var(--vscode-textCodeBlock-background); padding: 8px;
                  overflow-x: auto; }
    #toolbar { margin-top: 12px; }
    #toolbar button { background: var(--vscode-button-background);
                      color: var(--vscode-button-foreground); border: none;
                      padding: 4px 10px; cursor: pointer; }
    /* Budget meter: sticky so it stays readable while a long report scrolls. */
    #budget { position: sticky; top: 0; z-index: 1; display: none;
              background: var(--vscode-editor-background); padding: 6px 0 8px;
              border-bottom: 1px solid var(--vscode-widget-border, #8884);
              font-size: 0.88em; }
    #budget .axes { display: flex; gap: 10px; }
    #budget .axis { flex: 1; }
    #budget .axis .label { display: flex; justify-content: space-between;
                           opacity: 0.75; margin-bottom: 2px; }
    #budget .axis .bar { height: 4px; background: var(--vscode-widget-border, #8884); }
    #budget .axis .fill { height: 100%; width: 0;
                          background: var(--vscode-progressBar-background, #0a84ff); }
    /* The binding axis is the one that will end the run — the only number the
       reader has to look at, so it is the only one highlighted. */
    #budget .axis.binding .label { opacity: 1; font-weight: 600; }
    #budget .axis.binding .fill { background: var(--vscode-editorWarning-foreground, #cca700); }
    #budget .cost { opacity: 0.7; margin-top: 4px; }
    .scope { font-size: 0.9em; opacity: 0.8; margin: -4px 0 8px 0; }
</style>
</head>
<body>
<h3>Research</h3>
<div class="question" id="question"></div>
<div class="scope" id="scope" hidden></div>
<div id="budget">
    <div class="axes">
        <div class="axis" id="axis-time"><div class="label"><span>time</span><span class="val"></span></div><div class="bar"><div class="fill"></div></div></div>
        <div class="axis" id="axis-tokens"><div class="label"><span>tokens</span><span class="val"></span></div><div class="bar"><div class="fill"></div></div></div>
        <div class="axis" id="axis-steps"><div class="label"><span>steps</span><span class="val"></span></div><div class="bar"><div class="fill"></div></div></div>
        <div class="axis" id="axis-context"><div class="label"><span>context</span><span class="val"></span></div><div class="bar"><div class="fill"></div></div></div>
    </div>
    <div class="cost" id="cost"></div>
</div>
<div id="steps"></div>
<div class="status" id="status">starting…</div>
<div id="report"></div>
<div id="toolbar" style="display: none;"><button id="copy">Copy markdown</button></div>
<script nonce="${nonce}" src="${markedUri.toString()}"></script>
<script nonce="${nonce}">
const vscodeApi = acquireVsCodeApi();
document.getElementById("question").textContent = ${JSON.stringify(question)};
const scopeText = ${JSON.stringify(scope)};
if (scopeText !== "") {
    const node = document.getElementById("scope");
    node.textContent = "Scope: " + scopeText;
    node.hidden = false;
}
const steps = document.getElementById("steps");
const status = document.getElementById("status");
const report = document.getElementById("report");
const toolbar = document.getElementById("toolbar");

// done.reason values that mean the loop was stopped, not satisfied (mindex
// src/research.rs DoneReason). "finalized" is deliberately absent.
const CUT_SHORT = {
    budget_exhausted: "The model ran out of lookups before it was satisfied — "
        + "re-run at a higher effort, or ask something narrower.",
    time_exhausted: "The model ran out of time before it was satisfied — "
        + "re-run at a higher effort, or ask something narrower.",
    tokens_exhausted: "The run spent its whole token budget before the model was "
        + "satisfied — the transcript grew faster than the evidence. Ask something "
        + "narrower, or raise the token budget.",
    context_exhausted: "The evidence filled the model's context window, so the "
        + "investigation stopped early. Ask something narrower: a higher effort "
        + "would hit the same wall.",
    unparseable: "The model broke protocol and had to write the report early.",
    repeated_calls: "The model kept repeating the same lookups and was stopped — "
        + "its queries were not finding the material.",
};

let currentThinking = null; // <details> block collecting the in-progress thinking
let markdown = "";
// The server's citation-provenance verdict, if it sent one. Rendered at "done"
// because that is when the report markup is built.
let citations = null;
let renderQueued = false;

// ── budget meter ─────────────────────────────────────────────────────────────
//
// The server emits progress per step and per turn, not on a timer, so the clock
// is advanced locally between events: a turn can take a minute, and a time bar
// frozen for that long reads as a hung run.
const budgetBox = document.getElementById("budget");
const cost = document.getElementById("cost");
let latest = null;
let latestAt = 0;
let ticker = null;

function fmtCount(n) {
    return n >= 1000 ? (n / 1000).toFixed(n >= 10000 ? 0 : 1) + "k" : String(n);
}

function setAxis(id, used, max, text, binding) {
    const el = document.getElementById("axis-" + id);
    const pct = max > 0 ? Math.min(100, (used / max) * 100) : 0;
    el.querySelector(".fill").style.width = pct.toFixed(1) + "%";
    el.querySelector(".val").textContent = text;
    el.classList.toggle("binding", binding === id);
}

function paintBudget() {
    if (latest === null) return;
    budgetBox.style.display = "block";
    // Interpolated: real elapsed time since the snapshot, capped at the budget so
    // the bar cannot claim more than was granted.
    const elapsed = Math.min(
        latest.max_ms || Infinity,
        latest.elapsed_ms + (Date.now() - latestAt)
    );
    const b = latest.binding;
    setAxis("time", elapsed, latest.max_ms,
        (elapsed / 1000).toFixed(0) + "/" + Math.round((latest.max_ms || 0) / 1000) + "s", b);
    setAxis("tokens", latest.tokens, latest.max_tokens,
        fmtCount(latest.tokens) + "/" + fmtCount(latest.max_tokens), b);
    setAxis("steps", latest.steps, latest.max_steps,
        latest.steps + "/" + latest.max_steps, b);
    setAxis("context", latest.context_pct, 100,
        (latest.context_pct || 0).toFixed(0) + "%", b);
    cost.textContent =
        latest.turns + " turn(s) · " + fmtCount(latest.prompt_tokens) + " prompt + " +
        fmtCount(latest.eval_tokens) + " generated" +
        (latest.num_ctx ? " · window " + fmtCount(latest.num_ctx) : "");
}

function onProgress(p) {
    latest = p;
    latestAt = Date.now();
    paintBudget();
    if (ticker === null) {
        ticker = setInterval(paintBudget, 1000);
    }
}

function freezeBudget() {
    if (ticker !== null) {
        clearInterval(ticker);
        ticker = null;
    }
}

function ensureThinking() {
    if (currentThinking === null) {
        currentThinking = document.createElement("details");
        currentThinking.className = "thinking";
        currentThinking.open = true;
        const summary = document.createElement("summary");
        summary.textContent = "thinking…";
        currentThinking.appendChild(summary);
        currentThinking.appendChild(document.createElement("pre"));
        steps.appendChild(currentThinking);
    }
    return currentThinking.querySelector("pre");
}

function closeThinking() {
    if (currentThinking !== null) {
        currentThinking.open = false;
        currentThinking.querySelector("summary").textContent = "thinking (done)";
        currentThinking = null;
    }
}

function renderReport() {
    if (renderQueued) return;
    renderQueued = true;
    setTimeout(() => {
        renderQueued = false;
        report.innerHTML = marked.parse(markdown);
    }, 120);
}

window.addEventListener("message", (e) => {
    const msg = e.data;
    switch (msg.type) {
        case "thinking": {
            ensureThinking().textContent += msg.text;
            status.textContent = "thinking…";
            break;
        }
        case "step": {
            closeThinking();
            const div = document.createElement("div");
            div.className = "step";
            const arg =
                msg.step.query ?? msg.step.name ?? msg.step.path ?? msg.step.glob ?? "";
            div.textContent = "#" + msg.step.n + " " + msg.step.action + " → " + arg + " ";
            const hits = document.createElement("span");
            hits.className = "hits";
            hits.textContent = "(" + msg.step.hits + " hits)";
            div.appendChild(hits);
            steps.appendChild(div);
            status.textContent = "researching…";
            break;
        }
        case "progress": {
            onProgress(msg.progress);
            break;
        }
        case "summary": {
            closeThinking();
            status.textContent = "writing the report…";
            markdown += msg.text;
            renderReport();
            break;
        }
        case "citations": {
            // Held until "done" renders the report — prepending a note to markup
            // that is about to be replaced would lose it.
            citations = msg.citations;
            break;
        }
        case "done": {
            closeThinking();
            // "done" repeats every progress field, so the meter freezes on the
            // run's final numbers rather than on the last mid-run snapshot.
            if (typeof msg.info.max_ms === "number") {
                onProgress(msg.info);
            }
            freezeBudget();
            status.textContent =
                "done — " + msg.info.steps + " step(s), " +
                (msg.info.turns ? msg.info.turns + " turn(s), " : "") +
                (msg.info.tokens ? fmtCount(msg.info.tokens) + " tokens, " : "") +
                (msg.info.elapsed_ms / 1000).toFixed(1) + "s";
            // Not in the visible line — it is provenance, not a number the reader
            // is watching — but on the element, so a report that looks wrong can
            // be traced to the instructions that produced it.
            if (msg.info.prompt_version) {
                status.title = "prompt " + msg.info.prompt_version;
            }
            report.innerHTML = marked.parse(markdown);
            // Anything but "finalized" means the model was stopped rather than
            // satisfied, so the report rests on partial evidence. Say so above it
            // — the reader cannot tell from the prose.
            const cutShort = CUT_SHORT[msg.info.reason || ""];
            if (cutShort) {
                const note = document.createElement("div");
                note.className = "cutshort";
                note.textContent = "\u26a0 " + cutShort;
                report.prepend(note);
            }
            // Only the failure is worth screen space. A fully verified report is
            // the expected case and saying so every time trains the reader to
            // ignore the line that matters.
            if (citations && citations.unverified > 0) {
                const note = document.createElement("div");
                note.className = "cutshort";
                note.textContent =
                    "\u26a0 " + citations.unverified + " of " + citations.total
                    + " citations name files no lookup returned in this run \u2014 the "
                    + "model invented them: "
                    + (citations.unverified_paths || []).join(", ")
                    + ". Discount the claims that rest on them.";
                report.prepend(note);
            }
            // Freshness, separately from provenance: these locations were really
            // shown to the model, but the file has been reindexed since — so the
            // claim usually holds and the line numbers may not.
            if (citations && citations.stale > 0) {
                const note = document.createElement("div");
                note.className = "cutshort";
                note.textContent =
                    "⚠ " + citations.stale + " of " + citations.total
                    + " citations point into files that were reindexed while this "
                    + "run was reading them: "
                    + (citations.stale_paths || []).join(", ")
                    + ". The line ranges may have moved.";
                report.prepend(note);
            }
            // A repaired report is worth one quiet line: it reads as authoritative
            // as any other, and the reader is entitled to know the first draft
            // cited things the run never saw.
            if (citations && citations.draft_unverified !== null
                && citations.draft_unverified !== undefined) {
                const note = document.createElement("div");
                note.className = "cutshort";
                note.textContent =
                    "↺ The first draft cited "
                    + (citations.draft_unverified + (citations.draft_path_only || 0))
                    + " locations it had not looked at; it was sent back and "
                    + "rewritten.";
                report.prepend(note);
            }
            toolbar.style.display = "block";
            break;
        }
        case "error": {
            closeThinking();
            freezeBudget();
            status.textContent = "failed";
            // research.no_report means whatever streamed as summary text is not
            // a report (typically one more tool call). Showing it would be worse
            // than showing nothing, so drop it.
            if (msg.code === "research.no_report") {
                markdown = "";
                report.innerHTML = "";
            }
            const div = document.createElement("div");
            div.className = "error";
            div.textContent = msg.detail;
            document.body.insertBefore(div, report);
            break;
        }
        case "cancelled": {
            closeThinking();
            freezeBudget();
            status.textContent = "cancelled";
            break;
        }
    }
});

document.getElementById("copy").addEventListener("click", () => {
    vscodeApi.postMessage({ type: "copy", text: markdown });
});
</script>
</body>
</html>`;
    }
}

/** A tab title short enough to survive VS Code's truncation but still tell runs apart. */
/**
 * The run's scope in one line, or "" when unscoped.
 *
 * Mirrors the server's own `ToolScope::describe` wording ("only …", "never …") so the
 * panel, the report's caveats and the journal all name the same boundary the same way.
 */
function describeScope(scope?: { include?: SearchFilter; exclude?: SearchFilter }): string {
    if (scope === undefined) {
        return "";
    }
    const parts: string[] = [];
    const add = (label: string, f?: SearchFilter): void => {
        if (f?.paths !== undefined && f.paths.length > 0) {
            parts.push(`${label} ${f.paths.join(", ")}`);
        }
        if (f?.programming_languages !== undefined && f.programming_languages.length > 0) {
            parts.push(`${label} ${f.programming_languages.join(", ")}`);
        }
    };
    add("only", scope.include);
    add("never", scope.exclude);
    return parts.join("; ");
}

function titleFor(question: string): string {
    const flat = question.replace(/\s+/g, " ").trim();
    const slug = flat.length > 34 ? `${flat.slice(0, 33)}…` : flat;
    return slug === "" ? "Research" : `Research: ${slug}`;
}

function asString(v: unknown, fallback = ""): string {
    return typeof v === "string" ? v : fallback;
}

function makeNonce(): string {
    return Array.from({ length: 24 }, () =>
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789".charAt(
            Math.floor(Math.random() * 62)
        )
    ).join("");
}
