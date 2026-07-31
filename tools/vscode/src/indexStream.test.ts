import { describe, it } from "node:test";
import * as assert from "node:assert";
import {
    IndexEmbeddedEvent,
    IndexIndexedEvent,
    IndexSkippedEvent,
    parseSseFrame,
    routeIndexFrame,
} from "./api";

describe("parseSseFrame", () => {
    it("decodes event name and JSON data", () => {
        const parsed = parseSseFrame('event: indexed\ndata: {"path":"a.rs","count":3}');
        assert.deepStrictEqual(parsed, {
            event: "indexed",
            data: { path: "a.rs", count: 3 },
        });
    });

    it("skips keep-alive comments and malformed JSON instead of throwing", () => {
        assert.strictEqual(parseSseFrame(": keep-alive"), undefined);
        assert.strictEqual(parseSseFrame("event: indexed\ndata: not-json"), undefined);
        assert.strictEqual(parseSseFrame(""), undefined);
    });
});

describe("routeIndexFrame", () => {
    it("routes progress events to their callbacks", () => {
        const seen: string[] = [];
        const out = routeIndexFrame(
            'event: skipped\ndata: {"path":"b.rs","language":"rust","reason":"unchanged"}',
            {
                onSkipped: (e: IndexSkippedEvent) => seen.push(`${e.path}:${e.reason}`),
            }
        );
        assert.strictEqual(out, undefined);
        assert.deepStrictEqual(seen, ["b.rs:unchanged"]);
    });

    it("returns done as a terminal with the JSON-mode files shape", () => {
        const out = routeIndexFrame(
            'event: done\ndata: {"files":{"rust":{"a.rs":7}},"files_indexed":1,"chunks":7,"elapsed_ms":10}',
            {}
        );
        assert.deepStrictEqual(out?.done?.files, { rust: { "a.rs": 7 } });
    });

    it("returns error as a terminal, never a callback", () => {
        const out = routeIndexFrame(
            'event: error\ndata: {"code":"internal","detail":"boom"}',
            {}
        );
        assert.deepStrictEqual(out?.error, { code: "internal", detail: "boom" });
    });

    it("ignores unknown events so a newer server degrades to less detail", () => {
        const out = routeIndexFrame('event: brand_new\ndata: {"x":1}', {});
        assert.strictEqual(out, undefined);
    });

    it("delivers embedded and indexed with their counters", () => {
        const embedded: IndexEmbeddedEvent[] = [];
        const indexed: IndexIndexedEvent[] = [];
        routeIndexFrame(
            "event: embedded\n" +
                'data: {"batch_chunks":64,"chunks_done":128,"chunks_total":150,"elapsed_ms":900}',
            { onEmbedded: (e) => embedded.push(e) }
        );
        routeIndexFrame('event: indexed\ndata: {"path":"a.rs","language":"rust","count":5}', {
            onIndexed: (e) => indexed.push(e),
        });
        assert.strictEqual(embedded[0].chunks_done, 128);
        assert.strictEqual(indexed[0].count, 5);
    });
});
