/**
 * The Research output panel's script.
 *
 * A faithful port of what used to be an inline `<script>` in `researchView.ts` — same
 * events, same wording, same interpolated clock. What it gains by being a real module
 * is a type checker over the SSE payloads, which is worth more here than anywhere
 * else in the extension: this file's whole job is to render fields whose shape is a
 * server contract, and the failure mode of getting one wrong is a silently missing
 * line rather than an error.
 */
// Imports carry an explicit `.js`: this is emitted as a real ES module and loaded by
// the browser, which does not guess extensions the way a bundler would.
import { marked } from "marked";
import { el, icon, pageData, vscodeApi } from "./host.js";

interface PageData {
    question: string;
    scope: string;
    /** The stored reports handed to this run as background. */
    context: { id: string; seq: number; title: string }[];
    /**
     * This panel streams a challenge run. Suppresses the post-done Challenge
     * button — a challenge of a challenge is refused server-side.
     */
    isChallenge?: boolean;
}

interface Progress {
    steps: number;
    max_steps: number;
    elapsed_ms: number;
    max_ms: number;
    tokens: number;
    max_tokens: number;
    prompt_tokens: number;
    eval_tokens: number;
    num_ctx: number;
    context_pct: number;
    turns: number;
    binding: string;
    /**
     * Where the elapsed time went — a slow model and a busy GPU produce the same
     * `elapsed_ms`. `0` when the Ollama in use reports no durations.
     */
    generation_ms?: number;
    model_load_ms?: number;
    unaccounted_ms?: number;
    eval_tokens_per_second?: number;
}

interface Done extends Progress {
    reason?: string;
    prompt_version?: string;
    /**
     * How the finished run can be named afterwards — **null when the journal write
     * failed**, which is also how a report rejected by the markdown gate arrives.
     * Nullable rather than absent: a fabricated id would name a run nothing can
     * fetch.
     */
    run_id?: string | null;
    seq?: number | null;
}

interface Step {
    n: number;
    action: string;
    hits: number;
    query?: string;
    pattern?: string;
    name?: string;
    path?: string;
    glob?: string;
    text?: string;
    plan?: string;
}

interface Citations {
    /**
     * The report came from the server, not the model — the report window expired.
     * It cites nothing, so every count below reads as a flawless report would.
     */
    server_written?: boolean;
    /**
     * Nothing was shown to the run, and it was holding an earlier report. The
     * stronger claim of the two: not "this report may be empty" but "this report is
     * somebody else's, restated".
     */
    hearsay_only?: boolean;
    total: number;
    unverified: number;
    unverified_paths?: string[];
    stale: number;
    stale_paths?: string[];
    draft_unverified?: number | null;
    draft_path_only?: number | null;
}

/** A challenge stream's conclusion about the report it attacked. */
interface Verdict {
    challenged_run_id: string;
    overall: string | null;
    grounded: boolean;
    claims: { claim: string; verdict: string }[];
}

interface Excerpt {
    path: string;
    start_line: number;
    end_line: number;
    code: string;
}

interface Excerpts {
    excerpts: Excerpt[];
    total: number;
    truncated: boolean;
}

type Incoming =
    | { type: "thinking"; text: string }
    | { type: "step"; step: Step }
    | { type: "progress"; progress: Progress }
    | { type: "summary"; text: string }
    | { type: "citations"; citations: Citations }
    | { type: "excerpts"; excerpts: Excerpts }
    | { type: "verdict"; verdict: Verdict }
    | { type: "done"; info: Done }
    | { type: "error"; detail: string; code?: string }
    | { type: "cancelled" };

/**
 * `done.reason` values that mean the loop was *stopped* rather than satisfied
 * (the server's `DoneReason`). "finalized" is deliberately absent — a run that
 * finished has nothing to warn about.
 */
const CUT_SHORT: Record<string, string> = {
    budget_exhausted:
        "The model ran out of lookups before it was satisfied — re-run at a higher " +
        "effort, or ask something narrower.",
    time_exhausted:
        "The model ran out of time before it was satisfied — re-run at a higher " +
        "effort, or ask something narrower.",
    tokens_exhausted:
        "The run spent its whole token budget before the model was satisfied — the " +
        "transcript grew faster than the evidence. Ask something narrower, or raise " +
        "the token budget.",
    context_exhausted:
        "The evidence filled the model's context window, so the investigation stopped " +
        "early. Ask something narrower: a higher effort would hit the same wall.",
    unparseable: "The model broke protocol and had to write the report early.",
    repeated_calls:
        "The model kept repeating the same lookups and was stopped — its queries were " +
        "not finding the material.",
};

const api = vscodeApi<never>();
const data = pageData<PageData>() ?? { question: "", scope: "", context: [] };

const stepsBox = el("steps");
const status = el("status");
const report = el("report");
const toolbar = el("toolbar");
const budgetBox = el("budget");
const cost = el("cost");

el("question").textContent = data.question;
if (data.scope !== "") {
    const node = el("scope");
    node.textContent = `Scope: ${data.scope}`;
    node.hidden = false;
}
// The provenance line. Rendered once from page data rather than on `done`: it is
// true from the moment the run starts, and a reader watching the steps go by is
// exactly who wants to know what it was told beforehand.
if (data.context.length > 0) {
    const node = el("deps");
    node.hidden = false;
    node.appendChild(document.createTextNode("Built on: "));
    data.context.forEach((run, i) => {
        if (i > 0) {
            node.appendChild(document.createTextNode(", "));
        }
        const link = document.createElement("button");
        link.className = "deplink";
        link.textContent = `#${run.seq} ${run.title}`;
        link.title = "Open this report in its own tab";
        link.addEventListener("click", () =>
            api.postMessage({ type: "openRun", id: run.id, seq: run.seq, title: run.title })
        );
        node.appendChild(link);
    });
}

let currentThinking: HTMLDetailsElement | null = null;
let markdown = "";
/** The server's citation verdict, held until `done` builds the report markup. */
let citations: Citations | null = null;
/** A challenge run's verdict, held like `citations` until `done` renders. */
let verdict: Verdict | null = null;
/** Verbatim code for the report's verified citations, appended after it renders. */
let excerpts: Excerpts | null = null;
let renderQueued = false;

// ── budget meter ─────────────────────────────────────────────────────────────
//
// The server emits progress per step and per turn, not on a timer, so the clock is
// advanced locally between events: a turn can take a minute, and a time bar frozen
// for that long reads as a hung run.
let latest: Progress | null = null;
let latestAt = 0;
let ticker: ReturnType<typeof setInterval> | null = null;

function fmtCount(n: number): string {
    return n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : String(n);
}

function setAxis(id: string, used: number, max: number, text: string, binding: string): void {
    const node = el(`axis-${id}`);
    const pct = max > 0 ? Math.min(100, (used / max) * 100) : 0;
    (node.querySelector(".fill") as HTMLElement).style.width = `${pct.toFixed(1)}%`;
    (node.querySelector(".val") as HTMLElement).textContent = text;
    node.classList.toggle("binding", binding === id);
}

function paintBudget(): void {
    if (latest === null) {
        return;
    }
    budgetBox.hidden = false;
    // Interpolated: real elapsed time since the snapshot, capped at the budget so the
    // bar cannot claim more than was granted.
    const elapsed = Math.min(
        latest.max_ms || Infinity,
        latest.elapsed_ms + (Date.now() - latestAt)
    );
    const b = latest.binding;
    setAxis(
        "time",
        elapsed,
        latest.max_ms,
        `${(elapsed / 1000).toFixed(0)}/${Math.round((latest.max_ms || 0) / 1000)}s`,
        b
    );
    setAxis(
        "tokens",
        latest.tokens,
        latest.max_tokens,
        `${fmtCount(latest.tokens)}/${fmtCount(latest.max_tokens)}`,
        b
    );
    setAxis("steps", latest.steps, latest.max_steps, `${latest.steps}/${latest.max_steps}`, b);
    setAxis("context", latest.context_pct, 100, `${(latest.context_pct || 0).toFixed(0)}%`, b);
    cost.textContent =
        `${latest.turns} turn(s) · ${fmtCount(latest.prompt_tokens)} prompt + ` +
        `${fmtCount(latest.eval_tokens)} generated` +
        (latest.num_ctx ? ` · window ${fmtCount(latest.num_ctx)}` : "");
}

function onProgress(p: Progress): void {
    latest = p;
    latestAt = Date.now();
    paintBudget();
    ticker ??= setInterval(paintBudget, 1000);
}

function freezeBudget(): void {
    if (ticker !== null) {
        clearInterval(ticker);
        ticker = null;
    }
}

function ensureThinking(): HTMLPreElement {
    if (currentThinking === null) {
        const details = document.createElement("details");
        details.className = "thinking";
        details.open = true;
        const summary = document.createElement("summary");
        summary.textContent = "thinking…";
        details.append(summary, document.createElement("pre"));
        stepsBox.appendChild(details);
        currentThinking = details;
    }
    return currentThinking.querySelector("pre") as HTMLPreElement;
}

function closeThinking(): void {
    if (currentThinking !== null) {
        currentThinking.open = false;
        (currentThinking.querySelector("summary") as HTMLElement).textContent =
            "thinking (done)";
        currentThinking = null;
    }
}

function renderReport(): void {
    if (renderQueued) {
        return;
    }
    renderQueued = true;
    setTimeout(() => {
        renderQueued = false;
        report.innerHTML = marked.parse(markdown) as string;
    }, 120);
}

/** A warning banner above the report. Prepended, so the last added reads first. */
function prependNote(glyph: string, text: string): void {
    const note = document.createElement("div");
    note.className = "cutshort";
    note.append(
        icon(glyph, true),
        Object.assign(document.createElement("span"), { textContent: text })
    );
    report.prepend(note);
}

/**
 * The indexed code behind the report's verified citations, appended below it.
 *
 * Collapsed by default: it is reference material, not the answer, and a report
 * followed by several screens of source reads as though the source were the point.
 * Built with DOM nodes and `textContent` rather than through `marked` — this is
 * code from the index, and round-tripping it through a Markdown parser would let a
 * fenced block inside a source file end the fence that was supposed to contain it.
 */
function renderExcerpts(data: Excerpts): void {
    if (data.excerpts.length === 0) {
        return;
    }
    const box = document.createElement("details");
    box.className = "excerpts";
    const head = document.createElement("summary");
    head.textContent =
        `Cited code — ${data.excerpts.length} location(s), read from the index` +
        (data.truncated ? " (some were too large to include)" : "");
    box.appendChild(head);
    for (const e of data.excerpts) {
        const label = document.createElement("div");
        label.className = "excerpt-path";
        label.textContent = `${e.path}:${e.start_line}-${e.end_line}`;
        const pre = document.createElement("pre");
        const code = document.createElement("code");
        code.textContent = e.code;
        pre.appendChild(code);
        box.append(label, pre);
    }
    report.appendChild(box);
}

function renderStep(step: Step): void {
    const div = document.createElement("div");
    div.className = "step";
    const arg =
        step.query ??
        step.pattern ??
        step.name ??
        step.path ??
        step.glob ??
        step.text ??
        step.plan ??
        "";
    const n = document.createElement("span");
    n.className = "n";
    n.textContent = `#${step.n}`;
    const action = document.createElement("span");
    action.className = "action";
    action.textContent = step.action;
    const argNode = document.createElement("span");
    argNode.className = "arg grow";
    argNode.textContent = arg;
    const hits = document.createElement("span");
    hits.className = "hits";
    hits.textContent = `${step.hits} hits`;
    div.append(n, action, argNode, hits);
    stepsBox.appendChild(div);
}

window.addEventListener("message", (e: MessageEvent<Incoming>) => {
    const msg = e.data;
    switch (msg.type) {
        case "thinking": {
            ensureThinking().textContent += msg.text;
            status.textContent = "thinking…";
            break;
        }
        case "step": {
            closeThinking();
            renderStep(msg.step);
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
            // Held until "done" renders the report — prepending a note to markup that
            // is about to be replaced would lose it.
            citations = msg.citations;
            break;
        }
        case "excerpts": {
            // Held for the same reason as `citations`: `done` replaces the report
            // markup wholesale, so anything appended before it would be lost.
            excerpts = msg.excerpts;
            break;
        }
        case "verdict": {
            // Held like `citations`; only challenge streams send it.
            verdict = msg.verdict;
            break;
        }
        case "done": {
            closeThinking();
            // "done" repeats every progress field, so the meter freezes on the run's
            // final numbers rather than on the last mid-run snapshot.
            if (typeof msg.info.max_ms === "number") {
                onProgress(msg.info);
            }
            freezeBudget();
            status.textContent =
                `done — ${msg.info.steps} step(s), ` +
                (msg.info.turns ? `${msg.info.turns} turn(s), ` : "") +
                (msg.info.tokens ? `${fmtCount(msg.info.tokens)} tokens, ` : "") +
                `${(msg.info.elapsed_ms / 1000).toFixed(1)}s`;
            // Not in the visible line — it is provenance, not a number the reader is
            // watching — but on the element, so a report that looks wrong can be
            // traced to the instructions that produced it.
            if (msg.info.prompt_version !== undefined) {
                status.title = `prompt ${msg.info.prompt_version}`;
            }
            report.innerHTML = marked.parse(markdown) as string;
            // After the parse that replaces the markup, before the notes that
            // prepend to it: the excerpts belong below the report, the warnings
            // above it.
            if (excerpts !== null) {
                renderExcerpts(excerpts);
            }
            // Anything but "finalized" means the model was stopped rather than
            // satisfied, so the report rests on partial evidence. Say so above it —
            // the reader cannot tell from the prose.
            const cutShort = CUT_SHORT[msg.info.reason ?? ""];
            if (cutShort !== undefined) {
                prependNote("warning", cutShort);
            }
            // Before any count, because it changes what they mean. A server-written
            // report cites nothing, so it scores zero unverified and zero stale —
            // exactly what a flawless report scores. Without this line the reader
            // sees a clean citation record on a report no model wrote.
            if (citations !== null && citations.server_written === true) {
                prependNote(
                    "warning",
                    "The report window expired before the model wrote anything, so this " +
                        "report was assembled by the server from what the run had found. " +
                        "It cites nothing — the citation counts below say zero because " +
                        "there was nothing to check, not because everything checked out."
                );
            }
            // Prepended last, so it sits above the server-written note: this is the
            // stronger claim of the two. `server_written` says no model wrote the
            // report; this says no evidence went into it at all, and the prose came
            // from an earlier run's report rather than from the code.
            if (citations !== null && citations.hearsay_only === true) {
                prependNote(
                    "warning",
                    "No lookup returned a single location in this run, and the run was " +
                        "given earlier reports to work from — so this text rests on what " +
                        "another run wrote about an earlier state of the tree, not on the " +
                        "code as it stands. Treat it as a pointer to re-investigate, not " +
                        "as a finding."
                );
            }
            // Only the failure is worth screen space. A fully verified report is the
            // expected case, and saying so every time trains the reader to ignore the
            // line that matters.
            if (citations !== null && citations.unverified > 0) {
                prependNote(
                    "warning",
                    `${citations.unverified} of ${citations.total} citations name files no ` +
                        `lookup returned in this run — the model invented them: ` +
                        `${(citations.unverified_paths ?? []).join(", ")}. Discount the claims ` +
                        `that rest on them.`
                );
            }
            // Freshness, separately from provenance: these locations really were shown
            // to the model, but the file has been reindexed since — so the claim
            // usually holds and the line numbers may not.
            if (citations !== null && citations.stale > 0) {
                prependNote(
                    "warning",
                    `${citations.stale} of ${citations.total} citations point into files that ` +
                        `were reindexed while this run was reading them: ` +
                        `${(citations.stale_paths ?? []).join(", ")}. The line ranges may have moved.`
                );
            }
            // A repaired report is worth one quiet line: it reads as authoritative as
            // any other, and the reader is entitled to know the first draft cited
            // things the run never saw.
            if (
                citations !== null &&
                citations.draft_unverified !== null &&
                citations.draft_unverified !== undefined
            ) {
                prependNote(
                    "discard",
                    `The first draft cited ${
                        citations.draft_unverified + (citations.draft_path_only ?? 0)
                    } locations it had not looked at; it was sent back and rewritten.`
                );
            }
            // A challenge stream's conclusion, above everything else: it is what
            // the run was for. Inconclusive is stated as such — it must not read
            // as the subject being confirmed.
            if (verdict !== null) {
                const claims = verdict.claims
                    .map((c) => `${c.verdict.toUpperCase()}: ${c.claim}`)
                    .join("\n");
                if (verdict.overall === null) {
                    prependNote(
                        "warning",
                        "Challenge verdict: INCONCLUSIVE — the verdict turn produced " +
                            "nothing parseable. The subject report is challenged, not " +
                            "acquitted."
                    );
                } else {
                    prependNote(
                        verdict.overall === "confirmed" ? "discard" : "warning",
                        `Challenge verdict: ${verdict.overall.toUpperCase()}` +
                            (verdict.grounded
                                ? ""
                                : " (capped — this challenge verified no citations of its own)") +
                            (claims ? `\n${claims}` : "")
                    );
                }
            }
            // The journal is best-effort by contract, so `run_id` is null when the
            // write failed or the markdown gate rejected the draft. That is the
            // whole wire signal, and it used to be rendered nowhere: the report was
            // on screen, the History panel had no row for it, and nothing connected
            // the two. Say it here, where the reader still has the text to copy.
            if (msg.info.run_id === null || msg.info.run_id === undefined) {
                prependNote(
                    "warning",
                    "This report was NOT saved to Research History — it cannot be " +
                        "reopened or reused as context later. Copy it now if you want to " +
                        "keep it."
                );
            } else if (data.isChallenge !== true) {
                // The report just became a stored run, so it can be attacked. Not
                // on challenge panels: the server refuses a challenge of a
                // challenge, and offering one here would defer that refusal.
                const runId = msg.info.run_id;
                const challengeBtn = document.createElement("button");
                challengeBtn.className = "secondary";
                challengeBtn.append(
                    icon("shield", true),
                    document.createTextNode(" Challenge this report")
                );
                challengeBtn.title =
                    "Launch a challenge run: it re-derives this report's claims " +
                    "through the tools, on the report's own scope, and scores each claim.";
                challengeBtn.addEventListener("click", () =>
                    api.postMessage({ type: "challenge", id: runId })
                );
                toolbar.appendChild(challengeBtn);
            }
            toolbar.hidden = false;
            break;
        }
        case "error": {
            closeThinking();
            freezeBudget();
            status.textContent = "failed";
            // research.no_report means whatever streamed as summary text is not a
            // report (typically one more tool call). Showing it would be worse than
            // showing nothing, so drop it.
            if (msg.code === "research.no_report") {
                markdown = "";
                report.innerHTML = "";
            }
            const div = document.createElement("div");
            div.className = "error";
            div.textContent = msg.detail;
            report.parentNode?.insertBefore(div, report);
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

el("copy").addEventListener("click", () => {
    api.postMessage({ type: "copy", text: markdown });
});
