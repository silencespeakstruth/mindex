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
    programming_language TEXT    NOT NULL CHECK (programming_language IN (
        'rust', 'python', 'javascript', 'typescript', 'tsx',
        'go', 'c', 'cpp', 'java', 'csharp', 'ruby', 'php', 'bash',
        'html', 'css', 'json', 'scala', 'haskell', 'ocaml', 'zig', 'sql'
    )),

    status TEXT NOT NULL DEFAULT 'just_uploaded' CHECK (
        status IN ('just_uploaded', 'indexing', 'indexed', 'cancelled', 'failed')
    ),
    retry_count       INTEGER NOT NULL DEFAULT 0,
    status_updated_at INTEGER NOT NULL DEFAULT (unixepoch()),

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
