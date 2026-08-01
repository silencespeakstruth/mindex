/**
 * Research History, webview side.
 *
 * The host owns every request and all the state that matters; this module renders
 * what it is sent and reports what the user did. Nothing here re-renders the page
 * wholesale — the search box holds a half-typed query and the list holds a
 * multi-click selection, and both would be discarded.
 */

import { marked } from "marked";
import {
    challengeBadge,
    challengeGuard,
    trustBadge,
    verificationView,
    VerificationLike,
} from "../shared/runsFormat.js";
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
    references_count: number;
    referenced_by_count: number;
    context: RunDependency[];
    kind: string;
    challenged_run_id: string | null;
    challenge_verdict: string | null;
    trust: string;
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

/** One valid challenge aimed at the previewed run, as the host resolves them. */
interface ChallengeAgainst {
    id: string;
    seq: number;
    title: string;
    verdict: string | null;
    valid: boolean;
}

/** Survives a hidden tab; restored on reload so a half-built selection is not lost. */
interface State {
    v: string;
    query: string;
    freshness: string;
    validity: string;
    kind: string;
    selected: string[];
    /**
     * The report currently in the right pane. Persisted with the rest: the query
     * and the selection already survived a reload while the pane came back blank,
     * which reads as "the panel forgot what I was reading" rather than as a
     * deliberate reset.
     */
    activeId?: string;
}

const api = vscodeApi<State>();

const searchBox = el<HTMLInputElement>("runs-search");
const freshnessBox = el<HTMLSelectElement>("runs-freshness");
const validityBox = el<HTMLSelectElement>("runs-validity");
const kindBox = el<HTMLSelectElement>("runs-kind");
const refreshBtn = el<HTMLButtonElement>("runs-refresh");
const list = el<HTMLUListElement>("runs-items");
const empty = el("runs-empty");
const errorBox = el("runs-error");
const loading = el("runs-loading");
const moreBtn = el<HTMLButtonElement>("runs-more");
const useBtn = el<HTMLButtonElement>("runs-use");
const useLabel = el("runs-use-label");
const deleteBtn = el<HTMLButtonElement>("runs-delete");
const deleteLabel = el("runs-delete-label");
const preview = el("runs-preview");

/** What the right pane shows when nothing is open. */
const PREVIEW_PLACEHOLDER = "Select a run to read its report.";

let selected = new Set<string>();
let activeId: string | undefined;
/**
 * The rows currently rendered, by id. The host keeps the authoritative copy; this
 * one exists so the footer can say *why* a selection cannot be used as context
 * without asking for the summaries again on every checkbox click.
 */
const rows = new Map<string, RunSummary>();

const restored = api.getState();
if (restored?.v === "4") {
    searchBox.value = restored.query;
    freshnessBox.value = restored.freshness;
    validityBox.value = restored.validity;
    kindBox.value = restored.kind;
    selected = new Set(restored.selected);
    activeId = restored.activeId;
    if (activeId !== undefined) {
        api.postMessage({ type: "select", id: activeId });
    }
}

function save(): void {
    api.setState({
        v: "4",
        query: searchBox.value,
        freshness: freshnessBox.value,
        validity: validityBox.value,
        kind: kindBox.value,
        selected: [...selected],
        activeId,
    });
}

function sendSearch(): void {
    save();
    api.postMessage({
        type: "search",
        q: searchBox.value,
        freshness: freshnessBox.value,
        validity: validityBox.value,
        kind: kindBox.value,
    });
}

// The host debounces. Doing it on both sides would compound into a wait the user
// notices, and the host is where the AbortController lives.
searchBox.addEventListener("input", sendSearch);
freshnessBox.addEventListener("change", sendSearch);
validityBox.addEventListener("change", sendSearch);
kindBox.addEventListener("change", sendSearch);
// Not debounced — a button press is deliberate. `activeId` rides along because
// the webview owns it and the host needs to know which preview to re-fetch.
refreshBtn.addEventListener("click", () => api.postMessage({ type: "refresh", activeId }));
moreBtn.addEventListener("click", () => api.postMessage({ type: "more" }));
useBtn.addEventListener("click", () => api.postMessage({ type: "useAsContext" }));
deleteBtn.addEventListener("click", () => api.postMessage({ type: "deleteSelected" }));

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

/** Open a run in the right pane, from a row click or a subject link. */
function selectRun(id: string): void {
    activeId = id;
    for (const other of list.querySelectorAll(".runs-item")) {
        other.classList.toggle("selected-row", (other as HTMLElement).dataset.id === id);
    }
    save();
    api.postMessage({ type: "select", id });
}

/** A meta-row link from a challenge to the report it attacked. */
function subjectLink(label: string, subjectId: string): HTMLButtonElement {
    const b = document.createElement("button");
    b.className = "runs-subject-link";
    b.textContent = label;
    b.title = "Open the challenged report";
    b.addEventListener("click", (e) => {
        e.stopPropagation();
        selectRun(subjectId);
    });
    return b;
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
    rows.set(run.id, run);
    const li = document.createElement("li");
    li.className = "runs-item";
    li.dataset.id = run.id;
    if (run.id === activeId) {
        li.classList.add("selected-row");
    }

    const check = document.createElement("input");
    check.type = "checkbox";
    check.checked = selected.has(run.id);
    // Selection means "these rows", not "these context runs" — an out-of-date
    // report is exactly the kind worth deleting in a batch, so disabling its
    // checkbox would put the pruning workflow out of reach to protect a submit
    // that has its own guard. `Use as context` is what refuses instead.
    check.title = run.valid
        ? "Select — for context, or to delete"
        : "Select — this report cannot be used as context, but it can be deleted.";
    check.addEventListener("click", (e) => e.stopPropagation());
    check.addEventListener("change", () => {
        if (check.checked) {
            selected.add(run.id);
        } else {
            selected.delete(run.id);
        }
        save();
        refreshFooter();
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
    const who = document.createElement("span");
    who.textContent = `${run.model} · ${run.effort}`;
    meta.appendChild(who);
    // Retention is otherwise invisible: `expires_at` has always been on the wire and
    // rendered nowhere, so a report simply vanished one day. Pinned is the loud
    // state; a countdown appears only once it is close enough to act on.
    if (run.pinned) {
        meta.appendChild(badge("pinned", "pinned", "Kept indefinitely — never reaped."));
    } else if (run.expires_at !== null) {
        const days = Math.floor((run.expires_at - Date.now() / 1000) / 86400);
        if (days <= 7) {
            meta.appendChild(
                badge(
                    days <= 0 ? "expiring" : `${days}d left`,
                    "expiring",
                    "The retention sweep will delete this report. Pin it to keep it."
                )
            );
        }
    }
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
        // The word on the badge is the *reason*, not the verdict. "invalid" states
        // that something is wrong and leaves the user to hover for what — and the
        // three causes call for three different actions: reindex, accept the loss,
        // or fix the ancestor.
        meta.appendChild(
            badge(
                invalidLabel(run),
                "invalid",
                run.invalid_reason === "stale"
                    ? "The files this report read have moved; it no longer describes the tree."
                    : run.invalid_reason === "context_deleted"
                      ? "A report in this one's context chain was deleted."
                      : "A report in this one's context chain is no longer valid."
            )
        );
    }
    // The refutation channel. A challenge row says what it concluded; every row
    // says what valid challenges concluded about IT. `unchallenged` is silent —
    // it merely means untested, and a badge on every row is a badge on none.
    // (Wording lives in shared/runsFormat.ts so `node --test` reaches it.)
    const chBadge = challengeBadge(run);
    if (chBadge !== undefined) {
        meta.appendChild(badge(chBadge.label, chBadge.kind, chBadge.title));
    }
    if (run.kind === "challenge" && run.challenged_run_id !== null) {
        // The subject may not be on this page — its seq is then unknowable
        // client-side, and the link says so. Selecting still works either way:
        // the host fetches the detail by id.
        const subject = rows.get(run.challenged_run_id);
        meta.appendChild(
            subjectLink(
                subject === undefined ? "⚔ open subject" : `⚔ challenges #${subject.seq}`,
                run.challenged_run_id
            )
        );
    }
    const tBadge = trustBadge(run);
    if (tBadge !== undefined) {
        meta.appendChild(badge(tBadge.label, tBadge.kind, tBadge.title));
    }
    if (run.references_count > 0) {
        meta.appendChild(
            badge(
                `⤷${run.references_count}`,
                "deps",
                `Built directly on ${run.references_count} earlier report(s).\n` +
                    `Whole chain (${run.context.length}):\n` +
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
    if (run.referenced_by_count > 0) {
        meta.appendChild(
            badge(
                `↩${run.referenced_by_count}`,
                "refd",
                `${run.referenced_by_count} later report(s) were built on this one. ` +
                    "Deleting it invalidates every one of them."
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
        ghostButton("go-to-file", "Open this report in its own tab", () =>
            api.postMessage({ type: "openRun", id: run.id })
        )
    );
    actions.appendChild(
        ghostButton("trash", "Delete this report", () =>
            api.postMessage({ type: "delete", id: run.id })
        )
    );

    li.append(check, body, actions);
    li.addEventListener("click", () => selectRun(run.id));
    return li;
}

/** The badge word for an invalid run: why, not that. */
function invalidLabel(run: RunSummary): string {
    switch (run.invalid_reason) {
        case "stale":
            return `${run.files_moved}/${run.files_total} files changed`;
        case "context_deleted":
            return "context deleted";
        default:
            return "context out of date";
    }
}

/**
 * The two footer actions. They read the same selection and disable for different
 * reasons: context refuses an invalid pick (the server would 400), while deleting
 * one is the point.
 */
function refreshFooter(): void {
    const picked = [...selected].map((id) => rows.get(id));
    const invalid = picked.filter((r) => r !== undefined && !r.valid).length;

    useBtn.disabled = selected.size === 0 || invalid > 0;
    useLabel.textContent =
        selected.size === 0 ? "Use as context" : `Use ${selected.size} as context`;
    useBtn.title =
        invalid > 0
            ? `${invalid} of the selected reports are out of date; the server refuses ` +
              "them as context. Unselect them, or delete them."
            : "Hand the selected reports to the next question as background.";

    deleteBtn.disabled = selected.size === 0;
    deleteLabel.textContent = selected.size === 0 ? "Delete" : `Delete ${selected.size}`;
}

/** Return the right pane to its placeholder. */
function clearPreview(): void {
    preview.replaceChildren();
    const p = document.createElement("p");
    p.className = "runs-placeholder dim";
    p.textContent = PREVIEW_PLACEHOLDER;
    preview.appendChild(p);
}

function renderDetail(run: RunDetail): void {
    preview.replaceChildren();

    const head = document.createElement("div");
    head.className = "runs-detail-head";
    const h = document.createElement("h3");
    h.textContent = `#${run.seq} — ${run.question}`;
    head.appendChild(h);

    const openTab = document.createElement("button");
    openTab.className = "secondary";
    openTab.append(icon("go-to-file", true), document.createTextNode(" Open in a tab"));
    openTab.title =
        "Open this report as a Markdown document, so it can sit beside the code it " +
        "describes.";
    openTab.addEventListener("click", () => api.postMessage({ type: "openRun", id: run.id }));
    const reAsk = document.createElement("button");
    reAsk.className = "secondary";
    reAsk.append(icon("debug-restart", true), document.createTextNode(" Ask again"));
    reAsk.title =
        "Put this question back in the form with its scope and settings, and this " +
        "report as context — the usual way to follow one up.";
    reAsk.addEventListener("click", () => api.postMessage({ type: "reAsk", id: run.id }));
    const headActions = document.createElement("div");
    headActions.className = "row";
    headActions.append(openTab, reAsk);
    head.appendChild(headActions);

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

    head.appendChild(challengeSection(run));

    // The context ancestry: every report this one leaned on, so the reader knows
    // whose claims it inherited — and which of those have since gone bad.
    if (run.context.length > 0) {
        const p = document.createElement("p");
        p.textContent =
            run.references_count === run.context.length
                ? `Built on ${run.context.length} earlier report(s):`
                : `Built on ${run.references_count} earlier report(s), ` +
                  `${run.context.length} in the whole chain:`;
        head.appendChild(p);
        const ul = document.createElement("ul");
        ul.className = "runs-deps";
        for (const d of run.context) {
            const li = document.createElement("li");
            li.className = "runs-dep";
            const state = document.createElement("span");
            state.className = `dim dep-${d.state}`;
            state.textContent = d.state;
            if (d.state === "deleted") {
                const label = document.createElement("span");
                label.textContent = "deleted report";
                li.append(state, label);
            } else {
                // A dependency is a report, so it opens like one. Reading what a
                // claim was inherited from is the whole reason the chain is shown.
                const link = document.createElement("button");
                link.textContent = `#${d.seq} ${d.title ?? ""}`.trim();
                link.title = "Open this report in its own tab";
                link.addEventListener("click", () =>
                    api.postMessage({ type: "openRun", id: d.id })
                );
                li.append(state, link);
            }
            ul.appendChild(li);
        }
        head.appendChild(ul);
    }

    if (run.referenced_by_count > 0) {
        const p = document.createElement("p");
        p.className = "dim";
        p.textContent =
            `${run.referenced_by_count} later report(s) were built on this one; ` +
            "deleting it would invalidate them.";
        head.appendChild(p);
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

/**
 * The challenge half of the preview. For a challenge run: what it attacked and
 * what it concluded. For a research run: what valid challenges concluded about
 * it (filled asynchronously by the host's `challenges` message), plus the two
 * actions — offline re-verification and launching a challenge.
 */
function challengeSection(run: RunDetail): HTMLElement {
    const section = document.createElement("div");
    section.className = "runs-section";

    if (run.kind === "challenge") {
        const p = document.createElement("p");
        const subject = rows.get(run.challenged_run_id ?? "");
        const link = subjectLink(
            subject === undefined
                ? "open the challenged report"
                : `#${subject.seq} ${subject.title}`,
            run.challenged_run_id ?? ""
        );
        const verdict =
            run.challenge_verdict === null
                ? "inconclusive — its verdict turn produced nothing parseable, which is not an acquittal"
                : run.challenge_verdict;
        p.append(
            document.createTextNode("⚔ This run challenged "),
            link,
            document.createTextNode(`. Verdict: ${verdict}.`)
        );
        section.appendChild(p);
    } else {
        const tBadge = trustBadge(run);
        if (tBadge !== undefined) {
            const p = document.createElement("p");
            p.textContent = tBadge.title;
            section.appendChild(p);
        }
        // Filled by the host once it has resolved the challenges aimed at this
        // run — a separate request, so the preview never waits on it.
        const holder = document.createElement("div");
        holder.id = "runs-challenges";
        section.appendChild(holder);
    }

    const actions = document.createElement("div");
    actions.className = "row";
    const verify = document.createElement("button");
    verify.className = "secondary";
    verify.append(icon("verified", true), document.createTextNode(" Verify"));
    verify.title =
        "Re-check this report offline against the journalled evidence — provenance " +
        "and staleness, no model involved. Staleness is measured against the index " +
        "now, so re-running it after a reindex is the point.";
    verify.addEventListener("click", () => api.postMessage({ type: "verify", id: run.id }));
    actions.appendChild(verify);

    const guard = challengeGuard(run);
    const challenge = document.createElement("button");
    challenge.className = "secondary";
    challenge.append(icon("shield", true), document.createTextNode(" Challenge"));
    if (guard.ok) {
        challenge.title =
            "Launch a challenge run: it re-derives this report's claims through the " +
            "tools, on the report's own scope, and scores each claim.";
    } else {
        challenge.disabled = true;
        challenge.title = guard.reason;
    }
    challenge.addEventListener("click", () =>
        api.postMessage({ type: "challenge", id: run.id })
    );
    actions.appendChild(challenge);
    section.appendChild(actions);

    // Filled by the host's `verification` message after a Verify click.
    const verifyOut = document.createElement("div");
    verifyOut.id = "runs-verify-out";
    section.appendChild(verifyOut);

    return section;
}

/** Render the list of challenges aimed at the previewed run. */
function renderChallenges(holder: HTMLElement, challenges: ChallengeAgainst[]): void {
    holder.replaceChildren();
    if (challenges.length === 0) {
        return;
    }
    const p = document.createElement("p");
    p.textContent = `Challenged ${challenges.length} time(s):`;
    holder.appendChild(p);
    const ul = document.createElement("ul");
    ul.className = "runs-deps";
    for (const c of challenges) {
        const li = document.createElement("li");
        li.className = "runs-dep";
        const state = document.createElement("span");
        const verdict = c.verdict ?? "inconclusive";
        state.className = `dim verdict-${verdict}`;
        state.textContent = c.valid ? verdict : `${verdict} (stale)`;
        state.title = c.valid
            ? "This challenge's own evidence still stands."
            : "This challenge's own evidence has moved; it no longer counts toward trust.";
        const link = document.createElement("button");
        link.textContent = `#${c.seq} ${c.title}`;
        link.title = "Open this challenge's report";
        link.addEventListener("click", () => selectRun(c.id));
        li.append(state, link);
        ul.appendChild(li);
    }
    holder.appendChild(ul);
}

/** Render one offline re-verification result under the Verify button. */
function renderVerification(holder: HTMLElement, v: VerificationLike): void {
    holder.replaceChildren();
    const view = verificationView(v);
    for (const [text, cls] of [
        [view.provenanceLine, "runs-verify-line"],
        [view.spansNote, "runs-verify-line dim"],
        [view.stalenessLine, "runs-verify-line"],
        [view.warning, "runs-verify-warning"],
    ] as const) {
        if (text !== undefined) {
            const p = document.createElement("p");
            p.className = cls;
            p.textContent = text;
            holder.appendChild(p);
        }
    }
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
            refreshFooter();
            save();
            break;
        }
        case "preview":
            renderDetail(msg.run as RunDetail);
            break;
        case "challenges": {
            // Async enrichment of the open preview. A stale answer (the user has
            // already clicked another row) must not paint over the newer pane.
            if (msg.runId !== activeId) {
                break;
            }
            const holder = document.getElementById("runs-challenges");
            if (holder !== null) {
                renderChallenges(holder, (msg.list ?? []) as ChallengeAgainst[]);
            }
            break;
        }
        case "verification": {
            if (msg.runId !== activeId) {
                break;
            }
            const holder = document.getElementById("runs-verify-out");
            if (holder !== null) {
                renderVerification(holder, msg.v as VerificationLike);
            }
            break;
        }
        case "updated": {
            // Replace the one row in place. Re-rendering the list would scroll it back
            // to the top for a change to a single button.
            const run = msg.run as RunSummary;
            const row = list.querySelector(`[data-id="${CSS.escape(run.id)}"]`);
            row?.replaceWith(renderRow(run));
            break;
        }
        case "removed": {
            // One or many: the batch delete posts the same message with a list, so
            // there is one removal path rather than two that can disagree.
            const ids = Array.isArray(msg.ids)
                ? (msg.ids as unknown[]).map(String)
                : [String(msg.id)];
            for (const id of ids) {
                list.querySelector(`[data-id="${CSS.escape(id)}"]`)?.remove();
                selected.delete(id);
                rows.delete(id);
                // The report on the right outlives its row otherwise: a deleted run
                // stayed fully rendered, with `activeId` pointing at an id nothing
                // could resolve, and the next render highlighted no row at all.
                if (activeId === id) {
                    activeId = undefined;
                    clearPreview();
                }
            }
            empty.hidden = list.childElementCount > 0;
            refreshFooter();
            save();
            break;
        }
        case "selected":
            selected = new Set((msg.selected ?? []) as string[]);
            refreshFooter();
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

refreshFooter();
api.postMessage({ type: "ready" });
