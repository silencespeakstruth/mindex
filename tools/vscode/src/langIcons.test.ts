import { ok, strictEqual } from "node:assert";
import { describe, it } from "node:test";
import * as fs from "node:fs";
import * as path from "node:path";
import { ALL_LANGUAGES } from "./languages";
import { LANG_FALLBACK_CODICON, langGlyph } from "./shared/langIcons";
import { DEVICON_VERSION, LANG_GLYPH_SVG } from "./shared/langGlyphs";

const root = path.join(__dirname, "..");
const langCss = fs.readFileSync(path.join(root, "media", "lang.css"), "utf8");

/**
 * The base colours the pairs in `media/lang.css` are derived from.
 *
 * Four are deliberately **not** devicon's published colour, because devicon's is black
 * or a flat grey — a mark that says nothing and then has to be lightened into a
 * different grey anyway. These are the widely-used language colours instead. Listing
 * them here is what makes the substitution a decision someone made rather than a value
 * that drifted.
 */
const BRAND: Record<string, string> = {
    bash: "#89e051", // devicon: #293138 (near-black)
    c: "#a9bacd",
    cpp: "#004482",
    csharp: "#68217a",
    css: "#3d8fc6",
    go: "#00acd7",
    haskell: "#5e5185",
    html: "#e54d26",
    java: "#ea2d2e",
    javascript: "#f0db4f",
    json: "#505050",
    markdown: "#755838", // devicon: #000000
    ocaml: "#f18803",
    php: "#777bb3",
    python: "#ffd845",
    ruby: "#d91404",
    rust: "#dea584", // devicon: #000
    scala: "#de3423",
    sql: "#6a9fb5", // no devicon mark at all
    toml: "#9c4221", // no devicon mark at all; the widely-used TOML colour
    tsx: "#61dafb",
    typescript: "#007acc",
    yaml: "#cb171e",
    zig: "#f7a41d",
};

/** VS Code's two default backgrounds — what the pairs are required to be legible on. */
const DARK_BG = "#1f1f1f";
const LIGHT_BG = "#ffffff";
/** WCAG's floor for a graphical object, which is what an icon is. */
const MIN_CONTRAST = 3;

function channels(hex: string): number[] {
    const h = hex.replace("#", "");
    const full = h.length === 3 ? [...h].map((c) => c + c).join("") : h;
    return [0, 2, 4].map((i) => parseInt(full.substr(i, 2), 16));
}

function luminance(hex: string): number {
    const [r, g, b] = channels(hex)
        .map((v) => v / 255)
        .map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a: string, b: string): number {
    const [x, y] = [luminance(a), luminance(b)];
    return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}

/** The authoring rule: mix toward `target` in 5% steps until it clears the floor. */
function adapt(colour: string, background: string, target: string): string {
    const from = channels(colour);
    const to = channels(target);
    for (let p = 0; p <= 1.001; p += 0.05) {
        const mixed =
            "#" +
            from
                .map((v, i) => Math.round(v + (to[i] - v) * p))
                .map((v) => v.toString(16).padStart(2, "0"))
                .join("");
        if (contrast(mixed, background) >= MIN_CONTRAST) {
            return mixed;
        }
    }
    return target;
}

function rule(lang: string): { light?: string; dark?: string } {
    const block = new RegExp(`\\.lang-${lang}\\s*\\{([^}]*)\\}`).exec(langCss);
    if (block === null) {
        return {};
    }
    return {
        light: /--lang-light:\s*(#[0-9a-f]{6})/.exec(block[1])?.[1],
        dark: /--lang-dark:\s*(#[0-9a-f]{6})/.exec(block[1])?.[1],
    };
}

describe("language marks", () => {
    /**
     * The failure this prevents is silent: a language the extension can label but has
     * no mark for renders a bare generic glyph, which reads as "some other kind of
     * file" in a list where every other row is identifiable.
     */
    it("draws a mark for every language the extension can label", () => {
        for (const lang of ALL_LANGUAGES) {
            const glyph = langGlyph(lang);
            if (glyph.kind === "codicon") {
                ok(
                    lang in LANG_FALLBACK_CODICON,
                    `${lang} has no devicon mark and no declared fallback — it would ` +
                        `render as a generic file glyph`
                );
            }
            strictEqual(glyph.tone, `lang-${lang}`);
        }
    });

    /** The reverse: a vendored mark for a language the extension never labels. */
    it("vendors no mark the extension cannot reach", () => {
        const known = new Set<string>(ALL_LANGUAGES);
        for (const lang of Object.keys(LANG_GLYPH_SVG)) {
            ok(known.has(lang), `${lang} is vendored but is not a MINDex language`);
        }
        for (const lang of Object.keys(LANG_FALLBACK_CODICON)) {
            ok(known.has(lang), `${lang} declares a fallback but is not a MINDex language`);
        }
    });

    it("inherits currentColor in every vendored mark", () => {
        for (const [lang, svg] of Object.entries(LANG_GLYPH_SVG)) {
            ok(svg.includes('fill="currentColor"'), `${lang} does not inherit its colour`);
            // A surviving hard-coded fill wins over `currentColor` and pins the mark to
            // one theme — exactly the defect the vendoring step exists to remove.
            ok(!/fill="#/.test(svg), `${lang} kept a hard-coded fill and will not recolour`);
        }
    });

    /**
     * Colour is the half that cannot be eyeballed. More than half of the official
     * brand colours fail on one of the two default themes, so each language declares a
     * derived pair — and this recomputes the derivation rather than trusting it.
     */
    it("declares a legible colour pair for every language", () => {
        for (const lang of ALL_LANGUAGES) {
            const base = BRAND[lang];
            ok(base !== undefined, `${lang} has no base colour in the test's BRAND table`);
            const { light, dark } = rule(lang);
            ok(
                light !== undefined && dark !== undefined,
                `media/lang.css has no .lang-${lang}`
            );
            strictEqual(light, adapt(base, LIGHT_BG, "#000000"), `${lang} light`);
            strictEqual(dark, adapt(base, DARK_BG, "#ffffff"), `${lang} dark`);
            ok(contrast(light, LIGHT_BG) >= MIN_CONTRAST, `${lang} is illegible on light`);
            ok(contrast(dark, DARK_BG) >= MIN_CONTRAST, `${lang} is illegible on dark`);
        }
    });

    /**
     * `src/shared/langGlyphs.ts` is generated and committed, on the precedent of the
     * vendored tags queries. That only stays honest if a devicon bump that redraws a
     * mark fails here instead of silently shipping a different logo.
     */
    it("is in sync with the devicon release it was vendored from", () => {
        const installed = JSON.parse(
            fs.readFileSync(path.join(root, "node_modules", "devicon", "package.json"), "utf8")
        ) as { version: string };
        strictEqual(
            DEVICON_VERSION,
            installed.version,
            "run `npm run build` to re-vendor the language marks"
        );
    });
});
