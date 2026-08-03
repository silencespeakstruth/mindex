/**
 * Trailing debounce.
 *
 * Imports nothing from `vscode` on purpose — that is the whole reason it lives in
 * `shared/`. `npm test` runs `node --test` against `out/`, with no extension host, so
 * anything a test must reach cannot touch the `vscode` module. Same split as
 * `statusFetch.ts` and `shared/askFields.ts`.
 *
 * **Trailing, not leading.** The caller is a search box: the first keystroke of
 * "collection_for" is `c`, and firing on it costs a request whose results are wrong
 * by the time they arrive. Waiting for the pause is the point.
 *
 * Distinct from the 120 ms coalescer in `webview/research.ts`, which throttles a
 * stream the *server* drives. This one waits on a human.
 */
export interface Debounced<A extends unknown[]> {
    (...args: A): void;
    /**
     * Drop a pending call. Needed on disposal — a timer that fires into a torn-down
     * webview is a "postMessage on a disposed panel" error the user sees and cannot
     * act on.
     */
    cancel(): void;
    /** Run any pending call now. For an explicit "search" press mid-wait. */
    flush(): void;
}

export function debounce<A extends unknown[]>(
    ms: number,
    fn: (...args: A) => void
): Debounced<A> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let pending: A | undefined;

    const run = (): void => {
        timer = undefined;
        const args = pending;
        pending = undefined;
        if (args !== undefined) {
            fn(...args);
        }
    };

    const debounced = (...args: A): void => {
        // The LAST arguments win: a debounced search must issue the query the user
        // finished typing, not the prefix that started the wait.
        pending = args;
        if (timer !== undefined) {
            clearTimeout(timer);
        }
        timer = setTimeout(run, ms);
    };

    debounced.cancel = (): void => {
        if (timer !== undefined) {
            clearTimeout(timer);
            timer = undefined;
        }
        pending = undefined;
    };

    debounced.flush = (): void => {
        if (timer !== undefined) {
            clearTimeout(timer);
            run();
        }
    };

    return debounced;
}

/**
 * Leading **and trailing** throttle — at most one call per `ms`, and the last call
 * of a burst always lands.
 *
 * The trailing half is the whole reason this exists next to [`debounce`]. The
 * indexing feed's own rate cap was leading-only, and indexing arrives in bursts: a
 * whole batch prepares in a few milliseconds, then the run goes quiet for the
 * length of a GPU pass. Dropping the burst's *last* event froze every surface on
 * the second-to-last file's numbers for exactly the stretch they exist to explain,
 * which is what "nothing is moving" looked like from outside.
 *
 * The opposite of the search box, hence the opposite edge policy: here the first
 * event of a burst is the one worth showing immediately, and the human is reading
 * rather than typing.
 */
export function throttle<A extends unknown[]>(
    ms: number,
    fn: (...args: A) => void
): Debounced<A> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let pending: A | undefined;
    let lastRun = Number.NEGATIVE_INFINITY;

    const invoke = (args: A): void => {
        lastRun = Date.now();
        fn(...args);
    };

    const run = (): void => {
        timer = undefined;
        const args = pending;
        pending = undefined;
        if (args !== undefined) {
            invoke(args);
        }
    };

    const throttled = (...args: A): void => {
        const wait = ms - (Date.now() - lastRun);
        if (wait <= 0 && timer === undefined) {
            invoke(args);
            return;
        }
        // Inside the window: remember the newest arguments and let the timer
        // already running deliver them.
        pending = args;
        timer ??= setTimeout(run, Math.max(wait, 0));
    };

    throttled.cancel = (): void => {
        if (timer !== undefined) {
            clearTimeout(timer);
            timer = undefined;
        }
        pending = undefined;
    };

    throttled.flush = (): void => {
        if (timer !== undefined) {
            clearTimeout(timer);
            run();
        }
    };

    return throttled;
}
