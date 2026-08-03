/**
 * The language mark: which glyph stands for a language, and which colour class paints it.
 *
 * A language is the one label in this extension where an icon carries information the
 * word does not — a list of twenty `symbol-file` glyphs beside twenty language names
 * is twenty identical pixels, and the eye has to read every row. The marks are the
 * official ones (devicon, vendored by `esbuild.mjs` into [`LANG_GLYPH_SVG`]).
 *
 * It lives under `src/shared` because both halves need it: the webviews draw the marks
 * and `node --test` checks the table against `media/lang.css` without an extension host.
 *
 * **Colour is a class, never a value.** The pages' CSP carries no `'unsafe-inline'`,
 * so an inline `style="color:…"` is blocked outright and `element.style.color` sets
 * that same attribute — the colours therefore live in `media/lang.css` as one rule per
 * language, and this module only ever names one.
 */
import { LANG_GLYPH_SVG } from "./langGlyphs";

/**
 * Languages devicon draws no mark for, and the codicon that stands in.
 *
 * Two entries, for the same reason in two shapes. devicon has marks for a dozen
 * database *products* (postgresql, sqlite, mysql) and none for SQL itself, and
 * picking one of the products would label every `.sql` file in the project with a
 * database this project may not use. For TOML it publishes nothing at all — not the
 * language, not a stand-in — where its sibling formats json and yaml both have one.
 */
export const LANG_FALLBACK_CODICON: Record<string, string> = {
    sql: "database",
    toml: "settings",
};

/** A language this build has never heard of — a server newer than the extension. */
export const UNKNOWN_LANG_CODICON = "symbol-file";

/** How a language's mark should be drawn. */
export type LangGlyph =
    | { kind: "svg"; svg: string; tone: string }
    | { kind: "codicon"; codicon: string; tone: string };

/**
 * The mark for a language id, as the server spells it.
 *
 * Total: an unknown language degrades to a generic file codicon rather than to
 * nothing. A blank where a glyph belongs reads as a rendering bug, and the row it is
 * on is usually the one worth looking at.
 */
export function langGlyph(lang: string): LangGlyph {
    const tone = `lang-${lang}`;
    const svg = LANG_GLYPH_SVG[lang];
    if (svg !== undefined) {
        return { kind: "svg", svg, tone };
    }
    return {
        kind: "codicon",
        codicon: LANG_FALLBACK_CODICON[lang] ?? UNKNOWN_LANG_CODICON,
        tone,
    };
}
