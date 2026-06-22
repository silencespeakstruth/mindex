-- ============================================================
-- Defense-in-depth validation triggers
-- ============================================================
-- The request layer (backend::v0::validate) is the primary validator and rejects
-- bad input as a 400 before it reaches SQLite. These triggers are the last line of
-- defense: they enforce the same shape invariants the API can't be the *only* guard
-- for, raising SQLITE_CONSTRAINT_TRIGGER (surfaced as a 500) if a bug ever lets bad
-- data through. They use triggers, not tightened column CHECKs, because SQLite cannot
-- ALTER a CHECK onto an existing table and the schema is created in v0.1.0 — triggers
-- are the only additive mechanism (same reasoning as the v0.2.0 status machine).

-- A stored sha256 must be 64 hexadecimal characters. v0.1.0 already CHECKs the length;
-- this adds the hex constraint (a non-hex char would otherwise pass the length CHECK).
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
