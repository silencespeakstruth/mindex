import { icon } from "../host.js";

export interface SegmentedOption {
    value: string;
    label: string;
    /** Codicon name. Omitted for a compact ladder, where labels are enough. */
    glyph?: string;
    title?: string;
}

export interface Segmented {
    root: HTMLElement;
    nodes: HTMLElement[];
    read(): string;
    write(value: string): void;
    /** Retitle an option once the server says what it costs (the effort ladder). */
    setTitle(value: string, title: string): void;
}

/**
 * One of a short closed set, as a row of buttons.
 *
 * Used twice, which is the point: the Search/Research mode switch and the low/medium/
 * high effort ladder are the same act, and effort used to be a `<select>` for no
 * reason except that it was written at a different time. A `<select>` also hides the
 * options until clicked — fine for 22 languages, wasteful for three.
 *
 * `role="tablist"` + `aria-selected` rather than a radio group: the mode switch really
 * does swap a panel, and the ladder reads the same way to a screen reader.
 */
export function makeSegmented(
    id: string,
    options: SegmentedOption[],
    compact: boolean,
    onChange: (value: string) => void
): Segmented {
    const root = document.createElement("div");
    root.className = compact ? "segmented compact" : "segmented";
    root.id = id;
    root.setAttribute("role", "tablist");

    let value = options[0]?.value ?? "";
    const buttons = new Map<string, HTMLButtonElement>();

    for (const o of options) {
        const button = document.createElement("button");
        button.type = "button";
        button.setAttribute("role", "tab");
        button.setAttribute("aria-selected", "false");
        button.dataset.value = o.value;
        if (o.title !== undefined) {
            button.title = o.title;
        }
        if (o.glyph !== undefined) {
            button.appendChild(icon(o.glyph, true));
        }
        button.appendChild(document.createTextNode(o.label));
        button.addEventListener("click", () => {
            if (value === o.value) {
                return;
            }
            select(o.value);
            onChange(o.value);
        });
        buttons.set(o.value, button);
        root.appendChild(button);
    }

    function select(next: string): void {
        value = next;
        for (const [v, button] of buttons) {
            button.setAttribute("aria-selected", String(v === next));
        }
    }
    select(value);

    return {
        root,
        nodes: [root],
        read: () => value,
        write: (next) => {
            if (buttons.has(next)) {
                select(next);
            }
        },
        setTitle: (v, title) => {
            const button = buttons.get(v);
            if (button !== undefined) {
                button.title = title;
            }
        },
    };
}
