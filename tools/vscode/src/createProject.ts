import * as vscode from "vscode";
import { randomUUID } from "node:crypto";
import { NOISE_DOT_DIRS, renderMindexTemplate } from "./mindexTemplate";
import {
    activeGlobs,
    GitignoreSection,
    reconcileSections,
    shouldDescend,
    translateGitignore,
} from "./gitignore";
import { reportError } from "./errors";
import { BRAND } from "./brand";

/**
 * Write a fresh `.mindex` at a workspace root and open it for editing.
 *
 * The recovery path for the state the extension deliberately refuses to work in: no
 * marker, no project, every command rejecting. `reload` is the caller's
 * `reloadProject` — awaited so the `mindex.hasProject` context key flips before the
 * editor opens, rather than racing the file watcher that will also fire.
 *
 * Never overwrites: an existing file is opened instead, valid or not.
 */
export async function createProjectFile(reload: () => Promise<void>): Promise<void> {
    try {
        const folder = await pickFolder();
        if (folder === undefined) {
            return;
        }
        const uri = vscode.Uri.joinPath(folder.uri, ".mindex");
        if (await exists(uri)) {
            void vscode.window.showInformationMessage(
                `${folder.name} already has a .mindex — opening it.`
            );
            await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(uri));
            return;
        }

        const { activeDotDirs, otherDotDirs } = await scanDotDirs(folder.uri);
        const dotDirGlobs = activeDotDirs.map((dir) => `${dir}/**`);
        const { sections, truncated } = await collectGitignores(folder.uri, dotDirGlobs);
        const text = renderMindexTemplate({
            guid: randomUUID(),
            activeDotDirs,
            otherDotDirs,
            gitignoreSections: reconcileSections(sections, dotDirGlobs),
            walkTruncated: truncated,
        });
        // workspace.fs rather than node:fs so this works over remote and virtual
        // filesystems, and so the .mindex watcher sees the create reliably.
        await vscode.workspace.fs.writeFile(uri, Buffer.from(text, "utf8"));

        await reload();
        await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(uri));
        void vscode.window.showInformationMessage(
            sections.length === 0
                ? "Created .mindex. Review exclude_paths, save, then run Check Drift."
                : `Created .mindex with excludes from ${sections.length} .gitignore ` +
                      "file(s). Review exclude_paths, save, then run Check Drift."
        );
    } catch (e) {
        await reportError("Creating .mindex failed", e);
    }
}

async function pickFolder(): Promise<vscode.WorkspaceFolder | undefined> {
    const folders = vscode.workspace.workspaceFolders ?? [];
    if (folders.length === 0) {
        void vscode.window.showErrorMessage(
            "Open a folder first — .mindex lives at the project root."
        );
        return undefined;
    }
    if (folders.length === 1) {
        return folders[0];
    }
    // Multi-root: the first folder holding a marker wins at load time, so which one
    // gets the file is a real choice and cannot be guessed.
    return await vscode.window.showWorkspaceFolderPick({
        placeHolder: `Which folder is the ${BRAND} project root?`,
    });
}

async function exists(uri: vscode.Uri): Promise<boolean> {
    try {
        await vscode.workspace.fs.stat(uri);
        return true;
    } catch {
        return false;
    }
}

/**
 * Cap on how much tree is searched for .gitignore files.
 *
 * A file is being written while the user waits, so the walk is bounded rather than
 * exhaustive — and hitting a bound is announced in the generated file, never
 * swallowed. Thirty-two files is far past any project that keeps one .gitignore per
 * component; a project needing more has a scope question the template cannot answer.
 */
const MAX_GITIGNORE_FILES = 32;
const MAX_WALK_DIRS = 2000;

/**
 * Every .gitignore in the project, translated, outermost first.
 *
 * Breadth-first and **ignore-aware**: a directory the rules found so far already
 * exclude is not entered. That is load-bearing rather than an optimization — in this
 * repository alone, descending blindly reaches some forty .gitignore files inside
 * the third-party clones under perf/corpus/.clones/, and `.ruff_cache/.gitignore`,
 * whose entire content is `*`, would contribute an exclude rule.
 *
 * Outermost first also matters: a parent's rules must exist before its children are
 * considered for pruning, and reconcileSections resolves redundancy in the same
 * order, so the shortest spelling of a repeated rule is the one that survives.
 */
async function collectGitignores(
    root: vscode.Uri,
    seedGlobs: string[]
): Promise<{ sections: GitignoreSection[]; truncated: boolean }> {
    const sections: GitignoreSection[] = [];
    const globs = [...seedGlobs];
    let queue: string[] = [""];
    let visited = 0;
    let truncated = false;

    while (queue.length > 0) {
        const next: string[] = [];
        for (const dir of queue) {
            if (visited >= MAX_WALK_DIRS || sections.length >= MAX_GITIGNORE_FILES) {
                truncated = true;
                return { sections, truncated };
            }
            visited += 1;

            const uri = dir === "" ? root : vscode.Uri.joinPath(root, dir);
            let entries: [string, vscode.FileType][];
            try {
                entries = await vscode.workspace.fs.readDirectory(uri);
            } catch {
                // An unreadable directory contributes nothing; it must not cost the
                // user their .mindex.
                continue;
            }

            const text = await readGitignore(uri, entries);
            if (text !== undefined) {
                const section = translateGitignore(dir, text);
                sections.push(section);
                globs.push(...activeGlobs([section]));
            }

            for (const [name, type] of entries) {
                if ((type & vscode.FileType.Directory) === 0) {
                    continue;
                }
                const child = dir === "" ? name : `${dir}/${name}`;
                if (shouldDescend(child, globs)) {
                    next.push(child);
                }
            }
        }
        queue = next;
    }
    return { sections, truncated };
}

async function readGitignore(
    dir: vscode.Uri,
    entries: [string, vscode.FileType][]
): Promise<string | undefined> {
    const isFile = ([name, type]: [string, vscode.FileType]) =>
        name === ".gitignore" && (type & vscode.FileType.File) !== 0;
    if (!entries.some(isFile)) {
        return undefined;
    }
    try {
        const bytes = await vscode.workspace.fs.readFile(
            vscode.Uri.joinPath(dir, ".gitignore")
        );
        return Buffer.from(bytes).toString("utf8");
    } catch {
        return undefined;
    }
}

/** Root-level dot-directories, split into known noise and everything else. */
async function scanDotDirs(
    root: vscode.Uri
): Promise<{ activeDotDirs: string[]; otherDotDirs: string[] }> {
    const activeDotDirs: string[] = [];
    const otherDotDirs: string[] = [];
    let entries: [string, vscode.FileType][];
    try {
        entries = await vscode.workspace.fs.readDirectory(root);
    } catch {
        // An unreadable root is not worth failing the file over — the template's
        // commented suggestions still say everything the user needs.
        return { activeDotDirs, otherDotDirs };
    }
    for (const [name, type] of entries) {
        if (!name.startsWith(".") || (type & vscode.FileType.Directory) === 0) {
            continue;
        }
        if (NOISE_DOT_DIRS.includes(name)) {
            activeDotDirs.push(name);
        } else {
            otherDotDirs.push(name);
        }
    }
    return { activeDotDirs, otherDotDirs };
}
