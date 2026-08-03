import * as vscode from "vscode";
import { ActiveResearchRun, MindexApi } from "./api";
import { say } from "./brand";
import { humanize, logError, reportError } from "./errors";
import { describeActiveRun } from "./shared/activeRuns";

interface ActiveRunItem extends vscode.QuickPickItem {
    run: ActiveResearchRun;
}

/**
 * The live research runs, as a QuickPick — fetched on open, not polled.
 *
 * A palette command rather than a status-bar item or a StatusMonitor hook: an
 * occupied slot is a rare state consulted deliberately (usually right after a
 * 429 named this command), and permanent chrome or a per-tick poll is the wrong
 * price for it. Each row carries a stop button; cancelling re-fetches, because
 * the list is the server's answer and the slot frees as the job unwinds.
 */
export async function showActiveResearchRuns(api: MindexApi): Promise<void> {
    const pick = vscode.window.createQuickPick<ActiveRunItem>();
    pick.matchOnDescription = true;
    pick.ignoreFocusOut = true;

    const load = async (): Promise<void> => {
        pick.busy = true;
        try {
            const res = await api.activeResearch();
            pick.title = `Active research — ${res.slots_busy}/${res.slots_total} slot(s) busy`;
            pick.placeholder =
                res.runs.length === 0
                    ? "No research is running."
                    : "A run this window did not start can only be cancelled from here.";
            pick.items = res.runs.map((run) => {
                const row = describeActiveRun(run);
                return {
                    run,
                    label: row.label,
                    description: row.description,
                    detail: row.detail,
                    buttons: [
                        {
                            iconPath: new vscode.ThemeIcon("stop-circle"),
                            tooltip: "Cancel this run",
                        },
                    ],
                };
            });
        } catch (e) {
            pick.title = "Active research — unavailable";
            logError("Listing active research runs", e);
            pick.placeholder = humanize(e).text;
            pick.items = [];
        } finally {
            pick.busy = false;
        }
    };

    pick.onDidTriggerItemButton(async ({ item }) => {
        const yes = await vscode.window.showWarningMessage(
            `Cancel this run? It is stopped immediately and never stored.\n\n${item.run.question}`,
            { modal: true },
            "Cancel run"
        );
        if (yes !== "Cancel run") {
            return;
        }
        try {
            await api.cancelActiveResearch(item.run.run_id);
        } catch (e) {
            await reportError(say("cancel failed"), e);
        }
        // Re-fetch rather than splice: the 204 is idempotent and says nothing;
        // the list is the server's answer. A cancelled run may linger a moment
        // while its job unwinds — that is the honest state, not a rendering bug.
        // (If it was this window's own run, its panel reports the stream end.)
        await load();
    });
    pick.onDidHide(() => pick.dispose());

    pick.show();
    await load();
}
