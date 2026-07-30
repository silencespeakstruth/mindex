import { deepStrictEqual, ok, strictEqual } from "node:assert";
import { describe, it } from "node:test";
import { ASK_FIELDS, fieldsIn, groupApplies } from "./shared/askFields";
import { formatValue, positions, roundSignificant, toPosition, toValue } from "./shared/scale";

describe("ASK_FIELDS", () => {
    it("has unique ids", () => {
        // The id is the DOM id, the persisted-state key and the message key at once,
        // so a duplicate silently makes one field overwrite another's value.
        const ids = ASK_FIELDS.map((f) => f.id);
        deepStrictEqual([...new Set(ids)].sort(), [...ids].sort());
    });

    it("keeps the ids the previous form persisted", () => {
        // A user's saved webview state is keyed on these. Renaming one drops whatever
        // they had typed into it, silently, on upgrade — and `readBudget`/`readScope`
        // on the host read the same names off the submit message.
        for (const id of [
            "text",
            "topk",
            "effort",
            "model",
            "bseconds",
            "btokens",
            "bsteps",
            "sinclude",
            "sexclude",
            "slangs",
        ]) {
            ok(
                ASK_FIELDS.some((f) => f.id === id),
                `${id} must survive: the host and the persisted state both key on it`
            );
        }
    });

    it("gives every field at least one mode and a tooltip", () => {
        for (const f of ASK_FIELDS) {
            ok(f.modes.length > 0, `${f.id} belongs to no mode, so it can never render`);
            ok(f.title.length > 0, `${f.id} has no tooltip`);
        }
    });

    it("shares the whole scope block between both modes", () => {
        // The point of the redesign: Search used to have a single-select language
        // filter and no globs at all, while Research had both. `/search` takes the
        // same `include`/`exclude` selector, so there is no reason for two shapes.
        for (const f of ASK_FIELDS.filter((x) => x.group === "scope")) {
            deepStrictEqual([...f.modes].sort(), ["research", "search"]);
        }
    });

    it("makes every budget slider unset-able and no other slider", () => {
        // "Unset" means "use the effort preset", which only budget axes have.
        for (const f of ASK_FIELDS) {
            if (f.kind.k !== "slider") {
                continue;
            }
            strictEqual(
                f.kind.preset !== undefined,
                f.group === "budget",
                `${f.id}: only budget axes have a preset to fall back to`
            );
        }
    });

    it("selects fields by group and mode", () => {
        deepStrictEqual(
            fieldsIn("budget", "research").map((f) => f.id),
            ["bseconds", "btokens", "bsteps"]
        );
        deepStrictEqual(fieldsIn("budget", "search"), []);
        ok(groupApplies("scope", "search"));
        ok(!groupApplies("research", "search"));
    });
});

describe("scale", () => {
    const linear = { min: 1, max: 100, log: false };
    const seconds = { min: 1, max: 3600, log: false };
    const tokens = { min: 1, max: 8000, log: true };

    it("round-trips a linear value through its position", () => {
        for (const v of [1, 2, 10, 42, 99, 100]) {
            strictEqual(toValue(linear, toPosition(linear, v)), v);
        }
        for (const v of [1, 60, 300, 900, 1800, 3600]) {
            strictEqual(toValue(seconds, toPosition(seconds, v)), v);
        }
    });

    it("pins both ends of a log axis exactly", () => {
        // Two-significant-figure rounding would otherwise land the far right of the
        // track just past the server's ceiling and earn a 400 for dragging all the
        // way over.
        strictEqual(toValue(tokens, 0), 1);
        strictEqual(toValue(tokens, positions(tokens)), 8000);
        strictEqual(
            toValue(tokens, positions(tokens) + 50),
            8000,
            "clamped, not extrapolated"
        );
        strictEqual(toValue(tokens, -10), 1);
    });

    it("keeps a log round-trip within one rounding step", () => {
        // Exactness is not available on a log axis — the readout is deliberately
        // rounded to two significant figures — but the value must not drift.
        for (const v of [400, 1200, 6000]) {
            const back = toValue(tokens, toPosition(tokens, v));
            ok(
                Math.abs(back - v) / v < 0.02,
                `${v} came back as ${back}, more than a rounding step away`
            );
        }
    });

    it("clamps a value outside the track", () => {
        strictEqual(toPosition(linear, 0), 0);
        strictEqual(toPosition(linear, 9999), positions(linear));
    });

    it("gives a log axis fine steps and a linear axis whole ones", () => {
        // One position per value on every linear axis, so an arrow key steps by
        // exactly one and the slider can reach anything the number box can.
        strictEqual(positions(linear), 99);
        strictEqual(positions(seconds), 3599);
        strictEqual(positions(tokens), 1000);
    });

    it("rounds to significant figures without inventing fractions", () => {
        strictEqual(roundSignificant(1234, 2), 1200);
        strictEqual(roundSignificant(47.3, 2), 47);
        strictEqual(roundSignificant(7, 2), 7);
        strictEqual(roundSignificant(0, 2), 0);
    });

    it("formats a value the way a person would say it", () => {
        strictEqual(formatValue("count", 10), "10");
        strictEqual(formatValue("steps", 20), "20");
        strictEqual(formatValue("seconds", 45), "45s");
        strictEqual(formatValue("seconds", 900), "15m");
        strictEqual(formatValue("seconds", 3600), "60m");
        // Entered in thousands, so 1200 is 1.2 *million* tokens. Rendering it as
        // "1.2k" would be three orders of magnitude out — which is precisely the
        // mistake a slider is here to prevent.
        strictEqual(formatValue("ktokens", 400), "400k");
        strictEqual(formatValue("ktokens", 1200), "1.2M");
        strictEqual(formatValue("ktokens", 6000), "6M");
    });
});
