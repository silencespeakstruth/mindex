import * as fs from "node:fs/promises";
import * as path from "node:path";
import { createHash } from "node:crypto";
import picomatch from "picomatch";
import { detectLanguage } from "./languages";
import { MindexFile } from "./mindexFile";

export interface ScannedFile {
    /** Forward-slash path relative to the workspace root (stored in the index as-is). */
    relPath: string;
    absPath: string;
    language: string;
}

export interface Manifest {
    files: ScannedFile[];
    /** relPath → sha256 hex, exactly what POST /drift expects. */
    hashes: Record<string, string>;
    /** Files skipped because they are not valid UTF-8 or exceed the size cap. */
    skipped: string[];
}

/**
 * Per-file `code` cap enforced server-side ([limits].max_code_bytes default).
 * Mirrors `mindexfile::MAX_CODE_BYTES`, which the indexer and watcher use for the
 * same purpose — keep the two in step (see CLAUDE.md, "Four clients, one
 * working-tree view"). An over-cap file must stay out of the drift manifest as
 * well as out of any upload, or it is reported `missing` on every check forever.
 */
const MAX_CODE_BYTES = 16 * 1024 * 1024;

const strictUtf8 = new TextDecoder("utf-8", { fatal: true });

type Matcher = (rel: string) => boolean;

/** Exported for the glob-contract test that keeps this in step with the Rust side. */
export function buildMatcher(patterns: string[]): Matcher | undefined {
    if (patterns.length === 0) {
        return undefined;
    }
    return picomatch(patterns, { dot: true });
}

/**
 * Walks the workspace applying the .mindex scope (exclude before include, then the
 * language filter) — the same rules as tools/indexer's scan(), so drift and index
 * always see the same file set (otherwise excluded files show up as orphaned).
 */
export async function scanWorkspace(root: string, scope: MindexFile): Promise<ScannedFile[]> {
    const exclude = buildMatcher(scope.excludePaths);
    const include = buildMatcher(scope.includePaths);
    const languages = scope.languages.length > 0 ? new Set(scope.languages) : undefined;

    const out: ScannedFile[] = [];
    await walk(root, "", new Set<string>(), (relPath, absPath) => {
        if (exclude?.(relPath)) {
            return;
        }
        if (include && !include(relPath)) {
            return;
        }
        const language = detectLanguage(relPath);
        if (language === undefined || (languages && !languages.has(language))) {
            return;
        }
        out.push({ relPath, absPath, language });
    });
    out.sort((a, b) => (a.relPath < b.relPath ? -1 : 1));
    return out;
}

/**
 * Directory symlinks *are* followed: both Rust walks use
 * `WalkDir::follow_links(true)`, and a directory this walk refuses to enter is a
 * subtree the indexer indexes and drift then reports as orphaned on every check,
 * with no way for the extension to ever clear it.
 *
 * `ancestors` holds the real path of every directory on the way *down to here*, so
 * a link pointing back up terminates. It is deliberately not a set of everything
 * visited: two links to one directory are two real paths in the tree, and the Rust
 * walk yields the files under both. Dropping the second would trade a cycle guard
 * for the very divergence this follows links to avoid.
 */
async function walk(
    absDir: string,
    relDir: string,
    ancestors: Set<string>,
    visit: (relPath: string, absPath: string) => void
): Promise<void> {
    let entries;
    let real;
    try {
        real = await fs.realpath(absDir);
        if (ancestors.has(real)) {
            return;
        }
        entries = await fs.readdir(absDir, { withFileTypes: true });
    } catch {
        return; // unreadable directory — skip, like walkdir's filter_map(ok)
    }
    ancestors.add(real);
    for (const entry of entries) {
        const rel = relDir === "" ? entry.name : `${relDir}/${entry.name}`;
        const abs = path.join(absDir, entry.name);
        // A symlink is a directory when it points at one; readdir reports the link
        // itself, so its target has to be asked about separately.
        const isDir =
            entry.isDirectory() || (entry.isSymbolicLink() && (await isDirLink(abs)));
        if (isDir) {
            if (entry.name === ".git") {
                continue;
            }
            await walk(abs, rel, ancestors, visit);
        } else if (entry.isFile() || entry.isSymbolicLink()) {
            visit(rel, abs);
        }
    }
    ancestors.delete(real);
}

/** Whether a symlink points at a directory. A broken link resolves to neither. */
async function isDirLink(abs: string): Promise<boolean> {
    try {
        return (await fs.stat(abs)).isDirectory();
    } catch {
        return false;
    }
}

/**
 * Reads a file and returns its UTF-8 content, or undefined for binary / over-cap
 * files (the watcher skips those too, so they never look "missing").
 */
export async function readUtf8(absPath: string): Promise<string | undefined> {
    let buf: Buffer;
    try {
        buf = await fs.readFile(absPath);
    } catch {
        return undefined;
    }
    if (buf.length > MAX_CODE_BYTES) {
        return undefined;
    }
    try {
        return strictUtf8.decode(buf);
    } catch {
        return undefined;
    }
}

/** Hashes every scanned file for POST /drift. Unreadable/binary files are omitted. */
export async function buildManifest(files: ScannedFile[]): Promise<Manifest> {
    const hashes: Record<string, string> = {};
    const skipped: string[] = [];
    for (const f of files) {
        const content = await readUtf8(f.absPath);
        if (content === undefined) {
            skipped.push(f.relPath);
            continue;
        }
        hashes[f.relPath] = createHash("sha256").update(content, "utf8").digest("hex");
    }
    return { files, hashes, skipped };
}
