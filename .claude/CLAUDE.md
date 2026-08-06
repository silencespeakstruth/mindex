# CLAUDE.md — mindex architecture & conventions

Only what is **not obvious from reading the code**: invariants, non-trivial
"why", gotchas, regression guards. No flag tables (`--help`), no per-test lists,
no language table (the `ProgrammingLanguage` enum + `Cargo.toml`), no struct/SQL
dumps. Accepted limitations are stated next to the invariant they qualify.
Detail companions live in `docs/claude/` (research, git history, VS Code,
Qdrant, auth) —
this file keeps the invariants; read the matching companion before modifying
that area.

## Overview

`mindex` is an async RAG indexing + search engine in Rust. HTTPS API →
`tree-sitter` AST chunking → dense embeddings from a registry model
(`Qwen3-Embedding`, served over an OpenAI-compatible `/v1/embeddings`) →
`Qdrant` vectors +
`SQLite3` metadata. TLS is the only
transport security; authorization is opt-in (`[auth]`, below) and off by
default, so an unconfigured deployment is still an internal service that must
not be exposed.

TLS verification is uniform across every client: **OS trust store** by default
(where mkcert/corporate roots live), extra CA via `--ca-cert` / `ca_cert` /
`MINDEX_CACERT` / `mindex.caCert`, and `--no-verify` / `MINDEX_NO_VERIFY` —
verifies *nothing*, exists only for the self-signed cert `scripts/entrypoint.sh`
generates. Two sprung traps: reqwest's `rustls-tls` trusts only bundled Mozilla
roots — a Rust client needs `rustls-tls-native-roots` to see the OS store
(without it a locally-issued cert fails every request; this silently stopped
both CLI tools once); and the MCP `drift` tool **shells out to `mindex-index`**,
so a CA setting that misses the child process breaks one tool while the rest
work.

**There is one credential, and it is the token.** The shared `X-Api-Key` a
gateway used to check and mindex ignored is *gone*, not deprecated: two
credentials where one is strictly stronger is not defence in depth — the weaker
one sets the floor, and that one had no scope, no expiry and no way to withdraw
a single holder. The decisive argument is narrower: a token is worth pasting
into a context because it is narrow, and a deployment demanding the API key
*beside* it would put the shared secret back into that same context, which is
the leak the token closes. So `deploy/gate/` admits on the token's **presence**
(nginx cannot verify a signature and does not try) and every question of
validity, scope and action is answered in the server. The consequence is written
into that file and is not optional: **`[auth].enabled = true` is mandatory
behind a gateway** — with authorization off, `Authorization: Bearer x` is
admitted and served everything, so `enabled = false` now means exactly one
thing, a server on a trusted network that authorizes nothing (the Docker test
stack, a loopback-only install).

Four sources per client, first wins: `--token` > `$MINDEX_TOKEN` >
`$MINDEX_TOKEN_FILE` (a path to a 0600 file) > `token` in
`indexer.toml`/`watcher.toml` > the per-server entry in
`~/.config/mindex/credentials.toml`. `MINDEX_TOKEN_FILE` exists for a caller
configured by an environment block inside somebody else's config file — an MCP
server list lives in an editor's own JSON, where a token sits in plaintext under
no permission check and a path does not; its trap is the precedence, since a
shell exporting `MINDEX_TOKEN` passes it to every child, so such a block must
also set `MINDEX_TOKEN=""`. The header travels on the *client*, not per
request, so it reaches every endpoint — including `mindex-search.sh`'s `/config`
probe, which behind a gateway would otherwise quietly fall back to its built-in
language list. **VS Code is the one holder that keeps its own copy**, in
`SecretStorage`: not because a keychain answers "who holds the credential" (it
cannot — the CLI needs the same kind and no shell can read it), but because the
alternative *inside the extension* was a settings string, which Settings Sync
copies to every other machine. It also watches `exp` and warns in the status bar
(`src/token.ts`), verifying nothing — a client asserting validity would claim a
fact only the server establishes. It is also the one surface that *issues*
tokens: `mindex.mintAgentToken` (`agentToken.ts`) derives a token for the open
project, capped at seven days and labelled `agent`, over `POST /auth/tokens`.
Its action list is **two presets over a ticked list, not a fixed one**
(`tokenGrants.ts`, split out of `agentToken.ts` only because that file imports
`vscode` and the guard on these tables has to run under bare `node --test`):
read-only and read-and-write are offered first, `search`+`research` start ticked,
`index`/`delete` are offered off behind a second modal naming what they cost, and
`admin`/`mint` are absent from the list by construction. The presets are an
*ordering*, not a narrowing — the full tick list is one item down the same menu
and is still the only way to reach `delete`, and the write modal fires for the
preset exactly as it does for a tick. Offering only reads was
the earlier call and it was wrong in one direction — it does not prevent a write
token, it moves the minting to a shell, where what gets issued is usually wider.
Every one of those narrowings is **usability, not enforcement** — a command
palette is not a security boundary and does not need to be, because `may_mint`
refuses anything exceeding the minting token regardless of what the client
asked for. It is reachable from **three** surfaces, because a capability behind a
palette title nobody knows is one that does not exist: the Ask view's title bar
(the `$(key)` button, gated on `mindex.hasProject`), the Server Status panel's
header, and a `command:` link in the token indicator's tooltip — that last one
visible only when the indicator is, since it stays hidden while the token is
healthy by design. The issued token goes to the clipboard, and `Show it` opens it
in a **read-only in-memory document** (`tokenDoc.ts`, scheme `mindex-token`) —
not an untitled buffer, which is one accidental `Ctrl+S` from the credential on
disk that "a file is a copy nobody decided to keep" rules out. A window reload
loses it, and the provider says so rather than serving an empty tab.

## Authorization (`[auth]`, opt-in)

**The server used to authenticate nothing, and now optionally does.** That
sentence was an invariant in four places and is retired here rather than left to
rot. The break is bounded and the bound is the point: no user table, no
password, no session, no per-request server-side state. One HMAC check, and
every fact the decision needs rides inside the credential. **Full rationale,
the refusal table, the revocation story and the runbook live in
`docs/claude/auth.md` — read it before modifying `backend/auth.rs`, the scope
extractors or `ROUTE_POLICY`.** The hard invariants:

- **The token is the mapping; there is no schema change.** `prj` (dashless
  GUIDs, or exactly `["*"]`, which must be *spelled* — an empty list reaches
  nothing) and `act` (`search`/`research`/`index`/`delete`/`admin`/`mint`) are
  signed into it. The rejected alternative was a `tenant_id` column, and it cost
  a table rebuild, a trigger pinning one tenant per GUID, an in-process cache, a
  startup warm, a rule for pre-existing rows and an in-transaction re-read
  against `ON CONFLICT DO NOTHING` — none of which survives, along with one bug
  class: only a caller whose token already names a GUID can create that project,
  so `POST /index` stops being an existence oracle.
- **Gateway-only was never possible.** `GET /projects` enumerates every GUID in
  a response *body*, which no proxy filters without parsing, and a GUID is a
  bearer identifier. That listing is why this lives in the server.
- **An out-of-scope project answers 404 `project.not_found`, byte-identical to
  one that never existed.** A distinguishable refusal confirms which GUIDs
  exist, and an error `code` is the field clients are told to key on — so
  `auth.forbidden` cannot exist on that path however much better it reads in a
  log. The missing *action* **is** named (403): the caller already proved it
  holds the project. Pinned on response bytes, not status, by
  `an_out_of_scope_project_is_byte_identical_to_one_that_never_existed`.
- **Two enforcement layers, deliberately overlapping.** Typed extractors
  (`SearchScope`/`IndexScope`/…) are the mechanism — a **type** is what a
  source-text guard can see, which a `RouterState` helper is not (the
  `set_file_status` lesson) — and they check `covers(guid)` **then**
  `permits(action)`, in that order, so a caller that cannot see the project
  learns nothing about the action vocabulary. `enforce_route_policy` is the
  runtime half and **fails closed**: a routed path with no `ROUTE_POLICY` row is
  refused, not served. The layer deliberately does *not* do the project check —
  `/drift` must answer an out-of-scope project as it answers an unknown one,
  `/index` must create, and two listings filter a body, so a blanket answer
  needs a per-route exception table, the fifth copy nothing checks.
- **`ROUTE_POLICY` names every route**, in the `UNDOCUMENTED_ROUTES` idiom, with
  three guards: `every_route_is_named_by_the_authorization_policy` (both
  directions), `every_scoped_handler_takes_the_extractor_its_policy_names`, and
  `every_route_refuses_every_way_it_should` — the last drives the *table* and so
  stays exhaustive as routes are added, which a hand-written per-endpoint suite
  cannot. Public: `/health`, `/version`, `/config`, `/llms.txt`, the descriptor,
  plus `PUBLIC_PATH_PREFIXES` (`/swagger-ui`, `/api-docs/` — `merge`d, not
  routed, so absent from the table and otherwise refused as build defects).
  Liveness must not report the credential's health; discovery telling a caller
  it needs a credential cannot itself require one.
- **`admin` covers `/gc`, `/status`, `/metrics`; there is no `gc` action** —
  `POST /gc` holds the process-wide guard and walks every collection, so a
  project list cannot describe it. Consequence for this host: with `[auth]` on,
  the VictoriaMetrics scrape needs its own admin token (`bearer_token_file`),
  because mindex cannot tell a loopback scraper from a gated one.
- **HS256, written here rather than taken from a crate**, so every copy of the
  secret is owned (no `Debug`, zeroized, key file 0600 with `O_EXCL`) — and so
  algorithm confusion is closed *by construction*: `verify` reads `kid` and
  nothing else before checking the MAC, pinned by
  `the_algorithm_header_cannot_select_the_algorithm`. The TLS key is not reused.
- **Revocation is expiry or deleting a `kid`** — no denylist, by design, since
  that is the per-request state this removes. `--key-id … --new-key` is what
  makes per-holder ids one flag rather than hand-edited base64.
- **`--days 0` mints a non-expiring token, and only the local CLI may.**
  `POST /auth/tokens` refuses it: a network-reachable way to issue an eternal
  credential is a different and worse thing. A minted token can never exceed its
  minter (actions, projects, expiry) — without that, a read-only `mint`
  credential becomes `admin` one call later.
- **`aud` is the one claim nothing in the server reads, and that is the design.**
  `--for cli,vscode,agent` labels which kind of holder a token is for; no part of
  an HTTP request identifies the process behind it, so a server-side check would
  be theatre. The **clients** refuse — `mindexfile::token::audience_refusal` for
  the Rust CLIs (called once where the token is fully resolved, and it refuses
  rather than warns, since the request would otherwise succeed), `token.ts`'s
  `audienceRefusal` for the extension (overridable through a modal). It stops the
  editor's credential landing in a shell profile; it stops no attacker. Absent or
  empty means **every** audience — `skip_serializing_if` keeps the key off an
  unlabelled token so a client keying on presence never meets `"aud": []` and
  reads it as reaching nobody. **`may_mint` deliberately does not contain it**:
  audience is not authority, delegation is a change of holder, and containment
  would refuse the VS Code button minting an `agent` token from a `vscode` one —
  pinned by `the_audience_is_not_an_authority_axis_and_does_not_bind_delegation`,
  which exists because the inconsistency with the other three axes reads as a bug.
  There is no `Claims::intended_for`: a predicate with no production caller reads
  as a check the server performs.
- **Off by default**, and `authorization_off_ignores_the_header_entirely` pins
  that a client-supplied `Authorization` decides nothing when it is.

## Configuration (TOML file + CLI flags)

`config.rs` owns it; indexer and watcher CLIs mirror the scheme in their own
`config.rs` (`mindex/indexer.toml`, `mindex/watcher.toml`; `*.example.toml`).
Precedence **CLI flag > TOML > compiled default**. Defaults live *only* in
`Default` impls — clap holds no `default_value` (every flag is `Option<T>`, so
"passed" ≠ "absent"; that's what makes layering work). `resolve()` finds the
file by XDG canon (`--config`/`$MINDEX_CONFIG` → `$XDG_CONFIG_HOME` →
`$XDG_CONFIG_DIRS`; missing file = defaults), logs every path checked, source
loaded and flag override, then validates: *all* problems collected (not
fail-fast) with what/why/how-to-fix; any error aborts (`deny_unknown_fields`
makes a typo a parse error). Keys carry unit suffixes
(`*_ms/_seconds/_minutes/_days/_chunks/_tokens/_bytes/_points/_mib`).

**Only genuine tuning knobs are configurable.** Structural invariants stay
`const` next to their code with a "why not configurable" comment (the registry's
per-model `dim`/`max_seq`/`collection_slug`/`query_prefix`,
`COLLECTION_SCHEMA_VERSION`, HTTP 499, the SQLite PRAGMAs). Config reaches code through constructors/params, **never globals**.
New knob = key in the right `config.rs` section + `Default` + validation rule,
threaded to the consumer. Request-shape limits are knobs too: `[limits]` and
`[search].max_top_k`/`max_query_bytes` bound requests at the API edge (via
`RouterState` → the validation layer); they are **TOML-only** — tuning them in a
container means mounting a `config.toml`.

## Layout (non-obvious bits only)

- `tools/indexer` (`mindex-index`) and `tools/watcher` (`mindex-watch`) are
  **own crates with own `Cargo.lock`, not in the root workspace**, both
  path-depending on `tools/mindexfile` (the `.mindex` parser);
  `tools/mcp/{mindex,scout}` are Python/Poetry MCP stdio servers;
  `tools/search/mindex-search.sh` is the bash search frontend.
- **The embedder is a contract, and `deploy/embedder/` is where it is kept.**
  `embedder/` was a vendored BGE-M3 server that existed for exactly one reason —
  no general model server emitted its three heads together — and dense-only
  retrieval retired it. What replaces it is `POST /v1/embeddings` +
  `GET /v1/models` + `GET /health`, i.e. any OpenAI-compatible server, with
  three recipes measured against each other in that directory's README. Four
  things there are invisible from inside mindex and each cost a debugging
  session to find:
  **serving stacks differ by an order of magnitude** — the same model on the
  same card reindexes this repo in **51 s** through a ~200-line torch server and
  **410 s** through llama.cpp (measured across `-np` 1/8/32, three ubatch sizes,
  ROCm and Vulkan, 1/4/8 clients; `llama-bench` says its own backend is 3×
  faster than its server), while *query* latency is 16 ms against 30 ms, so the
  trade is entirely about bulk indexing and `[model].query_server_url` is the
  seam for splitting it; **pooling and normalisation are unverifiable over the
  wire** (Qwen3-Embedding pools the LAST token; mean pooling returns 1024
  plausible numbers and simply retrieves worse), so that README's cross-check
  against `sentence-transformers` is the only test there is; **the dtype is
  load-bearing** — Qwen3 in fp16 returns NaN for the longest chunks, which
  mindex refuses as `null` rather than indexing, and which would otherwise
  surface as `search_unscorable_winners`; and **`[model].id` names the model,
  not the precision it is served at**, so switching quantization invalidates no
  stored vector and triggers no re-embed (the same blind spot as a split
  deployment whose two instances differ — see **Retrieval pipeline**).
- Migrations in `src/db/migrations/`. **One**: `v2.0.0_schema.sql` (version 1,
  the whole v2 schema as a single baseline). The six v1 files are deleted, and
  the lineage **restarted** rather than continuing: v1 carried `model_id` in
  seven tables' primary keys and its rows cannot be read under v3 retrieval
  wrongly-but-plausibly, so an initialized pre-v2 database is **refused at
  startup** (`refuse_old_lineage`, keyed on `PRAGMA application_id` = `MX03`,
  stamped by the baseline) with a delete-and-reindex instruction. A fresh file
  has both pragmas at 0 and passes. The applied set is the `MIGRATIONS` slice in
  `main.rs`, keyed by the integer in `PRAGMA user_version`; the filename version
  is documentation. **The baseline is frozen exactly as v1.0.0 was** — the filter
  is `version > user_version`, so an in-place edit never reaches a database
  stamped at 1 and is skipped in silence. Thirteen tables: `embedding_models`
  (the registry's SQLite half — ids and dims `CHECK`ed, append-only by trigger),
  `projects`, `project_files`, `project_file_chunks`, `project_file_status_log`,
  `project_file_symbols`, `research_runs`, `research_run_files`,
  `research_run_evidence`, `research_run_citations`, `research_run_steps`,
  `project_commits`, `project_commit_paths`. **No 1:1 side tables** — the three
  that existed (an `ADD COLUMN` workaround) were folded back into their parents;
  each cost a hot-path JOIN. A new *field* is a table rebuild (rule 8);
  the four `research_run_*` tables are genuine 1:N children. `.sqlfluff` raises
  `large_file_skip_byte_limit` — sqlfluff skips files over 20 kB with only a
  warning, so without it the schema is silently unlinted.
- `scripts/entrypoint.sh` generates a self-signed cert on first container start.
- `rust-toolchain.toml` pins 1.95.

## Core invariants (violating these causes bugs)

**Project isolation = collection + has_id filter.** One Qdrant collection per
project **and model**, `{guid_simple}_{slug}_v3` (the registry's
`collection_slug` + `COLLECTION_SCHEMA_VERSION`, `qdrant.rs`); always derive
names via `collection_for(project_guid, spec)`, never by formatting. The
candidate set is a `has_id`
filter built from SQLite (`qdrant_guid` for chunks matching project + filters +
**`status='active'`**) — the *sole* isolation mechanism, also excluding
soft-deleted vectors. It grows linearly with active-chunk count — fine at this
scale; a very large collection would want a stored `project_guid` payload field
+ `match` filter.

**Append-only hot path.** Indexing never deletes from Qdrant. On reindex
(sha256 mismatch): old chunks marked `deleted` in SQLite, new inserted
`active`, new vectors upserted; old vectors orphan until GC (decouples indexing
latency from Qdrant delete latency).

**Symbols parallel chunks, but hard-delete — and they are definitions only.**
`project_file_symbols` holds the **definition** tags of the language's upstream
tree-sitter tags query — one universal extractor, `slicing/symbols.rs`, zero
per-language code; vendored queries in `slicing/queries/` where the crate
exports none. The query emits references too and they were stored until they
were measured: 23 810 reference rows against 3 397 definitions, **87.5% of the
table**, serving one model-facing tool (`callers`) called **twice** across
twenty-five recorded research runs at a 50% miss rate. The edges are lexical — a
reference row records a token in call position, not which definition it binds to
— so the most-referenced names here were `assert_eq` (1084), `clone`, `Ok`,
`unwrap`, `map`, several with exactly one definition in the tree. Nothing
aggregates usefully over that, which is why the repo map that ranked by it was
withdrawn along with them. `grep` answers "who uses this name" lexically **and
says so**, which is the honest version of what `callers` implied.
`parent_name`/`parent_kind` survive: for a definition the enclosing definition is
what makes `Gc::collect` readable, and both `outline` and `/symbols` return it.
The table has no Qdrant counterpart, so
its lifecycle is the opposite of chunks: hard `DELETE`, no soft-delete/GC.
Invariant: every tx that marks a file's chunks `deleted` (reindex-prepare,
`DELETE /files`, `/cancel`, `drop_cancelled`) deletes its symbols in the same
tx; `DELETE /projects/{guid}` drops them in its one hard-delete tx. Inserts
happen in the prepare tx alongside chunk inserts. FK RESTRICT backstops the
`prune_deleted_files` ordering. Extraction failure degrades to "no symbols"
(WARN), never fails indexing. `POST /v0/{guid}/symbols` is exact-name lookup
returning **ranked candidates + full totals** (collisions are contract, never a
single "the" answer); ranking is purely path-based (anchor file > its exact dir
> rest). Empty result = 200, not 404.

**GC hard-deletes only confirmed rows** (regression guard, `worker/gc.rs`). A
sweep deletes from SQLite *only* chunks whose Qdrant `delete_batch` succeeded;
failed collections keep their rows `deleted` for the next sweep (SQLite-first
would orphan the vector forever). If every collection in a batch fails, the
loop breaks rather than spinning. **A missing collection is a confirmation, not
a failure** — `delete_batch` (`db/qdrant.rs`) converts it, as `delete_collection`
and `count_points` beside it already did: the vectors it was asked to remove are
demonstrably not there. Reported as a failure it was worse than cosmetic, because
"keep the row until the vector is confirmed gone" then meant *never*: the chunk
rows were unsweepable, their `deleted` file rows unprunable behind the RESTRICT
FK, and the backlog grew for the life of the deployment — in exactly the state a
lost Qdrant volume leaves behind, where GC needs to work most. The conversion is
checked only **after** a failure (the ordinary path pays no extra round trip) and
only a definitive `Ok(false)` converts: if `collection_exists` cannot answer, the
original error stands, since reading "I could not ask" as "it is not there" would
hard-delete rows whose vectors are still present. The same pass prunes
`project_file_status_log` (`[workers].status_log_retention_days`, default 30)
and runs `prune_deleted_files` — drops `deleted` `project_files` rows once
their chunks are gone (guard: `NOT EXISTS` over *any* chunk row; FK RESTRICT,
so only after the sweep); that ordering is what makes `DELETE /files`
eventually physical. `POST /gc` runs the same `gc::collect` synchronously. GC
is **global**, serialized by `GcGuard` (`Arc<AtomicBool>`): `POST /gc` during a
running pass → **409**; the hourly worker skips its tick if a manual pass holds
the flag; the guard frees on `Drop`, so a panic can't wedge GC off.

**Status state machine** (`project_files.status`), enforced by SQLite triggers
(`project_files_status_{insert,update}_guard`). Legal moves: **any →
`indexing`** (incl. `deleted → indexing`, resurrection), **any → `deleted`**,
and **`indexing` → `indexed`|`cancelled`|`failed`**; a new row may only enter
as `just_uploaded`/`indexing`. Anything else raises
`SQLITE_CONSTRAINT_TRIGGER`.

- `indexing` is committed durably *before* heavy work (crash-recoverable; the
  retry worker picks up files stuck longer than `--stuck-grace-mins`, default
  30). That grace **must exceed the longest in-flight request** — cross-file
  batching holds a whole batch in `indexing` through the embed pass.
- A stuck file with **no active chunks** (sliced to 0) is marked `indexed`, not
  `failed` (`failed→indexed` is illegal — a wrong `failed` would trap it).
- `sha256` is (re)written on entering `indexing` and confirmed at `indexed`;
  the `retry_count` reset lands only on `indexed`.
- Status writes go through `db::files::set_file_status` (stamps
  `status_updated_at`, WARNs on rejection); AFTER-triggers log every transition
  to `project_file_status_log`. A file exhausting `MAX_RETRIES` (3) stays
  `failed` forever (`warn_permanently_failed` surfaces it at startup + hourly).
- **It returns `bool` and is `#[must_use]`: a status write can fail, and the
  caller decides what that means.** `false` covers a DB error, a trigger
  rejection *and* a 0-row `UPDATE` (the file was deleted meanwhile). Discarding
  it is legitimate on the indexing recovery paths — the request is already
  failing, there is nothing else to try — but it has to be written as `let _ =`
  with the reason, because the default was the bug: the retry worker reported
  `"indexed"` to `retry.files{outcome}` on the strength of a write it never
  checked, so a database that had stopped accepting writes kept a clean success
  rate while every file stayed stuck.
- **Recovery runs under its own token, never the request's**
  (`FileIndexer::recover`, `drop_cancelled`'s chunk cleanup). `SQLite3Pool::run`
  short-circuits on a cancelled token *before* touching the database, so a child
  of the request token made recovery a no-op in the one case it exists for — a
  cancelled/disconnected request left every prepared file `indexing` until the
  30-minute stuck-grace sweep. The unit test passed a fresh token and so agreed
  with the bug; `recovery_still_writes_when_the_requests_token_is_already_cancelled`
  is the guard.
- **The retry worker never infers "no chunks" from a failed read.** The
  active-chunk query fed the branch that marks a file `indexed` with zero
  vectors, behind an `unwrap_or_default()` — so a `PoolEmpty` or a locked
  database silently promoted files to permanently-indexed-and-empty, at `info!`.
  A read that fails leaves the file `indexing` for the next sweep
  (`a_database_error_never_passes_for_a_file_with_no_chunks`).

**sha256 + derivation-version + model-identity skip / empty 404.** Identical
content is skipped by hash — but only if the *derivation versions* also match
(a hash answers "did the file change", not "did the deriving code change") and
the *model identities* do (nor "is this the model whose vectors exist").
`file_already_indexed` is a five-way predicate over an `indexed` row:
`chunks_version`, `symbols_version`, `chunker_id` (the tokenizer that measured
the boundaries) and `embedded_model_id` (whose vectors exist) must all equal
the current consts and the active spec, and the stored `sha256` must equal the
posted one. All four columns are nullable, and NULL never matches, which is what
makes every backfill automatic. `post_search` returns
404 immediately when the SQLite candidate set is empty, without calling Qdrant
(avoids a 503 from a missing collection).

**Internal versions are all one notation: `MAJOR.MINOR`, as a string.** MINOR =
the *way* something is produced changed; MAJOR = its *shape* did. All compared
by plain equality, never ordered — both halves trigger the identical rebuild.
The set: `CHUNKS_DERIVATION_VERSION` (`"1.0"`),
`SYMBOLS_DERIVATION_VERSION` (`"1.1"` — bumped when references stopped being
extracted, which is what removes the 23 810 stored ones), `PROMPT_VERSION`
(`"2.7"` — 2.6 was the repo-map prelude and is **not** reverted to 2.5: nine
journalled runs carry it, and a reused version would make them name a prompt
that never existed). Deliberately outside it:
`COLLECTION_SCHEMA_VERSION` (`"v3"`, a collection-*name* component), the model
identities `chunker_id`/`embedded_model_id` (registry strings, not versions —
they name *which* model, not which revision of an algorithm) and the migration
`i32` in `PRAGMA user_version`.

`COLLECTION_SCHEMA_VERSION` **is still not self-healing** — bump it and the new
name names no collection while SQLite still reports every file `indexed`. The
symptom then depends on whether anything has indexed the project since, and
neither half is a good signal: untouched, the collection does not exist and
`/search` answers **503 `qdrant.unavailable`**, which reads as an infrastructure
fault rather than a missing index (observed on this host at the v1→v2 bump, for
the one project that had not been reindexed); touched, `ensure_project` has made
an empty one and search returns nothing at all. A bump means
reindexing every project by hand (`mindex-index --force`) and dropping the
collections left behind at the old version. What it is no longer is *silent*:
`worker::stale` runs at startup and hourly and answers two separate questions —
**stale** (a project holds active chunks but its current-version collection is
missing or empty; its search is broken now) and **orphaned** (a collection
exists at a previous version, unreachable by anything, still holding the whole
pre-bump index — SQLite records no layout, so this listing is the only thing
that can see it). Both are gauges (`mindex_stale_collections`,
`mindex_orphaned_collections`) seeded at **-1**, never 0, and a pass that could
not complete publishes nothing: `0` is the healthy reading, so an unreachable
Qdrant must not be able to spell it. Foreign collection names are classified and
then never mentioned — Qdrant may be shared, and telling an operator to delete
another service's data is worse than the problem being reported. Dropping the
old collections is deliberately **not** automated: it is what makes a rollback
impossible. The runbook is in `docs/claude/qdrant.md`; `v2 → v3` (one dense
vector, per-model names) is the second bump it covers, and the one that could
not be a migration — the vectors themselves are from another model.

**A model switch is not a version bump, and the classifier says so.** Since
the name carries the model slug, `classify_collection` has a third answer
between `Current` and `Foreign`: **`OtherModel`** — a *registered* model at the
current schema version, i.e. a collection this deployment wrote and may want
back. It is reported as held (info), never as orphaned, because switching
`[model].id` back is meant to be instant reuse; only `Previous` (a superseded
`COLLECTION_SCHEMA_VERSION`) is genuinely dead. Both halves of every name
component are checked, so a collection merely *ending* in `_v3` is still
`Foreign`.

**Derivation versions and model identities** (four nullable columns on
`project_files`), all stamped by the same prepare-tx upsert that moves the file
to `indexing` — the tx that writes the chunks/symbols they describe, so a row
can never claim a version, a tokenizer or a model whose rows were not actually
produced:

- `CHUNKS_DERIVATION_VERSION` (`slicing/traits.rs`) — the AST walk, node
  selection, left-extension rule. **Bump when a change would give
  different chunk boundaries for the same source.** Expensive (re-slice,
  re-embed, re-upsert). Two axes are deliberately *not* covered: the `[slicer]`
  token window (config; retuning is the operator's call) and the **tokenizer**,
  which has its own column — a version cannot see a tokenizer change, and
  pretending it could is how stale chunks hide behind a matching hash.
- `SYMBOLS_DERIVATION_VERSION` (`slicing/symbols.rs`) — `queries_for`, the
  vendored `.scm` files, the extraction walk, the grammar crates. **Bump on any
  new/edited/vendored tags query, an `ALL` variant change, a `SymbolExtractor`
  change, or a `tree-sitter-<lang>` bump that alters tags output.** Cheap (pure
  CPU) — separate precisely so a tags fix doesn't cost a full reindex.
- `chunker_id` — the registry `tokenizer_hf_id` the boundaries were measured
  with. Not bumped by hand: it changes when `[model].id` moves to a model with
  a *different* tokenizer, and that is what makes a re-slice mandatory rather
  than optional.
- `embedded_model_id` — the registry id whose vectors are in Qdrant. Changes
  with `[model].id`, and is the one a **re-embed alone** repairs.

Bumping is the *whole* action: the next ordinary `mindex-index` run rebuilds
affected files by itself. Two narrow passes exist for the two cheap cases, and
each refuses what it cannot honestly do. After a symbols bump use `mindex-index
--symbols-only` (body flag `symbols_only`): replaces symbol rows in one tx per
file, no slicing/embed/Qdrant — ~20× faster (0.3 s vs 6.5 s on this repo). It
skips files whose hash no longer matches (their chunks are stale too); run an
ordinary pass for those. After a `[model].id` change **within one tokenizer**
use `mindex-index --vectors-only` (body flag `vectors_only`): re-embed the
stored chunks into the new model's collection and stamp `embedded_model_id`, no
slicing and no symbols. It refuses a file whose `chunker_id` names a different
tokenizer — those chunks are the wrong chunks, not merely the wrong vectors —
and the two flags are mutually exclusive at validation. With one tokenizer
across the Qwen3 sizes, changing size is exactly this pass. Not bumping is the motivating failure: the symbols
feature shipped without one, hash-skipped files never gained symbols, and
`/symbols` answered "no such symbol" (contractually *definitive*) for a third
of the tree. Caveat the consts cannot see: a grammar-crate bump in `Cargo.lock`
changes tags output with the const untouched — bump by hand.

**FK is RESTRICT.** `project_file_chunks → project_files` is `ON DELETE
RESTRICT`. Never delete a parent row while chunks exist; mark chunks deleted,
let GC clean up.

**Management endpoints** (`handlers.rs`, routed in `http3::run`, *not* under
`/v0`). Full behavior in handlers + OpenAPI; the non-obvious parts:

- `DELETE /projects/{guid}`: immediate hard delete — rows first, collection
  dropped **last** so a retry re-attempts it; idempotent 204.
- `DELETE /projects/{guid}/files`: **soft** delete; `include`/`exclude`
  selector in the **body** (globs don't fit the path); empty selector = 400;
  204 if none matched, else 200+count.
- `POST /cancel`: same body selector + empty-400, matches **only
  `status='indexing'`** (a too-late cancel is a no-op); marks chunks `deleted`,
  `indexing → cancelled`. Takes **no** `IndexClaim` (so it can interrupt a held
  one); correctness against a live `/index` rests on **two re-reads**, not a
  lock: `post_index` runs `drop_cancelled` between Phase 1 and 2, and the retry
  worker re-checks status *after* acquiring the claim (else
  `cancelled → indexing`, a legal move, would resurrect it). A cancel landing
  mid-embed lets the pass finish; `mark_indexed`'s `UPDATE` carries `AND status
  = 'indexing'`, so it matches 0 rows, the illegal transition is never
  attempted, the file stays `cancelled`, the rest of the batch succeeds, GC
  reclaims the orphans. The trigger is the backstop, not the mechanism;
  `mark_indexed` is the one status write not going through `set_file_status`.
- `POST /retry`: requeues `failed` files; **empty body = all failed**
  (non-destructive). **Metadata-only**: `retry_count = 0`, status stays
  `failed` (skips triggers, takes no claim), and `status_updated_at` untouched
  so the retry worker's failed-branch cooldown (`status_updated_at < now-60`)
  fires on the next sweep, not after a fresh grace.
- `POST /drift`: **read-only**. Posted `path → sha256` manifest (capped by
  `[limits].max_drift_files`) classified against SQLite: `stale` (hash
  differs), `missing` (not indexed; `failed` counts), `orphaned` (indexed,
  absent from manifest), `indexing` (in-flight, excluded from `stale`/`missing`
  since its stored hash is the *incoming* value). Unknown project ≠ 404 — every
  posted file is simply `missing`. Backs `mindex-index --check`, the MCP
  `drift` tool, the watcher's sweep.
- **Stored research** (`GET /projects/{guid}/research[/{run_id}]`,
  `POST …/{run_id}/pin`, `DELETE …/{run_id}`): the browse half of the corpus,
  on the management plane like `/files`; the run that *produces* a report stays
  at `POST /v0/{guid}/research`. The list is keyset by `seq`, searches
  `question` **and** `report` with `like_escape` (FTS5 is the next ladder rung;
  `LIKE` unmeasured-insufficient at this corpus size), and never selects the
  report body — which is why it is a separate endpoint from the detail. `pin`
  is the one mutation on an otherwise append-only row; its `pinned` **defaults to
  true**, so `{}` pins — required, it made the obvious call on an endpoint named
  `/pin` a 400 naming a field the caller had no reason to guess, and unpinning is
  the direction worth spelling out. `DELETE …/research` (no
  `run_id`) is the **batch** form, body `{"ids": […]}`; empty list = 400
  `selector.empty`, capped by `[limits].max_research_delete_ids`; unknown ids
  ignored (idempotent). Each summary carries `references_count` (direct edges
  out) and `referenced_by_count` (direct edges in, whole corpus), from the
  `edges` CTE validity already builds. `references_count` is **not**
  `context.len()` (that is *transitive* ancestry). The inbound count is what
  makes a delete confirmation honest — removing a run invalidates every
  descendant. Summary columns are counted by `RESEARCH_SUMMARY_COLUMNS`, and
  the detail query indexes its four columns *from* that constant (a summary
  column added without moving it once handed the caller `invalid_flag` where
  `report` belonged).
  The response also carries **`totals`** (`ResearchCorpusTotals`:
  `total`/`current`/`challenges`/`stale` + `gc_candidates` and its four buckets;
  `challenges` and `stale` are the denominators a corpus of two populations
  needs — `stale` counts **pinned runs too**, unlike `gc_stale`, because it is a
  measure of drift and not a delete proposal) — one extra `SELECT`
  reusing the same recursive validity CTE the page already paid for, selecting
  no report body. **No filter on the request touches them**, deliberately: they
  are a fixed denominator, and a count that shrank as the caller typed into `q`
  would be a worse rendering of `runs.len()` (`the_corpus_totals_query_takes_no_filter`
  is the guard). `gc_candidates` is the **union** of the four buckets, never
  their sum — a run that is both stale and partial is one report to delete —
  and every bucket is unpinned-only, so the number and the proposal a client
  builds from it cannot disagree. Two filters beyond `freshness`/`valid`/
  `kind`/`pinned`: **`challenged_run_id`** (the first reader of
  `idx_research_runs_challenged`, and the only way to find a challenge whose
  verdict was inconclusive or whose own evidence has since moved — both of
  which `trust` correctly stops counting) and **`completeness`**
  (`finalized`/`partial` over `done_reason`). `completeness` is server-side
  *because* a client pruning a corpus pages this list to exhaustion: every
  filter applying before the `LIMIT` is what makes "a short page means no more"
  true, and that inference is the contract. `[research].list_page_limit` and
  `[limits].max_research_delete_ids` are published on `GET /config` for the
  same reason — a client sizing a paging loop must not guess.
- **Offline re-verification** (`GET /projects/{guid}/research/{run_id}/
  verification`): `check_citations` re-run as a pure function over journal rows
  (`report` + `research_run_evidence` spans + `research_run_files` vs
  `project_files`) — no model, no GPU. Two answers, deliberately separate:
  **provenance** is immutable and must match the recorded counters
  (`provenance_matches: false` = journal bug, never news about the code);
  **staleness** is computed against the index *now* and is the number that
  moves. Nothing is stamped — derived like validity, so it can never disagree
  with a recomputation. The report is scored **twice** and either match counts
  (`recheck_citations` / `recheck_citations_exact`): citation path resolution
  arrived with `PROMPT_VERSION` 2.4 and flips a bare filename's verdict, so
  scoring an older row only the new way reports a correct journal as broken —
  an alarm on healthy history, which is what that field must never be. The
  price is precise and bounded: for a report carrying a resolvable bare
  filename the check can no longer tell the two scorings apart; every other
  report is checked exactly as strictly as before. Retire the second scoring
  when a resolved path is stored per citation (a migration).
  Pre-v1.3.0 rows: `spans_available: false`, staleness
  half only (recomputing provenance without spans would score everything
  `unverified` — the check lying); the discriminator is `started_at IS NOT
  NULL`, which arrived in the same migration as the spans.
- **Live research runs** (`GET /research/active`, `DELETE
  /research/active/{run_id}`): global, not per project — the semaphore is. Kept
  off `/projects/{guid}/research` because that list is keyset-paged by `seq`,
  which a live run has not got yet. See the `/research` section for the registry
  behind them.
- The read-only set (`GET /projects[/{guid}][/files]`, `/status`, `/config`,
  `/health`, `/version`) + `POST /gc` are self-describing in OpenAPI.
  `GET /config` serves the canonical supported-language list (read by the
  search frontend); `/files?status=failed` is the dead-letter view. `/config`
  is static **except `research.models` and `research.observed`** — both worker
  -refreshed on a tick; don't cache it once.
- **Two discovery documents, and the second exists because the first is
  refusable.** `/llms.txt` is prose for a model; `/.well-known/mindex.json` is
  the same service as data (identity, endpoint inventory, the inlined `/config`
  snapshot). The split is not tidiness: a network-fetched document written *at*
  a model is a prompt-injection signature, GitHub Copilot refused the original
  `llms_doc.md` on those grounds, and losing it left an agent with nothing —
  it was the only entry point. So the prose now **argues instead of ordering**
  (every recommendation carries its reason, the reader is "a caller", never
  "you"), guarded by `llms_doc_avoids_the_injection_signature`; and JSON, which
  has no register to object to, is the floor under it. The descriptor **is** in
  the OpenAPI spec — the opposite call from `/llms.txt` and `/metrics`, because
  it is JSON for a client rather than prose for a reader or exposition for a
  scraper, and all three assertions sit together in
  `openapi_spec_is_complete_and_versioned` so the contrast reads as a decision.
  Its `endpoints[]` is **derived from the spec at first use**, never written:
  the route table already had four copies, and a hand-kept fifth is the one
  nothing checks. Three things are hand-kept and each has a guard —
  `UNDOCUMENTED_ROUTES` (the routes deliberately outside the spec; read by the
  descriptor *and* by `llms_doc_mentions_only_routes_that_exist`, one list
  because it was two), `DESCRIPTOR_HIDDEN_ROUTES` (`/metrics` alone: routed only
  under `[metrics].enabled`, so advertising it promises a 404), and
  `STREAMING_ENDPOINTS` (OpenAPI records a response's shape, not that it arrives
  in frames). `the_route_table_holds_no_path_the_descriptor_omits` scans
  `http3.rs`'s **production half** for `.route(` literals — neither axum nor
  utoipa enumerates routes at runtime, so a source-text test is what exists;
  it must skip the `#[cfg(test)]` module, whose throwaway routers are not API.
  `authentication` is serialized as an explicit `null`: "authenticates nothing"
  and "too old to say" must not look the same on the wire.
- **`GET /health` is tri-state, and the server owns the verdict**
  (`HealthChecks::verdict`, next to the data so nothing computes it twice):
  `ok`; `degraded` = only the **optional** Ollama is failing, which is exactly
  the state where a client should keep offering search and stop offering
  research; `unhealthy` = a required check failed (sqlite/qdrant/embedder, plus
  `query_embedder` *when present*) or a run is wedged. Severity wins — Ollama
  down *and* Qdrant down is `unhealthy`, never `degraded`. Two words rather than
  three was the defect: every client then needed its own copy of which check is
  required in order to decide whether `degraded` was worth disabling anything
  over, and the extension's did not match the server's. **`checks.*` is exactly
  `"ok"` or `"error"`** — the reason a probe failed goes to a `warn!` at the
  probe site with a sysadmin hint (`probe()` in `handlers.rs`), because this
  response is readable by anything that can reach the port and a driver's error
  chain carries paths, URLs and versions. HTTP is always 200. A client must test
  `== "ok"`, never `startsWith("error")` — an older server still sends
  `"error: <reason>"`.
- `GET /projects/{guid}` is the per-project **inventory**; the per-language
  *file* count is the load-bearing half. Keyed on chunks alone, a language
  whose files are all `failed` or sliced to zero chunks was absent from the map
  — indistinguishable from a language the project lacks, which is a different
  answer ("indexed, and search will still find nothing"). That distinction is
  what lets the VS Code pickers offer only `chunks_active > 0`.

## /research (Ollama-driven; SSE under `?stream=yes`)

`POST /v0/{guid}/research` — a local Ollama model
(`[research]` config, TOML-only) loops tools **via internal cores**
(`search_core`/`symbols_core` in `handlers.rs`; never HTTP-to-self), then
produces a Markdown report. **Full rationale, rejected alternatives and the
measurement record live in `docs/claude/research.md` — read it before
modifying `research.rs`, `models/ollama.rs`, the budgets or the SSE
contract.** (Design decisions marked "measured" point to the 2026-07-28
108-run and 2026-07-30 28-run corpora summarized there; the corpus of record
is the `research_runs` table.) The hard invariants:

- **Streaming is opt-in (`?stream=yes`), and the default answers one JSON body.**
  `/index`'s shape, adopted late and for a sharper reason: frames make
  disconnection the cancellation interface, which also makes *reading to `done`*
  compulsory — so a caller that issued the request and did not stay spent the
  whole budget, got nothing, and raised no error anywhere. Safe behaviour is now
  what you get by not asking. Both entrances read the query through
  `launch_research_job`, so it cannot mean one thing on research and another on a
  challenge; everything above the spawn (pre-flight refusals, permit, minted
  `run_id`, registry entry, `started` frame) is identical and only the tail forks.
  **Every field of `ResearchResponse` is a frame's own `ResearchEvent::data()`**,
  so the JSON mode is a transcription and not a fifth copy of the SSE contract —
  `the_json_body_carries_the_frames_the_stream_would_have_sent` asserts against
  `data()` rather than literals precisely so it cannot become one.
  `thinking`/`step`/`progress` are dropped (they exist to be watched; the count is
  on `done`, the trace in `research_run_steps`). Gating this on `aud=agent` was
  rejected: `aud` is not an authority axis and a check there stops only the caller
  honest enough to label itself.
- **A mid-run failure is an `error` frame on a stream and an HTTP status without
  one** — the one deliberate difference between the modes, and the reason
  `ollama.unavailable`/`ollama.error`/`research.no_report` became real `ApiError`
  variants (503/503/500) instead of string literals in `research.rs`. The crossing
  lives in `ApiError::from_research_failure` alone; it is a match on **strings**,
  where a missing arm still answers — as a well-formed 500 with the wrong code —
  so `FAILURE_CODES` (`#[cfg(test)]`, beside `ResearchAbort`) +
  `every_research_failure_code_rebuilds_itself` is what makes adding a failure
  code fail loudly.
- **Cancellation = cancelling the job token; two hands reach it, in both modes.**
  `SseEventStream`'s `Drop` (disconnect) is the primary one and still the only one
  the loop knows about; without a stream a `CancellationGuard` held across the
  drain does the same when axum drops the handler future (`post_index`'s JSON-mode
  shape), so the JSON mode removes the obligation to keep reading and **not** the
  cancel-on-disconnect rule. `DELETE /research/active/{run_id}` is the second, for the
  case disconnect cannot cover — a caller that abandoned the run while its socket
  stays open (scout holds its connection up to `RESEARCH_TOTAL_TIMEOUT`, 70 min).
  The semaphore permit rides **in the spawned job**, not the stream (releasing on
  stream-drop would over-admit past `max_concurrent`).
- **A run is named from admission, and listed while it runs.** `run_id` is minted
  in `post_research` (not by `insert_run`), streamed as the first frame
  (`started`), and registered in `backend::inflight::ResearchRegistry` — whose
  guard rides in the **same** spawned future as the permit, so the list can never
  describe a slot that is free or hide one that is not. A cancelled run is still
  never journalled; the registry, not the table, is what makes it visible. Before
  this a run had no id until it ended: nothing could list, cancel or name one while
  it ran, and with `max_concurrent = 1` an occupied slot was an unattributable
  total outage whose only remedy was a restart.
- **`GET /health` reports the slots** (`research.{slots_total, slots_busy,
  oldest_inflight_age_ms}`), and this is the one place research moves the verdict:
  a **busy** slot is never a degradation (permanent at `max_concurrent = 1`), while
  a run past `max_seconds + report_timeout_ms + inflight::WEDGE_GRACE` is
  **`unhealthy`** — it is the one input to the verdict with no failing check
  behind it. That
  same predicate is `worker::research_watchdog`'s cancel rule — one const, so
  health and the watchdog cannot disagree about "wedged". The watchdog is spawned
  **unconditionally**, unlike the metrics collector: gating a recovery mechanism on
  `[metrics].enabled` would let an observability switch decide whether the service
  can recover. It exists for the awaits that are not under a token (`/api/show`,
  Ollama's error-body read, the deliberately uncancellable journal write);
  `research_watchdog_cancels_total` is expected to stay at zero.
- **Dedicated runtime**, leaked in `main.rs` (`[research].worker_threads`);
  admission via `Arc<Semaphore>` (`max_concurrent`, published by `GET /config`) →
  429 `research.busy`, whose detail names `/research/active`.
- **Two seams** keep the loop testable: `OllamaModel` (`models/ollama.rs`) and
  `ResearchTools` (`research.rs`). Mocks: `tests/mock_ollama` (scripted via
  `POST /script`; `force_text_calls` covers `research.model_lacks_tools`),
  fakes in `research.rs` tests.
- **A prose tool call is detected in two notations, and it is a diagnostic,
  never a parser.** `looks_like_tool_call_attempt` scores JSON (a `name`/
  `action` object) **and** markup (`<tool_call`, `<function=`, `<|python_tag|>`
  — `MARKUP_TOOL_CALL_OPENINGS`). The markup half is not hypothetical: a model
  that calls tools natively all run long still writes markup on the **report**
  turn, where no tools are passed, and JSON-only detection let that through
  every gate meant to catch it — the section-retry filter, the content gate and
  `validate_report_markdown` — so a run shipped, streamed and journalled a
  "report" whose whole body was three fake calls. The content gate withholds
  the same set (`WITHHELD_OPENINGS`, one list so the gate and `is_withheld`
  cannot disagree about what was streamed — the property a re-ask rests on);
  its decision is **tri-state**, because a delta can be a bare `<`. In a
  section, the prose **before** the markup is kept when it clears
  `MIN_TRUNCATED_SECTION_CHARS` (200) and the section is retried otherwise: the
  observed shape is one restated-question line and then the fake call, and half
  a sentence under a server-supplied heading reads as an answer while saying
  nothing.
- **Native tool calling only; no text fallback.** Eleven tools
  (search/grep/symbols/outline/list_files/read_chunks/file_history/
  list_research/read_research/note/revise_plan, plus `finalize`) as `tools`
  JSON Schemas (`tool_specs`), back in `message.tool_calls`; a call becomes an
  `Action` (`#[serde(tag = "action")]`) — one deserializer. A prose call is
  detected (`looks_like_tool_call_attempt`) → `research.model_lacks_tools`,
  naming the model.
- **Every announced call gets exactly one `role: "tool"` reply, in order** —
  including rejected/skipped ones (the pairing invariant; the `NativeOllama`
  fake asserts it every turn). A deadline firing mid-batch must still answer
  every announced call before breaking.
- **Prose with no tool call = `Finalized`**; `Unparseable` = empty reply or
  nonexistent tool. Duplicates rejected, not re-executed; for `search`
  "duplicate" is near-duplicate (normalized, token-set Jaccard
  `NEAR_DUPLICATE_JACCARD` 0.5, ≥ `NEAR_DUPLICATE_MIN_TOKENS` tokens) —
  deliberately also rejecting a mild refinement. Only *executed* searches enter
  `seen_queries`. **A refusal names the colliding query, the ladder out of it,
  and what it costs**: the *earlier* query (found, not merely detected — quoting
  the model its own new words back tells it nothing it did not just write), the
  four tools that find what a rephrasing cannot (`symbols`/`grep`/`outline`/
  `read_chunks`), and `duplicate N of MAX_DUPLICATE_CALLS+1`, because a refusal
  is a step in all but name and a model that cannot see the counter spends its
  budget insisting. Measured: a `high` challenge ended `repeated_calls` at 11
  steps and 126k prompt tokens.
- **`read_chunks` reads the index, never the file** (pure SQL,
  `status='active'`, span overlap, `READ_CHUNKS_LIMIT` 8 × the run's
  `evidence_width`); gaps reported
  honestly ("indexed; lines N-M have no chunk"). **`path_prefix` on `search`
  is a post-filter** (`top_k * PREFIX_OVERFETCH`, then truncate), never
  appended to `include` (a union — the run could search out of its scope).
- **Bump `PROMPT_VERSION`** (`research.rs`) on any edit to `system_prompt`,
  `plan_request` (templated by the run's `max_report_sections` — the test fakes
  match it by prefix, `PLAN_REQUEST_PREFIX`), `SUFFICIENCY_REQUEST`,
  `REVALIDATION_SYSTEM_PROMPT`, `format_citation_complaint`,
  `format_ungrounded_complaint`, `format_hearsay_complaint`,
  `REPORT_ROLE`/`report_system_prompt`, either report turn's user message,
  `section_system_prompt`/`section_request`, `CHECKPOINT_REQUEST`, the
  budget nudges or `tool_specs`. Sampling (`temperature/top_p/seed`) is
  `Option` — absent = model default; a request's `seed` overrides config.
- **A report of 3+ plan items is written one section at a time.** The report used
  to be a single turn, so a model that could not produce it produced *nothing* —
  a fifteen-minute run returning zero. Now each numbered sub-question gets its
  own turn (`write_sectioned_report`), the server assembles the document under a
  derived `# heading`, and a section that fails costs that section, not the
  document. Below `MIN_SECTIONED_PLAN_ITEMS` (3, what `PLAN_REQUEST` asks for) —
  or with no plan at all — the run takes the old single-turn path byte-for-byte,
  which is the safety valve *and* the revert switch. Bounded three ways because
  it is a new turn-producing path: the run's `budget.max_report_sections`
  (effort default 6, request-overridable `3..=[research].
  max_request_report_sections`, and what the templated plan prompt asks for —
  "3-N"), `MAX_SECTION_ATTEMPTS` 2 (not `MAX_EMPTY_REPORT_RETRIES` 5 — that was
  sized for one turn, and 5×6 is thirty), plus `MIN_SECTION_MS` of window and
  `REPORT_TOKEN_OVERDRAFT` (1.5) which **stub rather than stop**. No new `DoneReason`: a failed section is a
  degradation, not a `break`. Each section turn sees its sub-question, the run's
  own sufficiency verdict on it, and the other sections' **headings only** —
  feeding back their prose would grow the prompt to compensate for shrinking the
  output. Two consequences: `validate_report_markdown` splits into
  `validate_markdown_body` (sections) + the heading check (documents), and
  **every sectioned run stores `title = NULL`**, since the heading is the
  server's and `extract_report_title` finds none — exactly what a
  repaired-heading run already does. `section_request` **forbids
  meta-narrative**: the route the run took is not a finding (the trace is
  journalled and streamed already), and "not answered" is narrowed to "nothing a
  tool returned bears on it" — sections were arriving titled after a step of the
  plan (`## 1. File discovery bypassed`) and spending the whole allowance
  explaining a shortcut, which costs a section the reader can use.
- **A section body may carry only its own `## N.`** — a reply whose numbered
  headings are all *other* items is not this section written badly, it is
  somebody else's section reproduced, and the server refuses it inside the
  attempt loop rather than wrapping it. The wrap at the end
  (`## N. {item.text}` over the raw text) exists for a reply with **no**
  numbered heading; handed a copy it pasted a whole second document under a
  heading of its own — measured, six headings for four plan items, sections 1-2
  shipped twice. Salvage first: the prose before the first foreign heading
  (`prefix_before_first_numbered_heading`, reading `section_heading_number` so
  the cut and the parse cannot disagree about what a heading is) when it clears
  `MIN_TRUNCATED_SECTION_CHARS`; otherwise the attempt is spent and the existing
  `fallback` ships this item's banked draft. The prompt half moves with it: the
  headings quoted back as "must not repeat" are **titles**
  (`heading_line_of`, bounded by `MAX_HEADING_REMINDER_CHARS`) — `#3` named
  nothing a model could recognise in its own prose — the banked draft is
  injected **without** its heading line (material to expand, not a document to
  copy), and the tail says the item's heading is the reply's only numbered one.
- **Citation repair regenerates one section, not the document.** The
  whole-document rewrite says "repeat everything that should survive" — a second
  full-volume generation of what just failed, when the run has least budget left.
  `defective_sections` maps each failing citation's offset to its section
  (`parse_citations_at` gives bytes, converted to **chars** at that one seam —
  a report is arbitrary UTF-8), and only those are rewritten, capped by
  `MAX_SECTION_REWRITES` (3). `rewrite_sections` is infallible: a report already
  exists, so a failure costs the repair, never the run.
- **Checkpoints make a stopped run return findings.**
  `[research].checkpoint_every_steps` (6, `0` = off; a request overrides it via
  `budget.checkpoint_every_steps`, `0` = off for that run, capped by
  `max_request_steps` — an interval above the step budget is `0` spelled
  differently) interrupts the tool loop to
  bank the sections already answerable into `RunState::draft_sections`, keyed by
  plan item and **replaced, never merged** (a later checkpoint saw more
  evidence). It **costs a step** *and* is capped by `MAX_CHECKPOINTS` (8):
  charging it makes it visible in the operator's budget, the cap stops a mis-set
  interval eating a run. It emits **no `step` event** — no tool call, and a
  `step` frame with no argument key breaks
  `each_action_names_its_argument_on_the_wire`; a step invisible on the wire is
  sanctioned (a rejected duplicate already is one) and pinned by a test. Its
  reply is **replaced by a stub** in the transcript (`shed_for_report`'s
  technique, a different reason) naming the numbers it banked: the text lives in
  `draft_sections`, whose three readers are the section fallback,
  `section_request`'s draft and `forced_synthesis`, so a verbatim copy informs
  nobody while sitting in the prompt of every later turn as a finished report —
  including each section turn, which `write_sectioned_report` otherwise keeps
  free of other sections' prose by popping its own request. One duly copied it.
  Payoff:
  a section that cannot be written now ships its banked version, and
  `forced_synthesis` assembles real findings instead of "No report was
  produced." Cost: ~15% of `medium`'s lookups become writing turns — **measure
  coverage on and off at the same seeds** before trusting the default.
- **Output volume, not retrieval, is where runs fail** (measured in the field;
  full record in `docs/claude/research.md`). Nothing had ever bounded the
  report: no length in any prompt, and `num_predict` sent on no turn ever.
  `[research.effort.*].max_report_words` (400/900/1800, `0` = off) is announced
  as a **ceiling** — "at most", never "about" — and arms `num_predict` at
  `REPORT_WORDS_TO_TOKENS` (4) × the grant, ~3× the prose ratio so only a
  runaway meets it: a tight cut severs a fence, fails the markdown gate, and
  buys a full-volume rewrite of what just failed. Request-overridable since the
  shape knobs shipped (`budget.max_report_words`, `0` or
  `150..=[research].max_request_report_words`) — safe because the *ceiling* is
  held to the same startup window check as the presets (`words × 4 <
  max_num_ctx_tokens / 2`), so no override can arm a `num_predict` the window
  cannot hold. The numbers are **unmeasured**; `0` is what keeps that honest.
- **The report turn's prompt is guarded too.** `context_fraction` only checks
  *between* turns against the *previous* turn's size, so the run's largest
  prompt (report turn + notes, or the rewrite with the whole draft) went
  unmeasured. `estimate_prompt_tokens` + `shed_for_report`: prior reports go
  first (hearsay, never citable), then oldest `role:"tool"` replies, each
  **replaced by a naming stub, never removed** (the pairing invariant is
  absolute); instructions/question/plan/notes/verdict/digest/request are never
  shed, and the shed is announced in the prompt. `tally.num_ctx == 0` skips the
  guard rather than substituting the configured ceiling. An **evidence digest**
  (paths + merged spans, no code) is pushed unconditionally — it is what
  `check_citations` scores against, and what makes shedding safe.
  `CHARS_PER_TOKEN_ESTIMATE` (4) is prose-derived and code tokenizes denser;
  this path may never fire on this hardware, so test it, don't measure it.
- **Citations are provenance-checked server-side** (`parse_citations` →
  `Evidence` → `CitationReport`; the `citations` event): `verified` /
  `path_only` / `unverified`; range existence deliberately unchecked; parser
  requires a file extension + relative path. The report turn writes a
  **draft** with the content gate closed; a failing draft is sent back with
  the offending *locations* (revalidation re-opens tools only when
  `reason == Finalized`, `MAX_REVALIDATION_STEPS` 4 /
  `MAX_REVALIDATION_TURNS` 3; revalidation steps don't increment `steps`); a
  failed rewrite ships the draft; draft counts ride as `draft_*`, null when no
  repair. An **ungrounded** report (`total: 0`) also trips the gate
  (`format_ungrounded_complaint`), with two load-bearing exemptions:
  `evidence.paths()` empty, or under `MIN_GROUNDED_REPORT_CHARS` (800) **from a
  budget-stopped run only** — a run that `Finalized` declared its evidence
  sufficient, so a short uncited report from it is a self-contradiction.
- **A cited path is resolved before it is scored** (`Evidence::match_path`;
  resolution is a field on `Evidence`, so `check_citations`, the complaint and
  `defective_sections` physically cannot disagree about a verdict). Exact
  first; otherwise the cited path may be the **tail** of exactly one shown path,
  the `/` in the suffix being the whole segment-boundary rule. Two candidates =
  none, and the complaint's own bucket asks for the directory. The measured
  failure it fixes is not a parser gap — `parse_citations` accepts
  `research.rs:5068-5291` — but `Unverified`, the verdict for a path *no tool
  returned*, being handed to a report about a file it had just read; five of
  twenty-seven in one run, and repeated inside the section the repair had made
  it rewrite. Two spellings therefore live side by side and must not be
  swapped: the **cited** one in anything quoting the report (`details`,
  `unverified_paths`, every complaint line — the model has to find the string
  it wrote), the **resolved** one in anything claiming something about a file
  (`cited_paths`, `stale_paths`, and above all `verified_locations`, which the
  excerpt channel reads code from — the report's spelling finds nothing there
  and a resolved citation would silently ship no code). `is_stale` resolves
  too, or resolution would admit exactly the citations whose staleness then
  went unreported. `citations.path_resolved` is the honesty counter: `verified`
  now means "a path a tool returned, identified unambiguously from what the
  report wrote", and this says how many leaned on the second half. Not
  journalled (`research_run_citations` stores the cited spelling, so a resolved
  row no longer joins to `research_run_evidence.path` — that wants the
  migration above).
- **A run that looked at nothing while holding hearsay is refused**
  (`citations.hearsay_only`, `format_hearsay_complaint`). The ungrounded gate's
  first exemption exists for a run with **nothing to say**; a run handed prior
  reports or a challenge subject has somebody else's answer to hand, and the
  same uncited prose is then that answer restated as findings — cited to
  nothing, in the field callers are told to trust, and byte-for-byte identical
  on the wire to the honest "nothing in this scope was shown to me". So the
  exemption is `!evidence.paths().is_empty() || hearsay_only`, and everything
  downstream (`citation_defects`, `tools_reopen` on `Finalized` only, the
  complaint push) is reused unchanged. The flag is computed **in
  `research_inner`** and carried on `ResearchOutcome`: `run_research` has only
  `content_paths` (span-bearing paths) while the gate asks whether any path was
  *named*, and the two must be one predicate. Not equivalent to
  `shown_paths == 0` in either direction — a run that called only `list_files`
  has the zero without the flag. The complaint is a separate function, not a
  branch of `format_ungrounded_complaint`, whose whole body is a list of shown
  files that is empty here; its remedy is two-sided, because "this run added
  nothing and the answer rests on run N" is a legitimate report — it just has
  to say so.
- **`citations.server_written` is not a nicety.** A `forced_synthesis` report
  cites nothing by construction, so `check_citations` scores it
  `total: 0, verified: 0, unverified: 0` — byte-for-byte what a clean report
  scores, in the field scout tells callers to trust. Every "verified 0 even
  though it read the files" is that collision. The flag comes from the same
  `RunTools.forced_synthesis` the journal already recorded; the fact existed
  and never reached the wire. Its sibling **`citations.shown_paths`** is the
  denominator the counts never had — how many files the run was shown the
  **inside** of (`Evidence::content_path_count`: paths with at least one
  recorded span). Deliberately *not* `file_baselines.len()`, which it was at
  first: that is every path any tool *named*, so one `list_files("**")` reported
  188 for a run that read one file — inflating the denominator exactly where
  `verified: 0` needs explaining. A span is the right discriminator because it
  is the citation verdict's own: a span-less path can only ever score
  `path_only`, never `verified`. The baselines keep the wider set on purpose —
  freshness must watch every path the run saw named. It is what makes
  the admission rule `steps > 0 && verified > 0` a *machine* check rather than
  a reader's discipline: `verified: 0` over `shown_paths: 12` cited none of what
  it read, while over `shown_paths: 0` it is the honest "nothing in this scope
  was shown to me" — the one case the ungrounded gate exempts, and therefore
  the one that reaches a caller looking exactly like a clean run. scout owns
  the rule in `_INSTRUCTIONS`.
- **The `excerpts` event ships code, so the model never has to.** Between
  `citations` and `done`: the indexed code at every **verified** citation,
  verbatim, via `read_chunks_core` (pure SQL, **scope-enforced** — this must
  not become how a scoped run leaks refused bytes). `path_only`/`unverified`
  are excluded (no location worth reading; real bytes would dress up a refused
  claim). `MAX_EXCERPT_CITATIONS` 24 / `MAX_EXCERPT_BYTES` 256 KiB, enforced by
  dropping **whole chunks** — never cutting one, since a report and its code
  are both arbitrary UTF-8. Best-effort: a failure costs the excerpt, never the
  run. This is what makes the prompt's "do NOT reproduce code you were shown"
  honest; the two ship together. Scout returns `excerpts_available` always and
  the bytes only under `include_excerpts=True` (~100 KB is the cost scout
  exists to prevent).
- **Indexing is never blocked by research; the run reports what moved.**
  Per-file consistency holds (the prepare tx is atomic); currency:
  `probe_freshness` re-reads `baseline_sha` per shown path before every turn +
  the report turn; `changed`/`removed` sticky, `in_flight` not; staleness is
  orthogonal to provenance (`citations.stale`) and joins the revalidation
  gate. `apply_versions` takes the *asked* path list; a failed probe changes
  no verdicts.
- **Every finished run is journalled** as one `research_runs` row **plus its
  structured children, in one transaction** via the `ResearchJournal` seam
  (`db/research.rs`): `research_run_files` (baselines),
  `research_run_evidence` (the shown spans — the one `check_citations` input
  that otherwise dies with the run, and what makes the offline re-verify
  possible), `research_run_citations` (per-occurrence verdicts, report order,
  duplicates kept) and `research_run_steps` (calls + arguments + landing spans,
  **no result bodies** — the code is in the index). The trace rows are built at
  the same sites as the `step` SSE frames from the same locals, so wire and
  journal cannot drift; checkpoints write no trace row (they emit no `step`
  frame either). Best-effort (`warn!`, never a failed run); **no FK to
  `project_files`** (must never surface in `/drift`); unset sampling = NULL —
  as is everything the environment did not provide: `model_digest`/
  `model_details_json` come from the model-catalog snapshot at admission (NULL
  until its first tick — never a fabricated identity), `top_p`, the four
  previously-unjournalled grants, `checkpoint_every_steps`/`checkpoints_taken`,
  the `revalidation_*` counters, `sufficiency_verdict`, `embedder_model_id`,
  `server_version`, `started_at` (admission wall-clock; `created_at` is the
  insert, i.e. the run's *end*). `kind` stays at its DEFAULT `'research'` —
  the challenge endpoint writes its own rows. `NoJournal` is
  `#[cfg(test)]`-gated.
- **The opponent**: `POST /v0/{guid}/research/{run_id}/challenge` is a research
  run whose subject is a stored report — same loop, same semaphore (a second
  pool would over-admit the GPU), same budgets, same citation gate on its own
  report. The subject is injected as hearsay-under-examination (**never seeds
  Evidence** — re-deriving every location through the tools *is* the
  refutation; `a_challenged_report_never_seeds_the_evidence`); the plan turn
  asks for the report's claims (`challenge_plan_request`); a closing toolless
  verdict turn scores each claim with the dictated vocabulary
  CONFIRMED/DISPUTED/REFUTED (the sufficiency-turn mechanism — never JSON).
  **The grounding cap is what makes this safe with weak models**, and it is
  symmetric: `grounded` = `verified > 0 AND unverified <= verified` (majority,
  because one surviving citation out of nine was enough to launder a challenge
  that had checked nothing — measured); an ungrounded `refuted` caps at
  `disputed` (an unshown accusation can dispute but never refute) and an
  ungrounded `confirmed` resolves to NULL (an unshown *acquittal* is not one).
  `disputed` passes through either way. Zero
  parseable verdict lines → NULL = "challenged, inconclusive", which **no
  reader may render as an acquittal**. Every failure shape of the verdict turn
  degrades to inconclusive, never a `break` (the counters-not-clock rule holds
  with no new counter). The subject must be **valid** (400
  `research.challenge_subject_invalid` — staleness must not be spendable as
  refutation) and must not itself be a challenge (400
  `research.challenge_subject_is_challenge`; trust aggregation is
  single-level). The scope is the subject's own, re-inhabited from
  `scope_spec_json` (the structured twin `scope_json` cannot provide — it is
  rendered prose); a pre-v1.3.0 scoped run is refused (`scope_unavailable`).
  **Trust is derived at read time, never stored** (`research_trust_column`,
  the validity philosophy): over *valid* challenges only — a stale challenge
  stops counting by itself — severity wins (`refuted` > `disputed` >
  `confirmed` > `unchallenged`), inconclusive counts toward none.
  **One challenge per report, newest verdict wins**: a challenge journalled
  with a **parseable verdict** deletes every earlier `kind='challenge'` row
  aimed at the same subject, inside `insert_run`'s own transaction — so a
  failed journal (best-effort, `warn!` + `None`) cannot destroy the standing
  verdict on behalf of a run that left no trace, and the `id <> ?3` in that
  DELETE is what stops it deleting itself. An **inconclusive** run evicts
  nothing: it produced no finding, and letting it erase a `refuted` would spend
  the mechanism's most valuable output on its least informative outcome — so a
  subject can legitimately carry an old verdict plus a new inconclusive row,
  and the severity fold stays (pre-rule databases hold several anyway).
  Not backed by a unique index on purpose: that would make the *insert* fail
  instead of evicting, the opposite of "newest wins". Counted by
  `mindex_research_challenges_replaced_total` (rare counter, `increase()`) and
  an `info!` after the commit — the eviction leaves no other trace. Surfaced as
  `kind`/`challenged_run_id`/`challenge_verdict`/`trust` on every summary,
  plus `challenged_seq`/`challenged_title` — the subject **resolved
  server-side** (`RESEARCH_SUMMARY_COLUMNS` is 26 now), because a challenge row
  must name what it attacked wherever it is rendered and the client used to
  hunt for the subject among the rows it happened to hold; NULL now means the
  subject is genuinely gone. In `list_research` lines and as a
  warning block in `read_research` (a refuted report must not read as settled),
  in VS Code badges, and by scout (which also owns the `challenge` MCP tool).
  Wire: **one** new event, `verdict`
  (`{challenged_run_id, overall, grounded, claims}`), challenge streams only,
  after `excerpts`/before `done`; an ordinary stream is byte-for-byte
  unchanged (`an_ordinary_run_emits_no_verdict_event`,
  `verdict_wire_fields_are_stable`). `worker::research_stats` filters
  `kind='research'` (`observed` is a promise about `POST /research`);
  `mindex_research_challenges_total{outcome}` counts verdicts
  (rare counter — `increase()`, never `rate()`). **The cap's own firing is
  counted, not published**: `ChallengeOutcome.capped` (derived at the call site
  as `worst_claim_verdict != resolve_challenge_verdict`, so the severity fold is
  never copied) drives `mindex_research_challenge_verdict_caps_total` and a
  `warn!` naming both verdicts. It is deliberately **absent from the `verdict`
  event** — "it tried to refute and was not allowed to" is precisely the
  inference the cap exists to forbid, and a wire field saying it would read as
  the verdict underneath the verdict. The counter exists because the override
  leaves no other trace: a capped accusation and an honest `disputed` are the
  same row, so this is the only answer to "does the cap ever fire here".
- **Stored runs as context**: `context_run_ids` injects prior reports before
  the plan turn. **Prior reports are hearsay** — never seeded into `Evidence`
  (`a_prior_report_never_seeds_the_evidence`); truncated with a marker at
  `[research].max_context_chars`. Staleness is per-path via
  `research_run_files` (a global counter was rejected): the join is
  `(project_guid, path)` against `project_files` — the run's baseline `sha256`
  against the current one, and it no longer carries a `model_id` half, because
  which model embedded a file says nothing about whether the file moved.
  `path` carries **no FK** (RESTRICT would brake GC, CASCADE
  would fake freshness); list filters apply **inside** the cursor-bounded
  subquery, before `LIMIT`.
- **Validity is derived, never stored**: `research_validity_ctes` — one
  recursive CTE over `context_run_ids_json` (`valid = own files unmoved AND
  every parent exists AND is valid`); dangling ids read as invalid with no
  write; cycles impossible (edges point backwards). Each summary carries
  `valid`/`invalid_reason` + `context`; invalid context in a request → 400
  `validation.research_context_invalid`.
- `list_research`/`read_research` browse the corpus (valid runs only, capped
  at `LIST_RESEARCH_LIMIT`), deliberately **unscoped**, both return
  `shown: []` (`read_research_never_seeds_the_evidence`); invalid seq =
  explicit refusal.
- **`title`** = the report's first ATX heading (`extract_report_title`,
  fallback: derived `research_title`); **`seq`** = per-project keyset cursor
  (never `OFFSET`), not identity — mutating endpoints key on the uuid `id`.
  **`expires_at IS NULL` = pinned** — the whole retention mechanism; stamped
  at insert; unpinning restores `created_at + retention`.
- **Markdown gate**: `validate_report_markdown` (four shape checks — empty,
  JSON start, no leading `#` heading, unclosed fence); a failing draft joins
  the citation complaint but re-opens **no** tools; a failing final report is
  streamed but **not journalled** (`done` carries null `run_id`/`seq`);
  `forced_synthesis` is exempt
  (`forced_synthesis_passes_the_markdown_gate`). **The missing heading is
  repaired, never refused** — `repair_missing_heading` writes
  `# {research_title(question)}` when that is the *sole* problem (a report also
  starting with JSON or leaving a fence open is still refused: a heading over
  JSON would pass the gate and remain unusable). Two sites, each **after** its
  `check_citations` — the derived heading comes from the question, which can
  itself contain a `path.rs:1-2`, and a server-written line must never enter
  the provenance report. The draft site (`research_inner`) also spares the run
  a rewrite turn; the final site (`run_research`) catches a *streamed* rewrite,
  so there — and only there — the stored report carries a line the live view
  did not show. `title` is read **before** the repair, so a repaired run stores
  none and its readers fall back to the question the heading came from.
- **A scope that admits no file is refused at admission** (400
  `research.scope_matches_nothing`, in `launch_research_job`, before the
  permit — so it covers the challenge entrance too). One indexed `COUNT` over
  `scope_subquery`, and only for `is_scoped()` runs, so an unscoped one is
  unchanged by construction. Without it such a run refuses every lookup and
  then reports the question unanswerable, which reads as a finding about the
  code: the commonest spelling (`"src/"`, where SQLite `GLOB` wanted
  `"src/**"`) cost a measured 302-second run with zero citations and no error
  anywhere.
- **Budgets**: `effort` selects `[research.effort.{low,medium,high}]`; a
  request overrides axis-by-axis (`Budget::resolve`), capped by
  `[research].max_request_{seconds,tokens,steps,report_sections,report_words}`
  + `max_evidence_width` (edge check `validate::research_budget` → 400; the
  shape axes get their own code, `validation.research_shape_out_of_range`,
  because they carry floors and two accept `0` = off; config validation
  rejects any ceiling below `effort.high`). `GET /config` publishes ladder +
  ceilings + `max_concurrent`, the context caps, a derived `worst_case_seconds`
  per level (`max_seconds + report_timeout_ms`, since the two bound *different
  phases* and reading the first as the whole wait understates `high` by five
  minutes) and `observed` — measured p50/p90 per `(model, effort)` from
  `research_runs` (`worker::research_stats`, model-catalog tick and its
  keep-the-last-snapshot rule; a pair under `MIN_RUNS_FOR_ESTIMATE` is absent
  rather than noisy). The ladder says what a level *grants*; `observed` is the only
  thing that says what it *takes*, which is what makes `effort` priceable before
  the fact. The axes:
  - **`max_seconds`** (300/900/3600) is a HARD deadline — poll **and**
    `DeadlineToken` (child of the job token; `stopped_by` tells it from a
    disconnect, job token tested first). A deadline stop is not a failure.
  - **Report window** `[research].report_timeout_ms` (120 s), token a child
    of the **job** token (never the budget one); expires with a draft → ship
    it; with nothing → `forced_synthesis`. A truncated run names its limit in
    the report; the sufficiency turn is skipped.
  - **`turn_timeout_ms` must sit ABOVE every budget** — startup-enforced
    (`validate` refuses `<= max_request_seconds`); it is a dead-socket guard,
    not a bound. Its blind spot is a socket that is **alive and mute**:
    `[research].first_token_timeout_ms` (120 s, `0` = off) abandons a turn that
    produced no token of any channel, as `OllamaError::Silent` → a named
    `ollama.unavailable` failure instead of a whole budget spent waiting on an
    Ollama that is (re)loading a model. It bounds the **silent prefix only** —
    armed across `post_chat` *and* the wait for the first delta (the stall can
    be in either; Ollama holds the connection open while it loads), spent by
    the first thinking/content/tool-call delta. Startup keeps it strictly under
    `turn_timeout_ms` (above it, it could never fire) and at/above 5 s (below,
    it preempts a merely long prompt evaluation).
  - **Runaway-thinking guard** `[research].max_turn_thinking_chars` (8192,
    `0` = off): abandons the turn as an **empty** `ChatOutcome` (every phase
    already recovers from one); instrumented in place
    (`research_runaway_thinking_turns` + `warn!`); `TokenTally::record` must
    not let its zero `num_ctx` overwrite a known window; its GPU cost lands
    in `turns_unreported`.
  - **Contention, named** `[research].slow_turn_tokens_per_second` (**`0` =
    off, and that is the default**): the third per-turn guard and the only one
    that stops nothing. A run inching along at a token a second is the same
    symptom as a broken model, a bad prompt and a wedged server — one measured
    run spent 985 s at ~1.5 tok/s and nothing anywhere could say why, because
    Ollama's `load_duration`/`eval_duration`/`total_duration` were parsed by no
    one. Now they ride on `ChatOutcome` (nanoseconds, `None` ≠ 0 — a zero
    `load_duration` *means* the model was resident), sum into `TokenTally`
    independently of the token counts, and reach the wire as
    `generation_ms`/`model_load_ms`/`unaccounted_ms`/`eval_tokens_per_second`
    on `progress` and `done`. **The wall clock is not redundant**: Ollama's
    `total_duration` is measured inside its own handler, so time queued behind
    another client on the same GPU falls entirely outside it — during exactly
    the contention being hunted, Ollama's numbers look healthy. Hence
    `unaccounted_ms` = wall − total, named for what it measures (it also holds
    HTTP/TLS/NDJSON) rather than for what a large value means. The rate is over
    *generation* time, so waiting shows in `unaccounted_ms` instead of being
    averaged into it and hidden. The `warn!` and the two histograms
    (`research_turn_tokens_per_second`, which needs a `BARE` entry since
    `_per_second` is not `_seconds`; `research_turn_load_seconds`) live in
    `ollama.rs` beside `warn_if_context_exhausted` — the one place holding the
    model name, the metrics handle and the timings at once. The default is `0`
    because a healthy rate is a fact about one model on one host (15 tok/s is
    fine for a 30B, alarming for a 7B), the `temperature = None` argument;
    read the histogram, then set it. **It answers only half the question, and not
    the half it was built for.** The rate is over Ollama's *own* generation clock,
    which is taken inside Ollama's handler — so a turn slowed by *queueing*
    generates at full speed while it holds the device and scores healthy. Measured
    2026-08-03: a plan turn took **912 s** of wall clock for 702 tokens while every
    one of the preceding week's 220 turns sat between 32 and 128 tok/s; the check
    would have returned early at any threshold. The 890 lost seconds were in
    `unaccounted_ms`, which reached nobody — it appeared in the code exactly once
    outside its definition, as a *field on the warning the rate check emits*, i.e.
    only after the rate check had already fired. So the companion knob
    **`[research].slow_turn_unaccounted_ms`** (60 000, `0` = off) warns when a turn
    spent that much wall clock outside Ollama's accounting, and
    `research_turn_unaccounted_seconds` measures it unconditionally. The two checks
    are **independent, and neither may gate the other** — they were one, with the
    rate tested first under an early `return`, which made the second unreachable in
    the only case it exists for (`a_turn_that_reports_no_rate_still_records_the_time_ollama_did_not_account_for`).
    Unlike the rate, this one ships a non-zero default on purpose: unaccounted time
    is not a fact about a model or a host, it is time nobody was computing for this
    request.
  - **`[research].max_turn_seconds`** (300, `0` = off) is the only one of the three
    that *stops* anything, and the reason the other two were not enough. Every other
    per-turn bound misses the observed shape: `turn_timeout_ms` is a dead-socket
    guard and sits **above** every budget by design, `first_token_timeout_ms` is
    spent by the first delta and cannot see a turn that starts fine and then
    dribbles, and `max_turn_thinking_chars` counts characters a stalled turn does not
    produce. Measured twice on this host — 985 s at ~1.5 tok/s, and 912 s for 702
    tokens — **one plan turn consumed a whole run's wall clock and the run ended with
    zero steps**. The turn is abandoned as an empty `ChatOutcome` (the
    runaway-thinking mechanism, which every phase already recovers from), so the run
    continues with the budget that is left rather than returning nothing. Checked in
    the delta callback, not on a timer: a stalling turn still emits deltas, and one
    emitting none is `first_token_timeout_ms`'s job. Startup refuses a value below
    `report_timeout_ms` (it would truncate the one legitimately long turn) or at/above
    `turn_timeout_ms` (it could never fire) — both are protections that read as armed
    and are not. Counted by `research_stalled_turns_total`, which unlike its
    neighbours is **not** expected to stay at zero on a shared GPU.

  **All three ship armed** (`the_contention_guards_are_armed_out_of_the_box`). The
  rate one used to default to `0` on the argument that a healthy rate is
  host-specific — true, and the wrong conclusion, since it stops nothing and a false
  positive costs one log line while default-off cost two multi-hour investigations of
  a symptom the code could already name. A guard that ships disabled is not a guard;
  `0` remains the escape hatch (CPU-only inference of a large model legitimately runs
  under 3 tok/s).
  - **`max_tokens`** (400k/1.2M/6M) is the cost axis (`prompt_eval + eval`;
    transcript resent every turn → super-linear in turns).
  - **`context_fraction`** (0.5/0.7/0.85): a guard against Ollama's silent
    transcript trim, checked against `tally.peak_prompt_tokens` *before* the
    next turn; not request-overridable, with `search_top_k` — the two axes a
    request cannot touch.
  - **`max_steps`** (8/20/64): coarse backstop (a step is a poor unit).
  - **`search_top_k`** (5 at every level) is width, not budget; config
    validation refuses `> [search].max_top_k` at **startup** (`search_core`
    leaves validation to callers). Deliberately still TOML-only: widening it
    was measured not to fix the failures it looks like it would.
  - **Shape axes** (request-overridable, resolved like the rest by
    `Budget::resolve` except `checkpoint_every_steps`, which the handler
    resolves into `ResearchParams` — it is a `[research]` scalar, not an
    effort axis): `max_report_sections` (6 at every level, `3..=12`; floor =
    `MIN_SECTIONED_PLAN_ITEMS`, kept as `config::MIN_REPORT_SECTIONS` so the
    validators share it), `max_report_words` (see the output-volume bullet),
    `checkpoint_every_steps` (see the checkpoints bullet) and
    `evidence_width` (1 at every level, `1..=[research].max_evidence_width`) —
    an integer multiplier on `READ_CHUNKS_LIMIT`/`GREP_LIMIT`/
    `FILE_HISTORY_LIMIT`/`SYMBOLS_LIMIT`, threaded as a `limit` param into the
    research-only core fns and stored on `StateResearchTools` (constant per
    run, so the `ResearchTools` trait is untouched). It deliberately does
    **not** scale `outline`/`list_files` (navigation — when 300 rows bind the
    fix is a narrower glob), `search` (its own axis), or `MAX_EXCERPT_*`
    (response caps). Width is resent every turn — it compounds into
    `max_tokens`. None of the shape grants are journalled (shape knobs never
    were); the resolved values ride on the "Starting a research job." log
    line, the only record of what a run was actually granted.

  Each stop has a loop-level test; the time one uses **real** `Instant` in
  small increments (`tokio::test(start_paused)` does not move it).
- **`progress`**: `RunProgress` (spent vs granted per axis + `turns` +
  `binding` + `shares`) emitted before the first turn, after every step and turn;
  `done` carries it + `reason`. **No ticker** (would race the cancellation token
  and make tests clock-dependent). `binding` names the axis with the largest
  **share spent** — a maximum, not a warning, and not what stopped the run
  (`done.reason` is); its fold seeds at `Time`, so an all-zero progress reports
  `"time"`. It was read as "about to run out" for its whole life, so `shares` (the
  four percentages it is chosen from) now ships beside it and scout promotes both.
- **A step reports where it landed** (`spans`, `path:start-end`, from the same
  `shown` locations citation provenance is scored against; capped with
  `spans_truncated`). `hits: 3` on a 4000-line file names no lines, which made the
  trace unusable for the only thing it is for.
- **The identifier rule governs code; documentation inverts it.** `*.md` is
  indexed, and `system_prompt`'s identifier paragraph carries the exception
  (*documentation is written in English; ask it in English*) — the corpus
  half and the prompt half must never ship apart. Any future prose channel
  inherits this.
- **`outline`/`list_files`**: pure SQL (`outline_core`/`list_files_core`,
  `idx_project_file_symbols_file`); intended path `list_files → outline →
  symbols/search/grep → read_chunks` (the prompt says so — half the
  feature). `outline` reports `indexed` separately from an empty symbol list.
  `list_files`' glob is SQLite `GLOB` (`*` crosses `/`, unlike `.mindex`).
  Post-stream errors are `error` events; `NoMatch` is a tool result.
- **Scope is enforced on every tool**: `ResearchTools` takes a `ToolScope` as
  a required argument on every model-facing method (a later tool cannot be
  the next exception). Evaluated in SQLite by `build_file_filter`, appended
  as a `file_path IN (SELECT …)` subquery, not a join. Path-keyed tools
  (`outline`, `read_chunks`): explicit refusal (`in_scope` flag);
  name/text-keyed (`symbols`, `grep`): rows dropped **and
  counted** against one unscoped `COUNT(*)`. All gated on `is_scoped()` —
  unscoped runs build byte-for-byte the old SQL (public `/symbols` provably
  unaffected). `SymbolsRequest.include/exclude` binds append **last**.
  `file_versions` is unfiltered (asks only about shown paths). The scope is
  *told* to the model (one `ToolScope::describe` feeds prompt + state note).
- **`note`/`revise_plan`** mutate the run (`apply_local`, bypass
  `ResearchTools`), each costs a step; notes pinned into the state note every
  turn + pushed before the report turn; caps announced, never silent
  (`MAX_NOTES` 24, `MAX_NOTE_CHARS` 500). **`grep`** is a case-insensitive
  `LIKE` over `project_file_chunks.code` (`grep_core`); **`like_escape` is
  mandatory** (`_` is a wildcard); reports line + chunk span; bounded by the
  scope subquery, `GREP_LIMIT`, `GREP_MIN_PATTERN_CHARS`. FTS5 deferred. **An
  empty result has three meanings, not one** — out-of-scope, nothing
  searchable, genuinely absent — so `GrepResponse` carries
  `searched_chunks`/`searched_files` (one extra `COUNT` over the same scope
  subquery, read **only on a miss**, the `out_of_scope` probe's rule) and
  `format_grep`'s empty branch is three-way. A glob matching no file used to
  read as proof of absence, which is how one run honestly reports 0 hits for a
  literal the next finds 5 times. A miss whose pattern carries regex
  punctuation additionally says the match was literal
  (`regex_metacharacter_hint`): `tool_specs` says so too, but that is read once
  at the start of a run, while this is read at the moment the mistake costs
  something — `\.bwp` → 0 and `.bwp` → 7 is a false negative the reader is
  handed as proof of absence.
- **An LSP client in mindex is refused, and this is where that is recorded.**
  The obvious cure for lexical edges is name resolution, and the obvious source
  of it is a language server — optional like Ollama, consulted when the project
  is reachable and the indexed file is current. It is refused on rule 10, not on
  effort: **the server never walks a tree**, it believes what a client posts,
  and an LSP server is the opposite — it reads the live worktree itself, one
  process per project *and* per language, with a startup index measured in
  minutes and gigabytes. Wiring one in makes mindex the first component owning
  its own working-tree view, which is the divergence class that surfaces as
  phantom drift rather than as an error. The currency check that makes the idea
  sound cheap is exactly where it bites: comparing the indexed sha256 against an
  editor buffer means mindex reading the file. And the consumer it would serve
  is already served — an agent harness exposes LSP to the frontier model
  directly; the only consumer that cannot reach one is the local research model,
  and serving *that* is precisely the frame break. If exact resolution is ever
  wanted here the rung is **SCIP/LSIF artifacts posted by a client**
  (single-producer, the git-history pattern), never a language server the index
  talks to.
- **The loop terminates on counters, not a clock** (regression guard): every
  iteration breaks or increments exactly one of `steps`, `parse_retries`
  (≤ `MAX_PARSE_RETRIES`), `duplicate_calls` (≤ `MAX_DUPLICATE_CALLS`); one
  level up, `reopens ≤ MAX_REOPENS` and the revalidation caps. A new
  rejection path needs a new bounded counter — or price the refusal as a
  **step** (what every refusal added since does).
- **A plan turn opens the run, a sufficiency turn closes it** (both
  `NO_TOOLS`) — the thinking channel is discarded from the transcript
  (`ChatMessage` has no `thinking` field), so the plan is pushed back as an
  **assistant** message; degrades to a plan-less run. Sufficiency re-opens
  only on model-chosen stop + an unspent axis + `declares_unanswered`
  (server-dictated vocabulary); the re-open nudge is the one place
  `revise_plan` is offered by name.
- **The run-state note is pinned, not appended** (`RunState` →
  `format_state_note`): one `user` message rebuilt and re-pushed before every
  turn, placed after the previous turn's `role: "tool"` replies.
- **`num_ctx`** = `min(model limit from /api/show, [research].
  max_num_ctx_tokens)` — a VRAM ceiling (default 131072), not a window; an
  unreachable `/api/show` degrades to the ceiling, never zero.
- **One `/api/show` per model per process, two facts** (`ShowFacts`, cached
  together because it is one request — two caches would disagree about a model
  re-pulled between them): the context length above, and `capabilities`. The
  second is the **pre-flight** half of `research.model_lacks_tools`, checked in
  `launch_research_job` before the permit like the scope count, so it covers the
  challenge entrance too. **Three-valued, and the third value is load-bearing**:
  only `Some(false)` refuses; `None` (unreachable Ollama, or one too old to have
  the field) must let the run proceed — a pre-flight that cannot be performed is
  not a refusal, and the trait's default impl returns `None` so no fake opts into
  a refusal it cannot substantiate. `Some(true)` is **not** a promise, which is
  why the mid-run symptom check (`looks_like_tool_call_attempt`) stays exactly
  where it is: a model can declare `tools` and have a template that never emits
  them. Both report the same code at different planes — a 400 before the run, an
  `error` event during it. Present on this host: `qwen2.5vl:7b` declares
  `["completion","vision"]`, and used to cost a slot, a model load and a turn to
  discover it.
- **Model catalog** (`worker::ollama_catalog` → `research.models` in
  `GET /config`, refresh `models_refresh_interval_seconds` 300): a failed
  tick keeps the previous list (`refreshed_at` not re-stamped — the only
  "no models" vs "never reached" signal); gated on nothing; never primed at
  startup; `health()` is a provided method over `list_models`; the
  `/api/tags` reader is `#[serde(default)]` throughout.
- **`[research].allowed_models`** (glob whitelist, compiled once at startup;
  empty = any): the resolved model is checked in `post_research` *before* the
  semaphore → 400 `research.model_not_allowed`; `GET /config` publishes
  `research.models` already filtered by it plus the raw patterns; a non-empty
  list must cover a non-empty `default_model` — startup refuses otherwise
  (every defaulted request would 400). Match is case-sensitive and includes
  the tag: `"gemma4:*"` does not cover bare `"gemma4"`.
- **Ollama errors**: `chat_stream` reads the error body (never
  `error_for_status`); a 500 containing `error parsing tool call` is resent
  with the same transcript at the next seed (`MAX_TOOL_CALL_PARSE_RETRIES`;
  safe because the 500 precedes the stream); anything else fails with
  Ollama's own words. Token counts are `Option` → `turns_unreported`; the
  WARN when `prompt_tokens` reaches `num_ctx_tokens` is the *only* symptom of
  a silently truncated transcript.
- **`Step` carries a typed `StepCall`** (each action names its own key on the
  wire). **SSE contract lives in four places that move together**:
  `post_research`'s doc comment, its `#[utoipa::path]` 200 description, the
  VS Code client (`api.ts` + `researchView.ts`), scout's reader (whitelists
  silently drop unknowns). One `data:` line per frame
  (`serde_json::to_string` escapes newlines) — keep it that way. Pinned by
  `progress_wire_fields_are_stable`,
  `done_event_carries_the_reason_and_the_run_cost_on_the_wire`,
  `done_names_no_run_when_the_journal_write_failed`,
  `each_action_names_its_argument_on_the_wire`,
  `citations_wire_fields_are_stable` (which pins `shown_paths`,
  `path_resolved` and `hearsay_only`),
  `a_server_written_report_says_so_on_the_wire`,
  `a_run_that_only_had_hearsay_says_so_on_the_wire`. `done` carries nullable
  `run_id`/`seq` (null when the journal write failed; rendered as "not
  saved" in VS Code). `started` is always the **first** frame; event order after
  the report is fixed: `summary` → `citations` → `excerpts` (only with a verified
  citation) → `verdict` (challenge streams only) → `done`. The non-streaming body
  is **not** a fifth place: it transcribes the same `data()` values, so every test
  above covers both modes.
- **A stream ending without a terminal event is a failure**:
  `SseEventStream` synthesises one `error` (`internal.error`) when the
  channel closes without `done`/`error` (a detached-job panic otherwise reads
  as a completed stream); `SseWireEvent` is generic, so streaming `/index`
  gets it free; `a_stream_that_ended_properly_gets_no_synthetic_terminal`. The
  JSON collector raises that same synthesised failure as a 500 rather than a 200
  with an empty report (`abnormal_end` is the one sentence both use;
  `a_job_that_dies_without_a_terminal_is_a_failure_not_an_empty_report`).
- **A report is arbitrary UTF-8 — never index it by byte**
  (`a_report_is_arbitrary_utf8_and_must_never_panic_the_parser`); the only
  safe indexes into model output are `char_indices` or ASCII-derived
  positions.
- **The report turn passes no tools** (field *omitted*) + swaps in
  `REPORT_SYSTEM_PROMPT`. The content gate in `chat_turn` withholds
  `{`-replies (`is_withheld` makes the re-ask safe); a second one →
  `research.no_report`. One `write_report` for draft and rewrite
  (`ReportOutcome`: `Written`/`Empty`/`ToolCall`).
- **`done.reason`** (`DoneReason`): `finalized` / `time_exhausted` /
  `tokens_exhausted` / `budget_exhausted` / `context_exhausted` /
  `unparseable` / `repeated_calls` — one per `break`; wire contract
  (`done_reason_wire_values_are_stable`); adding a `break` means adding a
  variant.

## Git history channel

`project_commits` + `project_commit_paths` say **why** the code became what it
is. Opt-in (`mindex-index --history`), metadata-only: **no embeddings, no
Qdrant, no chunks, no derivation version**; one model-facing tool,
`file_history`. Commit metadata is the whole feature (history questions are
SQL questions); semantic search over messages is deliberately excluded — the
ladder, its cost and the rationale live in **`docs/claude/git-history.md`**;
read it before touching `tools/indexer/src/git.rs` or `/history`. Hard
invariants:

- **Not pseudo-files**: own tables keep commits out of `/drift` and out of
  `build_search_query`'s candidate set by construction (pinned:
  `commit_rows_are_invisible_to_drift`,
  `test_commit_paths_never_surface_in_drift`).
- **`project_commit_paths.path` has no FK** (RESTRICT would refuse inserts,
  CASCADE would erase history on GC); the join to the code channel is soft,
  and `file_history` must report an un-indexed path as such.
- **Hard delete, no GC** (the `project_file_symbols` lifecycle; inverts if
  messages ever gain vectors).
- **`POST /v0/{guid}/history` is a full-set replace within `since`** (no
  update path — a sha is the hash of its content); `since` bounds only the
  **deletion** half and is load-bearing (without it a windowed walk wipes
  older history every pass). The posted set goes through a temp table, never
  `NOT IN (?, …)`.
- **Retention is `DELETE /v0/{guid}/history`**, operator-facing, called by no
  client: `keep_last=N` (newest by `committed_at`, `sha DESC` tie-break) and
  `older_than=<unix>` **intersect** — a commit dies only if both condemn it;
  naming neither = 400 `validation.history_bound_missing`. Destructive but
  not lossy (the repo refills it).
- **One producer: `mindex-index`** — rule 10 does not fire; do not teach the
  watcher/extension/MCP to walk git. `--history-only` runs the phase without
  enabling the channel (what the post-commit hook relies on). Missing
  git/non-repo = WARN, never a failed run.
- **`--relative` is not optional** below the repo root (else the soft join is
  empty for every file; pinned by
  `the_walk_asks_git_for_root_relative_paths`). The four `git log
  --format --raw -M -z` parsing traps (derived subject, `-z` + `%x1e`/`%x1f`,
  status-letter arity, the leading `"\n:"` header) are each pinned by a test
  in `tools/indexer/src/git.rs`; `old_path`'s biconditional validation turns
  a mis-parsed stream into a 400.
- **Four client-side drops, all announced** (age+count bounds together, short
  messages, truly-auto merge commits — the conjunction spares squash-merges —
  and all-paths-out-of-scope); an over-cap message is truncated with a
  marker, never dropped.
- **`file_history` reports three flags** (`history_indexed` / `in_scope` /
  `path_indexed`) — an empty list has three meanings. Out of scope is an
  explicit refusal. Its `shown` evidence is only the asked path, span-less.
  **No commit citation grammar**: a sha is verified by `git show`; claims
  anchor to `path:start-end` with the sha in prose (a shas-only report parses
  to `total: 0` and trips the ungrounded gate).

## Retrieval pipeline

**One dense vector, and the three-leg pipeline that preceded it was measured
worse.** One named vector per collection — `dense`, the registry model's width
(1024 for the 0.6B), cosine. Search is one Qdrant query at `top_k` with
`[qdrant].search_hnsw_ef` as the beam; there is no prefetch tree, no fusion and
no rerank. `post_search` still runs **two** SQLite queries around Qdrant —
candidate `qdrant_guid`s first, then `code`/metadata for *only* the top-k
winners; never load `code` for the whole active set — and results are still
**sorted by score descending** before responding (`rank_by_score`, NaN last;
don't rely on Qdrant's order). Batch sizes: `[indexing].embed_batch_chunks`
per `/v1/embeddings` call (default 256, the GPU-load lever), 256 points per
Qdrant upsert/delete (`embed.rs`); embed-response rows are positionally
aligned with the chunk list and **counted** against it. Startup refuses
`search_hnsw_ef < [search].max_top_k` — a beam narrower than the page asked
for truncates it with no error anywhere. The measurement record (RRF fusion
scoring *below* the single dense leg it fused, ColBERT never shown to help,
and the ~99.6%-of-bytes ColBERT store that paid for it) is
`docs/claude/retrieval-v2.md`, kept as evidence; the v3 grammar, the
cross-collection GC note and the runbook are `docs/claude/qdrant.md`.

**The model is a registry entry, and it varies over the artifact rather than
the file.** `src/models/registry.rs` compiles in every model mindex can serve
— three today (`qwen3-embedding-{0.6b,4b,8b}`, 1024/2560/4096-d, one shared
tokenizer, `max_seq` 32768) — each with the `collection_slug` its collections
are named by and the **instruct query prefix** its card specifies. The
registry is one half of a two-sided contract: the `embedding_models` table
pins the same ids and dims with a `CHECK` and is append-only by trigger, and
`verify_model_registry` refuses to start when the two disagree, so a rebuilt
binary can never silently reinterpret stored vectors. Adding a model is a
registry entry **plus** a migration widening that `CHECK`, in one commit.

**The prefix goes on queries and on nothing else.** Qwen3-Embedding is
instruction-tuned and asymmetric: an `Instruct: …` / `Query:` preamble
(trailing space included) in front of the query, documents bare. It is applied at exactly one site (`post_search`'s
`query_text`) and the constant is byte-for-byte the bench's, because a drifted
prefix degrades retrieval **silently** rather than failing — it is the first
thing to check when a re-measured nDCG comes in low.

**Collections are per (project, model)**: `{guid_simple}_{slug}_v3`, e.g.
`2f1c…b6a_q3e06b_v3`. Switching `[model].id` therefore does not overwrite
anything — the old model's collection is *held*, `worker::stale` classifies it
`OtherModel` (registered, not active, reusable) apart from `Previous` (a
superseded `COLLECTION_SCHEMA_VERSION`, genuinely orphaned), and switching back
is instant reuse. `DELETE /projects/{guid}` drops **every** model's collection
for that project, not just the active one.

**A model switch is a re-embed, not a reindex.** `project_files` carries
`chunker_id` (the tokenizer that measured the boundaries) and
`embedded_model_id` (whose vectors exist); both join the `file_already_indexed`
predicate beside the two derivation versions, so flipping `[model].id`
self-heals exactly like a version bump. `mindex-index --vectors-only` (body
flag `vectors_only`) is the cheap path: re-embed the **stored** chunks into the
active model's collection, no slicing and no symbols — refused across a
`chunker_id` mismatch, since different boundaries mean the chunks themselves
are wrong. One tokenizer across the Qwen3 sizes is what makes changing size
exactly this pass. The retry worker stamps `embedded_model_id` on the same
grounds.

**The client speaks OpenAI, and it checks who answered.**
`src/models/embedder.rs` posts `/v1/embeddings` to whatever OpenAI-compatible
server is configured — llama.cpp on this host, vLLM equally (the vendored BGE-M3
server is deleted; it existed only because nothing general returned three heads
at once). Two properties carried over verbatim: the **whole-call deadline**
(`[model].encode_timeout_ms`, default 10 min — per-attempt bounds let a
throttled server hold a search open for forty minutes), and the retry loop
counting its own backoffs, so three-retries-then-success is distinguishable
from one success. Busy is **429 *or* 503** — servers spell it both ways —
retried `[model].max_429_retries` times (200/400/800 ms, cancellation-aware);
then the file goes `failed` and the retry worker re-attempts later (layered
backoff). Two properties are new and both close identity holes the old pipeline
had: every response row is checked against the registry dim, and
`GET /v1/models` is a **handshake** — startup **refuses** a server that answers
and names a different model (a wrong model with a coincidental width would
poison every vector in silence) while an *unreachable* one is only a warning (a
down embedder is a state the retry worker already covers), and
`embedder_probe` re-checks it on every `/health`, so a model swapped under a
running mindex surfaces on the next read.

**The query path may run on a second embedder instance.**
`[model].query_server_url` (absent = one instance does both; `RouterState`
holds the *same* `Arc` twice) puts `/search` and every research search on its
own server — typically the smaller card, or CPU (a query is one short text,
latency-bound), freeing the indexing instance's VRAM. It is also the answer to a
serving stack that is fast at one and slow at the other, which is a real shape
and not a hypothetical: llama.cpp answers a query in ~30 ms while indexing 8×
slower than a torch server (`deploy/embedder/README.md`). Both instances must
serve the same model, and unlike v2 **something now checks**: the handshake
runs per instance at startup and `GET /health` pings the second one separately
(`checks.query_embedder`) — only when actually split, hence an `Option`
compared by `Arc::ptr_eq`, not by URL. What is still unchecked is *precision*:
two instances at different dtypes answer the handshake identically and present
as "search sometimes can't find the obvious thing", not as an error.

## Slicer

`Slicer` (`slicing/traits.rs`) walks the tree-sitter AST depth-first,
selecting **named nodes** whose token span (HF tokenizer) falls in
`[slicer].min_chunk_tokens..max_chunk_tokens` — **128–364** by default,
measured, not computed (`bench/FINDINGS.md` §2.5: 364 beat 512 by +0.0108
nDCG@10, p = 0.030, with the gain in the dense head; measured under the
*previous* tokenizer and carried over as best-available, so the bench re-run is
what confirms or moves it). The window's only hard ceiling is the active
model's own `max_seq` (32k for the whole Qwen3 family), which startup
validates. `code` is
extended left to line start over pure indentation, then over the node's **doc
comment and attributes** (`ABSORBED_KINDS`, matched as substrings — no
per-language table). Not cosmetic: a doc comment is a *preceding sibling*,
never a child, so without the extension the prose that says **why** is dropped
from every chunk — the actual cause of the "an NL query retrieves the test,
not the implementation" finding. Absorption stops at a blank line (a detached
comment documents nothing in particular), at `max_tokens`, and at the furthest
byte already emitted (a large `#[utoipa::path(...)]` clears `min_tokens`
alone, becomes a chunk, *and* is the preceding sibling of the function below —
without the bound both chunks would contain it).

**Node selection alone leaves ~37% of lines in no chunk**, so the walk is
followed by a **gap pass** (`[slicer].fill_gaps`, default on): everything
inside a node below `min_tokens` (consts, type aliases, small helpers, trait
signatures — and their doc comments, which left-extension can never reach when
there is no chunk to extend) plus everything between the selected children of
an oversized node, packed into line-aligned windows up to `max_tokens`,
breaking at blank lines. Fragments under `GAP_MIN_TOKENS` (24) merge into the
previous window, not dropped. Measured here: line coverage 63% → **99.7%**,
doc-comment coverage 40% → **100%**, chunks 553 → 972. Roughly doubles
embedding work — hence the knob. Beyond coverage: only 47% of symbol
definitions were inside any chunk, so `read_chunks` dead-ended on a coin flip
— the prescribed pipeline failed at its last hop.

`SlicedChunk.start_byte/end_byte` are `#[cfg(test)]`-gated (never persisted),
as is `from_gap` — **the token window governs node selection, not gap chunks**
(a gap chunk's floor is `GAP_MIN_TOKENS`). `tokens` **is** persisted
(`project_file_chunks.tokens`): a future model's window can then be checked
against the corpus by SQL instead of a re-tokenization, which is what a
blast-radius question needs. It is approximate by one edge token, because the
window is counted over **whole-file** token offsets and re-encoding a chunk
alone is a different measurement (an edge token splits differently without its
surroundings; a 364 can re-encode at 365). `chunks_satisfy_token_window`
therefore asserts the window ±`WINDOW_SLACK` — without the slack the test is a
tripwire on whichever file lands on a boundary (`src/research.rs` did).

**A line is not bounded by anything, so neither pass may cut only at line
boundaries.** A minified file (or one paragraph of prose) is one line for its
whole length, and a window is a window. Hence `token_boundary`
(`slicing/traits.rs`), the last resort of both passes: cut on a boundary the
tokenizer itself reported (ceiled to a `char` boundary — the text is sliced
right after, and a fake tokenizer in tests owes UTF-8 nothing). The bound it
cuts to is the slicer's **own** `max_tokens`, and that is the whole rule now:
the `STORABLE_TOKENS_CEILING` (1022) / `RETOKENIZATION_SLACK` chain that used
to sit above it was a **Qdrant multivector limit** — one 1024-wide ColBERT row
per token against a 1 048 576-element point — and died with ColBERT along with
the two mistakes it invited (a `min`-clamp in both constructors rather than
config validation, and aiming under a ceiling a re-encode could cross).

**Documentation is chunked by a second slicer, and every rule above
inverts.** `MarkdownSlicer` (`slicing/markdown.rs`, `markdown` only) walks
tree-sitter-md's **block** grammar to atomic blocks — descending into
`list_item` — then packs runs of adjacent blocks into chunks by dynamic
programming (`best[j] = min_i best[i] + cost(i..j)`, exact, O(n²)): one cost
term per chunk against a penalty for swallowing a level-3+ heading, `+∞` above
`[slicer].max_doc_chunk_tokens` and across any level-1/2 heading (greedy "fill
to the cap" buries subsection headings to save chunks it did not need to
save). Three inversions, each measured: **no lower bound** (a 40-token section
is a complete claim), **chunks nest and merge**, and **the cap is 1024, not
the code window** (512 answers 15/23 documentation questions vs 18/23 — it cuts
explanations away from what they explain; past 1024 nothing improves while
every hit costs proportionally more of a `/research` transcript). The code
window (364) is a *quality* ceiling, never the model's capacity — Qwen3's
`max_seq` is 32768, and both caps are validated against it at startup. Blocks
are truncated to `max_doc_chunk_tokens` before being embedded for the semantic
term, with the same number `segment` will cut to (a block is not a chunk and
has no size bound of its own).

**Boundaries come from two signals; the second refines the first.** Structure
sets the hard rules; *semantic shift* (embedding distance between blocks,
weighted `[slicer].doc_semantic_weight`) decides among what structure leaves
open. Measured on **this** repo the term changes nothing (moves 7-13% of
boundaries, MRR@10 0.3931 either way) — a fact about this densely-headed
corpus, not the technique; it is on by default because it is worth most where
headings are sparse and the packing would otherwise cut mid-topic. (A further
model-driven boundary re-check pass measured *worse*; not shipped.) Separately
real: block structure beats a line-based `#` splitter, MRR@10 0.3714 →
0.3931, recall@10 18/23 → 20/23. Three consequences of the term being on: it
costs **one embed call per document**, so block embedding happens *outside*
the prepare transaction — hence the two-phase `plan` → `segment` API; an
**unreachable embedder degrades to structure-only** with a WARN rather than
failing the file (a refinement must never be a dependency); and a documentation
chunk's boundaries therefore depend on the **embedder's model and precision**,
which nothing versions: `chunker_id` records the *tokenizer*, and every Qwen3
size shares one — so `--vectors-only` across sizes keeps documentation
boundaries that a different size's embeddings chose. Accepted (the term moves
7-13% of boundaries at equal MRR here), and the same blind spot as a
grammar-crate bump. Weight 0 restores pure structure and skips the round-trip.

## Concurrency & cancellation

- **Async-first.** SQLite runs in `spawn_blocking` via
  `db_pool.transaction()`; Qdrant/embed are `.await`-ed — no `block_on`. Every
  long loop / I/O respects a `CancellationToken`; client-cancelled requests
  return HTTP 499.
- **Cancellation propagation (subtle).** A handler's `CancellationGuard`
  wraps a *fresh* token cancelled only by its own `Drop`. On client disconnect
  axum drops the handler future → `Drop` fires, but the future is gone, so
  in-handler `Cancelled` arms are defensive, rarely hit. The token's real job
  is letting in-flight `spawn_blocking` (slicer) and the embed `select!` bail
  after abandonment; the half-written row is recovered by the retry worker.
  Clean shutdown uses a *separate* token tree rooted in `main.rs`.
- **`IndexClaim` is an in-process keyed lock**, so mindex assumes **one
  process per database**. It serializes handler↔handler and handler↔worker
  races on a file (contention → 429) within one process only; two processes
  against one SQLite/Qdrant would need a DB-level compare-and-swap claim (a
  conditional `… → indexing` update + an epoch column checked at
  `mark_indexed`) — a schema migration, hence not done speculatively.
- **Connection-return is cancellation-safe** (regression guard,
  `sqlite3.rs`). The blocking task pushes its connection back into the pool
  *itself* (`conns.blocking_lock().push`), not the awaiting code after
  `handle.await`: dropping a `spawn_blocking` JoinHandle does **not** cancel
  the task, and if release depended on the awaiting future, a future dropped
  mid-transaction would leak the conn — after `db_pool_size` (4) such events
  the pool is permanently `PoolEmpty`. A closure panic is the one unreturned
  case (logged on `JoinError`).

## SQLite pool

Fixed-size pool of `rusqlite::Connection` behind a
`tokio::sync::Mutex<Vec<_>>` (pop/push). Per-connection PRAGMAs: WAL,
`foreign_keys=ON`, `synchronous=NORMAL`, 16 KB pages. Handlers run **multiple
sequential `transaction()` calls** (one per logical step), not one giant
transaction — the soft-delete pattern keeps state recoverable if a later step
fails.

## post_index shape

Two phases so the GPU sees big batches: (1) **`prepare` every file** —
hash-check (`Ok(None)` = unchanged, skipped) → set `indexing` → main tx (mark
old chunks deleted + slice + insert) → `Prepared` with that file's chunks; own
`indexing_file` span each (no `Entered` guard across `.await`). (2)
**`embed_all`** chunks from all prepared files in one batched
`embed::embed_and_upsert` pass, then **`mark_indexed`** each + tally. Recovery
is per-batch: any failure sends every already-prepared file to
`failed`/`cancelled` via `recover_all`; the retry worker re-embeds later.
`tree_sitter::Parser` is `Send` — slicer built inside the `spawn_blocking`
closure. Body limit: `[server].max_body_mib` (default 256 MiB) via
`DefaultBodyLimit` (axum's 2 MB default is far too small); over-cap =
problem+json 413 (`request.body_too_large`), not axum's plain-text.

**`?stream=yes` reports the same pipeline as SSE**, and the pipeline is one
function either way: `run_index_job` is shared verbatim; the query only picks
who builds the terminal (`Json` body vs a `done`/`error` event), so the modes
cannot drift. The cancellation shapes differ on purpose, and research now shares
both of them: JSON mode keeps its `CancellationGuard` (handler-future drop =
cancel); SSE mode spawns the job detached and the *stream's* Drop cancels the
token (a guard in the handler would fire the instant the response is
constructed). Research's JSON mode is the same pair with the roles as here —
which is what makes "the default does not weaken cancel-on-disconnect" true
rather than merely intended.
Recovery runs inside the job, so a disconnected streaming client still lands
its batch in `cancelled`. The event vocabulary
(`started`/`prepared`/`skipped`/`embedded`/`indexed`/`done`/`error`,
`IndexEvent` in `models.rs`) is a wire contract in four places that move
together: `post_index`'s doc comment, its OpenAPI 200 description, the
`mindex-index` reader (`tools/indexer/src/client.rs`) and the VS Code client
(`api.ts`); both consumers drop unknown events silently; shapes pinned by
`index_event_names_are_stable` +
`index_event_data_names_its_fields_on_the_wire`. `embedded` (one per embed
batch, via `embed_and_upsert`'s optional progress callback — its one
deliberate side-channel) carries cumulative `chunks_done`/`chunks_total` plus
the server's own `elapsed_ms`, making a client's chunks-per-second a
measurement; both clients compute it over a sliding window and fall back to
plain JSON transparently when an older server ignores the query
(`StreamOutcome.streamed` / content-type sniff). `done.files` is byte-for-byte
the JSON response body, so both modes tally identically. A typo'd `?stream=`
value or key is a 400 (`IndexQuery` is `deny_unknown_fields`), never a silent
fall-through.

## Mockable interfaces

Three traits; production type is the sole real impl, fakes in `#[cfg(test)]`:
**`Embedder`** (`models/embedder.rs`; `Arc<dyn>` in `RouterState` + retry
worker — three methods: `embed`, the liveness `health`, and `served_models`,
which is the handshake half of model identity),
**`VectorStore`** (all Qdrant ops; error is `VectorStoreError`, a rendered
string — `QdrantError` isn't test-constructible), **`Tokenizing`** (the
slicer's only tokenizer need; fakes avoid the HF download). New seam = minimal
trait + owned error if the real one isn't constructible. `SQLite3Pool` is
deliberately **not** a trait (its generic-closure `transaction` isn't
object-safe) — test against a real `:memory:` pool.

## Error handling, validation & logging

**Client error contract: `ApiError` → RFC 7807 (`backend/error.rs`).** Every
non-2xx is `application/problem+json` (`ProblemDetails`) with a **stable,
namespaced machine `code`** (`validation.top_k_out_of_range`,
`selector.empty`, …) + English `title`/`detail` + optional `field`/`meta`; the
`code` is the localization key. `ApiError` is the *single* enum; its
`code()/status()/title()/detail()/meta()` and the lone `IntoResponse` impl are
the only place a response shape is built. **Codes are an API contract**: the
`codes_are_stable` snapshot test fails on any change (also update the
catalogue in `openapi.rs` `info.description` + clients). Handlers return
`Result<_, ApiError>`; domain errors (`SQLite3PoolError`, `SlicerError`,
`EncodeError`, `VectorStoreError`, `EmbedUpsertError`) convert via
`From`/constructors at the call site — the call site keeps the contextual log
+ sysadmin hint, `From` never logs. Mappings: `SQLite3PoolError::Cancelled` →
499, `PoolEmpty` → `DatabaseBusy` 503 (rest `Internal`); embed request/decode
→ `EmbedderUnavailable` 503; Qdrant search → `QdrantUnavailable`, upsert/drop
→ `Internal`. No external error crates.

**The three pool failures are three diagnoses and must not be one.**
`Cancelled` is the *caller* leaving — 499, and every call site deliberately
skips its own `error!` for it. `PoolEmpty` is load: retryable, so 503
`database.busy`, and the pool itself now `warn!`s with a hint (as `Internal`
it was an unretryable-looking 500 that produced no journal line at all — the
likeliest production failure, invisible). `Panicked` is a bug here: it stays
`Internal` 500, and it exists as its own variant because a `JoinError` used to
become `Cancelled`, which told the client it had closed a connection it never
closed, told the dashboard a disconnect, and silenced every call site's log.
It also costs a pool connection permanently, so `db.transactions` counts it as
`outcome="panic"` rather than burying it in `"cancelled"` — after
`db_pool_size` of them every request fails with `database.busy` and nothing
says why. The reachable instance was `locate_match` slicing the original
string with an offset found in its lowercased copy (`İ` grows by a byte), so
any indexed file containing one made `grep` a way to dismantle the pool four
requests at a time.

**Validation happens at the edge (`backend/v0/validate.rs`), before any
work** — bad input is a 400 with a precise `code`, never an opaque 500 from a
SQLite `CHECK`. It mirrors the schema constraints (`validate_path` = the path
CHECK + `..`-traversal guard; `validate_sha256_hex` = 64 hex) and enforces the
`[limits]`/`search.max_*` caps; `require_nonempty_selector` is the shared
guard for the destructive endpoints. Schema CHECKs and shape-validation
triggers stay as defense-in-depth. Handlers take `ApiJson`/`ApiPath`/`ApiQuery`
(`extract.rs`), not bare axum extractors, so malformed body/path/query is the
same problem+json envelope (`request.malformed_body`/`malformed_path`), not
axum's plain-text 400.

- No `unwrap`/`expect` in production paths (workers may `unwrap_or_default` on
  best-effort queries); startup-only panics name the file and what to check.
- **Logging shape:** a mandatory message stating *what operation failed*
  (never bare `error!(?err)`); error as a field (`error = ?e`/`%e`, not
  interpolated); identifiers as fields (`%` String/Uuid, `?` enums). Handlers
  carry `project_guid`/`pl`/`path` on the span; workers pass them explicitly.
  Infra failures end with a one-line sysadmin hint (embedder reachability +
  the `0.0.0.0` vs `127.0.0.1` gotcha, Qdrant, DB writability); logic errors
  don't.

## Metrics

`GET /metrics`, OpenMetrics text, same HTTPS listener — `prometheus-client`
with one owned `Registry` threaded as `Arc<Metrics>` through constructors
(`backend/metrics.rs`). No global recorder (the config rule; also the only
shape letting two unit tests each own an independent metric set). Scraped by
host VictoriaMetrics (`scheme: https`, no `tls_config` — the mkcert root is in
the system trust store, the only path that works since the VM unit sets
`ProtectHome=true`); rendered by the provisioned Grafana dashboard in
`deploy/grafana/`.

**Metric names and types are a contract, like `ApiError` codes** — a
dashboard is a client, a renamed family is a silently blank panel.
`metric_names_are_stable` pins *both*; the type matters more (a counter→gauge
flip renames nothing and breaks every `rate()`). Two encoding quirks the test
encodes: OpenMetrics puts `_total` on a counter's **sample** lines, not its
`# TYPE` line (family `mindex_gc_runs`, stored series `mindex_gc_runs_total`
— the test reconstructs and pins the series name), and `encode` emits `# EOF`,
so the body must be served as `metrics::CONTENT_TYPE`, never `text/plain`.

**The cardinality rule: every label value comes from a set the server
defines** — `MatchedPath`, `ApiError::code()`, `ProgrammingLanguage::name()`,
`DoneReason::as_str()`, tool names, file statuses. Never a raw URI, path,
query, or model-supplied string without a bound. `project_guid` is the sole
open-ended label: UUID-validated first, off by default on the HTTP families
(`[metrics].per_project_http_labels`). Two products are **split rather than
crossed** — `research_runs{model,done_reason}` vs
`research_runs_by_effort{model,effort}` (`model` is client-supplied), and
`project_chunks_active{project,language}` vs
`project_chunks_deleted{project}`. Histograms are not labelled by project at
all.

**Clear-and-repopulate, and why counters are exempt.** A `Family` retains a
label set for the life of the process (a deleted project would report its last
known count until restart). `worker/metrics.rs` builds each tick's whole value
map from SQL *first*, then clears and repopulates in one synchronous block
**with no `.await` between them** — the only thing keeping a scrape out of the
gap, so `apply` must never become `async`. Two structural guards: only
`StateMetrics` is ever cleared, and it holds **gauges only** — clearing a
counter reads as a process restart and permanently re-baselines every
`rate()`. `StateMetrics` is written by that worker and nothing else.

**Why decorators, and the four things that cannot be one.** `VectorStore`,
`Embedder`, `ResearchTools` and `ResearchJournal` are wrapped once in
`main.rs`/`post_research`: a seam decorator cannot miss a caller;
`MeteredJournal` alone yields nearly the whole research set (`RunRecord`
already carries it). The exceptions, each structural: `SQLite3Pool` is not a
trait, so it is instrumented in place at its single choke point; the
embedder's 429/503 retry loop lives inside `embed` (invisible from outside);
Ollama's tool-call-parse retry and silent transcript truncation happen inside
one `chat_stream` call; and the indexing-claim conflict is swallowed at
`Err(ApiError::FileInFlight) => {}` while the request still 200s, so HTTP
middleware can never see it. All four use an `Option<...>` field set by a
`with_metrics` builder — keeping test pools and bare test clients
constructing unchanged.

**In-flight gauges are `Drop` guards** — a disconnected client's future is
*dropped*, so code after `next.run(req).await` never runs; research SSE
streams die **only** by disconnect, so an inc/dec pair would ratchet the gauge
upward within a day. The guard also records the abandonment as `status=499,
code="request.cancelled"`, keeping `http_requests_total` reconcilable with
`http_requests_in_flight`. Same logic: `research_active` is **derived** in the
collector from `max_concurrent - available_permits()`, not incremented around
the spawn.

**`enabled` means exposed, not measured.** `Arc<Metrics>` is always built and
written into; `[metrics].enabled` gates only the route and the collector (the
alternative is an `Option` check at sixty call sites for a relaxed atomic
add). Two edge consequences: the HTTP layer sits **outermost** and must be a
`Router::layer` (outside the router there is no `MatchedPath`) — so a request
matching no route never reaches it and unknown-path 404s are uncounted rather
than bucketed under a fabricated label; and the HTTP/3 body-limit
short-circuit answers before the router, so it records itself — a second such
short-circuit needs the same three lines or it goes uncounted in silence.

**A rare labelled counter must be charted with `increase()`, never `rate()`**
— the rule that once made the whole research dashboard row read empty. A
`Family` series does not exist until its first event, so its first scraped
sample is already **1** — no preceding 0 — and a label set seeing one event
per process lifetime (normal for `research_runs{model,done_reason}` and every
per-run histogram) stays flat at 1: `rate()` over that is 0 forever.
`increase()` counts the first sample of a newly-appeared counter
(VictoriaMetrics does; upstream Prometheus does not — and seeding at zero is
impossible here, `model` is client-supplied). High-traffic families hide the
defect. Paired rendering rules: a handful of events a day is drawn as **bars**
with a `sum` legend; a quantile over a rare histogram as **points with gaps
kept**; a share-of-total stat needs `or vector(0)` (the healthy case is that
the `unverified` series does not exist).

**Three families exist to answer one open question**: does announcing a length
ceiling change what a model writes? `research_report_words{model}` against the
granted `max_report_words` is the measurement — if they are uncorrelated, the
prompt half of that knob is dead weight and only `num_predict` earns its place.
`research_report_length_caps` is expected to stay at **zero** (any value means
`REPORT_WORDS_TO_TOKENS` or the model is wrong, and a cut landed mid-token);
`research_report_context_sheds` may legitimately stay at zero forever on this
hardware, in which case the shed path is insurance and should be described as
such rather than as a mechanism.

**Every wait has a number, and it is the server's not the library's.** The class of
bug this closes is a default nobody chose: `[qdrant].timeout_ms`/`connect_timeout_ms`
exist because the client's own default is 5 s and no knob reached it — a project
whose fusion + ColBERT rerank ran past that failed **every** search with
`qdrant.unavailable`, untunably (the pipeline that took that long is gone; the
knob is not — a cold segment can still outlast a library default nobody chose).
`[model].encode_timeout_ms` bounds the whole
embed call rather than each attempt: per attempt the worst case was
`(1 + max_429_retries)` timeouts plus backoffs — forty minutes at the defaults —
while a throttled embedder held a search open or kept a file's indexing claim
(`EncodeError::Timeout`, its own metric label, because "too busy for too long" is a
capacity diagnosis and not a network one). `GET /health` runs its probes
**concurrently** under `HEALTH_PROBE_TIMEOUT_MS` (3 s, not configurable — this is
"how long may the liveness endpoint be made to wait", not "how long may the
dependency take"); they were sequential, and the SQLite one was the file's only
`transaction` without `with_cancellation_token`, so a wedged pool hung the one
endpoint that must always answer. The ten research/management cores gained the same
binding, so a disconnected client stops paying for a full `LIKE '%…%'` scan.
Shutdown now **drains** for `SHUTDOWN_DRAIN` (8 s, under both systemd's and
Docker's defaults): the signal arms used to cancel the token and return, logging
"Shutdown complete." while in-flight batches were torn out mid-flight.

**HTTP/3 streams frame by frame.** `send_axum_response` buffered the whole body
before the first `send_data`, so `/index?stream=yes` and `/research` did not stream
over h3 at all — the client saw nothing until the run ended (up to seventy minutes
for a `high` run) and the server accumulated every event in memory, unbounded. Both
endpoints exist *because* their output is worth watching arrive.

**A response is not trusted because it parsed.** The binary `/encode` reader
that once had to refuse a `Vec::with_capacity` off the wire is gone with the
protocol, but its lesson kept two successors on the JSON path: every embedding
row is checked against the **registry's dimension** for the configured model
(a server quietly serving something else is otherwise indistinguishable from a
correct one until search degrades), and `embed_and_upsert` checks the **row
count** against the text count — `zip` silently truncated a short response,
leaving a file marked `indexed` with vectors missing and no error anywhere, and
a long one indexed `guids[i]` out of bounds.

**Ollama's two failure classes are two codes.** `ollama.unavailable` is unreachable
or reachable-and-mute; `ollama.error` is Ollama answering *with* an error — nearly
always a model that is not pulled. Collapsed into one, a client could not word the
message or decide whether re-reading `/health` would say anything (the VS Code
extension refreshes on `ollama.unavailable`, and for a typo health is green every
time). `show_facts` caches **successes only**: caching the failure made one blip
permanent for the process, silently running every later run of that model at the
configured ceiling instead of its own window.

**`cancel_overdue` returns only what it cancelled.** A run wedged in an await its
token cannot reach stays registered, so the sweep kept finding it: re-cancelled,
re-warned and re-counted every 30 s for the life of the process, turning
`research_watchdog_cancels_total` — the counter documented to stay at zero — into an
unbounded number describing one event. It stays visible through
`oldest_inflight_age_ms` and the `unhealthy` verdict.

**Three families exist because "nothing" and "broken" were the same number.**
`search_orphaned_winners` counts chunks Qdrant scored whose SQLite row was gone —
silent before, and when *every* winner was one the caller got a 200 with an empty
list while an over-narrow filter gets a 404, i.e. the reassuring spelling for the
case that means the two stores disagree (`search_core_inner` now returns `NoMatch`
there too, so 404 uniformly means "nothing active matched"). `project_vectors` is
Qdrant's own point count per project, the *only* detector for the
no-mismatch-detection failure `db/qdrant.rs` documents: against
`project_chunks_active` it separates "this project is empty" from "this project's
vectors are gone"; it rides under `[metrics].probe_dependencies` (one round-trip
per project per tick) and a project the store cannot answer for is **absent, not
zero** — zero is the alarming value and must never be manufactured by an
unreachable Qdrant. `state_refreshed_timestamp_seconds` dates the last *successful*
snapshot: a failed read deliberately keeps the previous gauges, which was
indistinguishable from a healthy tick, so every `StateMetrics` value could sit
frozen with nothing saying so.

**A score that cannot be compared must rank last, not first**
(`rank_by_score`, `search_unscorable_winners`). `total_cmp` orders `+NaN` above
every finite value, so the plain descending sort by it handed the **top result
slot** — the one an agent reads and a human trusts — to a chunk the reranker
could not score. The producer is documented and local: the embedder's XPU
backend returns NaN for padded fp16 rows on its default attention kernel and
still answers 200 (see **Layout**), as does a split deployment whose two
instances differ in precision (see **Retrieval pipeline**). Without the counter
the symptom is "search sometimes puts something irrelevant first", which reads
as a ranking-quality complaint rather than the misconfigured embedder it is —
the third spelling of the same defect those two sections already describe, and
the only one visible from `/metrics`. NaN results are ranked last rather than
dropped: the chunk matched the filters and the candidate set, so it is a real
answer with an unusable score, and silently shortening the response is what
`search_orphaned_winners` exists to stop repeating. The sort is stable, so a
wholly unscorable batch keeps the reranker's own order — the only information
left. Expected to stay at zero.

**GC reports per phase whether it finished** (`gc::Phase`/`GcOutcome`). Each phase
used to return a bare `usize` with every error mapped to `0`, and `collect`
incremented `gc_runs{outcome="ok"}` whenever the token was live — so a GC failing
for days looked idle, and `POST /gc` answered 200 with zeros either way. Now
`outcome="error"` outranks `"cancelled"` (a shutdown mid-pass is routine; a phase
that could not run is not), `sweep`'s anti-spin `break`s set `failed`, and
`GcResponse.failed_phases` names them on the wire. The counts stay real — a phase
that failed part-way reports what it managed — so the list is what says whether
they are the whole story.

**`research_unjournalled_runs{model,outcome}` is the denominator every research
rate lacked.** All per-run research metrics live in the `MeteredJournal`
decorator, so the three endings that write no row — `cancelled`, `failed`,
`report_rejected` (failed the markdown gate) — were absent from `research_runs`,
`_duration`, `_steps`, `_tokens` and `_citations` simultaneously: the GPU hour was
spent and the dashboard said the run never happened. `run_research` therefore takes
an `Option<Arc<Metrics>>` of its own; the journal seam cannot see these.

**The workers are supervised, and the gauge lives outside `StateMetrics`.**
Every background worker goes through `supervise()` in `main.rs`: it publishes
`worker_running{worker}` before the task starts (a series that never existed
cannot be alerted on — a worker that panics on its first tick is *absent*, not
zero), joins the task, and on a `JoinError` emits an `error!` naming the worker
plus `worker_exits_total{worker,outcome="panic"}`. It deliberately does **not**
restart: a worker that panicked once panics again, and the loop would bury the
backtrace under its own noise. Before this, they were all bare `tokio::spawn`
with the `JoinHandle` dropped, so a panic stopped GC or the retry sweep
permanently and in silence — from outside indistinguishable from a healthy idle
system. `SupervisorMetrics` is its own group precisely because `StateMetrics` is
cleared and repopulated whole by the metrics worker, whose own liveness gauge
would then be erased by the tick that proves it alive. `CollectionMetrics`
(`worker::stale`, hourly) is a third group for the same reason from the other
side: written on its own cadence, it would be wiped by a tick of a worker that
knows nothing about it — and its cleared value, `0`, is its all-clear.

**What is deliberately not measured.** Per-project code bytes as a gauge
(`SUM(LENGTH(code))` full-scans the biggest column every tick and evicts the
page cache the candidate query depends on — the write-time
`index_code_bytes_total` counter answers it better). A drift *level* (the
server never walks a tree; `post_drift` counts what checks *reported*; a real
drift gauge belongs to `mindex-watch`). Tokio runtime internals (need
`--cfg tokio_unstable`) — "threads and pools" is covered by the SQLite pool,
the claim table, the research semaphore, in-flight requests and the research
runtime's worker count.

`/metrics` is the one route with no `#[utoipa::path]` and no `openapi.rs`
entry — not JSON, not versioned, not problem+json; its consumer is a scraper.
`openapi_spec_is_complete_and_versioned` asserts the *absence* so the omission
reads as a decision.

## Performance conventions (hot paths)

Build `ChunkAsVector` by **moving** the response's dense rows (`into_iter`
+ `zip(guids)`), never cloning — a batch is `embed_batch_chunks` × the model's
width of `f32`, and at 4096-d that is the largest allocation on the path. The
row-count check precedes the `zip`, not the other way round. Lives in
`embed.rs` (shared by `post_index`, the retry worker and the `vectors_only`
pass).

## Languages

The supported set *is* the `ProgrammingLanguage` enum + the grammar crates in
`Cargo.toml`; extension map in `tools/indexer/src/scanner.rs`. Hard
constraint: every grammar crate must depend on `tree-sitter ≥ 0.23`
(`LanguageFn` API) — older ones cause a native `links` conflict; verify with
`cargo info` + registry source before adding. **Adding a language touches all
of these** (each omission fails differently — 400 → SQLite CHECK 500 →
silently skipped file):

1. `ProgrammingLanguage` enum + `ToSql`/`FromSql` + `ALL`/`name()`
   (`backend/v0/models.rs` — crate-root `src/models.rs` is a two-line module
   list), lowercase serde name. Missing `name()` arm = compile error; missing
   `FromSql` arm fails on **read** (rows insert fine, then 500 on any query
   selecting the column); missing serde rename = 400
   `request.malformed_body`. Omission from `ALL` is the only **silent** one:
   absence from `GET /config`, *and* silent exclusion from
   `every_language_constructs_or_declines` (`slicing/symbols.rs`), which
   iterates `ALL` — a broken tags query for that language ships untested.
2. `CHECK` constraint on `project_files.programming_language` — in **two**
   places, and editing only the first is silent. `v1.0.0_schema.sql` builds a
   *fresh* database and is never re-read; a database in use needs a new
   migration rebuilding `project_files` with the widened list —
   `v1.1.0_toml_yaml_languages.sql` is the pattern (rule 8 has the rebuild's
   shape and hazards). Both files must end with the same list.
3. `tree-sitter-<lang>` in `Cargo.toml` (verify ≥ 0.23).
4. Arm in `tree_sitter_language(pl)` (`handlers.rs`) — total match, missing
   arm = compile error. A grammar here does **not** commit the language to the
   AST-walk slicer: `markdown` returns tree-sitter-md's *block* grammar and is
   dispatched to `MarkdownSlicer` by the one `pl == Markdown` branch in the
   prepare tx; a second such language adds a branch there, not a second code
   path.
5. Arm in `queries_for(pl)` (`slicing/symbols.rs`) — total match; `None` is
   legal (no symbols). Prefer the crate's `TAGS_QUERY` const; if the crate
   only *packages* `queries/tags.scm` (or ships a broken one), vendor the file
   under `slicing/queries/` with a provenance header (scala/csharp precedent).
   Add a fixture test in `symbols.rs` when a query exists. **Bump
   `SYMBOLS_DERIVATION_VERSION`** — without it already-indexed files never
   gain the new language's symbols. Today bash, html, css, json, toml, yaml,
   haskell, zig and sql ship no tags query → no symbols (chunking and search
   unaffected); revisit when upstream adds one. `markdown` is `None`
   *permanently*: headings are a table of contents, and making `outline` mean
   "sections" for one language buys a second meaning for the same tool. The
   vendored `.scm` files are verbatim copies — refresh when their crates bump.
6. `detect_language` + `Language::name()` in **three** extension maps, each
   silently skipping the file otherwise: `tools/indexer/src/scanner.rs`,
   `tools/watcher/src/scanner.rs` (a verbatim copy) and
   `tools/vscode/src/languages.ts` (`EXT_TO_LANGUAGE`).
7. `ext_to_lexer()` in `mindex-search.sh` (pygments map); its `VALID_LANGS` is
   only the offline fallback (canonical list from `GET /config`).
8. The VS Code **language mark**, four places — fails as a red test
   (`langIcons.test.ts` is exhaustive over `ALL_LANGUAGES`): `DEVICON_MARKS`
   (`esbuild.mjs`) if devicon draws the language, else `LANG_FALLBACK_CODICON`
   (`shared/langIcons.ts`) — sql and toml are the two it does not; a rule in
   `media/lang.css`; the base colour in the test's `BRAND` table. The two CSS
   hex values are **derived, not chosen** — the test recomputes them with its
   `adapt()` (mix toward white on dark / black on light in 5% steps until
   3:1); run that function and paste its output. `shared/langGlyphs.ts` is
   generated; rebuild, never hand-edit.
9. Rebuild the image. A container whose volume predates the `CHECK` change
   picks the widened list up from the migration in step 2 — which is what
   makes dropping the volume unnecessary, and why step 2 is not optional.

## Four clients, one working-tree view (sync rule)

`mindex-index`, `mindex-watch`, the VS Code extension and the MCP
`index_files` tool all answer the same question — *which files are in this
project and what is in them* — from four separate implementations. The server
never walks a tree; it only believes what a client posts. So **any change to
what a file set is, what a path spells, or what bytes get hashed must land in
every client in the same commit.** The concrete list:

- **The file set**: the `.mindex` walk (`tools/indexer/src/scanner.rs::scan`,
  `tools/watcher`'s `build_manifest`,
  `tools/vscode/src/scanner.ts::scanWorkspace`) — excludes-before-includes,
  glob dialect (`globset` with `literal_separator(true)` vs picomatch
  defaults), symlink policy, and the extension map (**three** copies,
  **Languages** step 6).
- **The bytes**: what is hashed for `/drift` must be exactly what `/index`
  would post — the server hashes `code.as_bytes()`, so a client hashing
  anything else (raw vs decoded, BOM kept vs stripped) reports permanent
  drift.
- **The refusals**: a file a client will not post must not appear in its
  manifest either. Binary, unreadable and over `mindexfile::MAX_CODE_BYTES`
  files are dropped from both, in every client (`scanner.ts` keeps its own
  copy of that constant). Claiming a file the server would reject is worse
  than dropping it: the 400 fails the whole batch, and the file reports
  `missing` forever.

This class of bug **never surfaces as an error** — only as drift reindexing
cannot clear, or an index quietly missing a third of the tree; hence the
checklist. `tools/mindexfile` exists to shrink the surface — the one Rust
parser, now also holding the shared size cap; the TypeScript mirror
(`mindexFile.ts` + `globContract.test.ts`'s shared fixture table) is the only
sanctioned copy.

**Git history is deliberately outside this rule**, single-producer — see **Git
history channel**. A commit list is not a file set, a path spelling, or hashed
bytes; do not "fix" that by teaching the watcher or the extension to walk git.

And the client is only as fresh as its build: the extension runs `dist/`, so a
change to `src/` never `npm run compile`d leaves a plugin scanning by
yesterday's rules. Recompile before concluding the plugin is wrong.

## Tooling gotchas (full docs: each tool's `--help` / README)

- `mindex-index`: identity and scope come from `.mindex` at `--root`;
  `--project`/`--include`/`--exclude`/`--language` **replace** (never extend)
  the matching key. `--print-guid` resolves the GUID and exits.
  `chunk_count == 0` = sliced to no chunks (<128 tokens), *not* unchanged —
  hash-unchanged files are absent entirely. `--check` runs `POST /drift`
  instead of uploading; non-zero exit on actionable drift (`--json` for
  scripts). `--force` bypasses the unchanged-skip (hash, derivation versions
  *and* model identities) — an escape hatch for what versioning can't see, not
  routine; scope it with `--include`/`--exclude`. `--symbols-only` rebuilds just
  the symbol table (no GPU, no Qdrant); its summary counts symbol rows, not
  chunks. `--vectors-only` re-embeds the stored chunks into the active model's
  collection (no slicing, no symbols) — what a `[model].id` change costs when
  the tokenizer is unchanged; it refuses files sliced by a different tokenizer,
  and cannot be combined with `--symbols-only`. `--history` additionally
  reconciles the git channel (off by
  default; `git_refs` in `.mindex` picks the refs, `--git-ref` replaces the
  list like every scope flag); `--history-only` restricts a run to that phase
  *without* switching the channel on. Watch the drop counts it prints.
- `mindex-watch`: inotify daemon — debounced reindex/delete (`--debounce-ms`,
  1000) + full drift sweep every `--drift-interval` (300 s) for offline
  changes. Reads `.mindex`. `--dry-run` makes no mutating call but still runs
  the read-only drift check.
- `mindex-search.sh`: the single search frontend. Prints results **ascending
  by score** (best match last, above the prompt); every option has a
  `MINDEX_*` env fallback (flag wins). Language validation fetches
  `GET /config` at runtime; baked-in `VALID_LANGS` is only the offline
  fallback. 404 = no match, not an error.
- MCP `mindex` (`tools/mcp/mindex/`): the **primary agent interface** —
  `search` (top-5 cap fixed in the adapter), `symbols` (exact-name **definition**
  lookup, 10-row cap — it does not answer who uses a name; grep does, lexically),
  `index_files`/`delete_files`, `drift`,
  `cancel_indexing`, read-only introspection. `index_files` is **only** for the few just-touched
  files, bodies passed **verbatim** (unchanged files are hash-skipped
  server-side); bulk jobs go through `mindex-index`. `search` takes optional
  `include`/`exclude` (`{paths, programming_languages}`) passed straight to
  `/search`. No network at handshake.
- VS Code (`tools/vscode`): the **Ask** sidebar WebviewView (`askView.ts`) is
  the one input surface for Search/Research (segmented toggle); search
  results stay in the QuickPick, research streams into a WebviewPanel; the
  SSE client is hand-rolled in `api.ts` (no reconnects — a drop is a cancel,
  by contract). **Full UX rationale: `docs/claude/vscode.md`** — read it
  before modifying the extension. Load-bearing rules: Research History is an
  editor-area panel — **one full-width list, no reading pane**; a selected
  run expands under its own row (provenance, ancestry, moved files, actions)
  and the report itself opens only as a Markdown tab, so nothing in that
  panel renders it (keyset paging by `seq`; one `AbortController` aborted
  per keystroke, and callers swallow `AbortError` themselves — `api.request`
  *rejects* on abort while `research()` resolves; keep the asymmetry). The
  context QuickPick offers **valid runs only**, tracks
  `onDidChangeSelection` (not `onDidAccept`'s visible selection), and
  `undefined` (cancel) ≠ empty array (clear). Stored reports open as
  read-only Markdown documents (scheme `mindex-research`) with a provenance
  block. The form offers only server-confirmed inventory (`chunks_active >
  0` languages, `research.models` via `StatusMonitor.refresh()`, which
  re-reads `/config` every pass) but validates against `ALL_LANGUAGES`
  (offering is a hint, validating is a contract; `undefined`/empty inventory
  falls back to `ALL_LANGUAGES`); state is pushed by `postMessage` and
  rebuilt, never by reassigning `webview.html`. `Availability {ask,
  research, reason}` is split (Ollama down disables only Research; the
  server says `degraded`, not `unhealthy`), and derived by `readHealth` from
  `status` **and** `checks` together — an older server spells the *required*
  failure `degraded` too, so keying on `status` alone would paint yellow and
  leave the form armed. **A degradation freezes the form and never
  the tabs**: both mode buttons stay live in every state (a disabled tab is a
  dead end whose explanation lives behind it), and the notice inside the mode
  names the missing dependency *and* what it costs in this tab; when the mode
  on screen cannot be served, every control inside `#form` is disabled through
  `setEnabled` except the mode switch, Stop and the notices' own
  Open-Server-Status links — and anything that rebuilds controls must re-run
  that sweep. `canSubmit()` gates the click *and* the Enter keydown — one
  predicate, or a keyboard path fires research at a dead Ollama. A degradation also aborts running work via `RunRegistry`,
  resetting handles **before** any notification (its thenable resolves only
  on dismissal), reported as a failure, not a cancellation; none of it is
  observable without `[mindex.statusPollSeconds]` (default 30, `0` = off).
  **The stored token is the second input to that same `Availability`**, folded in
  by `mergeAvailability` at the point of use rather than inside `fetchStatus` —
  the two have different lifetimes, and merging late is what lets a token stored
  now repaint the form without waiting for the next poll. So a `search`-only
  credential is a *supported way to run the extension*, not a broken one: it
  never refuses to activate (a read-only client is what a narrow token is for),
  it freezes the Research controls and names the missing action, and the tabs
  stay live under the same rule as above. It is a **hint** — `tokenAvailability`
  reads an unverified payload, so it decides what to offer while the server
  decides what to serve, the language-picker stance. The token reason **wins**
  over the health reason — but only while `health.ask` is still true: a
  dependency comes back by itself and a missing action does not, which stops
  being the useful ordering once the server can serve nothing, where naming the
  token sends the user to re-mint a credential that was never the problem. `#ollama-notice` therefore takes its
  sentence from the host (`#ollama-reason`) instead of naming Ollama in markup,
  since the same notice now has two causes with different remedies.
  `reindex()` checks `index` up front for the same reason — without it a batch
  403s file by file and renders as a partial reindex.
  **A brand-new project is the sharp case**: `createProjectFile` writes a fresh
  UUID no token names, and every later request answers 404 `project.not_found`,
  byte-identical to a GUID nobody indexed. Nothing downstream can tell them
  apart, so `createProjectFile` hands the GUID to its caller and the caller says
  so. Deliberately **no button on that message**: a wildcard token already covers
  the GUID and never gets there, and a named-project token is refused by
  `may_mint` for a project it does not hold — the remedy is genuinely on the
  host.
  **Every server-touching button single-flights through `BusyKeys`**
  (`src/busy.ts`; `[data-busy-key]` + `applyBusy`/`setEnabled` in the
  webview) — supersede reads, **refuse** writes and paging, and the greyed
  button is the echo of the host's refusal, never its cause. **No raw error
  reaches a user**: `humanize(e)` in `problem.ts` is the one funnel, the
  machine `code` never appears in the sentence, and the stack goes to the
  `MINDex` output channel via `logError`. Every request has a deadline
  (`mindex.requestTimeoutSeconds`, health clamped to 5 s); a stream's is
  **idle-only** (`mindex.streamIdleTimeoutSeconds`), never total — a `high`
  run may legitimately live 70 minutes.
  Language marks: generated `shared/langGlyphs.ts` (never hand-edit),
  two-toned colours derived and recomputed by `langIcons.test.ts`.
  The challenge surfaces: history rows/preview carry kind/trust badges and a
  challenge↔subject link built from the **server-resolved**
  `challenged_seq`/`challenged_title`; launching one is a QuickPick chain
  (`challengeFlow.ts` — the server accepts only effort/model/budget/seed, the
  subject supplies the rest), streamed into the ordinary `ResearchPanel`
  under the same single-flight handles; `challengeGuard`/trust wording live
  in `shared/runsFormat.ts`; offline re-verify (Verify button) renders
  provenance and staleness as separate answers; `GET /research/active` +
  cancel is a palette QuickPick (`activeRunsPick.ts`), which the 429 names.
  **The expanded row always states what was said about a report**
  (`challengeStateLine`, one indexed `challenged_run_id` lookup per opened row):
  it used to render a trust badge and nothing else, and trust is correctly
  silent about an inconclusive challenge and about one whose own evidence has
  moved — so a report that had been challenged and *refuted* could read as
  untouched. The lookup asks for `limit: 2` because the server's replace rule
  is verdict-gated, so two rows is a real state the line must name rather than
  silently pick from. With a challenge standing, the button becomes
  **Re-check** and forks (`recheckOptions`): "Links only" is
  `GET …/{challenge_id}/verification` — the *challenge's* citations, captioned
  as such, since reading them as the subject's would be a worse confusion than
  the one being fixed — and "Fresh run" goes through the same modal that names
  the verdict at risk. **Pruning**: `Select all` pages the server with the
  current filters to exhaustion (capped by the published `max_delete_ids`,
  `MAX_PAGES` as a runaway backstop) and the footer says when it stopped short;
  a bulk selection is *defined by* those filters, so any filter change clears
  it wholesale rather than pruning it row by row; the confirmation resolves
  through `summaries` (every row ever fetched), not `rows` (the rendered page),
  or a bulk delete would report `0` dependants for everything off screen.
  `Collect garbage` proposes the union of invalid/stale/partial/inconclusive
  (**pinned exempt, via the server's `pinned=false`**) into a review that
  takes the whole panel — not a QuickPick, which cannot show *why* a row is
  proposed or that four reports were built on it, the two things a reviewer
  unchecks over —
  each run in one group with its other reasons as labels, since three
  checkboxes for one report would let an uncheck not stick. The counts line is
  the corpus `totals` — `N reports · N challenges · N valid · N outdated`, a
  **fixed denominator** no filter moves, and *silent* when there is nothing to
  count (the empty middle of the panel already says so, in words and in two
  wordings: nothing stored versus nothing matched). The filter row
  is four selects and nothing else — the `Show` label and the
  `Outdated`/`Partial` preset buttons were a second, competing way to set one
  filter. Head chrome: the magnifier is inside the field, the refresh button
  sits in the search row, and the whole panel is drawn in the shared `--mx-*`
  tokens and controls rather than a palette of its own. In Server Status, the
  health card is **one dot per dependency in every state** (colour is the
  severity; the word beside it carries it without colour), laid out as a 2×2
  block: identity left (dot, name, `optional` badge, and under them what the
  dependency is for), verdict right (`ok`/`failed`, and under it what that
  state costs). Everything saying *what this is* stacks on one edge and
  everything saying *how it is doing* on the other, or a column of rows scans
  as five kinds of text taking turns. `mindex.browseResearch` is **gone** — one reading
  surface, and `ctrl+alt+,` now opens the panel. A reindex
  reads the server's claims from `/status.indexing_claims` + the follow-up
  `/drift`'s `indexing` bucket — never from the `/index` response, which
  swallows claim conflicts and 200s (a refused reindex otherwise reads as
  `unchanged`); the drift check runs **before** the summary; the poll drops
  to 3 s while claims are outstanding; everything funnels through the one
  `reindex()` helper (total re-entry guard, the `reindexRunning` flag in
  `activate()`). Progress is a **feed**, not a percentage (indexing is
  batched): `IndexFeed` + `RateWindow` over the server's **cumulative**
  `chunks_done`; a `withProgress` message is structurally single-line, so
  paths live in a `StatusBarItem` tooltip. Drift's `Sync all` is a synthetic
  row present only while there is actionable drift; its prose lives in
  `viewsWelcome`, not `TreeView.message`.
- MCP `scout` (`tools/mcp/scout/`): token-economy layer, two tools —
  `research`, a thin SSE client over `POST /v0/{guid}/research`, and
  `challenge`, the same client pointed at
  `POST /v0/{guid}/research/{run_id}/challenge`. Both send **`?stream=yes`**
  (`STREAM_QUERY`) now that frames are opt-in, and the reason is local to that
  file rather than a preference: `READ_TIMEOUT` (120 s) is an *idle* timeout
  resting on the 15 s keep-alive and is the only thing there that separates a
  working server from a wedged one, and the partial-report salvage
  (`truncated_by_client`/`live_run_id`/`still_running`) only works because bytes
  arrive as they are produced. scout is the caller the streaming mode is *for* —
  it reads to `done` by construction; the new default is for the caller that does
  not. (One `_run` consumer for both,
  so the reader whitelists cannot drift; `_VERDICT_KEYS` reads the challenge
  stream's extra event, and `_INSTRUCTIONS` teach the caller the trust field
  and the two rules a verdict must be read under — inconclusive ≠ acquittal,
  ungrounded ≤ disputed). The whole
  investigation runs on the server's local model; scout holds no prompt, no
  chunk budget, no Ollama connection — it owns the *reader* (`_STEP_KEYS`,
  `_USAGE_KEYS`, `_CITATION_KEYS`, whitelists that silently drop unknown
  fields) and the `_INSTRUCTIONS` telling the caller to trust the report but
  check `citations.unverified_paths` and `done_reason`, and to chain
  follow-ups via `context_run_ids` rather than re-investigating from cold.
  The cheap-breadth half; `mindex.search` is the paid-precision half. Fully
  removable layer.
- `.mindex` (repo-root, committed — index scope is part of the project):
  **YAML**, required `guid:` (either UUID spelling, normalized) + optional
  `exclude_paths:`/`include_paths:`/`languages:`/`git_refs:` **lists**.
  Unknown key = error (`deny_unknown_fields`), scalar-instead-of-list = error
  (a mistyped `exclude_path:` would otherwise index the tree it was meant to
  keep out). One file, repo root, no nesting. **`tools/mindexfile` is the
  only Rust parser** (indexer + watcher path-depend on it);
  `tools/vscode/src/mindexFile.ts` mirrors it; the post-commit hook parses
  nothing (shells out to `mindex-index --print-guid`). Adding a fourth parser
  is the mistake this crate exists to prevent. The extension also *writes*
  one (`tools/vscode/src/mindexTemplate.ts`, from the Drift view's welcome
  button) — its header comment restates the schema in prose and so drifts
  silently; keep it in step alongside the parser. Its exclude list is
  deliberately thin — only unambiguous root dot-dirs active, build artifacts
  commented out (a wrong guess shrinks the index with no error, and excludes
  apply *before* includes, so a blanket rule cannot be carved back open).
  Globs are root-relative, forward-slash, `*` stopping at `/` (`globset`
  needs `literal_separator(true)`; picomatch does it by default) — the
  shared fixture table in `mindexfile`'s tests and `globContract.test.ts`
  pins the subset; divergence surfaces as permanent phantom drift, not an
  error. The MCP servers don't parse it — the agent reads it and passes GUID
  + filters as call args.

## Docker & CI

- Toolchain pinned 1.95 (`libsqlite3-sys 0.38` needs ≥1.87). `cargo-chef` is
  **not** used (needed 1.88+, conflicted) — layer caching is
  `cargo fetch --locked` over a stub `src/main.rs`. Legacy builder supported;
  no `--mount=type=cache`.
- Three compose files, same `Dockerfile`:
  - **Prod** (`docker-compose.yml`): qdrant + mindex. The perf harness is
    host-side scripts in `perf/` driving this stack (`command:` flags read
    env; swap profiles via `--env-file perf/env/<f>.env`). **No host ports**;
    outbound-only `extra_hosts: host.docker.internal:host-gateway` reaches
    the host-run embedder (`:12434` by default, deliberately not composed —
    a model server plus a GPU runtime is a multi-gigabyte image and a device
    the compose stack has no business claiming). TOML-only knobs require
    mounting a `config.toml`.
  - **Exposed overlay** (`docker-compose.exposed.yml`): opt-in via `-f`;
    publishes API (`11111`) + Qdrant dashboard (`6333`) on `127.0.0.1` only
    (neither has auth). The sanctioned way to open the stack.
  - **Test** (`docker-compose.test.yml`): qdrant + mock-embedder + mindex +
    test-runner. Run with `--exit-code-from test-runner
    --abort-on-container-exit`. Healthchecks use `/dev/tcp` / `urllib` (no
    curl in images). Mounts `tests/integration/mindex-test-config.toml`
    (small caps) so limit tests can exercise edge rejections. **Edit
    `v2.0.0_schema.sql` and you must `down -v` before the next run**: a
    volume already stamped at 1 skips the edited schema in silence and every
    request touching a new column 500s with `no such column` — the price of
    editing the schema in place, only paid pre-release. A volume older than
    the v2 baseline is a different failure and a louder one: mindex refuses
    to start on the old lineage, which is `down -v` once, on purpose.

## Tests

- **Unit**: `cargo test --bin mindex`; each `tools/` crate carries its own.
  Read the test files for coverage — highlights: the connection-leak and GC
  orphan-prevention regressions, the `codes_are_stable` snapshot,
  trigger-level illegal transitions, `sweep_candidates` selection rules. No
  server/Docker; some slicer tests need the registry tokenizer
  (`Qwen/Qwen3-Embedding-0.6B`) in the HF cache — on an offline host, pre-cache
  it, since the Rust side has no `HF_HUB_OFFLINE` equivalent (a
  fake-`Tokenizing` test avoids it).
- **Three seams exist only so an untestable thing became testable**, each
  because the code it guards had regressed *without failing anything*. Reach for
  them rather than inventing a fourth. `router_state()` (handlers tests) builds
  a whole `RouterState` from `Config::default()` + refusing fakes — the eight
  `*_core` functions take one, so their real SQL was previously reachable only
  through the research fakes that replace them; its one hard field, the
  tokenizer, is the trivial `WordLevel` one `fixture()` already uses.
  `ResponseSink` (`http3.rs`) is the three-method seam under the h3 frame pump,
  which could otherwise be driven only by a live QUIC stream — and buffering the
  body raised no error and broke no test while removing streaming from both SSE
  endpoints. `apply_migrations_from` takes the migration list because the real
  one passes `pragma_foreign_key_check` by construction, so the rollback guard
  had nothing to refuse.
- **A test that pins a claim about a failure should be checked against that
  failure**, not merely written — reintroduce the bug, watch it go red, restore.
  Several tests here assert something true for the wrong reason otherwise: a
  pre-cancelled token drives a perfectly normal research run (the fakes do not
  observe it — a *closed channel* is what a disconnect looks like), and under a
  token cancelled up front no GC phase can fail, so `error` outranking
  `cancelled` needs a store that fails *and then* cancels.
- **Integration** (`tests/integration/`, pytest in Docker): mock embedder
  returns deterministic vectors seeded by text hash (stable ranking
  assertions). Fresh project GUID per test. Suites map by filename
  (`test_e2e`, `test_filters…`, `test_management`, `test_validation`,
  `test_concurrency`).
- **The stack holds two servers**, and the second one is why: `mindex-auth` runs
  with `[auth].enabled` while `mindex` does not, because the whole existing suite
  asserts the *unauthorized* behaviour and that is the coverage worth keeping —
  an auth-off deployment must stay byte-for-byte what it was. `test_auth*.py`
  drive the second (`auth` fixture, `AuthClient`), and every one of them skips
  rather than fails when `MINDEX_AUTH_URL` is unset, since a missing authorized
  server means the suite is being run some other way, not that something broke.
  Two shapes it is easy to get wrong: the auth server **mints its own credentials
  in its entrypoint before exec'ing the binary**, because `--abort-on-container-exit`
  tears the run down when *any* container exits and a one-shot bootstrap service
  exits by design; and its `--key-id … --new-key` falls back to plain `--key-id`,
  because the key volume persists and `--new-key` rightly refuses an id that
  already exists. Every narrow token is minted **from** the root one through
  `POST /auth/tokens`, so the containment rule is exercised dozens of times as a
  side effect of setting scenes.

## Linting (zero warnings everywhere — non-default flags matter)

- Rust: `cargo clippy --bin mindex` + `cargo clippy` in each `tools/` crate
  (own workspaces), and `cargo fmt --check` in each — all four crates are
  edition 2024, where `collapsible_if` fires on the `if let` + `if` nesting
  that let-chains replace, and rustfmt's 2024 style edition sorts imports
  differently.
- VS Code (`tools/vscode`): `npm run check` = prettier + eslint + `tsc` + the
  `node --test` suite (`src/*.test.ts`, compiled to `dist/`).
- Shell: `shellcheck scripts/entrypoint.sh`, `shellcheck --shell=bash
  tools/search/mindex-search.sh`; format `shfmt -i 4 -ci` (bare shfmt
  defaults to tabs).
- Python (`tests/`): `ruff check`, `ruff format --check` **and**
  `black --check` (kept compatible), `mypy` (`fastapi` is `# type: ignore` —
  stubs only in the mock's image). Run mypy **per directory** — `mypy tests/`
  fails with `Duplicate module named "main"`:
  `for d in tests/integration tests/mock_embedder tests/mock_ollama; do mypy $d; done`.
- Python (MCP servers): the same four, per server —
  `(cd tools/mcp/scout && ruff check . && ruff format --check . && black --check . && mypy src)`,
  likewise for `tools/mcp/mindex`. Easy to forget: neither is under `tests/`.
- SQL: `sqlfluff lint src/db/migrations/` (dialect/layout from repo-root
  `.sqlfluff`; schema is intentionally column-aligned).
- Prefer a scoped `#[allow(...)]`/config exclusion **with a reason** over
  contorting code; never project-wide suppression.

## When modifying code

1. New loops touching Qdrant/SQLite/embedder must respect the
   `CancellationToken`.
2. Multi-row DB writes go inside a `transaction`.
3. New endpoints: register in `backend::http3::run`, use `RouterState`,
   `{param}` routes, `#[debug_handler]`, the `ApiJson`/`ApiPath`/`ApiQuery`
   extractors, return `Result<_, ApiError>`, validate at the top via
   `backend::v0::validate` (new check = new `ApiError` variant + arms +
   `codes_are_stable` + a unit test). Add a `#[utoipa::path]` annotation
   (existing tag, every error `body = ProblemDetails`, a `**Concurrency:**`
   note) **and** an entry in `openapi.rs` `paths(...)` (+ new types in
   `schemas(...)`) — a handler missing there is silently absent from Swagger;
   `openapi_spec_is_complete_and_versioned` guards the count. Swagger UI at
   `/swagger-ui` (assets vendored, no network).
4. Reach Qdrant only via `VectorStore`; collection names via
   `collection_for`.
5. Any search-path SQLite query must include `AND c.status = 'active'`.
6. Status writes use `set_file_status` and must be a legal transition
   (triggers enforce it). New status-changing paths need a transition test.
7. Adding a language → the full checklist under **Languages**.
8. Schema change → new migration in the `MIGRATIONS` slice with the next
   sequential version; startup applies those above `PRAGMA user_version`,
   then stamps it. All SQL `IF NOT EXISTS` (cold re-run = no-op, enforced by
   `every_migration_sql_is_idempotent`). SQLite can't `ALTER` a `CHECK` onto
   an existing table — add new constraints as `BEFORE INSERT/UPDATE` triggers
   (the status-machine pattern, additive). New *columns* are equally blocked:
   `ADD COLUMN` has no `IF NOT EXISTS` form, so it fails the idempotency
   test. **v1.0.0 is frozen** — an in-place edit is skipped in silence on any
   database stamped at 1; first symptom is a 500 with `no such
   table`/`no such column`. New *tables* are the easy case
   (`v1.1.0_git_history.sql`). **Widening a constraint, and adding a column,
   are both answered by the table rebuild** — `v1.1.0_toml_yaml_languages.sql`
   is the precedent; copy its shape: create the replacement under a temporary
   name, copy rows with columns **named** (`SELECT *` binds by position),
   `DROP` the original, rename, recreate its triggers (the `DROP` took them).
   It runs under `SQLite3Pool::migration_transaction` because both halves
   need foreign keys suspended (rename-first makes the children follow the
   discarded table; `ON DELETE RESTRICT` refuses the `DROP`);
   `apply_pending_migrations` pays that back with one
   `PRAGMA foreign_key_check` before stamping. Idempotency comes from the
   leading `DROP TABLE IF EXISTS <tmp>`. Rehearse on a copy of a real
   database and compare row counts per table. **A 1:1 side table is not the
   answer for a new field** (the three that existed each cost a hot-path
   JOIN); `v1.2.0_research_context.sql` is the precedent for rebuilding to
   add columns. One rebuild consequence: the FK suspension also suspends
   `ON DELETE CASCADE`, so dropping the old table does **not** take child
   rows with it — `id` surviving the copy is what makes them still resolve;
   load-bearing, pinned by
   `rebuilding_research_runs_keeps_the_baselines_that_reference_it` (through
   an ordinary transaction the same migration silently erases every child
   row).
9. Changing how chunks or symbols are derived → bump the matching const under
   **Derivation versions** — that is what makes the change reach files
   already indexed; skipping it leaves them stale behind a matching hash,
   silently.
10. Changing which files a project contains, how a path is spelled, what
    bytes are hashed, or which files a client refuses to post → **the full
    list under Four clients, one working-tree view**, in the same commit. One
    client changed alone is not a smaller version of the change; it is
    phantom drift.
