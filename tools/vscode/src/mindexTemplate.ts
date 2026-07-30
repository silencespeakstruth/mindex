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
}

// Globs the user is *likely* to want but that are not safe to assume: a `dist/` may
// be checked-in output worth searching, and guessing wrong silently shrinks the
// index. Offered as commented lines so accepting one costs a keystroke.
const SUGGESTED_EXCLUDES: readonly string[] = [
    "target/**",
    '"**/target/**"',
    '"**/node_modules/**"',
    '"**/dist/**"',
    '"**/build/**"',
    '"**/__pycache__/**"',
    '"**/*.lock"',
    '"**/package-lock.json"',
    '"**/*.pem"',
];

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

/** Renders the full text of a new .mindex file, newline-terminated. */
export function renderMindexTemplate(opts: TemplateOptions): string {
    const lines: string[] = [HEADER, "", `guid: ${opts.guid}`, "", "exclude_paths:"];

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
    } else {
        lines.push("  # Nothing was detected that is safe to exclude without asking.");
    }

    if (opts.otherDotDirs.length > 0) {
        lines.push(
            "  # Also present, but left in: a dot-directory is often where a project",
            "  # keeps its densest documentation. Uncomment what is noise to you.",
            ...[...opts.otherDotDirs].sort().map((dir) => `  # - ${dir}/**`)
        );
    }

    lines.push(
        "  # Common excludes, none of them assumed — uncomment what applies.",
        ...SUGGESTED_EXCLUDES.map((glob) => `  # - ${glob}`),
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
