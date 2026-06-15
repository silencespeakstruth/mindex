-- ============================================================
-- Project metadata
-- ============================================================
-- A project is uniquely identified by (guid, model_id).
-- Different embedding models may index the same project independently.

CREATE TABLE IF NOT EXISTS projects (
    guid TEXT NOT NULL CHECK (length(guid) = 32),
    model_id TEXT NOT NULL CHECK (
        model_id IN ('BAAI/bge-m3')
    ),

    PRIMARY KEY (guid, model_id)
);



-- ============================================================
-- Source files
-- ============================================================
-- Files belonging to a project.
-- Paths are stored as normalized relative paths.

CREATE TABLE IF NOT EXISTS project_files (
    project_guid TEXT NOT NULL,
    model_id TEXT NOT NULL,

    path TEXT NOT NULL CHECK (
        length(path) > 0         AND
        path NOT GLOB '/*'       AND
        path NOT GLOB '*//*'     AND
        path NOT GLOB '*\\*'
    ),

    sha256 TEXT NOT NULL COLLATE NOCASE CHECK (length(sha256) = 64),

    programming_language TEXT NOT NULL CHECK (
        programming_language IN ('rust')
    ),

    PRIMARY KEY (project_guid, model_id, path),

    FOREIGN KEY (project_guid, model_id)
        REFERENCES projects (guid, model_id)
        ON DELETE CASCADE
);



-- ============================================================
-- Code chunks
-- ============================================================
-- A file is split into chunks for indexing and retrieval.

CREATE TABLE IF NOT EXISTS project_file_chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    project_guid TEXT NOT NULL,
    file_path TEXT NOT NULL,
    model_id TEXT NOT NULL,

    code TEXT NOT NULL,
    qdrant_guid TEXT NOT NULL CHECK (length(qdrant_guid) = 32),

    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_column   INTEGER NOT NULL,

    FOREIGN KEY (project_guid, model_id, file_path)
        REFERENCES project_files (
            project_guid,
            model_id,
            path
        )
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_project_file_chunks_lookup
ON project_file_chunks (
    project_guid,
    model_id,
    file_path
);

CREATE INDEX IF NOT EXISTS idx_project_file_chunks_lookup_qdrant
ON project_file_chunks (qdrant_guid);
