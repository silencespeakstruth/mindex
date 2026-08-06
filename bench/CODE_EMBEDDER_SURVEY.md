# Code embedder survey — candidates, budgets, and task-ready work items

Compiled 2026-08-05 for the hybrid proposal in `bench/FINDINGS.md` §8b:
BGE-M3 keeps prose, a code-specialised model takes code, ColBERT is deleted,
a reranker becomes the second stage.

**Scope of this file.** Published metrics, measured disk sizes, and a VRAM
budget against *this* host. It ends in numbered work items with acceptance
criteria so they can be cut into tasks directly. It does **not** recommend
shipping anything: only two changes in this whole investigation are supported
by measurement on this repo, and they are listed first.

---

## 1. The host budget, which decides the shortlist

| | |
|---|---|
| eGPU | AMD Radeon AI PRO R9700 (Navi 48), **32 624 MiB dedicated** — reserved for the research LLM |
| iGPU | Intel Arc Pro 140T (Arrow Lake-P), driver `xe`, torch reports **58.2 GiB** |
| system RAM | 62 GiB total |

**The iGPU has no VRAM wall — it shares system RAM.** Two real constraints
replace it:

1. **A 4 GiB ceiling on any single allocation** (observed on this host
   previously). This is the binding constraint, and it is not about weights.
2. **Bandwidth.** `CLAUDE.md` records the iGPU at ~17× slower per batch than
   the eGPU on bulk indexing, with the query path at ~28 ms either way.

### Where the 4 GiB ceiling actually bites — and why deleting ColBERT helps

Weights are never the problem: they are many tensors, and the largest single
one is the embedding matrix (vocab × hidden), which is under 100 MiB for every
candidate below.

The problem is **the ColBERT head's output**, which is one tensor of
`batch × seq_len × 1024 × 4 bytes`:

| batch | seq 512 | over 4 GiB? |
|---|---|---|
| 256 | 0.54 GiB | no |
| 512 | 1.07 GiB | no |
| 1024 | 2.15 GiB | no |
| **2048** | **4.29 GiB** | **yes** |

That is the same tensor whose *host-side* copy caused today's OOM at
`embed_batch_chunks = 2048` (§5.7 of FINDINGS). **A dense-only model emits
`batch × hidden` — one vector per chunk, ~1.5 MiB at batch 512 — three orders
of magnitude smaller.** So removing ColBERT removes the only allocation in the
pipeline that has ever approached the iGPU's per-object limit.

**Conclusion: every candidate below fits the iGPU comfortably. The question is
throughput, not capacity.**

---

## 2. Candidates, with measured sizes

Disk sizes read from the HuggingFace API on 2026-08-05 (safetensors only, ONNX
excluded). CoIR scores are `nDCG@10` averaged over eight subtasks.

| model | params | disk | license | CoIR | max seq | dim |
|---|---|---|---|---|---|---|
| **BGE-M3** (current) | 568M | 2 271 MB | MIT | **39.31** | 8192 | 1024 + sparse + ColBERT |
| **CodeRankEmbed** | **137M** | **547 MB** | **MIT** | **60.1** | 8192 | 768 |
| CodeSage-Large-v2 | 1.3B | — | Apache-2.0 | 59.4 | 2048 | Matryoshka |
| CodeSage-Base | 356M | — | Apache-2.0 | 57.5 | 2048 | — |
| jina-code-embeddings-0.5b | 494M | 988 MB | **CC-BY-NC-4.0** | — | 32768 | 896, Matryoshka |
| SFR-Embedding-Code-400M_R | 400M | 868 MB | **CC-BY-NC-4.0** | — | — | — |
| Qodo-Embed-1-1.5B | 1.5B | 6 173 MB | **"other"** | 68.53 † | — | — |
| nomic-embed-code | 7B | 28 283 MB | Apache-2.0 | — | 2048 | 768 |
| Voyage-Code-002 | ? | — | proprietary API | 56.26 | — | — |
| BM25 (floor) | — | — | — | 29.79 | — | — |

† Qodo's own evaluation, **not the CoIR paper's run**. Do not put 68.53 in the
same column as 60.1 and 56.26 — different harness. Kept in the table only so
nobody re-finds it and assumes it is comparable.

Sources: [CoIR paper Table 3](https://arxiv.org/html/2407.02883v3),
[CodeRankEmbed card](https://huggingface.co/nomic-ai/CodeRankEmbed),
[Modal's comparison](https://modal.com/blog/6-best-code-embedding-models-compared),
[Nomic Embed Code](https://www.nomic.ai/news/introducing-state-of-the-art-nomic-embed-code).

### The recommendation, and why it is not close

**CodeRankEmbed.** From its own card's table (CSN = CodeSearchNet MRR):

| model | params | CSN MRR | CoIR nDCG@10 |
|---|---|---|---|
| **CodeRankEmbed** | **137M** | **77.9** | **60.1** |
| CodeSage-Large | 1.3B | 71.2 | 59.4 |
| Jina-Code-v2 | 161M | 67.2 | 58.4 |
| Voyage-Code-002 | ? | 68.5 | 56.3 |
| OpenAI Ada-002 | ? | 71.3 | 45.6 |
| Arctic-Embed-M-Long (its own base) | 137M | 53.4 | 43.0 |

It beats a 1.3B model and a commercial API at **137M parameters and 547 MB on
disk**, under **MIT**, with an 8192 context — the same context BGE-M3 has, so
no chunking rule changes. Against BGE-M3's 39.31 that is **+20.8 CoIR points**,
nineteen times ColBERT's entire published contribution.

Disqualifying facts for the others:

- `jina-code-embeddings` and `SFR-Embedding-Code` are **CC-BY-NC-4.0** —
  non-commercial. mindex is the user's own tool, so this may be acceptable, but
  it is a licence decision and not a technical one; flagged, not decided.
- `Qodo-Embed-1-1.5B` is licence "other" (needs reading) and **6.2 GB**.
- `nomic-embed-code` is 7B / **28 GB** and its context is **2048**, shorter
  than the current slicer's ceiling — it would constrain chunking, and on the
  iGPU its throughput would be poor.

### Operational details that will bite

- **CodeRankEmbed requires a query prefix**: `"Represent this query for
  searching relevant code"`. Documents get none. Getting this wrong degrades
  silently, exactly like the `aud` claim or a mis-set derivation version.
- **Dimension is 768, not 1024.** `VECTOR_DIM` is a `const` documented as
  structural.
- **It is dense-only.** No sparse head, no multi-vector. §7a measured BGE-M3's
  *sparse* head as the strongest single arm on scikit-learn (0.5804 vs dense
  0.5301), so the lexical half must be replaced — `bench/baselines/bm25_fts5.py`
  is a working SQLite-FTS5 prototype that scored 0.3934 against mindex's 0.4289.

### The reranker gap

There is no code-specialised open cross-encoder in this survey.
`BAAI/bge-reranker-v2-m3` (2 271 MB, **Apache-2.0**) is multilingual **text** —
the same general-purpose mismatch as BGE-M3 itself.
`jina-reranker-v2-base-multilingual` is 1 119 MB but **CC-BY-NC-4.0**.

A first measurement of `bge-reranker-v2-m3` over this benchmark's own rankings
is in flight (`baselines/cross_encoder.py`); the 60-query smoke test came out
**negative** (0.3643 vs 0.3826), which is consistent with the mismatch but is
far too small to mean anything. **Do not plan around a reranker until that
number lands.**

---

## 3. Work items

Ordered so each is useful alone and the measured ones come before the
speculative ones. Every item states what makes it done.

### W1 — Replace the final ordering rule *(measured, two corpora)*

**What.** `db/qdrant.rs` fuses dense+sparse by RRF into a 200-pool, then orders
that pool by ColBERT alone, discarding the other two scores. Replace with
either the BGE-M3 weighted sum (`w1·dense + w2·sparse + w3·colbert`, w =
[1, 0.3, 1], ColBERT normalised by query token count) or plain RRF.

**Evidence.** Weighted sum: +0.0080 django (p=0.046), **+0.0211 sklearn
(p=0.0005)**. Plain RRF (no ColBERT at all): **+0.0276 sklearn (p=0.005)**.
ColBERT-as-sole-orderer has never been measured to help on either corpus at
either query length.

**Cost.** One query builder. No reindex, no schema change, no new storage.

**Trap.** Qdrant's `max_sim` returns the **sum** over query tokens; the paper's
weights assume a **mean**. Un-normalised the ColBERT term outweighs the others
by two orders of magnitude. The implementation that got this right,
`normalise_maxsim()` in `bench/baselines/pipeline_ablation.py`, was deleted with
the v2 pipeline (PROTOCOL §11, 2026-08-06); the trap is restated here rather
than pointed at, because this whole section is moot under v3 — there is no
late-interaction score to fuse.

**Done when.** The new ordering reproduces the bench arm's numbers to three
decimals on both corpora, and `bench/stats.py` shows non-inferiority at
δ = 0.01 against the current pipeline.

### W2 — Narrow the chunk window to 364 *(measured, one corpus)*

**What.** `[slicer].max_chunk_tokens` 512 → 364.

**Evidence.** +0.0108 nDCG@10 (django short, p=0.030). The gain is in the
**dense** head (+0.0124), not ColBERT. The more-chunks-per-file confound was
pre-registered and is **absent**: recall@20 moved +0.0001 while recall@1 moved
+0.0096, and MRR moved as much as nDCG.

**Cost.** A full reindex of every project. Do it when one is happening anyway.

**Not established.** 256 is not distinguishable from either 512 or 364, so 364
is a plateau, not a proven optimum. One corpus only.

**Done when.** Confirmed on scikit-learn short, and a 320/400/448 sweep has
either found a better point or shown the plateau.

### W3 — Finish the cross-encoder measurement *(in flight)*

**What.** `bench/baselines/cross_encoder.py` reranks a completed run's own
candidates — no reindex, no corpus pass. Currently running
`bge-reranker-v2-m3` at depth 50 over django short.

**Why it matters more than its size suggests.** Published cross-encoder gains
are **+5 to +15 nDCG@10** on MTEB/BEIR at **zero storage**. If any of that
transfers, the ColBERT question stops being "keep or drop" and becomes moot.

**Watch for.** The script prints a **ceiling** — the share of queries whose
gold file was among the input's 100 candidates at all (80% on the smoke run).
No reranker exceeds it. A gain that merely reaches the ceiling says the first
stage was already good enough and *ordering* was the whole problem, which is
what W1 also says.

**Done when.** A paired test over ≥1 000 queries on both corpora, at depths 25
and 50, with the latency cost recorded (cross-encoder latency is **linear** in
depth, unlike ColBERT's).

### W4 — Build the prose-retrieval corpus *(blocks the whole hybrid claim)*

**What.** A query set whose gold is documentation, not code: same
`build_docs_qrels.py`, inverse exclusion.

**Why.** **This benchmark cannot evaluate the prose half at all.** Queries come
from docs and the docs tree is excluded from ranking, so gold is always a code
file. "BGE-M3 stays for prose" is an untested premise. Nothing in the hybrid
proposal should be built before this exists.

**Done when.** ≥300 instances per corpus with a published leakage rate, audited
by hand, and BGE-M3 measured on it against the BM25 floor.

### W5 — Evaluate CodeRankEmbed offline, before touching mindex

**What.** Export the active chunks, embed with CodeRankEmbed (with its query
prefix), build a second Qdrant collection, query with the same query set,
score with the same `score.py`.

**Why offline.** It answers "does a code model beat BGE-M3 *on our code*"
without a `VECTOR_DIM` change, a `COLLECTION_SCHEMA_VERSION` bump, or any
mindex edit. If it does not win here, none of W6–W8 are worth planning.

**Budget.** 547 MB weights, 137M params, dense-only. Largest activation
`batch × hidden` ≈ 1.5 MiB at batch 512 — three orders of magnitude under the
iGPU's 4 GiB per-object ceiling. Fits either GPU trivially.

**Done when.** A paired test against the `dense-only` arm on both corpora, per
overlap stratum, with the query prefix verified present.

### W6 — Delete ColBERT *(gated on W1 and W3)*

**What.** Drop the `colbert` named vector; `COLLECTION_SCHEMA_VERSION` v2 → v3.

**Payoff, measured.** 838 MB/segment → 0, i.e. **99.6% of stored bytes**;
query latency 277 ms → 45 ms (**84%**), of which 252 ms is real MaxSim compute
isolated by a same-payload/1-candidate control. Plus: removes the only tensor
in the pipeline that has ever approached the iGPU's 4 GiB per-object limit.

**Do not do this before W1 and W3**, and note that the bump is **not
self-healing** — `docs/claude/qdrant.md` has the runbook, and every project
needs a manual `mindex-index --force`.

**Alternative if ColBERT survives.** Token pooling at factor 2–3 gives
"respectively no and little degradation"
([Answer.AI](https://www.answer.ai/posts/colbert-pooling.html),
[Qdrant](https://qdrant.tech/course/multi-vector-search/module-3/pooling-techniques/)) —
838 MB becomes ~280–420 MB. That turns "keep or drop" into "how much do we
pay", which is the better question if W3 comes back positive for late
interaction.

### W7 — Heterogeneous index: prose in BGE-M3, code in a code model *(gated on W4, W5)*

**What.** Route by `programming_language`: `markdown` → BGE-M3,
everything else → the code model. Two collections (or two named vectors of
different dims), queried in parallel, fused by **RRF over ranks**, reranked by
a cross-encoder.

**The load-bearing bit.** 1024-d and 768-d spaces are not comparable, so no
single score can order both lists. RRF fuses *ranks* and is model-agnostic; the
cross-encoder then rescores the top-k **on one scale**. In a heterogeneous
index the first stage owes only **recall** and the cross-encoder owns
**ordering** — so W3 is a prerequisite, not a follow-up.

**Already in place.** `model_id` is on `projects`, `project_files` and
`project_file_chunks`, so per-chunk model attribution exists in the schema.
Routing needs no classifier: `programming_language` is stored per file.

**New failure modes to design against.**
- The **Languages** checklist in CLAUDE.md gains a routing step; omitting it
  silently embeds a new language with the wrong model.
- `CHUNKS_DERIVATION_VERSION` already cannot see the embedder's identity;
  with two embedders a chunk's `model_id` becomes the only record of which
  space it lives in.
- Above the cross-encoder's depth the merged ordering is **ordinal only**.

### W8 — Replace the sparse half for code

**What.** Every code model in §2 is dense-only. BGE-M3's sparse head is the
strongest single arm on scikit-learn, so dropping it needs a replacement:
SQLite FTS5 BM25 over `project_file_chunks`.

**Prototype exists.** `bench/baselines/bm25_fts5.py` scored 0.3934 against
mindex's 0.4289 on django long — a conservative floor, since it searches
mindex's own AST chunks and has only a 60-word stoplist.

**Done when.** An FTS5 leg fused with the dense leg reproduces or beats the
current dense+sparse RRF arm on both corpora.

---

## 4. What is NOT recommended, with reasons

- **Binary/scalar quantization of ColBERT** — optimising the storage of a stage
  whose contribution is unproven.
- **Query expansion / HyDE / doc2query** — real published gains, but each puts
  an LLM call on the query path, and mindex's premise is that search is cheap
  and `/research` is where the model lives.
- **Fine-tuning an embedder on this codebase** — largest possible gain,
  largest possible overfit; needs held-out corpora that do not exist yet.
- **`nomic-embed-code` (7B)** — 28 GB, and a 2048 context shorter than the
  slicer's ceiling.
- **An LSP client** — refused on rule 10 in CLAUDE.md for reasons unrelated to
  retrieval quality.

---

## 5. Honest limits on everything above

- **CoIR is not this codebase.** All the +20.8 arithmetic is someone else's
  benchmark on someone else's code. W5 exists precisely because that transfer
  is an assumption.
- **The corpus is three times too small** to resolve a 0.01 effect
  (σ_d = 0.21 → ~3 400 queries needed at 80% power; there are 1 475). Effects
  the size of ColBERT's cannot be settled here at all.
- **Two Python corpora, one embedder, one hardware backend.** No Rust, Go, or
  TypeScript corpus has been measured on the descriptive tier.
- **BGE-M3 is multilingual and the code models are not.** A Russian-language
  query against a code-only model is untested, and mindex's user writes Russian.
