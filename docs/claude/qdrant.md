# Qdrant — one dense vector per collection, and how it got there

*Companion to `.claude/CLAUDE.md`, **Retrieval pipeline**. That file holds the
invariants; this one holds the name grammar, the classifier, the runbook and the
measurement record the current shape was derived from. Read this before changing
`ensure_project`, the `[qdrant]` config section, `collection_for`, or
`COLLECTION_SCHEMA_VERSION`. The retrieval-quality evidence itself lives in
`docs/claude/retrieval-v2.md` (kept as the record of what was measured) and
`bench/FINDINGS.md`.*

---

## The shape today (`v3`)

One named vector per collection: **`dense`**, the registry model's width
(1024 / 2560 / 4096 for the three Qwen3-Embedding sizes), cosine. No sparse
vector, no ColBERT multivector, no prefetch tree, no fusion, no rerank — a
search is one Qdrant query at `top_k` with `[qdrant].search_hnsw_ef` as the
beam, over a `has_id` candidate set built from SQLite.

The collection carries **no non-default options at all**. The three that
existed (`datatype: Float16`, `on_disk: true`, `hnsw_config.m = 0`) were every
one of them about the ColBERT vector, and they left with it. What survives from
that episode is the rule they were an instance of: anything that is part of the
collection's *schema* is a `const` in `src/db/qdrant.rs`, never a TOML key,
because changing it is a reindex and a config key invites an operator to trigger
one by accident.

## The name grammar

```text
{project_guid_simple}_{collection_slug}_{schema_version}
2f1c9a704d3e4e9b8b6a1f2e3d4c5b6a_q3e06b_v3
```

- `project_guid_simple` — the project GUID, dashless.
- `collection_slug` — from the registry entry (`q3e06b`, `q3e4b`, `q3e8b`).
  **The model is in the name**, which is the whole v3 change: a project has one
  collection *per model*, not one collection.
- `schema_version` — `COLLECTION_SCHEMA_VERSION`, `"v3"`.

Always derive a name through `collection_for(project_guid, spec)`; never format
one. `classify_collection` reads the grammar back, checking both halves of every
component — a foreign collection merely *ending* in `_v3` is still `Foreign`.

### Four classes, and why `OtherModel` is not `Previous`

| class | meaning | reported as |
|---|---|---|
| `Current` | active model, current schema version | served |
| `OtherModel` | a **registered** model, current version | held — info, not a problem |
| `Previous` | a superseded schema version | orphaned |
| `Foreign` | not written by this deployment | never mentioned |

`OtherModel` exists because switching `[model].id` is meant to be reversible.
The old model's vectors stay exactly where they are; flipping back reuses them
with no work at all. Reporting them as orphaned would be an instruction to
delete the thing that makes the switch cheap. `Previous` is the genuinely dead
case — nothing can reach it and nothing will.

`Foreign` is classified and then never named. Qdrant may be shared, and telling
an operator to delete another service's data is worse than the problem being
reported.

## GC crosses collections, and does not sweep them

`gc::sweep` confirms deletions against the **active model's** collection only.
A chunk soft-deleted while `qwen3-embedding-4b` is configured has its `0.6b`
vector left behind: unreachable by search (the candidate set is built from
SQLite, and those rows are gone), invisible to every metric that counts active
chunks, and paid for in disk.

This is accepted, and the reason it is accepted is that the alternative is
worse: sweeping every registered model's collection would mean issuing deletes
against collections this deployment may not have written, on a store that may be
shared, for vectors nobody is reading. The dead points are reclaimed by the two
things that were going to happen anyway — a `mindex-index --vectors-only` pass
into that model (which upserts over the same ids), or dropping the collection
when the model is genuinely retired.

If it ever stops being acceptable, the fix is a sweep that iterates
`EMBEDDING_MODELS` and treats a missing collection as a confirmation (which
`delete_batch` already does).

## The bump's own hazard, and what covers it

`COLLECTION_SCHEMA_VERSION` is a component of every collection's *name*. Bumping
it migrates nothing and fails nothing: the new name names no collection,
`ensure_project` makes an empty one, SQLite goes on reporting every file
`indexed` (the prepare-phase skip never looks at the layout), and search answers
`404 search.no_match` for ever. No error, no failed health check, no unusual log
line. The service is, from every angle it can see itself, working.

`src/worker/stale.rs` is what makes it visible. At startup and hourly it answers
two separate questions:

- **stale** — a project holds active chunks but its current-version collection
  is missing or empty. Its search is broken *now*. Gauge
  `mindex_stale_collections`.
- **orphaned** — a collection exists at a previous version. Nothing is broken,
  but nothing can reach it either and it holds the whole pre-bump index. SQLite
  records no layout, so this listing is the only thing in the system that can
  see it. Gauge `mindex_orphaned_collections`.

Both gauges seed at **-1**, never 0, and a pass that could not complete
publishes nothing: `0` is the healthy reading here, so an unreachable Qdrant
must not be able to spell it.

Dropping the old collections is deliberately **not** automated. It is what makes
a rollback impossible, and that call belongs to whoever can see whether the new
index is good.

## Runbook — `v2` → `v3`

This bump could not have been a migration in any case: the vectors themselves
are from a different model, and there is no function from a BGE-M3 dense vector
to a Qwen3 one. The database lineage restarts with it (`refuse_old_lineage`),
so this is a **delete and reindex**, and the order matters only in that nothing
is deleted before the new index is confirmed good.

1. Stand up the embedder: `deploy/embedder/README.md`, then
   `curl -s localhost:11211/v1/models`. Verify the **pooler** by cross-checking
   one embedding against `sentence-transformers` (that README's check 2) — a
   wrong pooler is a clean-looking null result, not an error.
2. Point `[model].server_url` at it and start mindex. Startup will:
   - **refuse** an old-lineage database, naming the file and the remedy;
   - cross-check the registry against `embedding_models`;
   - log `Embedder handshake ok.` per instance — this is the line that says the
     server behind the URL is the model config names.
3. Delete the old database file and start clean.
4. `mindex-index --force` per project. Confirm `{guid}_q3e06b_v3` exists with
   the expected point count and dimension.
5. Search something obvious. `search_unscorable_winners` must stay 0 (it counts
   NaN scores — the split-precision symptom).
6. Drop the old collections once the new index is confirmed:
   `curl -X DELETE <qdrant>/collections/<name>` for every `_v1`/`_v2` name the
   stale worker lists.

### Switching model *size* is a different, cheap procedure

Within one tokenizer (all three Qwen3 sizes share one), no reindex is needed:

1. Flip `[model].id`, restart. The handshake refuses if the server is not
   serving the new model, which is the common mistake.
2. `mindex-index --vectors-only` — re-embeds stored chunks into
   `{guid}_{new_slug}_v3`, stamps `embedded_model_id`, touches no chunk row and
   no symbol row.
3. The old collection is now `OtherModel`: held, reported as info. Flipping back
   is instant.

## The measurement that produced this shape (2026-08-03 → 2026-08-05, this host)

Kept because the current design is entirely a consequence of it, and because
"we deleted two thirds of the retrieval pipeline" reads as recklessness without
the numbers.

Qdrant 1.18.3, store at `~/.local/share/qdrant`, this repo's own collection,
under the **old** (`v1`) three-vector schema:

| storage | size | share |
|---|---|---|
| `vector_storage-colbert` | 838 MB | **99.6 %** |
| `vector_storage-dense` | 2.6 MB | 0.3 % |
| `vector_storage-sparse` | 0.47 MB | 0.06 % |

| | |
|---|---|
| this repo's collection | 7.4 GB for **4009 points** |
| per chunk | **~1.85 MB** ≈ 450 token rows × 1024 dims × fp32 |
| ColBERT : dense | **322 : 1** |
| search, cold | **20.3 s** |
| search, warm | 56-87 ms |
| Qdrant RSS | 186 MB |

Two facts follow, and between them they decided v3.

**ColBERT was the collection**, and the cold latency was the multivector paging
in: RSS 186 MB against 7.4 GB on disk means the store lived in page cache, and a
rerank over 200 candidates touched ~370 MB of it.

**And it was never shown to help.** The `v1 → v2` change (fp16, no ColBERT
graph, `on_disk`) was taken precisely because it was *not* a quality trade,
while the three rungs that were — binary quantization, token pooling, and
"does ColBERT earn its place at all" — were all gated on a retrieval-quality
harness that did not exist. `bench/` is that harness now, and the answer it
returned was blunter than the question: **RRF fusion of dense + sparse scored
below the single dense leg it fused**, and no configuration of the rerank
recovered the difference. So the third head went, then the second, and the model
underneath them was replaced by one measured at 0.4540 nDCG@10 against 0.3549
for what was deployed. The full record is `docs/claude/retrieval-v2.md` and
`bench/FINDINGS.md`.

## Considered and rejected

**Replacing the `has_id` filter with a payload filter.** The candidate set is
sent as a `has_id` list — ~4000 UUIDs on a query. A `status` payload field plus
a payload index would make the common case a plain ANN with a small `match`
filter. Rejected: keeping that field correct requires a Qdrant write on
soft-delete, which is precisely what the append-only hot-path invariant exists
to avoid (indexing latency is deliberately decoupled from Qdrant delete
latency). The `has_id` list is a real cost and it grows linearly with the
active-chunk count, but it is bounded by one project's index and it buys an
isolation mechanism that cannot drift out of sync with SQLite, because it *is*
SQLite. It also got cheaper in v3: one query stage instead of three.

A narrower variant is still open: skip the filter entirely when SQLite reports
no deleted chunks and the request carries no include/exclude/language filter.
That is the majority of searches and it costs one `COUNT`. Not done because it
has never been measured to matter.

**Per-project collections as such.** Many small collections is a known Qdrant
anti-pattern (per-collection segment and index overhead), and v3 multiplies
their number by the count of models in use. The alternative — one collection
with a `project_guid` payload field and a `match` filter — is noted in
CLAUDE.md. Still not revisited: it is the outer half of project isolation, and
per-model names are what make a model switch reversible, which is worth more
here than segment overhead on a deployment with a handful of projects.

**Keeping a sparse leg "for exact identifier matches".** The intuition is
strong and the measurement did not support it; `grep` (lexical, over
`project_file_chunks.code`, and honest about being lexical) is what answers the
identifier question in this system, and it does not need a vector.
