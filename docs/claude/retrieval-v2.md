# Retrieval v2 — one dense leg, and everything that falls away with it

> **Superseded (2026-08-05) by `retrieval-v3.md`.** The measurement record below
> (§1, §6) stands and is the evidence base; the implementation spec does not.
> What changed against this file's conclusion: the shipped family is
> **Qwen3-Embedding** (0.6B/4B/8B, operator-selectable), not granite —
> multilinguality (§6's untested Russian-query case) and the size ladder decided
> the tie that cost alone had broken, and vLLM serving reopens the throughput
> arithmetic (§6 already noted the 87 chunks/s figure was a floor). v3 also adds
> what §2 declined: a compiled model registry with canonical ids CHECKed into
> SQLite, per-model Qdrant collections, and a `vectors_only` re-embed path —
> because "a future model swap is cheap" was promoted from a property worth
> keeping to the point of the exercise.

*Companion to `.claude/CLAUDE.md`, **Retrieval pipeline**. That file holds the
invariants of the system as it stands; this one is the **implementation spec for
replacing it**, plus the measurement record that chose the replacement. Read
this before touching `db/qdrant.rs`, `embed.rs`, `models/bge_m3.rs`, the slicer
ceilings or the schema.*

*Evidence: `bench/PROTOCOL.md` §12.12 (families F5, F7 — pre-registered §5.3),
narrative in `bench/FINDINGS.md` §10. Nothing below is asserted that is not
measured there or marked as unmeasured.*

---

## 1. The decision, in one table

Measured on two corpora, identical chunk sets, every arm ranked by exact
brute-force cosine, fp16, device and dtype asserted rather than requested.

| arm | params | django nDCG@10 (n=1115) | scikit-learn (n=360) | chunks/s |
|---|---|---|---|---|
| mindex as deployed (dense+sparse RRF → ColBERT) | 568M | 0.3549 | 0.5567 | 133 |
| CodeRankEmbed (code-specialised) | 137M | 0.4060 | 0.5918 | 326 |
| **granite-embedding-english-r2** | **149M** | **0.4448** | **0.6241** | **267** |
| Qwen3-Embedding-0.6B | 595M | 0.4540 | 0.6251 | 87 |

**Ship `ibm-granite/granite-embedding-english-r2` as the single dense leg.**
Apache-2.0, 149M, **768-d**, 8192 context, **no query or document prefix of any
kind**. Against the deployed pipeline: **+0.089 (django), +0.067 (sklearn)**,
both CIs excluding zero.

Two results decide the shape, and both are the opposite of what the design
brief assumed:

**A code-specialised model is not what wins.** CodeRankEmbed is third on both
corpora; a 149M *general-purpose* encoder beats the 137M *code-specialised* one
by +0.039 (p = 0.0001) at the same size. CoIR ranks them the other way
(60.1 vs 55.3) and CORE-Bench (arXiv 2606.11864) ranks them this way. A
leaderboard ordering did not survive contact with this corpus.

**The sparse leg stops paying once the dense leg is good.** Weights chosen on
one corpus, reported on the other, both directions:

| direction | Δ vs granite alone | 95% CI | p |
|---|---|---|---|
| django → sklearn | +0.0048 | [−0.0080, +0.0186] | 0.47 |
| sklearn → django | +0.0038 | [−0.0023, +0.0099] | 0.22 |

The django interval, n = 1 115, has an **upper bound of +0.0099 — below δ = 0.01,
the smallest effect this protocol will call meaningful**. Against BGE-M3's dense
head the same sparse leg was worth +0.015: it was compensating for a weak dense
vector, not supplying a lexical signal the task needs.

And **plain RRF scores below the single leg it fuses** — 0.4164 vs 0.4448
(django), 0.6200 vs 0.6241 (sklearn) — because rank fusion is strength-blind.
Independently corroborated: on APPS code generation, hybrid RRF scored 33.54
against BM25 alone at 38.00 (arXiv 2605.14503).

**So: one dense vector per chunk. No fusion rule, no weights, no sparse head, no
ColBERT.** Qwen3-Embedding-0.6B is statistically indistinguishable from granite
on both corpora (CIs contain zero); granite is 4× smaller and measured 3× faster
on the same device, so cost decides.

---

## 2. What this removes

This is a **subtraction**, and reading it as a rewrite is the way to make it one.

| goes away | why |
|---|---|
| the `colbert` named vector | 99.6% of stored bytes, 84% of query latency, never measured to help |
| the `sparse` named vector | §1: +0.004, CI through zero, bounded by δ |
| `COLBERT_DATATYPE`, `COLBERT_HNSW_M` | with the vector |
| the RRF prefetch tree in `QdrantStore::search` | one query, one named vector, one `has_id` filter |
| `STORABLE_TOKENS_CEILING` / `MAX_STORABLE_TOKENS` | `1_048_576 / VECTOR_DIM − 2` = **1022 tokens**, a Qdrant multivector limit that exists only because ColBERT emits one row per token. Both slicer constructors `min`-clamp to it, so `[slicer].max_doc_chunk_tokens` (default 1024) is silently capped today. Replaced by a startup check against the leg's `max_seq`. |
| `model_id` from seven tables | §3 |
| `BGEm3EmbedResponse`'s three-head struct | one head |
| `[model].query_server_url`'s BGE-M3 assumptions | the seam stays; what it serves changes |

**Nothing is added except a smaller model and a smaller wire format.** The
architecture in the approved plan — N coexisting legs, `file_leg_coverage`,
calibrated weighted fusion, routing by `programming_language` — is **not built**:
it was designed to hold a sparse leg and a prose leg that the measurements then
refused to justify. Build the one-leg version; the leg registry is the thing to
reach for *if* §6's open questions ever produce a second leg that earns a slot.

---

## 3. Schema

`model_id` is in the primary key of `projects`, `project_files`,
`project_file_chunks`, `project_file_status_log`, `project_file_symbols`,
`project_commits` and `project_commit_paths`. It was a hedge on the wrong axis:
files, chunks, symbols and commits are facts about the working tree; a vector is
a derived artifact, and the model varies over the artifact. Keeping it would
mean a second copy of every file row and every chunk row *including `code`* the
first time a second model appeared — and `build_search_query`
(`handlers.rs:281-291`) joins on it without ever *filtering* by it, so both
models' chunks would enter one candidate set.

Destructive change and total data loss are authorised. So this is **not a
migration**: a new baseline `src/db/migrations/v2.0.0_schema.sql`, a new
`PRAGMA application_id`, `MIGRATIONS` restarting at 1, and a startup that
**refuses** a database of the old lineage with an instruction to delete it and
reindex rather than reading it wrongly.

```sql
PRAGMA application_id = 0x4D583033;   -- 'MX03'; old databases carry 0 and are refused

CREATE TABLE projects (
  guid       TEXT PRIMARY KEY,        -- dashless UUID. No model_id.
  created_at INTEGER NOT NULL
);

CREATE TABLE project_files (
  project_guid         TEXT NOT NULL REFERENCES projects(guid) ON DELETE CASCADE,
  path                 TEXT NOT NULL,
  sha256               TEXT NOT NULL,
  programming_language TEXT NOT NULL CHECK (...),
  status               TEXT NOT NULL CHECK (...),  -- the state machine, unchanged
  retry_count          INTEGER NOT NULL DEFAULT 0,
  status_updated_at    INTEGER NOT NULL,
  chunks_version       TEXT,
  chunker_id           TEXT,   -- NEW: the tokenizer identity, see below
  vectors_version      TEXT,   -- NEW: the embedder identity + version
  symbols_version      TEXT,
  PRIMARY KEY (project_guid, path)
);

CREATE TABLE project_file_chunks (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  project_guid TEXT NOT NULL,
  file_path    TEXT NOT NULL,
  code         TEXT NOT NULL,
  qdrant_guid  TEXT NOT NULL UNIQUE,
  start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
  start_column INTEGER NOT NULL, end_column INTEGER NOT NULL,
  status       TEXT NOT NULL CHECK (status IN ('active','deleted')),
  FOREIGN KEY (project_guid, file_path)
    REFERENCES project_files(project_guid, path) ON DELETE RESTRICT
);
```

The other five tables are rebuilt identically minus `model_id`.
`IndexClaim`'s key drops to `{guid}\0{path}`.

**Two new columns close two documented blind spots**, both of the same shape —
a hash answers "did the file change", not "did the deriving code change":

- **`chunker_id`** — `CHUNKS_DERIVATION_VERSION` cannot see the tokenizer, so a
  tokenizer change today leaves stale chunks behind a matching hash. Note the
  window is measured in *tokens* and granite's tokenizer is not BGE-M3's, so
  this changes on day one.
- **`vectors_version`** — the embedder's identity, which no version has ever
  covered. It is what makes "re-embed everything, do not re-slice" expressible:
  the chunk text is already in `project_file_chunks.code`, so a future embedder
  swap is a pure GPU pass over stored rows with no client, no tree walk and no
  drift. That is the one piece of the multi-leg design worth keeping, because it
  costs one column.

A file is current iff `sha256`, `chunks_version`, `chunker_id`,
`vectors_version` and `symbols_version` all match. One predicate drives the
hash-skip, the retry worker and the re-embed pass.

---

## 4. Qdrant

Collection `{guid_simple}_v3`, one named vector `dense`, 768-d, cosine, HNSW as
today. Qdrant 1.18 allows named vectors to be **added to and removed from an
existing collection** without recreating it — worth knowing for a future leg,
but irrelevant to this change, which discards the store anyway.

`QdrantStore::search` becomes a single `QueryPointsBuilder` with the `has_id`
filter and `.using("dense")`. The nested prefetch tree, `Fusion::Rrf` and the
ColBERT outer query all go.

**Keep `rank_by_score`.** It exists because `total_cmp` orders `+NaN` above
every finite value and handed the top slot to an unscorable chunk; the producer
was a misconfigured embedder, and a one-leg pipeline has exactly the same
exposure. `search_unscorable_winners` stays and is still expected to read zero.

Expected storage after: ~2.6 MB dense per segment against 841 MB today, and the
`[qdrant]` prefetch knobs (`dense_prefetch_limit`, `sparse_prefetch_limit`,
`fusion_limit`) lose their meaning — `search_hnsw_ef` keeps it.

---

## 5. Embedder

`embedder/src/bge_m3_api/__main__.py` hard-codes `MODEL_NAME = "BAAI/bge-m3"`
and `DENSE_DIM = 1024`, and its binary format `BM3\x01` carries three heads. A
dense-only server is **simpler than the one that exists**: one head, one array,
no sparse post-processing, no `attention_backend()` XPU NaN workaround for
padded ColBERT rows.

- New magic and format; `src/models/bge_m3.rs`'s `parse_encode_response` shrinks
  to dense.
- `--model` as an argument rather than a constant.
- granite takes **no prefix on either side** — unlike CodeRankEmbed, whose
  mandatory query prefix degrades silently when omitted. One fewer silent
  failure mode, and worth stating because the plan that preceded this named
  CodeRankEmbed.
- `Tokenizer::from_pretrained(model_id)` in `main.rs:800` follows the model, and
  that is what `chunker_id` records.

The largest activation becomes `batch × hidden` (~1.5 MiB at batch 512) instead
of ColBERT's `batch × seq × 1024 × 4B` — 4.29 GiB at batch 2048, the tensor
behind the recorded OOM and the only one that ever approached the iGPU's 4 GiB
per-allocation ceiling.

---

## 6. What is NOT settled, and must not be presented as if it were

- **Prose retrieval is unmeasured.** Every query set is derived from
  documentation and `docs/**` is *excluded from the ranking*, so gold is always
  a code file. There is no measurement anywhere of retrieval *into* markdown.
  If granite is worse than BGE-M3 on prose, a second leg and a routing rule come
  back — which is why `vectors_version` exists and why the leg design is
  recorded rather than deleted. `bench/build_docs_qrels.py` with the inverse
  exclusion is the missing corpus (family F9, declared, unrun).
- **Both corpora are Python**, and both query sets are documentation prose.
- **Identifier-heavy queries are the untested case for the sparse leg.** CoIR
  measures BM25 varying **56×** across its own datasets. The nearest stratum
  here is `obvious`, where fusion was *negative* (−0.0072) — evidence against
  the lexical leg, but on the wrong query distribution to settle it.
- **Russian queries against an English-only encoder are untested.** granite has
  a multilingual sibling at 311M, already in
  `bench/baselines/external_embedder.py::MODELS`, unrun. This matters: the
  user's own queries are often Russian, and BGE-M3 was multilingual.
- **A reranker is unmeasured on a fixed harness.** The one number that exists
  (`bge-reranker-v2-m3`, no gain, n = 118) came from a CPU run; the harness now
  pins device and dtype and reports delta-over-first-stage, but has not been
  re-run. CoREB (arXiv 2605.04615) finds every off-the-shelf reranker negative
  on at least one code task, so the prior is not favourable.
- **Throughput here is a floor, not a ceiling.** flash-attn was absent for every
  arm, so attention took the naive O(seq²) path. The 267 vs 87 chunks/s ratio is
  the comparable part; the absolute numbers are not the models' best.

---

## 7. Build order

1. **Schema** — `v2.0.0_schema.sql`, `application_id` refusal, `MIGRATIONS`
   restart, `model_id` removed from every query and struct. Largest diff,
   smallest risk, and everything else depends on it.
2. **Embedder server** — dense-only, `--model`, new wire format. Independently
   testable against the existing mock.
3. **`db/qdrant.rs`** — one named vector, single query, ColBERT consts deleted,
   `VECTOR_DIM` 1024 → 768, `{guid}_v3`.
4. **Slicer ceilings** — `STORABLE_TOKENS_CEILING` deleted, startup check
   against the leg's `max_seq`. `[slicer].max_doc_chunk_tokens` can now exceed
   1024 — a knob that was capped by ColBERT's geometry and is worth measuring
   once prose is measurable at all.
5. **`embed.rs` / `handlers.rs`** — one head, `vectors_version` stamped in the
   prepare transaction beside `chunks_version`.
6. **Reindex everything**, drop the v2 collections.
7. **Docs** — `CLAUDE.md`'s ColBERT paragraphs, its `model_id` invariants and
   `docs/claude/qdrant.md`'s non-self-healing bump runbook all describe a system
   that will no longer exist. Left stale they are worse than absent, because
   this repo's convention is that CLAUDE.md is the authority on invariants.

**Check before relying on it:** `model_id` is filled from one process-wide
constant (`main.rs:445`) and appears in no request body, no response body and no
`.mindex` key. If that holds, the **Four clients, one working-tree view** rule
is *not* triggered — `mindex-index`, `mindex-watch`, the VS Code extension and
the MCP tools need no change, the file set is unchanged, path spelling is
unchanged, hashed bytes are unchanged. One leak makes this a five-client change.

---

## 8. The bench harness, as it now stands

| path | what |
|---|---|
| `bench/ranx_bridge.py` | result JSONL → `ranx` Qrels/Run, keeping the chunk→file dedup |
| `bench/tests/test_ranx_equivalence.py` | 18 tests: `ranx` reproduces `score.py` exactly on 6 metrics over 7 archived runs, and pins three undocumented `ranx` behaviours |
| `bench/baselines/fusion.py` | chunk-level fusion over `ranx`'s 25 methods, weight search, `--train`/`--test` refuse the same corpus |
| `bench/baselines/external_embedder.py` | 5 models in `MODELS`, `--device`, device **and** dtype asserted |
| `bench/baselines/cross_encoder.py` | device/dtype asserted, delta-over-first-stage reported |
| `bench/.ruff.toml` | pins isort classification so a new sibling directory cannot reformat untouched files |

Three `ranx` properties are load-bearing and undocumented, read out of its
source and now pinned by tests: `fisher`/`student` apply **no** family-wise
correction (only `tukey` does), `n_permutations` defaults to **1000** against
the protocol's 10 000, and `ranx` computes **no confidence intervals** — so
`stats.py`'s BCa bootstrap and Holm correction cannot be retired.

One convention deliberately differs and is pinned rather than resolved:
`map@20`. `score.py` normalises AP by `min(|gold|, k)`, `ranx` by `|gold|`.
It affects one query in 1 475 — the one with 26 gold files against a cutoff of
20 — and `score.py`'s convention is the one argued for in its own docstring.
