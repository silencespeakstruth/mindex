import * as vscode from "vscode";
import { randomUUID } from "node:crypto";
import { NOISE_DOT_DIRS, renderMindexTemplate } from "./mindexTemplate";
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
        const text = renderMindexTemplate({ guid: randomUUID(), activeDotDirs, otherDotDirs });
        // workspace.fs rather than node:fs so this works over remote and virtual
        // filesystems, and so the .mindex watcher sees the create reliably.
        await vscode.workspace.fs.writeFile(uri, Buffer.from(text, "utf8"));

        await reload();
        await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(uri));
        void vscode.window.showInformationMessage(
            "Created .mindex. Review exclude_paths, save, then run Check Drift."
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
