# TODO / Known Issues / Assumptions

Living list of deferred work, accepted limitations, and design assumptions.
Architecture and invariants live in `.claude/CLAUDE.md`; this file is only what
is *not done* or *deliberately constrained*.

## Known issues / limitations

- **The `has_id` filter grows linearly with a project's active-chunk count** (it
  lists every candidate GUID per query). Fine for moderate projects; very large
  collections may want a stored Qdrant payload field (`project_guid`) + a `match`
  filter instead.
- **After `MAX_RETRIES` (3) failures a file stays `status='failed'`** and is no
  longer retried automatically. Surfaced via a WARN at startup and hourly
  (`worker::retry::warn_permanently_failed`), via `GET /projects/{guid}` (per-project
  `failed` count), and now as a proper dead-letter view: `GET
  /projects/{guid}/files?status=failed` lists the stuck files, and `POST
  /projects/{guid}/retry` requeues them (resets `retry_count`) without re-pushing
  content. A standing metric / alert is still future work.
- **Deleting all of a project's files leaves the project row + an empty Qdrant
  collection.** `DELETE /files` is scoped to files (the project persists so it can
  be re-indexed), so matching everything still leaves an empty collection behind.
  Use `DELETE /projects/{guid}` to remove the project and drop its collection.
- **`cancelled` files are never retried** by the worker (by design — the client
  gave up). They only revive on an explicit re-push (`cancelled → indexing` via the
  handler). A cancelled file never re-pushed keeps no vectors.
- **No *automatic* supersede-and-restart of a stale in-flight index.** `POST
  /projects/{guid}/cancel` (and the MCP `cancel_indexing` tool) now lets a client
  abort in-flight indexing for selected files — best-effort, transactional, moving
  matched `indexing` files to `cancelled` so the retry worker won't revive them — and
  the file can then be re-pushed on fresh content. What is *not* automated: detecting
  that an in-flight index has been superseded by a newer edit and restarting it
  without an explicit cancel+reindex from the client. The embed pass itself is still
  append-only (a cancel that lands mid-embed lets that pass finish wastefully; GC
  reclaims the result), so cancellation is a control-plane signal, not a hard abort of
  GPU work in progress.
- **No API authentication** (by design — internal service, TLS only). Revisit if
  ever exposed beyond a trusted network.
- **HTTP/3 is not implemented.** `run()` is HTTP/1.1 + HTTP/2; the `http3.rs`
  module name is aspirational.

## Future work

- **Server-side error-message localization.** The error contract is in place — every
  failure is RFC 7807 `application/problem+json` with a stable, namespaced `code`
  (`backend/error.rs`) plus structured `meta` for interpolation — but the server only
  emits English `title`/`detail`. A client localizes by keying on `code`. If
  server-driven translation is ever wanted, add a `code` → localized-template table
  (selected by `Accept-Language`); no code outside `ApiError::detail`/`title` changes.
- **Performance benchmarks for large-codebase indexing.** The "fast indexing"
  claim is so far unmeasured — no reproducible benchmark suite exists. Needed
  before quoting any numbers.
- **Cloud-GPU deployment template for the embedder.** Running `embedder/` on a
  remote GPU (and pointing mindex at it via `--model-server`) is a supported use
  case for machines without a local GPU, but there is no ready-made template
  (Dockerfile / deployment manifest) for it yet.
- **Languages awaiting upstream `tree-sitter ≥ 0.23` grammar crates** (older
  crates cause a native `links` conflict and cannot be added yet): `yaml`,
  `toml`, `kotlin`, `lua`, `elixir`, `nix`, `erlang`, `swift`. Add via the
  language-extensibility checklist in CLAUDE.md once the crates update.
- **Terminal search frontend (`tools/search/`) copy-to-clipboard buttons** for
  selecting result segments — left as a future iteration.
- **Multi-instance-safe indexing claim.** Concurrent same-file work is serialized by
  an *in-process* keyed lock (`IndexClaim` + `indexing_lock_key` in `handlers.rs`,
  keyed on `{guid}\0{model_id}\0{path}`): the `/index` handler claims a file for its
  whole prepare→embed→mark_indexed pipeline (contention → 429), **and the retry worker
  claims it too** before a sweep (skips if a live `/index` holds it). So the
  handler↔handler and handler↔worker races are both lock invariants, not merely a
  consequence of `--stuck-grace-mins` exceeding the longest request. That assumes a
  single mindex process (see Assumptions). To run more than one mindex process against
  one DB, replace the in-process set with a DB-level compare-and-swap claim — e.g. a
  conditional `… → indexing` update plus an epoch column checked at `mark_indexed`, so
  a superseded writer abandons rather than clobbering. Requires a schema migration.
- **A claim collision penalizes innocent files in the same batch.** When a multi-file
  `/index` batch hits 429 on *one* contended file, `post_index` recovers every
  already-prepared file in that batch to `failed` with `retry_count += 1` (it can't
  tell a contention 429 from a genuine prepare error — `prepare` returns an `ApiError`
  and `post_index` recovers on *any* `Err`). No corruption (the retry worker re-indexes
  them), but repeated collisions on the same co-batched file could burn its 3 retries
  and strand it in `failed`. Fix: have `prepare` signal contention distinctly (it
  already returns `ApiError::FileInFlight`, so `post_index` could branch on it) so the
  batch recovers those files **without** incrementing retry, or simply leaves them
  `indexing` for the worker.

## Assumptions

- **One embedding model at runtime** (`--model`, default `BAAI/bge-m3`). The
  `EmbeddingModel` enum is the extension point for more.
- **A single mindex process per database.** The per-file indexing mutual-exclusion
  (`IndexClaim`) is an in-process set in `RouterState`, so it only serializes
  same-file `/index` requests within one process. Running multiple mindex processes
  on one SQLite/Qdrant would need the DB-level claim noted in Future work.
- **The DB can be dropped and recreated freely** — no migration upgrade path is
  maintained. Schema changes are new files in the `MIGRATIONS` slice; a changed
  `CHECK` constraint requires dropping the `mindex_mindex_db` volume.
- **The embedding server is reachable at `--model-server`** and, when run on the
  host outside Docker, binds `0.0.0.0` (not `127.0.0.1`) so the container reaches
  it via the bridge gateway.
