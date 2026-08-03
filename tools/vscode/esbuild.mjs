import * as esbuild from "esbuild";
import * as fs from "node:fs";
import * as path from "node:path";

/**
 * The build.
 *
 * Two very different bundles come out of one config, because the extension has two
 * runtimes: the **host** (Node, CommonJS, `vscode` provided by the editor) and the
 * **webviews** (browser, ES modules, no Node at all). `tsc` still runs — it is the
 * type checker and it emits the test artifacts — but it no longer produces what
 * ships.
 *
 * What bundling actually buys here is not startup time; it is that
 * `node_modules` stops being part of the package. That was a real defect and not a
 * theoretical one: `.vscodeignore` excluded `node_modules/**` wholesale while
 * `mindexFile.ts` did `require("yaml")`, so the published `.vsix` could not activate
 * at all — invisible in development, where the directory is simply there. A bundle
 * has no such failure mode: whatever an import reaches is in the file or the build
 * failed.
 *
 * The two assets that cannot be bundled — the codicon stylesheet and its font — are
 * copied into `media/` instead, for the same reason: a webview asset resolved out of
 * `node_modules` is one more path that has to be listed in three places
 * (`.vscodeignore`, `localResourceRoots`, the CSP) and fails silently when it is not.
 */

const production = process.argv.includes("--production");
const watch = process.argv.includes("--watch");

/** Copy the codicon font + stylesheet into media/ so nothing loads from node_modules. */
function copyCodicons() {
    const from = path.join("node_modules", "@vscode", "codicons", "dist");
    fs.mkdirSync("media/codicons", { recursive: true });
    for (const file of ["codicon.css", "codicon.ttf"]) {
        fs.copyFileSync(path.join(from, file), path.join("media/codicons", file));
    }
}

/**
 * Which devicon mark stands for each MINDex language.
 *
 * The keys are `ProgrammingLanguage` names as the server spells them, so this map is
 * the *only* place the two vocabularies meet — `langIcons.ts` looks glyphs up by
 * language id and never learns a devicon name. `sql` is deliberately absent: devicon
 * has no generic SQL mark, and it falls back to a codicon (see `LANG_FALLBACK`).
 *
 * Every entry is a **monochrome** variant, which is what makes recolouring possible at
 * all — the marks are recoloured per language *and* per theme, because more than half
 * the official brand colours fail a 3:1 contrast check against one background or the
 * other (rust and markdown are black; C is near-white). A multicolour `-original`
 * would be unrecolourable and illegible on one theme in exchange.
 */
const DEVICON_MARKS = {
    rust: "rust/rust-original",
    python: "python/python-plain",
    javascript: "javascript/javascript-plain",
    typescript: "typescript/typescript-plain",
    tsx: "react/react-original",
    go: "go/go-plain",
    c: "c/c-original",
    cpp: "cplusplus/cplusplus-plain",
    java: "java/java-plain",
    csharp: "csharp/csharp-plain",
    ruby: "ruby/ruby-plain",
    php: "php/php-plain",
    bash: "bash/bash-plain",
    html: "html5/html5-plain",
    css: "css3/css3-plain",
    json: "json/json-plain",
    scala: "scala/scala-plain",
    haskell: "haskell/haskell-plain",
    ocaml: "ocaml/ocaml-plain",
    zig: "zig/zig-original",
    yaml: "yaml/yaml-plain",
    markdown: "markdown/markdown-original",
};

/**
 * Vendor the language marks into `src/shared/langGlyphs.ts`.
 *
 * Generated *and committed*, on the precedent of the tree-sitter tags queries vendored
 * under `slicing/queries/`: the alternative is importing `.svg` from `node_modules`,
 * which `tsc` refuses across `rootDir` and which would put the webview's assets back in
 * the directory the bundle exists to escape. `langGlyphsAreVendoredFromDevicon` fails
 * when this output stops matching what devicon ships, so a bump that redraws a mark is
 * loud rather than silent.
 *
 * Shipping the devicon *font* instead was measured and rejected: it is 1.5 MB for the
 * 21 glyphs used here, against a whole extension of 181 KB.
 */
function generateLangGlyphs() {
    const version = JSON.parse(
        fs.readFileSync(path.join("node_modules", "devicon", "package.json"), "utf8")
    ).version;

    const entries = Object.entries(DEVICON_MARKS).map(([lang, mark]) => {
        const raw = fs.readFileSync(
            path.join("node_modules", "devicon", "icons", `${mark}.svg`),
            "utf8"
        );
        const viewBox = /viewBox="([^"]+)"/.exec(raw)?.[1] ?? "0 0 128 128";
        // Drop every hard-coded colour so the mark inherits `currentColor`, and the
        // wrapper along with it — only the drawing is kept.
        const body = raw
            .replace(/^[\s\S]*?<svg[^>]*>/, "")
            .replace(/<\/svg>\s*$/, "")
            .replace(/\s*fill="[^"]*"/g, "")
            .trim();
        // `width`/`height` are attributes and not left to CSS on purpose: the mark then
        // has an intrinsic size, so it renders even if `media/lang.css` never arrives.
        // A stylesheet that fails to load is invisible, and an icon that silently
        // collapses to nothing is the way that failure would present.
        return `    ${lang}: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="${viewBox}" width="14" height="14" fill="currentColor" aria-hidden="true">${body}</svg>',`;
    });

    const out = `// GENERATED by esbuild.mjs from devicon ${version} — do not edit by hand.
//
// One monochrome mark per MINDex language, stripped of its hard-coded fills so CSS
// \`color\` drives it. Regenerate with \`npm run build\`; the source map of language →
// devicon mark lives in \`esbuild.mjs\` (\`DEVICON_MARKS\`), which is also what
// \`langIcons.test.ts\` re-reads to prove this file is still what devicon ships.
//
// devicon is MIT-licensed; see node_modules/devicon/LICENSE.

/** Language id (as the server spells it) → an inline SVG that inherits \`currentColor\`. */
export const LANG_GLYPH_SVG: Record<string, string> = {
${entries.join("\n")}
};

/** The devicon release these marks were vendored from. */
export const DEVICON_VERSION = "${version}";
`;

    const target = path.join("src", "shared", "langGlyphs.ts");
    // Only write on a real change: an unconditional write retriggers `tsc --watch`.
    if (!fs.existsSync(target) || fs.readFileSync(target, "utf8") !== out) {
        fs.writeFileSync(target, out);
    }
}

/** Reports a failed rebuild in watch mode, where esbuild otherwise stays quiet. */
const logProblems = {
    name: "log-problems",
    setup(build) {
        build.onEnd((result) => {
            for (const e of result.errors) {
                console.error(`✘ ${e.text}`, e.location ?? "");
            }
            console.log(result.errors.length === 0 ? "✔ build" : "✘ build failed");
        });
    },
};

const common = {
    bundle: true,
    minify: production,
    sourcemap: production ? false : "inline",
    logLevel: "silent",
    plugins: [logProblems],
};

/** The extension host: one CommonJS file. `vscode` is injected by the editor. */
const host = {
    ...common,
    entryPoints: ["src/extension.ts"],
    outfile: "dist/extension.js",
    platform: "node",
    format: "cjs",
    target: "node20",
    external: ["vscode"],
};

/**
 * One ES module per page. Not one shared bundle: three panels are never open in the
 * same document, so a shared chunk would mean every page downloads all three.
 */
const webviews = {
    ...common,
    entryPoints: {
        ask: "src/webview/ask.ts",
        status: "src/webview/status.ts",
        research: "src/webview/research.ts",
        runs: "src/webview/runs.ts",
        indexing: "src/webview/indexing.ts",
    },
    outdir: "media/js",
    platform: "browser",
    format: "esm",
    target: "es2022",
};

copyCodicons();
generateLangGlyphs();

if (watch) {
    const contexts = await Promise.all([esbuild.context(host), esbuild.context(webviews)]);
    await Promise.all(contexts.map((c) => c.watch()));
} else {
    await Promise.all([esbuild.build(host), esbuild.build(webviews)]);
}
