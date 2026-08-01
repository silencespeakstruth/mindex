// The gitignore→.mindex translation table, run with `npm test` (node:test).
//
// Every case here is a difference between two glob languages that fails *silently*
// if it is got wrong: the index quietly keeps a build tree, or quietly loses a
// source tree. Neither shows up as an error, which is why they are pinned.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
    activeGlobs,
    reconcileSections,
    shouldDescend,
    translateGitignore,
} from "./gitignore";

/** Active globs from one .gitignore, which is what most cases are about. */
function derive(dir: string, text: string): string[] {
    return activeGlobs([translateGitignore(dir, text)]);
}

const CASES: [gitignore: string, expected: string[]][] = [
    // A bare name is a name at any depth, and may be a file or a directory. Both
    // forms are needed: without the second, `target` excludes nothing under target/.
    ["target", ["**/target", "**/target/**"]],
    // A trailing slash is directory-only, so the file form would match nothing.
    ["dist/", ["**/dist/**"]],
    // A leading slash anchors — and must be stripped, since the Rust parser rejects
    // any pattern that keeps it.
    ["/target", ["target", "target/**"]],
    // An inner slash anchors just as a leading one does.
    ["docs/_build/", ["docs/_build/**"]],
    ["build/*.o", ["build/*.o"]],
    // A dot after the first character reads as a file, so no directory form.
    ["*.pyc", ["**/*.pyc"]],
    ["Session.vim", ["**/Session.vim"]],
    // A *leading* dot does not: .vscode and .ruff_cache are directories far more
    // often than they are files.
    [".vscode", ["**/.vscode", "**/.vscode/**"]],
    // No dot at all is equally ambiguous.
    ["*pem", ["**/*pem", "**/*pem/**"]],
    // The subset both engines agree on passes through untouched.
    ["**/foo", ["**/foo", "**/foo/**"]],
    ["[ab].txt", ["**/[ab].txt"]],
    ["?.txt", ["**/?.txt"]],
    // Comments and blank lines are git's syntax, not YAML's.
    ["# a comment\n\n  \ntarget/", ["**/target/**"]],
];

void test("gitignore patterns translate to the documented glob subset", () => {
    for (const [text, expected] of CASES) {
        assert.deepEqual(derive("", text), expected, `gitignore \`${text}\``);
    }
});

void test("a nested .gitignore is relative to its own directory", () => {
    // The whole reason nested files can be read at all: tools/vscode/.gitignore's
    // `dist/` means tools/vscode/**/dist/, never the repository's own dist/.
    assert.deepEqual(derive("tools/vscode", "dist/\nmedia/js/\n*.vsix"), [
        "tools/vscode/**/dist/**",
        "tools/vscode/media/js/**",
        "tools/vscode/**/*.vsix",
    ]);
});

void test("nothing the Rust parser would reject is ever emitted", () => {
    // A leading `/` and a `\` are hard errors in mindexfile::build_globset, and a
    // `!` means negation to picomatch and a literal to globset. None may reach the
    // file — and a pattern that cannot be carried is announced, never dropped mute.
    const section = translateGitignore("", "/target\nfoo\\ bar\n{a,b}.txt\n!keep.log");
    for (const rule of section.rules) {
        assert.ok(!rule.glob.startsWith("/"), rule.glob);
        assert.ok(!rule.glob.includes("\\"), rule.glob);
        assert.ok(!rule.glob.startsWith("!"), rule.glob);
    }
    assert.equal(section.notes.length, 3);
    assert.match(section.notes[0], /backslashes/);
    assert.match(section.notes[1], /brace/);
    assert.match(section.notes[2], /re-admits nothing/);
});

void test("a negation disarms only what it could re-admit", () => {
    // `!.gitkeep` overlaps nothing above it, so the build tree stays excluded.
    const kept = translateGitignore("", "target/\n*.o\n!.gitkeep");
    assert.deepEqual(activeGlobs([kept]), ["**/target/**", "**/*.o"]);
});

void test("the jemalloc case disarms the rule that would eat the sources", () => {
    // /test/unit/[A-Za-z]* plus !/test/unit/[A-Za-z]*.* means "ignore the built
    // binaries, keep the sources next to them". Translated without the negation the
    // file rule means "every unit test's source disappears" — no error anywhere —
    // so it is commented out for review.
    //
    // The directory rule stays active, and that is not an oversight: git cannot
    // re-include a file whose parent directory is excluded, so nothing under an
    // ignored test/unit/<dir>/ was ever going to come back.
    const section = translateGitignore(
        "deps/jemalloc",
        "/test/unit/[A-Za-z]*\n!/test/unit/[A-Za-z]*.*"
    );
    assert.deepEqual(activeGlobs([section]), ["deps/jemalloc/test/unit/[A-Za-z]*/**"]);
    const disarmed = section.rules.find((r) => !r.active);
    assert.equal(disarmed?.glob, "deps/jemalloc/test/unit/[A-Za-z]*");
    assert.match(disarmed?.note ?? "", /re-admitted by/);
});

void test("a negation of a named file disarms the rule that would drop it", () => {
    const section = translateGitignore("", "build/\n!build/keep.txt");
    assert.deepEqual(activeGlobs([section]), []);
});

void test("the walk does not descend into what is already excluded", () => {
    // Load-bearing: without it this repository yields some forty .gitignore files
    // out of the third-party clones under perf/corpus/.clones/, and
    // .ruff_cache/.gitignore — whose entire content is `*` — becomes an exclude rule.
    const globs = derive("perf", "corpus/.clones/\n");
    assert.equal(shouldDescend("perf/corpus/.clones", globs), false);
    assert.equal(shouldDescend("perf/corpus", globs), true);
    // .git holds no source and appears in no .gitignore, so it is named outright.
    assert.equal(shouldDescend(".git", []), false);
    assert.equal(shouldDescend("src", []), true);
});

void test("a rule that would swallow the project is dropped, loudly", () => {
    // The `.ruff_cache/.gitignore` shape, reached anyway because it sat at a root
    // the walk had no rule to prune.
    const [section] = reconcileSections([translateGitignore("", "*\n")], []);
    assert.deepEqual(section.rules, []);
    assert.match(section.notes[0], /exclude the whole project/);
});

void test("a glob is emitted once, however many .gitignores name it", () => {
    // __pycache__/ genuinely arrives from four separate files in this repository.
    const sections = reconcileSections(
        [
            translateGitignore("", "__pycache__/\n*.pyc"),
            translateGitignore("embedder", "__pycache__/\n.venv/"),
        ],
        ["**/*.pyc"]
    );
    // The first section keeps __pycache__; the second's copy and the *.pyc the
    // caller already writes are both gone.
    assert.deepEqual(activeGlobs(sections), ["**/__pycache__/**", "embedder/**/.venv/**"]);
});
