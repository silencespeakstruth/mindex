-- ============================================================
-- mindex schema — the v2 baseline
-- ============================================================
-- The whole schema in one file, restarting the migration lineage at 1. The v1
-- lineage (six files, `model_id` in seven primary keys, no embedding-model
-- registry) is NOT migratable: startup refuses any database whose
-- `application_id` is not ours while `user_version` is stamped, with an
-- instruction to delete and reindex (docs/claude/retrieval-v3.md). The refusal
-- is what lets this file be a clean fold of everything the v1 lineage had
-- accreted, rather than a seventh rebuild on top of it.
--
-- What changed against v1, in one place:
--
--   * `model_id` is gone from every table. Files, chunks, symbols and commits
--     are facts about the working tree; a vector is a derived artifact, and the
--     model varies over the artifact — so the embedding model's identity now
--     lives where the artifacts are tracked: `embedding_models` (the registry),
--     `project_files.embedded_model_id` (whose vectors exist for this file) and
--     `project_files.chunker_id` (which tokenizer sliced it).
--   * `project_file_chunks.tokens` — the chunk's own token count, so a model
--     swap's blast radius ("which chunks exceed the new window") is a query,
--     not a corpus-wide re-tokenization.
--   * `qdrant_guid` is UNIQUE — the search winners' display query looks rows up
--     by it alone and always assumed global uniqueness; now the schema says so.
--
-- Every statement is `IF NOT EXISTS` / `INSERT OR IGNORE`, so a cold re-run is
-- a no-op — `every_migration_sql_is_idempotent` enforces that and is what
-- forbids `ALTER TABLE ... ADD COLUMN`, which has no `IF NOT EXISTS` form.
--
-- Statement order is load-bearing: SQLite needs a table to exist before a
-- trigger or a foreign key can name it (`embedding_models` before
-- `project_files`, `project_commits` before `project_commit_paths`).

-- 'MX03'. The lineage mark startup reads BEFORE applying migrations: a database
-- with a different application_id and a non-zero user_version is an old-lineage
-- database and is refused rather than read wrongly.
PRAGMA application_id = 0x4D583033;


-- ============================================================
-- Embedding model registry
-- ============================================================
-- The SQLite half of the two-sided model-identity contract. The Rust half is
-- `src/models/registry.rs::EMBEDDING_MODELS`; startup refuses to run unless the
-- two agree id-for-id and dim-for-dim (`verify_model_registry` in main.rs), so
-- a rebuilt binary can never silently reinterpret stored vectors.
--
-- The CHECK is the point, not a decoration: a canonical id is a value the
-- schema itself can vouch for, so no code path — not even a hand-edited row —
-- can introduce vectors under a name the binary does not know. Adding a model
-- is a deliberate act shipped in one commit: a new Rust registry entry, plus a
-- migration that widens this CHECK (small-table rebuild, rule 8 in CLAUDE.md)
-- and INSERTs the seed row.
--
-- Append-only by trigger: a row is a fact about what vectors may exist in
-- Qdrant. Updating a dim in place would reinterpret every stored vector;
-- deleting a row would orphan a collection with no record of what produced it.

CREATE TABLE IF NOT EXISTS embedding_models (
    id  TEXT PRIMARY KEY CHECK (id IN (
        'qwen3-embedding-0.6b',
        'qwen3-embedding-4b',
        'qwen3-embedding-8b'
    )),
    dim INTEGER NOT NULL CHECK (dim > 0)
);

INSERT OR IGNORE INTO embedding_models (id, dim) VALUES
    ('qwen3-embedding-0.6b', 1024),
    ('qwen3-embedding-4b',   2560),
    ('qwen3-embedding-8b',   4096);

CREATE TRIGGER IF NOT EXISTS embedding_models_no_update
BEFORE UPDATE ON embedding_models
BEGIN
    SELECT RAISE(ABORT, 'embedding_models is append-only; a changed dim reinterprets stored vectors');
END;

CREATE TRIGGER IF NOT EXISTS embedding_models_no_delete
BEFORE DELETE ON embedding_models
BEGIN
    SELECT RAISE(ABORT, 'embedding_models is append-only; deleting a model orphans its collections');
END;


-- ============================================================
-- Project metadata
-- ============================================================

CREATE TABLE IF NOT EXISTS projects (
    guid TEXT PRIMARY KEY CHECK (length(guid) = 32)
);


-- ============================================================
-- Source files
-- ============================================================

CREATE TABLE IF NOT EXISTS project_files (
    project_guid TEXT NOT NULL,

    path TEXT NOT NULL CHECK (
        length(path) > 0     AND
        path NOT GLOB '/*'   AND
        path NOT GLOB '*//*' AND
        path NOT GLOB '*\\*'
    ),

    sha256               TEXT    NOT NULL COLLATE NOCASE CHECK (length(sha256) = 64),
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
    -- change"; these answer "did the thing that derives from it change" — four
    -- axes because the four rebuilds cost different things, and one shared
    -- version would price every cheap fix at the most expensive rebuild:
    --
    --   chunks_version    — the slicing ALGORITHM (CHUNKS_DERIVATION_VERSION):
    --                       node selection, left-extension, the gap pass.
    --                       Bumping it re-slices, re-embeds and re-upserts.
    --   chunker_id        — the TOKENIZER the slicer measured windows with
    --                       (spec.tokenizer_hf_id). The algorithm version cannot
    --                       see it, and a tokenizer change moves every boundary
    --                       behind a matching hash. Deliberately NOT the window:
    --                       the window is config, and folding it in would price
    --                       every window experiment at a corpus-wide re-slice.
    --   embedded_model_id — WHOSE vectors exist for this file. The axis that
    --                       makes a model swap self-healing: flip [model].id and
    --                       the equality check below stops matching, so the next
    --                       ordinary run (or `mindex-index --vectors-only`, a
    --                       pure re-embed over stored chunks) rebuilds it.
    --   symbols_version   — the tags extraction (SYMBOLS_DERIVATION_VERSION),
    --                       pure CPU.
    --
    -- All nullable, and NULL never matches an equality check — "derived by an
    -- unknown version" self-heals on the next ordinary run with no manual step.
    chunks_version    TEXT,
    chunker_id        TEXT,
    embedded_model_id TEXT REFERENCES embedding_models (id),
    symbols_version   TEXT,

    PRIMARY KEY (project_guid, path),

    FOREIGN KEY (project_guid)
        REFERENCES projects (guid)
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

    code         TEXT    NOT NULL,
    -- UNIQUE, not merely indexed: the point id in every per-model collection,
    -- and the display query resolves winners by it alone.
    qdrant_guid  TEXT    NOT NULL UNIQUE CHECK (length(qdrant_guid) = 32),

    -- The chunk's own token count, measured by the tokenizer chunker_id names
    -- at slicing time. What makes "which chunks exceed model X's window" a
    -- query instead of a corpus-wide re-tokenization.
    tokens       INTEGER NOT NULL CHECK (tokens > 0),

    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_column   INTEGER NOT NULL,

    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deleted')),

    FOREIGN KEY (project_guid, file_path)
        REFERENCES project_files (project_guid, path)
        ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_project_file_chunks_lookup
ON project_file_chunks (project_guid, file_path, status);

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
    path         TEXT    NOT NULL,

    old_status   TEXT,                          -- NULL on the initial insert
    new_status   TEXT    NOT NULL,
    retry_count  INTEGER NOT NULL,
    at           INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_status_log_file
ON project_file_status_log (project_guid, path, at);

CREATE TRIGGER IF NOT EXISTS project_files_status_log_insert
AFTER INSERT ON project_files
BEGIN
    INSERT INTO project_file_status_log
        (project_guid, path, old_status, new_status, retry_count)
    VALUES (NEW.project_guid, NEW.path, NULL, NEW.status, NEW.retry_count);
END;

-- Log only meaningful changes (status or retry_count); skip idempotent no-ops.
CREATE TRIGGER IF NOT EXISTS project_files_status_log_update
AFTER UPDATE OF status ON project_files
WHEN NEW.status <> OLD.status OR NEW.retry_count <> OLD.retry_count
BEGIN
    INSERT INTO project_file_status_log
        (project_guid, path, old_status, new_status, retry_count)
    VALUES (NEW.project_guid, NEW.path, OLD.status, NEW.status, NEW.retry_count);
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
-- measured — 87.5% of the table serving one tool nobody called. Unlike chunks,
-- symbol rows have no Qdrant counterpart, so they are HARD-deleted inline —
-- no soft-delete/GC cycle. Invariant: a file's symbol rows always parallel
-- its chunk set — every transaction that marks a file's chunks 'deleted'
-- deletes its symbols in the same statement batch, and inserts happen in the
-- same transaction as chunk inserts. ON DELETE RESTRICT guards the ordering
-- (a project_files row can only be pruned after its symbols are gone).

CREATE TABLE IF NOT EXISTS project_file_symbols (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,

    project_guid TEXT    NOT NULL,
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

    FOREIGN KEY (project_guid, file_path)
        REFERENCES project_files (project_guid, path)
        ON DELETE RESTRICT
);

-- Symbol lookup (the /symbols endpoint).
CREATE INDEX IF NOT EXISTS idx_project_file_symbols_lookup
ON project_file_symbols (project_guid, name);

-- Per-file replacement on reindex / delete, and the read behind `outline`.
CREATE INDEX IF NOT EXISTS idx_project_file_symbols_file
ON project_file_symbols (project_guid, file_path);


-- ============================================================
-- Research run journal
-- ============================================================
-- A /research run would otherwise exist only for the lifetime of its SSE
-- connection. This table is the trace, and it is also the corpus: question +
-- report + which prompt and model produced them is exactly what a measurement
-- harness needs, and it accumulates for free from ordinary use.
--
-- DELIBERATELY NOT foreign-keyed to project_files, and deliberately not indexed
-- into Qdrant: a research report is not a file — a project_files row would need
-- a language and a path that pass the CHECKs, and would then poison POST /drift
-- (the working-tree manifest can never contain it, so every sweep would classify
-- it `orphaned` forever). project_guid is a plain column: runs for a deleted
-- project simply outlive it, which is correct for a measurement record.
--
-- Writes are best-effort and happen after the report has already been streamed,
-- so a failure here costs a row and nothing else. The `*_json` columns are JSON
-- arrays/objects read whole by a human or a notebook; nothing joins on them.

CREATE TABLE IF NOT EXISTS research_runs (
    id                    TEXT    NOT NULL PRIMARY KEY,
    project_guid          TEXT    NOT NULL,
    created_at            INTEGER NOT NULL DEFAULT (unixepoch()),

    -- Per-project monotonic short id: what a human types and the keyset cursor
    -- the paginated list walks. GC reaps the OLDEST rows, so MAX(seq) survives
    -- a sweep and the sequence stays monotonic.
    seq                   INTEGER NOT NULL,
    -- NULL = pinned, never reaped. Otherwise created_at + [research].retention_days.
    expires_at            INTEGER,
    -- The earlier runs whose reports were injected into this one's transcript.
    context_run_ids_json  TEXT    NOT NULL DEFAULT '[]',
    -- The report's own first ATX heading, extracted server-side at journalling
    -- time; NULL when absent. Readers fall back to a title derived from `question`.
    title                 TEXT,

    -- What kind of run this row is. 'research' answers a question; 'challenge'
    -- attacks a stored report's claims. A column rather than a second table
    -- because a challenge IS a run and every reader would otherwise need a UNION.
    kind                  TEXT    NOT NULL DEFAULT 'research'
        CHECK (kind IN ('research', 'challenge')),
    -- The run this challenge attacked; NULL on ordinary research runs. NO
    -- foreign key, deliberately: RESTRICT would refuse to delete a run that was
    -- ever challenged, CASCADE would erase the challenge record along with its
    -- subject. A dangling id simply means the subject is gone.
    challenged_run_id     TEXT,
    -- The challenge's overall verdict; NULL on research runs — and on a
    -- challenge whose verdict turn produced nothing parseable, which readers
    -- must treat as "challenged, inconclusive", never as an acquittal.
    challenge_verdict     TEXT
        CHECK (
            challenge_verdict IS NULL
            OR challenge_verdict IN ('confirmed', 'disputed', 'refuted')
        ),
    claims_total          INTEGER,
    claims_confirmed      INTEGER,
    claims_disputed       INTEGER,
    claims_refuted        INTEGER,

    question              TEXT    NOT NULL,
    model                 TEXT    NOT NULL,
    -- The Ollama blob digest of `model` at run time; NULL when the catalog had
    -- not seen the model yet. `model` is a mutable name; the digest is what
    -- makes two runs actually comparable.
    model_digest          TEXT,
    model_details_json    TEXT,
    prompt_version        TEXT    NOT NULL,
    effort                TEXT    NOT NULL,
    seed                  INTEGER,
    temperature           REAL,
    top_p                 REAL,

    granted_seconds       INTEGER NOT NULL,
    granted_tokens        INTEGER NOT NULL,
    granted_steps         INTEGER NOT NULL,
    granted_search_top_k  INTEGER NOT NULL,
    granted_context_fraction  REAL,
    granted_report_words      INTEGER,
    granted_report_sections   INTEGER,
    granted_evidence_width    INTEGER,
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

    -- How far the index moved underneath the run. Nothing serializes /research
    -- against indexing, deliberately; the run reports what moved instead.
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
    -- The scope as told to the model (rendered prose); NULL — not '{}' — for an
    -- unscoped run, so "no scope" and "an empty scope" stay distinguishable.
    scope_json            TEXT,
    -- The scope as data (serialized include/exclude): what lets a challenge run
    -- re-inhabit its subject's exact scope. NULL on unscoped runs.
    scope_spec_json       TEXT,
    -- The report was written by the SERVER because the report window expired.
    forced_synthesis      INTEGER NOT NULL,
    report_window_ms      INTEGER NOT NULL,
    report_elapsed_ms     INTEGER NOT NULL,

    -- The citation-repair pass's counters; all four NULL together when the
    -- draft needed no correction.
    revalidation_draft_unverified INTEGER,
    revalidation_draft_path_only  INTEGER,
    revalidation_draft_stale      INTEGER,
    revalidation_steps            INTEGER,
    -- The sufficiency turn's own ANSWERED/UNANSWERED list, verbatim; NULL when
    -- the turn was skipped or came back empty.
    sufficiency_verdict           TEXT,

    -- Which embedding model the baselines in research_run_files were read
    -- under — a canonical id from `embedding_models`. What keeps stored runs
    -- interpretable across an embedder swap.
    embedder_model_id     TEXT,
    server_version        TEXT,
    -- Wall-clock admission time; `created_at` is the INSERT, i.e. the run's END.
    started_at            INTEGER,

    report                TEXT    NOT NULL
);

-- The two axes anything ever asks by: one project's history, newest first.
CREATE INDEX IF NOT EXISTS idx_research_runs_project
ON research_runs (project_guid, created_at);

-- "How did prompt X do against model Y" — the bake-off query.
CREATE INDEX IF NOT EXISTS idx_research_runs_model
ON research_runs (model, prompt_version, created_at);

-- The keyset page. UNIQUE is the backstop: a duplicated cursor value makes a
-- keyset page repeat or skip rows; a refused insert costs a `warn!` and a row,
-- which is the journal's pre-existing best-effort contract.
CREATE UNIQUE INDEX IF NOT EXISTS idx_research_runs_seq
ON research_runs (project_guid, seq);

-- The GC sweep. Partial: pinned rows are exactly the ones with no expiry, and
-- an index that does not contain them cannot lead a sweep to one.
CREATE INDEX IF NOT EXISTS idx_research_runs_expiry
ON research_runs (expires_at)
WHERE expires_at IS NOT NULL;

-- "Which challenges attack this run" — the trust-status join. Partial:
-- ordinary research rows have nothing to say here.
CREATE INDEX IF NOT EXISTS idx_research_runs_challenged
ON research_runs (challenged_run_id)
WHERE challenged_run_id IS NOT NULL;


-- ============================================================
-- What each stored run was written against
-- ============================================================
-- The persistent form of `Evidence.baseline_sha`: the index's hash for a file
-- at the moment the run first probed it. Staleness is a read-time comparison
-- against project_files. DELIBERATELY per-path, not a global project-version
-- counter (which would mark every run stale on every save), and `path` carries
-- NO foreign key: RESTRICT would brake the code channel's GC, CASCADE would
-- delete the baseline along with the file and a run whose evidence vanished
-- would read as FRESH — the one verdict that is certainly wrong. "The file left
-- the index" is a *result* this table exists to produce; a missing join row
-- reads as `removed`. Do not "fix" the missing FK.
--
-- run_id, by contrast, IS foreign-keyed, ON DELETE CASCADE: these rows own
-- nothing outside SQLite.
CREATE TABLE IF NOT EXISTS research_run_files (
    run_id TEXT NOT NULL,
    path   TEXT NOT NULL,
    -- COLLATE NOCASE to match project_files.sha256, so the comparison cannot
    -- fail on hex case alone.
    sha256 TEXT NOT NULL COLLATE NOCASE CHECK (length(sha256) = 64),

    PRIMARY KEY (run_id, path),

    FOREIGN KEY (run_id)
        REFERENCES research_runs (id)
        ON DELETE CASCADE
);


-- ============================================================
-- What each run's tools actually showed the model
-- ============================================================
-- The persistent form of `Evidence`'s spans — the one input of `check_citations`
-- that research_run_files does not carry; with it, offline re-verification is a
-- pure function over stored data. NOT folded into research_run_files: the row
-- sets differ (a path shown without a baseline is still a path whose citations
-- verify). spans_json is a JSON array of [start_line, end_line] pairs; a path
-- shown with no usable span stores '[]', which is the path_only verdict's input.
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
-- report citing the same span three times made three claims. The verdict and
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
-- code a call returned is in the index, and a copy here would dwarf the corpus
-- while going stale against it. `n` is the wire's step number (gaps by
-- construction: checkpoint turns consume a number without emitting a step).
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


-- ============================================================
-- Git history (the second content channel)
-- ============================================================
-- The working tree says what the code IS; these two tables say why it became
-- that way. NOT MODELLED AS project_files ROWS (a commit has no path or
-- language that passes the CHECKs, and would poison POST /drift forever);
-- living in their own tables also keeps commit rows out of the /search
-- candidate set BY CONSTRUCTION. HARD DELETE, not soft: these rows own nothing
-- outside SQLite (inverts if commit messages ever gain vectors). SYNC IS SET
-- RECONCILIATION: a sha IS the hash of its own content, so the client posts
-- the reachable shas and the server inserts what it lacks and deletes what the
-- client did not name — force-push and rebase are just reconciliations in
-- which many shas orphan at once.

CREATE TABLE IF NOT EXISTS project_commits (
    project_guid TEXT    NOT NULL,
    -- 40 hex (SHA-1) or 64 (SHA-256 repositories). Case is normalized to
    -- lowercase by the client; the server validates the alphabet at the edge.
    sha          TEXT    NOT NULL CHECK (length(sha) IN (40, 64)),

    author_name  TEXT    NOT NULL,
    author_email TEXT    NOT NULL,
    -- Both timestamps, because a rebase moves one and not the other.
    authored_at  INTEGER NOT NULL,
    committed_at INTEGER NOT NULL,
    parent_count INTEGER NOT NULL CHECK (parent_count >= 0),

    subject      TEXT    NOT NULL CHECK (length(subject) > 0),
    -- '' rather than NULL: a commit with no body is a fact, not a missing value.
    body         TEXT    NOT NULL,

    PRIMARY KEY (project_guid, sha),

    FOREIGN KEY (project_guid)
        REFERENCES projects (guid)
        ON DELETE CASCADE
);

-- "What changed recently in this project", and the ordering every listing uses.
CREATE INDEX IF NOT EXISTS idx_project_commits_recent
ON project_commits (project_guid, committed_at);

-- The paths one commit touched. THE `path` COLUMN IS DELIBERATELY NOT FOREIGN-
-- KEYED to project_files: a commit legitimately names paths deleted years ago,
-- paths .mindex excludes, and languages the enum does not carry. RESTRICT would
-- refuse the insert, CASCADE would silently erase history on GC. The join into
-- the code channel is SOFT, by equality, and any tool reading these rows must
-- be able to say "this path is not in the index".
CREATE TABLE IF NOT EXISTS project_commit_paths (
    project_guid TEXT NOT NULL,
    sha          TEXT NOT NULL,

    -- Same shape as project_files.path: repo-relative, forward slashes.
    path         TEXT NOT NULL CHECK (
        length(path) > 0
        AND path NOT GLOB '/*'
        AND path NOT GLOB '*//*'
        AND path NOT GLOB '*\*'
    ),

    change_type  TEXT NOT NULL CHECK (
        change_type IN ('added', 'modified', 'deleted', 'renamed', 'copied')
    ),
    -- The source path of a rename or a copy; NULL for every other change type.
    old_path     TEXT,

    PRIMARY KEY (project_guid, sha, path),

    FOREIGN KEY (project_guid, sha)
        REFERENCES project_commits (project_guid, sha)
        ON DELETE CASCADE
);

-- The one axis file_history asks by: "which commits touched this path".
CREATE INDEX IF NOT EXISTS idx_project_commit_paths_path
ON project_commit_paths (project_guid, path);
