/**
 * What the mint dialog may offer, as data.
 *
 * Separated from `agentToken.ts` for one reason: that file imports `vscode`, and
 * the suite runs under bare `node --test`. The tables below are the part worth
 * pinning — that `admin` and `mint` are absent by construction, and that `delete`
 * is reachable only through the tick list — and a guard that cannot be run is not
 * a guard. `token.ts` is separated from `tokenStatusBar.ts` for the same reason.
 *
 * The rationale for each choice lives in `agentToken.ts`'s header comment, beside
 * the flow it shapes; this file holds the values and nothing else.
 */

/**
 * What may be ticked, and what starts ticked.
 *
 * `admin` and `mint` are absent by construction rather than filtered later — a
 * list that holds them and hides them is one edit away from offering them.
 */
export const OFFERED_ACTIONS: readonly {
    action: string;
    label: string;
    detail: string;
    default: boolean;
}[] = [
    {
        action: "search",
        label: "search",
        detail: "read the index: search, symbols, outline, file lists, drift",
        default: true,
    },
    {
        action: "research",
        label: "research",
        detail: "run investigations and challenges, and read stored reports",
        default: true,
    },
    {
        action: "index",
        label: "index",
        detail: "WRITE — upload file contents, reindex, cancel, retry",
        default: false,
    },
    {
        action: "delete",
        label: "delete",
        detail: "DESTRUCTIVE — remove files, history and stored reports from the index",
        default: false,
    },
];

/** The id of the preset that means "do not use a preset". */
export const CUSTOM_PRESET = "custom";

/**
 * The presets offered before the tick list, in the order they are offered.
 *
 * `actions: undefined` is the fall-through to [`OFFERED_ACTIONS`] — the entry
 * exists in the same table rather than being appended at the call site so that
 * "what may this menu produce" is answerable by reading one array.
 */
export const ACTION_PRESETS: readonly {
    id: string;
    label: string;
    detail: string;
    actions?: readonly string[];
}[] = [
    {
        id: "read",
        label: "Read only",
        detail: "search + research — look at the index and investigate it",
        actions: ["search", "research"],
    },
    {
        id: "write",
        label: "Read and write",
        detail: "search + research + index — also keep the index current after editing",
        actions: ["search", "research", "index"],
    },
    {
        id: CUSTOM_PRESET,
        label: "Choose actions…",
        detail: "tick them individually — the only way to reach `delete`",
    },
];

/** The actions a preset grants, or `undefined` when the caller must ask. */
export function actionsForPreset(id: string): readonly string[] | undefined {
    return ACTION_PRESETS.find((p) => p.id === id)?.actions;
}

/** What this command labels the tokens it issues. */
export const AGENT_AUDIENCE = "agent";
