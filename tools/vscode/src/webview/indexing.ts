/**
 * The Indexing panel's script.
 *
 * Two things make it different from the other pages. It keeps **its own clock**: a
 * `setInterval` off `snapshot.startedAt`, so the panel goes on visibly moving
 * through the long silent stretch while a batch is on the GPU and no event
 * arrives. That stretch is precisely when a reindex looked dead, and a surface
 * that only redraws when the server speaks cannot fix it. And it renders the run
 * as **one list of files, keyed by path** — the `<li>` for a file is created once
 * and afterwards only its mark changes, so a file that finishes does not appear a
 * second time as a new row.
 *
 * Everything is written with `textContent`; the only markup this file produces is
 * elements it creates itself, and geometry travels as SVG/`<progress>` attributes
 * because the CSP allows no inline style.
 */
import { el, icon, langIcon, vscodeApi } from "./host.js";
import type { IndexRunSnapshot, RunFile, RunPhase, LangTally } from "../shared/indexRun.js";

interface HostMessage {
    type: "run" | "tick";
    snapshot?: IndexRunSnapshot;
}

const api = vscodeApi<never>();

const stateIcon = el("state-icon");
const titleText = el("title-text");
const subtitle = el("subtitle");
const clock = el("clock");
const badgeForce = el("badge-force");
const badgeSymbols = el("badge-symbols");
const cancelBtn = el<HTMLButtonElement>("cancel");
const progressBar = el<HTMLProgressElement>("progress-bar");
const idleNotice = el("idle-notice");
const fallbackNotice = el("fallback-notice");
const errorNotice = el("error-notice");
const errorDetail = el("error-detail");
const errorCode = el("error-code");

const filesCard = el("files-card");
const filesList = el("files");
const filesDetail = el("files-detail");
const filesEmpty = el("files-empty");

const summaryCard = el("summary-card");
const summaryIcon = el("summary-icon");
const summaryTitle = el("summary-title");
const summaryBody = el("summary-body");
const stats = el("stats");
const rateUnit = el("rate-unit");
const rateAvg = el("rate-avg");
const ratePeak = el("rate-peak");
const sparkLine = el<HTMLElement>("spark-line");
const sparkArea = el<HTMLElement>("spark-area");
const sparkAvg = el<HTMLElement>("spark-avg");
const langTable = el("lang-table");
const langRows = el("lang-rows");
const langUnitHead = el("lang-unit-head");
const copyBtn = el<HTMLButtonElement>("copy");

let current: IndexRunSnapshot | undefined;

cancelBtn.addEventListener("click", () => api.postMessage({ type: "cancel" }));
copyBtn.addEventListener("click", () => {
    if (current !== undefined) {
        void navigator.clipboard.writeText(summaryText(current));
    }
});

window.addEventListener("message", (e: MessageEvent<HostMessage>) => {
    if (e.data.type === "run") {
        // Rows are keyed by path, so a second run over the same files would light up
        // the previous run's rows rather than starting a list of its own.
        rows.clear();
        filesList.replaceChildren();
    }
    current = e.data.snapshot;
    render();
});

// The page's own heartbeat. One second is enough to read as alive and cheap enough
// to run for the length of a whole reindex.
setInterval(render, 1000);

api.postMessage({ type: "ready" });

// ── rendering ────────────────────────────────────────────────────────────────

function render(): void {
    const s = current;
    idleNotice.hidden = s !== undefined;
    if (s === undefined) {
        titleText.textContent = "Indexing";
        subtitle.textContent = "";
        clock.textContent = "";
        cancelBtn.hidden = true;
        progressBar.hidden = true;
        filesCard.hidden = true;
        summaryCard.hidden = true;
        return;
    }

    const running = s.outcome === undefined;
    const unit = s.symbolsOnly ? "symbol rows" : "chunks";

    // ── header
    stateIcon.className = `state-dot codicon ${headIcon(s)}`;
    titleText.textContent = running ? "Indexing" : `Indexing — ${s.outcome ?? ""}`;
    badgeForce.hidden = !s.force;
    badgeSymbols.hidden = !s.symbolsOnly;
    cancelBtn.hidden = !running;
    clock.textContent = duration(elapsedMs(s));
    subtitle.textContent = subtitleFor(s, running);

    // The embed pass reports **once, on completion** — the server calls `/encode`
    // per `embed_batch` chunks, so a batch below that size sends exactly one
    // `embedded` carrying `chunks_done == chunks_total`. There is no partial number
    // to draw, so the bar goes indeterminate: a `<progress>` with no `value` says
    // "working, position unknown", which is the truth, and it animates while it
    // says it. Interpolating a fake position here would be the hyperjump dressed up.
    const settled = s.indexed + s.skipped + s.failed;
    progressBar.hidden = false;
    if (running && s.phase === "embedding") {
        progressBar.removeAttribute("value");
    } else {
        progressBar.max = Math.max(s.files, 1);
        progressBar.value = Math.min(settled, progressBar.max);
    }

    fallbackNotice.hidden = s.streamed;
    errorNotice.hidden = s.error === undefined;
    if (s.error !== undefined) {
        errorDetail.textContent = s.error.detail;
        errorCode.textContent = s.error.code;
    }

    // ── the one list
    filesCard.hidden = false;
    filesDetail.textContent = filesDetailFor(s, unit, running, settled);
    filesEmpty.hidden = s.rows.length > 0;
    renderFiles(s, unit);

    // ── the summary, at the end and only once there is one
    summaryCard.hidden = running;
    if (!running) {
        summaryIcon.className = `codicon codicon-sm ${summaryIconName(s)}`;
        summaryTitle.textContent = summaryHeading(s);
        stats.replaceChildren(...summaryStats(s, unit));
        rateUnit.textContent = `${unit}/s`;
        rateAvg.textContent = num(s.averageChunksPerSecond);
        ratePeak.textContent = num(s.peakChunksPerSecond);
        drawSpark(s);
        summaryBody.replaceChildren(...summaryLines(s).map(([n, v]) => sumLine(n, v)));
        langUnitHead.textContent = unit;
        langTable.hidden = s.languages.length === 0;
        langRows.replaceChildren(...s.languages.map(langRow));
    }
}

function headIcon(s: IndexRunSnapshot): string {
    switch (s.outcome) {
        case undefined:
            return "codicon-sync codicon-modifier-spin state-running";
        case "done":
            return "codicon-check state-done";
        case "cancelled":
            return "codicon-circle-slash state-cancelled";
        default:
            return "codicon-error state-error";
    }
}

/**
 * The line under the title — what the run is *doing*, never what it has counted.
 *
 * The counts live one line down, on the file list's own header, and printing them
 * in both places is what made the page read as three surfaces saying the same
 * thing. A run that has gone quiet says so, with the number of seconds: the
 * difference between "the GPU is busy" and "this is wedged" is exactly that
 * number, and inventing motion instead is what the whole panel exists to stop.
 */
function subtitleFor(s: IndexRunSnapshot, running: boolean): string {
    if (!running) {
        return `${s.files} file(s) · ${duration(elapsedMs(s))} · ${s.batchCount} batch(es)`;
    }
    const where =
        s.batchCount > 1 ? ` · batch ${Math.max(s.batchIndex, 1)} of ${s.batchCount}` : "";
    if (s.phase === "embedding") {
        // Named and timed rather than left to look wedged. This phase is ~96% of a
        // small run and emits nothing at all until it finishes, so the seconds are
        // the only honest thing there is to show — and they are enough, because
        // what the reader wants to know is "busy or stuck".
        const on = s.batchChunksTotal > 0 ? s.batchChunksTotal : inFlightChunks(s);
        const what =
            on > 0 ? `${on} ${s.symbolsOnly ? "symbol rows" : "chunks"}` : "the batch";
        return `on the GPU — ${what}, ${duration(Date.now() - s.phaseSince)} so far${where}`;
    }
    const quiet = Date.now() - s.lastEventAt;
    if (quiet > 3000 && s.phase !== "idle") {
        return `${PHASE_LABEL[s.phase]} — no event for ${Math.round(quiet / 1000)}s${where}`;
    }
    return `${PHASE_LABEL[s.phase]}${where}`;
}

const PHASE_LABEL: Record<RunPhase, string> = {
    idle: "waiting",
    reading: "reading files",
    preparing: "slicing",
    embedding: "embedding",
    settling: "settling",
};

/** Chunks the server has told us about for the batch it is currently embedding. */
function inFlightChunks(s: IndexRunSnapshot): number {
    return s.rows
        .filter((f) => f.state === "embedding" || f.state === "prepared")
        .reduce((n, f) => n + f.chunks, 0);
}

function filesDetailFor(
    s: IndexRunSnapshot,
    unit: string,
    running: boolean,
    settled: number
): string {
    const parts = [`${settled} of ${s.files}`];
    if (s.chunks > 0) {
        parts.push(`${s.chunks} ${unit}`);
    }
    if (running && s.chunksPerSecond !== undefined) {
        parts.push(`${Math.round(s.chunksPerSecond)} ${unit}/s`);
    }
    if (s.failed > 0) {
        parts.push(`${s.failed} unfinished`);
    }
    return parts.join(" · ");
}

// ── the file list ────────────────────────────────────────────────────────────

/** One `<li>` per path, reused across renders — see the module comment. */
interface Row {
    li: HTMLElement;
    glyph: HTMLElement;
    note: HTMLElement;
    mark: HTMLElement;
    language?: string;
    state?: RunFile["state"];
    noteText?: string;
}

const rows = new Map<string, Row>();

/**
 * The run, file by file.
 *
 * `replaceChildren` over the *cached* elements, so the rows are moved rather than
 * rebuilt: identity is what makes the mark change in place instead of the row
 * blinking away and coming back. Order is the snapshot's — insertion order,
 * newest first — so a row never moves once the list has passed it.
 */
function renderFiles(s: IndexRunSnapshot, unit: string): void {
    const ordered: HTMLElement[] = [];
    const seen = new Set<string>();
    for (const f of s.rows) {
        seen.add(f.path);
        ordered.push(rowFor(f, unit));
    }
    for (const path of [...rows.keys()]) {
        if (!seen.has(path)) {
            rows.delete(path);
        }
    }
    filesList.replaceChildren(...ordered);
}

function rowFor(f: RunFile, unit: string): HTMLElement {
    let row = rows.get(f.path);
    if (row === undefined) {
        const li = document.createElement("li");
        const glyph = document.createElement("span");
        glyph.className = "file-glyph";
        const path = document.createElement("span");
        path.className = "file-path";
        path.textContent = f.path;
        path.title = f.path;
        path.addEventListener("click", () =>
            api.postMessage({ type: "openFile", path: f.path })
        );
        const note = document.createElement("span");
        note.className = "file-note";
        const mark = document.createElement("span");
        mark.setAttribute("aria-hidden", "true");
        li.append(glyph, path, note, mark);
        row = { li, glyph, note, mark };
        rows.set(f.path, row);
    }
    // The language mark, not a spinner: it is the one thing about the row that is
    // worth a glyph and does not change, and an animation here was measured to say
    // nothing at all — every row of a batch is in the same `/encode` call, so they
    // all pulsed together for the whole embed pass.
    if (row.language !== f.language) {
        row.language = f.language;
        row.glyph.replaceChildren(
            f.language === undefined ? icon("file", true) : langIcon(f.language)
        );
    }
    if (row.state !== f.state) {
        row.state = f.state;
        row.li.className = `file-row file-${f.state}`;
        row.mark.className = `file-mark codicon codicon-${MARK[f.state]}`;
    }
    const note = noteFor(f, unit);
    if (row.noteText !== note) {
        row.noteText = note;
        row.note.textContent = note;
    }
    return row.li;
}

/** The mark at the end of the row — the one thing that moves as a file advances. */
const MARK: Record<RunFile["state"], string> = {
    prepared: "circle-outline",
    embedding: "circle-filled",
    indexed: "check",
    skipped: "dash",
    failed: "error",
    cancelled: "circle-slash",
};

/**
 * What the row says about itself.
 *
 * The symbol count is **omitted at zero**, not printed. A language with no
 * tree-sitter tags query contributes no symbols by construction — markdown never
 * will, and html, css, the data formats and a few others have no upstream query —
 * so `5 chunks · 0 symbols` on a `.md` file states a fact that reads as a broken
 * counter. What is worth showing is what the file contributed.
 */
function noteFor(f: RunFile, unit: string): string {
    if (f.state === "indexed") {
        return f.symbols > 0
            ? `${f.chunks} ${unit} · ${f.symbols} symbols`
            : `${f.chunks} ${unit}`;
    }
    if (f.note !== undefined) {
        return f.note;
    }
    if (f.chunks > 0) {
        return `${f.chunks} ${unit}`;
    }
    return f.state === "embedding" ? "on the GPU" : "sliced";
}

function elapsedMs(s: IndexRunSnapshot): number {
    return Math.max(0, (s.endedAt ?? Date.now()) - s.startedAt);
}

// ── the sparkline ────────────────────────────────────────────────────────────

const W = 240;
const H = 44;

/**
 * The rate series, scaled to the run's own maximum rather than to the visible
 * window — a per-frame rescale makes a steady rate look like it is thrashing.
 */
function drawSpark(s: IndexRunSnapshot): void {
    const samples = s.rateSamples;
    if (samples.length < 2) {
        sparkLine.setAttribute("points", "");
        sparkArea.setAttribute("d", "");
        return;
    }
    const t0 = samples[0].t;
    const span = Math.max(1, samples[samples.length - 1].t - t0);
    const top = Math.max(1, ...samples.map((p) => p.v));
    const x = (t: number): number => ((t - t0) / span) * W;
    const y = (v: number): number => H - (v / top) * (H - 2) - 1;

    const points = samples.map((p) => `${x(p.t).toFixed(1)},${y(p.v).toFixed(1)}`);
    sparkLine.setAttribute("points", points.join(" "));
    sparkArea.setAttribute("d", `M0,${H} L${points.join(" L")} L${W},${H} Z`);

    const avg = s.averageChunksPerSecond;
    const line = avg === undefined ? H : y(avg);
    sparkAvg.setAttribute("y1", String(line));
    sparkAvg.setAttribute("y2", String(line));
}

// ── pieces ───────────────────────────────────────────────────────────────────

function summaryStats(s: IndexRunSnapshot, unit: string): HTMLElement[] {
    const out = [
        stat(String(s.indexed), "indexed"),
        stat(String(s.skipped), "skipped", s.skipReasons),
    ];
    if (s.failed > 0) {
        out.push(stat(String(s.failed), "unfinished"));
    }
    out.push(stat(String(s.chunks), unit), stat(String(s.symbols), "symbols"));
    return out;
}

function stat(value: string, name: string, reasons?: Record<string, number>): HTMLElement {
    const box = document.createElement("div");
    box.className = "stat";
    const v = document.createElement("div");
    v.className = "stat-value";
    v.textContent = value;
    const n = document.createElement("div");
    n.className = "stat-name";
    n.textContent = name;
    box.append(v, n);
    const entries = Object.entries(reasons ?? {});
    if (entries.length > 0) {
        const pills = document.createElement("div");
        pills.className = "pills";
        for (const [reason, count] of entries) {
            const pill = document.createElement("span");
            pill.className = "pill";
            pill.textContent = `${count} ${reason.replace(/_/g, " ")}`;
            pills.append(pill);
        }
        box.append(pills);
    }
    return box;
}

function langRow(l: LangTally): HTMLElement {
    const tr = document.createElement("tr");
    const name = document.createElement("td");
    const cell = document.createElement("span");
    cell.className = "lang-cell";
    cell.append(langIcon(l.language));
    const label = document.createElement("span");
    label.textContent = l.language;
    cell.append(label);
    name.append(cell);
    tr.append(name);
    for (const n of [l.filesIndexed, l.chunks, l.symbols, l.filesSkipped]) {
        const td = document.createElement("td");
        td.className = "num";
        td.textContent = String(n);
        tr.append(td);
    }
    return tr;
}

function sumLine(name: string, value: string): HTMLElement {
    const row = document.createElement("div");
    row.className = "sum-line";
    const n = document.createElement("span");
    n.className = "sum-name";
    n.textContent = name;
    const v = document.createElement("span");
    v.className = "sum-value";
    v.textContent = value;
    row.append(n, v);
    return row;
}

// ── the summary ──────────────────────────────────────────────────────────────

function summaryHeading(s: IndexRunSnapshot): string {
    switch (s.outcome) {
        case "cancelled":
            return "Cancelled";
        case "error":
            return "Failed";
        default:
            return "Finished";
    }
}

function summaryIconName(s: IndexRunSnapshot): string {
    switch (s.outcome) {
        case "cancelled":
            return "codicon-circle-slash state-cancelled";
        case "error":
            return "codicon-error state-error";
        default:
            return "codicon-check-all state-done";
    }
}

/**
 * The finished run, in words — the numbers the counters above cannot carry.
 *
 * It never says "unchanged" about the aggregate. A file the server refused as
 * in-flight comes back absent from the response exactly as a hash-skipped one
 * does, so only the server's own reasons — or, failing those, the follow-up drift
 * check — can tell them apart; claiming otherwise is the dishonesty the toast's
 * in-flight correction already exists to prevent.
 */
function summaryLines(s: IndexRunSnapshot): Array<[string, string]> {
    const lines: Array<[string, string]> = [
        ["elapsed (wall clock)", duration(elapsedMs(s))],
        ["server time", s.serverElapsedMs > 0 ? duration(s.serverElapsedMs) : "—"],
        ["files", `${s.filesPosted} posted of ${s.files} selected`],
    ];
    if (!s.streamed) {
        lines.push([
            "not returned",
            "no per-file reasons — unchanged, or refused as in-flight",
        ]);
    }
    if (s.inFlight !== undefined && s.inFlight > 0) {
        lines.push([
            "still indexing",
            `${s.inFlight} refused as in-flight (from the follow-up drift check)`,
        ]);
    }
    lines.push(["batches", String(s.batchCount)]);
    return lines;
}

function summaryText(s: IndexRunSnapshot): string {
    const unit = s.symbolsOnly ? "symbol rows" : "chunks";
    const head = `${summaryHeading(s)} — ${s.files} file(s)`;
    const counts: Array<[string, string]> = [
        ["indexed", String(s.indexed)],
        [
            "skipped",
            Object.entries(s.skipReasons)
                .map(([r, n]) => `${n} ${r.replace(/_/g, " ")}`)
                .join(", ") || String(s.skipped),
        ],
        ["unfinished", String(s.failed)],
        [unit, String(s.chunks)],
        ["symbols", String(s.symbols)],
        ["average rate", `${num(s.averageChunksPerSecond)} ${unit}/s over the whole run`],
        ["peak rate", `${num(s.peakChunksPerSecond)} ${unit}/s`],
    ];
    const body = [...summaryLines(s), ...counts].map(([n, v]) => `${n}: ${v}`).join("\n");
    const langs = s.languages
        .map(
            (l) =>
                `  ${l.language}: ${l.filesIndexed} files, ${l.chunks} chunks, ` +
                `${l.symbols} symbols, ${l.filesSkipped} skipped`
        )
        .join("\n");
    return langs === "" ? `${head}\n${body}` : `${head}\n${body}\nby language:\n${langs}`;
}

// ── formatting ───────────────────────────────────────────────────────────────

function num(v: number | undefined): string {
    return v === undefined ? "—" : String(Math.round(v));
}

function duration(ms: number): string {
    const total = Math.round(ms / 1000);
    if (total < 60) {
        return `${total}s`;
    }
    const m = Math.floor(total / 60);
    const s = total % 60;
    return m < 60 ? `${m}m ${s}s` : `${Math.floor(m / 60)}h ${m % 60}m`;
}
