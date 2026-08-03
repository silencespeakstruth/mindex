import js from "@eslint/js";
import tseslint from "typescript-eslint";
import prettier from "eslint-config-prettier";

export default tseslint.config(
    { ignores: ["dist/", "out/", "media/js/", "media/codicons/", "node_modules/", "*.vsix"] },
    js.configs.recommended,
    ...tseslint.configs.recommendedTypeChecked,
    {
        languageOptions: {
            parserOptions: {
                // Both projects, explicitly. `projectService: true` resolves a file
                // through the nearest tsconfig.json, and `src/webview/**` is
                // *excluded* from that one (it is the browser half, built by
                // tsconfig.webview.json) — so type-aware linting would fail there
                // with "file not found in project" rather than lint it.
                project: ["./tsconfig.json", "./tsconfig.webview.json"],
                tsconfigRootDir: import.meta.dirname,
            },
        },
        rules: {
            // Unused args are fine when they name a callback's signature; `_`-prefixed
            // bindings are the opt-out (tsc's noUnusedLocals already covers real dead code).
            "@typescript-eslint/no-unused-vars": [
                "error",
                { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
            ],
            // A rejected promise in an extension is a silently-lost error: every call must
            // be awaited or explicitly `void`-ed.
            "@typescript-eslint/no-floating-promises": "error",
        },
    },
    {
        files: ["src/**/*.test.ts"],
        rules: {
            // `describe`/`it` from node:test return a promise the runner owns; the
            // caller is explicitly not meant to await it, and the runner reports a
            // rejection as a failing test. Scoped to test files so the rule keeps its
            // teeth everywhere a lost rejection would really be lost.
            "@typescript-eslint/no-floating-promises": "off",
        },
    },
    {
        // Build tooling: plain ESM run by node, in neither tsconfig project, so
        // type-aware rules have no program to consult.
        files: ["eslint.config.mjs", "esbuild.mjs"],
        extends: [tseslint.configs.disableTypeChecked],
        languageOptions: { globals: { console: "readonly", process: "readonly" } },
    },
    prettier
);
