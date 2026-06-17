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
- **No API authentication** (by design — internal service, TLS only). Revisit if
  ever exposed beyond a trusted network.
- **HTTP/3 is not implemented.** `run()` is HTTP/1.1 + HTTP/2; the `http3.rs`
  module name is aspirational.

## Future work

- **Languages awaiting upstream `tree-sitter ≥ 0.23` grammar crates** (older
  crates cause a native `links` conflict and cannot be added yet): `yaml`,
  `toml`, `kotlin`, `lua`, `elixir`, `nix`, `erlang`, `swift`. Add via the
  language-extensibility checklist in CLAUDE.md once the crates update.
- **Terminal search frontend (`tools/search/`) copy-to-clipboard buttons** for
  selecting result segments — left as a future iteration.

## Assumptions

- **One embedding model at runtime** (`--model`, default `BAAI/bge-m3`). The
  `EmbeddingModel` enum is the extension point for more.
- **The DB can be dropped and recreated freely** — no migration upgrade path is
  maintained. Schema changes are new files in the `MIGRATIONS` slice; a changed
  `CHECK` constraint requires dropping the `mindex_mindex_db` volume.
- **The embedding server is reachable at `--model-server`** and, when run on the
  host outside Docker, binds `0.0.0.0` (not `127.0.0.1`) so the container reaches
  it via the bridge gateway.
