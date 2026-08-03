import { describe, it } from "node:test";
import * as assert from "node:assert";
import { describeActiveRun, formatAge } from "./activeRuns";

const base = {
    run_id: "r1",
    project_guid: "p1",
    question: "How does GC work?",
    model: "glm4",
    effort: "medium",
    age_ms: 60_000,
    granted_seconds: 900,
    worst_case_ms: 1_020_000,
};

describe("formatAge", () => {
    it("scales seconds, minutes, hours", () => {
        assert.strictEqual(formatAge(45_000), "45s");
        assert.strictEqual(formatAge(300_000), "5 min");
        assert.strictEqual(formatAge(7_200_000), "2.0 h");
    });
});

describe("describeActiveRun", () => {
    it("shows model, effort and age against the worst case", () => {
        const row = describeActiveRun(base);
        assert.strictEqual(row.label, "How does GC work?");
        assert.match(row.description, /glm4 · medium · running 60s of 17 min worst-case/);
        assert.strictEqual(row.overWorstCase, false);
    });

    it("flags a run past its worst case as likely wedged", () => {
        const row = describeActiveRun({ ...base, age_ms: 1_500_000 });
        assert.strictEqual(row.overWorstCase, true);
        assert.match(row.description, /likely wedged/);
    });
});
