-- ============================================================
-- mindex schema — migration 3: TOML and YAML in the language CHECK
-- ============================================================
-- Adds 'toml' and 'yaml' to the programming_language CHECK on project_files.
-- v1.0.0_schema.sql carries the same widened list, so a cold start creates the
-- table correctly in one step and this migration rebuilds it into an identical
-- shape; the two must be kept in step.
--
-- THIS IS THE FIRST MIGRATION THAT IS NOT PURELY ADDITIVE, and the reason is
-- SQLite's: a CHECK constraint is part of the CREATE TABLE text and there is no
-- ALTER TABLE that widens one. Editing v1.0.0_schema.sql alone would have been
-- silent rather than wrong — the startup filter is `version > user_version`, so
-- a database already stamped at 1 or 2 never re-reads that file, and the first
-- .toml upload would fail its CHECK as a 500 with nothing explaining why.
--
-- The rebuild is SQLite's own procedure, in its order: create the replacement
-- under a temporary name, copy, DROP the original, rename the replacement into
-- place. The order is load-bearing and the tempting inversion is wrong — renaming
-- the original out of the way first rewrites the REFERENCES clauses in
-- project_file_chunks and project_file_symbols to name project_files_old, so both
-- children follow the table being discarded instead of adopting its replacement,
-- and PRAGMA legacy_alter_table does not prevent it (it spares trigger and view
-- bodies, never foreign keys). Dropping first leaves those clauses untouched,
-- naming a table that does not exist for the one statement it takes to recreate it.
--
-- This batch therefore runs with foreign-key enforcement OFF, which is what
-- `SQLite3Pool::migration_transaction` exists for — the pragma is a silent no-op
-- inside a transaction, so it cannot live in this file. Without it the DROP is
-- refused outright: both children declare ON DELETE RESTRICT, and a DROP performs
-- an implicit delete. `apply_pending_migrations` runs PRAGMA foreign_key_check
-- before it stamps user_version, so the suspended check is paid in full, once,
-- while the whole batch can still roll back.
--
-- DROP TABLE takes the eight triggers on project_files with it — a trigger belongs
-- to the table it was created on — so they are recreated at the end, verbatim from
-- v1.0.0_schema.sql, where they are also documented. Re-running that file
-- afterwards is a no-op, since every CREATE TRIGGER there is IF NOT EXISTS.
--
-- Idempotent, like every migration: a second run rebuilds the table again into
-- the same shape. That costs a copy and changes nothing, which is what
-- `every_migration_sql_is_idempotent` requires. The leading DROP ... IF EXISTS
-- statements are what make the second run start from a known state rather than
-- inserting into a table it already filled.


-- Not IF NOT EXISTS: a second run must start from an empty replacement rather than
-- insert a second copy of every row into one this migration already filled.
DROP TABLE IF EXISTS project_files_new;


-- Identical to project_files in v1.0.0_schema.sql except for the two new values
-- in the programming_language CHECK. The column comments are not repeated here;
-- that file remains the place they are documented.
CREATE TABLE project_files_new (
    project_guid         TEXT    NOT NULL,
    model_id             TEXT    NOT NULL,

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

    chunks_version    TEXT,
    symbols_version   TEXT,

    PRIMARY KEY (project_guid, model_id, path),

    FOREIGN KEY (project_guid, model_id)
        REFERENCES projects (guid, model_id)
        ON DELETE CASCADE
);

-- Columns named explicitly: `SELECT *` would bind by position and go wrong in
-- silence the moment either table's column order stops matching.
INSERT INTO project_files_new (
    project_guid,
    model_id,
    path,
    sha256,
    programming_language,
    status,
    retry_count,
    status_updated_at,
    chunks_version,
    symbols_version
)
SELECT
    project_guid,
    model_id,
    path,
    sha256,
    programming_language,
    status,
    retry_count,
    status_updated_at,
    chunks_version,
    symbols_version
FROM project_files;


DROP TABLE project_files;

ALTER TABLE project_files_new RENAME TO project_files;


-- ============================================================
-- The eight triggers, recreated against the new table
-- ============================================================
-- Verbatim from v1.0.0_schema.sql; see there for what each one enforces and why
-- the status machine is a trigger family rather than a column CHECK.

CREATE TRIGGER IF NOT EXISTS project_files_status_insert_guard
BEFORE INSERT ON project_files
WHEN NEW.status NOT IN ('just_uploaded', 'indexing')
BEGIN
    SELECT RAISE(ABORT, 'illegal initial project_files.status (must be just_uploaded or indexing)');
END;

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

CREATE TRIGGER IF NOT EXISTS project_files_status_log_insert
AFTER INSERT ON project_files
BEGIN
    INSERT INTO project_file_status_log
        (project_guid, model_id, path, old_status, new_status, retry_count)
    VALUES (NEW.project_guid, NEW.model_id, NEW.path, NULL, NEW.status, NEW.retry_count);
END;

CREATE TRIGGER IF NOT EXISTS project_files_status_log_update
AFTER UPDATE OF status ON project_files
WHEN NEW.status <> OLD.status OR NEW.retry_count <> OLD.retry_count
BEGIN
    INSERT INTO project_file_status_log
        (project_guid, model_id, path, old_status, new_status, retry_count)
    VALUES (NEW.project_guid, NEW.model_id, NEW.path, OLD.status, NEW.status, NEW.retry_count);
END;

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
