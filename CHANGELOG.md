# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every component ships under one version: the server, `mindex-index`, `mindex-watch`,
the `.mindex` parser, both MCP servers and the VS Code extension. A component with no
changes of its own is still released, so "which version am I running" has one answer.

## [Unreleased]

### Added

- **TOML and YAML are indexed** (`tree-sitter-toml-ng`, `tree-sitter-yaml`), sliced by
  the ordinary AST walk like JSON. Neither grammar ships a tags query, so they
  contribute no symbols. `Cargo.toml`, `config.example.toml`, `docker-compose*.yml`
  and every `pyproject.toml` were previously dropped in silence: the three extension
  maps carried no entry for them, which is the "silently skipped file" failure the
  Languages checklist exists to catch.
  - **Migration 3** (`v1.1.0_toml_yaml_languages.sql`) widens the
    `programming_language` CHECK on `project_files`. It is the first migration that is
    not purely additive — SQLite cannot alter a CHECK, so the table is rebuilt — and
    the first to need `SQLite3Pool::migration_transaction`, which suspends foreign-key
    enforcement for the rebuild and verifies the result with `PRAGMA foreign_key_check`
    before stamping `user_version`. No manual step: an existing database upgrades on
    the next start, and the `.toml`/`.yml` files appear on the next indexing run.
  - The VS Code extension labels both: YAML gets devicon's mark, TOML a codicon —
    devicon draws none, the second language after `sql` for which that is true.
  - CI and deployment YAML (`.github/**`, `deploy/**/*.yml`) are excluded from this
    repository's own `.mindex`.

## [1.0.1] — 2026-07-31

Adds a second channel for research — **git history** — and reworks the VS Code
extension's status and drift surfaces. Also fixes an indexing failure that could take
a whole pass down with it.

### Added

- **Git history channel.** `project_commits` and `project_commit_paths` (migration 2,
  `v1.1.0_git_history.sql`) record what each commit touched and why. Opt-in and
  metadata-only: no embeddings, no Qdrant points, no chunks, no derivation version.
  - `POST /v0/{guid}/history` reconciles a commit set; `DELETE /v0/{guid}/history`
    prunes it by `keep_last` and `older_than`, intersected.
  - `mindex-index --history` walks the refs named by `git_refs` in `.mindex`;
    `--history-only` runs that phase alone, `--git-ref` scopes it.
  - `file_history` is the research loop's tenth tool. Historical claims must still be
    anchored to a `path:start-end` with the sha named in prose — a sha is
    content-addressed and needs no server-side citation grammar.
- **`git_refs:`** as a `.mindex` key.
- **VS Code — language marks.** Every language is drawn with its official mark
  (devicon, vendored at build time) in the Ask view's filters and the status panel's
  inventory and failed lists.
- **VS Code — Sync all.** One action in the Drift view that reindexes every stale and
  missing file and drops every orphan from the index. Present only while there is
  drift to clear.
- **VS Code — inline reindex progress**, in the Drift view itself rather than only in
  a corner notification, and it reports the server's own in-flight work as well as
  this window's uploads.
- **VS Code — `mindex.statusPollSeconds`** (default 30, `0` disables): a background
  health poll, which is what lets the Ask form stop offering work the server cannot
  currently do.

### Changed

- **The slicer cuts on token boundaries where a line boundary does not exist**, and
  both slicers clamp their configured window below what Qdrant can store.
- **VS Code — the Ask form is gated on server health.** Research disables itself when
  the server's Ollama goes away; the whole form disables when a *required* dependency
  does, and a run in flight is aborted rather than left to time out.
- **VS Code — the status panel reads as a dashboard**: health checks carry colour and
  an `optional` badge, the SQLite pool is one inline meter, and the failed-files card
  is hidden entirely when nothing has failed.
- `PROMPT_VERSION` is `1.1`. Research reports written under 1.0.0 and 1.0.1 were
  written under different instructions and are not directly comparable.

### Fixed

- **An oversized chunk could fail a whole indexing batch.** A file with no line
  boundaries — a minified JSON, one unwrapped paragraph of prose — produced a single
  chunk of unbounded size, which Qdrant refuses above 1 048 576 multivector elements
  and which exhausts the embedder's GPU memory before that. Several hundred innocent
  files in the same pass went unindexed, and reruns failed in the same place.
- **VS Code: a reindex against a busy server silently did nothing.** The server
  answers `200` with claimed files simply absent from the response, which is
  indistinguishable from a hash-skip — so the upload finished instantly and reported
  the files unchanged at the moment it had in fact been refused. The view now shows
  the server's claims, refuses to start against them, and the summary tells the two
  apart.
- **VS Code: concurrent reindex runs.** A second press started a second run over the
  same paths; their drift checks then raced, and the view could settle showing
  just-indexed files as still stale.
- **VS Code: `mindex.noVerify` could not be reached.** The client read `mindex.caCert`
  with an unguarded `readFileSync` in its constructor, so a path naming a file absent
  from this machine threw at activation — every command dead, and the one setting that
  would have connected anyway unusable, because the read that failed came first. The
  CA is now read outside the constructor, a failure is a warning naming the path, and
  `noVerify` overrides `caCert` rather than queueing behind it.
- **VS Code: TLS settings now ride on each request, not only on the shared agent.**
  With `http.proxySupport` at its default `"override"` the extension host may
  substitute its own proxy agent and discard ours — taking `rejectUnauthorized` with
  it. That is what made both settings look inert behind a corporate proxy.

### Upgrading

- **The database migrates in place.** Migration 2 is additive — two new tables — and
  was verified non-destructive against a copy of a real 1.0.0 database. Nothing needs
  reindexing.
- **`.mindex` files using `git_refs:` require 1.0.1 tooling.** The parser rejects
  unknown keys by design, so a 1.0.0 `mindex-index` or `mindex-watch` will fail on
  one. Files that do not use the key are unaffected in both directions.
- The git history channel stays off until `--history` is passed.

## [1.0.0] — 2026-07-30

First release. An HTTPS API server that indexes repositories locally and answers
semantic search, exact symbol lookup and research questions over them, with vectors in
a local Qdrant, metadata in a local SQLite file and embeddings from a local BGE-M3
server.

### Added

- **The server**: `tree-sitter` AST chunking, BGE-M3 multi-vector embeddings
  (dense/sparse/ColBERT), RRF fusion with a ColBERT rerank. 21 programming languages
  plus Markdown.
- **Wire contracts**: RFC 7807 errors with namespaced machine codes, OpenAPI 3.1 at
  `/api-docs/openapi.json` with Swagger UI, OpenMetrics at `/metrics`. All three are
  snapshot-tested.
- **`POST /v0/{guid}/research`**: a local Ollama model runs an investigation over the
  index and streams back a cited Markdown report. Citations are provenance-checked
  server-side before the report is shipped.
- **`POST /drift`**: read-only comparison of a posted `path → sha256` manifest against
  the index, classifying every file `stale` / `missing` / `orphaned` / `indexing`.
- **Tools**: `mindex-index`, `mindex-watch`, `mindex-search.sh`, the `mindex` and
  `scout` MCP servers, and a VS Code extension.

[Unreleased]: https://github.com/silencespeakstruth/mindex/compare/v1.0.1...HEAD
[1.0.1]: https://github.com/silencespeakstruth/mindex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/silencespeakstruth/mindex/releases/tag/v1.0.0
