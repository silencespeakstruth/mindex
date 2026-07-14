// Extension → mindex language id. Mirrors tools/indexer/src/scanner.rs::detect_language —
// keep the two maps in sync when adding a language (see CLAUDE.md "Languages" checklist).
const EXT_TO_LANGUAGE: Record<string, string> = {
    rs: "rust",
    py: "python",
    pyw: "python",
    js: "javascript",
    mjs: "javascript",
    cjs: "javascript",
    jsx: "javascript",
    ts: "typescript",
    mts: "typescript",
    cts: "typescript",
    tsx: "tsx",
    go: "go",
    c: "c",
    h: "c",
    cpp: "cpp",
    cc: "cpp",
    cxx: "cpp",
    hpp: "cpp",
    hxx: "cpp",
    hh: "cpp",
    java: "java",
    cs: "csharp",
    rb: "ruby",
    php: "php",
    phtml: "php",
    sh: "bash",
    bash: "bash",
    html: "html",
    htm: "html",
    xhtml: "html",
    css: "css",
    json: "json",
    scala: "scala",
    sc: "scala",
    hs: "haskell",
    lhs: "haskell",
    ml: "ocaml",
    mli: "ocaml",
    zig: "zig",
    sql: "sql",
};

/** mindex language id for a repo-relative path, or undefined if unsupported. */
export function detectLanguage(relPath: string): string | undefined {
    const base = relPath.slice(relPath.lastIndexOf("/") + 1);
    const dot = base.lastIndexOf(".");
    if (dot <= 0) {
        return undefined;
    }
    return EXT_TO_LANGUAGE[base.slice(dot + 1).toLowerCase()];
}
