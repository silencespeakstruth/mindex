import js from "@eslint/js";
import tseslint from "typescript-eslint";
import prettier from "eslint-config-prettier";

export default tseslint.config(
    { ignores: ["dist/", "node_modules/", "*.vsix"] },
    js.configs.recommended,
    ...tseslint.configs.recommendedTypeChecked,
    {
        languageOptions: {
            parserOptions: {
                projectService: true,
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
        // The flat config itself is not part of the tsconfig project.
        files: ["eslint.config.mjs"],
        extends: [tseslint.configs.disableTypeChecked],
    },
    prettier
);
