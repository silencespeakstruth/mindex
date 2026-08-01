import * as vscode from "vscode";
import type { MindexApi, ResearchRunSummary } from "./api";
import { ICON } from "./icons";
import { openResearchReport } from "./researchDocs";
import { debounce } from "./shared/debounce";
import { asTrust } from "./shared/runsFormat";

/** How long the box waits after the last keystroke before it asks the server. */
const SEARCH_DEBOUNCE_MS = 250;

/** How many runs one page of the picker offers. */
const PAGE = 50;

/**
 * Pick stored runs to feed the next research question as context.
 *
 * A **QuickPick**, not a panel, because this is the hot path: choosing context is
 * something done on the way to asking a question, and a flow that opens an editor
 * tab, takes a selection and sends it back to the sidebar is three surfaces for one
 * decision. The picker is a true overlay — it appears over whatever is on screen,
 * takes the choice, and leaves nothing behind.
 *
 * Only **valid** runs are offered, and that is a deliberate narrowing rather than a
 * shortcut: the server refuses an invalid run as context with a 400, so listing one
 * here would only defer the same refusal to submit time — the argument the History
 * panel's disabled checkbox already records. The full corpus, invalid rows included,
 * stays one button away.
 *
 * Returns the chosen runs, or `undefined` when the user dismissed the picker —
 * which must not be confused with an empty array, the deliberate "no context".
 */
export async function pickContextRuns(
    api: MindexApi,
    guid: string,
    current: readonly ResearchRunSummary[],
    openHistory: () => void
): Promise<ResearchRunSummary[] | undefined> {
    const pick = vscode.window.createQuickPick<RunItem>();
    pick.title = "Research context";
    pick.placeholder = "Search stored reports by title, question or body…";
    pick.canSelectMany = true;
    // The server does the searching (it can see the report bodies), so VS Code's
    // own filtering must be off or it would filter the filtered list again by
    // label alone — hiding rows that matched on their body.
    pick.matchOnDescription = false;
    pick.matchOnDetail = false;
    pick.keepScrollPosition = true;

    const historyButton: vscode.QuickInputButton = {
        iconPath: new vscode.ThemeIcon(ICON.researchHistory),
        tooltip: "Open Research History (shows out-of-date reports too)",
    };
    pick.buttons = [historyButton];

    /** Everything ever shown, so a selection survives a query that hides it. */
    const known = new Map<string, ResearchRunSummary>();
    for (const r of current) {
        known.set(r.id, r);
    }
    const selectedIds = new Set(current.map((r) => r.id));

    let inFlight: AbortController | undefined;
    let disposed = false;

    const render = (runs: ResearchRunSummary[]): void => {
        for (const r of runs) {
            known.set(r.id, r);
        }
        // Anything already picked stays on screen even when the current query does
        // not match it — otherwise typing a new query silently drops the picks made
        // under the previous one, and `selectedItems` can only reference items that
        // are in `items`.
        const shown = [...runs];
        const shownIds = new Set(shown.map((r) => r.id));
        for (const id of selectedIds) {
            const r = known.get(id);
            if (r !== undefined && !shownIds.has(id)) {
                shown.push(r);
            }
        }
        pick.items = shown.map(toItem);
        pick.selectedItems = pick.items.filter((i) => selectedIds.has(i.run.id));
    };

    const load = async (q: string): Promise<void> => {
        inFlight?.abort();
        const controller = new AbortController();
        inFlight = controller;
        pick.busy = true;
        try {
            const page = await api.listResearchRuns(
                guid,
                { q: q || undefined, valid: true, limit: PAGE },
                controller.signal
            );
            if (controller.signal.aborted || disposed) {
                return;
            }
            render(page.runs);
        } catch (e) {
            // `listResearchRuns` REJECTS on abort, so a superseded keystroke lands
            // here on every search. It is not a failure.
            if (!isAbort(e) && !disposed) {
                pick.items = [];
                pick.placeholder = `Could not list stored reports: ${messageOf(e)}`;
            }
        } finally {
            if (inFlight === controller) {
                inFlight = undefined;
                pick.busy = false;
            }
        }
    };

    const search = debounce(SEARCH_DEBOUNCE_MS, (q: string) => void load(q));

    return await new Promise<ResearchRunSummary[] | undefined>((resolve) => {
        let accepted: ResearchRunSummary[] | undefined;

        pick.onDidChangeValue((q) => search(q));
        // Track the selection continuously: `onDidAccept` fires with the *visible*
        // selection, and a pick made under an earlier query may not be visible.
        pick.onDidChangeSelection((items) => {
            selectedIds.clear();
            for (const i of items) {
                selectedIds.add(i.run.id);
                known.set(i.run.id, i.run);
            }
        });
        pick.onDidTriggerButton((b) => {
            if (b === historyButton) {
                openHistory();
                pick.hide();
            }
        });
        pick.onDidTriggerItemButton(({ item }) => {
            void openResearchReport(guid, item.run);
        });
        pick.onDidAccept(() => {
            accepted = [...selectedIds]
                .map((id) => known.get(id))
                .filter((r): r is ResearchRunSummary => r !== undefined)
                .sort((a, b) => a.seq - b.seq);
            pick.hide();
        });
        pick.onDidHide(() => {
            disposed = true;
            search.cancel();
            inFlight?.abort();
            pick.dispose();
            resolve(accepted);
        });

        pick.show();
        void load("");
    });
}

/**
 * Browse the stored corpus and open one report.
 *
 * The read-only twin of [`pickContextRuns`]: same rows, same server-side search,
 * single select, and accepting opens the report in a tab rather than attaching it
 * to a question. It exists so *reading* past research is a popup away too — the
 * two-pane History panel stays for comparing and pruning, which is what two panes
 * are actually for.
 *
 * Unlike the context picker this offers **every** run, valid or not: an
 * out-of-date report is still worth reading (it is how you learn the names), and
 * nothing here will be submitted anywhere.
 */
export async function browseResearchRuns(
    api: MindexApi,
    guid: string,
    openHistory: () => void
): Promise<void> {
    const pick = vscode.window.createQuickPick<RunItem>();
    pick.title = "Stored research";
    pick.placeholder = "Search stored reports by title, question or body…";
    pick.matchOnDescription = false;
    pick.matchOnDetail = false;

    const historyButton: vscode.QuickInputButton = {
        iconPath: new vscode.ThemeIcon(ICON.researchHistory),
        tooltip: "Open Research History (two panes, multi-select, delete)",
    };
    pick.buttons = [historyButton];

    let inFlight: AbortController | undefined;
    let disposed = false;

    const load = async (q: string): Promise<void> => {
        inFlight?.abort();
        const controller = new AbortController();
        inFlight = controller;
        pick.busy = true;
        try {
            const page = await api.listResearchRuns(
                guid,
                { q: q || undefined, limit: PAGE },
                controller.signal
            );
            if (!controller.signal.aborted && !disposed) {
                pick.items = page.runs.map(toItem);
            }
        } catch (e) {
            if (!isAbort(e) && !disposed) {
                pick.items = [];
                pick.placeholder = `Could not list stored reports: ${messageOf(e)}`;
            }
        } finally {
            if (inFlight === controller) {
                inFlight = undefined;
                pick.busy = false;
            }
        }
    };

    const search = debounce(SEARCH_DEBOUNCE_MS, (q: string) => void load(q));

    await new Promise<void>((resolve) => {
        pick.onDidChangeValue((q) => search(q));
        pick.onDidTriggerButton((b) => {
            if (b === historyButton) {
                openHistory();
                pick.hide();
            }
        });
        pick.onDidTriggerItemButton(({ item }) => void openResearchReport(guid, item.run));
        pick.onDidAccept(() => {
            const chosen = pick.selectedItems[0];
            if (chosen !== undefined) {
                void openResearchReport(guid, chosen.run);
            }
            pick.hide();
        });
        pick.onDidHide(() => {
            disposed = true;
            search.cancel();
            inFlight?.abort();
            pick.dispose();
            resolve();
        });
        pick.show();
        void load("");
    });
}

interface RunItem extends vscode.QuickPickItem {
    run: ResearchRunSummary;
}

/**
 * One row. The label carries identity, the description carries significance and
 * the detail carries the question — so the list is scannable at a glance and still
 * answers "why would I pick this one" without a second surface.
 */
function toItem(run: ResearchRunSummary): RunItem {
    const age = describeAge(run.created_at);
    // Kind and trust ride the description: a refuted report is still offered —
    // offering is a hint, validating is a contract — but picking one as context
    // for the next question is a decision worth making with the verdict in view.
    const trust = asTrust(run.trust);
    const marks = [
        run.kind === "challenge" ? `$(${ICON.challenge}) challenge` : null,
        trust !== undefined && trust !== "unchallenged"
            ? `${trust === "refuted" ? `$(${ICON.invalid}) ` : trust === "disputed" ? `$(${ICON.outOfDate}) ` : ""}trust: ${trust}`
            : null,
        run.pinned ? `$(pinned)` : null,
        run.references_count > 0 ? `$(references)${run.references_count}` : null,
        run.referenced_by_count > 0 ? `$(link)${run.referenced_by_count}` : null,
    ].filter((m): m is string => m !== null);

    // Only the browse picker ever shows these — the context picker asks the server
    // for valid runs only — but the mark belongs on the row rather than on the
    // caller, so the two lists cannot describe the same run differently.
    const state = !run.valid
        ? `$(${ICON.invalid}) `
        : run.stale
          ? `$(${ICON.outOfDate}) `
          : "";

    return {
        run,
        label: `${state}#${run.seq} ${run.title}`,
        description: [`${age}`, run.model, ...marks].join(" · "),
        detail: run.question.trim().replace(/\s+/g, " ").slice(0, 160),
        buttons: [
            {
                iconPath: new vscode.ThemeIcon(ICON.openReport),
                tooltip: "Open this report in a tab",
            },
        ],
    };
}

function describeAge(createdAt: number): string {
    const secs = Math.max(0, Date.now() / 1000 - createdAt);
    const mins = Math.round(secs / 60);
    if (mins < 60) {
        return `${mins}m ago`;
    }
    const hours = Math.round(mins / 60);
    if (hours < 24) {
        return `${hours}h ago`;
    }
    return `${Math.round(hours / 24)}d ago`;
}

function isAbort(e: unknown): boolean {
    return e instanceof Error && e.name === "AbortError";
}

function messageOf(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
}
