/**
 * The streaming `/index` (`?stream=yes`) wire events, mirroring the server's
 * `IndexEvent` (src/backend/v0/models.rs). Readers ignore events and fields they
 * do not know, so a newer server degrades to less detail rather than an error.
 *
 * They live under `shared/` rather than in `api.ts` because both the run aggregate
 * and the webview page are typed against them, and `api.ts` pulls in `node:https`
 * — which neither of those may import. `api.ts` re-exports every name here, so no
 * call site has to know where they moved.
 */

export interface IndexStartedEvent {
    files: number;
    /** True when every later `count` is symbol rows, not chunks. */
    symbols_only: boolean;
}
export interface IndexPreparedEvent {
    path: string;
    language: string;
    chunks: number;
    symbols: number;
}
export interface IndexSkippedEvent {
    path: string;
    language: string;
    /** `unchanged`, `in_flight` or `cancelled` today; opaque so a new reason displays. */
    reason: string;
}
export interface IndexEmbeddedEvent {
    /** One embed batch, encoded and upserted; `chunks_done` is cumulative — the
     *  honest chunks-per-second source. `elapsed_ms` is the server's own clock. */
    batch_chunks: number;
    chunks_done: number;
    chunks_total: number;
    elapsed_ms: number;
}
export interface IndexIndexedEvent {
    path: string;
    language: string;
    count: number;
}
export interface IndexDoneEvent {
    /** Byte-for-byte the JSON mode's `IndexResponse.files` — the two must agree,
     *  and `indexStream` asserts it by returning this as the response body. */
    files: Record<string, Record<string, number>>;
    files_indexed: number;
    chunks: number;
    elapsed_ms: number;
}

/**
 * Everything a caller can learn from one streaming `/index` request.
 *
 * `onDone` is not a terminal in the promise sense — the promise still settles on
 * the `done` event, and this is how a caller reads the three fields the response
 * body does not carry (`files_indexed`, `chunks`, the server's own `elapsed_ms`).
 * `onJsonFallback` is the opposite: it fires when the server answered plain JSON,
 * meaning no other callback here will ever fire for that request.
 */
export interface IndexStreamCallbacks {
    onStarted?(e: IndexStartedEvent): void;
    onPrepared?(e: IndexPreparedEvent): void;
    onSkipped?(e: IndexSkippedEvent): void;
    onEmbedded?(e: IndexEmbeddedEvent): void;
    onIndexed?(e: IndexIndexedEvent): void;
    onDone?(e: IndexDoneEvent): void;
    /** The server ignored `?stream=yes` and answered plain JSON — this request
     *  produced no events at all, and the numbers can only come from its body. */
    onJsonFallback?(): void;
}
