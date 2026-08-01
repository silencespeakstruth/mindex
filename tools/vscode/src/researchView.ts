import * as vscode from "vscode";
import {
    ResearchBudget,
    ResearchCitations,
    ResearchDone,
    ResearchEffort,
    ResearchExcerpts,
    ResearchProgress,
    ResearchStep,
    SearchFilter,
} from "./api";
import { say } from "./brand";
import { describeScope, Scope } from "./scope";
import { asString, mediaRoots, readMedia, renderPage } from "./webview";

/** What the sidebar form submits. */
export interface ResearchSubmission {
    question: string;
    effort: ResearchEffort;
    model: string;
    /** Only the axes the user filled in; absent ones keep the effort preset. */
    budget?: ResearchBudget;
    /**
     * The files the run may see. Enforced server-side on every lookup, so it bounds
     * the answer and not just the ranking — which is why the panel renders it: a
     * scoped report and an unscoped one are otherwise the same document, and only one
     * of them is entitled to say "nowhere in this project".
     */
    include?: SearchFilter;
    exclude?: SearchFilter;
    /** Stored runs handed to the model as background; picked in Research History. */
    contextRunIds?: string[];
}

/**
 * The streaming output tab: a webview panel showing the step feed (with the
 * model's live thinking under the current step, collapsed once the step lands)
 * and the incrementally rendered Markdown report.
 *
 * One panel per run, never reused. A finished report is a document you keep —
 * reusing the tab would silently destroy the previous answer the moment you asked
 * the next question, and two runs are rarely about the same thing. The tab title
 * carries a slug of the question so a row of them stays navigable.
 */
export class ResearchPanel {
    private panel: vscode.WebviewPanel;
    private disposed = false;

    constructor(
        extensionUri: vscode.Uri,
        question: string,
        readonly onDispose: () => void,
        /**
         * The scope the run was given, rendered in the header. Without it a scoped
         * report is indistinguishable from an unscoped one after the fact — and a
         * report that could only see `docs/**` saying "this is not in the project" is
         * misleading unless the reader can see why.
         */
        scope?: Scope,
        /**
         * The stored reports handed to this run as background, rendered as
         * clickable chips in the header.
         *
         * A report built on other reports inherits their claims, and afterwards the
         * document says nothing about whose. Showing the chain — and letting each
         * link open — is what makes a chained run auditable instead of a fact with
         * no visible provenance.
         */
        context: readonly { id: string; seq: number; title: string }[] = []
    ) {
        this.panel = vscode.window.createWebviewPanel(
            "mindexResearch",
            titleFor(question),
            { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
            {
                enableScripts: true,
                retainContextWhenHidden: true,
                localResourceRoots: mediaRoots(extensionUri),
            }
        );
        this.panel.onDidDispose(() => {
            this.disposed = true;
            onDispose();
        });
        this.panel.webview.html = renderPage(this.panel.webview, extensionUri, {
            body: readMedia(extensionUri, "research.html"),
            styles: ["common.css", "research.css"],
            // `marked` is bundled into the module now, so there is no second script
            // and no `node_modules` root to authorise.
            modules: ["js/research.js"],
            codicons: true,
            data: {
                question,
                scope: describeScope(scope),
                context: context.map((r) => ({ id: r.id, seq: r.seq, title: r.title })),
            },
        });
        this.panel.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
            if (msg.type === "openRun") {
                void vscode.commands.executeCommand("mindex.openResearchReport", {
                    id: asString(msg.id),
                    seq: Number(msg.seq),
                    title: asString(msg.title),
                });
                return;
            }
            if (msg.type === "copy") {
                void vscode.env.clipboard
                    .writeText(asString(msg.text))
                    .then(() =>
                        vscode.window.showInformationMessage(say("report copied as Markdown."))
                    );
            }
        });
    }

    get isDisposed(): boolean {
        return this.disposed;
    }

    reveal(): void {
        this.panel.reveal(undefined, true);
    }

    thinking(text: string): void {
        this.post({ type: "thinking", text });
    }
    step(step: ResearchStep): void {
        this.post({ type: "step", step });
    }
    progress(progress: ResearchProgress): void {
        this.post({ type: "progress", progress });
    }
    summary(text: string): void {
        this.post({ type: "summary", text });
    }
    citations(citations: ResearchCitations): void {
        this.post({ type: "citations", citations });
    }
    excerpts(excerpts: ResearchExcerpts): void {
        this.post({ type: "excerpts", excerpts });
    }
    done(info: ResearchDone): void {
        this.post({ type: "done", info });
    }
    /** `code` lets the view decide whether the streamed summary is salvageable. */
    error(detail: string, code?: string): void {
        this.post({ type: "error", detail, code });
    }
    cancelled(): void {
        this.post({ type: "cancelled" });
    }

    private post(msg: unknown): void {
        if (!this.disposed) {
            void this.panel.webview.postMessage(msg);
        }
    }
}

/** A tab title short enough to survive VS Code's truncation but still tell runs apart. */
function titleFor(question: string): string {
    const flat = question.replace(/\s+/g, " ").trim();
    const slug = flat.length > 34 ? `${flat.slice(0, 33)}…` : flat;
    return slug === "" ? "Research" : `Research: ${slug}`;
}
