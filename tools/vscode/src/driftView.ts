import * as vscode from "vscode";
import * as path from "node:path";
import { DriftResponse } from "./api";

export type Bucket = "stale" | "missing" | "orphaned" | "indexing";

const BUCKETS: { id: Bucket; label: string; icon: string; tooltip: string }[] = [
    { id: "stale", label: "Stale", icon: "diff-modified", tooltip: "Indexed, but the working-tree content differs — needs reindex" },
    { id: "missing", label: "Missing", icon: "diff-added", tooltip: "Present locally but not indexed (never indexed or failed) — needs reindex" },
    { id: "orphaned", label: "Orphaned", icon: "diff-removed", tooltip: "Indexed, but absent from the working tree — should be deleted from the index" },
    { id: "indexing", label: "Indexing", icon: "sync", tooltip: "In flight on the server — no action needed" },
];

/** A bucket whose files can be checkbox-selected (indexing is read-only). */
const SELECTABLE: ReadonlySet<Bucket> = new Set(["stale", "missing", "orphaned"]);

export class DriftNode {
    constructor(
        public readonly kind: "bucket" | "dir" | "file",
        public readonly bucket: Bucket,
        /** dir: the rel prefix; file: the full rel path; bucket: "". */
        public readonly relPath: string,
        public readonly label: string,
        public readonly children: DriftNode[]
    ) {}

    *files(): Iterable<DriftNode> {
        if (this.kind === "file") {
            yield this;
        }
        for (const c of this.children) {
            yield* c.files();
        }
    }
}

export class DriftTreeProvider implements vscode.TreeDataProvider<DriftNode> {
    private readonly changed = new vscode.EventEmitter<DriftNode | undefined>();
    readonly onDidChangeTreeData = this.changed.event;

    private roots: DriftNode[] = [];
    private selected: Record<Bucket, Set<string>> = emptySelection();
    /** When the last drift check ran, for the view description. */
    private checkedAt: Date | undefined;

    constructor(private readonly workspaceRoot: string) {}

    setDrift(drift: DriftResponse): void {
        this.roots = BUCKETS.map((b) => buildBucketTree(b.id, drift[b.id]));
        this.selected = emptySelection();
        this.checkedAt = new Date();
        this.changed.fire(undefined);
    }

    clear(): void {
        this.roots = [];
        this.selected = emptySelection();
        this.checkedAt = undefined;
        this.changed.fire(undefined);
    }

    get lastCheckedAt(): Date | undefined {
        return this.checkedAt;
    }

    /** Checked file paths in the given buckets. */
    selectedPaths(...buckets: Bucket[]): string[] {
        return buckets.flatMap((b) => [...this.selected[b]]).sort();
    }

    /** Every file path in the given buckets, selected or not. */
    allPaths(...buckets: Bucket[]): string[] {
        const wanted = new Set(buckets);
        return this.roots
            .filter((r) => wanted.has(r.bucket))
            .flatMap((r) => [...r.files()].map((f) => f.relPath))
            .sort();
    }

    applyCheckboxChanges(items: readonly [DriftNode, vscode.TreeItemCheckboxState][]): void {
        for (const [node, state] of items) {
            if (!SELECTABLE.has(node.bucket)) {
                continue;
            }
            const set = this.selected[node.bucket];
            for (const f of node.files()) {
                if (state === vscode.TreeItemCheckboxState.Checked) {
                    set.add(f.relPath);
                } else {
                    set.delete(f.relPath);
                }
            }
        }
    }

    getChildren(element?: DriftNode): DriftNode[] {
        return element === undefined ? this.roots : element.children;
    }

    getTreeItem(node: DriftNode): vscode.TreeItem {
        if (node.kind === "bucket") {
            const meta = BUCKETS.find((b) => b.id === node.bucket)!;
            const count = [...node.files()].length;
            const item = new vscode.TreeItem(
                meta.label,
                count > 0
                    ? vscode.TreeItemCollapsibleState.Expanded
                    : vscode.TreeItemCollapsibleState.None
            );
            item.description = String(count);
            item.iconPath = new vscode.ThemeIcon(meta.icon);
            item.tooltip = meta.tooltip;
            item.contextValue = `bucket-${node.bucket}`;
            return item;
        }

        const collapsible =
            node.kind === "dir"
                ? vscode.TreeItemCollapsibleState.Collapsed
                : vscode.TreeItemCollapsibleState.None;
        const item = new vscode.TreeItem(node.label, collapsible);
        if (node.kind === "file") {
            item.resourceUri = vscode.Uri.file(path.join(this.workspaceRoot, node.relPath));
            item.iconPath = vscode.ThemeIcon.File;
            item.tooltip = node.relPath;
            item.contextValue = `file-${node.bucket}`;
            // Orphaned files no longer exist on disk — nothing to open.
            if (node.bucket !== "orphaned") {
                item.command = {
                    command: "vscode.open",
                    title: "Open",
                    arguments: [item.resourceUri],
                };
            }
        } else {
            item.iconPath = vscode.ThemeIcon.Folder;
        }
        if (SELECTABLE.has(node.bucket)) {
            const checked =
                node.kind === "file"
                    ? this.selected[node.bucket].has(node.relPath)
                    : [...node.files()].every((f) => this.selected[node.bucket].has(f.relPath));
            item.checkboxState = checked
                ? vscode.TreeItemCheckboxState.Checked
                : vscode.TreeItemCheckboxState.Unchecked;
        }
        return item;
    }
}

function emptySelection(): Record<Bucket, Set<string>> {
    return { stale: new Set(), missing: new Set(), orphaned: new Set(), indexing: new Set() };
}

/** Builds the bucket → (compacted) directory → file hierarchy from a flat path list. */
function buildBucketTree(bucket: Bucket, paths: string[]): DriftNode {
    interface Dir {
        dirs: Map<string, Dir>;
        files: string[]; // full rel paths
    }
    const rootDir: Dir = { dirs: new Map(), files: [] };
    for (const p of [...paths].sort()) {
        const segments = p.split("/");
        let cur = rootDir;
        for (const seg of segments.slice(0, -1)) {
            let next = cur.dirs.get(seg);
            if (next === undefined) {
                next = { dirs: new Map(), files: [] };
                cur.dirs.set(seg, next);
            }
            cur = next;
        }
        cur.files.push(p);
    }

    function toNodes(dir: Dir, prefix: string): DriftNode[] {
        const nodes: DriftNode[] = [];
        for (const [name, sub] of dir.dirs) {
            // Compact chains of single-child directories (a/b/c) like VS Code's explorer.
            let label = name;
            let cursor = sub;
            while (cursor.files.length === 0 && cursor.dirs.size === 1) {
                const [nextName, nextDir] = [...cursor.dirs][0];
                label = `${label}/${nextName}`;
                cursor = nextDir;
            }
            const rel = prefix === "" ? label : `${prefix}/${label}`;
            nodes.push(new DriftNode("dir", bucket, rel, label, toNodes(cursor, rel)));
        }
        for (const f of dir.files) {
            nodes.push(new DriftNode("file", bucket, f, f.split("/").pop()!, []));
        }
        return nodes;
    }

    return new DriftNode("bucket", bucket, "", bucket, toNodes(rootDir, ""));
}
