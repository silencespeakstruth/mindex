/**
 * Research History, webview side.
 *
 * The host owns every request and all the state that matters; this module renders
 * what it is sent and reports what the user did. Nothing here re-renders the page
 * wholesale — the search box holds a half-typed query and the list holds a
 * multi-click selection, and both would be discarded.
 *
 * **No reading pane.** Opening a run expands its row in place with the things a
 * one-line row cannot carry — provenance, ancestry, the files that have moved
 * under it, and the actions — and the report itself opens as a Markdown tab. The
 * pane this replaces rendered the report beside a 24rem list, which is a worse
 * copy of the tab and cost the list the width its own content needed.
 */

import {
    bulkSelectionNote,
    challengeBadge,
    challengeGuard,
    challengeStateLine,
    corpusCountsLine,
    gcBucketLabel,
    gcButtonLabel,
    gcProposalNote,
    standingChallenge,
    subjectLabel,
    trustBadge,
    verificationView,
    ChallengeState,
    CorpusTotalsLike,
    GcBucket,
    GC_BUCKETS,
    VerificationLike,
} from "../shared/runsFormat.js";
import { el, icon, vscodeApi } from "./host.js";
import { applyBusy, paintBusy, setEnabled } from "./ui/busy.js";

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
    /** Server-resolved subject of a challenge; absent on an older server. */
    challenged_seq?: number | null;
    challenged_title?: string | null;
}

/** One row the garbage-collection pass proposes deleting, and why. */
interface GcRow {
    id: string;
    seq: number;
    title: string;
    referenced_by_count: number;
    buckets: GcBucket[];
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
    kind: string;
    completeness: string;
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
const completenessBox = el<HTMLSelectElement>("runs-completeness");
const refreshBtn = el<HTMLButtonElement>("runs-refresh");
const countsBox = el("runs-counts");
const selectAllBtn = el<HTMLButtonElement>("runs-select-all");
const gcBtn = el<HTMLButtonElement>("runs-gc");
const gcLabel = el("runs-gc-label");
const list = el<HTMLUListElement>("runs-items");
const empty = el("runs-empty");
const emptyTitle = el("runs-empty-title");
const emptyHint = el("runs-empty-hint");
const errorBox = el("runs-error");

function clearError(): void {
    errorBox.textContent = "";
    errorBox.className = "runs-error";
    errorBox.hidden = true;
}
const loading = el("runs-loading");
const moreBtn = el<HTMLButtonElement>("runs-more");
const useBtn = el<HTMLButtonElement>("runs-use");
const useLabel = el("runs-use-label");
const deleteBtn = el<HTMLButtonElement>("runs-delete");
const deleteLabel = el("runs-delete-label");
const gcView = el("runs-gc-view");
const footBar = document.querySelector<HTMLElement>(".runs-foot");

let selected = new Set<string>();
/** The run whose row is expanded, if any. Not the selection — that is `selected`. */
let activeId: string | undefined;
/** Whether the current selection was built by a filter rather than by clicking. */
let bulkSelection = false;
/** The truncation sentence the footer appends while a bulk selection stands. */
let bulkNote: string | undefined;
/**
 * Whether the garbage-collection review has the panel.
 *
 * It takes the whole surface — it is a decision about the corpus, not a detail of
 * one row — so the list, its footer and the empty note step aside while it is up,
 * and `Cancel` puts them back untouched, expanded row and all.
 */
let gcOpen = false;

function setGcOpen(open: boolean): void {
    gcOpen = open;
    gcView.hidden = !open;
    list.hidden = open;
    if (footBar !== null) {
        footBar.hidden = open;
    }
    if (open) {
        empty.hidden = true;
    } else {
        gcView.replaceChildren();
        renderEmpty();
    }
}

/**
 * The empty middle of the panel — and *which* empty it is.
 *
 * "Nothing is stored" and "nothing matched what you asked for" call for
 * different next moves (ask a question / widen the filter), and one sentence
 * covering both is the one that helps with neither. The discriminator is the
 * controls themselves: a query or any select off `all`.
 */
function renderEmpty(): void {
    empty.hidden = gcOpen || list.childElementCount > 0;
    if (empty.hidden) {
        return;
    }
    const filtered =
        searchBox.value.trim() !== "" ||
        [freshnessBox, validityBox, kindBox, completenessBox].some((b) => b.value !== "all");
    emptyTitle.textContent = filtered ? "Nothing found" : "No research yet";
    emptyHint.textContent = filtered
        ? "No stored report matches this search and these filters."
        : "Ask a question from the Ask sidebar — a run is stored when it finishes.";
}
/**
 * The rows currently rendered, by id. The host keeps the authoritative copy; this
 * one exists so the footer can say *why* a selection cannot be used as context
 * without asking for the summaries again on every checkbox click.
 */
const rows = new Map<string, RunSummary>();

// Version bumped with the `completeness` filter: an older blob is discarded
// wholesale rather than restored half-populated, which would leave the fourth
// select disagreeing with the query the host is about to run.
const restored = api.getState();
if (restored?.v === "5") {
    searchBox.value = restored.query;
    freshnessBox.value = restored.freshness;
    validityBox.value = restored.validity;
    kindBox.value = restored.kind;
    completenessBox.value = restored.completeness;
    selected = new Set(restored.selected);
    activeId = restored.activeId;
    if (activeId !== undefined) {
        api.postMessage({ type: "select", id: activeId });
    }
}

function save(): void {
    api.setState({
        v: "5",
        query: searchBox.value,
        freshness: freshnessBox.value,
        validity: validityBox.value,
        kind: kindBox.value,
        completeness: completenessBox.value,
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
        completeness: completenessBox.value,
    });
}

// The host debounces. Doing it on both sides would compound into a wait the user
// notices, and the host is where the AbortController lives.
searchBox.addEventListener("input", sendSearch);
freshnessBox.addEventListener("change", sendSearch);
validityBox.addEventListener("change", sendSearch);
kindBox.addEventListener("change", sendSearch);
completenessBox.addEventListener("change", sendSearch);
// Not debounced — a button press is deliberate. `activeId` rides along because
// the webview owns it and the host needs to know which preview to re-fetch.
refreshBtn.addEventListener("click", () => api.postMessage({ type: "refresh", activeId }));
selectAllBtn.addEventListener("click", () => api.postMessage({ type: "selectAllMatching" }));
gcBtn.addEventListener("click", () => api.postMessage({ type: "gcPropose" }));
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

/** The `<li>` of one run, if it is on the current page. */
function rowOf(id: string): HTMLElement | null {
    return list.querySelector<HTMLElement>(`[data-id="${CSS.escape(id)}"]`);
}

/**
 * Expand a run's row, from a row click or a subject link.
 *
 * Only one is open at a time: two expanded rows would push the list around while
 * the reader is still deciding, and the panel keeps exactly one `activeId` that
 * the host's `preview`, `challengeState` and `verification` messages all key on.
 */
function selectRun(id: string): void {
    closeDetail();
    activeId = id;
    rowOf(id)?.classList.add("open");
    save();
    api.postMessage({ type: "select", id });
}

/** Collapse whatever is open, leaving the selection alone. */
function closeDetail(): void {
    for (const open of list.querySelectorAll(".runs-item.open")) {
        open.classList.remove("open");
        open.querySelector(".runs-detail")?.remove();
    }
    activeId = undefined;
    save();
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

function ghostButton(
    glyph: string,
    title: string,
    onClick: () => void,
    busyKey?: string
): HTMLButtonElement {
    const b = document.createElement("button");
    b.className = "ghost";
    b.title = title;
    if (busyKey !== undefined) {
        b.dataset.busyKey = busyKey;
    }
    const mark = icon(glyph);
    if (busyKey !== undefined) {
        mark.dataset.busyIcon = "";
    }
    b.appendChild(mark);
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
        li.classList.add("open");
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
        // The subject's seq and title come from the SERVER now. This used to hunt
        // for the subject among the loaded rows and degrade to an anonymous "open
        // subject" when it was not there — which, on a list filtered to
        // challenges, was every row.
        meta.appendChild(subjectLink(subjectLabel(run), run.challenged_run_id));
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
            () => api.postMessage({ type: "pin", id: run.id, pinned: !run.pinned }),
            // Per row: pinning one report must not freeze the pin button on
            // every other row on screen.
            `row:${run.id}`
        )
    );
    actions.appendChild(
        // No key — opening a tab is local and instant.
        ghostButton("go-to-file", "Open this report in its own tab", () =>
            api.postMessage({ type: "openRun", id: run.id })
        )
    );
    actions.appendChild(
        ghostButton(
            "trash",
            "Delete this report",
            () => api.postMessage({ type: "delete", id: run.id }),
            "delete"
        )
    );

    const head = document.createElement("div");
    head.className = "runs-row";
    head.append(check, body, actions);
    // Clicking the open row closes it. Without that the only way out of an
    // expanded row is to open a different one, which is not a way out.
    head.addEventListener("click", () => {
        if (activeId === run.id) {
            closeDetail();
        } else {
            selectRun(run.id);
        }
    });
    li.appendChild(head);
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

    setEnabled(useBtn, selected.size > 0 && invalid === 0);
    useLabel.textContent =
        selected.size === 0 ? "Use as context" : `Use ${selected.size} as context`;
    useBtn.title =
        invalid > 0
            ? `${invalid} of the selected reports are out of date; the server refuses ` +
              "them as context. Unselect them, or delete them."
            : "Hand the selected reports to the next question as background.";

    setEnabled(deleteBtn, selected.size > 0);
    deleteLabel.textContent = selected.size === 0 ? "Delete" : `Delete ${selected.size}`;
    // A bulk selection is mostly off-screen by design, so the button says how much
    // of it the user can actually see. Deleting what you have not looked at is the
    // one hazard of selecting by filter, and it must not be silent.
    deleteBtn.title = bulkNote ?? "Delete the selected reports. They cannot be recovered.";
}

/** The corpus line and the Collect-garbage button, both from the same totals. */
function renderCounts(totals: CorpusTotalsLike | undefined, legacy: boolean): void {
    countsBox.textContent = corpusCountsLine(totals);
    countsBox.title = legacy
        ? "This server is too old to report corpus totals."
        : "Every stored report for this project, and how many are still valid — " +
          "unaffected by the filters above.";
    gcLabel.textContent = gcButtonLabel(totals);
    const collectable = (totals?.gc_candidates ?? 0) > 0;
    setEnabled(gcBtn, collectable);
    // From the totals, not from `gcBtn.disabled`: while the busy layer holds the
    // button the two disagree, and the tooltip would claim there is nothing to
    // collect about a pass that is collecting it.
    gcBtn.title = collectable
        ? "Review out-of-date, stale, partial and inconclusive reports, then delete " +
          "the ones you confirm. Pinned reports are never proposed."
        : "Nothing to collect — every unpinned report is current and finished.";
    // Select-all pages the server with the current filters, which an older server
    // cannot answer for `completeness`; it stays usable, just less precise.
    setEnabled(selectAllBtn, (totals?.total ?? 0) > 0 || legacy);
}

/**
 * The expanded row: everything about a run except its report.
 *
 * Built into the `<li>` it belongs to rather than into a pane, so the answer sits
 * under the question the user clicked. If the row is no longer on the page — a
 * filter moved under an in-flight fetch — the detail is simply dropped: there is
 * nowhere honest to put it.
 */
function renderDetail(run: RunDetail): void {
    const row = rowOf(run.id);
    if (row === null) {
        return;
    }
    closeDetail();
    activeId = run.id;
    row.classList.add("open");
    save();

    const head = document.createElement("div");
    head.className = "runs-detail";
    const h = document.createElement("h4");
    h.textContent = run.question;
    head.appendChild(h);

    const openTab = document.createElement("button");
    openTab.className = "secondary";
    openTab.append(icon("go-to-file", true), document.createTextNode(" Open in a tab"));
    openTab.title =
        "Read the report as a Markdown document, so it can sit beside the code it " +
        "describes. This panel deliberately does not render it.";
    openTab.addEventListener("click", (e) => {
        e.stopPropagation();
        api.postMessage({ type: "openRun", id: run.id });
    });
    const reAsk = document.createElement("button");
    reAsk.className = "secondary";
    reAsk.append(icon("debug-restart", true), document.createTextNode(" Ask again"));
    reAsk.title =
        "Put this question back in the form with its scope and settings, and this " +
        "report as context — the usual way to follow one up.";
    reAsk.dataset.busyKey = "preview";
    reAsk.addEventListener("click", (e) => {
        e.stopPropagation();
        api.postMessage({ type: "reAsk", id: run.id });
    });
    const headActions = document.createElement("div");
    headActions.className = "runs-detail-actions";
    headActions.append(openTab, reAsk);
    head.appendChild(headActions);

    const meta = document.createElement("div");
    meta.className = "dim runs-verify-line";
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
        warn.className = "dim runs-verify-line";
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
    // The report is NOT rendered here — "Open in a tab" above is the whole
    // reading surface, and a second, worse copy of it is what this panel dropped.
    row.appendChild(head);
    paintBusy(head);
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
        const label =
            run.challenged_seq === undefined || run.challenged_seq === null
                ? "the challenged report"
                : `#${run.challenged_seq} ${run.challenged_title ?? ""}`.trim();
        const link = subjectLink(label, run.challenged_run_id ?? "");
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
        // Filled by the host once it has resolved what was said about this run.
        // A separate request, so the preview never waits on it — but it now always
        // arrives and always says something, including for the two cases `trust`
        // is correctly silent about (an inconclusive challenge, and one whose own
        // evidence has moved). Those were exactly the reports that showed nothing
        // at all about having been challenged.
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
    verify.dataset.busyKey = "verify";
    verify.addEventListener("click", () => api.postMessage({ type: "verify", id: run.id }));
    actions.appendChild(verify);

    // Rebuilt in place once the host reports the challenge state, so the button
    // reads `Challenge` or `Re-check` according to what actually exists rather
    // than to what the preview could guess when it was drawn.
    const challengeHolder = document.createElement("span");
    challengeHolder.id = "runs-challenge-action";
    challengeHolder.appendChild(challengeButton(run, undefined));
    actions.appendChild(challengeHolder);
    section.appendChild(actions);

    // Filled by the host's `verification` message after a Verify click.
    const verifyOut = document.createElement("div");
    verifyOut.id = "runs-verify-out";
    section.appendChild(verifyOut);

    return section;
}

/**
 * `Challenge` for a report with none, `Re-check` for one that already carries a
 * verdict — the wording, including the refusals, from the shared module.
 *
 * A second Challenge button would be a lie about what pressing it does: a fresh
 * run now *replaces* the standing verdict (when it reaches one), and the guard's
 * `recheck` mode is what says so.
 */
function challengeButton(
    run: RunSummary,
    state: ChallengeState | undefined
): HTMLButtonElement {
    const guard = challengeGuard(run, state);
    const b = document.createElement("button");
    b.className = "secondary";
    b.dataset.busyKey = "preview";
    if (!guard.ok) {
        b.append(icon("shield", true), document.createTextNode(" Challenge"));
        b.disabled = true;
        b.title = guard.reason;
        return b;
    }
    if (guard.mode === "recheck") {
        b.append(icon("sync", true), document.createTextNode(" Re-check"));
        b.title =
            `Standing challenge: ${guard.current}. Re-check it offline (free) or ` +
            "with a fresh run on the GPU.";
        b.addEventListener("click", () => api.postMessage({ type: "recheck", id: run.id }));
        return b;
    }
    b.append(icon("shield", true), document.createTextNode(" Challenge"));
    b.title =
        "Launch a challenge run: it re-derives this report's claims through the " +
        "tools, on the report's own scope, and scores each claim.";
    b.addEventListener("click", () => api.postMessage({ type: "challenge", id: run.id }));
    return b;
}

/**
 * What was said about the previewed run — **always a sentence**, including
 * "never challenged".
 *
 * The old version rendered a list and nothing else, so it was silent in exactly
 * the cases a reader needs told: no list meant "not challenged", "challenged
 * inconclusively" and "challenged, but that challenge has gone stale" all at
 * once. The wording lives in `runsFormat.ts` where `node --test` reaches it.
 */
function renderChallengeState(holder: HTMLElement, state: ChallengeState): void {
    holder.replaceChildren();
    const p = document.createElement("p");
    p.textContent = challengeStateLine(state);
    const standing = standingChallenge(state);
    if (standing !== undefined) {
        p.classList.add(`verdict-${standing.verdict ?? "inconclusive"}`);
    }
    holder.appendChild(p);
    if (standing === undefined) {
        return;
    }
    // The challenge itself is a report, so it opens like one — reading the
    // argument is the whole reason a verdict is worth showing.
    const open = document.createElement("button");
    open.className = "runs-subject-link";
    open.textContent = `Open challenge #${standing.seq} — ${standing.title}`;
    open.addEventListener("click", () => selectRun(standing.id));
    holder.appendChild(open);
}

/**
 * The garbage-collection review: everything proposed for deletion, grouped by
 * what is wrong with it, every row pre-checked, one confirm at the end.
 *
 * In the right pane rather than a QuickPick because a QuickPick cannot show *why*
 * a row is proposed or that four later reports were built on it — and those are
 * the two things a reviewer unchecks a row over. `Cancel` restores whatever was
 * being read; it is a review, not a mode.
 */
function renderGc(proposed: GcRow[], expected: number | null): void {
    setGcOpen(true);
    gcView.replaceChildren();
    const checks = new Map<string, HTMLInputElement>();

    const head = document.createElement("div");
    head.className = "runs-gc-head";
    const h = document.createElement("h3");
    h.textContent = "Collect garbage";
    const note = document.createElement("p");
    note.className = "runs-gc-note";
    note.textContent = gcProposalNote(proposed.length, expected ?? undefined);
    head.append(h, note);
    gcView.appendChild(head);

    const deleteBtnGc = document.createElement("button");
    const updateCount = (): void => {
        const n = [...checks.values()].filter((c) => c.checked).length;
        setEnabled(deleteBtnGc, n > 0);
        deleteBtnGc.textContent = n === 0 ? "Delete" : `Delete ${n}`;
    };

    // Each run appears in ONE group — its most serious reason, `GC_BUCKETS` being
    // in that order — and carries the rest as labels. A run listed in three groups
    // would be three checkboxes for one report, and unchecking one of them would
    // not stop the other two from deleting it.
    for (const bucket of GC_BUCKETS) {
        const inBucket = proposed.filter((r) => r.buckets[0] === bucket);
        if (inBucket.length === 0) {
            continue;
        }
        const { title, why } = gcBucketLabel(bucket);
        const group = document.createElement("div");
        group.className = "runs-gc-group";

        const groupHead = document.createElement("div");
        groupHead.className = "runs-gc-group-head";
        const groupTitle = document.createElement("span");
        groupTitle.className = "runs-gc-group-title";
        groupTitle.textContent = `${title} (${inBucket.length})`;
        const toggle = document.createElement("button");
        toggle.className = "runs-subject-link";
        toggle.textContent = "uncheck all";
        toggle.addEventListener("click", () => {
            const turningOff = toggle.textContent === "uncheck all";
            for (const r of inBucket) {
                const c = checks.get(r.id);
                if (c !== undefined) {
                    c.checked = !turningOff;
                }
            }
            toggle.textContent = turningOff ? "check all" : "uncheck all";
            updateCount();
        });
        groupHead.append(groupTitle, toggle);
        group.appendChild(groupHead);

        const whyP = document.createElement("p");
        whyP.className = "dim runs-gc-why";
        whyP.textContent = why;
        group.appendChild(whyP);

        const ul = document.createElement("ul");
        ul.className = "runs-gc-rows";
        for (const r of inBucket) {
            const li = document.createElement("li");
            li.className = "runs-gc-row";
            const check = document.createElement("input");
            check.type = "checkbox";
            check.checked = true;
            check.addEventListener("change", updateCount);
            checks.set(r.id, check);

            const rowTitle = document.createElement("span");
            rowTitle.className = "runs-gc-row-title";
            rowTitle.textContent = `#${r.seq} — ${r.title}`;
            // Every reason, not just the group it was filed under: a report can be
            // both out of date and half-written, and the reviewer is deciding on
            // the whole of it.
            rowTitle.title = r.buckets.map((b) => gcBucketLabel(b).title).join(" · ");
            li.append(check, rowTitle);
            if (r.buckets.length > 1) {
                const also = document.createElement("span");
                also.className = "dim";
                also.textContent = `+${r.buckets.length - 1}`;
                also.title = rowTitle.title;
                li.appendChild(also);
            }
            if (r.referenced_by_count > 0) {
                const dep = document.createElement("span");
                dep.className = "runs-gc-dependants";
                dep.textContent = `↩${r.referenced_by_count}`;
                dep.title =
                    `${r.referenced_by_count} later report(s) were built on this one. ` +
                    "Deleting it invalidates every one of them.";
                li.appendChild(dep);
            }
            const open = document.createElement("button");
            open.className = "runs-subject-link";
            open.textContent = "read";
            open.title = "Open this report before deciding";
            // A Markdown tab, not the row underneath: expanding the row goes
            // through `preview`, which closes the review — so reading a candidate
            // destroyed the decision screen it was being read for, and for a
            // candidate off the loaded page `renderDetail` then found no row and
            // dropped the answer too, leaving neither.
            open.addEventListener("click", () =>
                api.postMessage({ type: "openRun", id: r.id })
            );
            li.appendChild(open);
            ul.appendChild(li);
        }
        group.appendChild(ul);
        gcView.appendChild(group);
    }

    const foot = document.createElement("div");
    foot.className = "runs-gc-foot";
    deleteBtnGc.className = "primary";
    // The same `delete` key as the row and footer buttons. It stayed live after
    // being pressed, which for the one button that deletes a reviewed batch is
    // the worst place to leave a second press possible.
    deleteBtnGc.dataset.busyKey = "delete";
    deleteBtnGc.addEventListener("click", () => {
        const ids = [...checks.entries()].filter(([, c]) => c.checked).map(([id]) => id);
        api.postMessage({ type: "gcDelete", ids });
    });
    const cancel = document.createElement("button");
    cancel.className = "secondary";
    cancel.textContent = "Cancel";
    // A review, not a mode: cancelling puts the list back exactly as it was,
    // including whichever row was expanded.
    cancel.addEventListener("click", () => setGcOpen(false));
    foot.append(deleteBtnGc, cancel);
    gcView.appendChild(foot);
    updateCount();
}

/** Render one offline re-verification result under the Verify button. */
function renderVerification(
    holder: HTMLElement,
    v: VerificationLike,
    of: "self" | "challenge"
): void {
    holder.replaceChildren();
    if (of === "challenge") {
        // Without this the challenge's provenance reads as the subject's, which is
        // a worse confusion than the one this whole surface exists to fix.
        const caption = document.createElement("p");
        caption.className = "runs-verify-line dim";
        caption.textContent =
            "Re-checked the CHALLENGE run's own citations — not this report's.";
        holder.appendChild(caption);
    }
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
    // A banner survives exactly until something renders successfully.
    //
    // That rule, rather than a host-side `ok()` at eight call sites, is what
    // makes it impossible to forget one — and forgetting one is how a transient
    // failure came to sit over a list that had since loaded perfectly well.
    if (
        msg.type === "runs" ||
        msg.type === "preview" ||
        msg.type === "removed" ||
        msg.type === "gc"
    ) {
        clearError();
    }

    switch (msg.type) {
        case "runs": {
            const runs = (msg.runs ?? []) as RunSummary[];
            if (msg.reset === true) {
                list.replaceChildren();
            }
            if (Array.isArray(msg.selected)) {
                selected = new Set(msg.selected as string[]);
            }
            bulkSelection = msg.bulk === true;
            if (!bulkSelection) {
                bulkNote = undefined;
            }
            for (const run of runs) {
                list.appendChild(renderRow(run));
            }
            // Rows are rebuilt constantly; without this a row rendered during an
            // in-flight delete is the one live button on a frozen page.
            paintBusy(list);
            renderEmpty();
            moreBtn.hidden = msg.nextBeforeSeq === null || msg.nextBeforeSeq === undefined;
            refreshFooter();
            save();
            // The expanded row is attached to a `<li>`, so a list that arrives
            // *after* its detail did — a reload restoring `activeId`, a refresh —
            // has the row back but not what was open in it. Ask again; the row
            // being present is what makes this terminate.
            if (activeId !== undefined) {
                const row = rowOf(activeId);
                if (row !== null && row.querySelector(".runs-detail") === null) {
                    api.postMessage({ type: "select", id: activeId });
                }
            }
            break;
        }
        case "totals":
            renderCounts(
                (msg.totals ?? undefined) as CorpusTotalsLike | undefined,
                msg.legacy === true
            );
            break;
        case "gc":
            renderGc((msg.rows ?? []) as GcRow[], (msg.expected ?? null) as number | null);
            paintBusy(gcView);
            break;
        case "preview":
            setGcOpen(false);
            renderDetail(msg.run as RunDetail);
            break;
        case "challengeState": {
            // Async enrichment of the open row. A stale answer (the user has
            // already clicked another row) must not paint over the newer one.
            if (msg.runId !== activeId || gcOpen) {
                break;
            }
            const state = msg.state as ChallengeState;
            const holder = document.getElementById("runs-challenges");
            if (holder !== null) {
                renderChallengeState(holder, state);
            }
            // The button only now knows whether it is a first challenge or a
            // re-check, so it is rebuilt rather than drawn hopefully up front.
            const action = document.getElementById("runs-challenge-action");
            const run = rows.get(String(msg.runId));
            if (action !== null && run !== undefined) {
                action.replaceChildren(challengeButton(run, state));
                paintBusy(action);
            }
            break;
        }
        case "verification": {
            if (msg.runId !== activeId || gcOpen) {
                break;
            }
            const holder = document.getElementById("runs-verify-out");
            if (holder !== null) {
                renderVerification(
                    holder,
                    msg.v as VerificationLike,
                    msg.of === "challenge" ? "challenge" : "self"
                );
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
                // The row carries its own expanded detail now, so removing it
                // takes the detail with it — but `activeId` must still be let go,
                // or the next `challengeState` would key on a run that is gone.
                list.querySelector(`[data-id="${CSS.escape(id)}"]`)?.remove();
                selected.delete(id);
                rows.delete(id);
                if (activeId === id) {
                    activeId = undefined;
                }
            }
            // The review described a corpus that no longer exists, so it goes with
            // the rows it proposed. Left up, it kept the deleted reports on screen
            // still ticked, under a `Delete N` that would re-post ids the server
            // had already dropped — while the header above it, which the `totals`
            // message does refresh, read `Collect garbage (0)`.
            if (gcOpen) {
                setGcOpen(false);
            }
            renderEmpty();
            refreshFooter();
            save();
            break;
        }
        case "selected": {
            selected = new Set((msg.selected ?? []) as string[]);
            bulkSelection = msg.bulk === true;
            const onScreen = [...selected].filter((id) => rows.has(id)).length;
            bulkNote = bulkSelection ? bulkSelectionNote(selected.size, onScreen) : undefined;
            if (bulkSelection && msg.truncated === true) {
                // Saying "500 selected" without saying "and more match" would let a
                // user believe one delete clears the filter when it does not.
                const cap = typeof msg.cap === "number" ? msg.cap : selected.size;
                bulkNote =
                    `${bulkNote ?? ""} ${cap} is the most one delete accepts; ` +
                    "more reports match this filter.";
            }
            // Re-tick the boxes the host just selected for us.
            for (const [id, row] of rows) {
                const box = list.querySelector<HTMLInputElement>(
                    `[data-id="${CSS.escape(id)}"] input[type=checkbox]`
                );
                if (box !== null) {
                    box.checked = selected.has(row.id);
                }
            }
            refreshFooter();
            save();
            break;
        }
        case "busy":
            if (typeof msg.key === "string") {
                applyBusy(msg.key, msg.busy === true);
                // The spinner is driven by the same key as the buttons, so the
                // two can no longer disagree about whether the list is loading —
                // which the old separate `loading` channel allowed.
                if (msg.key === "list") {
                    loading.hidden = msg.busy !== true;
                }
            }
            break;
        case "error":
            errorBox.textContent = typeof msg.message === "string" ? msg.message : "";
            // Yellow when pressing the same thing again could work, red when it
            // could not: `retryable` is the only thing that tells the user which
            // of those they are looking at.
            errorBox.className = msg.retryable === true ? "runs-error warn" : "runs-error";
            errorBox.hidden = errorBox.textContent === "";
            break;
    }
});

refreshFooter();
api.postMessage({ type: "ready" });
