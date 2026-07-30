import { deepStrictEqual, match, ok, strictEqual } from "node:assert";
import { describe, it } from "node:test";
import type { MindexApi } from "./api";
import { failedCount, StatusSnapshot, UNAVAILABLE } from "./shared/status";
import { Availability, fetchStatus } from "./statusFetch";

/** Enough of `MindexApi` for a refresh, with each call individually breakable. */
function fakeApi(over: Partial<Record<string, () => Promise<unknown>>> = {}): MindexApi {
    const fail = (): Promise<never> => Promise.reject(new Error("boom"));
    return {
        health:
            over.health ??
            (() =>
                Promise.resolve({
                    status: "ok",
                    version: "1.0.0",
                    checks: { sqlite: "ok", qdrant: "ok", embedder: "ok", ollama: "ok" },
                })),
        status:
            over.status ??
            (() =>
                Promise.resolve({
                    indexing_claims: 0,
                    gc_running: false,
                    pool_available: 4,
                    pool_size: 4,
                    indexing_files: 0,
                    files_by_status: {},
                })),
        config:
            over.config ??
            (() => Promise.resolve({ version: "1.0.0", model_id: "m", languages: ["rust"] })),
        projectStats:
            over.projectStats ??
            (() =>
                Promise.resolve({
                    project_guid: "g",
                    files: {},
                    languages: {
                        rust: {
                            files: 3,
                            indexed_files: 3,
                            chunks_active: 9,
                            chunks_deleted: 0,
                        },
                        json: {
                            files: 1,
                            indexed_files: 0,
                            chunks_active: 0,
                            chunks_deleted: 0,
                        },
                    },
                })),
        listFiles: over.listFiles ?? (() => Promise.resolve({ files: [] })),
        _fail: fail,
    } as unknown as MindexApi;
}

interface FanOut {
    availability: Availability[];
    inventory: (string[] | undefined)[];
    configs: number;
}

/** The last availability the fetch published — what the Ask form would be gated on. */
function gate(fan: FanOut): Availability {
    const last = fan.availability.at(-1);
    ok(last !== undefined, "the fetch published no availability at all");
    return last;
}

// No default for `guid`: `run(api, undefined)` would take it and quietly test the
// opposite of the "no project" case.
async function run(
    api: MindexApi,
    guid: string | undefined
): Promise<{ snapshot: StatusSnapshot; fan: FanOut }> {
    const fan: FanOut = { availability: [], inventory: [], configs: 0 };
    const snapshot = await fetchStatus(api, guid, "https://127.0.0.1:11111", {
        onAvailability: (a) => fan.availability.push(a),
        onInventory: (l) => fan.inventory.push(l),
        onServerConfig: () => {
            fan.configs += 1;
        },
    });
    return { snapshot, fan };
}

describe("fetchStatus", () => {
    /**
     * The invariant the whole rewrite rests on. The Ask form's language pickers, its
     * model list and its budget sliders are all fed by these three callbacks, and the
     * status *surface* is now an editor panel that is closed almost all the time. If
     * the fan-out ever became conditional on a subscriber, the form would silently
     * stop updating for every user who does not keep the panel open — which is all of
     * them.
     */
    it("feeds the Ask form even with nothing subscribed to its snapshots", async () => {
        const { fan } = await run(fakeApi(), "guid");
        deepStrictEqual(gate(fan), { ask: true, research: true });
        deepStrictEqual(fan.inventory, [["rust"]]);
        strictEqual(fan.configs, 1);
    });

    it("offers only languages with live chunks", async () => {
        // `json` has files but no active chunks: filtering on it returns a 404 that
        // reads to the user as "your query matched nothing".
        const { fan } = await run(fakeApi(), "guid");
        deepStrictEqual(fan.inventory, [["rust"]]);
    });

    it("reports an unreachable server without blanking the pickers", async () => {
        const { snapshot, fan } = await run(
            fakeApi({ health: () => Promise.reject(new Error("ECONNREFUSED")) }),
            "guid"
        );
        strictEqual(snapshot.state, "unreachable");
        // Both flags down, and the reason is the transport error — that string is what
        // the form shows, so "unreachable" must not reach the user as a bare word.
        strictEqual(gate(fan).ask, false);
        strictEqual(gate(fan).research, false);
        match(gate(fan).reason ?? "", /ECONNREFUSED/);
        // `undefined`, never `[]`: unknown falls back to the full supported list,
        // whereas empty would leave a dead picker.
        deepStrictEqual(fan.inventory, [undefined]);
        strictEqual(fan.configs, 0, "a failed health check must not push a config");
    });

    it("keeps a dead Ollama out of the server's state", async () => {
        const { snapshot, fan } = await run(
            fakeApi({
                health: () =>
                    Promise.resolve({
                        status: "ok",
                        version: "1.0.0",
                        checks: {
                            sqlite: "ok",
                            qdrant: "ok",
                            embedder: "ok",
                            ollama: "error: refused",
                        },
                    }),
            }),
            "guid"
        );
        // Ollama is optional: it costs Research and nothing else, so the state stays
        // "ok" and only `researchAvailable` moves.
        strictEqual(snapshot.state, "ok");
        strictEqual(snapshot.researchAvailable, false);
        // The half of the gate that matters most: Search must stay available. Gating
        // both on one flag would take the whole view down for a missing local model.
        strictEqual(gate(fan).ask, true);
        strictEqual(gate(fan).research, false);
    });

    /**
     * The other direction, and the one the gate exists for: a *required* dependency is
     * down, the server says so itself, and nothing in the Ask view can be asked for.
     */
    it("closes the whole Ask surface when a required dependency is down", async () => {
        const { snapshot, fan } = await run(
            fakeApi({
                health: () =>
                    Promise.resolve({
                        status: "degraded",
                        version: "1.0.0",
                        checks: {
                            sqlite: "ok",
                            qdrant: "ok",
                            embedder: "error: connection refused",
                            ollama: "ok",
                        },
                    }),
            }),
            "guid"
        );
        strictEqual(snapshot.state, "degraded");
        strictEqual(gate(fan).ask, false);
        strictEqual(gate(fan).research, false);
        // Names the process to go and start. "The server is degraded" would send the
        // user to the panel to find out what this already knows.
        match(gate(fan).reason ?? "", /embedder/);
    });

    /**
     * Ollama must never be blamed for degradation. It cannot cause it — the server
     * keeps health at "ok" without it — so naming it in the reason would point at the
     * one dependency that is not the problem.
     */
    it("names only required dependencies as the cause of degradation", async () => {
        const { fan } = await run(
            fakeApi({
                health: () =>
                    Promise.resolve({
                        status: "degraded",
                        version: "1.0.0",
                        checks: {
                            sqlite: "ok",
                            qdrant: "error: refused",
                            embedder: "ok",
                            ollama: "error: refused",
                        },
                    }),
            }),
            "guid"
        );
        match(gate(fan).reason ?? "", /qdrant/);
        ok(!(gate(fan).reason ?? "").includes("ollama"));
    });

    it("treats a missing ollama check as present, not down", async () => {
        // An older server omits the check entirely. Absent is not a failure.
        const { snapshot } = await run(
            fakeApi({
                health: () =>
                    Promise.resolve({
                        status: "ok",
                        version: "0.9.0",
                        checks: { sqlite: "ok", qdrant: "ok", embedder: "ok" },
                    }),
            }),
            "guid"
        );
        strictEqual(snapshot.researchAvailable, true);
    });

    /**
     * Three different empties, which the tree this replaced collapsed into one dim
     * leaf. The panel has to render them differently — "no project" is not "the fetch
     * failed" is not "this project holds nothing" — and only the middle one is a
     * problem worth a user's attention.
     */
    it("distinguishes absent, unavailable and empty inventories", async () => {
        const noProject = await run(fakeApi(), undefined);
        strictEqual(noProject.snapshot.inventory, undefined);
        deepStrictEqual(noProject.fan.inventory, [undefined]);

        const broken = await run(
            fakeApi({ projectStats: () => Promise.reject(new Error("boom")) }),
            "guid"
        );
        strictEqual(broken.snapshot.inventory, UNAVAILABLE);
        deepStrictEqual(broken.fan.inventory, [undefined]);

        const empty = await run(
            fakeApi({
                projectStats: () =>
                    Promise.resolve({ project_guid: "g", files: {}, languages: {} }),
            }),
            "guid"
        );
        deepStrictEqual(empty.snapshot.inventory, {});
        deepStrictEqual(empty.fan.inventory, [[]]);
    });

    it("degrades a section that fails without failing the refresh", async () => {
        const { snapshot } = await run(
            fakeApi({
                status: () => Promise.reject(new Error("boom")),
                listFiles: () => Promise.reject(new Error("boom")),
            }),
            "guid"
        );
        strictEqual(snapshot.state, "ok");
        strictEqual(snapshot.runtime, UNAVAILABLE);
        strictEqual(snapshot.failed, UNAVAILABLE);
        strictEqual(failedCount(snapshot), 0);
    });

    it("publishes a snapshot subscribers can read after the fact", async () => {
        const { snapshot } = await run(fakeApi(), "guid");
        strictEqual(snapshot.version, "1.0.0");
        strictEqual(snapshot.serverUrl, "https://127.0.0.1:11111");
        deepStrictEqual(snapshot.checks?.qdrant, "ok");
    });
});
