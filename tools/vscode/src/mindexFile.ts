// Parser for the repo-root .mindex file. Mirrors tools/mindexfile (Rust), which is
// the reference implementation — the indexer and watcher read the same file, and a
// disagreement here shows up as files that are permanently "orphaned" in the Drift
// view rather than as an error. Keep the two in step.
//
//     guid: c2d7e2c1-3165-42f5-9366-0ff1492b4bab
//     exclude_paths:
//       - tools/**
//     include_paths: []
//     languages: []

import { parse as parseYaml } from "yaml";

export interface MindexFile {
    /** Canonical hyphenated lowercase UUID (either spelling is accepted on disk). */
    guid: string;
    includePaths: string[];
    excludePaths: string[];
    /** Lowercase mindex language ids; empty means all languages. */
    languages: string[];
}

const KNOWN_KEYS = ["guid", "include_paths", "exclude_paths", "languages"];
const HYPHENATED = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const DASHLESS = /^[0-9a-f]{32}$/;

export function parseMindexFile(text: string): MindexFile {
    let doc: unknown;
    try {
        doc = parseYaml(text);
    } catch (e) {
        throw new Error(
            `.mindex is not valid YAML: ${e instanceof Error ? e.message : String(e)}`
        );
    }
    if (doc === null || typeof doc !== "object" || Array.isArray(doc)) {
        throw new Error(
            ".mindex must be a YAML mapping with a `guid:` key and optional " +
                "`include_paths:`/`exclude_paths:`/`languages:` lists"
        );
    }
    const map = doc as Record<string, unknown>;

    // Unknown keys are an error, as in the Rust parser: a mistyped `exclude_path:`
    // that is silently ignored means the excluded tree gets indexed.
    const unknown = Object.keys(map).filter((k) => !KNOWN_KEYS.includes(k));
    if (unknown.length > 0) {
        throw new Error(
            `.mindex has unknown key(s): ${unknown.join(", ")} — expected ${KNOWN_KEYS.join(", ")}`
        );
    }

    return {
        guid: normalizeGuid(map.guid),
        includePaths: stringList(map.include_paths, "include_paths"),
        excludePaths: stringList(map.exclude_paths, "exclude_paths"),
        languages: stringList(map.languages, "languages"),
    };
}

function normalizeGuid(value: unknown): string {
    if (typeof value !== "string" || value.trim() === "") {
        throw new Error(
            ".mindex has no `guid:` — add the project GUID, e.g. " +
                "`guid: c2d7e2c1-3165-42f5-9366-0ff1492b4bab`"
        );
    }
    const guid = value.trim().toLowerCase();
    if (HYPHENATED.test(guid)) {
        return guid;
    }
    if (DASHLESS.test(guid)) {
        return [
            guid.slice(0, 8),
            guid.slice(8, 12),
            guid.slice(12, 16),
            guid.slice(16, 20),
            guid.slice(20),
        ].join("-");
    }
    throw new Error(
        `.mindex \`guid: ${value}\` is not a UUID — expected e.g. ` +
            "c2d7e2c1-3165-42f5-9366-0ff1492b4bab (dashless is accepted too)"
    );
}

function stringList(value: unknown, key: string): string[] {
    if (value === undefined || value === null) {
        return [];
    }
    // A bare string is rejected rather than split: one syntax, not two.
    if (!Array.isArray(value) || value.some((v) => typeof v !== "string")) {
        throw new Error(
            `.mindex \`${key}:\` must be a list of strings, e.g.\n${key}:\n  - src/**`
        );
    }
    return (value as string[]).map((v) => v.trim()).filter((v) => v !== "");
}
