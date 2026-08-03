-- ============================================================
-- mindex schema — migration 6: symbols are definitions only
-- ============================================================
-- Drops project_file_symbols.role and the index that existed to select on it.
--
-- WHY. The table stored both halves of the tags query, definitions and
-- references, and the reference half was measured rather than assumed: 23 810
-- reference rows against 3 397 definitions — 87.5% of the table — serving one
-- model-facing tool, `callers`, which was called twice across twenty-five
-- recorded research runs at a 50% miss rate. The edges under it are lexical: a
-- reference row records a token in call position, not which definition it binds
-- to. On this repo the most-referenced names were `assert_eq` (1084), `clone`,
-- `Ok`, `unwrap`, `map` — several with exactly one definition in the tree — so a
-- file whose only content was a method named `map` collected 372 votes from
-- everyone else's `.map()`. Nothing aggregates usefully over that; separating a
-- core abstraction from a name shared with a language builtin is name
-- resolution, i.e. the LSP/SCIP wall this project has declined to climb. `grep`
-- answers "who uses this name" lexically and says so, which is the honest
-- version of what `callers` implied.
--
-- With references gone, `role` is a column whose every value is 'definition'.
--
-- THE DATA IS NOT MOVED BY THIS FILE. Symbol rows are wholly derived, so the
-- reference rows are removed by the mechanism that already exists for exactly
-- this: SYMBOLS_DERIVATION_VERSION went 1.0 → 1.1 in the same commit, and the
-- next ordinary `mindex-index` run (or `--symbols-only`, which is CPU-only)
-- replaces each file's rows in one transaction per file. This migration only
-- reshapes the table, and it carries surviving rows across verbatim: a row that
-- is still 'reference' when this runs is copied, and the reindex deletes it.
-- Filtering here instead would put the same rule in two places and let a
-- database that skipped the reindex look as though it had had one.
--
-- v1.0.0_schema.sql carries the same narrowed shape, so a cold start creates the
-- table correctly in one step and this migration rebuilds it into an identical
-- one; THE TWO MUST BE KEPT IN STEP, exactly as
-- v1.1.0_toml_yaml_languages.sql's header says of the language CHECK. Editing
-- that file alone would have been silent rather than wrong — the startup filter
-- is `version > user_version`, so a database already stamped at 1 never re-reads
-- it — and editing only this one would leave every migration re-run building an
-- index on a column that no longer exists, which is what
-- `every_migration_sql_is_idempotent` refuses.
--
-- The rebuild is SQLite's own procedure and its order is load-bearing — create
-- the replacement under a temporary name, copy with columns NAMED (SELECT *
-- binds by position and this rebuild changes the position of everything after
-- `kind`), DROP the original, rename. Renaming first would rewrite nothing here,
-- since no table references project_file_symbols, but the order is kept because
-- it is the shape every future rebuild should copy.
--
-- project_file_symbols is a CHILD of project_files with ON DELETE RESTRICT, and
-- DROP TABLE performs an implicit delete of the table being dropped rather than
-- of its parent, so this batch would survive with foreign keys on. It runs under
-- `SQLite3Pool::migration_transaction` anyway, because that is what the
-- MIGRATIONS list applies to every entry, and `apply_pending_migrations` pays
-- the suspended check back with one PRAGMA foreign_key_check before it stamps
-- user_version.
--
-- It carries no triggers, so unlike the project_files rebuild there is nothing
-- to recreate after the DROP.
--
-- Idempotent: the leading DROP ... IF EXISTS makes a second run start from an
-- empty replacement instead of inserting a second copy of every row, which is
-- what `every_migration_sql_is_idempotent` requires.


-- Not IF NOT EXISTS: a second run must start empty rather than append to a table
-- this migration already filled.
DROP TABLE IF EXISTS project_file_symbols_new;


-- Identical to project_file_symbols in v1.0.0_schema.sql except that `role` is
-- gone. The column comments are not repeated here; that file remains the place
-- they are documented.
CREATE TABLE project_file_symbols_new (
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

    -- Kept, and not a casualty of the reference half: for a definition the
    -- enclosing definition is what makes `Gc::collect` readable as such, and
    -- both `outline` and `/symbols` return it.
    parent_name  TEXT,
    parent_kind  TEXT,
    doc          TEXT,

    FOREIGN KEY (project_guid, model_id, file_path)
        REFERENCES project_files (project_guid, model_id, path)
        ON DELETE RESTRICT
);


-- Columns NAMED, never SELECT *: the source table has one more column than the
-- destination and everything after `kind` has shifted left by one, so a
-- positional copy would silently write start_line into end_line.
INSERT INTO project_file_symbols_new (
    id, project_guid, model_id, file_path, name, kind,
    start_line, end_line, start_column, end_column,
    parent_name, parent_kind, doc
)
SELECT
    id,
    project_guid,
    model_id,
    file_path,
    name,
    kind,
    start_line,
    end_line,
    start_column,
    end_column,
    parent_name,
    parent_kind,
    doc
FROM project_file_symbols;


DROP TABLE project_file_symbols;


ALTER TABLE project_file_symbols_new RENAME TO project_file_symbols;


-- Symbol lookup (the /symbols endpoint), minus the role component. Every read
-- that reaches this table now asks about definitions, so a column that says so
-- would be a constant in the key.
CREATE INDEX IF NOT EXISTS idx_project_file_symbols_lookup
ON project_file_symbols (project_guid, model_id, name);

-- Per-file replacement on reindex / delete, and the read behind `outline`.
CREATE INDEX IF NOT EXISTS idx_project_file_symbols_file
ON project_file_symbols (project_guid, model_id, file_path);

-- idx_project_file_symbols_parent is deliberately NOT recreated. It existed for
-- one query — `WHERE parent_name = ?`, the `callers` tool's `out` direction —
-- and that tool is gone. `parent_name` survives as a returned field, which needs
-- no index: every remaining read locates its rows by name or by file first.
DROP INDEX IF EXISTS idx_project_file_symbols_parent;
