-- ============================================================
-- project_files.status state machine + transition audit log
-- ============================================================
-- The status CHECK in v0.1.0 validates the *value*; these triggers validate the
-- *transition*. Legal moves:
--   * any → 'indexing'                              (start / reindex / retry)
--   * 'indexing' → 'indexed' | 'cancelled' | 'failed'   (terminal only from work)
-- Everything else (e.g. indexed→failed, failed→indexed, just_uploaded→indexed)
-- is rejected with SQLITE_CONSTRAINT_TRIGGER. Idempotent 'indexing'→'indexing'
-- is allowed (concurrent upserts); other self-loops are not.

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
