/**
 * One in-flight read at a time, newest wins.
 *
 * The counterpart to `BusyKeys`, which *refuses* a second caller. This one admits
 * it and cancels the first — the rule for reads, where the answer the user wants is
 * always the newest one (a search keystroke, opening another row).
 *
 * The whole of it is the ownership check in `end`, and the reason it is a class
 * rather than three lines repeated is that it was three lines repeated. The
 * Research History panel had the check in one of its three writers of the same
 * handle and a bare `finally` in the other two, so a pass that had *already been
 * superseded* would disown its successor on the way out: the spinner went off while
 * a fetch was still running, and the keystroke after that aborted nothing, leaving
 * a stale page free to render over a newer one.
 *
 * `vscode`-free, so `node --test` can reach it — which is the point, since none of
 * that is visible in a passing manual test.
 */
export class Supersedable {
    private current?: AbortController;

    /** Whether something is running. */
    get busy(): boolean {
        return this.current !== undefined;
    }

    /** The running controller, for a caller that needs to inspect its signal. */
    get controller(): AbortController | undefined {
        return this.current;
    }

    /** Abort whatever is running, and become the owner. */
    begin(): AbortController {
        this.current?.abort();
        const next = new AbortController();
        this.current = next;
        return next;
    }

    /**
     * Give the handle back.
     *
     * Returns whether `controller` was still the owner — which is what a caller
     * keys its "the spinner is now off" message on. A superseded caller gets
     * `false` and must stay quiet: its successor owns the surface now.
     */
    end(controller: AbortController): boolean {
        if (this.current !== controller) {
            return false;
        }
        this.current = undefined;
        return true;
    }

    /** Abort the current owner without expecting it to report back. For dispose. */
    abort(): void {
        this.current?.abort();
        this.current = undefined;
    }
}
