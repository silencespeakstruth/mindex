import { langIcon } from "../host.js";

export interface Pills {
    root: HTMLElement;
    nodes: HTMLElement[];
    /** Comma-separated, the same value shape the old multi-select produced. */
    read(): string;
    write(csv: string): void;
    /**
     * Replace the offered set. Selections survive where they still exist — a filter
     * that silently changes is worse than one that visibly resets.
     */
    setOptions(values: readonly string[]): void;
}

/**
 * A multi-select as a row of toggle chips.
 *
 * It replaces two controls at once: Search's single-select `<select id="lang">` and
 * Research's `<select multiple size="4">`. Those were the same filter in two shapes,
 * and the server takes the same `programming_languages` list for both endpoints.
 *
 * Chips rather than a multi-select because of where this lives. At sidebar width a
 * four-row `size=4` list is a scroll trap that needs ctrl-click to select a second
 * item and silently drops the first if you forget — an interaction most people have
 * never had to learn, in a panel 300 pixels wide. A chip is one tap, shows every
 * choice at once, wraps, and is a big enough target to hit.
 *
 * The value stays a comma-separated string so the host's `readScope` is untouched.
 */
export function makePills(id: string, title: string, onChange: () => void): Pills {
    const root = document.createElement("div");
    root.className = "pills";
    root.id = id;
    root.title = title;
    root.setAttribute("role", "group");
    root.setAttribute("aria-label", title);

    let selected = new Set<string>();
    let offered: readonly string[] = [];

    function render(): void {
        root.replaceChildren();
        if (offered.length === 0) {
            const empty = document.createElement("span");
            empty.className = "hint";
            empty.textContent = "no languages known yet";
            root.appendChild(empty);
            return;
        }
        for (const value of offered) {
            const pill = document.createElement("button");
            pill.type = "button";
            pill.className = "pill";
            pill.dataset.value = value;
            // The mark first, then the name. At sidebar width the chips wrap into a
            // block of same-length lowercase words, and the logo is what lets the eye
            // find the one it wants without reading all of them.
            pill.append(langIcon(value), document.createTextNode(value));
            pill.setAttribute("aria-pressed", String(selected.has(value)));
            pill.addEventListener("click", () => {
                if (selected.has(value)) {
                    selected.delete(value);
                } else {
                    selected.add(value);
                }
                pill.setAttribute("aria-pressed", String(selected.has(value)));
                onChange();
            });
            root.appendChild(pill);
        }
    }

    return {
        root,
        nodes: [root],
        read: () => offered.filter((v) => selected.has(v)).join(","),
        write: (csv) => {
            selected = new Set(
                csv
                    .split(",")
                    .map((s) => s.trim())
                    .filter((s) => s !== "")
            );
            render();
        },
        setOptions: (values) => {
            offered = values;
            // Keep only what is still on offer. A selection for a language the index
            // no longer holds would filter every search down to nothing, invisibly.
            selected = new Set([...selected].filter((v) => values.includes(v)));
            render();
        },
    };
}
