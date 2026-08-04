import { AskMode } from "./shared/askFields";

/**
 * What the host knows about the Ask form that the form itself cannot remember.
 *
 * The sidebar is a `WebviewView` registered without `retainContextWhenHidden`, so
 * collapsing it — or switching to another view container — destroys the page, and
 * coming back rebuilds one from the default HTML. Availability, the server config
 * and the context chips were already replayed into that new page. Two things were
 * not: whether a research run is in flight, and which busy keys the host is
 * holding.
 *
 * The consequence was the sharpest dead end in the extension. Start a run, collapse
 * the sidebar, come back: Submit is enabled and Stop is *hidden*, because the page
 * has never heard of the run. Pressing Submit earns a toast saying to cancel the
 * run first, and the only control that could cancel it is the one that is hidden.
 *
 * So the state that outlives the page lives here, beside the pending mode that
 * already did. `vscode`-free on purpose, like `busy.ts` and `token.ts`: that is
 * what makes it reachable from `node --test`, and a replay is precisely the kind of
 * logic no manual pass re-checks once it has worked once.
 */
export class AskFormState {
    /** A mode requested before the view existed; applied once it resolves. */
    private pendingMode?: AskMode;
    private isRunning = false;
    private readonly heldKeys = new Set<string>();

    /** Remember a mode to apply when the view next resolves. */
    requestMode(mode: AskMode): void {
        this.pendingMode = mode;
    }

    /**
     * The mode a resolving view should be switched to, consumed in the reading.
     *
     * Consumed rather than kept because it is a request and not a state: replaying
     * it a second time would drag the user back out of a tab they had since
     * switched to by hand.
     */
    takePendingMode(): AskMode | undefined {
        const mode = this.pendingMode;
        this.pendingMode = undefined;
        return mode;
    }

    setRunning(running: boolean): void {
        this.isRunning = running;
    }

    get running(): boolean {
        return this.isRunning;
    }

    setBusy(key: string, busy: boolean): void {
        if (busy) {
            this.heldKeys.add(key);
        } else {
            this.heldKeys.delete(key);
        }
    }

    /** Which keys the host is currently refusing. Test seam, and the replay's input. */
    get held(): readonly string[] {
        return [...this.heldKeys];
    }

    /**
     * The messages that bring a freshly built page up to date.
     *
     * Only states that differ from a default page are emitted: a `running: false`
     * or a released key would be a no-op the webview still has to route, and the
     * empty array is the honest answer for the ordinary case where nothing was
     * happening when the sidebar was reopened.
     */
    replay(): Array<Record<string, unknown>> {
        const messages: Array<Record<string, unknown>> = [];
        const mode = this.takePendingMode();
        if (mode !== undefined) {
            messages.push({ type: "mode", mode });
        }
        if (this.isRunning) {
            messages.push({ type: "running", running: true });
        }
        for (const key of this.heldKeys) {
            messages.push({ type: "busy", key, busy: true });
        }
        return messages;
    }
}
