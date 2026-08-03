import * as vscode from "vscode";
import { BRAND } from "./brand";
import { themeIconRef } from "./icons";
import { failedCount, ServerState, StatusSnapshot } from "./statusMonitor";

/**
 * The one-glance indicator: **MINDex** in green, yellow or red.
 *
 * It replaced `$(database) mindex: ok`, which spent a lot of status-bar width saying
 * something the colour alone says — and then answered a click by refreshing
 * invisibly. Now the colour is the whole message, and the click opens the panel that
 * explains it.
 *
 * **Three colours and no words.** Yellow used to mean nothing on its own — the
 * indicator carried a `$(stop) research` suffix to say what was actually wrong —
 * and now it means exactly one thing: an optional dependency is down, which
 * costs Research and nothing else. That is what the suffix was spending
 * status-bar width spelling out, so it is gone; the sentence and the remedy live
 * in the tooltip, which is where someone goes once the colour has told them to.
 *
 * The colour comes from `StatusBarItem.color` and **not** from `backgroundColor`.
 * That is not a style preference: `backgroundColor` accepts only
 * `statusBarItem.errorBackground` and `statusBarItem.warningBackground`, and setting
 * it makes VS Code override the foreground — so it can express two states plus "no
 * treatment", never three. `color` takes any `ThemeColor`.
 *
 * The glyph changes with the state as well as the colour, so the indicator still
 * reads under a colour-blind palette or a high-contrast theme, where a hue
 * distinction is exactly what is not available.
 */
export function paintStatusBar(item: vscode.StatusBarItem, snapshot?: StatusSnapshot): void {
    const state: ServerState = snapshot?.state ?? "unreachable";
    const glyph = {
        ok: themeIconRef("stateOk"),
        degraded: themeIconRef("stateDegraded"),
        unhealthy: themeIconRef("stateUnhealthy"),
        unreachable: themeIconRef("stateUnreachable"),
    }[state];
    // Red covers both ways of being unusable. They differ in remedy, not in what
    // the user can currently do, and a fourth hue would claim otherwise.
    const hue = {
        ok: "charts.green",
        degraded: "charts.yellow",
        unhealthy: "charts.red",
        unreachable: "charts.red",
    }[state];

    const researchDown = snapshot !== undefined && !snapshot.researchAvailable;

    item.text = `${glyph} ${BRAND}`;
    item.color = new vscode.ThemeColor(hue);
    // Deliberately never set: it would override `color` and cost the third state.
    item.backgroundColor = undefined;
    item.tooltip = tooltipFor(snapshot, state, researchDown);
    item.show();
}

function tooltipFor(
    snapshot: StatusSnapshot | undefined,
    state: ServerState,
    researchDown: boolean
): vscode.MarkdownString {
    const md = new vscode.MarkdownString();
    md.supportThemeIcons = true;
    if (snapshot === undefined) {
        md.appendMarkdown(`**${BRAND}** — not checked yet.`);
        return md;
    }

    const headline = {
        ok: "every dependency answering",
        degraded: "an optional dependency is failing",
        unhealthy: "a required dependency is failing, or a run is wedged",
        unreachable: snapshot.detail ?? "the server is not answering",
    }[state];
    md.appendMarkdown(`**${BRAND}** — ${state}: ${headline}\n\n`);
    if (snapshot.version !== undefined) {
        md.appendMarkdown(`server \`v${snapshot.version}\` at \`${snapshot.serverUrl}\`\n\n`);
    } else {
        md.appendMarkdown(`\`${snapshot.serverUrl}\`\n\n`);
    }
    if (researchDown) {
        md.appendMarkdown("Ollama is down: indexing and search work, Research does not.\n\n");
    }
    const failed = failedCount(snapshot);
    if (failed > 0) {
        md.appendMarkdown(`${failed} failed file(s) — click to review.\n\n`);
    }
    md.appendMarkdown(
        `_checked ${new Date(snapshot.at).toLocaleTimeString()} — click for details_`
    );
    return md;
}
