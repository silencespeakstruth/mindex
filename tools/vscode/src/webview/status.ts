/**
 * The Server Status panel's script.
 *
 * It renders one [`StatusSnapshot`] and nothing else — no fetching, no timers, no
 * state of its own. The panel may be closed for hours while the monitor keeps
 * refreshing, so "paint whatever the host last sent" is the whole contract, and the
 * host re-sends on every view-state change.
 *
 * The wording of the three explanatory notes below is carried over verbatim from the
 * tree view this replaced. It is the reason the tree was worth having: "ollama is
 * down" and "ollama is down and that only costs you Research" are different messages,
 * and the second one is what stops a green server reading as broken.
 */
import { el, icon, langIcon, vscodeApi } from "./host.js";
import { applyBusy, paintBusy } from "./ui/busy.js";
import {
    FailedFile,
    LanguageInventory,
    StatusSnapshot,
    UNAVAILABLE,
} from "../shared/status.js";

const api = vscodeApi<never>();

const stateIcon = el("state-icon");
const titleText = el("title-text");
const subtitle = el("subtitle");
const checked = el("checked");
const unreachable = el("unreachable");
const checks = el("checks");
const runtimeBox = el("runtime");
const inventoryBox = el("inventory");
const inventoryTotal = el("inventory-total");
const failedBox = el("failed");
const failedTotal = el("failed-total");
const retryAll = el<HTMLButtonElement>("retry-all");

const CARDS = ["health-card", "runtime-card", "inventory-card", "failed-card"];

// Two buttons, one act: the head one beside the timestamp it explains, the card
// one where the eye already is when a row has gone red. They share a busy key,
// so pressing either greys both.
for (const id of ["refresh", "health-refresh"]) {
    el(id).addEventListener("click", () => api.postMessage({ type: "refresh" }));
}
for (const id of ["settings", "unreachable-settings"]) {
    el(id).addEventListener("click", () => api.postMessage({ type: "openSettings" }));
}
retryAll.addEventListener("click", () => api.postMessage({ type: "retryAll" }));

interface HostMessage {
    snapshot?: StatusSnapshot;
    type?: string;
    key?: string;
    busy?: boolean;
}

window.addEventListener("message", (e: MessageEvent<HostMessage>) => {
    if (e.data.type === "busy" && typeof e.data.key === "string") {
        applyBusy(e.data.key, e.data.busy === true);
        return;
    }
    render(e.data.snapshot);
});

/** One list row: a fixed-width name, a value, and an optional trailing note. */
function line(
    name: string,
    value: string | HTMLElement,
    opts: { tone?: "bad" | "warn" | "muted"; note?: string; title?: string } = {}
): HTMLElement {
    const row = document.createElement("div");
    row.className = opts.tone === undefined ? "line" : `line ${opts.tone}`;
    if (opts.title !== undefined) {
        row.title = opts.title;
    }
    const n = document.createElement("span");
    n.className = "name";
    n.textContent = name;
    const v = document.createElement("span");
    v.className = "value";
    if (typeof value === "string") {
        v.textContent = value;
    } else {
        v.appendChild(value);
    }
    row.append(n, v);
    if (opts.note !== undefined) {
        const note = document.createElement("span");
        note.className = "note";
        note.textContent = opts.note;
        row.appendChild(note);
    }
    return row;
}

/**
 * The single dim row that stands in for a section whose fetch failed.
 *
 * `reason` is already a sentence when it is present — the host humanizes it — so
 * this never sees an exception's own words. It goes in the tooltip rather than
 * the row: four sections can fail at once, and four wrapped sentences would push
 * the thing the user came for off the screen.
 */
function unavailableRow(reason?: string): HTMLElement {
    return line("", "unavailable — the server answered health but not this", {
        tone: "muted",
        ...(reason !== undefined && reason !== "" ? { title: reason } : {}),
    });
}

function render(s?: StatusSnapshot): void {
    if (s === undefined) {
        titleText.textContent = "MINDex — checking…";
        return;
    }

    // One glyph in every state, colour carrying the verdict — the same rule the
    // check rows below follow. A dot that turns into a warning triangle and then
    // into an error circle reads as three unrelated indicators; what the user is
    // actually tracking is one thing changing colour.
    stateIcon.className = `state-dot codicon codicon-circle-filled state-${s.state}`;
    titleText.textContent = `MINDex — ${s.state}`;
    subtitle.textContent =
        (s.version !== undefined ? `server v${s.version} · ` : "") + s.serverUrl;
    checked.textContent = `checked ${new Date(s.at).toLocaleTimeString()}`;

    const dead = s.state === "unreachable";
    unreachable.hidden = !dead;
    if (dead) {
        el("unreachable-detail").textContent =
            s.detail ?? "The server did not answer its health check.";
    }
    // Nothing below health was fetched, so showing four empty cards would suggest
    // four separate failures rather than one.
    for (const id of CARDS) {
        el(id).hidden = dead;
    }
    if (dead) {
        return;
    }

    renderChecks(s);
    renderRuntime(s);
    renderInventory(s);
    renderFailed(s);
    // The failed rows were just rebuilt. Without this, a row re-rendered during
    // an in-flight retry is the one live button on an otherwise frozen page.
    paintBusy();
}

/**
 * What each dependency is, and what its absence costs.
 *
 * `purpose` is a standing caption — it is drawn under the name in every state,
 * because "qdrant is not answering" only means something to a reader who knows
 * what qdrant is *for*, and that was previously only in a tooltip nobody hovers
 * a green row for. Kept to one short clause: this card is scanned, not read.
 *
 * `optional` is the load-bearing column, and there is exactly one entry in it. A
 * failing optional dependency is the *only* thing that produces the server's
 * `degraded`; everything else failing is `unhealthy`. Anything not listed here is
 * treated as **required**, which is the safe direction to be wrong in: a check
 * the server adds later renders red when it fails.
 *
 * `query_embedder` used to be marked optional and was not — the server has always
 * counted it when it is present, because a dead query instance is *every search
 * failing*. It rendered as a soft warning for a hard outage.
 */
const CHECK_META: Record<string, { optional?: boolean; purpose: string; cost: string }> = {
    sqlite: {
        purpose: "Metadata database — files, chunks, research journal.",
        cost: "nothing works until it answers",
    },
    qdrant: {
        purpose: "Vector store — holds every embedded chunk.",
        cost: "search and indexing stop until it answers",
    },
    embedder: {
        purpose: "BGE-M3 — turns code and questions into vectors.",
        cost: "search and indexing stop until it answers",
    },
    query_embedder: {
        purpose: "Second embedder — the query half of a split deployment.",
        cost: "every search fails until it answers",
    },
    ollama: {
        optional: true,
        purpose: "Local model — Research runs on it.",
        cost: "only research needs it",
    },
};

/** An unlisted check: required by default, and honest about knowing no more. */
const UNKNOWN_CHECK = {
    purpose: "A dependency this version of the extension does not know.",
    cost: "treated as required until it is described here",
};

function renderChecks(s: StatusSnapshot): void {
    checks.replaceChildren();
    for (const [name, state] of Object.entries(s.checks ?? {})) {
        checks.appendChild(checkRow(name, state));
    }
    if (checks.childElementCount === 0) {
        checks.appendChild(unavailableRow());
    }
}

/**
 * One dependency, as a 2×2 block: **identity on the left, verdict on the right**.
 *
 * - top-left: the dot, the name, and the `optional` badge immediately after it —
 *   optionality is a property of the dependency, so it belongs to its name;
 * - bottom-left: what the thing is *for*;
 * - top-right: `ok` / `failed`;
 * - bottom-right: what its state costs, under the word it qualifies.
 *
 * The arrangement is the point. Everything that says *what this is* stacks on one
 * edge and everything that says *how it is doing* stacks on the other, so a column
 * of rows scans as two columns rather than as five kinds of text taking turns.
 * Both captions ride in the same row of the grid, which is what keeps the two
 * halves aligned when either of them wraps.
 *
 * **One glyph in every state.** A failing optional dependency is yellow and a
 * failing required one is red — the colour is the severity, and swapping the dot
 * for a triangle and then for an error circle made three indicators out of one.
 * The word beside it is what carries the state without colour, which is what the
 * shape used to be doing badly.
 */
function checkRow(name: string, state: string): HTMLElement {
    const meta = CHECK_META[name] ?? UNKNOWN_CHECK;
    // `=== "ok"` and never `startsWith("error")`: an older server sends
    // `"error: <reason>"` and this one sends `"error"`, and only one of those
    // tests survives both.
    const ok = state === "ok";
    const optional = meta.optional === true;
    const tone = ok ? "ok" : optional ? "warn" : "bad";

    const row = document.createElement("div");
    row.className = `line check check-${tone}`;
    row.title = ok
        ? meta.purpose + (optional ? "\nOptional — the server is fine without it." : "")
        : meta.purpose +
          (optional
              ? '\nOptional, which is why the server says "degraded" rather than ' +
                '"unhealthy".'
              : "\nRequired — the server reports itself unhealthy without it.") +
          "\nThe reason is in the server's log.";

    const label = document.createElement("div");
    label.className = "check-name";
    label.append(icon("circle-filled", true), document.createTextNode(name));
    if (optional) {
        // Beside the name, not down with the captions: it qualifies *this
        // dependency*, permanently, and never its current answer.
        const badge = document.createElement("span");
        badge.className = "badge";
        badge.textContent = "optional";
        badge.title = "The server stays healthy without this one.";
        label.appendChild(badge);
    }

    const value = document.createElement("div");
    value.className = "check-state";
    value.textContent = ok ? "ok" : "failed";

    const purpose = document.createElement("div");
    purpose.className = "check-purpose";
    purpose.textContent = meta.purpose;

    // Drawn in both states, under the word it qualifies: it is what this
    // dependency's state costs, and a caption that appeared only on failure
    // would make every red row taller than the green one above it.
    const impact = document.createElement("div");
    impact.className = "check-impact";
    impact.textContent = ok ? `otherwise: ${meta.cost}` : meta.cost;

    row.append(label, value, purpose, impact);
    return row;
}

function renderRuntime(s: StatusSnapshot): void {
    runtimeBox.replaceChildren();
    if (s.runtime === undefined || s.runtime === UNAVAILABLE) {
        runtimeBox.appendChild(unavailableRow(s.sectionErrors?.runtime));
        return;
    }
    const r = s.runtime;
    runtimeBox.append(
        line("indexing claims", String(r.indexing_claims), {
            title: "Files a request or worker currently holds. Not a queue depth.",
        }),
        line("GC", r.gc_running ? "running" : "idle", {
            title: "Garbage collection is global and runs at most one pass at a time.",
        }),
        line("SQLite pool", poolMeter(r.pool_available, r.pool_size), {
            title:
                "Connections free of the fixed-size pool. Sustained zero means " +
                "requests are queuing behind the database.",
        })
    );
    // Server-wide file counts are deliberately not shown: they sum every project the
    // server has ever indexed, which says nothing about this workspace and reads as a
    // contradiction next to the per-project Failed list below.
}

/**
 * The pool as one inline bar plus `available/size`.
 *
 * It used to draw one cell per connection with a sentence beside it, which at a pool
 * of 64 was a row of 64 boxes and three lines of wrapped text for a number. The bar is
 * fixed-width and the colour carries the reading: sustained zero available is the
 * condition worth noticing — requests are queuing behind the database — and that is
 * the only thing the old row said that anyone needed.
 */
function poolMeter(available: number, size: number): HTMLElement {
    const free = size > 0 ? available / size : 0;
    const wrap = document.createElement("span");
    wrap.className = "row";

    const meter = document.createElement("span");
    meter.className = `meter meter-${free >= 0.5 ? "ok" : free >= 0.2 ? "warn" : "bad"}`;
    const fill = document.createElement("span");
    fill.className = "fill";
    // Set through CSSOM, which the CSP does not govern — only a parsed `style=`
    // attribute is blocked. Same mechanism the research panel's progress bar uses.
    fill.style.width = `${Math.round(free * 100)}%`;
    meter.appendChild(fill);

    const label = document.createElement("span");
    label.className = "meter-label";
    label.textContent = `${available}/${size}`;

    wrap.append(meter, label);
    return wrap;
}

function renderInventory(s: StatusSnapshot): void {
    inventoryBox.replaceChildren();
    if (s.inventory === undefined) {
        inventoryTotal.textContent = "";
        // Absent is not the same as a failed fetch: there is no project, or the server
        // is too old to publish an inventory. Neither is an error.
        inventoryBox.appendChild(
            line("", "no project inventory — open a workspace with a .mindex file", {
                tone: "muted",
            })
        );
        return;
    }
    if (s.inventory === UNAVAILABLE) {
        inventoryTotal.textContent = "";
        inventoryBox.appendChild(unavailableRow(s.sectionErrors?.inventory));
        return;
    }

    const rows = Object.entries(s.inventory).sort(([a], [b]) => a.localeCompare(b));
    const files = rows.reduce((n, [, v]) => n + v.files, 0);
    const active = rows.reduce((n, [, v]) => n + v.chunks_active, 0);
    inventoryTotal.textContent = `${rows.length} languages · ${files} files · ${active} chunks`;

    if (rows.length === 0) {
        inventoryBox.appendChild(
            line("", "this project has nothing indexed yet", { tone: "muted" })
        );
        return;
    }

    const grid = document.createElement("div");
    grid.className = "inv";
    for (const h of ["language", "files", "indexed", "chunks"]) {
        const cell = document.createElement("div");
        cell.className = h === "language" ? "h" : "h num";
        cell.textContent = h;
        grid.appendChild(cell);
    }
    for (const [name, v] of rows) {
        grid.append(...inventoryRow(name, v));
    }
    inventoryBox.appendChild(grid);
}

function inventoryRow(name: string, v: LanguageInventory): HTMLElement[] {
    const searchable = v.chunks_active > 0;
    const lang = document.createElement("div");
    lang.className = searchable ? "lang" : "lang empty";
    // The mark stays the language's own in both states: it is what makes the row
    // scannable, and swapping it for a warning glyph answered "is this searchable?" by
    // discarding the answer to "which language is this?".
    lang.append(langIcon(name), document.createTextNode(name));
    if (!searchable) {
        lang.appendChild(icon("warning", true));
    }
    // A language with files but no live chunks is called out rather than hidden: it
    // *is* indexed, and searching it will still find nothing.
    lang.title = searchable
        ? `${v.indexed_files} of ${v.files} files indexed` +
          (v.chunks_deleted > 0
              ? `\n${v.chunks_deleted} chunks soft-deleted, awaiting GC`
              : "")
        : "Indexed but unsearchable: every file either failed or was too short to " +
          "produce a chunk. Not offered as a filter.";

    return [
        lang,
        ...[v.files, v.indexed_files, v.chunks_active].map((n) => {
            const cell = document.createElement("div");
            cell.className = searchable ? "num" : "num empty";
            cell.textContent = String(n);
            return cell;
        }),
    ];
}

/**
 * The dead-letter list, and the one card that is **absent** rather than empty.
 *
 * A section reading "Failed files · 0 · nothing failed" is three ways of saying the
 * same thing, permanently, about the state this server is in nearly all the time — it
 * costs a quarter of the panel to report a non-event. Gone, it says the same thing by
 * saying nothing, and its reappearance is then a signal in itself. The count is red
 * when it is not zero for the same reason: by then it is the only number here anyone
 * needs to see.
 *
 * "Unavailable" is not zero and keeps the card: a fetch that failed is something to
 * know about, and hiding it would claim nothing failed on no evidence.
 */
function renderFailed(s: StatusSnapshot): void {
    failedBox.replaceChildren();
    const card = el("failed-card");
    if (s.failed === undefined || s.failed === UNAVAILABLE) {
        card.hidden = false;
        failedTotal.textContent = "";
        failedTotal.className = "card-note dim";
        retryAll.hidden = true;
        failedBox.appendChild(unavailableRow(s.sectionErrors?.failed));
        return;
    }
    card.hidden = s.failed.length === 0;
    if (s.failed.length === 0) {
        return;
    }
    failedTotal.textContent = String(s.failed.length);
    failedTotal.className = "card-note failed-count";
    retryAll.hidden = false;
    for (const f of s.failed) {
        failedBox.appendChild(failedRow(f));
    }
}

function failedRow(f: FailedFile): HTMLElement {
    const row = document.createElement("div");
    row.className = "line";
    row.title =
        `${f.programming_language}, last change ` +
        `${new Date(f.status_updated_at * 1000).toLocaleString()}\n` +
        "Retry requeues it for the retry worker (~60 s).";

    const open = document.createElement("button");
    open.className = "path grow";
    open.append(langIcon(f.programming_language), document.createTextNode(f.path));
    open.addEventListener("click", () => api.postMessage({ type: "openFile", path: f.path }));

    const note = document.createElement("span");
    note.className = "note";
    note.textContent = `retries ${f.retry_count}`;

    const retry = document.createElement("button");
    retry.className = "secondary retry";
    retry.dataset.busyKey = `row:${f.path}`;
    const glyph = icon("debug-restart", true);
    glyph.dataset.busyIcon = "";
    retry.append(glyph);
    retry.appendChild(document.createTextNode("Retry"));
    retry.addEventListener("click", () =>
        api.postMessage({ type: "retryFile", path: f.path })
    );

    row.append(open, note, retry);
    return row;
}

// A panel restored from a hidden state starts blank; the host re-sends on every
// view-state change, but asking is what covers the very first paint.
api.postMessage({ type: "ready" });
