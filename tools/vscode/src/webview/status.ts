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

el("refresh").addEventListener("click", () => api.postMessage({ type: "refresh" }));
for (const id of ["settings", "unreachable-settings"]) {
    el(id).addEventListener("click", () => api.postMessage({ type: "openSettings" }));
}
retryAll.addEventListener("click", () => api.postMessage({ type: "retryAll" }));

window.addEventListener("message", (e: MessageEvent<{ snapshot?: StatusSnapshot }>) => {
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

/** The single dim row that stands in for a section whose fetch failed. */
function unavailableRow(): HTMLElement {
    return line("", "unavailable — the server answered health but not this", {
        tone: "muted",
    });
}

function render(s?: StatusSnapshot): void {
    if (s === undefined) {
        titleText.textContent = "MINDex — checking…";
        return;
    }

    stateIcon.className = `state-dot codicon codicon-${
        { ok: "circle-filled", degraded: "warning", unreachable: "error" }[s.state]
    } state-${s.state}`;
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
}

/**
 * What each dependency is, and what its absence costs.
 *
 * `optional` is the load-bearing column. The server's health stays `"ok"` without
 * Ollama or a split query embedder, so a red row beside a green header reads as a
 * contradiction unless the row says it is allowed to be red. Everything not listed
 * here is treated as **required**: a check the server adds later then renders red when
 * it fails, which is the safe direction to be wrong in.
 */
const CHECK_META: Record<string, { optional?: boolean; costs: string }> = {
    sqlite: { costs: "The metadata database. Nothing works without it." },
    qdrant: { costs: "The vector store. Search and indexing both need it." },
    embedder: { costs: "BGE-M3. Indexing and search both embed through it." },
    query_embedder: {
        optional: true,
        costs: "The second embedder instance, only present on a split deployment.",
    },
    ollama: { optional: true, costs: "The local model behind Research." },
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
 * One dependency: a coloured dot, its name, its verdict in the same colour, and an
 * `optional` badge where one applies.
 *
 * A failing optional dependency is **yellow, not red** — it is the difference between
 * "this server is broken" and "one feature is unavailable", and it is the whole reason
 * the row carries a sentence saying which.
 */
function checkRow(name: string, state: string): HTMLElement {
    const meta = CHECK_META[name];
    const ok = state === "ok";
    const optional = meta?.optional === true;
    const tone = ok ? "ok" : optional ? "warn" : "bad";

    const row = document.createElement("div");
    row.className = `line check check-${tone}`;
    row.title = ok
        ? (meta?.costs ?? "") + (optional ? "\nOptional — the server is fine without it." : "")
        : `${state}\n${meta?.costs ?? ""}` +
          (optional
              ? '\nOptional dependency, which is why Health can still say "ok".'
              : "\nRequired — the server reports itself degraded without it.");

    const label = document.createElement("span");
    label.className = "name check-name";
    label.append(
        icon(ok ? "circle-filled" : optional ? "warning" : "error", true),
        document.createTextNode(name)
    );

    const value = document.createElement("span");
    value.className = "value check-state";
    value.textContent = state;

    row.append(label, value);
    if (optional) {
        const badge = document.createElement("span");
        badge.className = "badge";
        badge.textContent = "optional";
        badge.title = "The server stays healthy without this one.";
        row.appendChild(badge);
    }
    if (!ok && name === "ollama") {
        // The one failure worth spelling out on the row itself: it is the dependency a
        // user is most likely to be missing, and Research is the only thing it costs.
        const note = document.createElement("span");
        note.className = "note";
        note.textContent = "only Research needs it";
        row.appendChild(note);
    }
    return row;
}

function renderRuntime(s: StatusSnapshot): void {
    runtimeBox.replaceChildren();
    if (s.runtime === undefined || s.runtime === UNAVAILABLE) {
        runtimeBox.appendChild(unavailableRow());
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
        inventoryBox.appendChild(unavailableRow());
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
        failedBox.appendChild(unavailableRow());
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
    retry.append(icon("debug-restart", true));
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
