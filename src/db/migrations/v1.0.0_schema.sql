-- ============================================================
-- mindex schema
-- ============================================================
-- The 1.0.0 schema. Every statement is `IF NOT EXISTS`, so a cold re-run is a
-- no-op — `every_migration_sql_is_idempotent` enforces that and is what forbids
-- `ALTER TABLE ... ADD COLUMN`, which has no `IF NOT EXISTS` form.
--
-- This file is now FROZEN: 1.0.0's schema is in use, so editing it in place
-- would not reach a database already stamped at version 1 (the migration filter
-- is `version > user_version`), and the change would be skipped in silence. A
-- new table goes in a new migration file appended to the `MIGRATIONS` slice in
-- `src/main.rs` — v1.1.0_git_history.sql is the first — and a new *field*, with
-- `ADD COLUMN` unavailable, as a 1:1 side table with `ON DELETE CASCADE`.
--
-- Statement order is load-bearing: SQLite needs a table to exist before a
-- trigger or a foreign key can name it.


-- ============================================================
-- Project metadata
-- ============================================================

CREATE TABLE IF NOT EXISTS projects (
    guid     TEXT NOT NULL CHECK (length(guid) = 32),
    model_id TEXT NOT NULL CHECK (model_id IN ('BAAI/bge-m3')),

    PRIMARY KEY (guid, model_id)
);



-- ============================================================
-- Source files
-- ============================================================

CREATE TABLE IF NOT EXISTS project_files (
    project_guid         TEXT    NOT NULL,
    model_id             TEXT    NOT NULL,

    path TEXT NOT NULL CHECK (
        length(path) > 0     AND
        path NOT GLOB '/*'   AND
        path NOT GLOB '*//*' AND
        path NOT GLOB '*\\*'
    ),

    sha256               TEXT    NOT NULL COLLATE NOCASE CHECK (length(sha256) = 64),
    -- Kept in step with migration 3, which widens this same list on databases
    -- that were created before it: this file is only ever read by a cold start.
    programming_language TEXT    NOT NULL CHECK (programming_language IN (
        'rust', 'python', 'javascript', 'typescript', 'tsx',
        'go', 'c', 'cpp', 'java', 'csharp', 'ruby', 'php', 'bash',
        'html', 'css', 'json', 'scala', 'haskell', 'ocaml', 'zig', 'sql',
        'toml', 'yaml',
        'markdown'
    )),

    status TEXT NOT NULL DEFAULT 'just_uploaded' CHECK (
        status IN ('just_uploaded', 'indexing', 'indexed', 'cancelled', 'failed', 'deleted')
    ),
    retry_count       INTEGER NOT NULL DEFAULT 0,
    status_updated_at INTEGER NOT NULL DEFAULT (unixepoch()),

    -- What produced this file's derived rows. sha256 answers "did the file
    -- change"; it does NOT answer "did the code that derives chunks and symbols
    -- from it change" — so a slicer or tags-query change would leave stale derived
    -- rows behind a matching hash, and the prepare-phase skip would never notice.
    -- That is exactly how every file indexed before the symbols feature ended up
    -- permanently symbol-less: unchanged content, hash match, extraction never run,
    -- and /symbols answering "no such symbol" (which its contract calls definitive)
    -- for a third of the tree.
    --
    -- NULL means "derived by an unknown version". The skip requires equality, so
    -- NULL can never match and the next ordinary indexer run rebuilds that file by
    -- itself — the backlog self-heals with no manual step.
    --
    -- Two versions, not one, because the two rebuilds cost different things:
    -- re-deriving symbols is pure CPU (tree-sitter tags), while re-deriving chunks
    -- re-embeds on the GPU and re-upserts to Qdrant. One shared version would price
    -- every tags-query fix at a full reindex — which is how you end up not bumping
    -- it. TEXT because both hold the MAJOR.MINOR string every internal version in
    -- mindex uses (src/slicing/traits.rs); compared for equality, never ordered.
    chunks_version    TEXT,
    symbols_version   TEXT,

    PRIMARY KEY (project_guid, model_id, path),

    FOREIGN KEY (project_guid, model_id)
        REFERENCES projects (guid, model_id)
        ON DELETE CASCADE
);



-- ============================================================
-- Code chunks
-- ============================================================
-- ON DELETE RESTRICT: chunks must be explicitly managed; no silent cascade.
-- Deleted chunks keep status='deleted' until the GC worker removes them from
-- Qdrant and then hard-deletes them here.

CREATE TABLE IF NOT EXISTS project_file_chunks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,

    project_guid TEXT    NOT NULL,
    file_path    TEXT    NOT NULL,
    model_id     TEXT    NOT NULL,

    code         TEXT    NOT NULL,
    qdrant_guid  TEXT    NOT NULL CHECK (length(qdrant_guid) = 32),

    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_column   INTEGER NOT NULL,

    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deleted')),

    FOREIGN KEY (project_guid, model_id, file_path)
        REFERENCES project_files (project_guid, model_id, path)
        ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_project_file_chunks_lookup
ON project_file_chunks (project_guid, model_id, file_path, status);

CREATE INDEX IF NOT EXISTS idx_project_file_chunks_lookup_qdrant
ON project_file_chunks (qdrant_guid);

-- Partial index used by the GC worker.
CREATE INDEX IF NOT EXISTS idx_chunks_deleted
ON project_file_chunks (project_guid, qdrant_guid)
WHERE status = 'deleted';



-- ============================================================
-- project_files.status state machine + transition audit log
-- ============================================================
-- The status CHECK above validates the *value*; these triggers validate the
-- *transition*. Legal moves:
--   * any → 'indexing'                              (start / reindex / retry)
--   * any → 'deleted'                               (DELETE /files; GC then removes the row)
--   * 'indexing' → 'indexed' | 'cancelled' | 'failed'   (terminal only from work)
-- Everything else (e.g. indexed→failed, failed→indexed, just_uploaded→indexed)
-- is rejected with SQLITE_CONSTRAINT_TRIGGER. Idempotent 'indexing'→'indexing'
-- is allowed (concurrent upserts); other self-loops are not. 'deleted' is terminal
-- except 'deleted'→'indexing' (re-indexing a path that is pending deletion resurrects
-- it) — covered by the any→'indexing' rule.

-- A brand-new row may only enter in a non-terminal state.
CREATE TRIGGER IF NOT EXISTS project_files_status_insert_guard
BEFORE INSERT ON project_files
WHEN NEW.status NOT IN ('just_uploaded', 'indexing')
BEGIN
    SELECT RAISE(ABORT, 'illegal initial project_files.status (must be just_uploaded or indexing)');
END;

-- Fires for both plain UPDATEs and the DO UPDATE branch of upserts.
CREATE TRIGGER IF NOT EXISTS project_files_status_update_guard
BEFORE UPDATE OF status ON project_files
WHEN NOT (
    NEW.status = 'indexing'
    OR NEW.status = 'deleted'
    OR (OLD.status = 'indexing' AND NEW.status IN ('indexed', 'cancelled', 'failed'))
)
BEGIN
    SELECT RAISE(ABORT, 'illegal project_files.status transition');
END;

-- ============================================================
-- Durable transition log — reconstruct the full event history per file.
-- ============================================================
CREATE TABLE IF NOT EXISTS project_file_status_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,

    project_guid TEXT    NOT NULL,
    model_id     TEXT    NOT NULL,
    path         TEXT    NOT NULL,

    old_status   TEXT,                          -- NULL on the initial insert
    new_status   TEXT    NOT NULL,
    retry_count  INTEGER NOT NULL,
    at           INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_status_log_file
ON project_file_status_log (project_guid, model_id, path, at);

CREATE TRIGGER IF NOT EXISTS project_files_status_log_insert
AFTER INSERT ON project_files
BEGIN
    INSERT INTO project_file_status_log
        (project_guid, model_id, path, old_status, new_status, retry_count)
    VALUES (NEW.project_guid, NEW.model_id, NEW.path, NULL, NEW.status, NEW.retry_count);
END;

-- Log only meaningful changes (status or retry_count); skip idempotent no-ops.
CREATE TRIGGER IF NOT EXISTS project_files_status_log_update
AFTER UPDATE OF status ON project_files
WHEN NEW.status <> OLD.status OR NEW.retry_count <> OLD.retry_count
BEGIN
    INSERT INTO project_file_status_log
        (project_guid, model_id, path, old_status, new_status, retry_count)
    VALUES (NEW.project_guid, NEW.model_id, NEW.path, OLD.status, NEW.status, NEW.retry_count);
END;



-- ============================================================
-- Defense-in-depth validation triggers
-- ============================================================
-- The request layer (backend::v0::validate) is the primary validator and rejects
-- bad input as a 400 before it reaches SQLite. These are the last line of defense:
-- the same shape invariants, enforced where a bug in the API cannot route around
-- them, raising SQLITE_CONSTRAINT_TRIGGER (surfaced as a 500).
--
-- Triggers rather than tightened column CHECKs, for two reasons that outlive the
-- original one (that SQLite cannot ALTER a CHECK onto an existing table): each
-- one RAISEs a message naming the constraint it broke, where a column CHECK
-- reports only the table, and a constraint added later has to be a trigger
-- anyway. Keeping the whole family in one mechanism is what makes that uniform.

-- A stored sha256 must be 64 hexadecimal characters. The column CHECK above
-- already covers the length; this adds the hex constraint (a non-hex char would
-- otherwise pass a length-only CHECK).
CREATE TRIGGER IF NOT EXISTS project_files_sha256_insert_guard
BEFORE INSERT ON project_files
WHEN NEW.sha256 GLOB '*[^0-9a-fA-F]*'
BEGIN
    SELECT RAISE(ABORT, 'project_files.sha256 must be 64 hexadecimal characters');
END;

CREATE TRIGGER IF NOT EXISTS project_files_sha256_update_guard
BEFORE UPDATE OF sha256 ON project_files
WHEN NEW.sha256 GLOB '*[^0-9a-fA-F]*'
BEGIN
    SELECT RAISE(ABORT, 'project_files.sha256 must be 64 hexadecimal characters');
END;

-- retry_count is a non-negative counter.
CREATE TRIGGER IF NOT EXISTS project_files_retry_count_insert_guard
BEFORE INSERT ON project_files
WHEN NEW.retry_count < 0
BEGIN
    SELECT RAISE(ABORT, 'project_files.retry_count must be non-negative');
END;

CREATE TRIGGER IF NOT EXISTS project_files_retry_count_update_guard
BEFORE UPDATE OF retry_count ON project_files
WHEN NEW.retry_count < 0
BEGIN
    SELECT RAISE(ABORT, 'project_files.retry_count must be non-negative');
END;

-- A chunk must carry non-empty code and a sane, non-negative line/column span.
CREATE TRIGGER IF NOT EXISTS project_file_chunks_span_insert_guard
BEFORE INSERT ON project_file_chunks
WHEN length(NEW.code) = 0
    OR NEW.start_line < 0
    OR NEW.end_line < 0
    OR NEW.start_column < 0
    OR NEW.end_column < 0
    OR NEW.start_line > NEW.end_line
BEGIN
    SELECT RAISE(ABORT, 'project_file_chunks requires non-empty code and a valid line/column span');
END;



-- ============================================================
-- Code symbols (definitions)
-- ============================================================
-- Extracted at indexing time from the language's upstream tree-sitter tags
-- query (one universal extractor, src/slicing/symbols.rs). DEFINITIONS ONLY:
-- the query emits references too, and they were stored until they were
-- measured — 87.5% of the table serving one tool nobody called. See
-- v1.4.0_symbol_definitions.sql, which narrows an existing database to the
-- shape this file now creates directly; the two must be kept in step. Unlike
-- chunks,
-- symbol rows have no Qdrant counterpart, so they are HARD-deleted inline —
-- no soft-delete/GC cycle. Invariant: a file's symbol rows always parallel
-- its chunk set — every transaction that marks a file's chunks 'deleted'
-- deletes its symbols in the same statement batch, and inserts happen in the
-- same transaction as chunk inserts. ON DELETE RESTRICT guards the ordering
-- (a project_files row can only be pruned after its symbols are gone).

CREATE TABLE IF NOT EXISTS project_file_symbols (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,

    project_guid TEXT    NOT NULL,
    model_id     TEXT    NOT NULL,
    file_path    TEXT    NOT NULL,

    name         TEXT    NOT NULL CHECK (length(name) > 0),
    kind         TEXT    NOT NULL CHECK (length(kind) > 0),

    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_column   INTEGER NOT NULL,

    parent_name  TEXT,
    parent_kind  TEXT,
    doc          TEXT,

    FOREIGN KEY (project_guid, model_id, file_path)
        REFERENCES project_files (project_guid, model_id, path)
        ON DELETE RESTRICT
);

-- Symbol lookup (the /symbols endpoint).
CREATE INDEX IF NOT EXISTS idx_project_file_symbols_lookup
ON project_file_symbols (project_guid, model_id, name);

-- Per-file replacement on reindex / delete.
CREATE INDEX IF NOT EXISTS idx_project_file_symbols_file
ON project_file_symbols (project_guid, model_id, file_path);



-- ============================================================
-- Research run journal
-- ============================================================
-- A /research run would otherwise exist only for the lifetime of its SSE
-- connection: one INFO line at the start, one at the end, and nothing that held
-- the question or the report. Only runs somebody was watching could then be
-- measured — production traffic would be permanently unobservable, and every
-- quality question ("did that prompt change help?", "which model answers
-- better?") would have to be answered by re-running rather than by querying what
-- already happened.
--
-- This table is that trace. It is also the corpus: question + report + which
-- prompt and model produced them is exactly what a measurement harness needs,
-- and it accumulates for free from ordinary use.
--
-- DELIBERATELY NOT foreign-keyed to project_files, and deliberately not indexed
-- into Qdrant. A research report is not a file: giving it a project_files row
-- would need a programming_language that passes the CHECK and a repo-relative
-- path that passes the path CHECK, and would then poison POST /drift — the
-- working-tree manifest can never contain it, so every sweep would classify it
-- `orphaned`, `mindex-index --check` would report permanent actionable drift and
-- exit non-zero, and the watcher would keep trying to delete it. project_guid is
-- therefore a plain column: runs for a deleted project simply outlive it, which
-- is correct for a measurement record.
--
-- Writes are best-effort and happen after the report has already been streamed,
-- so a failure here costs a row and nothing else. One row, one INSERT: a run is a
-- single flat measurement record, so every count below is a column here rather
-- than a 1:1 side table, and "a run has all its rows or none" needs no
-- transaction to be true.

CREATE TABLE IF NOT EXISTS research_runs (
    id                    TEXT    NOT NULL PRIMARY KEY,
    project_guid          TEXT    NOT NULL,
    created_at            INTEGER NOT NULL DEFAULT (unixepoch()),

    question              TEXT    NOT NULL,
    model                 TEXT    NOT NULL,
    -- Which generation of the server's instructions drove the run. Two reports
    -- written under different prompts are not comparable; without this a prompt
    -- regression is indistinguishable from model variance.
    prompt_version        TEXT    NOT NULL,
    effort                TEXT    NOT NULL,
    -- Sampling actually used. NULL = the model's own Modelfile default, which is
    -- not the same as "unknown" and not the same as any particular number.
    seed                  INTEGER,
    temperature           REAL,

    -- The resolved budget (effort preset + request overrides), as granted.
    granted_seconds       INTEGER NOT NULL,
    granted_tokens        INTEGER NOT NULL,
    granted_steps         INTEGER NOT NULL,
    granted_search_top_k  INTEGER NOT NULL,

    -- What it actually cost.
    done_reason           TEXT    NOT NULL,
    steps                 INTEGER NOT NULL,
    turns                 INTEGER NOT NULL,
    elapsed_ms            INTEGER NOT NULL,
    prompt_tokens         INTEGER NOT NULL,
    eval_tokens           INTEGER NOT NULL,
    peak_prompt_tokens    INTEGER NOT NULL,
    num_ctx               INTEGER NOT NULL,

    -- The provenance verdict on the report's citations.
    citations_total       INTEGER NOT NULL,
    citations_verified    INTEGER NOT NULL,
    citations_path_only   INTEGER NOT NULL,
    citations_unverified  INTEGER NOT NULL,
    -- JSON arrays, not child tables: nothing joins on them and nothing enforces
    -- them; they are read whole, by a human or a notebook.
    cited_paths_json      TEXT    NOT NULL,
    unverified_paths_json TEXT    NOT NULL,

    -- How far the index moved underneath the run. Nothing serializes /research
    -- against indexing, deliberately: the writer is an external process, so
    -- excluding it would mean answering `mindex-watch` with a 409 and dropping the
    -- debounced change for the very file the user had just edited. Indexing keeps
    -- priority; the run reports what moved instead. Without these, a report written
    -- over a corpus that shifted is indistinguishable from one written over a
    -- corpus that held still, and "was this answer describing code that still
    -- exists?" cannot be asked of the history at all. Zero is a measurement.
    --
    -- `stale_citations` is distinct from `citations_unverified`: provenance asks
    -- whether the model was ever shown the location, staleness asks whether what it
    -- was shown still holds. There is no `in_flight_files` on purpose — whether a
    -- reindex was in flight is a momentary state the loop reports to the model
    -- while it is true, and a count of it at the instant a run ended says nothing.
    changed_files         INTEGER NOT NULL,
    removed_files         INTEGER NOT NULL,
    stale_citations       INTEGER NOT NULL,
    stale_paths_json      TEXT    NOT NULL,

    -- Tool usage and scope. A scratchpad the model writes to (`note`), an
    -- exact-literal lookup (`grep`) and a server-enforced scope each landed on an
    -- argument rather than a measurement; these are how those arguments become
    -- checkable. Per-tool call counts are deliberately absent — the Prometheus
    -- counter `mindex_research_tool_calls` already carries them, labelled by tool.
    -- What is here is what the metric cannot express: refusals, caps hit, and the
    -- per-run scope the counts have to be read against.
    --
    --   notes_*            — is the scratchpad used at all, and does the cap bite?
    --   plan_revisions     — zero means `revise_plan` is dead weight
    --   grep_calls/_hits   — does `grep` earn the step it costs?
    --   out_of_scope_*     — what a scoped run spends finding its walls, against
    --                        how much noise the scope kept out of the transcript
    --   forced_synthesis   — the report was written by the SERVER because the
    --                        report window expired: the operational symptom of a
    --                        `report_timeout_ms` set too tight, and the one case
    --                        where `report` is not the model's words
    --   report_window_ms   — granted vs taken, which nothing else can answer:
    --   report_elapsed_ms    `elapsed_ms` stops before the report phase begins
    --
    -- `scope_json` is NULL — not `{}` — for an unscoped run, so "no scope" and "a
    -- scope that happened to be empty" stay distinguishable. Read whole by a human
    -- or a notebook, like the `*_paths_json` columns; nothing joins on it.
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

-- The two axes anything ever asks by: one project's history, newest first.
CREATE INDEX IF NOT EXISTS idx_research_runs_project
ON research_runs (project_guid, created_at);

-- "How did prompt X do against model Y" — the bake-off query.
CREATE INDEX IF NOT EXISTS idx_research_runs_model
ON research_runs (model, prompt_version, created_at);
