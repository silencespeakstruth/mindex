import { SearchFilter } from "./api";

/** A scope's two halves, as every surface that carries one passes them around. */
export interface Scope {
    include?: SearchFilter;
    exclude?: SearchFilter;
}

/**
 * The scope in one line, or `""` when unscoped.
 *
 * Mirrors the server's own `ToolScope::describe` wording ("only …", "never …") so the
 * research panel header, the search picker title and the report's caveats all name the
 * same boundary the same way. It lives here rather than in `researchView.ts` because
 * scope is no longer a research-only idea: a search can be scoped too, and a picker
 * that shows 3 results without saying it was looking at a tenth of the tree is
 * indistinguishable from a project that small.
 */
export function describeScope(scope?: Scope): string {
    if (scope === undefined) {
        return "";
    }
    const parts: string[] = [];
    const add = (label: string, f?: SearchFilter): void => {
        if (f?.paths !== undefined && f.paths.length > 0) {
            parts.push(`${label} ${f.paths.join(", ")}`);
        }
        if (f?.programming_languages !== undefined && f.programming_languages.length > 0) {
            parts.push(`${label} ${f.programming_languages.join(", ")}`);
        }
    };
    add("only", scope.include);
    add("never", scope.exclude);
    return parts.join("; ");
}

/** Whether anything at all narrows this scope. */
export function isScoped(scope?: Scope): boolean {
    return describeScope(scope) !== "";
}
