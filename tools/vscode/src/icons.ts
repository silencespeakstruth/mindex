/**
 * Semantic name → codicon name, in one table.
 *
 * The extension draws icons in three places that used to disagree: `package.json`'s
 * `$(…)` syntax (toolbars, commands), `vscode.ThemeIcon` (status bar, trees) and the
 * webviews — which, before codicons were loaded into them, hand-rolled their own SVG
 * glyphs. The result was a magnifying glass in the sidebar toolbar and a *different*
 * magnifying glass inside the form.
 *
 * With `@vscode/codicons` loaded as a webview font all three draw from the same set,
 * and this table is what keeps them naming the same member of it. `package.json`
 * cannot import TypeScript, so its `$(…)` strings are still written by hand — but
 * `iconsAreConsistentWithPackageJson` in `icons.test.ts` reads both and fails when
 * they diverge, which is the part that could not be checked before.
 */
export const ICON = {
    /** The Ask view itself — both modes under one entry point. */
    ask: "comment-discussion",
    /** Search mode, the search command, the submit button in Search. */
    search: "search",
    /** Research mode, the research command, the submit button in Research. */
    research: "beaker",
    /** Stop an in-flight research run. */
    stop: "close",

    /** Server reachable and every required dependency answering. */
    stateOk: "circle-filled",
    /** Server reachable, a required dependency failing. */
    stateDegraded: "warning",
    /** Server not answering at all. */
    stateUnreachable: "error",

    /** The status panel and the command that opens it. */
    status: "pulse",
    /** Jump to the extension's own settings. */
    settings: "gear",
    /** Refresh whatever the surface is showing. */
    refresh: "refresh",

    /** The Budget disclosure. */
    budget: "settings",
    /** The Scope disclosure. */
    scope: "filter",
    /** Scope the run to the folder of the active editor. */
    scopeFolder: "folder-opened",
    /** Reset the scope to the project's `.mindex`. */
    scopeMindex: "file-code",
    /** Clear the scope. */
    scopeClear: "clear-all",
    /** Drop a slider axis back to its effort preset. */
    reset: "discard",

    /** Requeue a failed file. */
    retry: "debug-restart",
    /** Drift, and the command that checks it. */
    drift: "git-compare",
    /** Clear every difference drift found, in one action. */
    sync: "sync",
    /** Upload files to the index. */
    reindex: "cloud-upload",
    /** An indexed file. */
    file: "symbol-file",
    /** The project's language inventory. */
    inventory: "library",
    /** Files whose indexing failed. */
    failed: "flame",
} as const;

export type IconName = keyof typeof ICON;

/** The `$(…)` spelling, for status-bar text and `MarkdownString` labels. */
export function themeIconRef(name: IconName): string {
    return `$(${ICON[name]})`;
}
