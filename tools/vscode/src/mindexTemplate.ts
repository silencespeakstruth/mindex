// Generator for a fresh .mindex file. The comments in the emitted text are the
// feature: the extension writes the file, the user reads the prose, uncomments the
// few lines that apply to their project and saves — at which point the watcher in
// extension.ts re-reads it and the extension comes alive.
//
// The header block below duplicates the schema documented by tools/mindexfile (Rust,
// the reference parser) and mirrored by mindexFile.ts. All three must stay in step:
// prose that lies about the schema is worse than no prose, because the file it
// produces still parses.
//
// Kept free of `vscode` imports so it can be unit-tested under `node --test`.

import { activeGlobs, GitignoreSection } from "./gitignore";

/**
 * Dot-directories excluded outright when found at the project root.
 *
 * Short on purpose. Each entry is tool or VCS state that can never contain source
 * worth retrieving, so excluding it needs no judgement about the project. Anything
 * else starting with `.` is offered commented-out instead: `.claude/`, `.github/`
 * and friends routinely hold the densest documentation in a repo, and a blanket
 * `**\/.*\/**` could not be carved back open — excludes are applied *before*
 * includes, so `include_paths` cannot rescue a subtree its own excludes dropped.
 */
export const NOISE_DOT_DIRS: readonly string[] = [
    ".git",
    ".venv",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    ".tox",
    ".gradle",
    ".cache",
];

export interface TemplateOptions {
    /** Project GUID, either UUID spelling — it is written out verbatim. */
    guid: string;
    /** Root dot-dirs from NOISE_DOT_DIRS that exist; emitted as active excludes. */
    activeDotDirs: string[];
    /** Every other root dot-dir that exists; emitted commented-out. */
    otherDotDirs: string[];
    /**
     * Excludes translated from the project's own .gitignore files, already
     * reconciled (see gitignore.ts). Empty restores the pre-.gitignore output
     * byte-for-byte, which is both the no-git case and the revert switch.
     */
    gitignoreSections?: GitignoreSection[];
    /** Announced when the .gitignore walk stopped early rather than finished. */
    walkTruncated?: boolean;
}

// Globs the user is *likely* to want but that are not safe to assume: a `dist/` may
// be checked-in output worth searching, and guessing wrong silently shrinks the
// index. Offered as commented lines so accepting one costs a keystroke.
const SUGGESTED_EXCLUDES: readonly string[] = [
    "target/**",
    "**/target/**",
    "**/node_modules/**",
    "**/dist/**",
    "**/build/**",
    "**/__pycache__/**",
    "**/*.lock",
    "**/package-lock.json",
    "**/*.pem",
];

/**
 * A glob as a YAML scalar.
 *
 * Load-bearing rather than cosmetic: a glob beginning with `*` is a YAML *alias*,
 * so emitting one bare produces a .mindex that does not parse at all — and every
 * derived exclude is arbitrary text from someone else's .gitignore rather than a
 * literal typed here. gitignore.ts refuses any pattern containing `\` or `"`
 * upstream, which is what lets this quote without ever having to escape.
 */
export function yamlScalar(glob: string): string {
    const plain = /^[A-Za-z0-9_.]/.test(glob) && !/[#:]/.test(glob);
    return plain ? glob : `"${glob}"`;
}

const HEADER = `# MINDex project marker (YAML). Read by mindex-index, mindex-watch, the VS Code
# extension and the post-commit hook; the MCP servers don't parse it — the agent
# reads it and passes the GUID and filters as call arguments. One file, at the
# repo root, no nesting.
#
# Keys: guid (required, dashed or dashless UUID), plus the optional scope lists
# exclude_paths / include_paths / languages / git_refs. An unknown key is an error, not a
# silent no-op — a mistyped \`exclude_path:\` would otherwise index the tree it was
# meant to keep out. Globs are root-relative with forward slashes; \`*\` stops at a
# directory separator, \`**\` crosses them; excludes are applied before includes, so
# an include cannot re-admit what an exclude dropped.
#
# This file was generated. Adjust the lists below, save, and the extension picks
# the change up immediately — no window reload.`;

// Added only when there was something to read. The excludes below are the one part
// of this file not written from guesswork, and the user should know which part that
// is before deciding what to keep.
const GITIGNORE_NOTE = `# The excludes marked with a source file were read from this project's own
# .gitignore files and translated: git's pattern language is not this one, and
# what it cannot express (a \`!\` re-inclusion, an escaped character) is left
# commented out with the reason. Nothing else is derived. mindex itself never
# reads .gitignore — this happened once, when the file was created.`;

/** Renders the full text of a new .mindex file, newline-terminated. */
export function renderMindexTemplate(opts: TemplateOptions): string {
    const sections = opts.gitignoreSections ?? [];
    const header = sections.length === 0 ? HEADER : `${HEADER}\n#\n${GITIGNORE_NOTE}`;
    const lines: string[] = [header, "", `guid: ${opts.guid}`, "", "exclude_paths:"];

    // .git first, then the rest alphabetically: the one entry every project has
    // should not sort into the middle of a list nobody reads twice.
    const active = [...opts.activeDotDirs].sort((a, b) => {
        if (a === ".git") return -1;
        if (b === ".git") return 1;
        return a.localeCompare(b);
    });

    if (active.length > 0) {
        lines.push("  # Version control and tool state found in this project.");
        for (const dir of active) {
            lines.push(`  - ${dir}/**`);
        }
    } else if (sections.length === 0) {
        lines.push("  # Nothing was detected that is safe to exclude without asking.");
    }

    if (opts.otherDotDirs.length > 0) {
        lines.push(
            "  # Also present, but left in: a dot-directory is often where a project",
            "  # keeps its densest documentation. Uncomment what is noise to you.",
            ...[...opts.otherDotDirs].sort().map((dir) => `  # - ${dir}/**`)
        );
    }

    lines.push(...renderSections(sections, opts.walkTruncated === true));

    // A suggestion sitting directly under the live version of itself reads as an
    // oversight, so anything .gitignore already answered is dropped from the offer.
    const derived = new Set(activeGlobs(sections));
    const suggestions = SUGGESTED_EXCLUDES.filter((glob) => !derived.has(glob));
    if (suggestions.length > 0) {
        lines.push(
            "  # Common excludes, none of them assumed — uncomment what applies.",
            ...suggestions.map((glob) => `  # - ${yamlScalar(glob)}`)
        );
    }

    lines.push(
        "",
        "# Empty means the whole tree (minus the excludes above). A non-empty list",
        "# narrows indexing to what it matches.",
        "include_paths: []",
        "",
        "# Empty means every language MINDex supports; the canonical list is served",
        "# by the server at GET /config.",
        "languages: []",
        "",
        "# Ref patterns whose commits make up this project's history, e.g.",
        '# ["master", "dev", "feat/*"]. Empty means the current branch alone —',
        "# check that first if the default branch was ever squashed, since the prose",
        "# then lives on the branches that were never merged. Only mindex-index reads",
        "# this, and only when run with --history; it is off by default.",
        "git_refs: []",
        ""
    );

    return lines.join("\n");
}

/**
 * The .gitignore-derived blocks, one per source file.
 *
 * Each block names the file it came from, because that is the argument for trusting
 * it: unlike the suggestions above, these are not guesses about the project — they
 * are the project's own statement of what in it is generated. Naming the source is
 * also what makes them reviewable, and this repository's own .mindex already writes
 * that provenance by hand ("The list mirrors tools/vscode/.gitignore, which is the
 * authority on what in that directory is generated").
 */
function renderSections(sections: GitignoreSection[], truncated: boolean): string[] {
    if (sections.length === 0) {
        return [];
    }
    const lines: string[] = [];
    for (const section of sections) {
        lines.push(
            `  # From ${section.source} — what the project itself says is generated.`,
            "  # Translated, not copied: review before committing."
        );
        for (const note of section.notes) {
            lines.push(...wrapComment(note, "  # "));
        }
        for (const rule of section.rules) {
            if (rule.active) {
                lines.push(`  - ${yamlScalar(rule.glob)}`);
            } else {
                lines.push(`  # - ${yamlScalar(rule.glob)}`);
                if (rule.note !== undefined) {
                    lines.push(...wrapComment(`(${rule.note})`, "  #   "));
                }
            }
        }
    }
    if (truncated) {
        lines.push(
            "  # The search for .gitignore files stopped at its limit before the tree",
            "  # ended, so deeper ones were not read. Nothing here is wrong; it may",
            "  # just be incomplete."
        );
    }
    return lines;
}

/** Wraps a note to the file's comment width, so no generated line runs off-screen. */
function wrapComment(text: string, prefix: string): string[] {
    const width = 79 - prefix.length;
    const lines: string[] = [];
    let current = "";
    for (const word of text.split(" ")) {
        if (current === "") {
            current = word;
        } else if (current.length + 1 + word.length <= width) {
            current += ` ${word}`;
        } else {
            lines.push(prefix + current);
            current = word;
        }
    }
    if (current !== "") {
        lines.push(prefix + current);
    }
    return lines;
}
