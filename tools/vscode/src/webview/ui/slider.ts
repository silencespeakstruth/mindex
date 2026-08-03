import {
    formatValue,
    positions,
    Scale,
    toPosition,
    toValue,
    Unit,
} from "../../shared/scale.js";
import { icon } from "../host.js";

export interface SliderOptions {
    id: string;
    label: string;
    title: string;
    unit: Unit;
    min: number;
    max: number;
    /**
     * When set, the field may be **unset** — meaning "use the effort preset" — and
     * this is the preset it parks on. Changing effort re-parks an unset axis and
     * leaves an overridden one alone.
     */
    preset?: number;
    /** Value to start at when nothing is persisted. Ignored when `preset` is set. */
    initial?: number;
    onChange(): void;
}

/**
 * A bounded number: a track, a number box and (for a budget axis) a reset button.
 *
 * ## Why a slider at all
 * Every number this form sends has a ceiling the server publishes. A bare `<input
 * type="number">` shows none of it — you cannot see whether 900 is generous or
 * nothing without knowing the maximum is 3600 — and the old form's hard-coded
 * `max="50"` for a server ceiling of 100 is what happens when the bound lives in the
 * markup instead of coming from `GET /config`.
 *
 * ## Why a number box as well
 * A log-scaled track is a coarse instrument by construction, and some values are
 * worth typing exactly. The two are one control: whichever you touch, the other
 * follows. The number box is deliberately **never clamped** — the server owns the
 * ceilings, and silently correcting a typed value runs something other than what was
 * asked for.
 *
 * ## Unset
 * A budget axis left alone means "use the effort preset", and the wire shape for that
 * is an empty string — byte-identical to the old blank text field, which is why the
 * host's `readBudget` needed no change. Unset renders as an *empty* number box
 * showing the preset as a placeholder, a dimmed track parked at the preset position,
 * and a disabled reset. Touching either control overrides the axis; the reset button
 * puts it back. A checkbox was the alternative and was rejected: it turns a value
 * into a mode and costs a click before you may touch the thing you came to touch.
 *
 * Dragging to exactly the preset value still counts as overridden. The request that
 * results is identical either way, and the alternative — the value silently vanishing
 * when you land on the preset — is a surprise with nothing to gain.
 */
export interface Slider {
    root: HTMLElement;
    nodes: HTMLElement[];
    read(): string;
    write(value: string): void;
    /** Re-park an *unset* axis on a new effort preset; overridden axes are untouched. */
    setPreset(preset: number): void;
    /** Re-bound the track once `GET /config` says what the real ceiling is. */
    setMax(max: number): void;
}

export function makeSlider(o: SliderOptions): Slider {
    let scale: Scale = { min: o.min, max: o.max, log: o.unit === "ktokens" };
    let preset = o.preset;
    const unsettable = o.preset !== undefined;
    let overridden = !unsettable;

    const root = document.createElement("div");
    root.className = "slider";

    const label = document.createElement("label");
    label.className = "slider-label";
    label.htmlFor = `${o.id}-range`;
    label.textContent = o.label;
    label.title = o.title;

    const range = document.createElement("input");
    range.type = "range";
    range.id = `${o.id}-range`;
    range.className = "slider-range";
    range.title = o.title;
    range.min = "0";
    range.step = "1";

    const box = document.createElement("input");
    box.type = "number";
    box.id = o.id;
    box.className = "slider-box";
    box.title = o.title;
    box.min = String(o.min);

    const reset = document.createElement("button");
    reset.type = "button";
    reset.className = "ghost slider-reset";
    reset.title = "Use the effort preset";
    reset.setAttribute("aria-label", `Reset ${o.label} to the effort preset`);
    reset.appendChild(icon("discard", true));
    reset.hidden = !unsettable;

    root.append(label, range, box, reset);

    /** The value the track is currently showing, whether or not it is the user's. */
    const shown = (): number =>
        Number(range.value === "" ? 0 : toValue(scale, Number(range.value)));

    function paint(): void {
        range.max = String(positions(scale));
        box.max = String(scale.max);
        root.classList.toggle("preset", !overridden);
        reset.disabled = !overridden;
        // Screen readers get the number *and* whether it is the user's choice; a
        // dimmed track says that visually and nothing else does.
        range.setAttribute(
            "aria-valuetext",
            formatValue(o.unit, shown()) + (overridden ? "" : " (preset)")
        );
        if (!overridden) {
            box.value = "";
            box.placeholder = preset === undefined ? "preset" : String(preset);
        }
    }

    /** Park the track on `value` without claiming the user chose it. */
    function park(value: number): void {
        range.value = String(toPosition(scale, value));
        paint();
    }

    function adopt(value: number): void {
        overridden = true;
        box.value = String(value);
        range.value = String(toPosition(scale, value));
        paint();
        o.onChange();
    }

    range.addEventListener("input", () => adopt(toValue(scale, Number(range.value))));
    box.addEventListener("input", () => {
        const n = Number(box.value);
        if (box.value.trim() === "" || !Number.isFinite(n)) {
            return;
        }
        overridden = true;
        // The typed value is authoritative and is left exactly as typed — including
        // over the ceiling, where the server's own 400 is a better answer than a
        // silent correction. The track just snaps as close as it can.
        range.value = String(toPosition(scale, n));
        paint();
        o.onChange();
    });
    box.addEventListener("change", () => {
        // Clearing the box is how you ask for the preset back, for anyone who reaches
        // for the keyboard rather than the reset button.
        if (unsettable && box.value.trim() === "") {
            overridden = false;
            park(preset ?? o.min);
            o.onChange();
        }
    });
    reset.addEventListener("click", () => {
        overridden = false;
        park(preset ?? o.min);
        o.onChange();
    });

    // Initial state: an unset-able axis starts on its preset, everything else on its
    // seed (the `mindex.topK` setting, for the result count).
    if (unsettable) {
        park(preset ?? o.min);
    } else {
        adopt(o.initial ?? o.min);
        overridden = true;
    }

    return {
        root,
        nodes: [range, box],
        read: () => (overridden ? box.value.trim() : ""),
        write: (value) => {
            const trimmed = value.trim();
            if (trimmed === "" && unsettable) {
                overridden = false;
                park(preset ?? o.min);
                return;
            }
            const n = Number(trimmed);
            if (Number.isFinite(n) && n > 0) {
                overridden = true;
                box.value = String(n);
                range.value = String(toPosition(scale, n));
                paint();
            }
        },
        setPreset: (next) => {
            preset = next;
            if (!overridden) {
                park(next);
            } else {
                paint();
            }
        },
        setMax: (max) => {
            scale = { ...scale, max };
            // Re-derive the position from the *value*, not from the old position: the
            // track just changed length underneath it.
            const current = overridden ? Number(box.value) : (preset ?? o.min);
            range.value = String(
                toPosition(scale, Number.isFinite(current) ? current : o.min)
            );
            paint();
        },
    };
}
