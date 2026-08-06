# Changelog

All notable changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every component ships under one version: the server, `mindex-index`, `mindex-watch`,
the `.mindex` parser, both MCP servers and the VS Code extension. A component with no
changes of its own is still released, so "which version am I running" has one answer.

## [2.0.0] — 2026-08-07

**Retrieval v3.** Replaces the three-headed BGE-M3 pipeline with **one dense leg from a
registry of Qwen3-Embedding models**, served by any OpenAI-compatible endpoint. This is
the largest breaking change the project has made, and it is not a refactor: the vectors
are from a different model, so **every index must be rebuilt from scratch**.

The grounds are measured, not architectural. `bench/` (new in this cycle) is a
pre-registered retrieval-quality harness — ground truth built from each project's own
Sphinx documentation and resolved by AST against the source tree, so no model and no LLM
touches the answer key.

**The headline is the two shipped systems against each other**, both driven through
`POST /v0/{guid}/search`, same corpus, same queries, same cutoff:

| | v3 | v2 | Δ | 95% CI | p |
|---|---|---|---|---|---|
| django docs, n = 1 115 | **0.4563** | 0.3549 | **+0.1014** | [+0.0832, +0.1190] | 0.0001 |

Three qualifications, because the number is worth less without them. It is **one
corpus** — Python, documentation prose, and there is no v3 run on the second one. It is
a **system** comparison, so the embedder, the chunk window (512 → 364) and the tokenizer
that measures it all move together and no part of the gain is attributable to one of
them. And the gain is **not** confined to the easy stratum: `obvious` +0.1343, `mixed`
+0.0728 with an interval clear of zero, `non-obvious` +0.0375 with an interval through
it at n = 148.

What the extra heads were worth, separately: equal-weight RRF over the dense and sparse
legs scored **below the single dense leg it fused** (0.4164 against 0.4448), and the
sparse leg contributed +0.004 with both intervals through zero once the dense leg was a
2026 encoder. The late-interaction rerank **significantly harmed** long queries (−0.016,
p = 0.023) and was never established either way on short ones — a comparison the corpus
is underpowered for by 3× — while costing 99.6% of Qdrant's storage (838 MB per segment
against 2.6 MB dense). The embedder was the lever; the extra heads were not.

Two things the harness does **not** establish, stated here rather than left to be found:
the model that ships is not the one the model comparison selected — granite-r2 was
statistically indistinguishable and cheaper, and Qwen3 was chosen for multilingual
queries and its one-tokenizer size ladder, neither of which is measured here — and the
364 chunk window comes from an exploratory sweep, on one corpus, under the *previous*
tokenizer. Full record, including the corrections made to it before this release:
`bench/PROTOCOL.md` §11–§12, `bench/FINDINGS.md` §11, `docs/claude/retrieval-v2.md`,
`docs/claude/qdrant.md`.

**Three version numbers now read as one, and they are independent.** The product is
**2.0.0**, the SQLite baseline is `v2.0.0_schema.sql`, and the Qdrant collection suffix
is `_v3`. The coincidence is new and accidental: the schema filename was already v2 when
the product was 1.2.0, and the collection layout was already v3. None of them predicts
another — a future release can bump the product without touching either, and
`GET /version` reports `db_schema_version` as the migration integer (`1`) rather than as
any of these strings.

### Upgrading — REQUIRED, and it is a rebuild

**1. The database lineage restarts.** The v1 schema carried `model_id` in seven primary
keys; v3 removes it (a file, a chunk and a symbol are facts about the working tree —
the *model* varies over the artifact instead). Rather than migrate rows whose vectors no
longer exist, mindex **refuses to start** on a pre-v2 database, names the file and tells
you to delete it. Fresh databases are unaffected. The Docker test stack needs
`down -v` once.

**2. The embedder changes process, and mindex stops shipping one.** `embedder/` — the
vendored BGE-M3 server — is deleted; it existed only because no general server emitted
three heads at once. What replaces it is a contract rather than a process:
`deploy/embedder/` documents the three endpoints mindex needs, three recipes (a ~200-line
torch reference server that ships with it, llama.cpp, and vLLM) with their measured
throughput, and the three checks worth running. One of those checks is
not optional: **pooling and normalisation cannot be verified over the wire**, and
Qwen3-Embedding pools the *last* token — mean pooling returns 1024 plausible numbers and
simply retrieves worse, with no error anywhere.

**3. Config keys changed, and stale ones now fail at startup** (`deny_unknown_fields`,
deliberately):

| key | change |
|---|---|
| `[model].name` | **renamed** to `[model].id`; value is a registry id (`qwen3-embedding-0.6b` \| `-4b` \| `-8b`), not an HF repo |
| `[model].served_name` | **new**, optional — for a server started with `--served-model-name` |
| `[model].server_url` | port unchanged (`:11211`), but what answers it is not — an OpenAI-compatible server instead of the vendored one's binary `/encode` |
| `[model].max_429_retries` | unchanged, but now also covers **503** — the other spelling of "busy" |
| `[qdrant].dense_prefetch_limit` | **removed** (no prefetch) |
| `[qdrant].sparse_prefetch_limit` | **removed** (no sparse leg) |
| `[qdrant].fusion_limit` | **removed** (no fusion) |
| `[qdrant].search_hnsw_ef` | kept; the successor rule is `>= [search].max_top_k` |
| `[indexing].sparse_min_weight` | **removed** (no sparse weights) |
| `[slicer].max_chunk_tokens` | default 512 → **364** (exploratory: +0.0108, p = 0.030, one corpus, measured under the *previous* tokenizer — `bench/PROTOCOL.md` §12.10. The only hard ceiling is now the model's own 32k context.) |

**4. Rebuild.** `mindex-index --force` per project, then drop the `_v1`/`_v2` Qdrant
collections the startup log names. Runbook: `docs/claude/qdrant.md`.

**Choose the serving stack by the indexing number, not by the protocol.** They differ by
an order of magnitude for identical vectors: on the reference host this repository
reindexes in **51 s** through `deploy/embedder/server.py` (torch) and **410 s** through
llama.cpp, while *query* latency is 16 ms against 30 ms. No llama.cpp configuration
recovers it — `-np` 1/8/32, three ubatch sizes, ROCm and Vulkan, and 1/4/8 concurrent
clients were all measured. `[model].query_server_url` is the seam for serving the two
paths from different processes. Numbers, method and the three traps that cost real
debugging (bf16 vs fp16 NaN, token-budget batching, `empty_cache()` corrupting output on
ROCm) are in `deploy/embedder/README.md`.

### Added

- **A model registry** (`src/models/registry.rs`): three canonical ids with their
  width, context, collection slug, tokenizer and query prefix, cross-checked at startup
  against an `embedding_models` table whose ids and dims are `CHECK`ed and append-only.
  A rebuilt binary cannot silently reinterpret stored vectors.
- **Per-model collections**: `{guid}_{slug}_v3`. Switching `[model].id` writes a new
  collection and *holds* the old one — the stale worker reports it as `OtherModel`
  (registered, not active) rather than orphaned, so switching back is instant reuse.
  Deleting a project drops every model's collection.
- **`mindex-index --vectors-only`** (body flag `vectors_only`): re-embed the stored
  chunks into the active model's collection — no slicing, no symbols. This is what a
  model *size* change costs, since all three Qwen3 sizes share one tokenizer. Refused
  across a tokenizer change, and mutually exclusive with `--symbols-only`.
- **A model-identity handshake.** `GET /v1/models` is checked at startup (a server that
  answers and names a different model is a **refusal**; an unreachable one is only a
  warning) and re-checked by `GET /health`, per instance when the query path is split.
  Every embedding row is checked against the registry's dimension. Nothing checked
  either before — a wrong embedder behind the right URL indexed in silence.
- **Two new `project_files` columns**, `chunker_id` and `embedded_model_id`, folded into
  the unchanged-file predicate beside the derivation versions: flipping the model
  self-heals exactly like a version bump. `project_file_chunks` now stores each chunk's
  `tokens`, so a future window can be checked against the corpus by SQL.
- **`GET /config`** publishes `embedding_dim`, `min_chunk_tokens` and
  `max_chunk_tokens`; `model_id` is the canonical registry id.
- **`bench/` — a pre-registered retrieval-quality harness**, and the evidence for
  everything above. `PROTOCOL.md` is the pre-registration: metrics, statistical
  procedure, the non-inferiority margin, stopping rules and threats to validity,
  committed before any number existed, with every later deviation recorded as a dated
  amendment in §11. Ground truth is each project's own Sphinx documentation resolved by
  AST against the source tree, so no model and no LLM touches the answer key.
  `FINDINGS.md` is the working narrative, including a section of ten errors made while
  building it. Neither claims more than it measured, and `FINDINGS.md` opens with the
  list of what the harness does not establish.
- **`bench/published/` — the artefacts behind every number quoted**, 132 KB of summary
  and paired-comparison JSON plus a sha256 manifest of the query sets. The runs
  themselves are 654 MB and are not committed, which for one release meant that nothing
  a reader could check was: `PROTOCOL.md` §5.6's "a corpus is frozen when its output is
  committed" pointed at no committed output. `bench/publish.py --check` fails on a
  number whose artefact no longer produces it, on a citation with no artefact, and on an
  artefact nothing cites.

### Removed

- `embedder/` (the whole vendored server), `src/models/bge_m3.rs` and its binary wire
  protocol, the sparse and ColBERT vectors, the RRF prefetch tree, the ColBERT rerank
  and the fp16 / `on_disk` / `hnsw m=0` tuning that existed for it, the
  `STORABLE_TOKENS_CEILING` chain (a Qdrant multivector limit), the six v1 migrations,
  and `model_id` from seven tables.

### Fixed

- **A client that asked for frames with `Accept: text/event-stream` was answered with
  one JSON body, silently.** `POST /v0/{guid}/research`, its `/challenge` sibling and
  `POST /v0/{guid}/index` now read that header as the second spelling of
  `?stream=yes`. Making frames opt-in (1.2.0) was right — a caller that does not read
  the stream to `done` cancels the run and spends the whole budget for nothing — but
  the query was made the *only* way to ask, which turned the same defect around and
  aimed it at the caller who had done everything right: its frame parser found nothing
  in the body, so a finished run read as one that never terminated, with no error
  anywhere. 1.2.0's own upgrade note called that symptom "unambiguous rather than
  subtle", and it is neither. Every pre-2.0.0 client hits it, `curl -N -H 'Accept:
  text/event-stream'` hits it, and the VS Code extension could not be upgraded past
  it because the release page carried no `.vsix` (below).

  One predicate decides for all three endpoints (`models::wants_stream`), so they
  cannot drift. `?stream=` still wins whenever it is present, in **both** directions —
  an explicit `no` is a decision a header may not override — and a wildcard `*/*` is
  **not** a request for frames, since that is what Swagger UI, a browser and every
  default HTTP client send while wanting exactly the JSON mode. Nothing about a run
  changes: same loop, same budgets, same journal row.
- **The v2.0.0 release page was published with no assets at all**, and nothing in the
  release workflow could notice. Every upload job is independent, so the run's own
  conclusion says nothing about whether the page is complete — the same blind spot
  that left `mindex-cli-x86_64-apple-darwin.tar.gz` missing from v1.1.0 and v1.2.0
  while `README.md` promised it. A final `verify` job now reads the release itself and
  fails when a promised artefact is absent. It runs under `if: always()`, because a
  job whose dependencies were *cancelled* is skipped rather than failed — and
  queued-then-cancelled is precisely the observed failure: on the v2.0.0 rebuild five
  jobs were evicted after fifteen minutes without ever being assigned a runner.

  The page was empty for a reason outside this repository — GitHub Actions was in a
  major incident from 2026-08-06T15:22Z with webhook delivery throttled to about 15%,
  so the tag push never triggered a run at all. What is ours is that nothing said so.
- **The Windows CLI archive stopped being built**, and `verify` caught it on its first
  production run. The `Build` step declared no `shell:`, so on the Windows runner it
  ran under PowerShell, which does not read `\` as a line continuation — a ParserError
  before cargo was invoked. The continuations arrived together with `--target` in the
  commit that fixed the macOS gap, so that fix broke this archive in the same breath:
  `mindex-cli-x86_64-pc-windows-msvc.zip` shipped in v1.1.0 and v1.2.0 and was absent
  from the v2.0.0 rebuild, with five sibling jobs succeeding around it.
- **Every research turn logged the same immutable fact.** `effective_num_ctx` emitted
  `Model's context window exceeds [research].max_num_ctx_tokens; capping` on each
  call, though the `/api/show` answer behind it is cached per model per process — one
  `medium` run wrote it seventeen times and buried the lines that do move. The window
  is now announced once, on the pass that establishes it.
- **`/llms.txt` described a retrieval pipeline this server does not have.** The
  discovery document told every agent that reads it that `POST /v0/{guid}/search` is
  "hybrid semantic + lexical retrieval", and carried a "Measured retrieval property"
  claiming identifier queries rank implementation chunks first. The first has been
  false since v3 removed the sparse leg — search is one dense vector and one Qdrant
  query, with no lexical matching anywhere in it — and the second was never measured
  by anything in `bench/results/`. Both misdirect the one reader this endpoint has:
  an agent choosing a tool. A caller told search is lexical does not reach for
  `symbols` or `grep`, which are what actually match a string. Replaced with what the
  architecture entails, and guarded by `llms_doc_makes_no_unmeasured_retrieval_claim`
  — the three existing `llms_doc` tests check routes, provenance and register, and
  none of them can see a well-formed sentence that is simply untrue.
- **The reference embedder's non-finite recovery was a no-op.** On a NaN row,
  `deploy/embedder/server.py` "recomputed in float32" by passing `precision="float32"`
  to `SentenceTransformer.encode` — which selects the *output* quantization and is
  already the default — inside a `torch.autocast(enabled=False)` that does nothing for
  natively bf16 parameters. It recomputed at exactly the precision that had just failed,
  so the deterministic outcome was a second NaN and a 500. It now casts the module and
  restores it in a `finally`.
- **That server also answered `/health` 200 while the model was loading**, for up to the
  five minutes a cold load takes — so mindex's handshake passed and `checks.embedder`
  read `"ok"` while every `/v1/embeddings` answered 503, and files burned their retry
  budget against a server they had been told was healthy. It is 503 until the model is
  in. Its startup hook is a lifespan handler for the same reason: the deprecated
  `@app.on_event` would, on removal, leave `_model` None forever with nothing saying so.
  Its `torch.cuda.*` calls are resolved per device, so `MINDEX_EMBED_DEVICE=xpu` and
  `=cpu` no longer `AttributeError` on the OOM path.
- **The published error-code catalogue was missing four codes.**
  `openapi.rs`'s `info.description` is documented as the field clients localize against
  and lists every code; `index.file_in_flight`, `auth.route_not_configured`,
  `validation.index_modes_exclusive` and `research.invented` were absent, each added by a
  change that updated `codes_are_stable` beside it and did not know the prose existed.
  Guarded now by `the_published_error_catalogue_names_every_code`.
- **The perf harness pointed at a port nothing serves** (`11212` in all three
  `perf/env/*.env`, against `11211` everywhere else), recorded eight columns from the
  deleted `/stats` endpoint as permanent `NA` while two plots charted them and the tuning
  method taught a third, and sent no `Authorization` header at all — so against the
  deployment shape this project calls mandatory behind a gateway, every run landed in
  `err_other` with an empty CSV.
- **The dashboard had no panel for `mindex_stale_collections` or
  `mindex_orphaned_collections`** — the two gauges `README.md` names as worth an alert on
  day one, and the only signal that a schema-version or model change has left a project's
  search silently broken. Eight Overview panels and three contention-guard panels added,
  plus `deploy/grafana/gen_alerts.py`, which generates eight provisioning rules (its
  output is not committed: Grafana binds each rule to a per-installation datasource uid).
- **Instructions that named deleted files**: `deploy/systemd/README.md`'s install block
  enabled a template that is not a template and lives in another directory; the migration
  precedents in `.claude/CLAUDE.md`, `docs/claude/vscode.md` and an assertion message in
  `models.rs` told a contributor to copy v1 migrations that no longer exist;
  `docs/claude/vscode.md` turned out to end in a truncated verbatim copy of CLAUDE.md's
  lint matrix and all ten modification rules; `README.md` promised a `SHA256SUMS` where
  the workflow emits `.sha256` sidecars.
- **The release workflow published no Docker image**, although the README calls Docker
  the supported way to run the server anywhere but Linux x86-64, and **no embedder**,
  although mindex now ships none and cannot start without one. Both are jobs.
- **`mindex-cli-x86_64-apple-darwin` was missing from 1.1.0 and 1.2.0**, while the README
  promised "macOS (Intel and Apple silicon)". Its `macos-13` runner is retired, so the job
  was never picked up, queued for GitHub's 24-hour maximum and was auto-cancelled — and
  because the other five jobs succeeded and uploaded, the only symptom was the run's
  overall conclusion reading `cancelled` a day after anyone was looking. Both macOS
  targets now build on `macos-14`, the Intel one as a cross-compile, which is safe for
  two pure-Rust binaries and fails in minutes rather than queueing for a day if it ever
  is not. `--target` is passed on every target besides: without it cargo built for the
  host and the archive named `mindex-cli-<triple>` carried whatever the runner happened
  to be — a label rather than a fact, correct only by coincidence.

## [1.2.0] — 2026-08-04

Gives the server **one credential that says what it may do and who holds it**, and
retires the shared API key a gateway used to check. Makes the service discoverable by an
agent handed nothing but a URL, including the harnesses that will not read prose off the
network. Stops `POST /research` charging a caller a whole GPU budget for not having
stayed to read the answer.

### Upgrading — REQUIRED if this server is reachable through a gateway

**`[auth].enabled = true` is mandatory behind `deploy/gate/`.** The gateway admits on the
*presence* of a Bearer-shaped header — nginx cannot verify a signature and does not try —
so every question of validity, scope and action is answered in the server. With
authorization off, `Authorization: Bearer x` is admitted and served everything.
`enabled = false` now means exactly one thing: a server on a trusted network that
authorizes nothing (the Docker test stack, a loopback-only install). It remains the
default, and an auth-off deployment is byte-for-byte what it was.

Migrate in this order, which is the only safe one: **turn `[auth]` on first, then switch
the gateway map.** While mindex checked nothing, the API key was the only thing guarding
the remote path; nothing breaks at the second step that the first did not already break,
since a caller holding only the old key is refused by mindex the moment authorization
comes on.

```sh
mindex mint-token --sub alice --project '*' --can search,research,index --days 30
```

**The shared `X-Api-Key` is gone rather than deprecated.** Two credentials where one is
strictly stronger is not defence in depth — the weaker one sets the floor, and that one
carried no scope, no expiry and no way to withdraw a single holder. Remove it from every
client; nothing reads it.

**A metrics scraper needs a credential of its own.** `/metrics` is `admin`-scoped, which
is structural rather than a choice: nothing about it is per-project, and mindex cannot
tell a loopback scraper from a request that arrived through a gateway. So enabling
`[auth]` blanks every dashboard until the scrape carries a token. Two details worth
stating, because both fail in the direction that looks like it worked: Prometheus's
`bearer_token_file` must point **outside `$HOME`** (a unit running `ProtectHome=true`
reads a path there as absent, and the symptom is a scrape that stops rather than a
permissions message), and `--days 0` with **its own `--key-id`** is right here and almost
nowhere else — an expiry would blank the dashboards at an hour nobody is watching, and a
separate key id makes withdrawal one table deleted from the key file rather than a
rotation that logs out every other client.

There is **no denylist, by design**: revocation is expiry or deleting a `kid`. That is
the per-request server-side state this design removes, and `--key-id … --new-key` is what
makes per-holder ids one flag rather than hand-edited base64.

### Upgrading — a caller that wants research frames must now ask for them

`POST /v0/{guid}/research` and `POST /v0/{guid}/research/{run_id}/challenge` answer
**one JSON body** unless the request carries `?stream=yes`. Both shipped clients already
do: `tools/mcp/scout` and the VS Code extension were changed in the same commit, so an
upgrade is invisible from either. A hand-written SSE client is what has to change, and
the symptom if it does not is unambiguous rather than subtle — the body parses as zero
frames, so a run reads as one that never terminated.

The failure shapes move with the default, and only because they can. With no stream open
there is no "after the status was already 200", so what would have been an `error` frame
is a problem+json status: **503** `ollama.unavailable` (unreachable or mute), **503**
`ollama.error` (Ollama answering *with* an error — nearly always a model that is not
pulled) and **500** `research.no_report`. Under `?stream=yes` all three remain `error`
events on a 200, byte-for-byte as before.

One operational consequence: without frames the whole run is one silent request, so any
intermediary between caller and server must tolerate `worst_case_seconds` of quiet —
`GET /config` publishes it per effort level, and for `high` it is over an hour.

### Added

- **`[auth]` — opt-in authorization, one HMAC and no server-side state.** HS256, written
  here rather than taken from a crate, so every copy of the secret is owned (no `Debug`,
  zeroized, key file 0600 created with `O_EXCL`) and algorithm confusion is closed by
  construction: `verify` reads `kid` and nothing else before checking the MAC, pinned by
  `the_algorithm_header_cannot_select_the_algorithm`. The TLS key is not reused. Keys:
  `enabled`, `signing_key_file`, `max_token_days`, `leeway_seconds`.
- **The token is the mapping, and there is no schema change.** `prj` (dashless GUIDs, or
  exactly `["*"]`, which must be *spelled* — an empty list reaches nothing) and `act`
  (`search`/`research`/`index`/`delete`/`admin`/`mint`) are signed into it. The rejected
  alternative was a `tenant_id` column, and it cost a table rebuild, a trigger, an
  in-process cache, a startup warm, a rule for pre-existing rows and an in-transaction
  re-read — none of which survives, along with one bug class: only a caller whose token
  already names a GUID can create that project, so `POST /index` stops being an existence
  oracle.
- **`POST /auth/tokens`** — mint a narrower token from the one presented. A minted token
  can never exceed its minter in actions, projects or expiry; without that, a read-only
  `mint` credential becomes `admin` one call later. `--days 0` mints a non-expiring token
  and **only the local CLI may**: a network-reachable way to issue an eternal credential
  is a different and worse thing, so the endpoint refuses it.
- **`mindex mint-token`** — `--sub`, `--project`, `--can`, `--for`, `--days`,
  `--key-id`, `--new-key`. Its whole output is one credential on stdout; logs go to
  stderr so a redirect cannot mix them.
- **`aud` labels the kind of holder a token is for (`--for cli,vscode,agent`), and it is
  the one claim nothing in the server reads.** No part of an HTTP request identifies the
  process behind it, so a server-side check would be theatre. The **clients** refuse
  instead — `mindexfile::token::audience_refusal` for the Rust CLIs, `token.ts`'s
  `audienceRefusal` for the extension (overridable through a modal). It stops the editor's
  credential landing in a shell profile; it stops no attacker. Absent or empty means every
  audience. Containment deliberately does not cover it: audience is not authority, and
  binding it would refuse the VS Code button minting an `agent` token from a `vscode` one.
- **Four ways to give a client the token, first wins:** `--token` > `$MINDEX_TOKEN` >
  `$MINDEX_TOKEN_FILE` (a path to a 0600 file) > `token` in `indexer.toml`/`watcher.toml`
  > the per-server entry in `~/.config/mindex/credentials.toml`. `MINDEX_TOKEN_FILE`
  exists for a caller configured by an environment block inside somebody else's config
  file — an MCP server list lives in an editor's own JSON, where a token sits in plaintext
  under no permission check and a path does not. Its trap is the precedence: a shell
  exporting `MINDEX_TOKEN` passes it to every child, so such a block must also set
  `MINDEX_TOKEN=""`.
- **The VS Code extension keeps its own copy in `SecretStorage`, and can issue tokens.**
  Not because a keychain answers "who holds the credential" — it cannot, the CLI needs the
  same kind and no shell can read it — but because the alternative *inside the extension*
  was a settings string, which Settings Sync copies to every other machine. It watches
  `exp` and warns in the status bar, verifying nothing: a client asserting validity would
  claim a fact only the server establishes. `mindex.mintAgentToken` derives a
  project-scoped token capped at seven days and labelled `agent`, reachable from three
  surfaces (the Ask view's title bar, the Server Status header, and a `command:` link in
  the token indicator's tooltip) because a capability behind a palette title nobody knows
  is one that does not exist. The issued token goes to the clipboard, and `Show it` opens
  a **read-only in-memory document** rather than an untitled buffer — one accidental
  `Ctrl+S` from the credential on disk.
- **`GET /.well-known/mindex.json`** — the machine twin of `/llms.txt`: service
  identity, the full endpoint inventory (method, path, one-line summary, which ones
  stream and in what encoding) and the live `GET /config` snapshot inlined, so
  bootstrapping costs one request. The endpoint inventory is derived from the OpenAPI
  spec rather than hand-written, and three tests pin it against the spec and against
  the router's own `.route(...)` table. Documented in OpenAPI, unlike `/llms.txt` and
  `/metrics` — it is JSON for a client, which is what the spec is for.

### Changed

- **An out-of-scope project answers 404 `project.not_found`, byte-identical to one that
  never existed.** A distinguishable refusal confirms which GUIDs exist, and a GUID is a
  bearer identifier — so `auth.forbidden` cannot exist on that path however much better it
  would read in a log. The missing *action* **is** named (403): the caller has already
  proved it holds the project. Pinned on response bytes rather than on status.
- **Two enforcement layers, deliberately overlapping.** Typed scope extractors are the
  mechanism — a type is what a source-text guard can see — and they check `covers(guid)`
  **then** `permits(action)`, in that order, so a caller that cannot see a project learns
  nothing about the action vocabulary. `enforce_route_policy` is the runtime half and
  **fails closed**: a routed path with no `ROUTE_POLICY` row is refused, not served.
  `ROUTE_POLICY` names every route, with three guards, one of which drives the whole
  refusal table so it stays exhaustive as routes are added.
- **Five routes are public and each says why:** `/health` and `/version` are liveness — a
  probe needing a credential reports the credential's health and not the server's — and
  `/config`, `/llms.txt` and the descriptor are discovery, which cannot be discovered from
  behind a credential. `admin` covers `/gc`, `/status` and `/metrics`; there is no `gc`
  action, because `POST /gc` holds the process-wide guard and walks every collection, so a
  project list cannot describe it.
- **The gate now refuses legibly instead of invisibly.** A keyless request to a route that
  exists gets **401** with `WWW-Authenticate` and a problem+json body naming `/llms.txt`
  and the routes served without a token — the same envelope and shape of machine `code`
  mindex uses everywhere else, so a client parsing its errors needs no special case for
  the gateway's. `/.env`, `/js/config.js` and `/` still get 444, and a banned address gets
  444 whatever it asked for. `444` bought invisibility and paid for it with legibility: an
  empty reply is indistinguishable from a dead host, so a correctly refused agent reported
  the deployment as broken rather than as needing a credential. That was worth it while
  the host answered nothing at all; it stopped being worth it once five routes went
  public and the descriptor began listing every endpoint.
- **The gate's keyless set is now `ROUTE_POLICY`'s public set** (GET and HEAD, anchored so
  `/configuration` is not `/config`). Opening only one of the five added no security — it
  moved the refusal from the server, which answers 200 to all five by design, to a
  boundary that answers nothing, and the cost fell entirely on callers doing the right
  thing. Measured over three hours: one configured client produced 840 closed connections
  on `GET /health`, and an agent that followed `/llms.txt` to `/config` met the same wall
  and was then banned for doing what the document it had just read told it to do — 1065 of
  1168 log lines the jail called attacks. The public surface also carries
  `X-Robots-Tag: noindex`; nothing there is secret, but a search result is a directory
  entry for scanners.
- **The fail2ban jail counts a keyless 401 on `key_ok:0` alone.** mindex issues its own
  401s and 403s for a token that is real but expired or scoped elsewhere, and those carry
  `key_ok:1`: they are the signature of a client to re-credential, not an attacker to ban.
  Banning them turns one stale editor into an outage that also swallows its own `/health`.
- **`/llms.txt` is rewritten to argue rather than order.** The document addressed the
  model in the second person and told it what to do — which is the prompt-injection
  signature, and GitHub Copilot refused to read it on a corporate machine, leaving that
  agent with no entry point at all. Every recommendation now carries its reason and the
  reader is "a caller"; nothing operational was dropped, and the document leads with the
  problem the service solves rather than with a list of endpoints. Pinned by
  `llms_doc_avoids_the_injection_signature`. A refusal is now survivable in any case,
  since the JSON descriptor above carries the same discovery data.
- `UNDOCUMENTED_ROUTES` in `http3.rs` is the single list of routes deliberately outside
  the spec; the `/llms.txt` route guard and the descriptor read it instead of holding a
  copy each.
- **Research streams are opt-in, and the safe behaviour is what you get by not asking.**
  Frames were compulsory, which made *reading the response to the end* compulsory too,
  since a disconnect cancels the run. So a caller that issued the request and did not
  stay spent the whole budget, received nothing, and raised no error anywhere — the
  expensive failure, and the silent one. `/index` had adopted the opposite default long
  before; research now matches it. Both entrances read the query through one
  `launch_research_job`, so `?stream=yes` cannot come to mean one thing on a research run
  and another on a challenge: everything above the spawn — pre-flight refusals, the
  permit, the minted `run_id`, the registry entry, the `started` frame — is identical, and
  only the tail forks.
- **The JSON body is a transcription of the stream, not a second contract.** Every field
  of `ResearchResponse` is the `data()` of the frame with the same name, produced by the
  same code: report, `summary`, `citations`, `excerpts`, a challenge's `verdict`, and the
  `done` payload. It omits `thinking`, `step` and `progress`, which exist to be watched —
  the step count rides on `done` and the trace is journalled in `research_run_steps`.
  `the_json_body_carries_the_frames_the_stream_would_have_sent` asserts against `data()`
  rather than against literals, precisely so the body cannot drift into being a fifth copy
  of the SSE contract.
- **Cancel-on-disconnect is unchanged, by a second hand rather than by the stream's.** In
  SSE mode `SseEventStream`'s `Drop` still cancels the job token; in JSON mode a
  `CancellationGuard` held across the drain does the same when axum drops the handler
  future. So the new default removes the obligation to keep reading, and not the rule.
  `DELETE /research/active/{run_id}` remains the answer for a caller that abandoned a run
  while its socket stayed open.
- `ollama.unavailable`, `ollama.error` and `research.no_report` are now real `ApiError`
  variants instead of string literals in `research.rs`. The crossing lives in
  `ApiError::from_research_failure` alone, and it matches on strings — where a missing arm
  would still answer, as a well-formed 500 carrying the wrong code. `FAILURE_CODES` beside
  `ResearchAbort` plus `every_research_failure_code_rebuilds_itself` is what makes adding a
  failure code fail loudly instead.
- A stream that dies without a terminal event was already synthesised into one `error`;
  the JSON collector now raises that same synthesised failure as a **500** rather than a
  200 with an empty report. Both spell it with one sentence, `abnormal_end`.
- `?stream=` is `deny_unknown_fields` and accepts only `yes`/`no`: `?stream=true` or a
  typo'd key is a 400, never a silent fall-through to the default — which for this
  endpoint would cost a caller an hour of GPU before it noticed.
- `tools/mcp/scout` and the VS Code research panel both request `?stream=yes` explicitly,
  each with the reason recorded in its own file. scout's is not a preference: its
  `READ_TIMEOUT` is an *idle* clock resting on the server's 15 s keep-alive and is the only
  thing there separating a working server from a wedged one, and its partial-report salvage
  (`truncated_by_client`, `live_run_id`, `still_running`) works only because bytes arrive as
  they are produced.

### Removed

- **The shared `X-Api-Key`.** See the first Upgrading section — it is gone, not
  deprecated. A token is worth pasting into a context because it is narrow, and a
  deployment demanding the API key *beside* it would put the shared secret back into that
  same context, which is the leak the token closes.
- **`mindex.browseResearch`.** One reading surface for stored research, not two;
  `ctrl+alt+,` opens the panel.

### Fixed

- **`mindex-watch` could not be built for Windows.** `tokio::signal::unix` is absent
  there, not empty, so the unconditional import broke the build before any `cfg` could
  help; the SIGTERM handler is `#[cfg(unix)]` now. Ctrl+C was already registered
  separately and tokio maps a Windows service stop onto it, so no platform loses a
  shutdown path. The tool had been Linux-only by accident since it was written, and
  nothing said so because nothing had ever built it elsewhere — the release workflow is
  what found it.
- **The gate's ban list was tested before its keyless exception**, so a ban closed
  `/llms.txt` — the one URI served without a credential, whose whole job is to tell an
  unauthenticated caller that it needs one. Fetch the published document, ask for
  `/favicon.ico` nine times as browsers do, trip the jail on those 444s, and the ban
  swallows the document; from outside that is indistinguishable from a dead host. Both
  fail2ban files now live in `deploy/gate/` beside the nginx one they are half of.
- **Three service sandbox protections read as armed and were inert.** `IPAddressDeny=`,
  `SocketBindDeny=` and `RestrictNetworkInterfaces=` are enforced by BPF a `--user`
  manager cannot load, while `systemd-analyze` scores the unit as confined. Measured: a
  user unit denying everything but localhost fetched `example.com` and got 200. All three
  services move to system units, which also makes their ordering real rather than lucky.
- **VS Code: an action that succeeded left the screen describing the old world.** The
  garbage-collection review survived its own delete — the deleted reports stayed on
  screen, still ticked, under a `Delete N` that would re-post ids the server had already
  dropped. Six more of the same class were found by walking the destructive paths: the
  review's `read` link destroyed the screen it was being read for; `pin` wrote only the
  rendered page, so the delete dialog read a pre-pin copy; a finished run reached the
  panel through nothing at all, leaving its subject still wearing the trust badge it had
  before it was refuted; a deleted run stayed attached to the Ask form as context; and
  `cancelIndexing` refreshed the drift check but not the poll `reindex` refuses on — so
  the user cancelled and then could not do the thing they had cancelled in order to do.
  None of these surface as an error. They surface as a screen that disagrees with itself.
- **VS Code: a page rebuilt mid-run drew Submit enabled and Stop hidden.** The Ask view is
  registered without `retainContextWhenHidden`, so collapsing the sidebar destroys it, and
  `running` was not among the state replayed into the rebuilt page — pressing Submit then
  earned "cancel it first" while the only control that could cancel was the one missing.
  `AskFormState` holds what outlives the page; `mindex.cancelResearch` covers a sidebar
  closed rather than hidden.
- **VS Code: 401 and 403 fell through to the generic error branch**, so an expired
  credential surfaced only as requests that quietly stopped working — exactly the state
  the token indicator exists to prevent and cannot detect for an opaque token. 401 now
  offers Set Token and 403 leads with the missing action.
- **VS Code: the token reason could outrank a server that could serve nothing.** A user
  whose Qdrant was down and whose token lacked `research` was told to re-mint a credential
  that was never the problem. The token reason now wins only while `health.ask` holds — a
  dependency comes back by itself and a missing action does not, which stops being the
  useful ordering once the server can serve nothing at all.
- **`.dockerignore` said `target/`, which matches only the repository root**, so every
  build shipped `tools/{indexer,watcher,mindexfile}/target` to the daemon — 2.2 GB of
  context per build on a machine that had run the test matrix.
- **The documented stale-collection symptom was the rarer of two.** A
  `COLLECTION_SCHEMA_VERSION` bump leaves search silent only once something has indexed
  the project since; a project nobody has touched has no collection at all and `/search`
  answers **503 `qdrant.unavailable`**, which reads as Qdrant being down rather than as an
  index that needs rebuilding. Observed on the maintainer's host during the v1 → v2
  upgrade, with `mindex_stale_collections` sitting at 1 and naming the project. Both
  halves are written down now, because an operator who has read only the silent one goes
  looking for a network fault.

## [1.1.0] — 2026-08-03

Gives research reports an **opponent** and an **offline re-verification**, writes them
**section by section** so a run that runs out of time still returns findings, makes
`GET /health` **tri-state** so a client stops guessing which dependency matters,
publishes the whole workflow at **`/llms.txt`**, and halves the on-disk size of a
collection. Withdraws the `callers` tool and the reference half of the symbol table,
which is the one removal here. A broad stability pass gave every wait a number.

### Upgrading — REQUIRED. Until you do this, search returns nothing and says nothing

`COLLECTION_SCHEMA_VERSION` moved `v1` → `v2`, and it is **not self-healing**: the new
name names no collection while SQLite still reports every file `indexed`. Search then
fails in one of two ways, and which one you get is not a distinction worth relying on:

- If nothing has indexed the project since the upgrade, the collection does not exist and
  `/search` answers **`503 qdrant.unavailable`** — which reads as an infrastructure
  problem rather than as "this project needs reindexing".
- If anything *has* touched it, `ensure_project` will have created an **empty** collection
  and search comes back empty **with no error anywhere** — the worse of the two, because
  nothing anywhere says the index is gone.

1. Stop the server.
2. Start it once. Migrations 5 and 6 apply in place.
3. **Reindex every project**: `mindex-index --root <repo> --force`. This also rebuilds
   the symbol table, which migration 6 needs (`SYMBOLS_DERIVATION_VERSION` is `1.1`).
4. **Drop the leftover `*_v1` Qdrant collections by hand.** They still hold the whole
   pre-upgrade index and nothing reaches them.

Step 4 is deliberately not automated: leaving the old collections in place is what makes
a rollback possible. New in this release, `worker::stale` runs at startup and hourly and
publishes `mindex_stale_collections` and `mindex_orphaned_collections`, so the state
between steps 2 and 4 is visible rather than silent. Both gauges are seeded at `-1`,
never `0` — `0` is the healthy reading and an unreachable Qdrant must not be able to
spell it. The runbook is in `docs/claude/qdrant.md`.

Two smaller notes: **`role:` on `POST /v0/{guid}/symbols` is now a `400`**, not an
ignored field (see below); and `PROMPT_VERSION` is `2.7`, so reports written before this
release are not directly comparable — partition a stored corpus on it.

### Added

- **Reports have an opponent.** `POST /v0/{guid}/research/{run_id}/challenge` is a
  research run whose subject is a stored report: same loop, same semaphore, same
  budgets, its own citation gate. The subject is injected as hearsay under examination
  and **never seeds the evidence** — re-deriving every location through the tools *is*
  the refutation. A closing verdict turn scores each claim `CONFIRMED` / `DISPUTED` /
  `REFUTED`, and the stream carries one new event, `verdict`, between `excerpts` and
  `done`; an ordinary run's stream is byte-for-byte unchanged.
  - **The grounding cap is what makes this safe with a weak model, and it is
    symmetric.** A challenge is `grounded` when it verified at least one citation and
    its unverified citations do not outnumber them. An **ungrounded `refuted` caps at
    `disputed`** — an accusation that showed no code refutes nothing — and an ungrounded
    `confirmed` resolves to NULL, because an unshown *acquittal* is not one either. One
    surviving citation out of nine was enough to launder a challenge that had checked
    nothing; that is measured, not hypothetical.
  - Zero parseable verdict lines is NULL: **challenged, inconclusive**, which no reader
    may render as an acquittal.
  - **Trust is derived at read time, never stored** — over *valid* challenges only, so a
    challenge whose own evidence goes stale stops counting by itself. Severity wins
    (`refuted` > `disputed` > `confirmed` > `unchallenged`). One challenge stands per
    report: a newer one **with a parseable verdict** evicts the older, inside the same
    transaction as its own insert. An inconclusive run evicts nothing — it produced no
    finding, and letting it erase a `refuted` would spend the mechanism's most valuable
    output on its least informative outcome.
  - Refused when the subject is invalid (`400 research.challenge_subject_invalid` —
    staleness must not be spendable as refutation) or is itself a challenge
    (`research.challenge_subject_is_challenge`; trust aggregation is single-level).
- **Offline re-verification.** `GET /projects/{guid}/research/{run_id}/verification`
  re-runs the citation check as a pure function over journal rows — no model, no GPU.
  It answers two questions and keeps them **separate**: *provenance* is immutable and
  must match the recorded counters, so a mismatch is a journal bug and never news about
  the code; *staleness* is computed against the index as it stands now and is the number
  that actually moves. Nothing is stamped, so it can never disagree with a
  recomputation.
- **Migration 5** (`v1.3.0_research_verification.sql`) makes the run journal structured:
  `research_run_evidence` (the shown spans — the one `check_citations` input that
  otherwise died with the run, and what makes the offline check possible),
  `research_run_citations` (per-occurrence verdicts in report order) and
  `research_run_steps` (calls, arguments and landing spans; no result bodies — the code
  is in the index). Trace rows are built at the same sites as the `step` SSE frames from
  the same locals, so wire and journal cannot drift.
- **`GET /llms.txt`** — the whole workflow as one document, so pointing an agent at a
  URL is enough to get it started. Static narrative plus a live section rendered from
  the same snapshot `GET /config` serves: available models, the effort ladder, and the
  **measured** p50/p90 per model and effort. Absent data is stated as absent rather than
  papered over with an invented value. Deliberately outside the OpenAPI spec, and a test
  asserts the absence so the omission reads as a decision.
- **A report of three or more plan items is written one section at a time.** The report
  used to be a single turn, so a model that could not produce it produced *nothing* — a
  fifteen-minute run returning zero. Each numbered sub-question now gets its own turn,
  the server assembles the document, and a section that fails costs that section rather
  than the document. Below three plan items the run takes the old single-turn path
  byte-for-byte, which is both the safety valve and the revert switch.
- **Checkpoints make a stopped run return findings.** `[research].checkpoint_every_steps`
  (6; `0` disables) interrupts the tool loop to bank the sections already answerable.
  A section that cannot be written later ships its banked version, and a forced synthesis
  assembles real findings instead of "No report was produced." It costs a step, so it is
  visible in the operator's budget, and is capped so a mis-set interval cannot eat a run.
- **A request can shape the report.** `max_report_sections`, `max_report_words`,
  `checkpoint_every_steps` and `evidence_width` join the per-axis budget overrides, each
  capped by a new `[research]` ceiling that config validation refuses to set below what
  `effort.high` already grants. `evidence_width` is one integer multiplier on how much a
  single lookup returns; it deliberately does not scale navigation tools or `search`.
- **A run is named from admission and listed while it runs.** `run_id` is minted before
  the work starts, streamed as the first frame, and registered — so `GET /research/active`
  lists live runs and `DELETE /research/active/{run_id}` cancels one whose caller
  abandoned it without closing the socket. Before this a run had no id until it ended:
  with `max_concurrent = 1` an occupied slot was an unattributable total outage whose
  only remedy was a restart. A `429` now names the endpoint that explains it.
- **`GET /health` is tri-state, and the server owns the verdict.** `ok`; `degraded`,
  meaning only the **optional** Ollama is failing — exactly the state where a client
  should keep offering search and stop offering research; `unhealthy`, meaning a
  required check failed or a research run is wedged. Severity wins, so Ollama down *and*
  Qdrant down is `unhealthy`. Two words rather than three was the defect: every client
  then needed its own copy of which check is required, and the VS Code extension's did
  not match the server's. `checks.*` is now exactly `"ok"` or `"error"` — the reason a
  probe failed goes to a log with a sysadmin hint, because this response is readable by
  anything that can reach the port and a driver's error chain carries paths, URLs and
  versions. Clients must test `== "ok"`, never a prefix.
- **`[research].allowed_models`**, a glob whitelist compiled once at startup (empty =
  any). Checked before the semaphore, so a refused model costs no slot; `GET /config`
  publishes the model list already filtered by it, plus the raw patterns.
- **A toolless model is refused before it costs anything.** `/api/show`'s `capabilities`
  is now read and cached alongside the context length, and a model that does not declare
  `tools` is a `400` at admission instead of a slot, a model load and a wasted turn. It
  is three-valued on purpose: only an explicit "no" refuses, since a pre-flight that
  cannot be performed is not a refusal.
- **Three contention guards, all shipping armed.** `[research].max_turn_seconds` (300)
  abandons a turn that is still producing but has consumed a whole run's wall clock —
  measured twice on one host, 985 s at ~1.5 tok/s and 912 s for 702 tokens, each time a
  single plan turn eating the run. `slow_turn_tokens_per_second` and
  `slow_turn_unaccounted_ms` warn without stopping anything; they are **independent**,
  and neither may gate the other, because the second exists precisely for the case the
  first cannot see (time spent queueing behind another client falls outside Ollama's own
  accounting entirely). Ollama's `load_duration` / `eval_duration` / `total_duration`
  were previously parsed by nobody; they now ride on `progress` and `done` as
  `generation_ms` / `model_load_ms` / `unaccounted_ms` / `eval_tokens_per_second`.
- **`GET /config` publishes what a run costs, not only what it grants.** `observed` is
  measured p50/p90 per `(model, effort)` from the journal, and a pair with too few runs
  is absent rather than noisy. Also `worst_case_seconds` per level, since `max_seconds`
  and the report window bound *different phases* and reading the first as the whole wait
  understated `high` by five minutes.
- **A step reports where it landed** (`spans`, `path:start-end`), from the same locations
  citation provenance is scored against. `hits: 3` on a 4000-line file named no lines,
  which made the trace unusable for the only thing it is for.
- **Collection metrics.** `mindex_stale_collections` and `mindex_orphaned_collections`
  (see Upgrading), `mindex_project_vectors` (Qdrant's own point count per project — the
  only detector for a lost vector volume, and a project the store cannot answer for is
  *absent*, not zero), `mindex_search_orphaned_winners`, `mindex_search_unscorable_winners`,
  `mindex_worker_running` / `mindex_worker_exits_total`, and
  `mindex_research_unjournalled_runs` — the denominator every research rate lacked, since
  the three endings that write no row were absent from every per-run metric at once.
- **VS Code — Research History is now the one reading surface**, an editor-area panel
  with a challenge launcher, kind/trust badges linking challenge to subject, a Verify
  action for the offline re-check, an active-runs picker, and a garbage-collection review
  that proposes invalid, stale, partial and inconclusive runs (pinned exempt) with the
  reasons visible. `mindex.browseResearch` is gone; `ctrl+alt+,` opens the panel.
- **VS Code — `.gitignore` writes the excludes it already knows.** Creating a `.mindex`
  translates every `.gitignore` in the project, nested ones included, naming the file
  each block came from and commenting out what it cannot express rather than guessing.
- **VS Code — three new settings**: `mindex.requestTimeoutSeconds`,
  `mindex.streamIdleTimeoutSeconds` (an **idle** clock, never a total one — a `high` run
  may legitimately live 70 minutes) and `mindex.indexingPanel`.
- **Prebuilt binaries.** `mindex-index` and `mindex-watch` for Linux, Windows and macOS
  (Intel and Apple silicon), the server for Linux x86-64, and the `.vsix`, all built on
  native runners by a new release workflow. `mindex-watch`'s filesystem watching has only
  ever been exercised on Linux inotify.

### Changed

- **The ColBERT vector is stored as fp16, with no HNSW graph.** Measured on this repo's
  own index, ColBERT was **99.6% of the collection's bytes** — 838 MB per segment against
  2.6 MB dense and 0.5 MB sparse, roughly 1.85 MB per chunk. `datatype: Float16` halves
  that and is not a quality trade the way quantization would be: this vector only
  *orders* a pool dense and sparse already agreed on. `hnsw_config.m = 0` builds no graph
  at all, which is correct only because ColBERT is always the outer query over a prefetch
  pool and never an entry point. This is what `COLLECTION_SCHEMA_VERSION` `v2` is.
- **`callers` is withdrawn, along with the reference half of the symbol table and the
  repo map that ranked by it.** The reference rows were measured rather than assumed:
  23 810 of them against 3 397 definitions — **87.5% of the table** — serving one
  model-facing tool called **twice** across twenty-five recorded research runs at a 50%
  miss rate. The edges were lexical, so the most-referenced names here were `assert_eq`
  (1084), `clone`, `Ok`, `unwrap`, `map`, several with exactly one definition in the
  tree. Separating a core abstraction from a name shared with a language builtin is name
  resolution, which is the wall this project declines to climb. `grep` answers "who uses
  this name" lexically **and says so**, which is the honest version of what `callers`
  implied. `parent_name`/`parent_kind` survive — for a definition, the enclosing
  definition is what makes `Gc::collect` readable.
  - **Migration 6** (`v1.4.0_symbol_definitions.sql`) drops `project_file_symbols.role`,
    now that every value is `'definition'`. It does not delete the reference rows:
    symbol rows are wholly derived, so `SYMBOLS_DERIVATION_VERSION` `1.0` → `1.1` removes
    them on the next indexing run, keeping the rule in one place.
  - **`POST /v0/{guid}/symbols` now rejects `role:` with a `400`.** Accepting and
    ignoring it would answer a `role: "reference"` query with the *definitions* — the one
    wrong answer that costs nothing to detect and looks exactly like a right one.
- **A score that cannot be compared ranks last, not first.** `total_cmp` orders `+NaN`
  above every finite value, so the plain descending sort handed the **top result slot** to
  a chunk the reranker could not score. NaN results are ranked last rather than dropped —
  the chunk really did match the filters — and counted, because the symptom otherwise
  reads as a ranking-quality complaint rather than the misconfigured embedder it is.
- **Every wait has a number, and it is the server's, not the library's.**
  `[qdrant].timeout_ms` / `connect_timeout_ms` exist because the client's own default was
  5 s and no knob reached it: a project whose rerank ran past that failed **every** search,
  untunably. `[model].encode_timeout_ms` now bounds the whole call rather than each
  attempt — per attempt the worst case was forty minutes at the defaults. `GET /health`
  runs its probes concurrently under a fixed 3 s ceiling; they were sequential, and the
  SQLite one was the file's only transaction without a cancellation token, so a wedged
  pool hung the one endpoint that must always answer. Shutdown now drains for 8 s instead
  of logging "Shutdown complete." while in-flight batches were torn out mid-flight.
- **HTTP/3 now streams frame by frame.** The response body was buffered whole before the
  first send, so `/index?stream=yes` and `/research` did not stream over h3 at all — the
  client saw nothing until the run ended, up to seventy minutes, while the server
  accumulated every event in memory. Both endpoints exist *because* their output is worth
  watching arrive.
- **A prose tool call is detected in two notations.** A model that calls tools natively
  all run long can still write markup on the report turn, where no tools are passed;
  JSON-only detection let that through every gate meant to catch it, and a run shipped and
  journalled a "report" whose whole body was three fake calls.
- **A cited path is resolved before it is scored.** A cited path may be the unambiguous
  tail of exactly one shown path; two candidates resolve to none. The failure this fixes
  is not a parser gap but `unverified` — the verdict for a path no tool returned — being
  handed to a report about a file it had just read, five times in one run. `citations`
  gained `path_resolved` as the honesty counter, plus `shown_paths` (how many files the
  run saw the *inside* of) and `server_written`, because a forced-synthesis report cites
  nothing by construction and scored byte-for-byte what a clean report scores.
- **A run that looked at nothing while holding hearsay is refused.** The ungrounded gate
  exempts a run with nothing to say; a run handed prior reports or a challenge subject has
  somebody else's answer to hand, and the same uncited prose is then that answer restated
  as findings, in the field callers are told to trust.
- **Research metrics are charted with `increase()`, and the report phase is charted at
  all.** The Grafana dashboard gained the report-phase row, which is where runs actually
  fail; rare histograms are drawn as points with gaps kept.
- **VS Code — a degradation freezes the form and never the tabs.** Both mode buttons stay
  live in every state: a disabled tab is a dead end whose explanation lives behind it.
  Every server-touching button single-flights, supersedable for reads and refused for
  writes. No raw error reaches a user — one funnel, machine codes to the log.
- `mindex-watch` is described as a filesystem watcher rather than an inotify daemon, since
  it is now released for three platforms.

### Fixed

- **Recovery ran under the request's own token, so it was a no-op in the one case it
  exists for.** The pool short-circuits on a cancelled token *before* touching the
  database, so a cancelled or disconnected request left every prepared file `indexing`
  until the 30-minute stuck-grace sweep. The unit test passed a fresh token and so agreed
  with the bug.
- **A failed status write read as a success.** `set_file_status` returns a `bool` covering
  a DB error, a trigger rejection and a 0-row update, and it was discarded: the retry
  worker reported `"indexed"` on the strength of a write it never checked, so a database
  that had stopped accepting writes kept a clean success rate while every file stayed
  stuck. It is `#[must_use]` now, and the legitimate discards are written as such with
  their reason.
- **The retry worker inferred "no chunks" from a failed read.** Behind an
  `unwrap_or_default()`, a `PoolEmpty` or a locked database silently promoted files to
  permanently-indexed-and-empty, at `info!`. A read that fails now leaves the file for the
  next sweep.
- **A crash read as the client hanging up.** A panicked pool task became `Cancelled`,
  which told the client it had closed a connection it never closed, told the dashboard a
  disconnect, and silenced every call site's log — while permanently costing a pool
  connection. It is its own variant now, counted as `outcome="panic"`; after
  `db_pool_size` of them every request failed with `database.busy` and nothing said why.
  The reachable instance was `grep` slicing a string with an offset found in its
  lowercased copy (`İ` grows by a byte), so any indexed file containing one made `grep` a
  way to dismantle the pool four requests at a time.
- **A dead background worker was invisible.** Workers were bare spawns with the handle
  dropped, so a panic stopped GC or the retry sweep permanently and in silence —
  indistinguishable from a healthy idle system. They are supervised now, publishing
  `worker_running` *before* the task starts, because a series that never existed cannot be
  alerted on.
- **`PoolEmpty` was an unretryable-looking 500 that produced no log line at all** — the
  likeliest production failure, invisible. It is `503 database.busy` with a hint.
- **GC reported success for a pass that could not run.** Each phase returned a bare count
  with every error mapped to `0`, and the run counted as `ok` whenever the token was live,
  so a GC failing for days looked idle and `POST /gc` answered 200 with zeros either way.
  Phases now report whether they finished, and `failed_phases` names them on the wire.
- **A missing Qdrant collection was treated as a GC failure, which made the backlog
  permanent.** "Keep the row until the vector is confirmed gone" then meant *never*: chunk
  rows were unsweepable, their file rows unprunable behind the RESTRICT FK, and the
  backlog grew for the life of the deployment — in exactly the state a lost Qdrant volume
  leaves behind, where GC needs to work most. A missing collection is a confirmation now,
  checked only after a failure, and only a definitive answer converts.
- **`cancel_overdue` re-reported what it had already cancelled.** A run wedged in an await
  its token cannot reach stays registered, so the sweep re-cancelled, re-warned and
  re-counted it every 30 s, turning a counter documented to stay at zero into an unbounded
  number describing one event.
- **`show_facts` cached failures**, making one blip permanent for the process and silently
  running every later run of that model at the configured ceiling instead of its own
  window. Successes only, now.
- **Ollama's two failure classes are two codes.** `ollama.unavailable` is unreachable or
  reachable-and-mute; `ollama.error` is Ollama answering *with* an error, nearly always a
  model that is not pulled. Collapsed into one, a client could not word the message or
  decide whether re-reading `/health` would say anything — and for a typo, health is green
  every time.
- **A turn against a live but mute socket spent the whole budget waiting.**
  `[research].first_token_timeout_ms` (120 s) bounds the silent prefix only, armed across
  the request *and* the wait for the first delta, since Ollama holds the connection open
  while it loads a model.
- **A scope that admits no file is refused at admission.** Without it such a run refuses
  every lookup and then reports the question unanswerable, which reads as a finding about
  the code: the commonest spelling (`"src/"`, where SQLite `GLOB` wanted `"src/**"`) cost a
  measured 302-second run with zero citations and no error anywhere.
- **A report missing its heading is repaired rather than refused**, when that is the sole
  problem — and always *after* the citation check, since the derived heading comes from the
  question and a server-written line must never enter the provenance report.
- **An empty `grep` result had three meanings and one spelling.** Out-of-scope, nothing
  searchable, and genuinely absent are now distinguished, and a miss whose pattern carries
  regex punctuation says the match was literal: `\.bwp` → 0 against `.bwp` → 7 is a false
  negative the reader was handed as proof of absence.
- **`embed_and_upsert` trusted the row counts off the wire.** `zip` silently truncated a
  short response, leaving a file marked `indexed` with vectors missing and no error
  anywhere; a long one indexed out of bounds. Relatedly, a `Vec::with_capacity` from an
  unvalidated `u32` asked for ~100 GB on a corrupt body and aborted the process.
- **A section turn could ship the checkpoint's sections again, or somebody else's.** A
  reply whose numbered headings are all *other* items is not this section written badly; it
  is a second document, and one was measured shipping sections 1-2 twice.
- **Search returned `200` with an empty list when every winner was orphaned** — the
  reassuring spelling for the case that means the two stores disagree, while an over-narrow
  filter gets a `404`. It is a `404` uniformly now, and the orphans are counted.
- **VS Code: a reindex read its result from the `/index` response**, which swallows claim
  conflicts and answers `200`, so a refused reindex read as `unchanged`. It reads the
  server's claims and the follow-up drift check instead.

## [1.0.1] — 2026-07-31

Adds a second channel for research — **git history** — turns its stored reports into a
validity-tracked corpus the model and the reader can both browse, streams indexing
progress over SSE, and indexes TOML and YAML. The VS Code extension got the rest of the
attention. Also fixes an indexing failure that could take a whole pass down with it,
and a research run that could vanish without a trace.

### Added

- **Stored research is a validity-tracked knowledge graph.** Every finished run is
  browsable (`GET /projects/{guid}/research[/{run_id}]`), keeps or drops its place in
  the retention sweep (`POST …/{run_id}/pin`), and can be fed to a later question as
  prior context (`context_run_ids`). Validity is **derived at read time**, never
  stored: a run is valid when its own files are unmoved *and* every run in its
  transitive context chain still exists and is itself valid. Staleness can heal, and
  a deleted parent leaves a dangling id that the recursive CTE reads as invalid
  immediately — so there is no cascade to write and nothing to keep in step.
  Migration 4 (`v1.2.0_research_context.sql`) rebuilds `research_runs` for `seq`,
  `expires_at` and `context_run_ids_json`, and adds `research_run_files`.
- **Batch delete for stored research** — `DELETE /projects/{guid}/research` with
  `{"ids": […]}`. A corpus is pruned in handfuls, and one request per pick is one
  chance per pick to fail halfway; this is a single transaction. Unknown ids are
  ignored, so it is idempotent like the single-run delete. An **empty** list is a 400
  (`selector.empty`) rather than a whole-corpus wipe, and the batch is capped by the
  new `[limits].max_research_delete_ids` (TOML-only, default 500) — over it,
  `validation.research_delete_too_many`.
- **Two significance counters on every run summary**: `references_count` (how many
  reports this one was built on) and `referenced_by_count` (how many were built on
  it, counted across the whole corpus rather than the page). The first is
  deliberately *not* `context.length`, which is the transitive ancestry — a run built
  on one report that itself rests on three reads `1` and lists four. The second is
  what makes a delete confirmation honest: removing a run invalidates every
  descendant, and the caller is owed that number before agreeing.
- **VS Code — research is a popup-first surface.** An `Add…` button beside the
  context chips opens a QuickPick over the stored corpus, and it is visible in
  Research mode whether or not anything is picked; while the block was hidden until
  it had contents, the feature could only be found by already knowing about the
  History panel. `Browse Stored Research` is the single-select twin for reading.
  Stored reports open as read-only Markdown documents in their own tab (scheme
  `mindex-research`, rendered by VS Code's own preview) with a provenance header, so
  a report can sit beside the code it describes. A live run's header lists the
  reports it was built on as clickable chips. Research History gained batch delete,
  an `Ask again` action, visible retention (`pinned` / `3d left`), and both counters.

- **`POST /v0/{guid}/index?stream=yes` streams indexing progress as SSE.** The
  default (`stream=no` or absent) keeps the one-shot JSON summary byte-for-byte, and
  both modes run the identical pipeline — the query parameter only decides how the
  result travels. The stream reports `started`, per-file `prepared`/`skipped`
  (`unchanged`/`in_flight`/`cancelled`), one `embedded` per GPU embed batch with
  cumulative `chunks_done`/`chunks_total` and the server's own `elapsed_ms`, per-file
  `indexed`, then exactly one terminal `done` (whose `files` is the JSON mode's
  response body) or `error` (the stable `ApiError` code, since the HTTP status is
  already 200 by then). Closing the connection cancels the request and recovers the
  batch exactly as a dropped JSON request would. A mistyped `?stream=` value or key
  is a 400, never a silent fall-through to the mode the caller did not ask for.
- **`mindex-index` progress is now a measurement, not an estimate.** The bar consumes
  the SSE events: the file counter advances as the server settles each file instead
  of once per 100-file batch, the status line names the file being worked on, and
  chunks-per-second is computed over a 20-second sliding window fed by the per-batch
  `embedded` events — the old figure was a cumulative average over a counter that
  jumped once per batch response. Against an older server the client detects the
  plain-JSON answer and degrades to exactly the previous behaviour.
- **VS Code — live reindex progress.** The reindex notification and the Drift view's
  progress row now show `settled/total files · N chunks/s · <path>` from the same
  stream, updating per file and per embed batch (throttled) instead of once per
  batch. The Drift row's tooltip no longer has to disclaim that a posted batch may
  still be on the GPU — a file now counts only once it is settled.

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
- `PROMPT_VERSION` is `1.3`. Research reports written under 1.0.0 and 1.0.1 were
  written under different instructions and are not directly comparable; if you are
  keeping a corpus, partition on it.

### Fixed

- **A research run whose report contained a non-ASCII character next to a citation
  was lost outright.** `parse_citations` walks backwards from `:<digits>-<digits>` to
  collect the path and did it by slicing the report at a **byte** index, which panics
  when that byte falls inside a multi-byte character. `gpt-oss:20b` writes
  OpenAI-style `【…】` citation brackets; one landing before a line range killed the
  job thread, so no `done` event reached the client, `journal.record` was never
  called, and the run left no row and no error — the report streamed to the screen and
  then simply did not exist. Russian or any other non-ASCII prose in the report does
  the same. The walk is over bytes now, which is exactly equivalent because the path
  character class is ASCII-only. As a side effect the bracketed form parses correctly
  instead of crashing.
- **An SSE stream that ended without a terminal event looked like success.** Both
  `/research` and `/index?stream=yes` spawn their job detached, so a panic aborts the
  task, drops the sender and closes the channel — which is byte-for-byte what a
  completed stream looks like to every consumer. The stream now tracks whether a
  `done`/`error` went through and synthesises one `error` (`internal.error`) when the
  channel closes without one. No new event name and no new code, so the SSE contract
  and its consumers are unchanged.
- **A run the server did not save now says so.** `done.run_id` has always been null
  when the best-effort journal write failed or the report failed the Markdown gate,
  and no surface rendered it — so a report that would never appear in Research
  History was indistinguishable from one that would. The VS Code panel now warns
  above the report, while the text is still there to copy.
- **VS Code: deleting the open report left it rendered.** The Research History
  preview kept showing a run that no longer existed, with the selection pointing at a
  dead id. The pane resets, and it now remembers which report was open across a
  reload the way the query and the selection already did.
- **`selector.empty` named a field the request did not have.** The batch research
  delete reuses the rule, but its selector is `ids`, not `include`/`exclude` — so the
  error pointed a client at a field that does not exist in that body. The code is
  unchanged (the rule is one rule); the `field` pointer and the detail now name
  whichever selector the endpoint actually takes.
- **The Grafana dashboard's whole Research row read as empty while the runs were
  there.** A labelled metric family is created by the first event carrying its label
  set, so its first scraped sample is already `1` — there is no preceding zero for
  `rate()` to subtract from, and `research_runs{model, done_reason}` normally sees
  exactly one run per label set per process lifetime, so the series sat flat at 1
  until restart and every panel built on `rate()` drew zero. The metrics themselves
  were correct throughout, and high-traffic families hid the defect because their
  second event arrives seconds after their first. Research counters and per-run
  histograms are now charted with `increase()`, which counts a new series' first
  sample, and drawn as bars with a `sum` legend rather than as a per-second line;
  quantiles over a rare histogram are drawn as points with the gaps kept. The
  "Unverified citation share" stat gained `or vector(0)` — the healthy case is that
  no `unverified` series exists, and "No data" is indistinguishable from a broken
  query.
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
- **A Windows clone reported permanent drift.** Git's default `core.autocrlf=true`
  rewrites the working tree to CRLF on checkout, and mindex hashes `code.as_bytes()` —
  so every client saw every file as changed, reindexed the whole tree on every check,
  and nothing errored anywhere. `.gitattributes` now declares `* text=auto eol=lf`
  repo-wide, which overrides the setting rather than leaving it to each contributor,
  and a minimal `.editorconfig` keeps an editor from putting CRLF back.

### Upgrading

- **The database migrates in place**, on the next start. Migration 2 is additive — two
  new tables — and was verified non-destructive against a copy of a real 1.0.0
  database. Migrations 3 and 4 rebuild a table each (`project_files` for the widened
  language CHECK, `research_runs` for `seq`/`expires_at`/`context_run_ids_json`),
  which SQLite cannot express as an `ALTER`; both run under
  `SQLite3Pool::migration_transaction`, which suspends foreign-key enforcement for the
  rebuild and verifies the result with `PRAGMA foreign_key_check` before stamping
  `user_version`. Nothing needs reindexing — `.toml` and `.yml` files simply appear on
  the next indexing run.
- **`.mindex` files using `git_refs:` require 1.0.1 tooling.** The parser rejects
  unknown keys by design, so a 1.0.0 `mindex-index` or `mindex-watch` will fail on
  one. Files that do not use the key are unaffected in both directions.
- The git history channel stays off until `--history` is passed.
- **An existing Windows clone must run `git add --renormalize .` once.**
  `.gitattributes` applies at checkout, so a tree already written out as CRLF stays
  that way until it is renormalised — and until then it keeps reporting drift.

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

[Unreleased]: https://github.com/silencespeakstruth/mindex/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/silencespeakstruth/mindex/compare/v1.2.0...v2.0.0
[1.2.0]: https://github.com/silencespeakstruth/mindex/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/silencespeakstruth/mindex/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/silencespeakstruth/mindex/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/silencespeakstruth/mindex/releases/tag/v1.0.0
