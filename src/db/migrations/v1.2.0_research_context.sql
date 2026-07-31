-- ============================================================
-- mindex schema — migration 4: stored research becomes reusable context
-- ============================================================
-- A /research run used to exist only for the lifetime of its SSE connection. The
-- journal held the report, but nothing read it back, so every question had to be
-- re-asked from scratch at full GPU cost. This migration turns that journal into a
-- corpus a *new* run can be given as prior context, which needs three things the
-- table could not express:
--
--   seq                  a short, per-project, monotonic identifier. It is both the
--                        number a human types ("research #17") and the keyset cursor
--                        a paginated list walks. `id` is a UUID and can be neither:
--                        it is unreadable aloud and unordered, so paging by it would
--                        have to fall back to OFFSET, which skips and duplicates rows
--                        whenever the set changes underneath the reader.
--   expires_at           when GC may reap the row. NULL means PINNED — kept forever.
--                        Nullable rather than a separate `pinned` flag because the
--                        two would be able to disagree, and a pinned row with a live
--                        expiry is a bug waiting for a sweep to find it.
--   context_run_ids_json which earlier runs were fed to this one. A JSON array, like
--                        the *_paths_json columns beside it: read whole, by a human or
--                        a notebook, never joined on.
--
-- v1.0.0_schema.sql is frozen and is only ever read by a cold start, so it keeps the
-- ORIGINAL shape of this table and is NOT edited to match. Unlike migration 3 there
-- is nothing to keep in step: a cold start applies this file too, one statement after
-- the other, and lands in the same place.
--
-- The rebuild is SQLite's own procedure, in its order — create the replacement under
-- a temporary name, copy, DROP the original, rename into place — for the reason
-- migration 3 documents at length: renaming the original out of the way first would
-- rewrite any child's REFERENCES clause to name the corpse. research_runs has no
-- children *yet*, which is exactly why the order matters here: research_run_files is
-- created below, so a second run of this file rebuilds a table that does have one.
--
-- research_runs carries no triggers, so unlike migration 3 there is nothing to
-- recreate afterwards but its two indexes — DROP TABLE takes those with it.
--
-- Idempotent: a second run rebuilds the table into the same shape, which costs a copy
-- and changes nothing. The leading DROP ... IF EXISTS is what makes that second run
-- start from an empty replacement instead of inserting a second copy of every row.


-- Not IF NOT EXISTS: a second run must start from an empty replacement.
DROP TABLE IF EXISTS research_runs_new;


-- Identical to research_runs in v1.0.0_schema.sql except for the three columns
-- marked NEW below. The column comments are not repeated here; that file remains
-- the place they are documented.
CREATE TABLE research_runs_new (
    id                    TEXT    NOT NULL PRIMARY KEY,
    project_guid          TEXT    NOT NULL,
    created_at            INTEGER NOT NULL DEFAULT (unixepoch()),

    -- NEW. Assigned per project as MAX(seq) + 1 inside the insert transaction.
    -- GC reaps the OLDEST rows, so MAX(seq) survives a sweep and the sequence stays
    -- monotonic; only wiping every run of a project restarts it at 1, which cannot
    -- confuse a live cursor because a cursor over an empty set returns nothing either
    -- way.
    seq                   INTEGER NOT NULL,
    -- NEW. NULL = pinned, never reaped. Otherwise created_at + [research].retention_days.
    expires_at            INTEGER,
    -- NEW. The earlier runs whose reports were injected into this one's transcript.
    context_run_ids_json  TEXT    NOT NULL DEFAULT '[]',
    -- NEW. The report's own first ATX heading, extracted server-side at journalling
    -- time. NULL when the report has no heading or the heading trivially repeats
    -- the question; readers fall back to a title derived from `question`.
    title                 TEXT,

    question              TEXT    NOT NULL,
    model                 TEXT    NOT NULL,
    prompt_version        TEXT    NOT NULL,
    effort                TEXT    NOT NULL,
    seed                  INTEGER,
    temperature           REAL,

    granted_seconds       INTEGER NOT NULL,
    granted_tokens        INTEGER NOT NULL,
    granted_steps         INTEGER NOT NULL,
    granted_search_top_k  INTEGER NOT NULL,

    done_reason           TEXT    NOT NULL,
    steps                 INTEGER NOT NULL,
    turns                 INTEGER NOT NULL,
    elapsed_ms            INTEGER NOT NULL,
    prompt_tokens         INTEGER NOT NULL,
    eval_tokens           INTEGER NOT NULL,
    peak_prompt_tokens    INTEGER NOT NULL,
    num_ctx               INTEGER NOT NULL,

    citations_total       INTEGER NOT NULL,
    citations_verified    INTEGER NOT NULL,
    citations_path_only   INTEGER NOT NULL,
    citations_unverified  INTEGER NOT NULL,
    cited_paths_json      TEXT    NOT NULL,
    unverified_paths_json TEXT    NOT NULL,

    changed_files         INTEGER NOT NULL,
    removed_files         INTEGER NOT NULL,
    stale_citations       INTEGER NOT NULL,
    stale_paths_json      TEXT    NOT NULL,

    notes_written         INTEGER NOT NULL,
    notes_rejected        INTEGER NOT NULL,
    plan_revisions        INTEGER NOT NULL,
    grep_calls            INTEGER NOT NULL,
    grep_hits             INTEGER NOT NULL,
    out_of_scope_refusals INTEGER NOT NULL,
    out_of_scope_rows     INTEGER NOT NULL,
    scoped                INTEGER NOT NULL,
    scope_json            TEXT,
    forced_synthesis      INTEGER NOT NULL,
    report_window_ms      INTEGER NOT NULL,
    report_elapsed_ms     INTEGER NOT NULL,

    report                TEXT    NOT NULL
);

-- Columns named explicitly: `SELECT *` would bind by position and go wrong in
-- silence the moment either table's column order stops matching.
--
-- Existing rows are numbered by created_at, with id breaking the tie so two runs
-- recorded in the same second renumber reproducibly on a re-run. They are given no
-- expiry — NULL, i.e. pinned: a retention policy introduced today must not
-- retroactively condemn a corpus recorded before it existed, and the operator can
-- unpin whatever they do not want.
INSERT INTO research_runs_new (
    id,
    project_guid,
    created_at,
    seq,
    expires_at,
    context_run_ids_json,
    title,
    question,
    model,
    prompt_version,
    effort,
    seed,
    temperature,
    granted_seconds,
    granted_tokens,
    granted_steps,
    granted_search_top_k,
    done_reason,
    steps,
    turns,
    elapsed_ms,
    prompt_tokens,
    eval_tokens,
    peak_prompt_tokens,
    num_ctx,
    citations_total,
    citations_verified,
    citations_path_only,
    citations_unverified,
    cited_paths_json,
    unverified_paths_json,
    changed_files,
    removed_files,
    stale_citations,
    stale_paths_json,
    notes_written,
    notes_rejected,
    plan_revisions,
    grep_calls,
    grep_hits,
    out_of_scope_refusals,
    out_of_scope_rows,
    scoped,
    scope_json,
    forced_synthesis,
    report_window_ms,
    report_elapsed_ms,
    report
)
SELECT
    id,
    -- Normalised to the 32-char simple form every other table uses. Rows written
    -- before this migration stored `Uuid::to_string()`, which is hyphenated: nothing
    -- had ever read this table back BY PROJECT, so the mismatch was invisible until
    -- the browse endpoints and the per-project gauges arrived — the first would have
    -- returned an empty list for every project, and the second would have labelled
    -- the same project two different ways from `project_files`. Idempotent: a guid
    -- with no hyphens is unchanged.
    replace(project_guid, '-', '') AS project_guid,
    created_at,
    ROW_NUMBER() OVER (PARTITION BY project_guid ORDER BY created_at, id) AS seq,
    NULL AS expires_at,
    '[]' AS context_run_ids_json,
    -- Pre-migration reports were journalled without a heading contract, so no title
    -- is reconstructed for them; readers fall back to the question.
    NULL AS title,
    question,
    model,
    prompt_version,
    effort,
    seed,
    temperature,
    granted_seconds,
    granted_tokens,
    granted_steps,
    granted_search_top_k,
    done_reason,
    steps,
    turns,
    elapsed_ms,
    prompt_tokens,
    eval_tokens,
    peak_prompt_tokens,
    num_ctx,
    citations_total,
    citations_verified,
    citations_path_only,
    citations_unverified,
    cited_paths_json,
    unverified_paths_json,
    changed_files,
    removed_files,
    stale_citations,
    stale_paths_json,
    notes_written,
    notes_rejected,
    plan_revisions,
    grep_calls,
    grep_hits,
    out_of_scope_refusals,
    out_of_scope_rows,
    scoped,
    scope_json,
    forced_synthesis,
    report_window_ms,
    report_elapsed_ms,
    report
FROM research_runs;


DROP TABLE research_runs;

ALTER TABLE research_runs_new RENAME TO research_runs;


-- ============================================================
-- Indexes, recreated against the new table
-- ============================================================
-- The first two are verbatim from v1.0.0_schema.sql; DROP TABLE took them with it.

-- The two axes anything ever asks by: one project's history, newest first.
CREATE INDEX IF NOT EXISTS idx_research_runs_project
ON research_runs (project_guid, created_at);

-- "How did prompt X do against model Y" — the bake-off query.
CREATE INDEX IF NOT EXISTS idx_research_runs_model
ON research_runs (model, prompt_version, created_at);

-- NEW. The keyset page: one project's runs, newest first, resumed from a cursor.
-- SQLite scans an index backwards for ORDER BY seq DESC, so no DESC is needed here.
-- UNIQUE is the backstop rather than an optimisation: two runs finishing in the same
-- instant would otherwise be able to share a number, and a duplicated cursor value
-- makes a keyset page repeat or skip rows. A refused insert is better — the journal
-- is best-effort, so it costs a `warn!` and a row, which is the pre-existing contract.
CREATE UNIQUE INDEX IF NOT EXISTS idx_research_runs_seq
ON research_runs (project_guid, seq);

-- NEW. The GC sweep. Partial, because the pinned rows it must never touch are
-- exactly the ones with no expiry, and an index that does not contain them cannot
-- lead a sweep to one.
CREATE INDEX IF NOT EXISTS idx_research_runs_expiry
ON research_runs (expires_at)
WHERE expires_at IS NOT NULL;



-- ============================================================
-- What each stored run was written against
-- ============================================================
-- A stored report describes the tree as it was when the run read it, and the tree
-- moves. Without a baseline, a report from last month and one from this morning are
-- indistinguishable — so the reader cannot tell which claims still hold, and feeding
-- the wrong one into a new run as "prior context" injects confident, obsolete
-- statements into its transcript.
--
-- This is the persistent form of what the loop already tracks in memory:
-- `Evidence.baseline_sha`, the index's hash for a file at the moment the run first
-- probed it. Staleness is then a read-time comparison against project_files, which is
-- the same question `apply_versions` answers during a run, asked later.
--
-- DELIBERATELY NOT a global project-version counter, which was the obvious
-- alternative. A counter bumped by every indexing transaction would mark every stored
-- run of a project stale the moment any one file changed — and with mindex-watch
-- running, that is every save. The feature would be correct and useless. Per-path is
-- more storage and one more join, and it is the difference between "this report is
-- about code that moved" and "somebody touched the README".
--
-- `path` carries NO foreign key, for the reason project_commit_paths.path carries
-- none, and the two candidate constraints fail in opposite directions:
--
--   RESTRICT would make `prune_deleted_files` refuse to drop any file that any past
--   run ever read. That breaks a live invariant elsewhere — the sweep-then-drop
--   ordering in worker/gc.rs is what makes `DELETE /files` eventually physical — so
--   research would silently become a brake on the GC of the code channel.
--   CASCADE would delete the baseline along with the file, and a run whose evidence
--   row vanished reads as FRESH: the one verdict that is certainly wrong, since the
--   file it described is gone.
--
-- "The file left the index" is a *result* this table exists to produce, not a
-- corruption to repair. The join into project_files is therefore soft, by equality,
-- and a missing row reads as `removed`. Do not "fix" the missing FK.
--
-- Note the join also needs `model_id`, which is NOT stored here: project_files is
-- keyed (project_guid, model_id, path), and a database re-indexed under a second
-- embedding model would otherwise match a run's baseline against two rows. Readers
-- bind it from RouterState, exactly as `file_versions_core` does. The run's *research*
-- model is a different thing and lives in research_runs.model.
--
-- run_id, by contrast, IS foreign-keyed, ON DELETE CASCADE: these rows own nothing
-- outside SQLite, so their lifecycle is project_file_symbols' (delete and be done),
-- not project_file_chunks' (soft-delete and wait for GC). That inverts only if a
-- stored run ever gains vectors.
CREATE TABLE IF NOT EXISTS research_run_files (
    run_id TEXT NOT NULL,
    path   TEXT NOT NULL,
    -- COLLATE NOCASE to match project_files.sha256, so the comparison cannot fail on
    -- hex case alone.
    sha256 TEXT NOT NULL COLLATE NOCASE CHECK (length(sha256) = 64),

    PRIMARY KEY (run_id, path),

    FOREIGN KEY (run_id)
        REFERENCES research_runs (id)
        ON DELETE CASCADE
);
