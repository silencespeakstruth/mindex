import * as fs from "node:fs";
import * as vscode from "vscode";

/**
 * The one place a webview page is assembled.
 *
 * Every webview in this extension used to build its own `<!DOCTYPE html>` string with
 * its own CSP, its own nonce generator and its own copy of `escapeHtml` — two copies
 * that had already drifted (one allowed `'unsafe-inline'` styles, the other did not;
 * one used a 24-character nonce, the other 32). A third webview would have been a
 * third copy, so the shape is centralised before the status panel is written rather
 * than after.
 *
 * The rule this module enforces is that **markup carries no data**. Page bodies are
 * static files under `media/`, and everything dynamic travels in one JSON block the
 * script reads — which is why `escapeHtml` has no callers on the page-building path
 * any more and why the CSP can drop `'unsafe-inline'` entirely.
 */

/** Where `renderPage` may load assets from, beyond `media/`. */
export interface PageOptions {
    /** Static markup, read from `media/` by [`readMedia`]. */
    body: string;
    /** `media/`-relative stylesheet paths, in cascade order. */
    styles: string[];
    /** `media/`-relative ES-module entry points. */
    modules: string[];
    /**
     * Serialised into `<script type="application/json" id="page-data">`. The script
     * reads it with `JSON.parse(document.getElementById("page-data").textContent)`.
     *
     * `JSON.stringify` output is escaped for `<` so a string value containing
     * `</script>` cannot close the block early — the one injection vector a data
     * block still has.
     */
    data?: unknown;
    /** Load the codicon font + stylesheet. */
    codicons?: boolean;
}

/**
 * The one root a webview may load from.
 *
 * It is a single directory because the build puts everything a page needs inside it:
 * scripts are bundled by esbuild, and the codicon font and stylesheet are copied in.
 * Nothing resolves out of `node_modules` any more — a path listed there would have to
 * agree with `.vscodeignore`, this list and the CSP all at once, and when it does not,
 * the asset fails to load **silently**: a blank panel with no error anywhere.
 */
export function mediaRoots(extensionUri: vscode.Uri): vscode.Uri[] {
    return [vscode.Uri.joinPath(extensionUri, "media")];
}

const mediaCache = new Map<string, string>();

/**
 * Read a file from `media/`, cached for the life of the process.
 *
 * Synchronous on purpose: `WebviewViewProvider.resolveWebviewView` is called on the
 * sidebar's critical path, and a few kilobytes of local HTML is not worth making it
 * async — an awaited resolve shows an empty view until the promise settles.
 */
export function readMedia(extensionUri: vscode.Uri, name: string): string {
    const cached = mediaCache.get(name);
    if (cached !== undefined) {
        return cached;
    }
    const text = fs.readFileSync(
        vscode.Uri.joinPath(extensionUri, "media", name).fsPath,
        "utf8"
    );
    mediaCache.set(name, text);
    return text;
}

export function renderPage(
    webview: vscode.Webview,
    extensionUri: vscode.Uri,
    o: PageOptions
): string {
    const nonce = makeNonce();
    const src = webview.cspSource;
    const media = (name: string): string =>
        webview.asWebviewUri(vscode.Uri.joinPath(extensionUri, "media", name)).toString();

    const styles = [...o.styles.map(media)];
    if (o.codicons === true) {
        styles.unshift(media("codicons/codicon.css"));
    }

    // `script-src` keeps `${src}` alongside the nonce so a module may still fetch a
    // sibling: a nonce does not propagate to a module's *imports*, the browser fetches
    // those itself. Everything `${src}` authorises is inside `media/`.
    const csp = [
        "default-src 'none'",
        `img-src ${src} data:`,
        `font-src ${src}`,
        `style-src ${src}`,
        `script-src 'nonce-${nonce}' ${src}`,
    ].join("; ");

    const dataBlock =
        o.data === undefined
            ? ""
            : `<script type="application/json" id="page-data" nonce="${nonce}">${JSON.stringify(
                  o.data
              ).replace(/</g, "\\u003c")}</script>`;

    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<meta http-equiv="Content-Security-Policy" content="${csp}">
${styles.map((href) => `<link rel="stylesheet" href="${href}">`).join("\n")}
</head>
<body>
${o.body}
${dataBlock}
${o.modules
    .map((m) => `<script type="module" nonce="${nonce}" src="${media(m)}"></script>`)
    .join("\n")}
</body>
</html>`;
}

/**
 * A per-page nonce. 32 characters of alphanumerics — enough that guessing it is not
 * a route past the CSP, which is the only thing it is for.
 */
export function makeNonce(): string {
    const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let out = "";
    for (let i = 0; i < 32; i++) {
        out += chars.charAt(Math.floor(Math.random() * chars.length));
    }
    return out;
}

/**
 * HTML-escape a string.
 *
 * Kept for the few places that still build markup in TypeScript. New code should put
 * the value in `PageOptions.data` and let the page script set `textContent`, which
 * cannot be got wrong.
 */
export function escapeHtml(s: string): string {
    return s.replace(
        /[&<>"']/g,
        (c) =>
            ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
                c
            ] as string
    );
}

/** Narrow an untrusted `postMessage` field to a string. */
export function asString(v: unknown, fallback = ""): string {
    return typeof v === "string" ? v : fallback;
}
