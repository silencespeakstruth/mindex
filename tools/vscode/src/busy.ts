/**
 * Single-flight per key, and the wire that tells a webview about it.
 *
 * Two things a UI that talks to a server has to get right, and this is both of
 * them in one place:
 *
 * **The refusal is here, not in the webview.** A disabled button is a courtesy,
 * not a guarantee — a panel restored from a hidden state, a keyboard-driven
 * double activation, a message already in flight when the disable was painted,
 * or simply a bug can all still post. So every write asks this class for
 * permission, and the greyed-out button is the *echo* of that decision rather
 * than its cause. Getting this backwards is why double-clicking Delete opened
 * two confirmation modals for the same rows.
 *
 * **Refuse, do not supersede.** Aborting the in-flight call and starting a new
 * one is right for a keystroke-driven list load, where the newer query is
 * strictly the wanted one, and wrong for everything else: a superseded page
 * fetch loses the cursor it was going to advance, and a superseded delete is
 * still a delete. Reads that genuinely want supersede keep their own
 * `AbortController` and use a key only so the button can echo it.
 *
 * `vscode`-free on purpose, which is what makes it reachable from `node --test`.
 */
export class BusyKeys {
    private readonly held = new Set<string>();

    constructor(private readonly post: (message: unknown) => void) {}

    isBusy(key: string): boolean {
        return this.held.has(key);
    }

    /**
     * Run `fn` unless `key` is already held. Returns `undefined` when refused —
     * which a caller may almost always ignore, because a refusal means the work
     * is already happening.
     *
     * A refusal deliberately posts **nothing**: a `busy:false` from the second
     * press would re-enable the button while the first call is still running,
     * which is the exact state this exists to prevent.
     */
    async run<T>(key: string, fn: () => Promise<T>): Promise<T | undefined> {
        if (this.held.has(key)) {
            return undefined;
        }
        this.held.add(key);
        this.post({ type: "busy", key, busy: true });
        try {
            return await fn();
        } finally {
            this.held.delete(key);
            this.post({ type: "busy", key, busy: false });
        }
    }

    /**
     * Release every key. For `onDidDispose`: a webview that dies mid-call would
     * otherwise leave a key held forever on a panel the user then reopens.
     */
    reset(): void {
        this.held.clear();
    }
}
