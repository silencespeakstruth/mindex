// Translates .gitignore files into .mindex `exclude_paths` globs, for the one
// moment it is safe to do so: writing a fresh marker.
//
// The motivation is that a project already states, authoritatively, what in it is
// generated — mindexTemplate.ts otherwise has to *guess*, and says so in its own
// comments. A .gitignore is not a guess. What it is, though, is a different glob
// language, and three of its differences are silent rather than loud:
//
//   1. mindexfile::build_globset REJECTS a pattern starting with `/` or containing
//      `\` (tools/mindexfile/src/lib.rs). gitignore anchors with a leading `/` and
//      escapes with `\`. The extension's own mindexFile.ts does not mirror that
//      check, so an untranslated pattern yields a .mindex that loads here and fails
//      in mindex-index and mindex-watch.
//   2. A leading `!` means negation to picomatch and a literal character to
//      globset — the two engines .mindex is read by. It must never reach the file.
//   3. Both scanners match globs against FILE paths only, never a bare directory.
//      So `target` excludes nothing under `target/`; only `target/**` does. This is
//      the difference that would quietly leave a whole build tree indexed.
//
// Kept free of `vscode` imports so it can be unit-tested under `node --test`; the
// filesystem walk that feeds it lives in createProject.ts.

import picomatch from "picomatch";

/** One translated pattern. Inactive rules are rendered commented-out, with `note`. */
export interface DerivedRule {
    glob: string;
    active: boolean;
    note?: string;
}

/** Everything one .gitignore contributed, kept together so its source can be named. */
export interface GitignoreSection {
    /** Root-relative path of the .gitignore, e.g. `tools/vscode/.gitignore`. */
    source: string;
    rules: DerivedRule[];
    /** Free-form lines rendered as comments: what was refused, and why. */
    notes: string[];
}

/**
 * Whether a glob has no literal content — only wildcards and separators.
 *
 * Such a glob cannot be a targeted exclusion; it names the whole tree, and emitting
 * one produces an empty index. Written as a predicate rather than a list of known
 * spellings because there are many (`*`, `**`, `**\/*`, `*\/**`, `**\/*\/**`) and a
 * list would go stale against the one that actually turns up.
 */
function namesEverything(glob: string): boolean {
    return glob.replace(/\[[^\]]*\]/g, "").replace(/[*?/]/g, "") === "";
}

/**
 * Translate one .gitignore.
 *
 * `dir` is the file's root-relative directory with no trailing slash (`""` for the
 * repo root) — gitignore patterns are relative to their own file, which is the
 * whole reason nested files can be read at all.
 */
export function translateGitignore(dir: string, text: string): GitignoreSection {
    const source = dir === "" ? ".gitignore" : `${dir}/.gitignore`;
    const rules: DerivedRule[] = [];
    const notes: string[] = [];
    const negations: string[] = [];

    for (const raw of text.split(/\r?\n/)) {
        // gitignore drops trailing whitespace unless it is backslash-escaped; a
        // backslash anywhere sends the line to the refusal branch below anyway.
        const line = raw.trimEnd();
        if (line === "" || line.startsWith("#")) {
            continue;
        }
        const negated = line.startsWith("!");
        const pattern = negated ? line.slice(1) : line;

        const refusal = untranslatable(pattern);
        if (refusal !== undefined) {
            notes.push(`\`${line}\` was not translated: ${refusal}.`);
            continue;
        }

        const globs = patternToGlobs(dir, pattern);
        if (globs.length === 0) {
            continue;
        }
        if (negated) {
            // Recorded, never emitted: .mindex has no negation, and `include_paths`
            // cannot re-admit what an exclude dropped (excludes are applied first).
            negations.push(line);
            continue;
        }
        for (const glob of globs) {
            rules.push({ glob, active: true });
        }
    }

    disarm(rules, negations, dir, notes);
    return { source, rules, notes };
}

/**
 * Why a pattern cannot be carried across, or `undefined` if it can.
 *
 * Refusing loudly is the point. A pattern silently dropped is a subtree that stays
 * in the index; a pattern silently mistranslated is a subtree that leaves it.
 */
function untranslatable(pattern: string): string | undefined {
    if (pattern.includes("\\")) {
        return "mindex globs reject backslashes, and gitignore escapes with them";
    }
    if (pattern.includes('"')) {
        return "a quote cannot be carried through the YAML scalar";
    }
    if (pattern.includes("{") || pattern.includes("}")) {
        return "a brace is a literal to git but an alternation to the glob engines";
    }
    return undefined;
}

/**
 * One gitignore pattern to the zero, one or two `.mindex` globs that mean the same.
 *
 * Anchoring: git anchors a pattern to its own directory iff a `/` appears anywhere
 * but the end; otherwise it matches at any depth below. A trailing `/` means
 * directory-only. A pattern that is neither may be a file or a directory, so both
 * forms are emitted — dropping `X/**` is difference 3 in the header comment.
 */
function patternToGlobs(dir: string, pattern: string): string[] {
    const dirOnly = pattern.endsWith("/");
    let core = dirOnly ? pattern.slice(0, -1) : pattern;
    const anchored = core.includes("/");
    if (core.startsWith("/")) {
        // Stripped rather than kept: a leading `/` is a hard error in the Rust parser.
        core = core.slice(1);
    }
    if (core === "") {
        return [];
    }

    const prefix = dir === "" ? "" : `${dir}/`;
    const base = anchored ? `${prefix}${core}` : `${prefix}**/${core}`;

    if (dirOnly) {
        return [`${base}/**`];
    }
    return looksLikeFileName(core) ? [base] : [base, `${base}/**`];
}

/**
 * Whether the pattern's last segment names a file rather than a directory.
 *
 * A dot after the first character is the signal (`*.pyc`, `foo.db`, `Session.vim`);
 * a leading dot alone is not, because `.vscode` and `.ruff_cache` are directories
 * far more often than they are files. Guessing "file" only suppresses the `/**`
 * form, so the cost of guessing wrong here is one unexcluded directory — never an
 * unindexed one.
 */
function looksLikeFileName(core: string): boolean {
    const last = core.slice(core.lastIndexOf("/") + 1);
    return last.indexOf(".") > 0;
}

/**
 * Comment out every positive rule a negation could have re-admitted.
 *
 * `.mindex` has no `!`, so the alternative to this is over-excluding in silence —
 * the jemalloc .gitignore vendored under perf/corpus/ is the textbook case:
 * `/test/unit/[A-Za-z]*` plus `!/test/unit/[A-Za-z]*.*` means "every unit test's
 * source disappears", with no error anywhere.
 *
 * Overlap is decided by materializing the negation into concrete sample paths and
 * asking whether a positive glob matches one. `!.gitkeep` then disarms nothing and
 * the rule for `target` survives; `!build/keep.txt` disarms the rule for `build/`,
 * which is right, because that exclusion really does drop more than git does.
 */
function disarm(
    rules: DerivedRule[],
    negations: string[],
    dir: string,
    notes: string[]
): void {
    for (const negation of negations) {
        const globs = patternToGlobs(dir, negation.slice(1));
        const samples = globs.flatMap(materialize);
        let hit = false;
        for (const rule of rules) {
            if (!rule.active) {
                continue;
            }
            const match = picomatch(rule.glob, { dot: true });
            if (samples.some((s) => match(s))) {
                rule.active = false;
                rule.note = `re-admitted by \`${negation}\``;
                hit = true;
            }
        }
        if (!hit) {
            notes.push(
                `\`${negation}\` re-admits nothing the rules above exclude, so it was dropped.`
            );
        }
    }
}

/**
 * Concrete paths a glob would match, for overlap testing.
 *
 * Two per glob because `**\/` matches zero directories as well as one, and the
 * two cases can be matched by different positive rules. Testing both disarms more
 * rather than fewer, which is the safe direction: an unexcluded build directory is
 * visible in the index, an excluded source tree is not.
 */
function materialize(glob: string): string[] {
    // A trailing `/**` is the directory form; its samples must still be file paths,
    // since that is all either scanner ever matches a glob against.
    const core = glob.endsWith("/**") ? `${glob.slice(0, -3)}/x` : glob;
    const fill = (doubleStar: string) =>
        core
            .replace(/\*\*\/?/g, doubleStar)
            .replace(/\[[^\]]*\]/g, "x")
            .replace(/[*?]/g, "x")
            .replace(/\/+/g, "/")
            .replace(/^\/|\/$/g, "");
    return [fill(""), fill("d/")];
}

/**
 * Whether the walk should enter `relDir` given the globs derived so far.
 *
 * Load-bearing rather than an optimization. Without it this very repository yields
 * some forty .gitignore files out of the third-party clones under
 * perf/corpus/.clones/, and `.ruff_cache/.gitignore` — whose entire content is `*`
 * — becomes an exclude rule.
 */
export function shouldDescend(relDir: string, globs: string[]): boolean {
    const name = relDir.slice(relDir.lastIndexOf("/") + 1);
    if (name === ".git") {
        return false;
    }
    if (globs.length === 0) {
        return true;
    }
    // Globs are matched against file paths, never directories, so ask about a file
    // the directory would contain.
    return !picomatch(globs, { dot: true })(`${relDir}/x`);
}

/**
 * Drop what must not be emitted twice, and what must not be emitted at all.
 *
 * `already` is the globs the template writes from another source (the root dot-dirs).
 * Redundancy is judged by coverage, not by string equality: `__pycache__/` genuinely
 * arrives from four separate .gitignore files here, and the nested copies spell
 * themselves `embedder/**\/__pycache__/**` — different text, nothing new excluded.
 */
export function reconcileSections(
    sections: GitignoreSection[],
    already: string[]
): GitignoreSection[] {
    const kept = [...already];
    const out: GitignoreSection[] = [];

    for (const section of sections) {
        const rules: DerivedRule[] = [];
        const notes = [...section.notes];
        for (const rule of section.rules) {
            // A rule matching the marker matches everything worth indexing too. An
            // empty index is not a scope.
            if (namesEverything(rule.glob) || picomatch(rule.glob, { dot: true })(".mindex")) {
                notes.push(
                    `\`${rule.glob}\` was dropped: it would exclude the whole project.`
                );
                continue;
            }
            if (rule.active && covered(kept, rule.glob)) {
                continue;
            }
            if (rule.active) {
                kept.push(rule.glob);
            }
            rules.push(rule);
        }
        if (rules.length > 0 || notes.length > 0) {
            out.push({ source: section.source, rules, notes });
        }
    }
    return out;
}

/**
 * Whether globs already emitted exclude everything `glob` would.
 *
 * Decided on materialized samples, so it is an approximation — but a directional
 * one: a false positive drops a redundant rule and indexes marginally more, which is
 * visible in the index and costs a scroll. It can never remove an exclusion nothing
 * else covers, because every sample must be matched, not merely one.
 */
function covered(kept: string[], glob: string): boolean {
    if (kept.length === 0) {
        return false;
    }
    const match = picomatch(kept, { dot: true });
    return materialize(glob).every((sample) => match(sample));
}

/** Every glob a reconciled set will actually apply. Used to suppress suggestions. */
export function activeGlobs(sections: GitignoreSection[]): string[] {
    return sections.flatMap((s) => s.rules.filter((r) => r.active).map((r) => r.glob));
}
