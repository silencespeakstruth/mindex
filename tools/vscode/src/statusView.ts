import * as vscode from "vscode";
import { FileEntry, HealthResponse, MindexApi, StatusResponse } from "./api";
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
        private readonly statusBar: vscode.StatusBarItem
    ) {}

    /** Fetches /health, /status and the failed-file list, then redraws. Never throws. */
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
                    icon: new vscode.ThemeIcon("error", new vscode.ThemeColor("errorForeground")),
                    tooltip: `${detail}\nCheck mindex.serverUrl / mindex.noVerify and that the server is running.`,
                    children: [],
                },
            ];
            this.setStatusBar("unreachable");
            this.changed.fire(undefined);
            return;
        }

        const healthNode: StatusNode = {
            label: `Health: ${health.status}`,
            description: `v${health.version}`,
            icon: new vscode.ThemeIcon(health.status === "ok" ? "pass" : "warning"),
            children: Object.entries(health.checks).map(([name, state]) =>
                leaf(name, state, state === "ok" ? "pass" : "error")
            ),
        };
        this.setStatusBar(health.status);

        const nodes: StatusNode[] = [healthNode];

        // /status and the failed list are best-effort detail — health already rendered.
        try {
            const status = await this.api().status();
            nodes.push(runtimeNode(status));
        } catch {
            nodes.push(leaf("Runtime", "unavailable", "warning"));
        }

        const guid = this.guid();
        if (guid !== undefined) {
            try {
                const failed = (await this.api().listFiles(guid, { status: "failed" })).files;
                nodes.push(failedNode(failed));
            } catch {
                nodes.push(leaf("Failed files", "unavailable", "warning"));
            }
        }

        this.roots = nodes;
        this.changed.fire(undefined);
    }

    private setStatusBar(state: "ok" | "degraded" | "unreachable"): void {
        const icons = { ok: "$(database)", degraded: "$(warning)", unreachable: "$(error)" };
        this.statusBar.text = `${icons[state]} mindex: ${state}`;
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

function runtimeNode(s: StatusResponse): StatusNode {
    const byStatus = Object.entries(s.files_by_status)
        .filter(([, n]) => n > 0)
        .map(([k, n]) => `${k}: ${n}`)
        .join(", ");
    return {
        label: "Runtime",
        icon: new vscode.ThemeIcon("pulse"),
        children: [
            leaf("indexing files", String(s.indexing_files)),
            leaf("indexing claims", String(s.indexing_claims)),
            leaf("GC running", String(s.gc_running)),
            leaf("SQLite pool", `${s.pool_available}/${s.pool_size} available`),
            leaf("files by status", byStatus === "" ? "none" : byStatus),
        ],
    };
}

function failedNode(failed: FileEntry[]): StatusNode {
    return {
        label: "Failed files",
        description: String(failed.length),
        icon: new vscode.ThemeIcon(failed.length > 0 ? "flame" : "pass"),
        tooltip:
            failed.length > 0
                ? "Files whose indexing failed. Retry requeues them for the retry worker (~60 s)."
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
