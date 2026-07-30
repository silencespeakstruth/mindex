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
        unreachable: themeIconRef("stateUnreachable"),
    }[state];
    const hue = {
        ok: "charts.green",
        degraded: "charts.yellow",
        unreachable: "charts.red",
    }[state];

    // A dead Ollama is *not* a degraded server, so it never changes the state or the
    // colour — it only annotates what is unavailable.
    const researchDown = snapshot !== undefined && !snapshot.researchAvailable;
    const suffix = researchDown ? ` ${themeIconRef("stop")} research` : "";

    item.text = `${glyph} ${BRAND}${suffix}`;
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
        degraded: "a required dependency is failing",
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
