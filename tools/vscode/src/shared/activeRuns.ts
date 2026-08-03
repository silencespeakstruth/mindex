/**
 * Row derivation for the Active Research Runs QuickPick — vscode-free so
 * `node --test` reaches it.
 */

/** The slice of `GET /research/active`'s run shape this module reads. */
export interface ActiveRunLike {
    run_id: string;
    project_guid: string;
    question: string;
    model: string;
    effort: string;
    age_ms: number;
    granted_seconds: number;
    worst_case_ms: number;
}

export interface ActiveRunRow {
    label: string;
    description: string;
    detail: string;
    /**
     * Past `granted_seconds + report window` — the same predicate health and the
     * watchdog use for "wedged". Such a run is the one worth cancelling by hand.
     */
    overWorstCase: boolean;
}

export function formatAge(ms: number): string {
    const secs = Math.floor(ms / 1000);
    if (secs < 90) {
        return `${secs}s`;
    }
    const mins = Math.floor(secs / 60);
    if (mins < 90) {
        return `${mins} min`;
    }
    return `${(mins / 60).toFixed(1)} h`;
}

export function describeActiveRun(run: ActiveRunLike): ActiveRunRow {
    const over = run.age_ms > run.worst_case_ms;
    return {
        label: run.question,
        description:
            `${run.model} · ${run.effort} · running ${formatAge(run.age_ms)}` +
            ` of ${formatAge(run.worst_case_ms)} worst-case` +
            (over ? " — past its worst case, likely wedged" : ""),
        detail: `project ${run.project_guid}`,
        overWorstCase: over,
    };
}
