import { deepStrictEqual, match, ok, strictEqual } from "node:assert";
import { describe, it } from "node:test";
import * as fs from "node:fs";
import * as path from "node:path";
import { ICON, IconName, themeIconRef } from "./icons";

interface PackageJson {
    displayName: string;
    contributes: {
        viewsContainers: { activitybar: { id: string; title: string }[] };
        views: Record<string, { id: string; name: string }[]>;
        commands: { command: string; title: string; category: string; icon?: string }[];
        menus: Record<string, { command: string; when: string; group: string }[]>;
        configuration: { title: string };
    };
}

const pkg = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8")
) as PackageJson;

describe("icons", () => {
    it("names a codicon for every semantic use", () => {
        for (const [name, glyph] of Object.entries(ICON)) {
            match(glyph, /^[a-z][a-z0-9-]*$/, `${name} is not a codicon name`);
        }
    });

    it("spells a theme-icon reference the way package.json does", () => {
        strictEqual(themeIconRef("refresh"), "$(refresh)");
    });

    /**
     * `package.json` cannot import TypeScript, so its `$(…)` strings are written by
     * hand — the one place the icon table cannot enforce itself. This test reads both
     * and fails when a command's icon is not in the table at all, which is how a
     * fourth magnifying glass would have got in before.
     */
    it("draws every command icon from the shared table", () => {
        const known = new Set<string>(Object.values(ICON));
        // Icons that belong to VS Code's own vocabulary for an action this extension
        // did not invent. Listed rather than added to ICON, which is for glyphs the
        // extension's *own* surfaces share.
        const borrowed = new Set(["new-file", "save", "stop-circle", "debug-rerun"]);
        for (const cmd of pkg.contributes.commands) {
            if (cmd.icon === undefined) {
                continue;
            }
            const glyph = /^\$\((.+)\)$/.exec(cmd.icon)?.[1];
            ok(glyph !== undefined, `${cmd.command}: icon "${cmd.icon}" is not $(…)`);
            ok(
                known.has(glyph) || borrowed.has(glyph),
                `${cmd.command} uses "${glyph}", which is neither in ICON nor on the ` +
                    `borrowed list — add it to one so the surfaces cannot drift`
            );
        }
    });
});

describe("package.json contributions", () => {
    it("ships exactly two sidebar views", () => {
        // Server Status moved to an editor panel opened from the status bar: it is
        // consulted only when the indicator is not green, and a permanent third of the
        // sidebar was the wrong price for that.
        deepStrictEqual(
            pkg.contributes.views.mindex.map((v) => v.id),
            ["mindexAsk", "mindexDrift"]
        );
    });

    it("has no menu entry pointing at the removed status view", () => {
        // A `when` naming a view that no longer exists is not an error anywhere — the
        // entry simply never appears, silently.
        for (const [menu, entries] of Object.entries(pkg.contributes.menus)) {
            for (const e of entries) {
                ok(
                    !e.when.includes("mindexStatus"),
                    `${menu}: ${e.command} still targets the deleted mindexStatus view`
                );
            }
        }
    });

    it("registers a command for every menu entry", () => {
        const commands = new Set(pkg.contributes.commands.map((c) => c.command));
        for (const entries of Object.values(pkg.contributes.menus)) {
            for (const e of entries) {
                ok(commands.has(e.command), `${e.command} is in a menu but not contributed`);
            }
        }
    });

    /**
     * The brand is **MINDex**. The *identifiers* — command ids, setting ids, view ids,
     * the container id, the package name, `.mindex` — stay lowercase, and sweeping
     * them along with the prose would break every `when` clause and every user's
     * settings at once. This pins both halves.
     */
    it("says MINDex in prose and mindex in identifiers", () => {
        strictEqual(pkg.displayName, "MINDex");
        strictEqual(pkg.contributes.configuration.title, "MINDex");
        strictEqual(pkg.contributes.viewsContainers.activitybar[0].title, "MINDex");
        strictEqual(pkg.contributes.viewsContainers.activitybar[0].id, "mindex");
        for (const cmd of pkg.contributes.commands) {
            strictEqual(cmd.category, "MINDex", `${cmd.command} has the wrong category`);
            match(cmd.command, /^mindex\./, "command ids stay lowercase");
            // `.mindex` is the filename, not the brand, and stays as it is.
            ok(
                !/(^|[^.\w])mindex\b/i.test(cmd.title),
                `${cmd.command}: title says "mindex" where a user reads the brand`
            );
        }
    });

    it("contributes the two commands the redesign added", () => {
        const commands = new Set(pkg.contributes.commands.map((c) => c.command));
        for (const id of ["mindex.openStatus", "mindex.openSettings"]) {
            ok(commands.has(id), `${id} is referenced by the UI but not contributed`);
        }
    });
});

// Exercises the exported type without a runtime cost; `IconName` is otherwise only
// used at call sites the compiler already checks.
const _sample: IconName = "search";
void _sample;
