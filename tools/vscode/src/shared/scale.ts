/**
 * Slider maths, shared by the webview that draws sliders and the tests that check
 * them.
 *
 * An `<input type="range">` is an integer position; the values the form actually
 * sends span four very different domains — 1…100 results, 1…3600 seconds, 1…200
 * steps and 1…8000 thousand tokens. A linear slider over that last one is useless:
 * the whole interesting range (the presets are 400, 1200 and 6000) sits in the first
 * fifth of the track, and one pixel is worth ~30k tokens. So the token axis is
 * mapped logarithmically and the rest linearly, and this module is the mapping.
 *
 * Everything here is pure and free of the DOM on purpose: this is the part with
 * arithmetic in it, and arithmetic in a webview is arithmetic nothing can test.
 */

/** How a slider position maps to a value. */
export interface Scale {
    min: number;
    max: number;
    /** Logarithmic when the useful range spans orders of magnitude. */
    log: boolean;
}

/**
 * Slider positions across the track.
 *
 * A **linear** axis gets one position per value, so an arrow key steps by exactly one
 * and the slider can express every value the number box can. The cap only exists so a
 * hypothetical enormous linear domain does not produce an absurd `step`; every linear
 * axis the form actually has (1…100 results, 1…3600 seconds, 1…200 steps) is well
 * inside it and therefore lossless.
 *
 * A **log** axis cannot be lossless — it is deliberately a coarse readout rounded to
 * two significant figures — so 1000 is chosen for feel instead: one arrow key is
 * worth ≈0.9 % over 1…8000.
 */
const MAX_LINEAR_POSITIONS = 10000;

export function positions(s: Scale): number {
    return s.log ? 1000 : Math.min(Math.max(1, s.max - s.min), MAX_LINEAR_POSITIONS);
}

/** The slider position that best represents `value`. Clamped to the track. */
export function toPosition(s: Scale, value: number): number {
    const n = positions(s);
    const clamped = Math.min(s.max, Math.max(s.min, value));
    const t = s.log
        ? (Math.log(clamped) - Math.log(s.min)) / (Math.log(s.max) - Math.log(s.min))
        : (clamped - s.min) / (s.max - s.min);
    return Math.round(t * n);
}

/** The value at slider position `pos`. */
export function toValue(s: Scale, pos: number): number {
    const n = positions(s);
    const t = Math.min(1, Math.max(0, pos / n));
    if (!s.log) {
        return Math.round(s.min + t * (s.max - s.min));
    }
    // Rounded to two significant figures so the readout is a number a person would
    // say — "1.2M", not "1234567". The endpoints are pinned because rounding would
    // otherwise put the slider's own maximum just outside the server's ceiling and
    // earn a 400 for dragging all the way right.
    if (t <= 0) {
        return s.min;
    }
    if (t >= 1) {
        return s.max;
    }
    const raw = Math.exp(Math.log(s.min) + t * (Math.log(s.max) - Math.log(s.min)));
    return Math.min(s.max, Math.max(s.min, roundSignificant(raw, 2)));
}

/** `1234` → `1200`, `47.3` → `47`, `0.86` → `0.86`. */
export function roundSignificant(value: number, digits: number): number {
    if (value === 0) {
        return 0;
    }
    const magnitude = Math.floor(Math.log10(Math.abs(value))) - (digits - 1);
    const step = Math.pow(10, magnitude);
    // At or below one unit per significant figure there is nothing left to round to,
    // and dividing by a sub-unit step reintroduces the fractions this exists to avoid.
    return step < 1 ? Math.round(value) : Math.round(value / step) * step;
}

/** What a value is measured in — decides how it reads, not what is sent. */
export type Unit = "seconds" | "ktokens" | "steps" | "count";

/**
 * A value as a person reads it.
 *
 * `ktokens` is the awkward one: the field is *entered* in thousands (the wire scales
 * it back up), so 1200 has to render as "1.2M" and not "1200" or "1.2k". Getting that
 * wrong makes a budget look three orders of magnitude off, which is exactly the kind
 * of thing a slider is supposed to stop happening.
 */
export function formatValue(unit: Unit, value: number): string {
    switch (unit) {
        case "seconds":
            return value >= 120
                ? `${(value / 60).toFixed(value % 60 === 0 ? 0 : 1)}m`
                : `${value}s`;
        case "ktokens":
            return value >= 1000 ? `${trim(value / 1000)}M` : `${value}k`;
        case "steps":
        case "count":
            return String(value);
    }
}

/** `1.20` → `1.2`, `6.00` → `6`. */
function trim(n: number): string {
    return n.toFixed(1).replace(/\.0$/, "");
}
