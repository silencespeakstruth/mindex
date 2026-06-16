# TODO / Known Issues / Assumptions

Living list of deferred work, accepted limitations, and design assumptions.
Architecture and invariants live in `.claude/CLAUDE.md`; this file is only what
is *not done* or *deliberately constrained*.

## Known issues / limitations

- **`post_search` loads the full `code` of every active chunk, not just the
  top-k.** The candidate SQLite query selects `code` + line/column metadata for
  all `status='active'` chunks matching the filters, then uses only the ~`top_k`
  (default 5) winners Qdrant returns. On large projects this reads megabytes per
  query, >99% discarded. **Fix:** two queries — (1) select only `qdrant_guid` for
  the `has_id` filter, (2) after Qdrant returns top-k ids, fetch display rows for
  just those. Deferred because it reshapes a tested core path. Top perf follow-up.
- **The `has_id` filter grows linearly with a project's active-chunk count** (it
  lists every candidate GUID per query). Fine for moderate projects; very large
  collections may want a stored Qdrant payload field (`project_guid`) + a `match`
  filter instead.
- **After `MAX_RETRIES` (3) failures a file stays `status='failed'`** and needs
  manual re-indexing — no dead-letter surfacing or alerting.
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
