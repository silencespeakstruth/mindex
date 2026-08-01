// The generated file has one hard obligation: it must satisfy the parser it will be
// read back by. Every case here renders a template and parses it, so a comment that
// accidentally becomes a value — or a key the schema does not know — fails loudly
// instead of shipping a file that disables the extension it was written to enable.

import test from "node:test";
import assert from "node:assert/strict";
import { parseMindexFile } from "./mindexFile";
import { NOISE_DOT_DIRS, renderMindexTemplate } from "./mindexTemplate";
import { reconcileSections, translateGitignore } from "./gitignore";

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

// This repository's own .gitignore files, verbatim — the corpus the feature was
// designed against, and the one place the whole pipeline is exercised end to end.
const CORPUS: [dir: string, text: string][] = [
    ["", "/target\n\n__pycache__/\n*.pyc\n\n*pem\n\n.vscode\n\n*.db*\n\ncerts/\n"],
    [
        "tools/vscode",
        "node_modules/\ndist/\n*.vsix\n\n# Generated\nmedia/js/\nout/\nmedia/codicons/\n",
    ],
    [
        "perf",
        "# Fetched corpus\ncorpus/data/\ncorpus/.clones/\n\nresults/\nresults.csv\nplots/\n",
    ],
    ["embedder", ".venv/\n.venv-*/\n__pycache__/\n*.pyc\n.ruff_cache/\n"],
];

function renderCorpus(): string {
    const sections = reconcileSections(
        CORPUS.map(([dir, text]) => translateGitignore(dir, text)),
        [".git/**"]
    );
    return renderMindexTemplate({
        guid: GUID,
        activeDotDirs: [".git"],
        otherDotDirs: [],
        gitignoreSections: sections,
    });
}

void test("this repository's .gitignore files translate to the expected scope", () => {
    // The assertion is on the *parsed* result, so it guards the YAML quoting too: a
    // glob starting with `*` emitted bare is a YAML alias, i.e. a .mindex that does
    // not parse at all.
    assert.deepEqual(parseMindexFile(renderCorpus()).excludePaths, [
        ".git/**",
        "target",
        "target/**",
        "**/__pycache__/**",
        "**/*.pyc",
        "**/*pem",
        "**/*pem/**",
        "**/.vscode",
        "**/.vscode/**",
        "**/*.db*",
        "**/certs/**",
        "tools/vscode/**/node_modules/**",
        "tools/vscode/**/dist/**",
        "tools/vscode/**/*.vsix",
        "tools/vscode/media/js/**",
        "tools/vscode/**/out/**",
        "tools/vscode/media/codicons/**",
        "perf/corpus/data/**",
        "perf/corpus/.clones/**",
        "perf/**/results/**",
        "perf/**/results.csv",
        "perf/**/plots/**",
        "embedder/**/.venv/**",
        // `.venv-*/` carries a trailing slash, so only the directory form — and the
        // embedder's `__pycache__/`/`*.pyc` are already covered by the root file's.
        "embedder/**/.venv-*/**",
        "embedder/**/.ruff_cache/**",
    ]);
});

void test("a suggestion already answered by .gitignore is not offered again", () => {
    const text = renderCorpus();
    // `**/__pycache__/**` is live, so its commented twin must be gone; `**/*.lock`
    // was never in a .gitignore here and must survive.
    assert.ok(!text.includes('# - "**/__pycache__/**"'), "stale suggestion");
    assert.ok(text.includes('# - "**/*.lock"'), "unrelated suggestion dropped");
});

void test("a project with no .gitignore renders exactly what it always did", () => {
    // The revert switch: the whole feature is additive on the empty case.
    const before = renderMindexTemplate({
        guid: GUID,
        activeDotDirs: [".git"],
        otherDotDirs: [".claude"],
    });
    const after = renderMindexTemplate({
        guid: GUID,
        activeDotDirs: [".git"],
        otherDotDirs: [".claude"],
        gitignoreSections: [],
    });
    assert.equal(before, after);
    assert.ok(!before.includes("From "), "provenance block leaked into the empty case");
});

void test("a disarmed rule is commented out and says why, and still parses", () => {
    const sections = reconcileSections(
        [translateGitignore("", "build/\n!build/keep.txt\nfoo\\ bar\n")],
        []
    );
    const text = renderMindexTemplate({
        guid: GUID,
        activeDotDirs: [],
        otherDotDirs: [],
        gitignoreSections: sections,
    });
    assert.deepEqual(parseMindexFile(text).excludePaths, []);
    assert.match(text, /# - "\*\*\/build\/\*\*"/);
    assert.match(text, /re-admitted by `!build\/keep\.txt`/);
    assert.match(text, /backslashes/);
});
