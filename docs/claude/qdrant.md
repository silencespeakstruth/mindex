# Qdrant — what the collection costs, and what was done about it

*Companion to `.claude/CLAUDE.md`, **Retrieval pipeline**. That file holds the
invariants; this one holds the measurement record they were derived from, and the
rungs deliberately not climbed. Read this before changing `ensure_project`, the
`[qdrant]` config section, or `COLLECTION_SCHEMA_VERSION`.*

---

## The measurement (2026-08-03, this host)

Qdrant 1.18.3, store at `~/.local/share/qdrant`, two projects indexed.

Per segment of the mindex repo's own collection:

| storage | size | share |
|---|---|---|
| `vector_storage-colbert` | 838 MB | **99.6 %** |
| `vector_storage-dense` | 2.6 MB | 0.3 % |
| `vector_storage-sparse` | 0.47 MB | 0.06 % |

| | |
|---|---|
| total store | 7.9 GB |
| this repo's collection | 7.4 GB for **4009 points** |
| per chunk | **~1.85 MB** ≈ 450 token rows × 1024 dims × fp32 |
| ColBERT : dense | **322 : 1** |
| search, cold | **20.3 s** |
| search, warm | 56-87 ms |
| Qdrant RSS | 186 MB |

Two things follow from the shape of that table.

**ColBERT is the collection.** Everything else is rounding error. Any conversation
about Qdrant storage or Qdrant indexing cost in this project is a conversation
about the multivector and nothing else.

**The cold number is the multivector too.** RSS is 186 MB, so the 7.4 GB lives in
page cache, not the heap: a rerank over `fusion_limit` = 200 candidates touches
200 × 1.85 MB ≈ 370 MB of mmap'd storage. On a cold cache that is the whole 20 s.
It is not a one-off — this host runs with almost all of its 31 GiB of swap in use,
so the cache does get evicted and the cold path does recur.

## What the collection was configured with, before

Nothing. `GET /collections/<name>` reported `quantization_config: null`, no
`datatype`, no `on_disk`, `hnsw_config` at Qdrant's defaults (`m: 16,
ef_construct: 100`), an empty payload, no payload indexes, and no `SearchParams`
on any of the three query stages. Seven keys in `[qdrant]`, none of them about the
index or memory.

That is not an oversight to be embarrassed about — it is the correct starting
point, and it is why the numbers above were worth taking. It does mean every
option below was available and unpriced.

## What changed (schema `v1` → `v2`)

All three are on the ColBERT vector, and all three are `const`s in
`src/db/qdrant.rs` rather than config keys, because each is part of the
collection's schema: changing one is a reindex, and a TOML key would invite an
operator to do that by accident.

- **`datatype: Float16`.** Halves the store. Not a quality trade in the way
  quantization would be: fp16 carries ~3 decimal digits, and this vector only ever
  *orders* a pool that dense and sparse already agreed was relevant.
- **`hnsw_config: { m: 0 }`** — build no graph. Correct because of an invariant of
  `VectorStore::search`: ColBERT is always the *outer* query over a prefetch pool,
  never an entry point, so Qdrant rescores an explicit candidate list and never
  traverses a graph. Building one was pure indexing cost on the most expensive
  vector in the collection.

  **The trap:** a future query using `.using("colbert")` *without* a prefetch would
  not fail. It would silently brute-force the whole collection. If ColBERT ever
  needs to be searched directly, this goes back to a real degree and the schema
  version moves with it.
- **`on_disk: true`.** Already effectively true via mmap; the explicit flag stops a
  future Qdrant default from putting gigabytes per project back into the heap.

`dense` and `sparse` are untouched on purpose. Dense *is* ANN-searched in the
prefetch and costs 2.6 MB per segment — there is nothing there to win.

Expected result: **7.4 GB → ~3.7 GB**, ColBERT HNSW construction gone, cold-search
page-in halved.

## What changed on the query side

`[qdrant].search_hnsw_ef` (default 256), applied as `SearchParams` on the **dense**
prefetch only — sparse is served by an inverted index and ColBERT now has no graph,
so both would ignore it.

Honest caveat, repeated in the code and in `config.example.toml`: on a collection
under Qdrant's `optimizers_config.indexing_threshold` (10 000 points by default) no
HNSW index exists and the prefetch is an exact scan, so **today this changes nothing
on this repo** (4009 points). It is set now because Qdrant's implicit `ef` is 128
against a `dense_prefetch_limit` of 200 — the day a project crosses the threshold
would otherwise be the day its recall quietly drops, with no error at startup and
none at query time. Startup validation refuses `search_hnsw_ef < dense_prefetch_limit`.

## The bump's own hazard, and what now covers it

`COLLECTION_SCHEMA_VERSION` is a component of every collection's *name*. Bumping it
migrates nothing and fails nothing: the new name names no collection,
`ensure_project` makes an empty one, SQLite goes on reporting every file `indexed`
(the prepare-phase hash skip never looks at the layout), and search answers
`404 search.no_match` for ever. No error, no failed health check, no unusual log
line. The service is, from every angle it can see itself, working.

Shipping a bump while that was still true would have been shipping a documented
catastrophe on purpose, so `src/worker/stale.rs` landed in the same change. At
startup and hourly it answers two separate questions:

- **stale** — a project holds active chunks but its current-version collection is
  missing or empty. Its search is broken *now*. Gauge `mindex_stale_collections`.
- **orphaned** — a collection exists at a previous version. Nothing is broken, but
  nothing can reach it either and it holds the whole pre-bump index. SQLite records
  no layout, so this listing is the only thing in the system that can see it. Gauge
  `mindex_orphaned_collections`.

Both gauges seed at **-1**, never 0, and a pass that could not complete publishes
nothing: `0` is the healthy reading here, so an unreachable Qdrant must not be able
to spell it. Foreign collection names are classified and then never mentioned —
Qdrant may be shared, and a message telling an operator to delete another service's
data is worse than the problem being reported.

Dropping the old collections is deliberately **not** automated. It is what makes a
rollback impossible, and that call belongs to whoever can see whether the new index
is good.

## Rollout

1. Deploy. New collections are created as `_v2`; `_v1` collections are ignored, and
   startup names every affected project.
2. `mindex-index --force` per project. This repo: 4009 chunks at ~133 chunks/s ≈
   30 s of GPU.
3. Verify the `_v2` point counts, then `curl -X DELETE <qdrant>/collections/<name>`
   for each `_v1`.

Peak disk during the overlap ≈ 7.4 GB + ~3.7 GB ≈ 11 GB.

## The ladder, and where it stops

Each rung below is real, and each is deliberately not taken yet. The gate is the
same for all three and it is not a matter of taste: **there is no retrieval-quality
harness in this repo.** `perf/` measures indexing throughput; the MRR@10 0.3931 /
recall@10 20/23 figures in CLAUDE.md came from a one-off markdown-slicer evaluation
that no longer exists as a runnable thing. fp16 shipped without one because it is
not a quality trade. Nothing below has that excuse.

1. **Binary quantization of the ColBERT vector** (`always_ram` for the quantized
   copy, the fp16 originals on disk for rescoring). ×32 on top of fp16 — ~230 MB,
   RAM-resident, and the cold-start problem disappears entirely. This is the
   standard shape for late interaction, and reranking 200 pre-agreed candidates is
   about the most forgiving possible place to spend precision. It is still a
   quality trade, and quality is currently unmeasurable here.
2. **ColBERT token pooling** in the embedder — cluster adjacent/similar token rows
   before storing, 2-4× fewer rows. Orthogonal to quantization and compounding with
   it (fp16 + pooling + BQ ≈ 14 KB/chunk against today's 1.85 MB). Same gate.
3. **Ask whether ColBERT earns its place at all.** It is 99.6% of storage, all of
   the cold latency, and a third of the embedder's work. If a rerank-on/rerank-off
   comparison moves MRR@10 by less than a couple of points, the honest answer is to
   drop the third head. **This is the measurement to build first** — it is the one
   whose answer changes what the other two are worth.

## Considered and rejected

**Replacing the `has_id` filter with a payload filter.** Today the candidate set is
sent as a `has_id` list — ~4000 UUIDs, on three stages of every query. A `status`
payload field plus a payload index would make the common case a plain ANN with a
small `match` filter. Rejected: keeping that field correct requires a Qdrant write
on soft-delete, which is precisely what the append-only hot-path invariant exists
to avoid (indexing latency is deliberately decoupled from Qdrant delete latency).
The `has_id` list is a real cost and it grows linearly with the active-chunk count,
but it is bounded by one project's index and it buys an isolation mechanism that
cannot drift out of sync with SQLite, because it *is* SQLite.

A narrower variant is still open: skip the filter entirely when SQLite reports no
deleted chunks and the request carries no include/exclude/language filter. That is
the majority of searches and it costs one `COUNT`. Not done because it has never
been measured to matter — warm search is 56 ms.

**Per-project collections as such.** Many small collections is a known Qdrant
anti-pattern (per-collection segment and index overhead), and the alternative — one
collection with a `project_guid` payload field and a `match` filter — is noted in
CLAUDE.md. Not revisited here: it is the outer half of project isolation, and this
deployment has two projects.
