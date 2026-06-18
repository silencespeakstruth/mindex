# TODO / Known Issues / Assumptions

Living list of deferred work, accepted limitations, and design assumptions.
Architecture and invariants live in `.claude/CLAUDE.md`; this file is only what
is *not done* or *deliberately constrained*.

## Known issues / limitations

- **The `has_id` filter grows linearly with a project's active-chunk count** (it
  lists every candidate GUID per query). Fine for moderate projects; very large
  collections may want a stored Qdrant payload field (`project_guid`) + a `match`
  filter instead.
- **After `MAX_RETRIES` (3) failures a file stays `status='failed'`** and needs
  manual re-indexing (re-push). Surfaced via a WARN at startup and hourly
  (`worker::retry::warn_permanently_failed`) and now via `GET /projects/{guid}`
  (per-project `failed` count); a real dead-letter view / metric / alert is still
  future work.
- **Deleting all of a project's files leaves the project row + an empty Qdrant
  collection.** `DELETE /files` is scoped to files (the project persists so it can
  be re-indexed), so matching everything still leaves an empty collection behind.
  Use `DELETE /projects/{guid}` to remove the project and drop its collection.
- **`cancelled` files are never retried** by the worker (by design — the client
  gave up). They only revive on an explicit re-push (`cancelled → indexing` via the
  handler). A cancelled file never re-pushed keeps no vectors.
- **In-flight indexing cannot be aborted (no cancellation of committed work).** The
  hot path is append-only: once a file is `indexing`, a batch already in progress
  runs to completion even if the file changed again meanwhile — there is no way to
  abort the stale in-flight index and restart on the fresh content. Drift detection
  *surfaces* this (the `indexing` bucket — no action, wait for it to settle), it
  does not fix it. **Wanted (important):** an abort mechanism so a superseded
  in-flight index can be cancelled and immediately re-run on the latest content,
  instead of waiting a full batch and then reindexing.
- **No API authentication** (by design — internal service, TLS only). Revisit if
  ever exposed beyond a trusted network.
- **HTTP/3 is not implemented.** `run()` is HTTP/1.1 + HTTP/2; the `http3.rs`
  module name is aspirational.

## Future work

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
  tell a contention 429 from a genuine prepare error — `prepare` returns an opaque
  `ErrorResponse`). No corruption (the retry worker re-indexes them), but repeated
  collisions on the same co-batched file could burn its 3 retries and strand it in
  `failed`. Fix: have `prepare` signal contention distinctly (e.g. a `Contended`
  variant) so the batch recovers those files **without** incrementing retry, or simply
  leaves them `indexing` for the worker.

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
