import { strict as assert } from "node:assert";
import { describe, it } from "node:test";
import {
    ACTION_PRESETS,
    actionsForPreset,
    AGENT_AUDIENCE,
    CUSTOM_PRESET,
    OFFERED_ACTIONS,
} from "./tokenGrants";

/**
 * These are guards on a claim `agentToken.ts` makes in prose and nothing checked:
 * that the menu cannot offer `admin` or `mint`, and that `delete` costs a
 * deliberate tick. Neither is a security boundary — the server refuses anything
 * exceeding the minting token whatever the client asks for — but both are the
 * usability contract this dialog exists to keep, and a table is exactly the kind
 * of thing that gains an entry in a hurry.
 */
describe("the actions the mint dialog may offer", () => {
    const NEVER_OFFERED = ["admin", "mint"];

    it("never offers admin or mint, in any preset or in the tick list", () => {
        for (const forbidden of NEVER_OFFERED) {
            assert.equal(
                OFFERED_ACTIONS.some((a) => a.action === forbidden),
                false,
                `${forbidden} is tickable`
            );
            for (const preset of ACTION_PRESETS) {
                assert.equal(
                    (preset.actions ?? []).includes(forbidden),
                    false,
                    `preset ${preset.id} grants ${forbidden}`
                );
            }
        }
    });

    it("reaches delete only through the tick list", () => {
        assert.ok(OFFERED_ACTIONS.some((a) => a.action === "delete"));
        for (const preset of ACTION_PRESETS) {
            assert.equal(
                (preset.actions ?? []).includes("delete"),
                false,
                `preset ${preset.id} grants delete without a tick`
            );
        }
    });

    it("starts read-only when nothing is ticked", () => {
        assert.deepEqual(
            OFFERED_ACTIONS.filter((a) => a.default).map((a) => a.action),
            ["search", "research"]
        );
    });

    it("grants what each preset says it grants", () => {
        assert.deepEqual(actionsForPreset("read"), ["search", "research"]);
        assert.deepEqual(actionsForPreset("write"), ["search", "research", "index"]);
    });

    it("falls through to the tick list for the custom preset and for anything unknown", () => {
        assert.equal(actionsForPreset(CUSTOM_PRESET), undefined);
        assert.equal(actionsForPreset("no-such-preset"), undefined);
    });

    it("offers the custom entry from the same table it offers the presets from", () => {
        // The fall-through lives in ACTION_PRESETS rather than being appended at
        // the call site, so reading one array answers "what may this menu produce".
        assert.ok(ACTION_PRESETS.some((p) => p.id === CUSTOM_PRESET));
    });

    it("offers only actions the tick list also knows about", () => {
        const tickable = new Set(OFFERED_ACTIONS.map((a) => a.action));
        for (const preset of ACTION_PRESETS) {
            for (const action of preset.actions ?? []) {
                assert.ok(tickable.has(action), `preset ${preset.id} grants ${action}`);
            }
        }
    });

    it("labels its tokens for the agent audience", () => {
        // `token.ts`'s `audienceRefusal` is keyed on this string: change it here
        // and the editor stops refusing an agent's credential pasted into its own
        // keychain, silently.
        assert.equal(AGENT_AUDIENCE, "agent");
    });
});
