// Parser for the repo-root .mindex file. Mirrors tools/watcher/src/mindex_file.rs:
// first non-comment, non-blank line that isn't a recognized `key:` line is the project
// GUID (passed through verbatim); optional include_paths/exclude_paths/languages
// comma-lists carry the project's indexing/search scope.

export interface MindexFile {
    guid: string;
    includePaths: string[];
    excludePaths: string[];
    /** Lowercase mindex language ids; empty means all languages. */
    languages: string[];
}

export function parseMindexFile(text: string): MindexFile {
    let guid: string | undefined;
    let includePaths: string[] = [];
    let excludePaths: string[] = [];
    let languages: string[] = [];

    for (const raw of text.split("\n")) {
        const line = raw.trim();
        if (line === "" || line.startsWith("#")) {
            continue;
        }
        if (line.startsWith("include_paths:")) {
            includePaths = commaList(line.slice("include_paths:".length));
        } else if (line.startsWith("exclude_paths:")) {
            excludePaths = commaList(line.slice("exclude_paths:".length));
        } else if (line.startsWith("languages:")) {
            languages = commaList(line.slice("languages:".length));
        } else if (guid === undefined) {
            guid = line;
        }
    }

    if (guid === undefined) {
        throw new Error(
            ".mindex has no project GUID — the first non-comment non-blank line must be the GUID"
        );
    }
    return { guid, includePaths, excludePaths, languages };
}

function commaList(s: string): string[] {
    return s
        .split(",")
        .map((p) => p.trim())
        .filter((p) => p !== "");
}
