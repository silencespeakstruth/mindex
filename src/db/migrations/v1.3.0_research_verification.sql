-- ============================================================
-- mindex schema — migration 5: research runs become re-verifiable
-- ============================================================
-- A journalled run stored its report and its citation COUNTS, but not the facts
-- the counts were computed from: which lines each tool actually showed the model
-- (the `Evidence` spans), each citation's own verdict, or the tool-call trace.
-- So "re-check this report's citations today" was unanswerable offline — the
-- verified/path_only distinction cannot be recomputed from a path list — and
-- "what did this run actually do" lived only on an SSE stream nobody kept.
--
-- This migration adds the persistent half of that machinery:
--
--   research_run_evidence    per (run, path): the line spans tools returned. The
--                            one input of `check_citations` that did not survive
--                            journalling; with it, the whole check is a pure
--                            function over stored data — no GPU, no model.
--   research_run_citations   per citation: path, range, verdict, staleness — the
--                            structured form of what the report claims, queryable
--                            by SQL ("which runs cite this file") without
--                            re-parsing every report.
--   research_run_steps       the tool-call trace: calls + arguments + the spans
--                            each call landed on. Deliberately NO result bodies —
--                            the code is in the index, and a second copy of it
--                            would dwarf the corpus.
--
-- and rebuilds research_runs with the metadata that was measured-but-dropped
-- (revalidation counters, sufficiency verdict), request-decided-but-unjournalled
-- (top_p, the shape grants, checkpoint interval), environmental (model digest and
-- details, embedder model id, server version, wall-clock start) — plus the
-- refutation channel's columns (`kind`, `challenged_run_id`, the claim verdicts),
-- written by the challenge endpoint. All nullable: an old run genuinely did not
-- record them, and NULL is that fact, not a defect.
--
-- The rebuild is the same procedure as migration 4, in the same order (create the
-- replacement under a temporary name, copy with columns NAMED, drop, rename), for
-- the reason documented there: renaming the original first would rewrite the
-- children's REFERENCES clauses to name the corpse. It must run under
-- `migration_transaction` — research_run_files (and, on a re-run, the three new
-- children) reference this table, and only the FK suspension keeps the DROP from
-- cascading their rows away.
--
-- Idempotent: the leading DROP ... IF EXISTS makes a second run start from an
-- empty replacement; the child tables are IF NOT EXISTS.


-- Not IF NOT EXISTS: a second run must start from an empty replacement.
DROP TABLE IF EXISTS research_runs_new;


-- Identical to research_runs as migration 4 left it, plus the columns marked NEW.
CREATE TABLE research_runs_new (
    id                    TEXT    NOT NULL PRIMARY KEY,
    project_guid          TEXT    NOT NULL,
    created_at            INTEGER NOT NULL DEFAULT (unixepoch()),

    seq                   INTEGER NOT NULL,
    expires_at            INTEGER,
    context_run_ids_json  TEXT    NOT NULL DEFAULT '[]',
    title                 TEXT,

    -- NEW. What kind of run this row is. 'research' answers a question;
    -- 'challenge' attacks a stored report's claims. A column rather than a second
    -- table because a challenge IS a run — same loop, same budgets, same citation
    -- provenance — and every reader of this table would otherwise need a UNION.
    kind                  TEXT    NOT NULL DEFAULT 'research'
        CHECK (kind IN ('research', 'challenge')),
    -- NEW. The run this challenge attacked; NULL on ordinary research runs.
    -- NO foreign key, deliberately (the research_run_files.path precedent, but
    -- for a run id): RESTRICT would refuse to delete a run that was ever
    -- challenged, CASCADE would silently erase the challenge record along with
    -- its subject — and the challenge report is evidence in its own right. A
    -- dangling id simply means the subject is gone; the trust status derives
    -- nothing from it.
    challenged_run_id     TEXT,
    -- NEW. The challenge's overall verdict over the subject's claims:
    -- 'confirmed' / 'disputed' / 'refuted'. NULL on research runs — and on a
    -- challenge whose verdict turn produced nothing parseable, which readers
    -- must treat as "challenged, inconclusive", never as an acquittal.
    challenge_verdict     TEXT
        CHECK (
            challenge_verdict IS NULL
            OR challenge_verdict IN ('confirmed', 'disputed', 'refuted')
        ),
    -- NEW. Per-claim verdict counts behind the overall verdict above.
    claims_total          INTEGER,
    claims_confirmed      INTEGER,
    claims_disputed       INTEGER,
    claims_refuted        INTEGER,

    question              TEXT    NOT NULL,
    model                 TEXT    NOT NULL,
    -- NEW. The Ollama blob digest of `model` at run time, from the model catalog;
    -- NULL when the catalog had not seen the model yet. `model` is a mutable name
    -- ("qwen3:32b" after a re-pull is a different artifact); the digest is what
    -- makes two runs actually comparable.
    model_digest          TEXT,
    -- NEW. The catalog's details object for the model (parameter size,
    -- quantization, family), stored whole as JSON: read by humans and notebooks,
    -- never joined on.
    model_details_json    TEXT,
    prompt_version        TEXT    NOT NULL,
    effort                TEXT    NOT NULL,
    seed                  INTEGER,
    temperature           REAL,
    -- NEW. The third sampling axis, threaded beside the two above; NULL = the
    -- model's own default, exactly like temperature.
    top_p                 REAL,

    granted_seconds       INTEGER NOT NULL,
    granted_tokens        INTEGER NOT NULL,
    granted_steps         INTEGER NOT NULL,
    granted_search_top_k  INTEGER NOT NULL,
    -- NEW. The four grants that never reached the row: the context guard and the
    -- three shape axes. "What was this run allowed" was otherwise answerable only
    -- from a log line.
    granted_context_fraction  REAL,
    granted_report_words      INTEGER,
    granted_report_sections   INTEGER,
    granted_evidence_width    INTEGER,
    -- NEW. The resolved checkpoint interval (0 = off) and how many checkpoint
    -- turns actually fired — the pair that says whether banked drafts were in
    -- play when judging what a stopped run shipped.
    checkpoint_every_steps    INTEGER,
    checkpoints_taken         INTEGER,

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
    -- NEW. The scope as data (serialized include/exclude), beside the rendered
    -- prose above. `scope_json` is what the model was told and cannot be parsed
    -- back; this is what lets a challenge run re-inhabit its subject's exact
    -- scope. NULL on unscoped runs and on rows journalled before it existed.
    scope_spec_json       TEXT,
    forced_synthesis      INTEGER NOT NULL,
    report_window_ms      INTEGER NOT NULL,
    report_elapsed_ms     INTEGER NOT NULL,

    -- NEW. The four counters of the citation-repair pass, previously measured and
    -- then dropped at the journal's door (only the metrics decorator read them).
    -- All four NULL together when the draft needed no correction — the stored
    -- form of the wire's nullable draft_* fields.
    revalidation_draft_unverified INTEGER,
    revalidation_draft_path_only  INTEGER,
    revalidation_draft_stale      INTEGER,
    revalidation_steps            INTEGER,
    -- NEW. The sufficiency turn's own ANSWERED/UNANSWERED list, verbatim. NULL
    -- when the turn was skipped (budget-stopped run) or came back empty.
    sufficiency_verdict           TEXT,

    -- NEW. Which embedding model the baselines in research_run_files were read
    -- under. The staleness join has always needed a model_id and bound it from
    -- RouterState; stamping it here is what keeps stored runs interpretable if
    -- the server's embedder is ever swapped.
    embedder_model_id     TEXT,
    -- NEW. The mindex version that produced the row (CARGO_PKG_VERSION).
    server_version        TEXT,
    -- NEW. Wall-clock admission time. `created_at` is the INSERT's time — the
    -- run's END — so the corpus never actually recorded when a run began;
    -- (created_at - started_at) also cross-checks elapsed_ms.
    started_at            INTEGER,

    report                TEXT    NOT NULL
);

-- Columns named explicitly: `SELECT *` would bind by position and go wrong in
-- silence the moment either table's column order stops matching. The NEW columns
-- are not named, so existing rows get NULL — honest "not recorded" — and `kind`
-- takes its DEFAULT 'research'.
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
FROM research_runs;


DROP TABLE research_runs;

ALTER TABLE research_runs_new RENAME TO research_runs;


-- ============================================================
-- Indexes, recreated against the new table
-- ============================================================
-- The first four are verbatim from migration 4; DROP TABLE took them with it.

CREATE INDEX IF NOT EXISTS idx_research_runs_project
ON research_runs (project_guid, created_at);

CREATE INDEX IF NOT EXISTS idx_research_runs_model
ON research_runs (model, prompt_version, created_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_research_runs_seq
ON research_runs (project_guid, seq);

CREATE INDEX IF NOT EXISTS idx_research_runs_expiry
ON research_runs (expires_at)
WHERE expires_at IS NOT NULL;

-- NEW. "Which challenges attack this run" — the trust-status join. Partial:
-- ordinary research rows have nothing to say here, and an index that does not
-- contain them cannot slow their inserts down.
CREATE INDEX IF NOT EXISTS idx_research_runs_challenged
ON research_runs (challenged_run_id)
WHERE challenged_run_id IS NOT NULL;


-- ============================================================
-- What each run's tools actually showed the model
-- ============================================================
-- The persistent form of `Evidence`'s spans — the one input of `check_citations`
-- that research_run_files does not carry. NOT folded into research_run_files,
-- although both are keyed (run_id, path), because their row sets differ in
-- exactly the direction that matters: research_run_files drops paths that were
-- never probed for a hash (nothing to compare later), while the evidence spans
-- must keep them — a path that was shown without a baseline is still a path
-- whose citations verify. Widening research_run_files' sha256 to nullable would
-- also force a semantic edit onto the validity CTE for a column it never reads.
--
-- spans_json is a JSON array of [start_line, end_line] pairs — read whole by the
-- verification endpoint, never joined on, like every *_json column in this
-- schema. A path shown with no usable span (a list_files hit) stores '[]', which
-- is exactly the path_only verdict's input.
--
-- run_id cascades like research_run_files: these rows own nothing outside
-- SQLite.
CREATE TABLE IF NOT EXISTS research_run_evidence (
    run_id     TEXT NOT NULL,
    path       TEXT NOT NULL,
    spans_json TEXT NOT NULL,

    PRIMARY KEY (run_id, path),

    FOREIGN KEY (run_id)
        REFERENCES research_runs (id)
        ON DELETE CASCADE
);


-- ============================================================
-- Each report's citations, structured
-- ============================================================
-- One row per citation OCCURRENCE, in report order (`ord`), duplicates kept: a
-- report citing the same span three times made three claims, and collapsing them
-- would make the recorded counts disagree with the row set. The verdict and
-- staleness are the run's own, at the moment it finished — a later re-check may
-- disagree, which is the finding, not a corruption.
CREATE TABLE IF NOT EXISTS research_run_citations (
    run_id     TEXT    NOT NULL,
    -- Position in the report's citation sequence, 0-based.
    ord        INTEGER NOT NULL,
    path       TEXT    NOT NULL,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL,
    verdict    TEXT    NOT NULL
        CHECK (verdict IN ('verified', 'path_only', 'unverified')),
    -- Orthogonal to the verdict, exactly as on the parent row's counters.
    stale      INTEGER NOT NULL,

    PRIMARY KEY (run_id, ord),

    FOREIGN KEY (run_id)
        REFERENCES research_runs (id)
        ON DELETE CASCADE
);

-- "Which stored runs cite this file" — the query this table exists for.
CREATE INDEX IF NOT EXISTS idx_research_run_citations_path
ON research_run_citations (path);


-- ============================================================
-- The tool-call trace
-- ============================================================
-- Calls, arguments and where each landed — NO result bodies, deliberately: the
-- code a call returned is in the index (read_chunks re-reads it for free), and a
-- copy here would out-weigh the corpus while going stale against it.
--
-- `n` is the wire's step number, so a stored trace lines up with what a client
-- saw; it has gaps by construction (checkpoint turns consume a number without
-- emitting a step). `phase` separates the investigation's steps from the
-- citation-repair pass's, whose numbers continue the same sequence.
-- `spans_json` is the step frame's `spans` list ("path:start-end" strings),
-- bounded by the same cap as the wire.
CREATE TABLE IF NOT EXISTS research_run_steps (
    run_id          TEXT    NOT NULL,
    n               INTEGER NOT NULL,
    phase           TEXT    NOT NULL
        CHECK (phase IN ('main', 'revalidation')),
    -- `action` is the wire field's name on the `step` frame, and a stored trace
    -- that spells it differently would make every cross-reference a translation.
    action          TEXT    NOT NULL, -- noqa: RF04
    argument        TEXT    NOT NULL,
    hits            INTEGER NOT NULL,
    spans_json      TEXT    NOT NULL,
    spans_truncated INTEGER NOT NULL,
    -- Milliseconds since the investigation started, so the trace is a timeline.
    at_ms           INTEGER NOT NULL,

    PRIMARY KEY (run_id, n),

    FOREIGN KEY (run_id)
        REFERENCES research_runs (id)
        ON DELETE CASCADE
);
