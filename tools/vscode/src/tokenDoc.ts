import * as vscode from "vscode";

/**
 * URI scheme for a freshly issued token shown in a tab.
 *
 * WHY A DOCUMENT AT ALL. The clipboard is where a minted token is going in the
 * ordinary case, and `agentToken.ts` explains why that is the default. It stops
 * being enough the moment the token has to be *edited into* something by hand — an
 * MCP server list in an editor's own JSON, a config on another machine, a line read
 * out loud. None of that is servable by a clipboard the next copy overwrites.
 *
 * WHY NOT AN UNTITLED BUFFER, which is the one-line version of this file. An
 * untitled buffer is one absent-minded `Ctrl+S` away from a credential on disk, in
 * whatever directory happened to be open, surviving the token's own expiry. That is
 * precisely the "a file is a copy nobody decided to keep" that `agentToken.ts`
 * rejects, and it would be reintroduced by accident rather than by decision.
 *
 * A `TextDocumentContentProvider` cannot be saved to disk by the user at all, and
 * the content lives in this module's map and nowhere else — so "shown once and
 * stored nowhere" stays literally true. After a window reload the map is empty and
 * the provider says so, which is the honest rendering of the same promise: VS Code
 * restores the *tab*, and there is nothing left to put in it.
 */
export const TOKEN_SCHEME = "mindex-token";

/**
 * The tokens currently on screen, keyed by the nonce in their URI.
 *
 * Module-level rather than a field, because `provideTextDocumentContent` is called
 * again on every reveal of a restored tab and must answer from the URI alone. Not
 * persisted, not written anywhere, and dropped when the document closes.
 */
const held = new Map<string, string>();

/** What a token is served with, so a reader knows what they are holding. */
export interface TokenFacts {
    actions: readonly string[];
    projects: readonly string[];
    expiresAt?: number | null;
}

function body(token: string, facts: TokenFacts): string {
    const until =
        facts.expiresAt === undefined || facts.expiresAt === null
            ? "never — this token does not expire"
            : new Date(facts.expiresAt * 1000).toLocaleString();
    // The token alone on line 1, so select-line and copy yield it clean with no
    // trimming. Everything else is below it and commented, because the commonest
    // destination for this text is a JSON or shell config where a stray word is a
    // syntax error and a `#` is at worst inert.
    return [
        token,
        "",
        `# Issued for: ${facts.projects.join(", ")}`,
        `# May: ${facts.actions.join(", ")}`,
        `# Expires: ${until}`,
        "#",
        "# This is the only copy. It is held in memory until this tab closes and is",
        "# written to no file, no setting and no keychain. Reloading the window",
        "# loses it — issue another rather than looking for it.",
        "",
    ].join("\n");
}

/**
 * Serves issued tokens, read-only, from memory.
 *
 * Registered once for the window, beside the research one.
 */
export class TokenDocumentProvider implements vscode.TextDocumentContentProvider {
    provideTextDocumentContent(uri: vscode.Uri): string {
        return (
            held.get(uri.path) ??
            "# This token is no longer held.\n#\n" +
                "# It was shown once and stored nowhere, so there is nothing left to\n" +
                "# display here. Issue another from the Ask view or Server Status.\n"
        );
    }

    dispose(): void {
        held.clear();
    }
}

/**
 * Open a freshly issued token in its own read-only tab.
 *
 * The nonce makes two tokens shown in one session two documents rather than one
 * that silently changes under the reader. It is not a secret — the secret is the
 * value the map holds against it — but it is unguessable enough that a second
 * extension cannot open somebody else's tab by constructing the URI.
 */
export async function showIssuedToken(token: string, facts: TokenFacts): Promise<void> {
    const nonce = `/${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
    held.set(nonce, body(token, facts));
    const uri = vscode.Uri.from({ scheme: TOKEN_SCHEME, path: nonce });
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc, { preview: true });
}

/**
 * Forget a token whose tab has closed.
 *
 * Wired to `onDidCloseTextDocument` rather than to the tab's disposal because a
 * document is what the provider is keyed on; closing the tab is the user saying
 * they are done with it, which is the only signal available that does not depend
 * on the token's own expiry.
 */
export function forgetIfToken(doc: vscode.TextDocument): void {
    if (doc.uri.scheme === TOKEN_SCHEME) {
        held.delete(doc.uri.path);
    }
}
