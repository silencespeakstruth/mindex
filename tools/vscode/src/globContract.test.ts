// The cross-implementation glob contract, run with `npm test` (node:test).
//
// The Rust tools compile .mindex globs with `globset`, this extension with
// `picomatch`. Two engines will never agree on everything, but they must agree on
// the subset .mindex is documented to support — otherwise a file the indexer skips
// and the extension scans sits in the Drift view as "orphaned" forever, which is a
// silent bug rather than an error. The table below is byte-identical to
// `glob_contract_matches_the_documented_subset` in tools/mindexfile/src/lib.rs;
// change one, change both.

import { test } from "node:test";
import assert from "node:assert/strict";
import { buildMatcher } from "./scanner";
import { parseMindexFile } from "./mindexFile";

// Every path here is a FILE path: both scanners match globs against files only,
// never against a bare directory, and that is where the two engines still differ
// (picomatch says `tools/**` matches `tools`, globset says it does not).
const CASES: [pattern: string, path: string, expected: boolean][] = [
    ["tools/**", "tools/a.rs", true],
    ["tools/**", "tools/deep/nested/a.rs", true],
    ["tools/**", "src/tools/a.rs", false],
    ["**/target/**", "a/b/target/x.rs", true],
    ["**/target/**", "target/x.rs", true],
    ["**/*.lock", "Cargo.lock", true],
    ["**/*.lock", "tools/indexer/Cargo.lock", true],
    ["src/*.rs", "src/main.rs", true],
    ["src/*.rs", "src/db/qdrant.rs", false],
    ["src/?.rs", "src/a.rs", true],
    ["src/?.rs", "src/ab.rs", false],
    ["src/[ab].rs", "src/a.rs", true],
    ["src/[ab].rs", "src/c.rs", false],
    [".claude/**", ".claude/settings.json", true],
    ["**/.venv/**", "tools/mcp/.venv/lib/x.py", true],
];

void test("glob contract matches the documented subset", () => {
    for (const [pattern, path, expected] of CASES) {
        const match = buildMatcher([pattern]);
        assert.ok(match !== undefined);
        assert.equal(match(path), expected, `pattern \`${pattern}\` vs path \`${path}\``);
    }
});

void test("an empty pattern list means no filter, not no files", () => {
    assert.equal(buildMatcher([]), undefined);
});

const GUID = "123e4567-e89b-42d3-a456-426614174000";

void test("parses guid, scope lists and comments", () => {
    const f = parseMindexFile(
        `# a comment\n\nguid: ${GUID}\ninclude_paths:\n  - src/**\nexclude_paths:\n  - target/**\nlanguages:\n  - rust\ngit_refs:\n  - master\n`
    );
    assert.equal(f.guid, GUID);
    assert.deepEqual(f.includePaths, ["src/**"]);
    assert.deepEqual(f.excludePaths, ["target/**"]);
    assert.deepEqual(f.languages, ["rust"]);
    assert.deepEqual(f.gitRefs, ["master"]);
});

void test("guid-only file has empty scope", () => {
    const f = parseMindexFile(`guid: ${GUID}\n`);
    assert.deepEqual(f.includePaths, []);
    assert.deepEqual(f.excludePaths, []);
    assert.deepEqual(f.languages, []);
    assert.deepEqual(f.gitRefs, []);
});

void test("a dashless guid is normalized to hyphenated", () => {
    assert.equal(parseMindexFile("guid: 123e4567e89b42d3a456426614174000\n").guid, GUID);
});

void test("missing, malformed and unknown keys are errors", () => {
    assert.throws(() => parseMindexFile("exclude_paths:\n  - src/**\n"), /guid/);
    assert.throws(() => parseMindexFile("guid: not-a-uuid\n"), /not a UUID/);
    // The typo this format change exists to catch.
    assert.throws(
        () => parseMindexFile(`guid: ${GUID}\nexclude_path:\n  - t/**\n`),
        /unknown key/
    );
    // The old comma-separated form must fail loudly, not parse as one odd glob.
    assert.throws(
        () => parseMindexFile(`guid: ${GUID}\nexclude_paths: a/**, b/**\n`),
        /list of strings/
    );
    assert.throws(() => parseMindexFile("just a string\n"), /YAML mapping/);
});
