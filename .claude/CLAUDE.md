# CLAUDE.md — mindex architecture & conventions

Only what is **not obvious from reading the code**: invariants, non-trivial "why",
gotchas, regression guards. No flag tables (`--help`), no per-test lists (read the
tests), no language table (the `ProgrammingLanguage` enum + `Cargo.toml`), no
struct/SQL dumps. Accepted limitations are stated next to the invariant they
qualify, not collected in a list of their own.

## Overview

`mindex` is an async RAG indexing + search engine in Rust. HTTPS API → `tree-sitter`
AST chunking → `BGE-M3` multi-vector embeddings (dense/sparse/ColBERT) → `Qdrant`
vectors + `SQLite3` metadata. Internal service: TLS is the only transport security,
no API auth.

Since TLS is the *whole* of that security, every client verifies it the same way and
the options are uniform: the **OS trust store** by default (which is where mkcert and
corporate roots install themselves, so the common local setup needs no option at
all), an extra CA named explicitly when it is not installed there
(`--ca-cert` / `ca_cert` / `MINDEX_CACERT` / `mindex.caCert`), and
`--no-verify` / `MINDEX_NO_VERIFY` — which verifies *nothing* and exists for the
self-signed certificate `scripts/entrypoint.sh` generates, not as a way past a
certificate problem. Two traps this has already sprung: reqwest's `rustls-tls`
feature trusts only its own bundled Mozilla roots, so a Rust client needs
`rustls-tls-native-roots` to see the OS store at all (without it a locally-issued
cert fails every request, and `--no-verify` is the only way through — which is how
both CLI tools silently stopped indexing); and the MCP `drift` tool **shells out to
`mindex-index`**, so a CA setting that reaches the Python process but not the child
makes one tool of that server fail while the rest work.

TLS being the whole of that security is also why reaching mindex from anywhere else
is a *proxy's* job, not a feature of this server: it stays unauthenticated and
loopback-bound, and something in front of it decides who may connect. Clients carry
one optional header for that — `X-Api-Key` (`--api-key` / `api_key` /
`$MINDEX_API_KEY` / `mindex.apiKey`), uniform across all of them for the same reason
the CA options are. It is **additive**: unset sends no header, so the direct
`https://127.0.0.1:11111` path is byte-for-byte unchanged, and mindex itself never
reads it. The header travels on the client, not the request builder, so it reaches
every endpoint including the ones a future client adds — and including
`mindex-search.sh`'s `/config` probe, which is easy to miss: behind a gate a keyless
probe is refused, and the script would quietly fall back to its built-in language
list rather than report anything. Prefer the environment variable over the flag; a
flag value is visible in `ps` to every user on the machine.

## Configuration (TOML file + CLI flags)

`config.rs` owns it; the indexer and watcher CLIs mirror the same scheme in their own
`config.rs` (`mindex/indexer.toml`, `mindex/watcher.toml`; examples: `*.example.toml`).
Precedence **CLI flag > TOML > compiled default**. Defaults live *only* in the
`Default` impls — clap holds no `default_value` (every flag is `Option<T>`, so
"passed" is distinguishable from "absent"; that's what makes the layering work).
`resolve()` finds the file by XDG canon (`--config`/`$MINDEX_CONFIG` →
`$XDG_CONFIG_HOME` → `$XDG_CONFIG_DIRS`; missing file = defaults), logs every path
checked, the source loaded, and every flag override, then validates: *all* problems
collected (not fail-fast) with what/why/how-to-fix messages; any error aborts
(`deny_unknown_fields` makes a mis-typed key a parse error). Keys carry their unit
suffix (`*_ms/_seconds/_minutes/_days/_chunks/_tokens/_bytes/_points/_mib`).

**Only genuine tuning knobs are configurable.** Structural invariants stay `const`
next to their code with a "why not configurable" comment (BGE-M3 `VECTOR_DIM` 1024,
`ENCODE_MAGIC`, `COLLECTION_SCHEMA_VERSION`, HTTP 499, the SQLite PRAGMAs). Config
values reach code through constructors/params, **never globals**. New knob = key in
the right `config.rs` section + its `Default` + a validation rule, threaded to the
consumer (don't reintroduce a `const`). Request-shape limits are knobs too: the
`[limits]` section and `[search].max_top_k`/`max_query_bytes` bound a request at the
API edge (via `RouterState` → the validation layer). They are **TOML-only** (no CLI
flag) — tuning them in a container means mounting a `config.toml`.

## Layout (non-obvious bits only — the tree itself is one `ls` away)

- `tools/indexer` (`mindex-index`) and `tools/watcher` (`mindex-watch`) are **own
  crates with own `Cargo.lock`, not in the root workspace**, both path-depending on
  `tools/mindexfile` (the `.mindex` parser); `tools/mcp/{mindex,scout}`
  are Python/Poetry MCP stdio servers; `tools/search/mindex-search.sh` is the bash
  search frontend.
- `embedder/` is the vendored BGE-M3 server (3 heads) — **host-run + GPU, NOT in the
  Docker image** (see `embedder/README.md`). On this host it is a systemd **template**,
  `mindex-embedder@{egpu,igpu}` (unit + env files in `embedder/systemd/`, symlinked into
  `~/.config`): same server, two torch backends (ROCm / Intel XPU) in two venvs
  (`.venv-%i`), **mutually exclusive** via a symmetric `Conflicts=`+`After=` naming both
  instances — systemd drops the self-reference, so one template serves both. `@igpu` is
  the default: it leaves the discrete card entirely to the research LLM, whose KV cache
  and BGE-M3 stopped co-fitting once `max_num_ctx_tokens` grew. Switch to `@egpu` for
  bulk reindexing (~17× faster on a batch; the query path is ~28 ms either way). The
  backends are **not** bit-identical and nothing checks that they are — the
  split-embedder warning under **Retrieval pipeline** applies across time here rather
  than across two live servers — but they were measured interchangeable (dense cosine
  0.999996, sparse Jaccard 0.9968) **once XPU is kept off its default attention
  kernel**, which returns NaN for padded rows in fp16 and still answers 200. That is
  `attention_backend()` in `__main__.py`; removing it silently corrupts every batch of
  more than one text.
- Migrations live in `src/db/migrations/`. **Four**: `v1.0.0_schema.sql` (version 1)
  — the whole 1.0.0 schema, in the order SQLite needs (tables before the triggers
  and foreign keys that name them) — `v1.1.0_git_history.sql` (version 2),
  which adds `project_commits` + `project_commit_paths`, and
  `v1.1.0_toml_yaml_languages.sql` (version 3), which rebuilds `project_files` to
  widen its `programming_language` CHECK, and `v1.2.0_research_context.sql`
  (version 4), which rebuilds `research_runs` to add `seq`/`expires_at`/
  `context_run_ids_json` and adds `research_run_files`. The applied set is the
  `MIGRATIONS` slice in `main.rs`, keyed by the integer that lands in
  `PRAGMA user_version`; the filename's version is documentation. **v1.0.0 is now
  frozen** — editing it in place no longer reaches a database stamped at 1, since
  the filter is `version > user_version`, so the edit would be skipped in silence.
  Nine tables: `projects`, `project_files`, `project_file_chunks`,
  `project_file_status_log`, `project_file_symbols`, `research_runs`,
  `research_run_files`, `project_commits`, `project_commit_paths`. There are
  **no 1:1 side tables** — the three that existed were
  artefacts of `ADD COLUMN` having no `IF NOT EXISTS` form, and folding them back
  into their parents removed a JOIN from the prepare-phase skip and two INSERTs
  from every journalled run. `ADD COLUMN` is still blocked, so a new *field* is a
  table rebuild rather than a side table (rule 8); `research_run_files` is a
  genuine 1:N child, not a revival of that pattern. Note `sqlfluff` skips files over its default 20 kB and
  only *warns*, so `.sqlfluff` raises `large_file_skip_byte_limit` — without it the
  schema is silently unlinted.
- `scripts/entrypoint.sh` generates a self-signed cert on first container start.
- `rust-toolchain.toml` pins 1.95.

## Core invariants (violating these causes bugs)

**Project isolation = collection + has_id filter.** One Qdrant collection per
project, `{guid_simple}_v1` (`COLLECTION_SCHEMA_VERSION`, `qdrant.rs`); always derive
names via `collection_for(project_guid)`, never hardcode. Within a collection the
candidate set is a `has_id` filter built from SQLite (`qdrant_guid` for chunks
matching project + filters + **`status='active'`**) — this is the *sole* isolation
mechanism and also excludes soft-deleted vectors. That filter lists every candidate
GUID, so it grows linearly with a project's active-chunk count — fine at this
scale; a very large collection would want a stored Qdrant payload field
(`project_guid`) plus a `match` filter instead.

**Append-only hot path.** Indexing never deletes from Qdrant. On reindex (sha256
mismatch) old chunks are marked `deleted` in SQLite, new ones inserted `active`, new
vectors upserted; old vectors orphan until GC removes them (decouples indexing
latency from Qdrant delete latency).

**Symbols parallel chunks, but hard-delete.** `project_file_symbols` (defs/refs from
the language's upstream tree-sitter tags query — one universal extractor,
`slicing/symbols.rs`, zero per-language code; vendored query data in
`slicing/queries/` where the crate exports none) has **no Qdrant counterpart**, so
its lifecycle is the *opposite* of chunks: hard `DELETE`, no soft-delete/GC.
Invariant: every tx that marks a file's chunks `deleted` (reindex-prepare,
`DELETE /files`, `/cancel`, `drop_cancelled`) deletes its symbols in the same tx;
`DELETE /projects/{guid}` likewise drops them in its one hard-delete tx. Inserts
happen in the prepare tx alongside chunk inserts. FK RESTRICT backstops the
`prune_deleted_files` ordering. Extraction failure degrades to "no symbols" (WARN),
never fails indexing. `POST /v0/{guid}/symbols` is exact-name lookup returning
**ranked candidates + full totals** (never a single "the" answer — collisions are
contract); ranking is purely path-based (anchor file > its exact dir > rest).
Empty result = 200, not 404.

**GC hard-deletes only confirmed rows** (regression guard, `worker/gc.rs`). A sweep
deletes from SQLite *only* chunks whose Qdrant `delete_batch` succeeded; failed
collections keep their rows `deleted` for the next sweep (deleting the SQLite row
first would orphan the vector forever). If every collection in a batch fails, the
loop breaks rather than spinning. The same pass prunes `project_file_status_log`
(`[workers].status_log_retention_days`, default 30 — a threaded config value, not a
`const`) and runs `prune_deleted_files` — drops `deleted`
`project_files` rows once their chunks are gone (the guard is `NOT EXISTS` over
*any* chunk row, not just active ones; FK is RESTRICT, so only after the sweep);
that sweep-then-drop ordering is what makes `DELETE /files` eventually physical. `POST /gc` runs the same `gc::collect` synchronously. GC is **global**,
serialized process-wide by `GcGuard` (an `Arc<AtomicBool>`): `POST /gc` during a
running pass returns **409**, the hourly worker skips its tick if a manual pass holds
the flag. The guard frees on `Drop`, so a panic can't wedge GC off.

**Status state machine** (`project_files.status`), enforced by SQLite triggers
(`project_files_status_{insert,update}_guard`), not just convention. Legal moves: **any → `indexing`** (start / reindex /
retry — this includes `deleted → indexing`, resurrection), **any → `deleted`**
(`DELETE /files`), and **`indexing` → `indexed`|`cancelled`|`failed`** (a terminal is
reachable only from in-progress work); a new row may only enter as
`just_uploaded`/`indexing`, never straight into a terminal. Anything else (e.g.
`failed→indexed`) raises `SQLITE_CONSTRAINT_TRIGGER`.

- `indexing` is committed durably *before* heavy work (crash-recoverable; the retry
  worker picks up files stuck longer than `--stuck-grace-mins`, default 30). That
  grace **must exceed the longest in-flight request**: cross-file batching holds a
  whole batch in `indexing` through the embed pass; a too-short grace makes the
  worker race a live batch.
- A stuck file with **no active chunks** (sliced to 0) is marked `indexed`, not
  `failed` (`failed→indexed` is illegal — a wrong `failed` would trap it).
- `sha256` is (re)written on entering `indexing` and confirmed at `indexed`, so the
  stored hash always matches the chunks in the table; the `retry_count` reset lands
  only on `indexed`.
- Status writes go through `db::files::set_file_status` (stamps `status_updated_at`,
  WARNs on rejection); AFTER-triggers log every transition to
  `project_file_status_log`. A file exhausting `MAX_RETRIES` (3) stays `failed`
  forever (`warn_permanently_failed` surfaces it at startup and hourly).

**sha256 + derivation-version skip / empty 404.** Identical content is skipped by
hash — but only if the *derivation versions* also match, because a hash answers
"did the file change", not "did the code that derives chunks and symbols from it
change". `file_already_indexed` requires `project_files.chunks_version` and
`symbols_version` to equal the current consts; both are nullable, and NULL — a file
derived by an unknown version — can never match, because nothing equals NULL. See **Derivation
versions** below — this is the invariant that keeps derived data from silently
rotting behind a matching hash. `post_search` returns 404 immediately when the
SQLite candidate set is empty, without calling Qdrant (avoids a 503 from a missing
collection).

**Internal versions are all one notation: `MAJOR.MINOR`, as a string.** MINOR moves
when the *way* something is produced changes, MAJOR when its *shape* does — old data
unreadable rather than merely recomputable. Every one of them is compared by plain
equality and never ordered, so both halves trigger the identical rebuild: the split
informs whoever reads the release notes, and claiming more for it would be a
distinction the code does not make. The set is `CHUNKS_DERIVATION_VERSION`,
`SYMBOLS_DERIVATION_VERSION` and `PROMPT_VERSION` (the first two `"1.0"`; the
prompt one moves with the instructions, currently `"1.3"`). Two
neighbours deliberately stay outside it: `COLLECTION_SCHEMA_VERSION` (`"v1"`) is a
Qdrant collection-*name* component, where a dot buys nothing, and the migration
version is the `i32` SQLite stores in `PRAGMA user_version`.

`COLLECTION_SCHEMA_VERSION` is also the one version with **no mismatch detection and
no self-healing**. Bump it and the new name simply names no collection:
`ensure_collection` makes an empty one, SQLite still reports every file `indexed`
(the prepare-phase skip never looks at the collection layout), and search returns
nothing with no error anywhere. A bump there means reindexing every project, by hand.

**Derivation versions** (two nullable columns on `project_files`). Two consts
describe *what produced* a file's derived rows, and both are stamped by the same
prepare-tx upsert that moves the file to `indexing` — the transaction that then
writes the chunks and symbols they describe, so a row cannot claim a version whose
rows were never produced:

- `CHUNKS_DERIVATION_VERSION` (`slicing/traits.rs`) — the AST walk, node selection,
  left-extension rule, tokenizer. **Bump when a change would give different chunk
  boundaries for the same source text.** Expensive: every affected file is
  re-sliced, re-embedded on the GPU, re-upserted to Qdrant. The `[slicer]` token
  window is deliberately *not* covered — it is config, and retuning it is the
  operator's call.
- `SYMBOLS_DERIVATION_VERSION` (`slicing/symbols.rs`) — `queries_for`, the vendored
  `.scm` files, the extraction walk, the grammar crates the queries compile
  against. **Bump on any new/edited/vendored tags query, an `ALL` variant gaining
  or losing one, a change to `SymbolExtractor`, or a `tree-sitter-<lang>` bump that
  alters tags output.** Cheap by comparison (pure CPU) — which is exactly why it is
  a separate version: one shared const would price every tags fix at a full
  reindex, and you would stop bumping it.

Bumping is the *whole* action: the next ordinary `mindex-index` run rebuilds the
affected files by itself. No manual reindex, no remembering which projects are
behind, no `--force`. After a `SYMBOLS_DERIVATION_VERSION` bump, use
`mindex-index --symbols-only` (body flag `symbols_only`): it replaces symbol rows
in one transaction per file with no slicing, no embed pass and no Qdrant contact —
measured ~20× faster than the full path on this repo (0.3 s vs 6.5 s). It skips
files whose hash no longer matches, since their chunks are stale too and symbols
must parallel the chunk set; run an ordinary pass for those. Not bumping is the failure mode that motivated this — the
symbols feature shipped, unchanged files were hash-skipped forever, and `/symbols`
answered "no such symbol" (which its contract calls *definitive*) for a third of
the tree until it was found by accident. **A version bump is not optional
politeness; it is how the change reaches existing data.** Caveat the consts cannot
see: a grammar-crate bump in `Cargo.lock` changes tags output with the const
untouched — bump it by hand, or derive the version from a hash of the inputs.

**FK is RESTRICT.** `project_file_chunks → project_files` is `ON DELETE RESTRICT`.
Never delete a parent row while chunks exist; mark chunks deleted, let GC clean up.

**Management endpoints** (`handlers.rs`, routed in `http3::run`, *not* under `/v0`).
Full behavior is in the handlers + OpenAPI; the non-obvious parts:

- `DELETE /projects/{guid}`: immediate hard delete — rows first, collection dropped
  **last** so a retry re-attempts it; idempotent 204.
- `DELETE /projects/{guid}/files`: **soft** delete; `include`/`exclude` selector in
  the **request body** (globs don't fit the path); empty selector = 400 (can't wipe
  a project); 204 if none matched, else 200+count.
- `POST /cancel`: same body selector + empty-400, but matches **only
  `status='indexing'`** (a too-late cancel is a no-op); marks chunks `deleted`,
  `indexing → cancelled`. Deliberately takes **no** `IndexClaim` (so it can interrupt
  a held one); correctness against a live `/index` rests on **two re-reads**, not a
  lock: `post_index` runs `drop_cancelled` between Phase 1 and 2 (re-reads status,
  drops now-`cancelled` files before the embed — also closes the prepare race), and
  the retry worker re-checks status *after* acquiring the claim (else
  `cancelled → indexing`, a legal move, would resurrect it). A cancel landing
  mid-embed lets the pass finish; `mark_indexed` then matches **0 rows** — its
  `UPDATE` carries `AND status = 'indexing'`, so the illegal `cancelled → indexed`
  is never attempted and no trigger fires. The row count is discarded, so the file
  stays `cancelled`, the request succeeds for the rest of the batch, and GC
  reclaims the orphaned vectors. The status-machine trigger is the backstop here, not the
  mechanism; `mark_indexed` is also the one status write that issues its own SQL
  instead of going through `set_file_status`, so it neither WARNs on a miss nor is
  covered by that convention.
- `POST /retry`: requeues `failed` files; **empty body = all failed** (retry is
  non-destructive). **Metadata-only** write — `retry_count = 0`, status stays
  `failed` (skips the triggers, takes no claim) — and deliberately leaves
  `status_updated_at` untouched so the retry worker's failed-branch cooldown
  (`status_updated_at < now-60`) fires on the next sweep, not after a fresh grace.
- `POST /drift`: **read-only** working-tree comparison. Posted `path → sha256`
  manifest (capped by `[limits].max_drift_files`) classified against SQLite into
  `stale` (hash differs), `missing` (not indexed — `failed` counts as missing),
  `orphaned` (indexed but absent from manifest), `indexing` (in-flight, deliberately
  excluded from `stale`/`missing` since its stored hash is the *incoming* value).
  Unknown project ≠ 404 — every posted file is simply `missing`. Backs
  `mindex-index --check`, the MCP `drift` tool, the watcher's periodic sweep.
- **Stored research** (`GET /projects/{guid}/research[/{run_id}]`,
  `POST …/{run_id}/pin`, `DELETE …/{run_id}`): the browse half of the corpus, on the
  management plane because it reads server state the way `/files` does — the run that
  *produces* a report stays at `POST /v0/{guid}/research`. The list is keyset by
  `seq`, searches `question` **and** `report` with `like_escape` (FTS5 is the next
  rung of the documented ladder and nothing has measured `LIKE` insufficient over a
  corpus two orders of magnitude smaller than `project_file_chunks`), and never
  selects the report body — that is what makes it a separate endpoint from the detail
  one. `pin` is the one mutation on an otherwise append-only row.
- The read-only set (`GET /projects[/{guid}][/files]`, `/status`, `/config`,
  `/health`, `/version`) + `POST /gc` are self-describing in OpenAPI. `GET /config`
  serves the canonical language list of what the server *supports* (read by the
  search frontend); `/files?status=failed` is the dead-letter view. `GET /config` is
  otherwise static **except `research.models`** — see the model catalog under
  **/research**, and don't cache the response once.
- `GET /projects/{guid}` is the per-project **inventory**, and its per-language
  *file* count is the load-bearing half rather than a companion to the chunk counts.
  Keyed on chunks alone (the shape before `LanguageStats`) a language whose every
  file is `failed`, or whose files all sliced to zero chunks, has no chunk rows and
  so was absent from the map entirely — identical to a language the project does not
  contain. Those are different answers: the first means "indexed, and a search will
  still find nothing". That distinction is the whole reason the endpoint exists for a
  client rather than for a human, and it is what lets the VS Code pickers offer only
  `chunks_active > 0`.

## /research (SSE, Ollama-driven)

`POST /v0/{guid}/research` — long-lived one-way SSE: a local Ollama model
(`[research]` config, TOML-only) loops search/symbols **via internal cores**
(`search_core`/`symbols_core` in `handlers.rs` — the handlers are thin wrappers
around the same functions; never HTTP-to-self), then streams a Markdown report.
Non-obvious invariants:

- **Cancellation = disconnect.** No cancel endpoint. `ResearchEventStream`'s
  `Drop` cancels the job token (axum drops the SSE body on client disconnect);
  a closed mpsc channel is the same signal from the other side. The semaphore
  permit rides **in the spawned job**, not in the stream: the job is detached, so a
  permit released when a disconnected client's stream dropped would over-admit past
  `max_concurrent` while the old job still spent GPU and DB time — which matters now
  that a run may be granted an hour.
- **Dedicated runtime.** Jobs run on a small separate multi-thread runtime
  (`[research].worker_threads`, leaked in `main.rs` — dropping a runtime from
  async context panics). Admission via `Arc<Semaphore>`
  (`[research].max_concurrent`) → 429 `research.busy` up front.
- **Two seams** keep the loop testable without Ollama/Qdrant/embedder:
  `OllamaModel` (`models/ollama.rs`, streamed NDJSON chat, thinking+content
  deltas) and `ResearchTools` (`research.rs`; prod impl = the cores). Mocks:
  `tests/mock_ollama` (scripted), fakes in `research.rs` tests. The mock emits
  **native** `tool_calls` and recognises the report turn by the *absence* of
  `tools`, mirroring production; `POST /script` (`{"actions": [...]}`, one per tool
  turn) drives any sequence, and the `force_text_calls` knob makes it write a call
  as text so the `research.model_lacks_tools` path is covered. It finds its place in
  the script by counting `role: "tool"` messages.
- **Model protocol = Ollama's native tool calling.** The twelve tools
  (search/grep/symbols/outline/callers/list_files/read_chunks/file_history/
  list_research/read_research/note/revise_plan, plus `finalize`)
  are passed as `tools` JSON Schemas
  (`tool_specs`) and arrive back in `message.tool_calls` — a field distinct from
  `content` and `thinking`, which is the whole point: a model cannot put its
  decision in the wrong channel. A call becomes an `Action` by injecting the
  function name into its arguments object (`Action` is `#[serde(tag = "action")]`),
  so there is one deserializer and one set of error messages.
- **There is no text fallback, deliberately.** A model whose Ollama template lacks
  tool support writes the call as prose; the loop *detects* that
  (`looks_like_tool_call_attempt`) and fails with `research.model_lacks_tools`,
  naming the model. Parsing it instead was measured as a bad trade: it bought a
  second protocol (own prompt paragraph, own shapes, own loop branch) for the
  worst-scoring model in the bake-off, whose template also mangled the tool name
  (`"search Semantic code search over…"`) — so accommodating it meant guessing,
  turning a model error into a successful call with unvalidated arguments. A
  `tools` capability in `ollama show` is not proof; the template is what matters.
- **A turn may ask for several tools**, and all of them execute, each as its own
  `step`. Hard invariant: **every call gets exactly one `role: "tool"` reply**, in
  order — including calls rejected as duplicates or skipped for budget. An
  assistant turn announcing N calls followed by fewer results is a malformed
  transcript; the `NativeOllama` test fake asserts the pairing from the model's
  side every turn.
- **Prose with no tool call means "done"** (`Finalized`) — that is what answering
  is, in a tool-calling loop. `Unparseable` is now only an empty reply or a call to
  a tool that does not exist. Duplicate calls are rejected, not re-executed, and
  for `search` "duplicate" is **near**-duplicate: queries are normalized (trim,
  lowercase, collapsed whitespace) and compared by token-set Jaccard
  (`NEAR_DUPLICATE_JACCARD` 0.5, ≥ `NEAR_DUPLICATE_MIN_TOKENS` tokens). That
  threshold also rejects a mild *refinement*, deliberately: refining a query
  instead of learning a name is the trapped loop §6.1 measured, and the rejection
  reply names the earlier query so the model is told what it already asked. Only
  *executed* searches enter `seen_queries`, so a rejection cannot poison the set.
- **`read_chunks(path, start_line, end_line)` reads the index, never the file.**
  Pure SQL over `project_file_chunks` (`status='active'`, span overlap,
  `READ_CHUNKS_LIMIT` 8) — no filesystem access, no new table, nothing persisted.
  It exists because the model was observed *searching* for a line range `symbols`
  had just handed it. Like `outline` it must report a gap honestly ("the file IS
  indexed; lines 53-60 have no chunk") — a silent empty answer reads as "the file
  is empty there", which is the failure `outline`'s `indexed` flag already guards.
- **`path_prefix` on `search` is a post-filter, not an extra `include`.** The run
  requests `top_k * PREFIX_OVERFETCH` with its *unchanged* `include`/`exclude`,
  drops non-matching paths, then truncates. Appending the prefix to `include`
  would be widening — `include` is a union — so a scoped run could search its way
  out of its own scope. Correct by construction, at the cost of a wider Qdrant
  query.
- **Sampling is configurable and `PROMPT_VERSION` is stamped on every run.**
  `[research].temperature/top_p/seed` are `Option` (absent = the model's own
  Modelfile default, which is the right *production* behaviour and the wrong
  setting for comparing models: those defaults differ per model). A request's
  `seed` overrides the config's — that is the axis a harness varies for
  repetitions. `PROMPT_VERSION` (`research.rs`, next to `system_prompt`) rides on
  `done` and into the run journal; **bump it on any edit to `system_prompt`,
  `PLAN_REQUEST`, `SUFFICIENCY_REQUEST`, `REVALIDATION_SYSTEM_PROMPT`,
  `format_citation_complaint`, `REPORT_SYSTEM_PROMPT`, either report turn's user
  message, the budget nudges or `tool_specs`** — two reports written under
  different instructions are not comparable, and nothing else on the stream says
  which was in force. The run-state note is in scope for its *labels* only; its
  contents are the run's own history and differ every run by design.
- **Citations are provenance-checked server-side** (`parse_citations` →
  `Evidence` → `CitationReport`, emitted as the `citations` event between
  `summary` and `done`). Every `path:start-end` in the report is bucketed against
  what the run's own tools returned: `verified` (path shown **and** the range
  overlaps a shown span), `path_only`, `unverified` (a path no tool returned).
  Range *existence* is deliberately not checked — the schema holds no line counts,
  so overlap-against-shown is the honest 90%. The parser requires a file extension
  **and** a relative path (no leading `/`, no `//`) so `http://host:8080-8090` is
  not a citation. This ships **with** its consumers by rule: scout's reader
  silently drops unknown events, and its `_INSTRUCTIONS` used to say "do not
  spot-check the report" — a lie detector whose listener is told to ignore it is
  worse than none, so that block now points at `citations.unverified_paths` as the
  one thing worth checking.
- **A failed citation check sends the report back; it is no longer only a
  `warn!`.** The report turn writes a **draft** with the content gate closed
  (`stream_content: false`), so nothing reaches the client before
  `check_citations` has run. Clean draft (the common case) → shipped as-is in one
  `summary` event, nothing generated twice. Otherwise the offending locations are
  named back to the model (`format_citation_complaint` — the *locations*, not the
  counts: a model told "3 are unverified" rewrites the ones that were right).
  That list goes out on **both** paths; what is conditional is the remedy.
  `REVALIDATION_SYSTEM_PROMPT` swaps in and the tools re-open for
  `MAX_REVALIDATION_STEPS` (4) executed calls over `MAX_REVALIDATION_TURNS` (3)
  turns **only when `reason == Finalized`**: a run stopped by a budget has
  nothing left to spend, so its complaint closes by telling it to correct or drop
  the claim rather than to go and look it up. Then a rewrite turn streams the real
  `summary`. Revalidation steps
  emit `Step` events numbered on from `steps` but do **not** increment it — the
  budget-facing count must stay inside what was granted. A rewrite that fails
  ships the draft (a mis-cited report still beats none, and its `citations` say
  which parts to distrust). The draft's counts ride on the `citations` event as
  `draft_unverified`/`draft_path_only`/`draft_stale`/`revalidation_steps`, null when
  no repair happened: without them a repaired report and a clean one are the same
  event, and "does this pass pay for itself?" is unanswerable from the corpus.
- **A report that cites *nothing* is the third defect in that gate, and it was the
  one the gate could not see.** `citations: {total: 0}` is byte-for-byte what a
  clean report emits, so an ungrounded one shipped looking perfect — in exactly the
  place scout tells the caller to trust the report and check `unverified_paths`,
  which is empty here. Measured 2026-07-30: 5 of 24 runs shipped one, and only *one*
  of the five wrote a form (`(lines N-M)`) that a wider `parse_citations` could have
  caught — the other four named real paths with no ranges at all. So the fix is the
  missing route into the gate, **not** the parser: widening it buys 1 case in 5 and
  a third citation format. Two exemptions keep the gate from demanding a
  fabrication, and both are load-bearing: a run **no tool showed a single file**
  (`evidence.paths()` empty) cannot cite anything, and its "the answer is not
  reachable from this scope" report is the measured *correct* outcome; and a report
  under `MIN_GROUNDED_REPORT_CHARS` (800, sized from that corpus) is the short
  honest version of the same answer. `format_citation_complaint` dispatches to
  `format_ungrounded_complaint` rather than printing its three buckets empty —
  different content, because there is no failing location to name: what the model
  needs is the *form* (`path:START-END`) and the list of files it may cite, which is
  usually a formatting fix over spans the transcript already holds. No wire field
  was added: an ungrounded draft is `revalidation` present with all three draft
  counts at **zero**, since a report with no parseable citations has no failing ones
  either — the existing shape already says it, and a fourth count would cost the
  four-places SSE contract.
- **Indexing is never blocked by research; the run reports what moved instead.**
  Nothing serializes the two — research takes no `IndexClaim` and `post_index`
  never looks at `research_semaphore` — so a run lasting up to `max_seconds` reads
  an index that `mindex-index`/`mindex-watch` keep writing. Deliberate: the writer
  is an *external* process, so mutual exclusion could only be a 409/429, and the
  change it would refuse is the one the user just made to the file they are reading
  about. Two things make that safe to leave open. Per-file **consistency already
  holds**: the prepare tx marks a file's old chunks `deleted`, inserts the new ones
  and replaces its symbols in one transaction, so a reader never sees half a file —
  only an older whole one. And **currency is reported**: `Evidence` keeps a
  `baseline_sha` per shown path, `probe_freshness` (via `ResearchTools::
  file_versions` → `file_versions_core`, one chunked indexed SELECT, no HTTP, no
  step, no budget axis) re-reads them before every turn and once more before the
  report turn, and the run-state note names what CHANGED, what LEFT the index, and
  what is being reindexed *right now*. `changed`/`removed` are sticky (the
  transcript already holds notes taken from the intermediate version, and the
  transcript is the run's only memory); `in_flight` is not, because it drives a
  statement about what `search` can reach at this moment. That third section exists
  for a window nothing else reports: between `post_index` phase 1 and phase 2 a
  chunk is in the `has_id` candidate set with no vector in Qdrant yet, so `search`
  silently under-retrieves it while `read_chunks`/`outline`/`symbols` (pure SQL)
  still work. Staleness is **orthogonal to provenance** and buckets separately
  (`citations.stale`/`stale_paths`): a citation can be impeccably `verified` and
  stale, and it joins the two provenance defects in the revalidation gate because
  the remedy is the same. `apply_versions` takes the *asked* path list, not just the
  results — the query is chunked, and inferring removal from a path nobody asked
  about would invent staleness; a probe that fails leaves the previous verdicts
  standing, since "I could not check" is not "this changed". A snapshot (`as_of`)
  read was rejected: it would need a chunk-deletion side table written in the hot
  path, an `as_of` parameter through five shared cores and a GC low-watermark lease,
  would still be *partial* (symbols and `project_files` hard-delete), and would buy
  internal consistency at the cost of currency — the wrong trade for code research,
  where the answer is wanted about the tree as it is now.
- **Every finished run is journalled** as one `research_runs` row: the question and
  report, the granted budget against what it cost, the citation verdict, how far the
  index moved underneath the run, and its tool usage (notes written and rejected,
  plan revisions, grep calls and hits, out-of-scope refusals and hidden rows, the
  run's scope, whether the report was server-written, and what the report window was
  granted against what it took). One row and one INSERT, because a run *is* a single
  flat measurement record — so `changed_files = 0` / `notes_written = 0` are
  measurements by construction, with no cross-table invariant to hold.
  Per-tool call counts are deliberately *not* journalled —
  `research_tool_calls{tool}` has them; what is journalled is what the metric cannot
  express. All go through the
  `ResearchJournal` seam (`db/research.rs`; prod impl `SqliteResearchJournal`).
  Best-effort: an insert failure is a `warn!`, never a failed run. **No FK to
  `project_files`** — a run is not a file and must never surface in `/drift`.
  Unset sampling stores NULL, not 0. This is what makes a bake-off re-analysable
  later without the harness's CSVs. `NoJournal` is `#[cfg(test)]`-gated on
  purpose: production is never offered a trace-less journal.
- **A stored run is reusable as context, and staleness is per-path — not a global
  counter.** `context_run_ids` on a request names earlier runs of the *same
  project*; their reports are injected as one `user` message before the plan turn
  (`format_prior_reports`), so the plan can use them. A global monotonic
  `project_version` was the obvious alternative and was **rejected**: with
  `mindex-watch` running, one save of any file would mark every stored run stale at
  once, and the feature would be correct and useless. Instead each run's own
  baselines are persisted — `Evidence.baseline_sha` per shown path, into
  `research_run_files` — and staleness is the same `changed || removed` comparison
  `apply_versions` makes during a live run, asked later against `project_files`.
  Three things about it are easy to break. The join needs **`model_id`**, which
  `research_runs` does not store (bind it from `RouterState`, as `file_versions_core`
  does): `project_files` is keyed `(project_guid, model_id, path)`, so joining on the
  path alone matches across embedding models. `research_run_files.path` carries **no
  FK** — `RESTRICT` would make `prune_deleted_files` refuse to drop any file a past
  run ever read, silently turning research into a brake on the GC of the code
  channel, and `CASCADE` would erase the baseline and make a run whose file is *gone*
  read as fresh. And the freshness and validity filters on the list must be applied
  **inside** the cursor-bounded subquery, before `LIMIT`, or a short page stops
  meaning "there is no more".
- **Validity is the transitive verdict, and it is derived — never stored.**
  `context_run_ids_json` is the edge set of a knowledge graph (edge A → B = "B was
  in A's context at launch"), and `research_validity_ctes` (`handlers.rs`) computes
  `valid = own files unmoved AND every context parent exists AND is itself valid`
  as one recursive CTE over `json_each` at read time. A stored flag was rejected:
  staleness can *heal* (a file reindexed back to the same bytes), and its onset is
  an ordinary indexing write with no research-side event to hook a cascade on.
  Deletion needs no cascade either — hard `DELETE` (and the GC retention sweep,
  which is the same event here) leaves a dangling id in every child's edge list,
  and the CTE reads a dangling reference as invalid, transitively and immediately,
  with no write anywhere. Cycles are impossible by construction (context ids are
  validated at launch and the run's own row does not exist yet, so edges point
  strictly backwards), and the recursive `UNION` deduplicates besides. `freshness`
  keeps its self-staleness meaning; `valid=true|false` is the orthogonal filter,
  and each summary carries `valid`/`invalid_reason`
  (`stale`/`context_deleted`/`context_invalid`) plus `context` — the flat
  transitive ancestry, each ancestor with its own state — so a human picks context
  from what the graph still vouches for. A request naming an invalid run in
  `context_run_ids` is refused up front (400 `validation.research_context_invalid`,
  offenders and reasons in `meta.runs`): the client showed `valid` before the pick,
  so the refusal only fires when the index moved in between.
- **The model can browse the stored corpus itself** — `list_research` (seq, title,
  question of *valid* runs only, minus the ones already injected as context, capped
  at the `LIST_RESEARCH_LIMIT` const) and `read_research(seq)` (one valid report,
  truncated out loud at `max_context_chars`). Both are deliberately **unscoped** —
  reports are not files, and `ToolScope` does not apply (the tool descriptions and
  the `system_prompt` paragraph say so) — and both return `shown: Vec::new()`
  unconditionally: the hearsay invariant below covers them
  (`read_research_never_seeds_the_evidence`). An invalid or missing seq is an
  explicit refusal, not an empty answer — the `outline.indexed` lesson. Self-scan
  needs no check: the live run has no `research_runs` row yet.
- **Prior reports are hearsay, and nothing in them may be cited.** They are the
  fastest way to learn the real names — the measured bottleneck a cold run spends its
  first steps on — and they are not evidence. Their paths are **never** seeded into
  `Evidence`, so a `path:start-end` copied out of one lands `unverified` and trips the
  revalidation gate exactly as an invented one would; seeding it would promote hearsay
  to verified provenance and destroy the one guarantee scout's "trust the report"
  instruction rests on (`a_prior_report_never_seeds_the_evidence`). The
  `system_prompt` paragraph that says so is conditional, like `scope_rule`, and ships
  with the corpus half or not at all — the markdown lesson again. Each section states
  its own staleness in words, because a report written against files that have since
  moved is still useful for names and actively misleading about specifics, and only
  the header says which. Over-cap reports are truncated **with a marker**
  (`[research].max_context_chars`), never silently: an injected block is prompt tokens
  on *every* turn, so that cap is a budget axis and not politeness.
- **`title` is the report's own heading, `seq` is an ordinal, `id` is identity.**
  `extract_report_title` stores the report's first ATX heading at journalling time —
  NULL when there is none or it trivially repeats the question — and the wire `title`
  falls back to the question-derived truncation (`research_title`, still derived at
  read time for the reason it always was: a stored copy of a *truncation* goes stale
  the day the rule changes; a stored copy of the model's own output cannot). The
  list's `q` searches title, question **and** report. `seq` is
  per-project, monotonic, and doubles as the keyset cursor — never `OFFSET`, over a
  table GC prunes and every run appends to. It is **not** identity: a total wipe of a
  project's runs restarts it at 1, so every mutating endpoint keys on the uuid `id`.
- **A structurally broken report is sent back, and if it stays broken it is never
  journalled.** `validate_report_markdown` is four honest shape checks (empty, JSON
  start, no leading `# heading`, unclosed fence) — tree-sitter-md accepts anything,
  so parsing would be a validator that cannot fail. A failing draft joins the
  citation gate's complaint (`format_markdown_complaint`, appended to the citation
  complaint when both fire); a markdown-only defect re-opens **no** tools — nothing
  needs looking up, only rewriting. If the final text still fails, it is streamed
  (a watched broken report beats a vanished one) but `journal.record` is skipped and
  `done` carries null `run_id`/`seq` — the existing failed-journal wire shape, no new
  field. `forced_synthesis` is exempt by flag and valid by construction
  (`forced_synthesis_passes_the_markdown_gate`); the skipped run is also invisible
  to `MeteredJournal`'s counters, accepted with a `warn!` as the trace.
- **`expires_at IS NULL` means pinned**, and that is the whole retention mechanism.
  The deadline is stamped at insert from `[research].retention_days`, so changing that
  setting moves future runs only and `prune_expired_research` takes no retention
  argument — comparing against the *current* config would make pinning inexpressible
  and silently re-date the corpus on every edit. Unpinning restores
  `created_at + retention`, which means unpinning a run older than the window makes it
  eligible at the very next sweep; stamping `now + retention` instead would turn a
  checkbox toggled twice into a silent renewal of everything it touched.
- **`effort` selects a budget; the request may override it** (`[research.effort.
  {low,medium,high}]` → `EffortBudget` → `research::Budget` via `Budget::resolve`,
  which applies `ResearchRequest.budget` axis by axis — an absent axis keeps the
  preset). The run stops at whichever axis is reached first and `done.reason` says
  which. The levels are **config keys, not `match` arms on `Effort`** — per the
  tuning-knob rule above, since the right numbers are hardware- and
  model-dependent. **Four axes with different jobs**, and the ordering is
  measured, not intuited:
  - **`max_seconds` is the budget, and it is a HARD deadline** (300/900/3600). It is
    what the caller waits and what holds a `max_concurrent` slot. Polling it between
    turns is *not* enough and that was a real bug: one `chat_stream` can retry
    internally, so a single turn could take `6 × turn_timeout_ms`, and every phase
    after the tool loop (sufficiency, draft with its empty-retries, revalidation,
    rewrite — ~18 turns) ran with no time check at all, for a measured overrun of
    order 1.5 h holding a slot. It is now *also* enforced by cancellation: a
    `DeadlineToken` child of the job token fires at `started + max_seconds`, which
    reaches `chat_stream`'s two `select!`s (dropping the reqwest body is what makes
    Ollama abort generation) and every `*_core`'s child token. **Both mechanisms
    stay**: the poll is the graceful stop that leaves a well-formed transcript with
    an explicit "proceed to the report" turn, the token is the backstop for a turn
    that never returns. A deadline stop is told apart from a client disconnect by
    `stopped_by` — the job token is tested *first*, since a disconnect cancels the
    whole tree — and it is not a failure: the run keeps what it found and reports.
    Two traps: a deadline firing mid-batch must still answer every announced call
    (the pairing invariant) before breaking, and it must not charge a step for a
    lookup that returned nothing.
  - **The report phase has a window of its own** (`[research].report_timeout_ms`,
    default 120 s), so `max_seconds + report_timeout_ms` is the true worst case. Its
    token is a child of the **job** token, never of the budget one — parented to the
    deadline that just fired it would be dead before it opened, and every long run
    would end in the server-written notice. A run stopped by its deadline still gets
    to synthesise: that is what taking the report out of `max_seconds` buys. The
    window bounds the empty-report retries, the revalidation loop and the rewrite;
    if it expires with a draft in hand the draft ships, and if it expires with
    nothing, `forced_synthesis` writes an honest account of the run (question, plan,
    notes, the paths it was shown) rather than closing a 200 stream with no
    `summary`. Salvaging the model's half-written draft was rejected: `chat_stream`
    discards accumulated content on cancel, and a report truncated mid-sentence reads
    as authoritative in a way the server's notice does not.
  - **A truncated run says so in its own report.** `report_request` prepends a
    paragraph naming the limit that stopped it and the plan's open sub-questions;
    `done.reason` is a wire field, and the report is what gets pasted and quoted
    months later. The sufficiency turn is *skipped* on a truncated run — both of its
    outcomes are pointless there, and it used to run unbudgeted.
  - **`turn_timeout_ms` must sit ABOVE every budget, and startup enforces it.**
    Measured twice in one afternoon from the same wrong intuition: tightened to 120 s,
    glm's cold opening turn (model load + a ~98k-token KV allocation) crossed it and
    the run died at step 0; raised to 600 s, a turn where glm looped in its *thinking*
    channel crossed that and the run died at step 0 again — with `max_seconds` at 900,
    so the deadline would have cancelled the turn 300 s later and shipped a report.
    A turn timeout is a `reqwest` error, so it fails the **whole run** with
    `ollama.unavailable`; the deadline cancels the same turn and still answers. A model
    that never stops generating is *precisely* what the hard deadline is for, so a
    transport timeout firing first inverts the design. `validate` therefore refuses
    `turn_timeout_ms <= max_request_seconds`. It is a dead-socket guard, not a bound.
  - **The runaway-thinking guard is what actually catches that model, and it counts
    volume rather than time** (`[research].max_turn_thinking_chars`, default 20000,
    `0` = off). The pathology is a turn that never leaves the thinking channel: the
    socket is healthy, deltas arrive steadily for the whole run, and the *only* thing
    that stops it is the deadline — by which point the budget is spent on nothing.
    **There are two pathologies here, and separating them is what fixed the number.**
    The per-run totals that read like single runaway turns (32197 / 37148 / 37826)
    are *run* totals spanning two turns, and splitting them inverted the first
    conclusion: a wedged **investigation** turn produces ~16550 characters over the
    entire 900 s deadline (~18 chars/s), while a wedged **report** turn runs at
    ~310 chars/s. An initial 20000 — chosen to sit above the largest whole-run total
    observed (29067 over 11 turns) and so provably clear of every healthy turn —
    caught only the fast one, verified live: safe and useless. The default is now
    **8192**, which drops the slow wedge at ~445 s and leaves ~455 s of the deadline,
    enough to still investigate given that glm's healthy runs finish in ~180 s. The
    price is a 3.1× margin over the busiest healthy turn measured (2642 characters)
    rather than an order of magnitude, and it is a margin over *averages* — per-turn
    maxima are not recorded, so a false positive is possible and the `warn!` names the
    model and the count so it is visible when it happens. A volume bound stays
    structurally late against a slow generator: the two populations differ ~6× in
    volume but ~50× in *duration* (a healthy turn is ~10 s), so **the instrument that
    catches the slow wedge early is a clock armed on the first thinking delta** —
    immune to the cold-start trap that killed both previous per-turn timers, because a
    loading model emits no deltas and so never arms it. That is a later change, not
    this one. Not per-model and not request-overridable: the healthy
    populations of the two measured models differ by less than the margin above
    either of them, and like `context_fraction` nothing good lies on either side of the
    default — raising it buys a longer wedge, lowering it buys abandoned healthy
    turns, and the caller holds no information the server lacks. An abandoned turn is
    returned as an **empty** `ChatOutcome`, deliberately: every phase already recovers
    from one (plan → plan-less run, tool loop → bounded parse retry, sufficiency →
    drop the question, report → re-ask at a shifted seed), and inventing a sixth path
    would mean inventing five that exist. It is therefore invisible in the return
    value, which is why it is instrumented in place —
    `research_runaway_thinking_turns` plus a `warn!` — and why
    `TokenTally::record` must not let its zero `num_ctx` overwrite a known window.
    Its GPU cost is unavoidably invisible to `max_tokens`: Ollama's `done` line never
    arrives for a cancelled turn, so the counts come back `None` and the turn lands in
    `turns_unreported` having really made the GPU work.
  - **`max_tokens` is the *cost*** (400k/1.2M/6M): `prompt_eval + eval` summed
    over turns, which is what the run actually makes the GPU do. It is the axis
    `max_steps` was pretending to be — the whole transcript is resent every turn,
    so cost grows super-linearly with turns while steps count linearly and mean
    nothing. Sized from measurement (a medium run of 8 steps = 52149 prompt +
    3431 eval), so on this hardware time normally binds first and this catches the
    pathological long-transcript run.
  - **`context_fraction`** (0.5/0.7/0.85) is a *guard*, not a budget: measured
    at 16 steps / 20 turns the peak prompt was ~12k of 65536 (18%), so filling a
    64k window would take ~85 steps. It exists for small-window models, where
    Ollama trims the transcript in silence. Checked against
    `tally.peak_prompt_tokens` *before* the next turn — one turn short of the
    window, never after a trim. **The one axis a request cannot override**:
    raising it buys truncation, nothing else.
  - **`max_steps`** (8/20/64) is the coarse backstop. A step is a poor unit and
    that is why it is not the budget: `outline` is one indexed SELECT while
    `search` is a GPU embed plus a vector query, one turn may ask for several, and
    the same run measured 20 turns against 16 executed steps — four turns produced
    no step at all (rejected duplicates, unknown tools).

  A fifth key, **`search_top_k`**, rides in the same section but is not a budget
  axis: it is the evidence width of one `search` call, 5 at every level on
  purpose (the runs that missed an answer were already getting five hits and lost
  on query *formulation*, so raising it buys transcript, not coverage). It is a
  knob so a harness can sweep it. The trap it carries is that research builds a
  `SearchRequest` directly and `search_core` leaves validation to its callers —
  so config validation refuses `search_top_k > [search].max_top_k` at **startup**,
  where the edge validator would never see it.

  Each stop has a loop-level test (`the_time_budget_ends_a_run_that_still_has_
  steps_left`, `the_token_budget_ends_a_run_the_clock_and_the_step_cap_would_not`,
  `the_context_budget_ends_a_run_before_ollama_would_trim_it`). The time one uses
  **real** time in small increments: the budget is measured with
  `std::time::Instant`, which `tokio::test(start_paused)` does not move.
- **Per-request `budget` is capped by `[research].max_request_{seconds,tokens,
  steps}`** (TOML-only, like `[limits]`), checked at the edge by
  `validate::research_budget` → 400 `validation.research_budget_out_of_range` with
  `field` naming the axis. Config validation additionally rejects a ceiling below
  `[research.effort.high]`, which would make `effort = "high"` unreachable through
  `budget`. `GET /config` publishes the whole ladder plus the ceilings — that is
  what clients render, after three independent hardcoded copies of the numbers
  ("3/8/16", "5/16/32", 6/20/48 — now 8/20/64) had each drifted from the server.
- **`progress` makes a live run steerable**, and is the only reason budgets can be
  tuned from evidence rather than taste: `RunProgress` (steps/time/tokens/context
  spent against granted, plus `turns` and `binding` — the axis closest to
  exhaustion) is emitted **once before the first turn** (limits, nothing spent),
  then after every executed step and every completed turn. `done` carries the same
  struct plus `reason`, so the run's whole cost is on the stream and a measurement
  harness no longer has to reconstruct it from server logs it cannot see for
  traffic it did not initiate. **No ticker**: a timer task would race the
  cancellation token for a number the client can interpolate between events, and
  would make the loop's tests clock-dependent.
- **The identifier rule governs code; documentation inverts it, and shipping the
  exception is half the docs feature.** `*.md` files are indexed (language
  `markdown`), and they answer "why" questions about this repo about as well as the
  source does — measured, `docs only` scored 9/13 against the whole source tree's
  10/13, and adding the channel took documentation questions from 1/8 to 5/8 with
  nothing else regressing. But indexing them alone was measured to change **nothing**:
  the model never opened a document, because `system_prompt`'s loudest paragraph tells
  it that plain English fails and only identifiers work. That paragraph therefore
  carries its own exception (*documentation is written in English; ask it in
  English*), and the two must never ship apart — the corpus half is invisible without
  the prompt half. Any future channel of prose (git history is the named one) inherits
  this rule.
- **`outline`/`list_files` exist because search matches text and code is written
  in identifiers.** Measured on this repo: a natural-language query retrieves the
  *test* that describes a behaviour (score ~9), the identifier retrieves the
  implementation (~13) — so a model that doesn't yet know a name cannot ask the
  query that would work, and spends its budget rephrasing. The intended path is
  `list_files → outline → symbols/search/callers → read_chunks`, and the system prompt
  says so explicitly; that instruction is half the feature. Both are pure SQL over
  `project_files`/`project_file_symbols` (`outline_core`/`list_files_core`,
  covered by `idx_project_file_symbols_file`) — no embedder, no Qdrant, no HTTP
  handler of their own. `outline` reports `indexed` separately from an empty
  symbol list: a wrong path guess and a symbol-less file must read differently, or
  the model concludes the file is empty. `list_files`' glob is **SQLite `GLOB`** —
  the same operator `/search` and `/files` use, so no fifth glob dialect; note `*`
  crosses `/` there, unlike the `.mindex` contract. Errors after stream start are
  `error` *events* (HTTP is already 200); `NoMatch` is a tool result ("no results"),
  not a failure.
- **The run's scope is enforced on every tool, and `ToolScope` is why it cannot stop
  being.** For a while it was not: `include`/`exclude` reached only `search` and
  `list_files`, so a run scoped to `docs/**` could still read any file in the project
  by naming it — a scope in the documentation and not in the server. The four
  direct-read tools had nowhere to *put* one, which is the whole reason
  `ResearchTools` now takes a `ToolScope` (`research.rs`) as a required argument on
  every model-facing method: a tool added later cannot quietly be the next exception.
  Evaluated in **SQLite**, by `build_file_filter`, because `src/` has no glob matcher
  of its own (`globset` lives in `tools/mindexfile`) and an in-process check would be
  a fifth dialect. Appended as a `file_path IN (SELECT …)` subquery, **not** a join:
  `build_file_filter` emits unqualified column names, every one ambiguous against
  `project_file_chunks`/`project_file_symbols`, and teaching it to qualify them would
  touch `DELETE /files` and `POST /cancel` to tidy a research lookup. Two shapes of
  enforcement, and the difference matters. Path-keyed (`outline`, `read_chunks`):
  **explicit refusal**, a third read plus an `in_scope` flag mirroring `indexed` —
  a refusal that reads as an empty result tells the model the file is empty, the
  exact failure `indexed` already exists to prevent, and it also sends it hunting for
  spellings of a path it may simply not read. Name/text-keyed (`symbols`, `callers`,
  `grep`): rows are dropped **and counted** against one extra unscoped `COUNT(*)`,
  because "not here" and "not anywhere" are different answers and `/symbols` calls the
  second definitive. `callers`' `defined` probe stays unscoped for the same reason.
  Everything is gated on `is_scoped()`, so an unscoped run builds byte-for-byte the
  SQL it always did — that is what makes the public `/symbols` sharing these cores
  provably unaffected. `SymbolsRequest` gained optional `include`/`exclude` so
  research passes the scope through the *same* field the endpoint uses (and the MCP
  `symbols` tool gained it for free); its binds must be appended **last**, since
  `symbols_core` rewrites the role bind by Vec index. `file_versions` is deliberately
  *not* filtered: it only asks about paths already shown, and a file that leaves the
  scope must still be reported as changed rather than going quiet. The scope is also
  *told* to the model — a `system_prompt` paragraph and a `Scope:` line in the state
  note, both from the one `ToolScope::describe` — because a wall it has forgotten is
  a wall it spends calls rediscovering.
- **`note` is the run's only durable memory, and `grep` is what `search` cannot do.**
  `note(text)` and `revise_plan(plan)` mutate the run rather than the index, so they
  bypass `ResearchTools` entirely (`apply_local`); both are charged a step, because a
  decision the reader should see is worth a step and pricing it stops note-churn.
  Notes are pinned into the state note every turn *and* pushed as a message of their
  own before the report turn — where the state note is deliberately not rebuilt —
  since they are the conclusions the report is meant to be written from. Caps are
  announced, never silent (`MAX_NOTES` 24, deliberately double `STATE_NOTE_MAX_ITEMS`;
  `MAX_NOTE_CHARS` 500; at the cap the oldest is dropped *out loud*): a memory that
  forgets in silence is worse than one with a known size. `grep` is a `LIKE` over
  `project_file_chunks.code` (`grep_core`, core-only like the other four), case-
  insensitive, and **`like_escape` is mandatory rather than defensive** — `_` is a
  wildcard and this codebase's identifiers are full of it, so an unescaped
  `read_chunks` also matches `readXchunks`. It reports the matching line *and* the
  chunk span, because the chunk is what a citation can verify against. The cost is
  real and bounded, not hidden: a scan of the biggest column in the schema, narrowed
  by the scope subquery, stopped early by `GREP_LIMIT`, and refused below
  `GREP_MIN_PATTERN_CHARS`. FTS5 is the real answer and is deferred — a table plus an
  invalidation surface is a project, not a tool.
- **`callers` is deliberately an *approximate* call graph, and the imprecision is
  the feature.** `project_file_symbols` has **no target column**: a
  `role='reference'` row records that a token appeared in a call position, never
  which definition it binds to. But it does carry `parent_name` — the enclosing
  definition, assigned by pure byte-span containment — so "who calls X" is one
  indexed `SELECT` over data the symbol table has always held, grouped per (file, definition)
  because the raw rows are resent every later turn (`callers_core` +
  `build_callers_query`, pulled out to be testable like `build_symbols_query`).
  `direction: "out"` reads the same table the other way (`WHERE parent_name = ?`,
  hence `idx_project_file_symbols_parent`). The edges are exact only up to name collision, and an
  aliased import breaks them entirely — which is stated in the tool description
  **and repeated on every result**, because by the time a result is read the
  description is thousands of tokens back and a list of `path:line` pairs reads as
  resolved unless it says otherwise. An empty answer distinguishes "defined, never
  referenced" from "no such name" (hence the two reads), the same way `outline`'s
  `indexed` flag does; a top-level reference with no parent is reported as such,
  not dropped, or the totals would disagree with the list for no visible reason.
  Resolution (LSP/SCIP) was **considered and rejected** for the product case: a
  live language-server fleet cannot be plug-and-play at the lifecycle layer (every
  server signals readiness differently, and querying before ready returns *wrong
  empty answers*), it needs each project to build on the indexing host, and its
  degradation is per-language and invisible — quality would silently vary between
  users with nothing saying why. This tool exists to make the ambiguity
  **measurable** first. The property it rests on is language-agnostic but not
  free: a language's tags query must tag the enclosing definition with a span
  covering the call, which nothing guarantees, so
  `symbols_cross_language_tests.rs` pins it across five non-Rust languages and its
  allow-list forces a decision when a language with a tags query is added.
  Measured on first contact: the model calls it when the question is shaped as
  reach ("who uses this / what would each caller have to change") and reaches for
  `symbols` + repeated `read_chunks` when it is not — so the lever here is the
  prompt's wording, not the precision of the edges.
- **The loop terminates on counters, not on a clock** (regression guard). Every
  tool-loop iteration either breaks or increments exactly one of `steps`
  (≤ `max_steps`), `parse_retries` (≤ `MAX_PARSE_RETRIES`) or `duplicate_calls`
  (≤ `MAX_DUPLICATE_CALLS`). A rejected duplicate executes nothing, so it must
  *not* cost tool budget — which is why it needs its own cap: without one, a model
  repeating one call spins forever, since each turn gets a fresh `turn_timeout_ms`
  and there is no cancel endpoint (two such jobs wedge both `max_concurrent`
  slots). The counters remain the primary guarantee even though `max_seconds` is now
  a hard deadline, because a run that spins *inside* its budget should be reported as
  `repeated_calls` rather than as a timeout. Keep the invariant when adding a
  rejection path: a new `continue` needs a new bounded counter — or, better, price
  the refusal as a **step**, which is what every refusal added since does (note over
  the cap, grep pattern too short, out-of-scope path). A mistake the model can repeat
  for free is the hazard the caps exist for; one that costs a step needs no cap. The rule now also
  binds one level up: the tool loop sits inside `'phases`, whose `continue` (the
  sufficiency re-entry) is bounded by `reopens ≤ MAX_REOPENS`, and the
  revalidation loop by `MAX_REVALIDATION_STEPS`/`MAX_REVALIDATION_TURNS`.
- **A plan turn opens the run, and a sufficiency turn closes it** — both toolless
  (`NO_TOOLS`), both answering the same measured problem: **the thinking channel is
  discarded from the transcript.** `ChatMessage` has no `thinking` field and
  `ChatOutcome` never captures one (`chat_stream` forwards it straight to SSE), so
  a thinking model plans in the one channel that is erased every turn and then
  re-derives the plan from raw tool output — which is what "looping" looks like
  from outside. `PLAN_REQUEST` moves that thought into the channel that *is*
  replayed: the reply is pushed back as an **assistant** message. It degrades to a
  plan-less run rather than failing (a plan is an aid, not a contract). It is also
  the run's only sufficiency criterion — `SUFFICIENCY_REQUEST` then asks the model
  to mark each sub-question ANSWERED/UNANSWERED, which either re-opens the loop
  (only if the model *chose* to stop, an axis is still unspent, and
  `declares_unanswered` — a substring test on vocabulary the server dictated) or
  rides into the report so "the evidence was insufficient" is a list, not a
  formality. Measured on glm/gpt-oss/gemma4: 26 of glm's 36 medium runs were
  *stopped* by a cap rather than finishing, while gemma4 finalized at a median of
  4 steps with 34% coverage — the same missing criterion at both ends. Raising
  `max_steps` 20→48 was tried first and moved nothing (median depth 16→16,
  citations 60→32) — under `max_seconds: 240`, which bound first; see **What was
  measured (2026-07-30)**.
  The re-open nudge is also the **one** place `revise_plan` is offered by name, and
  that is deliberate: the tool went uncalled in 28 measured runs, and the likelier
  reason than "never needed" is that nothing asked for it where it fits — the plan is
  the run's only sufficiency criterion, so the turn that has just found the plan
  unfinished is exactly where "the plan asked the wrong question" becomes visible.
  Naming it there also makes the two explanations distinguishable on the next
  corpus. Removing the tool instead was considered and rejected: without it a run
  with a wrong plan is re-opened against the wrong plan, and the crowding argument
  for deleting it rests on a single run.
- **The run-state note is pinned, not appended** (`RunState` →
  `format_state_note`). One `user` message rebuilt from what the loop already
  tracks — executed queries, symbols, outlines, globs, ranges read, paths shown,
  the plan, the budget position — lifted out and re-pushed before every turn, so
  the model sees exactly one, adjacent to where it generates. Costs the model
  nothing to produce. It exists because the transcript is the run's only memory
  and by step 19 it is ~165k tokens of chunk bodies in which "I already asked
  that" is written nowhere. A `user` message on purpose: it is not something the
  model said, and attributing invented history to the assistant is worse than
  useless. Placed after the previous turn's `role: "tool"` replies, so the
  call/reply pairing is untouched.
- **`num_ctx` is the model's own limit, capped — not a configured target.**
  `OllamaHttpClient` asks `/api/show` once per model (cached; the key is found by
  `.context_length` suffix because it is namespaced per architecture) and requests
  `min(model_limit, [research].max_num_ctx_tokens)`. Asking a 32k model for 65k
  does not buy context — llama.cpp allocates it and the model degrades past its
  training length in silence, so one generous global setting becomes a per-model
  quality bug and makes any comparison *between* models invalid. The config key is
  therefore a **VRAM ceiling** (default 131072), not a window: `num_ctx` allocates
  KV up front, measured ~30 KiB/token at `OLLAMA_KV_CACHE_TYPE=q8_0` (~54 at f16),
  so a 262k-token model unguarded would ask for ~7.5 GiB. An unreachable
  `/api/show` degrades to the ceiling, never to zero.
- **The model catalog is what makes `GET /config` no longer static.**
  `worker::ollama_catalog` reads `/api/tags` every
  `[research].models_refresh_interval_seconds` (default 300) into an
  `Arc<RwLock<ModelCatalog>>` on `RouterState`, and `get_config` publishes it as
  `research.models`. It exists so a client can offer a **closed** model list instead
  of a free-text field whose typo comes back as `ollama.unavailable` mid-run — the
  same argument as publishing the effort ladder. Consequences that are easy to
  break: a **failed tick keeps the previous list** (blanking a picker because one
  probe timed out is worse than a list five minutes old), so `refreshed_at` is *not*
  re-stamped and is the only thing separating "Ollama has no models" from "Ollama was
  never reached" — both of which are an empty array; the worker is gated on
  **nothing**, since an Ollama that comes up an hour later must still be picked up;
  and nothing primes the snapshot before serving, because startup must never block
  on an optional dependency (`interval`'s first tick is immediate, and a `/config`
  inside that window is the designed degradation, not a bug). `health()` is now a
  *provided* method over `list_models` — one URL, one timeout
  (`health_timeout_ms`), so the liveness ping and the catalog read cannot drift
  apart; the `/api/tags` reader is `#[serde(default)]` throughout so a shape change
  degrades to an empty list rather than failing the optional `checks.ollama`. No
  metric was added: `dependency_up{dependency="ollama"}` already answers up/down, and
  a catalog gauge could not simply join `StateMetrics` (cleared-and-repopulated, and
  written by `worker/metrics.rs` alone).
- **A non-2xx from Ollama carries its reason in the body, and one class of 500 is
  retried in silence.** `chat_stream` reads the error body instead of
  `error_for_status` (which drops it — that is what made a measured gpt-oss defect
  read as a bare "500" for a whole bake-off). A 500 whose body contains
  `error parsing tool call` is resent **with the same transcript at the next seed**,
  up to `MAX_TOOL_CALL_PARSE_RETRIES`: `gpt-oss:20b` sometimes emits its analysis
  prose into the same harmony message as a call's JSON arguments, Ollama
  `json.Unmarshal`s the lot and fails the turn — 11 of its 36 bake-off runs died
  this way. The fault is in one sampled reply, not in the transcript, which is why
  only `sampling.seed` moves. A *fully* verbatim resend was tried first and is not
  enough: at a pinned seed the same reply comes back often enough that it rescued
  only 2 of 4 turns. The retry is equally deliberately **not** a nudge telling the
  model to emit only JSON — that would edit the transcript every later turn resends,
  bind the fix to `PROMPT_VERSION`, and coach a model that never misunderstood
  anything. It is safe *because* the 500 arrives
  before the stream opens — nothing has reached the client, so there is no half-reply
  to duplicate. Any other status, and any 500 for another reason, fails at once with
  Ollama's own words: a wrong model name is a real answer, not a flaky one.
- **Token accounting is the run's only trace.** `ChatOutcome`
  (`models/ollama.rs`) carries `prompt_eval_count`/`eval_count` from Ollama's
  `done` line; `TokenTally` folds them per run and `run_research` logs one record
  (steps, elapsed, turns, tokens). Counts are `Option` — a turn Ollama reports
  none for lands in `turns_unreported`, never as zero. The client WARNs when
  `prompt_tokens` reaches `num_ctx_tokens`: Ollama trims an over-long prompt and
  streams on silently, so that log line is the *only* symptom of a truncated
  transcript.
- **`Step` carries a typed `StepCall`**, not an action string plus a loose
  argument: the wire gives each action its own key (`query`/`name`/`path`/`glob`),
  and choosing it by matching on a string kept the same list in two places, with a
  silent `"query"` fallback when they drifted.
- SSE event contract lives in **four** places, all of which move together:
  `post_research`'s doc comment, its `#[utoipa::path]` 200 description, the VS
  Code client (`tools/vscode/src/api.ts` + `researchView.ts`) and scout's reader
  (`tools/mcp/scout/.../server.py`) — whose `if/elif` chain and field whitelists
  (`_STEP_KEYS`, `_USAGE_KEYS`, `_CITATION_KEYS`) **silently drop** anything they
  don't know, so a new event or field that skips it fails by going quiet, not by
  erroring. Both consumers read SSE *per line*, which is safe only because the
  payload is `serde_json::to_string` (newlines escaped, so a frame is always one
  `data:` line) — keep it that way, or they lose every multi-line frame in
  silence. The wire
  shapes are pinned by `progress_wire_fields_are_stable`,
  `done_event_carries_the_reason_and_the_run_cost_on_the_wire`,
  `done_names_no_run_when_the_journal_write_failed` and
  `each_action_names_its_argument_on_the_wire`. `done` carries `run_id`/`seq` — how a
  client that just watched a run offers it back as context — and both are **null**
  when the best-effort journal write failed, since a fabricated id would name a run
  nothing can fetch. Nullable, not absent: scout reads them explicitly rather than
  through `_USAGE_KEYS`, because they are not cost.
- **The report turn passes no tools at all** — the field is *omitted*, not sent
  empty, so there is structurally nothing to call. That is the fix for a measured
  failure: on the old text protocol, ~1 run in 5 across *three* different models
  answered "write the report" with one more tool call, because up to sixteen turns
  had rewarded exactly one reply shape. It also swaps in `REPORT_SYSTEM_PROMPT`
  (a writer role) rather than appending another instruction. Two backstops remain
  for the case where a model writes JSON anyway: the **content gate** in
  `chat_turn` withholds (never streams) a reply whose first non-whitespace char is
  `{`, which is what makes a re-ask safe — `is_withheld` tells the caller nothing
  reached the client — and a second such reply fails the run with
  `research.no_report` instead of shipping JSON as a briefing. A withheld reply
  that is *not* a call attempt still gets streamed, in one event. Both report
  passes (draft and rewrite) go through the one `write_report`, which returns a
  `ReportOutcome` — `Written`/`Empty`/`ToolCall`, kept apart because
  `research.no_report`'s detail names which defect to re-ask about. With the gate
  closed for the draft, "nothing streamed" is unconditional, so the re-ask needs
  no `is_withheld` guard there.
- **`done` carries a `reason`** (`DoneReason`): `finalized` when the model judged
  the evidence sufficient, else `time_exhausted` / `tokens_exhausted` /
  `budget_exhausted` (steps) / `context_exhausted` / `unparseable` /
  `repeated_calls` — one per `break` in the tool loop, and the four budget reasons
  are distinct precisely so a log query can say *which* limit is binding in
  practice. The values are a wire contract
  (`done_reason_wire_values_are_stable`), like `ApiError` codes. This exists
  because a report cut short and a complete one were previously
  indistinguishable, so scout's "trust the report, don't spot-check it"
  instruction had no honest coverage signal to qualify it; scout now surfaces
  `done_reason` + an `incomplete` hint, and its `_INSTRUCTIONS` tell the caller to
  read that field. Adding a `break` to the loop means adding a variant.

### What was measured (2026-07-28)

A 108-run matrix — 4 models × 12 questions about this repo × 3 seeds, effort
medium, temperature 0.2 — plus follow-up arms. Both the harness and the corpus
revision predate 1.0.0 and are gone; this text is the record. Every design decision
above that says "measured" points here, and nothing in it is reproducible from the
repository — treat it as evidence for the choices, not as a benchmark to re-run.

**Why the pipeline learns names before it searches.** Search matches text and code
is written in identifiers, so a model that does not yet know a name cannot ask the
query that would work and spends its budget rephrasing (a natural-language query
retrieves the *test* that describes a behaviour, score ~9; the identifier retrieves
the implementation, ~13). Worse, the transcript is the run's only memory — the
thinking channel is discarded every turn — so it re-derives the same plan from raw
tool output each turn. `list_files`/`outline`/`read_chunks`, the plan turn, the
pinned run-state note and the sufficiency turn each close one of those measured
failures; none is a guess.

**Why depth is not the knob.** Four budget axes with different jobs, and the axis
that looks obvious is the one that does nothing. Raising `max_steps` 20→48 for
`glm-4.7-flash` (seed 1, all 12 questions, everything else held): median depth
16→16, `finalized` 3/12 in both arms, and citations 60→32 — deep reports drift
from `path:start-end` to bare `(lines 369-394)`, which names no file and cites
nothing. Runs stopped by the step cap fell 5/12→1/12 and became `repeated_calls`
stops instead: the wall moved, it did not open. The one run that spent the whole
raised budget wrote the longest report in the arm and the worst coverage in the
sample.

**Why citations are checked server-side.** `qwen2.5-coder:32b` made **zero** tool
calls in 36 runs, answered from its weights in 16 s, declared the evidence
sufficient every single time, and all 18 of its `path:line-line` citations were
unverified. Without provenance checking it would have topped the speed column and
read as competent prose. That measurement is why a failing draft is now sent back
before anything reaches the client.

**The two winners.** `glm-4.7-flash` is the most thorough — 48 % hand-scored
coverage across all 12 questions, 192 of 193 citations verified with **none**
invented, and the only model to answer several questions at all — at 120 s median,
19 steps and 160 k prompt tokens, with only 22 % of runs (8/36) ending because it
decided they should. `gpt-oss:20b` is roughly **twice as fast** (40-45 s median,
9-10 steps) and writes the single best answers it manages (59 % over the 8
questions it answered, 90 % on one), **but it cites more loosely**: 5 unverified
of 151 citations in its last 36-run arm against glm's 0 of 193, and it never once
called `symbols` in 36 runs — it navigates by text similarity and never uses
exact-name lookup. It also needed the tool-call-parse retry to exist at all (11 of
its first 36 runs died on an Ollama 500). `gemma4:12b` is the cheapest, most
reliable and shallowest (34 % coverage, 4 steps, 78 s, 86 % finalized, 145/146
verified); `qwen2.5-coder:32b` is disqualified on integrity. `glm-4.7-flash` stays
`[research].default_model`; gpt-oss is the pick when latency dominates and the
reader will check the `citations` event.

**What these numbers are not.** Coverage was hand-scored by the same person who
wrote the rubrics, the loop and the harness, over seed 1 only; the independent
judge never ran. The mechanical columns (time, steps, stop reason, citation
counts) are over all 108 runs and are the ones to trust. Duplicate rejections are
invisible on the wire — a rejected call emits no `step` — so a model burning turns
on rephrasing shows up only as turns without steps plus an early non-`finalized`
stop.

### What was measured (2026-07-30)

A 28-run corpus on this repo after the hard deadline, the new tools and scope
enforcement landed: 12 questions × `glm-4.7-flash` and × `gpt-oss:20b` at `effort:
medium`, seed 1, temperature 0.2, plus 3 scope probes and 1 seed-2 control. No
hand-scored coverage — one seed, one afternoon — so only the mechanical columns are
claims. The harness was throwaway; the corpus of record is the `research_runs`
table. (Those runs predate 1.0.0 and its schema, where the tool-usage and staleness
side tables were folded into columns on that table.)

**Scope enforcement holds, and is nearly free.** A run scoped to `*.md` and asked
about `src/` kept every lookup — `search`, `grep` *and* `read_chunks` — inside
markdown and said in its report that it could not reach the source. A run scoped to
`programming_languages: ["rust"]` and asked about the Python MCP servers got 0 hits
from `list_files` twice and reported the question unanswerable instead of inventing
Python behaviour — which is the property that matters, and the reason the
ungrounded-report gate must exempt a run that was shown nothing. Across the scoped
runs the scope hid 511 rows from name- and text-keyed lookups at a cost of **one**
out-of-scope refusal: the model learns the walls from the prompt and the state note
rather than by hitting them.

**The new tools split by model, oppositely.** `grep` is glm's (13 calls, 11 with
hits; gpt-oss never called it once, and searched instead on the same question);
`note` is gpt-oss's (12 calls over 4 runs, including on questions unrelated to
notes; glm never wrote one in 16 runs). `revise_plan` was called **zero** times by
either — see the sufficiency-turn bullet for why it was wired into the re-open nudge
rather than removed. The note cap never bit, so it is not why glm abstains.

**Provenance is perfect where it can see, and blind where it cannot.** 85/85
citations verified, 0 invented, 0 stale, 0 path-only across both models — and 5 of 24
reports cited nothing parseable at all, which is the hole the gate now closes.

**`max_steps: 20` is the axis under pressure.** Time never bound (max 441 s of 900),
tokens never bound (max 390 240 of 1 200 000 — the raise from 400 000 was necessary,
not cosmetic), context never came close (peak prompt 28 328 of 98 304 × 0.7); 3 of 24
runs stopped at exactly 20 and 5 of glm's 12 reached ≥19 against a median of 15.5.
Note that `done.binding` is **not** evidence here: it names the axis with the largest
*fraction* spent, and steps/20 beats time/900 in nearly any run, so it reads `steps`
even for a 4-step run. The report window is comfortable — the normal report phase
takes up to 31 s of the 120 s granted.

**The deadline works, and cost two lessons.** glm reproducibly wedges in its
*thinking* channel on one question at seed 1: before, the run died at 600 s with
`ollama.unavailable` and zero output; after, it stopped at the 900 s deadline and
shipped a report. Seed 2 on the same question finished in 120 s, so the wedge is
sampling, not the prompt. 2 of 12 glm runs had a turn exceeding 600 s — a 17 % hard
failure rate at the old `turn_timeout_ms`, which is why that key now must sit above
every budget.

## Git history channel

The working tree says what the code **is**; `project_commits` +
`project_commit_paths` say **why** it became that way. Opt-in
(`mindex-index --history`, off by default), metadata-only: **no embeddings, no
Qdrant, no chunks, no derivation version**, and one model-facing tool
(`file_history`) the run reaches for when it decides the question is historical.
The channel is complete at that: **commit metadata is the whole feature**, not a
first instalment of one, because the high-value history questions ("what touched
this file and why", "what changed recently") are SQL questions, not similarity
questions — and the tool is offered, never imposed, so a run that has no use for
history simply never calls it.

**Semantic search over commit messages is not part of it, and the cost is why.**
Vectors here are not one more column: commit points cannot live in the project's
collection (isolation is a `has_id` filter built over `project_file_chunks`, and
widening it hands commits to `/search` and to every client expecting
`path:start-end`), so it means a second collection per project, doubling
`COLLECTION_SCHEMA_VERSION`'s no-self-healing hazard; the hard-delete lifecycle
below would have to invert to soft-delete + GC in three places; a sha is a content
hash and so is structurally incapable of noticing a changed message-composition
rule, which forces a derivation version of its own; and `POST /history` would grow
an embed phase — a first reconciliation of 20 000 commits is ~78 GPU batches inside
one request that today returns in milliseconds. That is more than the whole channel
cost. If message *search* is ever wanted, the ladder is `LIKE` (the `grep_core`
precedent) → FTS5 → vectors, each rung having to be measured insufficient before
the next; FTS5 is unusually cheap here because commit messages are immutable and are
only ever replaced wholesale by reconciliation, so the invalidation surface that
makes FTS5 a project for chunks is nearly absent. A text-keyed commit tool carries
one non-obvious defect to solve first: a message hit shows the model **no file**, so
its `shown` evidence is empty, and a run whose only tool was that one lands in the
ungrounded-report gate's *exemption* — the hole the 2026-07-30 measurement closed.

**Not pseudo-files, and `/drift` is the reason.** A commit modelled as a
`project_files` row would need a `programming_language` passing that CHECK and a
path passing the path CHECK — and would then be reported `orphaned` by every
drift check forever (the working-tree manifest can never contain it),
`mindex-index --check` would exit non-zero on a clean tree, and the watcher would
keep trying to delete it. Exactly the `research_runs` argument. Living in their
own tables also excludes commit rows from `build_search_query`'s candidate set
**by construction** rather than by a filter someone must remember to write, which
is why this is two tables rather than a `channel` column on
`project_file_chunks`. Pinned by `commit_rows_are_invisible_to_drift` and
`test_commit_paths_never_surface_in_drift`.

**`project_commit_paths.path` carries no FK, deliberately.** A commit names paths
deleted years ago, paths `.mindex` excludes, and paths in languages the enum does
not carry: `RESTRICT` would refuse the insert, `CASCADE` would erase history when
`prune_deleted_files` runs. The join into the code channel is therefore a *soft*
join by equality, and `file_history` must report an un-indexed path as such — the
`outline.indexed` failure again.

**Hard delete, no GC.** These rows own nothing outside SQLite, so their lifecycle
is `project_file_symbols`' (delete and be done), not `project_file_chunks`'. That
inverts if commit messages ever gain vectors.

**Sync is set reconciliation, and that is the whole design.** `POST
/v0/{guid}/history` is a **full-set replace within `since`**: a sha is the hash of
its own content, so there is no "same identity, different bytes" case and no
update path at all. Force-push, rebase and history rewrite are therefore *not*
special cases — each is one reconciliation in which many shas orphan at once.
`since` bounds only the **deletion** half and is load-bearing: without it a client
walking a window would wipe everything older on every pass, since from the
server's side an unmentioned commit and one outside the walk look identical. The
posted set goes through a temp table, not a `NOT IN (?, …)` list, whose bind count
would hit SQLite's variable limit inside the range `max_history_commits` permits.

**Retention is `DELETE /v0/{guid}/history`, and it is the half reconciliation
structurally cannot do.** A `POST` drops only what the tracked refs no longer
reach, so a commit still on `master` never ages out however old it gets — the age
window bounds *ingestion*, not retention, which is easy to misread as a retention
policy that is quietly not one. The bounds are `keep_last=N` (newest by
`committed_at`, `sha DESC` breaking the tie so a rebase's same-second commits
prune reproducibly) and `older_than=<unix seconds>`, and they **intersect**: a
commit dies only if both condemn it, so `keep_last` is a floor the clock cannot
cut through and "prune anything older than a year, but never leave me fewer than
N" means what it reads as. Naming neither is a 400
(`validation.history_bound_missing`), the `require_nonempty_selector` rule for a
resource whose bounds are scalars — a wipe is asked for (`keep_last=0`), never
arrived at by forgetting a parameter. It is deliberately **operator-facing and
called by no client**: the endpoint is a handle, and giving `mindex-index` a
retention flag would make every ordinary indexing run a potential deleter. Unlike
`DELETE /files` this is destructive without being lossy — the repository is the
source of truth, so the next `--history-only` run refills whatever the refs still
reach.

**One producer: `mindex-index`.** Rule 10 (**Four clients**) does **not** fire —
its trigger is what a file set is, what a path spells, what bytes get hashed,
which files a client refuses, and a commit list is none of those. So the watcher,
the VS Code extension and the MCP `index_files` tool are deliberately *not*
producers; replicating a git walk four times would add the surface that rule
exists to shrink. `--history-only` restricts a run to the history phase **without
switching the channel on** — that split is what lets the post-commit hook pass it
unconditionally without enabling history behind the operator's back. A missing
`git` or a non-repo root is a WARN that skips the phase, never a failed run (the
`SymbolExtractor` degradation rule).

**`--relative` is not optional.** `git log --raw` reports paths relative to the
**repository** root while `--root` may be a subdirectory of it, so without it a
run scoped to `src/` indexes `db/qdrant.rs` as a file and `src/db/qdrant.rs` as a
commit path — the soft join is then empty for every file in the project and
`file_history` answers "no commit touches this" with nothing erroring. At the
repository root the flag is a no-op; below it, it also drops commits that touched
nothing under `--root`, which is the right scoping. Pinned by
`the_walk_asks_git_for_root_relative_paths`.

**Four traps in `git log --format=<sep> --raw -M -z`**, each pinned by a test in
`tools/indexer/src/git.rs`. `%s` is **not** requested: it is the first *paragraph*
of `%B` joined, so asking for both invites disagreement on a wrapped subject — the
subject is derived instead. `-z` plus `%x1e`/`%x1f` is mandatory, not tidiness: a
body contains newlines and may contain tabs and anything else (a body containing
`\x1f` itself costs that one commit its paths and nothing else, because records
split on `\x1e` first). And **the raw block's arity depends on its status letter**:
an ordinary change emits one path, a rename or copy emits *two* — a parser
assuming one desynchronises for the rest of the stream and silently files every
later path under the wrong commit. And **git separates the format output from the
diff with a newline**, so the first raw header arrives as `"\n:100644 …"`: a
parser that tests `starts_with(':')` without trimming stops at the first token and
returns **no paths at all**, for every commit, with no error — which is how it
shipped past eight unit tests whose fixture was tidier than git's real bytes. A
commit legitimately having no paths (a merge) is what makes that silent. That is also what `old_path`'s biconditional
validation catches at the edge: `Some` on a modification is the signature of a
mis-parsed stream, so it is a 400 rather than a stored desync.

**Four client-side drops, all announced.** Age **and** count bounds together (one
alone breaks on a repo idle for a year, or on one having a furious month);
messages under `history_min_message_bytes`; merge commits whose subject is
git-generated **and** whose body is empty **and** which have >1 parent — the
conjunction is what spares a GitHub squash-merge, which is single-parent and
carries the PR description, often the best prose in a repo; and commits all of
whose paths are outside the project's globs. An over-cap message is **truncated
with a marker**, not dropped: the server would 400 the whole reconciliation, and
dropping would take the commit's path list with it. A channel that quietly indexes
a third of what it walked is indistinguishable from a repository that small.

**`file_history` reports three flags because an empty list has three meanings**
(`history_indexed` / `in_scope` / `path_indexed`), and a bare `[]` reads as the
one that is never true. Path-keyed, so out of scope is an explicit **refusal**.
Its `shown` evidence is **only the asked path, span-less**: recording the commit's
other touched paths would mark files the model never saw as shown, quietly
promoting a later invented citation from `unverified` to `path_only`. **No commit
citation grammar, deliberately** — a sha is content-addressed and `git show`
verifies it, so it is the one class needing no server-side gate; the prompt
requires a historical claim be anchored to a `path:start-end` in the code with the
sha named in prose, and every result repeats that. A report citing only shas
therefore parses to `total: 0` and correctly trips the ungrounded-report gate.
Shipping the tool without its `system_prompt` paragraph would repeat the markdown
lesson exactly: the corpus half is invisible without the prompt half.

## Retrieval pipeline

Three named vectors per collection: `dense` (1024-d cosine), `sparse`
(SPLADE-style), `colbert` (1024-d, multivector MaxSim). Search: prefetch top-200
dense + top-200 sparse → RRF fusion → ColBERT rerank → top-k. `post_search` runs
**two** SQLite queries around Qdrant — candidate `qdrant_guid`s first, then
`code`/metadata for *only* the top-k winners; never load `code` for the whole active
set (don't collapse into one query). Results are **sorted by score descending**
before responding (don't rely on Qdrant's order). Sparse weights ≤ 1e-5 are dropped
before upsert. Batch sizes: `--embed-batch` chunks per `/encode` (default 256, the
GPU-load lever), 256 points per Qdrant upsert/delete (`embed.rs`). Embed-response
vectors are positionally aligned with the chunk list.

**The query path may run on a second embedder instance.**
`[model].query_server_url` (absent = one instance does both, and `RouterState`
holds the *same* `Arc` twice) puts `/search` and every research search on its own
BGE-M3 — typically `--device cpu`, since a query is one ~20-token text and is
latency-bound, while indexing sends batches of hundreds and is throughput-bound.
What it buys is the ~6 GiB of VRAM the resident fp32 model otherwise holds
permanently, which on a 32 GiB card decides whether a 23 GB local LLM runs on the
GPU at all. Both instances must be the same model at the same precision and
**nothing checks that they are**: reduced precision on one side flips low-weight
token ids in and out of the sparse set, and a query-side sparse head that
disagrees with the index-side one presents as "search sometimes can't find the
obvious thing", not as an error. `GET /health` pings the second one separately
(`checks.query_embedder`) — but only when the deployment is actually split, which
is why the field is an `Option`, compared by `Arc::ptr_eq` rather than by URL.

The embedder client (`bge_m3.rs`) retries HTTP **429** up to 3× (200/400/800 ms,
respecting the cancellation token in sleeps), then gives up — the file goes `failed`
and the retry worker re-attempts later (layered backoff). Each `/encode` attempt has
a whole-request timeout (`[model].encode_timeout_ms`, default 10 min) so a wedged
embedder can't hang the retry worker.

## Slicer

`Slicer` (`slicing/traits.rs`) walks the tree-sitter AST depth-first, selecting
**named nodes** whose token span (HF tokenizer) is **128–512 tokens** (BGE-M3's
sweet spot). Token boundaries don't align with AST nodes and tokenization is
context-dependent, so the window is measured, not computed. `code` is extended left
to line start over pure indentation, and then further over the node's **doc comment
and attributes** (`ABSORBED_KINDS`, matched as substrings so there is no per-language
table). That extension is not cosmetic: a doc comment is a *preceding sibling*, never
a child, so without it the prose that says **why** is dropped from every chunk — which
is the actual cause of the documented "a plain-English query retrieves the test, not
the implementation" finding. Tests are simply the one place where prose and code land
in the same node. Absorption stops at a blank line (a detached comment documents
nothing in particular), at `max_tokens`, and at the furthest byte already emitted —
a large `#[utoipa::path(...)]` clears `min_tokens` on its own, becomes a chunk, *and*
is the preceding sibling of the function below it, so without that bound both chunks
would contain it.

**Node selection alone leaves ~37% of lines in no chunk at all**, so the walk is
followed by a **gap pass** (`[slicer].fill_gaps`, default on): everything inside a
node below `min_tokens` (consts, type aliases, small helpers, trait signatures — and
the doc comments attached to them, which left-extension can never reach because there
is no chunk to extend) plus everything between the selected children of an oversized
node, packed into line-aligned windows up to `max_tokens`, breaking at blank lines so
a chunk does not begin mid-sentence. Fragments under `GAP_MIN_TOKENS` (24) are not
worth a vector and are merged into the previous window rather than dropped. Measured
on this repo: line coverage 63% → **99.7%**, doc-comment coverage 40% → **100%**,
chunk count 553 → 972. It roughly doubles embedding work, which is why it is a knob.
The reason it matters beyond coverage: `read_chunks` used to dead-end on a coin flip,
because only 47% of symbol definitions were inside any chunk — so the pipeline the
research prompt prescribes (`symbols`/`outline` hands you a location → read it) failed
at its last hop.

`SlicedChunk.start_byte/end_byte` are
`#[cfg(test)]`-gated (byte-alignment tests only, never persisted), as is `from_gap` —
the window test needs it because **the token window governs node selection, not gap
chunks**: a gap chunk's floor is `GAP_MIN_TOKENS`, and holding it to 128 would mean
discarding the lines all over again. The window is
counted over **whole-file** token offsets, so re-encoding a chunk on its own is a
different measurement — an edge token splits differently without its surroundings,
and a node measured at 512 can re-encode at 513. `chunks_satisfy_token_window`
therefore asserts 128–512 ±`WINDOW_SLACK`; without the slack the test is a tripwire
on whichever source file happens to land on a boundary (`src/research.rs` did),
not a guard on the window.

**A line is not bounded by anything, so neither pass may cut only at line
boundaries.** A minified file is one line for its whole length, and one paragraph
of prose can be too, so the structural boundary both slicers prefer simply is not
there — and what came out was a chunk of *any* size. That is not a coarse chunk,
it is an unstorable one: a Qdrant multivector point holds at most 1 048 576
elements, ColBERT emits one 1024-wide row per token, and the embedder adds
`[CLS]`/`[SEP]`, so anything above `STORABLE_TOKENS_CEILING` (1022 tokens) is
**refused — failing the whole upsert batch, not the offending file**, while a
large one first exhausts the embedder's GPU memory and fails the batch that way.
Hence `token_boundary` (`slicing/traits.rs`), the last resort of both passes: cut
on a boundary the tokenizer itself reported. Two things about the ceiling are
easy to get wrong. It is `min`-clamped **in both constructors** rather than
enforced by config validation, because `[slicer].max_doc_chunk_tokens` **defaults
to 1024** — over the ceiling before a single chunk is cut — and rejecting that at
startup would refuse a config the operator never chose; the code window (512) is
far below it and unaffected. And a slicer must aim `RETOKENIZATION_SLACK` *under*
it, for the whole-file-offsets reason above: a cut measured at 1022 re-encodes at
1023. Documentation blocks are truncated to the same ceiling before they are
embedded for the semantic term — a block is not a chunk and has no size bound,
and its opening is what that vector is for.

**Documentation is chunked by a second slicer, and every rule above inverts.**
`MarkdownSlicer` (`slicing/markdown.rs`, `markdown` only) walks tree-sitter-md's
**block** grammar to atomic blocks — descending into `list_item`, because one list in
this file runs 359 lines — then packs runs of adjacent blocks into chunks by dynamic
programming (`best[j] = min_i best[i] + cost(i..j)`, exact, O(n²)). The cost is one
term per chunk against a penalty for swallowing a level-3+ heading, with `+∞` above
`[slicer].max_doc_chunk_tokens` and across any level-1/2 heading; greedy "fill to the
cap" is *not* the optimum, because it buries subsection headings to save chunks it
did not need to save. Three inversions, each measured: **no lower bound** (a 40-token
section is a complete claim; the code slicer's floor would drop it), **chunks nest and
merge** rather than one-node-one-chunk, and **the cap is 1024, not 512** (512 answers
15/23 documentation questions against 18/23 — it cuts explanations away from what they
explain). Note `MODEL_MAX_TOKENS` (512) is the *code* window's quality ceiling, not the
model's capacity: the embedder truncates at its `--maxlen`, default 8192.

**Boundaries come from two signals, and the second is a refinement of the first.**
Structure sets the hard rules; *semantic shift* — the embedding distance between
blocks, weighted `[slicer].doc_semantic_weight` — decides where to cut among what
structure leaves open. Measured on **this** repo the semantic term changes nothing:
it moves 7-13% of boundaries and alters the retrieved rank of **zero** of 23
documentation questions (MRR@10 0.3931 either way, identical per-case ranks). That
is a fact about this corpus, not about the technique — these documents are densely
and deliberately headed, so the author already marked every topic change. It is on by
default because the signal it adds is exactly the one structure cannot supply, and it
is worth most where structure is weakest: with sparse or absent headings the packing
degenerates to "fill to the cap" and cuts mid-topic, which is the common case in
projects whose documentation is not written like this one's. (A further pass
re-checking the shakiest boundaries against the model was measured *worse* and is not
shipped.) Separately measured and real: block structure beats a line-based `#`
splitter, MRR@10 0.3714 → 0.3931, recall@10 18/23 → 20/23.

Three consequences of the term being on. It costs **one `/encode` per document**, so
block embedding happens *outside* the prepare transaction — hence the two-phase
`plan` → `segment` API, unlike the code slicer's single call. An **unreachable
embedder degrades to structure-only** with a WARN rather than failing the file: a
refinement must never be a dependency, and structure alone is a good answer. And
chunk boundaries now depend on the **embedder's model and precision**, which
`CHUNKS_DERIVATION_VERSION` cannot see — the same blind spot as a grammar-crate bump.
Setting the weight to 0 restores pure structure and skips the round-trip entirely.

## Concurrency & cancellation

- **Async-first.** SQLite runs in `spawn_blocking` via `db_pool.transaction()`;
  Qdrant/embed are `.await`-ed in handlers — no `block_on`. Every long loop / I/O
  respects a `CancellationToken`; client-cancelled requests return HTTP 499.
- **Cancellation propagation (subtle).** A handler's `CancellationGuard` wraps a
  *fresh* token cancelled only by its own `Drop`. On client disconnect axum drops
  the handler future → `Drop` fires, but the future is gone, so in-handler
  `Cancelled` arms are defensive, rarely hit. The token's real job is letting
  in-flight `spawn_blocking` (slicer) and the embed `select!` bail after
  abandonment; the half-written row is recovered by the retry worker. Clean
  shutdown uses a *separate* token tree rooted in `main.rs`.
- **`IndexClaim` is an in-process keyed lock**, so mindex assumes **one process per
  database**. It serializes the handler↔handler and handler↔worker races on a file
  (contention → 429), but only within one process; running two mindex processes
  against one SQLite/Qdrant would need a DB-level compare-and-swap claim instead —
  a conditional `… → indexing` update plus an epoch column checked at
  `mark_indexed`, so a superseded writer abandons rather than clobbers. That is a
  schema migration, which is why it has not been done speculatively.
- **Connection-return is cancellation-safe** (regression guard, `sqlite3.rs`). The
  blocking task pushes its connection back into the pool *itself*
  (`conns.blocking_lock().push`), not the awaiting code after `handle.await`:
  dropping a `spawn_blocking` JoinHandle does **not** cancel the task, and if
  release depended on the awaiting future, a future dropped mid-transaction would
  leak the conn — after `db_pool_size` (4) such events the pool is permanently
  `PoolEmpty`. A closure panic is the one unreturned case (logged on `JoinError`).

## SQLite pool

Fixed-size pool of `rusqlite::Connection` behind a `tokio::sync::Mutex<Vec<_>>`
(pop/push). Per-connection PRAGMAs: WAL, `foreign_keys=ON`, `synchronous=NORMAL`,
16 KB pages. Handlers run **multiple sequential `transaction()` calls** (one per
logical step), not one giant transaction — the soft-delete pattern keeps state
recoverable if a later step fails.

## post_index shape

Two phases so the GPU sees big batches: (1) **`prepare` every file** — hash-check
(`Ok(None)` = unchanged, skipped) → set `indexing` → main tx (mark old chunks
deleted + slice + insert) → `Prepared` with that file's chunks; own `indexing_file`
span each (no `Entered` guard across `.await`). (2) **`embed_all`** chunks from all
prepared files in one batched `embed::embed_and_upsert` pass, then **`mark_indexed`**
each + tally. Recovery is per-batch: any `prepare`/embed failure sends every
already-prepared file to `failed`/`cancelled` via `recover_all`; the retry worker
re-embeds later. `tree_sitter::Parser` is `Send` — slicer built inside the
`spawn_blocking` closure. Request body limit: `[server].max_body_mib` (default
256 MiB) via `DefaultBodyLimit` (axum's 2 MB default is far too small); over-cap =
problem+json 413 (`request.body_too_large`), not axum's plain-text.

**`?stream=yes` reports the same pipeline as SSE**, and the pipeline itself is one
function either way: `run_index_job` is shared verbatim, the query only picks who
builds the terminal (`Json` body vs a `done`/`error` event), so the two modes
cannot drift. The cancellation shapes differ on purpose and mirror research's: the
JSON mode keeps its `CancellationGuard` (handler-future drop = cancel), the SSE
mode spawns the job detached and the *stream's* Drop cancels the token — a guard in
the handler would fire the instant the response is constructed. Recovery therefore
runs inside the job, so a disconnected streaming client still lands its batch in
`cancelled`. The event vocabulary (`started`/`prepared`/`skipped`/`embedded`/
`indexed`/`done`/`error`, `IndexEvent` in `models.rs`) is a wire contract like the
research one, in four places that move together: `post_index`'s doc comment, its
OpenAPI 200 description, the `mindex-index` reader (`tools/indexer/src/client.rs`)
and the VS Code client (`api.ts`); both consumers drop unknown events silently, and
the shapes are pinned by `index_event_names_are_stable` +
`index_event_data_names_its_fields_on_the_wire`. `embedded` (one per embed batch,
via `embed_and_upsert`'s optional progress callback — its one deliberate
side-channel) carries cumulative `chunks_done`/`chunks_total` plus the server's own
`elapsed_ms`, which is what makes a client's chunks-per-second a measurement
instead of the old batch-granular estimate; both clients compute it over a sliding
window and fall back to plain JSON transparently when an older server ignores the
query (`StreamOutcome.streamed` / content-type sniff). `done.files` is
byte-for-byte the JSON response body, so both modes tally identically. A typo'd
`?stream=` value or key is a 400 (`IndexQuery` is `deny_unknown_fields`), never a
silent fall-through to the mode the caller did not ask for.

## Mockable interfaces

Three traits; production type is the sole real impl, fakes live in `#[cfg(test)]`:
**`BGEm3Model`** (embedder, `Arc<dyn>` in `RouterState` + retry worker),
**`VectorStore`** (all Qdrant ops; error is `VectorStoreError`, a rendered string,
because `QdrantError` isn't test-constructible), **`Tokenizing`** (the slicer's only
tokenizer need; fakes avoid the HF download). New seam = minimal trait + owned error
if the real one isn't constructible. `SQLite3Pool` is deliberately **not** a trait
(its generic-closure `transaction` isn't object-safe) — test against a real
`:memory:` pool.

## Error handling, validation & logging

**Client error contract: `ApiError` → RFC 7807 (`backend/error.rs`).** Every non-2xx
is `application/problem+json` (`ProblemDetails`) with a **stable, namespaced machine
`code`** (`validation.top_k_out_of_range`, `selector.empty`, …) + English
`title`/`detail` + optional `field`/`meta`; the `code` is the localization key.
`ApiError` is the *single* enum; its `code()/status()/title()/detail()/meta()` and
the lone `IntoResponse` impl are the only place a response shape is built. **Codes
are an API contract**: the `codes_are_stable` snapshot test fails on any change, so
changing one is deliberate (also update the catalogue in `openapi.rs`
`info.description` + clients). Handlers return `Result<_, ApiError>`; domain errors
(`SQLite3PoolError`, `SlicerError`, `EncodeError`, `VectorStoreError`,
`EmbedUpsertError`) convert via `From`/constructors at the call site — the call site
keeps the contextual log + sysadmin hint, `From` never logs. Mappings:
`SQLite3PoolError::Cancelled` → 499 (rest `Internal`); embed request/decode →
`EmbedderUnavailable` 503; Qdrant search → `QdrantUnavailable`, upsert/drop →
`Internal`. No external error crates.

**Validation happens at the edge (`backend/v0/validate.rs`), before any work** — bad
input is a 400 with a precise `code`, never an opaque 500 from a SQLite `CHECK`. It
mirrors the schema constraints (`validate_path` = the path CHECK + `..`-traversal
guard; `validate_sha256_hex` = 64 hex) and enforces the `[limits]`/`search.max_*`
caps; `require_nonempty_selector` is the shared guard for the destructive endpoints.
The schema CHECKs and the shape-validation triggers stay as defense-in-depth. Handlers take
`ApiJson`/`ApiPath`/`ApiQuery` (`extract.rs`), not bare axum extractors, so
malformed body/path/query is the same problem+json envelope
(`request.malformed_body`/`malformed_path`), not axum's plain-text 400.

- No `unwrap`/`expect` in production paths (workers may `unwrap_or_default` on
  best-effort queries); startup-only panics name the file and what to check.
- **Logging shape:** a mandatory message stating *what operation failed* (never bare
  `error!(?err)`); error as a field (`error = ?e`/`%e`, not interpolated);
  identifiers as fields (`%` String/Uuid, `?` enums). Handlers carry
  `project_guid`/`pl`/`path` on the span; workers pass them explicitly. Infra
  failures end with a one-line sysadmin hint (embedder reachability + the `0.0.0.0`
  vs `127.0.0.1` gotcha, Qdrant, DB writability); logic errors don't.

## Metrics

`GET /metrics`, OpenMetrics text, on the same HTTPS listener — `prometheus-client`
with one owned `Registry` threaded as `Arc<Metrics>` through constructors
(`backend/metrics.rs`). No global recorder: that is the same rule config follows,
and it is also the only shape under which two unit tests in one binary can each
own an independent metric set. Scraped by the host VictoriaMetrics (`scheme:
https`, no `tls_config` — the mkcert root is in the system trust store, which is
the only path that works since the VM unit sets `ProtectHome=true`) and rendered
by the provisioned Grafana dashboard in `deploy/grafana/`.

**Metric names and types are a contract, exactly like `ApiError` codes** — a
dashboard is a client, and a renamed family is a silently blank panel.
`metric_names_are_stable` is the `codes_are_stable` mirror and pins *both*; the
type matters more than the name, since a counter→gauge flip renames nothing and
breaks every `rate()` built on it. Two encoding quirks the test encodes so you
never rediscover them: OpenMetrics puts `_total` on a counter's **sample** lines
and not on its `# TYPE` line (the family is `mindex_gc_runs`, the series
Prometheus stores is `mindex_gc_runs_total` — the test reconstructs and pins the
series name), and `encode` emits a `# EOF` terminator, so the body must be served
as `metrics::CONTENT_TYPE` and never as `text/plain`.

**The cardinality rule: every label value comes from a set the server defines** —
`MatchedPath` (router-owned), `ApiError::code()`, `ProgrammingLanguage::name()`,
`DoneReason::as_str()`, the tool names, the file statuses. Never a raw URI, path,
query, or model-supplied string without a bound. `project_guid` is the sole
open-ended label: it is UUID-validated before it becomes one, and on the HTTP
families it is off by default (`[metrics].per_project_http_labels`). Two products
are deliberately **split rather than crossed** for the same reason —
`research_runs{model,done_reason}` from `research_runs_by_effort{model,effort}`
(`model` is client-supplied), and `project_chunks_active{project,language}` from
`project_chunks_deleted{project}`. Histograms are not labelled by project at all:
each is a dozen-plus exposition lines, and multiplying that by the project count
buys a breakdown nobody reads.

**Clear-and-repopulate, and why counters are exempt.** A `Family` retains a label
set for the life of the process, so a deleted project would report its last known
file count until restart. `worker/metrics.rs` therefore builds each tick's whole
value map from SQL *first*, then clears and repopulates in one synchronous block
**with no `.await` between them** — that is the only thing keeping a scrape out of
the gap, so `apply` must never become `async`. Two structural guards: only
`StateMetrics` is ever cleared, and it holds **gauges only**. Clearing a counter
reads as a process restart to Prometheus and permanently re-baselines every
`rate()` over it. `StateMetrics` is also written by that worker and nothing else.

**Why decorators, and the four things that cannot be one.** `VectorStore`,
`BGEm3Model`, `ResearchTools` and `ResearchJournal` are wrapped once in `main.rs`
/ `post_research`: a seam decorator cannot miss a caller, an edited call site can,
and `MeteredJournal` alone yields nearly the whole research set because
`RunRecord` already carries it (so `run_research` needs no instrumentation). The
exceptions, each for a structural reason: `SQLite3Pool` is not a trait by design
(the generic-closure `transaction` is not object-safe) so it is instrumented in
place at that single choke point; the embedder's **429 retry loop lives inside
`encode`**, where three retries then a success is indistinguishable from one
success from outside; Ollama's tool-call-parse retry and silent transcript
truncation likewise happen *inside* one `chat_stream` call and leave no trace in
its return value; and the **indexing claim conflict** is swallowed at
`Err(ApiError::FileInFlight) => {}` while the request still 200s, so the HTTP
middleware can never see it. All four use an `Option<...>` field set by a
`with_metrics` builder, which is also what keeps the many `:memory:` test pools
and bare test clients constructing unchanged.

**In-flight gauges are `Drop` guards**, for the same reason `CancellationGuard`
is: a disconnected client's future is *dropped*, so the code after
`next.run(req).await` never runs. Research SSE streams die **only** by disconnect,
so an inc/dec pair would ratchet the gauge upward within a day. The guard also
records the abandonment as `status=499, code="request.cancelled"`, which is what
keeps `http_requests_total` reconcilable with `http_requests_in_flight`. The same
"a normal exit is a dropped future" logic is why `research_active` is **derived**
in the collector from `max_concurrent - available_permits()` rather than
incremented around the spawn.

**`enabled` means exposed, not measured.** `Arc<Metrics>` is always built and
always written into; `[metrics].enabled` gates only the route and the collector.
The alternative is an `Option` check at sixty call sites, for a relaxed atomic
add. Two consequences at the edges: the HTTP layer sits **outermost** and must be
a `Router::layer` (outside the router there is no `MatchedPath`, and every series
would carry an empty `route`), which also means a request matching **no** route
never reaches it — unknown-path 404s are uncounted rather than bucketed under a
fabricated label; and the HTTP/3 body-limit short-circuit answers before the
router, so it records itself. A second such short-circuit needs the same three
lines or it goes uncounted in silence.

**A rare labelled counter must be charted with `increase()`, never `rate()`** —
this is a dashboard rule that follows from how the metric layer works, and it is
the one that made the whole research row read as empty while `research_runs` held
the runs. A `Family` series does not exist until the first event carrying that
label set, so its first scraped sample is already **1**: there is no preceding 0
to subtract from, and a label set seeing exactly one event in a process lifetime —
the normal case for `research_runs{model,done_reason}`, and for every per-run
histogram — stays flat at 1 until restart. `rate()` over that is 0 for the entire
life of the series. `increase()` counts the first sample of a newly-appeared
counter (VictoriaMetrics does; upstream Prometheus does not, which is what
seeding the label sets at zero would buy — impossible here, since `model` is
client-supplied), so the same data is visible under one and invisible under the
other. High-traffic families (`http_requests_total`, `index_files_total`) hide the
defect because their second event arrives seconds after their first. The paired
rendering rule: a handful of events a day is drawn as **bars** with a `sum`
legend, and a quantile over a rare histogram as **points with gaps kept**, since
there is nothing to join into a line. And a share-of-total stat needs
`or vector(0)`: the healthy case is that the `unverified` series does not exist,
and "No data" is indistinguishable from a broken query.

**What is deliberately not measured.** Per-project code bytes as a *gauge*:
`SUM(LENGTH(code))` is a full scan of the biggest column in the schema every tick
and would evict the page cache the search candidate query depends on — the
write-time `index_code_bytes_total` counter answers it better anyway. A drift
*level*: `/drift` compares against a client-posted manifest and the server never
walks a tree (see **Four clients, one working-tree view**), so there is nothing to
gauge; `post_drift` counts what checks *reported*, and a real drift gauge belongs
to `mindex-watch`. And tokio runtime internals (task counts, blocking-pool depth,
worker parking), which need `--cfg tokio_unstable` against an unstable API —
"threads and pools" is covered instead by the SQLite pool, the claim table, the
research semaphore, in-flight requests and the research runtime's configured
worker count.

`/metrics` is the one route with no `#[utoipa::path]` and no `openapi.rs` entry —
it is not JSON, not versioned, not problem+json, and its consumer is a scraper.
`openapi_spec_is_complete_and_versioned` asserts the *absence* so the omission
reads as a decision rather than an oversight.

## Performance conventions (hot paths)

Build `ChunkAsVector` by **moving** `dense_vecs`/`colbert_vecs` (`into_iter`), not
cloning; split the sparse `HashMap` into parallel index/value arrays in a **single
pass** with the `>1e-5` threshold applied once. Lives in `embed.rs` (shared by
`post_index` and the retry worker).

## Languages

The supported set *is* the `ProgrammingLanguage` enum + the grammar crates in
`Cargo.toml`; extension map in `tools/indexer/src/scanner.rs`. Hard constraint:
every grammar crate must depend on `tree-sitter ≥ 0.23` (`LanguageFn` API) — older
ones cause a native `links` conflict; verify with `cargo info` + registry source
before adding. **Adding a language touches all of these** (each omission fails
differently — 400 → SQLite CHECK 500 → silently skipped file):

1. `ProgrammingLanguage` enum + `ToSql`/`FromSql` + `ALL`/`name()`
   (`backend/v0/models.rs` — the crate-root `src/models.rs` is a two-line module list),
   lowercase serde name. The sub-items fail differently: a missing `name()` arm is a
   compile error; a missing `FromSql` arm fails on **read** (rows insert fine, then
   500 on any query selecting the column); a missing serde rename means the wire
   name never deserializes → 400 `request.malformed_body` (not 422 — every unmapped
   `ApiError` falls through to `BAD_REQUEST`). Omission from `ALL` is the only
   **silent** one, and it costs two things: absence from `GET /config`, *and* silent
   exclusion from `every_language_constructs_or_declines` (`slicing/symbols.rs`),
   which iterates `ALL` — so a broken tags query for that language ships untested.
2. `CHECK` constraint on `project_files.programming_language` — in **two** places, and
   editing only the first is silent. `src/db/migrations/v1.0.0_schema.sql` is what a
   *fresh* database is built from and is never re-read afterwards (the filter is
   `version > user_version`), so a database already in use needs a new migration that
   rebuilds `project_files` with the widened list. `v1.1.0_toml_yaml_languages.sql` is
   the pattern to copy: SQLite cannot alter a CHECK, and the rebuild must be
   create-copy-**drop**-rename in that order, running under
   `SQLite3Pool::migration_transaction` — renaming the old table out of the way first
   makes the two child tables' `REFERENCES` clauses follow the corpse, and the `DROP`
   is refused by their `ON DELETE RESTRICT` unless foreign keys are suspended. Both
   files must end up with the same list.
3. `tree-sitter-<lang>` in `Cargo.toml` (verify ≥ 0.23).
4. Arm in `tree_sitter_language(pl)` (`handlers.rs`) — total match, missing arm =
   compile error. A grammar here does **not** commit the language to the AST-walk
   slicer: `markdown` returns tree-sitter-md's *block* grammar and is then dispatched
   to `MarkdownSlicer` by the one `pl == Markdown` branch in the prepare tx (see
   **Slicer**). A second such language adds a branch there, not a second code path.
5. Arm in `queries_for(pl)` (`slicing/symbols.rs`) — total match; `None` is legal
   (language yields no symbols). Prefer the crate's `TAGS_QUERY` const; if the crate
   only *packages* `queries/tags.scm` (or ships a broken one), vendor the file under
   `slicing/queries/` with a provenance header (scala/csharp precedent). Add a
   fixture test in `symbols.rs` when a query exists. **Bump
   `SYMBOLS_DERIVATION_VERSION`** — without it, files already indexed keep their
   matching hash and never gain the new language's symbols. Today the bash, html,
   css, json, toml, yaml, haskell, zig and sql crates ship no tags query, so those files
   contribute **no symbols** (chunking and search are unaffected) — revisit per
   language when upstream adds one. `markdown` is `None` *permanently*, not pending
   upstream: headings are a table of contents, and making `outline` mean "the
   document's sections" for one language and "definitions" everywhere else buys a
   second meaning for the same tool. The vendored `.scm` files are verbatim copies
   and must be refreshed when their crates bump.
6. `detect_language` + `Language::name()` in **three** extension maps, each silently
   skipping the file when it lacks the entry: `tools/indexer/src/scanner.rs`,
   `tools/watcher/src/scanner.rs` (a verbatim copy — the watcher goes blind to the
   language otherwise), and `tools/vscode/src/languages.ts` (`EXT_TO_LANGUAGE`).
7. `ext_to_lexer()` in `mindex-search.sh` (pygments map); its `VALID_LANGS` is only
   the offline fallback (canonical list comes from `GET /config`).
8. The VS Code **language mark**, in four places — this one fails as a red test rather
   than at runtime, because `langIcons.test.ts` is exhaustive over `ALL_LANGUAGES`.
   `DEVICON_MARKS` (`esbuild.mjs`) if devicon draws the language, else
   `LANG_FALLBACK_CODICON` (`shared/langIcons.ts`) — sql and toml are the two it does
   not; a rule in `media/lang.css`; and the base colour in the test's own `BRAND`
   table, which every language needs whether it paints a mark or a codicon. The two
   hex values in the CSS are **derived, not chosen** — the test recomputes them with
   its `adapt()` (mix toward white on dark, black on light, in 5% steps until 3:1),
   so run that function and paste its output. `shared/langGlyphs.ts` is generated;
   rebuild, never hand-edit.
9. Rebuild the image. A container whose volume predates the `CHECK` change picks the
   widened list up from the migration added in step 2 — that is what makes dropping
   the volume unnecessary, and why step 2 is not optional.

## Four clients, one working-tree view (sync rule)

`mindex-index`, `mindex-watch`, the VS Code extension and the MCP `index_files`
tool all answer the same question — *which files are in this project and what is in
them* — from four separate implementations. The server never walks a tree; it only
believes what a client posts. So **any change to what a file set is, what a path
spells, or what bytes get hashed must land in every client in the same commit.**
The concrete list, each item a place divergence has to be re-checked:

- **The file set**: the `.mindex` walk (`tools/indexer/src/scanner.rs::scan`,
  `tools/watcher`'s `build_manifest`, `tools/vscode/src/scanner.ts::scanWorkspace`)
  — excludes-before-includes, glob dialect (`globset` with
  `literal_separator(true)` vs picomatch defaults), symlink policy, and the
  extension map (**three** copies, see **Languages** step 6).
- **The bytes**: what is hashed for `/drift` must be exactly what `/index` would
  post — the server hashes `code.as_bytes()`, so any client that hashes something
  else (raw bytes vs decoded text, BOM kept vs stripped) reports permanent drift.
- **The refusals**: a file a client will not post must not appear in its manifest
  either. Binary, unreadable and **over `mindexfile::MAX_CODE_BYTES`** files are
  dropped from both, in every client (`scanner.ts` keeps its own copy of that
  constant). Claiming a file the server would reject is worse than dropping it: the
  400 fails the whole batch it travelled in, and the file is reported `missing` on
  every check forever.

This class of bug **never surfaces as an error**. It surfaces as drift that
reindexing cannot clear, or as an index quietly missing a third of the tree, which
is why it is worth a checklist. `tools/mindexfile` exists to shrink the surface —
it is the one Rust parser and now also holds the shared size cap; the TypeScript
mirror (`mindexFile.ts` + `globContract.test.ts`'s shared fixture table) is the
only sanctioned copy.

**Git history is deliberately outside this rule**, and single-producer for that
reason — see **Git history channel**. The rule's trigger is what a file set is,
what a path spells, what bytes get hashed, and which files a client refuses; a
commit list is none of those, so `mindex-index` walks git and nothing else does.
Do not "fix" that by teaching the watcher or the extension to walk it too.

And the client is only as fresh as its build: the extension runs `dist/`, so a
change to `src/` that was never `npm run compile`d leaves a plugin scanning by
yesterday's rules against today's server. Recompile before concluding the plugin
is wrong.

## Tooling gotchas (full docs: each tool's `--help` / README)

- `mindex-index`: identity and scope come from `.mindex` at `--root`;
  `--project`/`--include`/`--exclude`/`--language` **replace** (never extend) the
  matching key, so a scoped one-off run drops the file's list for that key.
  `--print-guid` resolves the GUID and exits — the way a script gets it without a
  second parser. `chunk_count == 0` = sliced to no chunks
  (<128 tokens), *not* unchanged — hash-unchanged files are absent entirely.
  `--check` runs `POST /drift` instead of uploading; non-zero exit on actionable
  drift (`--json` for scripts). `--force` bypasses the unchanged-skip (hash *and*
  derivation versions) — an escape hatch for what versioning can't see, not a
  routine flag; scope it with `--include`/`--exclude`. `--symbols-only` rebuilds
  just the symbol table (no GPU, no Qdrant); its summary counts symbol rows, not
  chunks, and reports no "too short" (that is a slicer verdict). `--history`
  additionally reconciles the git channel (off by default; `git_refs` in `.mindex`
  picks the refs, and `--git-ref` replaces that list like every other scope flag);
  `--history-only` restricts a run to that phase *without* switching the channel
  on, which is how the post-commit hook passes it unconditionally. Watch the drop
  counts it prints — see **Git history channel**.
- `mindex-watch`: inotify daemon keeping the index live — debounced reindex/delete
  (`--debounce-ms`, 1000) + full drift sweep every `--drift-interval` (300 s) to
  catch offline changes. Reads `.mindex`. `--dry-run` makes no mutating call but
  still runs the read-only drift check.
- `mindex-search.sh`: the single search frontend. Prints results **ascending by
  score** (best match last, above the prompt); every option has a `MINDEX_*` env
  fallback (flag wins). Language-flag validation fetches `GET /config` at runtime;
  baked-in `VALID_LANGS` is only the offline fallback. 404 = no match, not an error.
- MCP `mindex` (`tools/mcp/mindex/`): the **primary agent interface** — `search`
  (top-5 cap fixed in the adapter), `symbols` (exact-name defs/refs lookup, 10-per-role
  cap; the first stop for "where is X defined / who calls X"),
  `index_files`/`delete_files`, `drift`, `cancel_indexing`, plus read-only
  introspection. `index_files` is **only** for the
  few just-touched files, bodies passed **verbatim** (unchanged files are
  hash-skipped server-side); bulk jobs go through `mindex-index`. `search` takes
  optional `include`/`exclude` (`{paths, programming_languages}`) passed straight to
  `/search`. No network at handshake.
- VS Code (`tools/vscode`): the **Ask** sidebar WebviewView (`askView.ts`) is the one
  entry point for both query modes — a Search/Research segmented toggle over a shared
  box, options swapping per mode. It is an *input surface only*: search results stay
  in the QuickPick (live editor preview + Esc restore, which a narrow sidebar list
  would lose) and research still streams into its WebviewPanel tab (steps + live
  thinking + `marked`-rendered report). The SSE client is hand-rolled in `api.ts` (no
  reconnects — a drop is a cancel, by contract). Force reindex (this file / whole
  project, the latter modal-confirmed) lives in the Drift view's overflow menu.
  **Research History** (`researchRunsPanel.ts` + `webview/runs.ts`) is an
  editor-area panel, not a third sidebar view — the same argument `icons.test.ts`
  already records for moving Server Status out, so that pinned view list is
  unchanged. Two panes, a debounced search (`shared/debounce.ts`, vscode-free so
  `node --test` can reach it; trailing, because the first keystroke of an identifier
  is one letter and its results would be wrong on arrival), keyset paging by `seq`,
  and a multi-select that posts to the host and arrives in the Ask form as removable
  chips. **One `AbortController`, aborted on every keystroke**, and the caller must
  swallow `AbortError` itself: `api.request` *rejects* on abort while `research()`
  resolves, and "fixing" that asymmetry would break every other caller's ability to
  tell a cancelled request from an empty answer.
  **The form offers only what the server confirmed exists**: the language pickers are
  the project's `chunks_active > 0` languages and the model field is a `<select>` over
  `research.models`, both arriving through `StatusMonitor.refresh()` — the one
  place that already runs at activation, on `.mindex` change, and after every
  reindex/delete, so a new refresh site cannot forget them (it re-reads `/config` on
  every pass for the same reason: the model list is no longer static). Three rules
  hold that together. `undefined` inventory means *unknown* (server down, no project,
  a 404, an older server) and falls back to the full `ALL_LANGUAGES`, as does an
  *empty* one — an empty picker is a dead form, while a superset merely lets a filter
  match nothing. The `readScope`/submit whitelists stay `ALL_LANGUAGES` and are
  deliberately **not** narrowed to the inventory: offering is an availability hint,
  validating is a contract, and a language indexed a second after the last stats fetch
  is legitimate. And everything is pushed by `postMessage` and rebuilt in the webview,
  never by reassigning `webview.html` — a re-render would discard the half-typed
  question, the restored `getState()` and a live run's Cancel state, on every status
  refresh.
  **The form is also gated on what the server can currently do, and that gate needs a
  clock.** `fetchStatus` publishes one `Availability {ask, research, reason}`, split
  because the server has two classes of dependency: a *required* one takes everything
  down and the server reports itself `degraded`, while Ollama takes only Research and
  leaves health at `"ok"` deliberately — so `ask` reads `health.status` and `research`
  additionally reads the one check. One flag would either kill Search whenever no local
  model was running, or keep offering Research against a server that cannot serve it.
  The reason names the *required* checks that are down and never Ollama, which cannot
  be the cause. `!research` disables the Research **tab** and `!ask` disables every
  control (Stop excepted — a live run still has a connection to drop) leaving the
  half-typed question visible and inert; the mode is never switched out from under the
  user. A degradation also aborts what is running — research and any in-flight search,
  via `RunRegistry` — resetting the handles **before** reporting, since a notification's
  thenable resolves only when the user dismisses it (the trap that once left Research
  disabled behind an un-clicked toast), and reporting it as a failure rather than as a
  cancellation, which would read as the user's own Stop. None of this is observable
  without `[mindex.statusPollSeconds]` (default 30, `0` = off): every other refresh is
  event-driven, so before the timer a dependency could die and the form would go on
  offering work against it indefinitely.
  **Language marks are vendored, two-toned and tested.** `esbuild.mjs` generates
  `src/shared/langGlyphs.ts` from devicon's *monochrome* SVGs — fills stripped so CSS
  `color` drives them — committed like the vendored tags queries, and `sql` alone falls
  back to a codicon (devicon draws products, not the language). Each language declares
  **two** colours in `media/lang.css`, not one: 13 of the 21 official brand colours fail
  3:1 against one of VS Code's two default backgrounds (rust and markdown are black, C
  is near-white), so the pair is derived by mixing toward white or black in 5% steps
  until it clears — and `langIcons.test.ts` recomputes that derivation rather than
  trusting it, alongside asserting no mark kept a hard-coded fill. Shipping devicon's
  *font* instead was rejected on size: 1.5 MB for 21 glyphs against a 181 KB extension.
  Drift's `Sync all` is a synthetic first tree row present **only** while there is
  actionable drift, so "the list is empty" and "there is nothing to press" are one
  statement; it reindexes before deleting, so a failure or a declined confirm still
  leaves the index better off. Its explanatory prose lives in `viewsWelcome` and not in
  `TreeView.message`, which VS Code renders *instead of* the welcome view when the tree
  is empty — the message is therefore set only once a check has produced rows.
  **A reindex must show the server's claims, not just its own upload, and that is a
  correctness matter rather than a nicety.** `post_index` swallows the indexing-claim
  conflict (`Err(ApiError::FileInFlight) => {}`, see **Management endpoints**) and still
  answers 200 with the claimed file *absent from the response* — which is byte-for-byte
  how a hash-skipped file comes back. A client cannot tell the two apart from `/index`
  alone, so the extension reported a refused reindex as `unchanged`, finished in
  milliseconds, and looked like it had done nothing. It is now read from two places
  neither of which is that response: `/status`'s `indexing_claims` drives a live row in
  the Drift view and *refuses* to start an upload that would be swallowed, and the
  follow-up `/drift`'s `indexing` bucket is what the summary subtracts to say "still
  indexing" instead of "unchanged" — so the drift check must run **before** the summary,
  not after. The status poll drops to 3 s while claims are outstanding, since a count
  that only moves every 30 s reads as wedged; the configured interval stays the ceiling.
  Every entry point funnels through the one `reindex()` helper, which is also what makes
  its re-entry guard total — two concurrent runs over the same paths raced their own
  drift checks and could settle showing just-indexed files as stale.
- MCP `scout` (`tools/mcp/scout/`): token-economy layer, and one tool — `research`,
  a thin SSE client over `POST /v0/{guid}/research`. The whole investigation runs on
  the server's local model, so scout itself holds no prompt, no chunk budget and no
  Ollama connection; what it owns is the *reader* (`_STEP_KEYS`, `_USAGE_KEYS`,
  `_CITATION_KEYS` — whitelists that silently drop unknown fields, so an SSE change
  that skips them fails by going quiet) and the `_INSTRUCTIONS` that tell the caller
  to trust the report but check `citations.unverified_paths` and `done_reason`, and
  to chain a follow-up by passing the previous `run_id` in `context_run_ids` rather
  than re-investigating from cold. The
  **cheap-breadth half**; `mindex.search` is the paid-precision half. Fully removable
  layer.
- `.mindex` (repo-root, committed — index scope is part of the project): **YAML**,
  required `guid:` (either UUID spelling, normalized to hyphenated) + optional
  `exclude_paths:`/`include_paths:`/`languages:`/`git_refs:` **lists**. Unknown key = error
  (`deny_unknown_fields`), scalar-instead-of-list = error: a mistyped
  `exclude_path:` would otherwise index the tree it was meant to keep out. One file,
  repo root, no nesting. **`tools/mindexfile` is the only Rust parser** (indexer +
  watcher path-depend on it); `tools/vscode/src/mindexFile.ts` mirrors it, and the
  post-commit hook parses nothing (it shells out to `mindex-index --print-guid`).
  Adding a fourth parser is the mistake this crate exists to prevent. The extension
  also *writes* one (`tools/vscode/src/mindexTemplate.ts`, from the Drift view's
  no-project welcome button) — whose header comment restates the schema in prose, so
  it drifts silently the way a parser cannot: a stale comment still produces a file
  that parses. Keep it in step with `mindexfile` alongside the parser. Its exclude
  list is deliberately thin — only root dot-dirs that are unambiguously tool/VCS
  state are active; build artifacts and every other dot-dir ship commented out,
  because a wrong guess shrinks the index with no error, and excludes are applied
  *before* includes so a blanket rule cannot be carved back open. Globs are
  root-relative, forward-slash, `*` stopping at `/` (`globset` needs
  `literal_separator(true)` for that; picomatch does it by default) — the shared
  fixture table in `mindexfile`'s tests and `globContract.test.ts` pins the subset,
  and divergence there surfaces as permanent phantom drift, not an error. The MCP
  servers don't parse it — the agent reads it and passes GUID + filters as call args.

## Docker & CI

- Toolchain pinned 1.95 (`libsqlite3-sys 0.38` needs ≥1.87). `cargo-chef` is **not**
  used (needed 1.88+, conflicted) — layer caching is `cargo fetch --locked` over a
  stub `src/main.rs`. Legacy builder supported; no `--mount=type=cache`.
- Three compose files, same `Dockerfile`:
  - **Prod** (`docker-compose.yml`): two services, qdrant + mindex. The perf harness
    is *not* one of them — it is host-side scripts in `perf/` that drive this stack
    (`command:` flags read env; swap profiles via `--env-file perf/env/<f>.env`).
    **No host ports**; outbound-only `extra_hosts: host.docker.internal:host-gateway`
    reaches the host-run embedder (`:11211`, deliberately not composed — ~8 GB torch
    deps). TOML-only knobs (`[limits]`, `search.max_*`) require mounting a
    `config.toml`.
  - **Exposed overlay** (`docker-compose.exposed.yml`): opt-in, passed explicitly
    with `-f`; publishes API (`11111`) + Qdrant dashboard (`6333`) on `127.0.0.1`
    only (neither has auth). The sanctioned way to open the stack.
  - **Test** (`docker-compose.test.yml`): qdrant + mock-embedder + mindex +
    test-runner. Run with `--exit-code-from test-runner --abort-on-container-exit`.
    Healthchecks use `/dev/tcp` / `urllib` (no curl in images). Mounts
    `tests/integration/mindex-test-config.toml` (small caps) so limit tests can
    exercise edge rejections — those knobs are TOML-only. **Edit
    `v1.0.0_schema.sql` and you must `down -v` before the next run**: the migration
    filter is `version > user_version`, so a volume already stamped at 1 skips the
    edited schema in silence and every request that touches a new column 500s with
    `no such column`. That is the price of editing the schema in place instead of
    appending a migration, and it is only paid pre-release.

## Tests

- **Unit**: `cargo test --bin mindex`; each `tools/` crate carries its own. Read the
  test files for coverage — highlights: the connection-leak and GC orphan-prevention
  regressions, the `codes_are_stable` contract snapshot, trigger-level illegal
  transitions, `sweep_candidates` selection rules. No server/Docker; some slicer
  tests need the BGE-M3 tokenizer in the HF cache (a fake-`Tokenizing` test avoids
  it).
- **Integration** (`tests/integration/`, pytest in Docker): mock embedder returns
  deterministic vectors seeded by text hash (stable ranking assertions). Fresh
  project GUID per test. Suites map by filename (`test_e2e`, `test_filters…`,
  `test_management`, `test_validation`, `test_concurrency`).

## Linting (zero warnings everywhere — non-default flags matter)

- Rust: `cargo clippy --bin mindex` + `cargo clippy` in each `tools/` crate (own
  workspaces), and `cargo fmt --check` in each — all four crates are edition 2024,
  where `collapsible_if` fires on the `if let` + `if` nesting that let-chains
  replace, and rustfmt's 2024 style edition sorts imports differently.
- VS Code (`tools/vscode`): `npm run check` = prettier + eslint + `tsc` + the
  `node --test` suite (`src/*.test.ts`, compiled to `dist/`).
- Shell: `shellcheck scripts/entrypoint.sh`, `shellcheck --shell=bash
  tools/search/mindex-search.sh`; format `shfmt -i 4 -ci` (bare shfmt defaults to
  tabs).
- Python (`tests/`): `ruff check`, `ruff format --check` **and** `black --check`
  (kept compatible), `mypy` (`fastapi` is `# type: ignore` — stubs only in the
  mock's image). Run mypy **per directory** — `mypy tests/` fails before checking
  anything with `Duplicate module named "main"`, since `tests/mock_embedder/` and
  `tests/mock_ollama/` each define one:
  `for d in tests/integration tests/mock_embedder tests/mock_ollama; do mypy $d; done`.
- Python (MCP servers): the same four, per server —
  `(cd tools/mcp/scout && ruff check . && ruff format --check . && black --check . && mypy src)`,
  likewise for `tools/mcp/mindex`. Easy to forget: neither is under `tests/`.
- SQL: `sqlfluff lint src/db/migrations/` (dialect/layout from repo-root `.sqlfluff`;
  schema is intentionally column-aligned).
- Prefer a scoped `#[allow(...)]`/config exclusion **with a reason** over contorting
  code; never project-wide suppression.

## When modifying code

1. New loops touching Qdrant/SQLite/embedder must respect the `CancellationToken`.
2. Multi-row DB writes go inside a `transaction`.
3. New endpoints: register in `backend::http3::run`, use `RouterState`, `{param}`
   routes, `#[debug_handler]`, the `ApiJson`/`ApiPath`/`ApiQuery` extractors, return
   `Result<_, ApiError>`, validate at the top via `backend::v0::validate` (new check
   = new `ApiError` variant + its arms + `codes_are_stable` + a unit test). Add a
   `#[utoipa::path]` annotation (existing tag, every error `body = ProblemDetails`,
   a `**Concurrency:**` note) **and** an entry in `openapi.rs` `paths(...)` (+ new
   types in `schemas(...)`) — a handler missing there is silently absent from
   Swagger; the `openapi_spec_is_complete_and_versioned` test guards the count.
   Swagger UI at `/swagger-ui` (assets vendored, no network at build/runtime).
4. Reach Qdrant only via `VectorStore`; collection names via `collection_for`.
5. Any search-path SQLite query must include `AND c.status = 'active'`.
6. Status writes use `set_file_status` and must be a legal transition (triggers
   enforce it). New status-changing paths need a transition test.
7. Adding a language → the full checklist under **Languages**.
8. Schema change → new migration in the `MIGRATIONS` slice with the next sequential
   version; startup applies those above `PRAGMA user_version`, then stamps it. All
   SQL `IF NOT EXISTS` (cold re-run = no-op, enforced by
   `every_migration_sql_is_idempotent`). SQLite can't `ALTER` a `CHECK` onto an
   existing table — add new constraints as `BEFORE INSERT/UPDATE` triggers (the
   pattern the status-machine and shape-validation triggers set, additive, no volume
   drop). New *columns* are equally
   blocked: `ADD COLUMN` has no `IF NOT EXISTS` form, so it fails the idempotency
   test. **v1.0.0 is frozen** —
   its schema is in use, so an in-place edit is skipped in silence on any database
   already stamped at 1 (`version > user_version`), and the first symptom is a 500
   with `no such table`/`no such column` on the request that needs it. New *tables*
   are the easy case: `v1.1.0_git_history.sql` adds two, and
   the upgrade is verified non-destructive on a copy of a real database.
   **Widening an existing constraint, and adding a column, are the cases none of
   that covers**, and both are answered by the same table rebuild —
   `v1.1.0_toml_yaml_languages.sql` is the precedent: a trigger cannot loosen a
   CHECK that is already in the table text, so the table is rebuilt. Copy its
   shape rather than re-deriving it — create the replacement under a temporary
   name, copy the rows with the columns **named** (`SELECT *` binds by position),
   `DROP` the original, then rename; recreate its triggers, which the `DROP` took
   with it. It runs under `SQLite3Pool::migration_transaction` because both halves
   need foreign keys suspended (a rename-first ordering makes the children follow
   the discarded table, and `ON DELETE RESTRICT` refuses the `DROP`), and
   `apply_pending_migrations` pays that back with one `PRAGMA foreign_key_check`
   before it stamps `user_version`. Idempotency comes from the leading
   `DROP TABLE IF EXISTS <tmp>`: a second run rebuilds again, which costs a copy
   and changes nothing. Rehearse it on a copy of a real database and compare row
   counts per table, not just that it ran.
   **A 1:1 side table is not the answer for a new field**, though `ADD COLUMN`
   being blocked makes it look like one: the three that existed were folded back
   into their parents before release precisely because each cost a JOIN on a hot
   path, and reintroducing one would undo that for the convenience of the
   migration rather than of the reader. `v1.2.0_research_context.sql` is the
   precedent — it rebuilds `research_runs` to add three columns.
   One consequence of the rebuild is worth knowing before it surprises someone:
   the FK suspension also suspends `ON DELETE CASCADE`, so dropping the old table
   does **not** take a child table's rows with it, and `id` surviving the copy is
   what makes them still resolve. That is load-bearing rather than lucky, and
   `rebuilding_research_runs_keeps_the_baselines_that_reference_it` pins it —
   applied through an ordinary transaction instead, the same migration silently
   erases every child row.
9. Changing how chunks or symbols are derived → bump the matching const under
   **Derivation versions**. That is what makes the change reach files already
   indexed; skipping it leaves them stale behind a matching hash, silently.
10. Changing which files a project contains, how a path is spelled, what bytes are
    hashed, or which files a client refuses to post → **the full list under Four
    clients, one working-tree view**, in the same commit. One client changed alone
    is not a smaller version of the change; it is phantom drift.
