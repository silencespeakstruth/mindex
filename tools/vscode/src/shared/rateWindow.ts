/**
 * Sliding-window rate over a monotonically growing counter — what turns the
 * streaming /index `embedded` events (cumulative `chunks_done`) into an honest
 * live chunks-per-second. A cumulative average flattens a long run into a
 * meaningless number; a window answers "what is it doing *now*".
 *
 * vscode-free on purpose, like `debounce.ts`, so `node --test` can reach it.
 */
export class RateWindow {
    private samples: Array<{ t: number; v: number }> = [];

    constructor(private readonly windowMs: number) {}

    /** Record the counter's value `v` at time `t` (ms). `t` must not go backwards. */
    push(t: number, v: number): void {
        this.samples.push({ t, v });
        // Keep one sample older than the window so the rate always spans at
        // least the full window once enough time has passed.
        while (this.samples.length > 2 && t - this.samples[1].t > this.windowMs) {
            this.samples.shift();
        }
    }

    /** Units per second across the window; undefined until two samples span time. */
    perSecond(): number | undefined {
        if (this.samples.length < 2) {
            return undefined;
        }
        const first = this.samples[0];
        const last = this.samples[this.samples.length - 1];
        const dt = last.t - first.t;
        if (dt <= 0) {
            return undefined;
        }
        return ((last.v - first.v) * 1000) / dt;
    }
}
