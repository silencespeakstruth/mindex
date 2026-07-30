// The generated file has one hard obligation: it must satisfy the parser it will be
// read back by. Every case here renders a template and parses it, so a comment that
// accidentally becomes a value — or a key the schema does not know — fails loudly
// instead of shipping a file that disables the extension it was written to enable.

import test from "node:test";
import assert from "node:assert/strict";
import { parseMindexFile } from "./mindexFile";
import { NOISE_DOT_DIRS, renderMindexTemplate } from "./mindexTemplate";

const GUID = "123e4567-e89b-42d3-a456-426614174000";

void test("a rendered template parses, and only the active dot-dirs are scope", () => {
    const f = parseMindexFile(
        renderMindexTemplate({
            guid: GUID,
            activeDotDirs: [".venv", ".git"],
            otherDotDirs: [".claude", ".github"],
        })
    );
    assert.equal(f.guid, GUID);
    // .git first, then the rest sorted; the suggestions and the other dot-dirs stay
    // commented out and must not leak into the scope.
    assert.deepEqual(f.excludePaths, [".git/**", ".venv/**"]);
    assert.deepEqual(f.includePaths, []);
    assert.deepEqual(f.languages, []);
});

void test("a template with nothing detected still parses to an empty scope", () => {
    // The likeliest way to break this file: `exclude_paths:` followed only by
    // comments is a null value, not a string and not a list.
    const f = parseMindexFile(
        renderMindexTemplate({ guid: GUID, activeDotDirs: [], otherDotDirs: [] })
    );
    assert.deepEqual(f.excludePaths, []);
});

void test("every known noise dot-dir renders as an active exclude", () => {
    const f = parseMindexFile(
        renderMindexTemplate({
            guid: GUID,
            activeDotDirs: [...NOISE_DOT_DIRS],
            otherDotDirs: [],
        })
    );
    assert.deepEqual(
        [...f.excludePaths].sort(),
        [...NOISE_DOT_DIRS].map((d) => `${d}/**`).sort()
    );
});

void test("a generated guid round-trips unchanged", () => {
    const guid = "5f3c9a2b-7d41-4e8a-9c06-1b2e3d4f5a6b";
    assert.equal(
        parseMindexFile(renderMindexTemplate({ guid, activeDotDirs: [], otherDotDirs: [] }))
            .guid,
        guid
    );
});
