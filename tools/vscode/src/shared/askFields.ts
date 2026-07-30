import { Unit } from "./scale";

export type AskMode = "search" | "research";

/** Which block of the form a field is mounted into. */
export type FieldGroup = "query" | "search" | "research" | "budget" | "scope";

/**
 * A number the server publishes, named rather than copied.
 *
 * The Ask form used to hard-code `max="50"` for the result count against a real
 * server ceiling of 100, and three separate copies of the effort ladder had each
 * drifted before the ladder was published. A field therefore never carries a bound —
 * it carries the *name* of one, and the form resolves it from `GET /config` when it
 * arrives.
 */
export type ConfigBound =
    | "search.max_top_k"
    | "research.max_request_seconds"
    | "research.max_request_tokens_k"
    | "research.max_request_steps";

/** Which effort-preset axis an unset slider parks on. */
export type PresetAxis = "max_seconds" | "max_tokens_k" | "max_steps";

export type FieldKind =
    | { k: "textarea"; rows: number }
    | { k: "text" }
    | { k: "segmented"; options: { value: string; label: string; icon?: string }[] }
    /** A closed list from the server, degrading to free text when there is none. */
    | { k: "model" }
    /** Multi-select over the project's language inventory, as toggle chips. */
    | { k: "languages" }
    | {
          k: "slider";
          unit: Unit;
          min: number;
          max: ConfigBound;
          /** Fallback ceiling until `GET /config` answers (or on an older server). */
          fallbackMax: number;
          /**
           * Present ⇒ the field may be **unset**, meaning "use the effort preset",
           * and this names the preset axis it parks on. Absent ⇒ always sent.
           */
          preset?: PresetAxis;
          /** Where the initial value comes from when nothing is persisted. */
          seed?: "topK";
      };

export interface AskField {
    /**
     * DOM id, persisted-state key and `postMessage` key, all at once. They were
     * always the same three strings written three times; making that structural is
     * most of what this table buys.
     */
    id: string;
    label: string;
    /** The `title=` tooltip. Every control has one — none of them is self-evident. */
    title: string;
    group: FieldGroup;
    modes: AskMode[];
    kind: FieldKind;
    placeholder?: string;
}

/**
 * The Ask form, declared once.
 *
 * Before this, the same ten fields were enumerated by hand in five places — the
 * submit handler, the "this folder" handler (a verbatim copy of the submit handler
 * plus one flag), the state writer, the state reader and the change-listener loop —
 * and adding a field meant remembering all five. The one that got forgotten was
 * always the same one: the listener loop, whose omission does not break anything
 * visibly, it just stops persisting that field.
 *
 * **The ids are the ids the old form used**, deliberately. A user's persisted webview
 * state survives the upgrade, and `readBudget`/`readScope` on the host side keep
 * working on the same message keys they always did. Only `lang` is gone — the Search
 * tab's single-select language filter, now folded into the shared `slangs` chips (see
 * the v1→v2 migration in `webview/ask.ts`).
 *
 * Order within a group is render order.
 */
export const ASK_FIELDS: readonly AskField[] = [
    {
        id: "text",
        label: "Question",
        title: "What to search for, or what to research",
        group: "query",
        modes: ["search", "research"],
        kind: { k: "textarea", rows: 4 },
    },
    {
        id: "topk",
        label: "results",
        title: "How many results to request (top-k)",
        group: "search",
        modes: ["search"],
        kind: {
            k: "slider",
            unit: "count",
            min: 1,
            max: "search.max_top_k",
            fallbackMax: 100,
            seed: "topK",
        },
    },
    {
        id: "effort",
        label: "effort",
        title: "Preset budget: time, local tokens and tool calls",
        group: "research",
        modes: ["research"],
        kind: {
            k: "segmented",
            options: [
                { value: "low", label: "low" },
                { value: "medium", label: "medium" },
                { value: "high", label: "high" },
            ],
        },
    },
    {
        id: "model",
        label: "model",
        title: "Ollama model for this run; the server's default when left alone",
        group: "research",
        modes: ["research"],
        kind: { k: "model" },
        placeholder: "model (optional)",
    },
    {
        id: "bseconds",
        label: "time",
        title: "Wall-clock for the investigation. The budget you actually wait for.",
        group: "budget",
        modes: ["research"],
        kind: {
            k: "slider",
            unit: "seconds",
            min: 1,
            max: "research.max_request_seconds",
            fallbackMax: 3600,
            preset: "max_seconds",
        },
    },
    {
        id: "btokens",
        label: "tokens",
        title:
            "Local tokens the run may spend — prompt + generated, summed over turns. " +
            "What it costs the GPU.",
        group: "budget",
        modes: ["research"],
        kind: {
            k: "slider",
            // Entered and persisted in *thousands*; the host multiplies by 1000 on the
            // way out, which is why the unit formats 1200 as "1.2M".
            unit: "ktokens",
            min: 1,
            max: "research.max_request_tokens_k",
            fallbackMax: 8000,
            preset: "max_tokens_k",
        },
    },
    {
        id: "bsteps",
        label: "steps",
        title: "Executed tool calls. A backstop, not a measure of work.",
        group: "budget",
        modes: ["research"],
        kind: {
            k: "slider",
            unit: "steps",
            min: 1,
            max: "research.max_request_steps",
            fallbackMax: 200,
            preset: "max_steps",
        },
    },
    {
        id: "slangs",
        label: "languages",
        title: "Restrict to these languages; select none for all",
        group: "scope",
        modes: ["search", "research"],
        kind: { k: "languages" },
    },
    {
        id: "sinclude",
        label: "only",
        title: "Comma-separated globs; only matching files are looked at",
        group: "scope",
        modes: ["search", "research"],
        kind: { k: "text" },
        placeholder: "whole project",
    },
    {
        id: "sexclude",
        label: "never",
        title: "Comma-separated globs; matching files are hidden",
        group: "scope",
        modes: ["search", "research"],
        kind: { k: "text" },
        placeholder: "nothing excluded",
    },
];

/** The fields that belong to `group`, in render order, for the active mode. */
export function fieldsIn(group: FieldGroup, mode: AskMode): AskField[] {
    return ASK_FIELDS.filter((f) => f.group === group && f.modes.includes(mode));
}

/** Whether any field of this group is shown in `mode` — i.e. whether to show it. */
export function groupApplies(group: FieldGroup, mode: AskMode): boolean {
    return ASK_FIELDS.some((f) => f.group === group && f.modes.includes(mode));
}
