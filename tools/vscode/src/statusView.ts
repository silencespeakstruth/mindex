import * as vscode from "vscode";
import {
    ConfigResponse,
    FileEntry,
    HealthResponse,
    LanguageStats,
    MindexApi,
    StatusResponse,
} from "./api";
import { UnreachableError } from "./errors";

interface StatusNode {
    label: string;
    description?: string;
    icon?: vscode.ThemeIcon;
    tooltip?: string;
    contextValue?: string;
    /** Set on failed-file leaves so the retry command knows the path. */
    filePath?: string;
    children: StatusNode[];
}

function leaf(label: string, description?: string, icon?: string): StatusNode {
    return {
        label,
        description,
        icon: icon ? new vscode.ThemeIcon(icon) : undefined,
        children: [],
    };
}

export class StatusTreeProvider implements vscode.TreeDataProvider<StatusNode> {
    private readonly changed = new vscode.EventEmitter<StatusNode | undefined>();
    readonly onDidChangeTreeData = this.changed.event;

    private roots: StatusNode[] = [];

    constructor(
        private readonly api: () => MindexApi,
        private readonly guid: () => string | undefined,
        private readonly statusBar: vscode.StatusBarItem,
        /**
         * Told after every refresh whether `/research` can work at all — the server's
         * Ollama is an optional dependency, so its state is invisible in `status`
         * and has to be forwarded to the surface that offers Research.
         */
        private readonly onResearchAvailability: (available: boolean) => void = () => {},
        /**
         * The project's live language inventory — the languages a search can
         * actually match something in. `undefined` means *unknown* (server
         * unreachable, no project, a 404, or a server too old to publish it), which
         * a consumer must treat differently from `[]`: unknown falls back to the
         * full supported list, empty means the index really holds nothing.
         */
        private readonly onInventory: (languages: string[] | undefined) => void = () => {},
        /**
         * `GET /config`, re-read on every refresh rather than once at activation:
         * `research.models` is refreshed server-side, so a model pulled after the
         * window opened must appear without a reload. Fetching it here and not at
         * each call site is the point — a future refresh site cannot forget it.
         *
         * Called only when there *is* a config: a failed read leaves the consumer's
         * last known one standing, since stale budgets beat unlabelled ones.
         */
        private readonly onServerConfig: (config: ConfigResponse) => void = () => {}
    ) {}

    /**
     * Fetches /health, /status, /config, the project's stats and its failed-file
     * list, then redraws. Never throws.
     *
     * This is also where the two *inventories* the Ask view renders come from — the
     * project's languages and the server's model list — because this method is
     * already called from everywhere they can change: activation, the explicit
     * refresh command, a `.mindex` edit, and every reindex/delete.
     */
    async refresh(): Promise<void> {
        const api = this.api();
        let health: HealthResponse;
        try {
            health = await api.health();
        } catch (e) {
            const detail = e instanceof UnreachableError ? e.cause_.message : String(e);
            this.roots = [
                {
                    label: "Server unreachable",
                    description: detail,
                    icon: new vscode.ThemeIcon(
                        "error",
                        new vscode.ThemeColor("errorForeground")
                    ),
                    tooltip: `${detail}\nCheck mindex.serverUrl / mindex.noVerify and that the server is running.`,
                    children: [],
                },
            ];
            this.setStatusBar("unreachable");
            this.onResearchAvailability(false);
            // Unknown, not empty: an unreachable server must leave the pickers at
            // their last known contents rather than blanking them. The config is
            // simply not re-pushed, for the same reason.
            this.onInventory(undefined);
            this.changed.fire(undefined);
            return;
        }

        // Ollama is the server's *optional* dependency: `status` stays "ok" without
        // it, and only Research stops working. Older servers omit the check entirely
        // — absent is not down, so it must not read as a failure anywhere.
        const ollama = health.checks.ollama;
        const researchAvailable = ollama === undefined || ollama === "ok";

        const healthNode: StatusNode = {
            label: `Health: ${health.status}`,
            description: `v${health.version}`,
            icon: new vscode.ThemeIcon(health.status === "ok" ? "pass" : "warning"),
            children: Object.entries(health.checks).map(([name, state]) =>
                name === "ollama"
                    ? ollamaLeaf(state ?? "unknown")
                    : dependencyLeaf(name, state ?? "unknown")
            ),
        };
        this.setStatusBar(health.status, researchAvailable);
        this.onResearchAvailability(researchAvailable);

        const nodes: StatusNode[] = [healthNode];

        // /status and the failed list are best-effort detail — health already rendered.
        try {
            const status = await this.api().status();
            nodes.push(runtimeNode(status));
        } catch {
            nodes.push(leaf("Runtime", "unavailable", "warning"));
        }

        try {
            this.onServerConfig(await this.api().config());
        } catch {
            // Decoration on the way in; the server validates on the way out.
        }

        const guid = this.guid();
        if (guid !== undefined) {
            try {
                const stats = await this.api().projectStats(guid);
                const languages = stats.languages;
                if (languages === undefined) {
                    this.onInventory(undefined); // a server too old to publish one
                } else {
                    nodes.push(inventoryNode(languages));
                    // Only languages with live chunks: a filter on a language whose
                    // files all failed (or sliced to nothing) returns a 404 that
                    // reads to the user as "your query matched nothing".
                    this.onInventory(
                        Object.entries(languages)
                            .filter(([, v]) => v.chunks_active > 0)
                            .map(([name]) => name)
                            .sort()
                    );
                }
            } catch {
                // Includes the 404 of a project that has never been indexed — which
                // is unknown, not empty.
                this.onInventory(undefined);
                nodes.push(leaf("Indexed", "unavailable", "warning"));
            }

            try {
                const failed = (await this.api().listFiles(guid, { status: "failed" })).files;
                nodes.push(failedNode(failed));
            } catch {
                nodes.push(leaf("Failed files", "unavailable", "warning"));
            }
        } else {
            this.onInventory(undefined);
        }

        this.roots = nodes;
        this.changed.fire(undefined);
    }

    private setStatusBar(
        state: "ok" | "degraded" | "unreachable",
        researchAvailable = true
    ): void {
        const icons = { ok: "$(database)", degraded: "$(warning)", unreachable: "$(error)" };
        // A dead Ollama is *not* a degraded server, so it never changes the state or
        // the background — it only annotates what is unavailable.
        const suffix = researchAvailable ? "" : " $(circle-slash) research";
        this.statusBar.text = `${icons[state]} mindex: ${state}${suffix}`;
        this.statusBar.tooltip = researchAvailable
            ? "mindex server health — click to refresh"
            : "mindex server health — click to refresh\nOllama is down: indexing and search work, Research does not.";
        this.statusBar.backgroundColor =
            state === "ok"
                ? undefined
                : new vscode.ThemeColor(
                      state === "degraded"
                          ? "statusBarItem.warningBackground"
                          : "statusBarItem.errorBackground"
                  );
        this.statusBar.show();
    }

    getChildren(element?: StatusNode): StatusNode[] {
        return element === undefined ? this.roots : element.children;
    }

    getTreeItem(node: StatusNode): vscode.TreeItem {
        const item = new vscode.TreeItem(
            node.label,
            node.children.length > 0
                ? vscode.TreeItemCollapsibleState.Expanded
                : vscode.TreeItemCollapsibleState.None
        );
        item.description = node.description;
        item.iconPath = node.icon;
        item.tooltip = node.tooltip;
        item.contextValue = node.contextValue;
        return item;
    }
}

function dependencyLeaf(name: string, state: string): StatusNode {
    return leaf(name, state, state === "ok" ? "pass" : "error");
}

/** The optional dependency: a failure is a warning, and it says what it costs. */
function ollamaLeaf(state: string): StatusNode {
    const down = state !== "ok";
    return {
        ...leaf("ollama", state, down ? "warning" : "pass"),
        tooltip: down
            ? `${state}\nOptional dependency — only Research needs it. Indexing and ` +
              `search are unaffected, which is why Health stays "ok".`
            : "The local model behind Research (optional dependency).",
    };
}

/**
 * Live server state. The file *counts* `/status` also carries are deliberately not
 * shown: they sum every project the server has ever indexed, which says nothing
 * about this workspace and reads as a contradiction next to the per-project Failed
 * list (a server-wide `failed: 160` beside `Failed files: 0`). What is left is
 * genuinely about the server, and has no per-project meaning to be confused with.
 */
function runtimeNode(s: StatusResponse): StatusNode {
    return {
        label: "Runtime",
        icon: new vscode.ThemeIcon("pulse"),
        children: [
            leaf("indexing claims", String(s.indexing_claims)),
            leaf("GC running", String(s.gc_running)),
            leaf("SQLite pool", `${s.pool_available}/${s.pool_size} available`),
        ],
    };
}

/**
 * What this project actually contains, per language — the same inventory that
 * decides which languages the Ask view offers, made visible so the filter list is
 * not a silent decision.
 *
 * A language with files but no live chunks is called out rather than hidden: it *is*
 * indexed, and searching it will still find nothing, which is worth one word of
 * explanation instead of a mystery.
 */
function inventoryNode(languages: Record<string, LanguageStats>): StatusNode {
    const rows = Object.entries(languages).sort(([a], [b]) => a.localeCompare(b));
    const files = rows.reduce((n, [, v]) => n + v.files, 0);
    const active = rows.reduce((n, [, v]) => n + v.chunks_active, 0);
    return {
        label: "Indexed",
        description: `${rows.length} languages · ${files} files · ${active} chunks`,
        icon: new vscode.ThemeIcon("library"),
        tooltip:
            "What this project holds. Only languages with searchable chunks are " +
            "offered as filters in the Ask view.",
        children: rows.map(([name, v]) => ({
            ...leaf(
                name,
                `${v.files} files · ${v.chunks_active} chunks`,
                v.chunks_active > 0 ? "symbol-file" : "warning"
            ),
            tooltip:
                v.chunks_active > 0
                    ? `${v.indexed_files} of ${v.files} files indexed` +
                      (v.chunks_deleted > 0
                          ? `\n${v.chunks_deleted} chunks soft-deleted, awaiting GC`
                          : "")
                    : "Indexed but unsearchable: every file either failed or was too " +
                      "short to produce a chunk. Not offered as a filter.",
        })),
    };
}

/** This project's dead letters — the only files Retry acts on. */
function failedNode(failed: FileEntry[]): StatusNode {
    return {
        label: "Failed files",
        description: String(failed.length),
        icon: new vscode.ThemeIcon(failed.length > 0 ? "flame" : "pass"),
        tooltip:
            failed.length > 0
                ? "Files in this project whose indexing failed. Retry requeues them for " +
                  "the retry worker (~60 s)."
                : undefined,
        children: failed.map((f) => ({
            label: f.path,
            description: `retries: ${f.retry_count}`,
            icon: new vscode.ThemeIcon("error"),
            tooltip: `${f.programming_language}, last change ${new Date(
                f.status_updated_at * 1000
            ).toLocaleString()}`,
            contextValue: "failedFile",
            filePath: f.path,
            children: [],
        })),
    };
}

export function failedFilePath(node: unknown): string | undefined {
    return node !== null && typeof node === "object" && "filePath" in node
        ? (node as StatusNode).filePath
        : undefined;
}
