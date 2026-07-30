/**
 * The bits of the webview runtime that are not the DOM.
 *
 * `acquireVsCodeApi` is injected by VS Code and has no ambient declaration outside
 * `@types/vscode-webview`, which this extension does not depend on for one function.
 */

import { langGlyph } from "../shared/langIcons.js";

export interface VsCodeApi<S> {
    postMessage(message: unknown): void;
    getState(): S | undefined;
    setState(state: S): void;
}

declare function acquireVsCodeApi<S>(): VsCodeApi<S>;

/**
 * Must be called at most once per page — a second call throws. Every module that
 * needs it takes the handle as an argument rather than calling this itself.
 */
export function vscodeApi<S>(): VsCodeApi<S> {
    return acquireVsCodeApi<S>();
}

/**
 * The page's data block: everything dynamic the host wanted to hand the script.
 *
 * Absent block or unparseable JSON returns `undefined` rather than throwing, because
 * a page that renders without its data is recoverable and a page whose script died on
 * line one is a blank panel with nothing in the console the user will ever see.
 */
export function pageData<T>(): T | undefined {
    const node = document.getElementById("page-data");
    if (node === null || node.textContent === null) {
        return undefined;
    }
    try {
        return JSON.parse(node.textContent) as T;
    } catch {
        return undefined;
    }
}

/** `document.getElementById`, typed and loud about a missing id. */
export function el<T extends HTMLElement = HTMLElement>(id: string): T {
    const node = document.getElementById(id);
    if (node === null) {
        throw new Error(`missing element #${id}`);
    }
    return node as T;
}

/** A codicon `<span>`, so the glyph markup is written once. */
export function icon(name: string, small = false): HTMLSpanElement {
    const span = document.createElement("span");
    span.className = `codicon codicon-${name}${small ? " codicon-sm" : ""}`;
    span.setAttribute("aria-hidden", "true");
    return span;
}

const svgParser = new DOMParser();

/**
 * A language's official mark, coloured by `media/lang.css`.
 *
 * Parsed as XML rather than assigned to `innerHTML`. Both would be *safe* — the markup
 * is a build-time constant vendored from devicon by `esbuild.mjs`, reached only by a
 * language-id lookup, so nothing the server or the user says is ever interpolated —
 * but only one of them is reliable. `innerHTML` runs the **HTML** fragment parser,
 * which resolves an `<svg>` subtree through its foreign-content path and is the sort
 * of thing a webview host can also intercept; `parseFromString(…, "image/svg+xml")`
 * puts every node in the SVG namespace by construction. An icon that fails to parse
 * has no error to show for it — it is simply absent — so the mechanism with no
 * ambiguity in it is worth the four extra lines.
 */
export function langIcon(lang: string): HTMLSpanElement {
    const glyph = langGlyph(lang);
    const span = document.createElement("span");
    span.setAttribute("aria-hidden", "true");
    if (glyph.kind === "codicon") {
        span.className = `lang-glyph codicon codicon-${glyph.codicon} ${glyph.tone}`;
        return span;
    }
    span.className = `lang-glyph ${glyph.tone}`;
    const root = svgParser.parseFromString(glyph.svg, "image/svg+xml").documentElement;
    // A parse failure yields a `<parsererror>` document rather than throwing. Appending
    // it would put its message in the middle of a table, so drop it and leave the box
    // empty — the language's name is beside it either way.
    if (root.nodeName.toLowerCase() === "svg") {
        span.appendChild(document.importNode(root, true));
    }
    return span;
}
