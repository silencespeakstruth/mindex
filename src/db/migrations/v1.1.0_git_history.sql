-- ============================================================
-- mindex schema — migration 2: the git history channel
-- ============================================================
-- Additive and idempotent, like every migration: two new tables and their
-- indexes, all `IF NOT EXISTS`, touching nothing that already exists. That is
-- what lets it reach a database already stamped at version 1 without a rebuild —
-- the git channel is the first schema change that had to, since by the time it
-- landed 1.0.0's schema was in use.
--
-- Statement order is load-bearing: project_commit_paths names project_commits in
-- a foreign key, so that table must exist first.


-- ============================================================
-- Git history (the second content channel)
-- ============================================================
-- The working tree says what the code IS; these two tables say why it became
-- that way and what moved around it. A commit is the unit, its message is the
-- payload, and its list of touched paths is the join back into the code
-- channel. Patches are deliberately absent: a patch is mostly context lines the
-- chunk table already holds, and the informative +/- fragment needs the
-- surrounding code that /search and read_chunks retrieve better.
--
-- NOT MODELLED AS project_files ROWS, for the same reason research_runs is not
-- (see the comment above it, which this repeats because the trap is the same):
-- a commit has no repo-relative path that passes the path CHECK and no
-- programming_language that passes that CHECK, and a project_files row it does
-- not deserve would poison POST /drift — the working-tree manifest can never
-- contain a commit, so every sweep would report it `orphaned` forever,
-- `mindex-index --check` would exit non-zero and the watcher would keep trying
-- to delete it. Living in their own tables also means commit rows are excluded
-- from the /search candidate set BY CONSTRUCTION rather than by a filter
-- somebody has to remember to write, which is why this is two tables and not a
-- `channel` column on project_file_chunks.
--
-- HARD DELETE, not soft. Chunks are soft-deleted because a chunk owns a Qdrant
-- vector that must outlive its SQLite row until GC confirms the delete. These
-- rows own nothing outside SQLite, so their lifecycle is project_file_symbols'
-- (delete and be done), not project_file_chunks'. That inverts if commit
-- messages ever gain vectors of their own.
--
-- SYNC IS SET RECONCILIATION, and that is the whole design. A sha IS the hash
-- of its own content, so there is no equivalent of the file channel's
-- "same path, different bytes": the client posts the shas reachable from the
-- refs it tracks, the server inserts what it lacks and deletes what the client
-- did not name. Force-push, rebase and history rewrite are therefore not
-- special cases at all — each is one reconciliation in which many shas orphan
-- at once.
--
-- model_id is here only because projects' primary key is composite; nothing
-- about a commit depends on the embedding model today. It starts meaning
-- something if commit messages are ever embedded.

CREATE TABLE IF NOT EXISTS project_commits (
    project_guid TEXT    NOT NULL,
    model_id     TEXT    NOT NULL,
    -- 40 hex (SHA-1) or 64 (SHA-256 repositories). Case is normalized to
    -- lowercase by the client; the server validates the alphabet at the edge.
    sha          TEXT    NOT NULL CHECK (length(sha) IN (40, 64)),

    author_name  TEXT    NOT NULL,
    author_email TEXT    NOT NULL,
    -- Both timestamps, because a rebase moves one and not the other: authored_at
    -- is when the work was done, committed_at is when this sha came to exist.
    -- Reconciliation windows and "what changed recently" use committed_at.
    authored_at  INTEGER NOT NULL,
    committed_at INTEGER NOT NULL,
    -- Used to tell a merge from ordinary work. The client already drops
    -- generated merge messages, but a count here lets a later query ask.
    parent_count INTEGER NOT NULL CHECK (parent_count >= 0),

    subject      TEXT    NOT NULL CHECK (length(subject) > 0),
    -- '' rather than NULL: a commit with no body is a fact about the commit,
    -- not a missing value.
    body         TEXT    NOT NULL,

    PRIMARY KEY (project_guid, model_id, sha),

    FOREIGN KEY (project_guid, model_id)
        REFERENCES projects (guid, model_id)
        ON DELETE CASCADE
);

-- "What changed recently in this project", and the ordering every listing uses.
CREATE INDEX IF NOT EXISTS idx_project_commits_recent
ON project_commits (project_guid, model_id, committed_at);

-- The paths one commit touched. THE `path` COLUMN IS DELIBERATELY NOT FOREIGN-
-- KEYED to project_files, and that is not an oversight to tidy up later: a
-- commit legitimately names paths that were deleted years ago, paths .mindex
-- excludes, and paths in languages the enum does not carry. ON DELETE RESTRICT
-- would refuse the insert outright, and ON DELETE CASCADE would silently erase
-- history whenever prune_deleted_files ran. So the join into the code channel
-- is a SOFT join by equality, and any tool reading these rows must be able to
-- say "this path is not in the index" rather than returning nothing — the same
-- honesty outline's `indexed` flag exists for.
--
-- change_type and old_path are load-bearing for the same reason: without them a
-- lookup on a file that was moved returns no rows, which reads as "this file
-- has no history" when the truth is "its history is filed under its old name".
CREATE TABLE IF NOT EXISTS project_commit_paths (
    project_guid TEXT NOT NULL,
    model_id     TEXT NOT NULL,
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

    PRIMARY KEY (project_guid, model_id, sha, path),

    FOREIGN KEY (project_guid, model_id, sha)
        REFERENCES project_commits (project_guid, model_id, sha)
        ON DELETE CASCADE
);

-- The one axis file_history asks by: "which commits touched this path".
CREATE INDEX IF NOT EXISTS idx_project_commit_paths_path
ON project_commit_paths (project_guid, model_id, path);
