# Search-quality investigation — state of play, 2026-08-06

Handoff document. Written to be read cold by whoever continues this, including
me tomorrow. `PROTOCOL.md` is the pre-registration and the formal record;
**this file is the working narrative** — what was found, what was wrong, what
is still open, and where everything lives. Where the two disagree, PROTOCOL.md
is authoritative for method and this file for state.

> **§§0–9 are a snapshot of 2026-08-05, when nothing had been decided.** They
> open by saying so, in the present tense, and they are left that way because
> the order in which things were believed is the point of a working narrative.
> **§11 is what happened next**: the evidence was acted on, retrieval v3
> shipped, and mindex now runs one dense leg from a Qwen3-Embedding registry.
> Read §§0–9 for how the investigation arrived; read §11 for what it decided,
> what the decision does *not* rest on, and which of §8's next steps are now
> void. Section 10 sits between the two and is the round that produced the
> evidence §11 acts on.

## What this harness does not establish

Stated at the top rather than in a threats section, because these are the
limits a reader should carry into every number below.

- **Two corpora, one language, one query shape.** django and scikit-learn, both
  Python, both query sets derived from the projects' own documentation prose.
  Every non-Python corpus in `corpora.toml` tiers 1–2 was declared and never
  run. CoIR measures BM25 varying **56×** across datasets; nothing here bounds
  how these results travel.
- **Retrieval of prose is unmeasured (F9).** Gold is always a code file and
  `docs/**` is excluded from the ranking, so no claim about how documentation
  itself retrieves follows from anything here.
- **Russian queries are unmeasured**, and they are the stated reason the shipped
  model is Qwen3 rather than the arm F5 selected. That reason is a preference
  with a rationale, not a measurement — see §11.
- **The noise floor was measured on the query path only.** `--index-repeats 0`
  says the query path is deterministic; it says nothing about a comparison whose
  arms were indexed separately, which includes the chunk-window result and the
  v3-vs-v2 system comparison. PROTOCOL §12.5 carries the missing run.
- **"Holm–Bonferroni" and "TOST" are not what the code does.** The
  non-inferiority check is a confidence interval clearing the margin, not two
  one-sided tests, and the family-wise correction was applied by hand in one
  family and nowhere else. PROTOCOL §11, 2026-08-06.
- **Judgments are incomplete and the bias is downward** (PROTOCOL §9.1): the
  sampled hand-judging that would size it has not been done, so absolute scores
  are floors and only the paired comparisons are safe to read.

---

## 0. The one-paragraph summary

**Superseded by §10 on the two points that matter — read this for how the
investigation started and §10 for where it arrived.** "Dense+sparse RRF fusion
earns its keep" was true of BGE-M3's own heads and is **false** once the dense
leg is a 2026 encoder: the sparse head then contributes +0.004 with both
intervals through zero, and plain RRF scores *below* the single leg it fuses.
The chunk-window and combination-rule improvements below are still real and
still unshipped, but both are an order of magnitude smaller than the embedder,
which is now measured at +0.089 to +0.112 nDCG@10 over the deployed pipeline.

mindex's retrieval was never measured. It now is. Against a lexical floor
(BM25/FTS5 over its own chunks) the deployed pipeline wins by +0.036 to +0.055
nDCG@10 — real, but modest. Decomposing it: **dense+sparse RRF fusion earns its
keep; the ColBERT rerank does not clearly earn its 270× storage and 6× latency**
— its measured contribution is +0.007±0.01, which is *the same size the
BGE-M3 paper itself reports* (+1.1 nDCG points) and below what this corpus can
resolve. Two concrete improvements did surface and both are testable today:
**narrowing the chunk window 512→364 gives +0.011 (p = 0.028)**, and
**replacing mindex's ColBERT-only final ordering with the weighted sum BGE-M3's
authors specify gives +0.008 (p = 0.046)**. The harness is calibrated (a random
ranker lands on its analytic expectation) and the query path is bit-for-bit
reproducible (0 variance over 9 072 observations).

---

## 1. Where everything is

### Code (all new this session unless noted)

| path | what |
|---|---|
| `bench/PROTOCOL.md` | pre-registration + §12 results. Amendments in §11. |
| `bench/sphinx_docs.py` | Sphinx docs → queries + AST-verified gold. `--self-test` (parser + resolver, synthetic package). |
| `bench/build_docs_qrels.py` | driver; `--short` emits the short-query variant; `--audit N` samples instances for reading |
| `bench/run.py` | checkout → reindex → drift-prune → query → JSONL (pre-existing, edited) |
| `bench/score.py` | nDCG/Recall/MRR/MAP/Acc, strata, `--self-test` (pre-existing) |
| `bench/stats.py` | **new** — paired permutation, BCa bootstrap, non-inferiority against a margin. `--self-test` validates type-I rate, power against analytic, CI coverage |
| `bench/noise_floor.py` | edited — `--reuse-label` re-queries an existing index |
| `bench/slicer_sweep.py` | **new** — F3: rewrite config, restart server, reindex, query, verify window by re-tokenizing produced chunks |
| `bench/baselines/bm25_fts5.py` | **new** — F1 lexical floor + `--system random` calibration arm |
| `bench/baselines/pipeline_ablation.py` | **DELETED 2026-08-06** — F2's arms against Qdrant directly, incl. `weighted-sum`. It spoke BGE-M3's binary protocol and its arms were `full`/`no-colbert`/`dense-only`/`sparse-only`, a pipeline v3 does not have. Every F2 and §6 number below was produced by it and is frozen in `results/F2__*.json`; none can be re-run. PROTOCOL §11. |
| `bench/build_ident_qrels.py` | **new (round 3)** — the identifier query set (PROTOCOL §3.4): four query projections per instance, gold inherited byte-identically from the frozen issue tier |
| `bench/baselines/symbol_lookup.py` | **new (round 3)** — ranks by mindex's own `/symbols`, the arm that asks whether a positive F10 argues for a lexical leg or for routing |
| `bench/ranx_bridge.py` | **new (round 2)** — result JSONL → `ranx` Qrels/Run, keeping the chunk→file dedup |
| `bench/tests/test_ranx_equivalence.py` | **new (round 2)** — 18 tests; `ranx` reproduces `score.py` exactly on 6 metrics over 7 archived runs, and pins three undocumented `ranx` behaviours |
| `bench/baselines/fusion.py` | **new (round 2)** — chunk-level fusion over `ranx`'s 25 methods + weight search; `--train`/`--test` refuse the same corpus |
| `bench/baselines/external_embedder.py` | edited (round 2) — 5 models in `MODELS`, `--device`, device **and** dtype asserted |
| `bench/baselines/cross_encoder.py` | edited (round 2) — device/dtype asserted, delta-over-first-stage reported |
| `bench/.ruff.toml` | **new (round 2)** — pins isort classification so a new sibling directory cannot reformat untouched files |
| `docs/claude/retrieval-v2.md` | **the implementation spec** the round-2 evidence selected. Start there for coding. |

### Data

- Corpora: `bench/.data/qrels/{django,scikit-learn}-docs[-short].jsonl`
- Results: `bench/results/*.jsonl` + `.summary.json`
- Logs: `bench/.data/*.log`
- Clones: `bench/.clones/{django,django-364,scikit-learn,ripgrep}`

### Servers and indexes

A benchmark pass needs its own mindex process, its own SQLite file and its own
Qdrant collections, so that it cannot perturb or be perturbed by a live index.
None of the three is described by an absolute path here: this file is committed
and public, and the previous version of this section published one machine's
directory layout, database locations and derived project GUIDs — which is
operational state that rots in a week and tells a reader nothing they can use.

| | |
|---|---|
| bench server | `mindex --config bench/bench-config.toml --bind 127.0.0.1:11121 --cert-path "$MINDEX_BENCH_CERT" --key-path "$MINDEX_BENCH_KEY" --db-path "$MINDEX_BENCH_DB"` |
| bench database | `$MINDEX_BENCH_DB`; `run.py --db-path` reads index state from it out of band |
| Qdrant | a plain local instance; a full tier-1 pass stores ~113 GiB and bench collections are not cleaned up automatically |
| embedder | any OpenAI-compatible server on `:11211` — `deploy/embedder/` |

Project GUIDs are **derived, not configured**: `bench/run.py::project_guid`
hashes the label and the corpus name, so an arm's GUID is reproducible from the
command that produced it and does not need to be written down. That is also what
keeps two arms of the same sweep in separate Qdrant collections.

A sweep leaves the server on the **last arm's** config (`slicer_sweep.py` writes
`.bench-config-<window>.toml` and restarts against it). Restart against
`bench/bench-config.toml` before running anything else — §5's error log records
what happens when that step is skipped.

---

## 2. What is established

Confidence intervals are BCa bootstrap, p from paired two-sided randomization
(B = 10 000), δ = 0.01 (protocol floor; see §4).

### 2.1 The instrument is sound

- **Random ranker calibration.** Ran a uniform ranker through the whole chain:
  measured recall@10 = 0.0126 against an independently computed analytic
  0.0133. The qrels → results → chunk-to-file dedup → scorer path does not lie.
- **Noise floor = 0.** Seven passes of 1 296 queries over the same index:
  nDCG@10 = 0.428854 every time, **0 of 1 296 queries moved**. Same-index
  comparisons carry zero measurement noise.
- **The ablation reproduces the system.** `F2-full` = mindex to four decimals
  (0.4289 vs 0.4289) over all 1 296 queries, identical top-10 on a 120-query
  sample.
- **Gold reachability**: 0 of 2 112 gold files absent from the index.
- **Exclusion applied**: 0 doc-tree paths in 129 600 ranked positions.

### 2.2 Absolute numbers (django, short queries, n = 1 115, window 512)

| system | nDCG@10 | MRR@10 | R@1 | R@10 |
|---|---|---|---|---|
| random ranker | ~0.008 | — | — | 0.013 |
| sparse-only | 0.3183 | 0.3134 | 0.1475 | 0.4628 |
| dense-only | 0.3329 | 0.3264 | 0.1539 | 0.4941 |
| RRF fusion (no ColBERT) | 0.3484 | 0.3426 | 0.1638 | 0.5088 |
| **mindex as deployed** | **0.3549** | 0.3468 | 0.1671 | 0.5215 |
| **weighted sum (paper's)** | **0.3630** | 0.3536 | 0.1735 | 0.5308 |

The floor of this scale is 0.008, not 0.5 — django has 2 701 indexed files and
a median gold set of 1. "0.35" is not "35% right".

### 2.3 F1 — the lexical floor (long-query corpus)

BM25 via SQLite FTS5 over the **identical chunk set** read from mindex's own DB,
same depth, same exclusions, same scorer.

| corpus | stratum | n | mindex | BM25 | Δ | 95% CI | p |
|---|---|---|---|---|---|---|---|
| django | all | 1296 | 0.4289 | 0.3934 | **+0.0355** | [+0.018, +0.053] | 0.0002 |
| django | obvious | 356 | 0.4623 | 0.4409 | +0.0214 | [−0.013, +0.057] | 0.230 |
| django | mixed | 695 | 0.4460 | 0.3984 | **+0.0476** | [+0.024, +0.071] | 0.0001 |
| django | non-obvious | 245 | 0.3316 | 0.3100 | +0.0216 | [−0.019, +0.062] | 0.287 |
| sklearn | all | 430 | 0.6621 | 0.6070 | **+0.0551** | [+0.036, +0.075] | 0.0001 |
| sklearn | non-obvious | 208 | 0.5887 | 0.5588 | **+0.0299** | [+0.005, +0.054] | 0.020 |

The BM25 baseline **inherits mindex's AST slicer and gap-fill** (it searches
mindex's chunks) and has a 60-word stoplist — so it is a *conservative* floor
and the measured advantage is the smaller of the two available readings.

### 2.4 F2 — stage contributions

**Fusion earns its keep** (long corpus, both directions significant):

| corpus | comparison | Δ | 95% CI | p |
|---|---|---|---|---|
| django | RRF vs dense-only | +0.0279 | [+0.018, +0.038] | 0.0001 |
| django | RRF vs sparse-only | +0.0341 | [+0.024, +0.045] | 0.0001 |
| sklearn | RRF vs dense-only | +0.0354 | [+0.025, +0.047] | 0.0001 |
| sklearn | RRF vs sparse-only | **−0.0126** | [−0.023, −0.002] | 0.0196 |

**The last row reverses the sign**: on scikit-learn, sparse **alone** beats the
fused pool and beats the deployed pipeline (0.6829 vs 0.6621, Δ = +0.0208,
p = 0.008). Hybrid fusion is corpus-dependent. Neither corpus alone shows this.

**ColBERT's contribution depends on query length, significantly:**

| band | n | Δ (full − no-colbert) | 95% CI | p |
|---|---|---|---|---|
| short, < 300 B | 336 | +0.0234 | [−0.002, +0.048] | 0.067 |
| long, ≥ 300 B | 960 | **−0.0157** | [−0.029, −0.003] | **0.023** |

Permuting the band label (a direct interaction test): difference 0.0391,
**p = 0.0035**. On the short corpus the harm is gone: +0.0065 [−0.006, +0.019].

### 2.5 F3 — the chunk token window (short corpus, full reindex each)

| window | chunks | chunks/file | nDCG@10 | MRR@10 | Δ vs 512 | 95% CI | p |
|---|---|---|---|---|---|---|---|
| 512 | 26 228 | 9.71 | 0.3549 | 0.3468 | — | — | — |
| **364** | 29 432 | 10.90 | **0.3657** | 0.3580 | **+0.0108** | [+0.0012, +0.0208] | **0.030** |
| 256 | 35 033 | 12.97 | 0.3629 | 0.3557 | +0.0080 | [−0.003, +0.020] | 0.173 |

364 vs 256: +0.0028 [−0.008, +0.013], p = 0.61 — **not monotone; 364 looks
like a plateau or a shallow optimum, and 256 is not distinguishable from 512.**

**The tickets confound was pre-registered and is absent.** More chunks per file
means more lottery tickets, and that mechanism helps *more* as k grows. The
observed profile is the opposite:

| | R@1 | R@5 | R@10 | R@20 |
|---|---|---|---|---|
| Δ (364 − 512) | +0.0096 | +0.0130 | +0.0064 | **+0.0001** |

Twelve percent more chunks bought **nothing** at k = 20, and MRR@10 moved as
much as nDCG. This is a change in *ordering*, not coverage.

Where it comes from — **not from ColBERT**:

| arm | 512 | 364 | Δ |
|---|---|---|---|
| dense-only | 0.3329 | 0.3453 | **+0.0124** |
| sparse-only | 0.3183 | 0.3199 | +0.0016 |
| RRF fusion | 0.3484 | 0.3573 | +0.0089 |
| full | 0.3549 | 0.3657 | +0.0108 |

The dense head is what sharpens. Mechanism: a dense vector is **one** averaged
representation per chunk, diluted by unrelated tokens; a sparse vector keeps
terms separate and barely cares about chunk length. Interaction test (does
narrowing make the rerank worth more?): +0.0019 [−0.008, +0.013], **p = 0.73 —
no**.

### 2.6 The paper's weighted sum beats mindex's ordering

BGE-M3 specifies `s = w1·dense + w2·sparse + w3·colbert`, w = [1, 0.3, 1].
**mindex instead orders the fused pool by ColBERT alone**, discarding the dense
and sparse scores at the final step.

| stratum | n | Δ (weighted sum − mindex) | 95% CI | p | TOST δ=0.01 |
|---|---|---|---|---|---|
| all | 1115 | **+0.0080** | [+0.0001, +0.0159] | **0.046** | PASS |
| obvious | 604 | +0.0108 | [−0.000, +0.021] | 0.054 | PASS |
| mixed | 363 | +0.0123 | [−0.002, +0.026] | 0.084 | PASS |
| non-obvious | 148 | −0.0137 | [−0.034, +0.005] | 0.171 | FAIL |

Interesting and unexplained: the gain is in `obvious`/`mixed` and **reverses**
in `non-obvious`. Underpowered there (n = 148), but worth a look.

**Implementation detail that is load-bearing.** Qdrant's `max_sim` returns the
**sum** over query tokens (a 192-token query scores 191.999 against its own
text); the paper's weights assume FlagEmbedding's `colbert_score`, which
divides by the query token count. Without normalising, the ColBERT term
outweighs the others by two orders of magnitude and the "weighted sum" is just
`full` with extra steps. The fix was `normalise_maxsim()` in
`pipeline_ablation.py`, which is deleted (§1) — the correction is recorded here
because it is the kind of thing the next person to fuse a late-interaction score
into anything has to rediscover, and the code that did it is gone.

### 2.7 Cost, measured

| | value |
|---|---|
| ColBERT storage | 838 MB/segment vs 2.6 MB dense + 0.5 MB sparse — **270×**, 99.6% of bytes (from `qdrant.md`, not re-measured here) |
| ColBERT latency | full **131.7 ms** vs no-colbert 45.7 ms on short queries; **277 ms vs 45 ms** on long — 84% of query time |
| — of which real MaxSim | 252 ms (isolated by sending the same payload with a 1-candidate pool: 368 ms → 116 ms) |
| weighted-sum latency | 318.8 ms — but that is **5 HTTP round-trips in my harness**, not an implementation estimate. In-process it is one extra score fetch. |

Narrowing the chunk window does **not** reduce ColBERT storage: one row per
token, and a corpus's token count does not depend on how it is cut.

---

## 3. Academic sources and the calculation

Primary: **M3-Embedding (BGE-M3)**, arXiv 2402.03216
— <https://arxiv.org/html/2402.03216v5>
Model card: <https://huggingface.co/BAAI/bge-m3>

### The combination formula (paper, hybrid retrieval section)

```
s_rank = w1·s_dense + w2·s_lex + w3·s_mul

MIRACL, MKQA:  w = [1,    0.3, 1   ]
MLDR:          w = [0.15, 0.5, 0.35]
```

Model card example: `w[0]*dense_score + w[1]*sparse_score + w[2]*colbert_score`,
weights `[0.4, 0.2, 0.4]`.

### The published ablation, and ColBERT's marginal contribution

| benchmark | metric | Dense | Sparse | Multi-vec | Dense+Sparse | All | **All − (D+S)** |
|---|---|---|---|---|---|---|---|
| MIRACL | nDCG@10 | 69.2 | 53.9 | 70.5 | 70.4 | 71.5 | **+1.1** |
| MKQA | Recall@100 | 67.8 | 36.3 | 68.4 | 68.1 | 68.8 | **+0.7** |
| NarrativeQA | nDCG@10 | 48.7 | 57.5 | 55.4 | 60.1 | 61.7 | **+1.6** |
| **MLDR (long docs)** | nDCG@10 | 52.5 | **62.2** | 57.6 | 64.8 | 65.0 | **+0.2** |

**The calculation that matters.** ColBERT's marginal contribution over
dense+sparse is **+0.2 to +1.6 nDCG points, i.e. 0.3–2.7% relative**. On MLDR —
long documents, the nearest published analogue to code chunks — it is **+0.2
points, 0.3%**. And on MLDR the head ordering inverts entirely: sparse (62.2)
beats dense (52.5) by ten points. *That matches what this benchmark measured on
scikit-learn, where sparse alone beat the full pipeline.*

**My measurement vs the paper's.** I measured ColBERT at +0.0065 (window 512)
and +0.0084 (window 364), CI [−0.004, +0.021]. The paper's +0.011 sits inside
that interval. **There is no contradiction with the published result — I
reproduced it and lacked the power to call it significant.**

### The authors on cost

> *"For the multi-vector method (denoted as Multi-vec), considering its heavy
> cost, we use it as reranker to re-rank the top-200 candidates from dense
> method."*

They acknowledge the cost and never quantify storage or latency. mindex's
architecture (rerank the top-200 pool) matches their *evaluation protocol* for
the `Multi-vec` row — but mindex's final ordering is ColBERT alone, which is
that row (70.5), not the `All` row (71.5).

### Not verified, and it matters

None of these benchmarks are **code**. MIRACL is multilingual QA, MLDR long
documents, NarrativeQA fiction. The transfer to code is assumed, not shown. My
own corpus supports it but is underpowered.

---

## 4. Statistics — what the numbers can and cannot say

- **δ = 0.01**, the protocol floor, because the measured between-run SD is
  **0**. Rule fixed before data: δ = 2 × pooled SD, round up to 0.005, floor
  0.01.
- **This retired a provisional δ = 0.030** from the earlier ripgrep run
  (12 queries, included index rebuilds) — it overstated same-index noise ~30×
  and had already been written into a conclusion.
- **Power.** σ_d = 0.21 per query for the ColBERT contrast. At 80% power,
  α = 0.05:

  | to detect | queries needed |
  |---|---|
  | δ = 0.02 | **~850** |
  | δ = 0.01 | **~3 400** |

  The short corpus has 1 115. **An effect the size of ColBERT's (+0.007) is
  three times beyond this corpus's reach.** No amount of re-running changes
  that — only more queries do.
- `stats.py --self-test` validates: type-I rate 3.5% (want ~5%), power 57%
  empirical vs 53% analytic on uncorrelated pairs, 40/40 on correlated pairs,
  95% CI covered the truth 94.7%, `ppf` inverts `cdf`.

---

## 5. My errors — every one, with what it cost

Recorded because several produced *plausible* results, and a plausible wrong
answer is the failure mode this whole exercise exists to catch.

### 5.1 Measured the wrong task (caught by the user)

Built the entire first corpus around **issue localization** — "here is a bug
report, name the files to fix". mindex does *matching*; localization needs
*inference*, which belongs to `/research`. Would have answered the ColBERT
question on a task the component does not perform. **Cost: the first corpus,
and it was the load-bearing choice.**

### 5.2 Queries far longer than any real caller's (caught by the user's challenge)

The descriptive corpus had median query 562 B (django) / 1 089 B (sklearn),
because doc sections are long. Real callers — MCP `search`, `mindex-search.sh`,
the VS Code Ask box — type ~140 B. **I measured the retriever outside its
operating regime, and specifically in the band where ColBERT does harm.** This
is what produced "ColBERT is useless", which was wrong.

### 5.3 Declared ColBERT's contribution "indistinguishable from zero" without checking the literature

Should have compared against the published +1.1 nDCG before framing a null
result as a finding. The number I measured *agrees* with the paper. **Stated as
a finding, withdrawn.**

### 5.4 Three ground-truth resolver defects, all silent

- **Bare name under `currentmodule`** did not follow re-exports. scikit-learn
  writes `.. currentmodule:: sklearn.decomposition` then ``:class:`PCA` ``, so
  **904 of 1 966 references (46%)** resolved to an `__init__.py` that defines
  nothing and were discarded. Corpus 239 → 422.
- **Test doubles counted as definitions.** `tests/test_sgd.py` defines a class
  named `SGDRegressor`, so the real one scored *ambiguous* and was dropped —
  while `Ridge` and `Lasso`, named in the same section only as alternatives,
  survived. **The gold set named everything the section was not about.**
- **The fix for that was too wide at first**: excluding `test/` (singular)
  deleted `django/test/`, which is public API. 389 → 585 unresolvable refs.

All three were found by *reading sampled instances*, none would have failed a
test. The resolver now has one (`sphinx_docs.py --self-test`, synthetic
package with every shape).

### 5.5 Called the test-file dominance a ranking pathology

Reported "42% of top-10 slots are test files, 34% of queries have a test #1" as
a defect. Then measured the index: **tests are 66.4% of django's chunks**, so
the ranker *demotes* them by a third relative to chance. Corpus composition,
not pathology. Corrected within the same session.

### 5.6 Blamed a benchmark failure on the indexer's exit code twice

`mindex-index` exits 1 when any file failed. First failure: transient
(embedder restart, retry worker fixed it). Second: a deliberately non-UTF-8
django fixture. My "did it complete" check then read `stdout` — **the indexer
reports on `stderr`** — so the guard never fired and two multi-hour runs died at
the starting line.

### 5.7 OOM'd the machine

Set `embed_batch_chunks = 2048` from `perf/`'s throughput numbers, where the
GPU is the bottleneck. **ColBERT stores one 1024-float row per token = 4 KiB
per token**, so an in-flight batch costs `batch × tokens/chunk × 4 KiB`: 2 048
chunks of ~350 tokens ≈ 2.9 GB, and `mindex-index` sends four concurrently ≈
**11.6 GB**. With two servers up, 62 GB ran out and the kernel killed the
user's Steam. Lowered to 512. **Nothing in the flag, the config or the docs
says the cost is that shape** — see §7.

### 5.8 Wrote a self-test that asserted a number instead of a relationship

`stats.py`'s power check asserted "≥80% at δ=0.03, n=1000" and failed at 57%.
The *test* was right: those draws are uncorrelated, and the analytic power is
53%. Rewrote it to assert agreement with the analytic value, plus a separate
correlated-pairs case. **A self-test that encodes my expectation rather than a
property is worse than none.**

### 5.9 Nearly reported a precision arm that never ran

Asked `SentenceTransformer` for `float16` via `model_kwargs={"torch_dtype": …}`.
The `trust_remote_code` loader **builds the module itself and ignores it** —
the parameters stayed `float32`. Both "fp32" and "fp16" runs were fp32, which
is why they came out **byte-identical on every one of 1 115 queries** and at
the same throughput. That agreement is what looked like a clean result: "fp16
costs no quality and buys no speed", about a precision never used. Caught by
checking `next(model.parameters()).dtype` instead of trusting the argument.
The script now casts explicitly **and asserts**, and the mislabelled results
were deleted rather than relabelled.

### 5.10 Joined Qdrant point ids in the wrong spelling

SQLite stores them dashless, Qdrant answers with dashes. Every ranking came
back empty. Caught only because unmatched points are *counted* — the same
reason mindex has `search_orphaned_winners`.

---

## 6. Discrepancies in counting — read before comparing tables

1. **The two corpora are not comparable in aggregate.** Cutting to the first
   sentence moves the `obvious` stratum from 27% → 54% (the leading sentence is
   where the defined name sits). **Only per-stratum rows may be read across
   long and short.**
2. **`django` vs `django-364` instance ids.** The 364 arm ran against a second
   clone under a renamed corpus, so its ids carry a `django-364:` prefix.
   Normalised copies exist as `slicer364__django-docs-short.jsonl` and
   `F3-364-*__django-docs-short.jsonl`. **Use the normalised ones for paired
   tests**; `stats.py` intersects on id and would silently compare nothing.
3. **`F2-full` vs a real mindex run.** `F2-full` reaches Qdrant over HTTP/JSON;
   mindex uses gRPC. Rankings are identical (verified); **latencies are not
   comparable** — my JSON body is 4.4 MB on long queries.
4. **`weighted-sum` latency (319 ms) is a harness artifact**, five round-trips.
   Not an implementation estimate.
5. **Per-dataset drop counters sum to more than the unique loss** on the issue
   tier (Verified ⊂ full). Tracked as a set; reported uniquely.
6. **`recall@20` on the long corpus** has 4 of 1 296 queries whose ranking is
   shorter than 20 — at that cutoff it is recall-over-what-came-back.
   `score.py` prints this; do not silently drop the line.
7. **`chunks_active` counts differ from `points_count`** in Qdrant by design
   (append-only hot path leaves orphans until GC).

---

## 7. Findings about mindex itself, not about retrieval quality

Candidates for CLAUDE.md; none written there yet.

- **`embed_batch_chunks` costs host RAM as `batch × tokens/chunk × 4 KiB ×
  client concurrency`.** Four multipliers, in three different places (a server
  config key, a slicer config key in another section, and a client flag), and
  none of them says so. Default 256 is safe; 2048 is 11.6 GB.
- **`mindex-index`'s exit code conflates "could not run" with "n files
  failed"**, and it reports on stderr. A non-UTF-8 file (django ships one
  deliberately) makes every run exit 1 forever. `/drift` correctly reports 0
  missing — the scanner drops it from the manifest too — so this is *only* the
  exit code.
- **`GET /config` does not publish the slicer window**, so nothing can verify
  which chunking a running server is using. `slicer_sweep.py` works around it
  by re-tokenizing produced chunks. A server silently left on the wrong config
  would report a baseline under another name — a clean null result, the most
  believable wrong answer.
- **The ColBERT rerank discards the dense and sparse scores** at the final
  ordering step, which is not what BGE-M3 specifies (§2.6, §3).

---

## 7a. CONFIRMED ON THE SECOND CORPUS (added after the first draft)

scikit-learn, short queries, n = 360, window 512. **Both leads confirm, and one
result strengthens into something that was previously only a direction.**

| arm | nDCG@10 | MRR@10 | R@1 | R@10 |
|---|---|---|---|---|
| dense-only | 0.5301 | 0.5312 | 0.3194 | 0.6752 |
| **mindex as deployed** | 0.5567 | 0.5594 | 0.3412 | 0.7014 |
| weighted sum (paper's) | 0.5778 | 0.5865 | 0.3762 | 0.7084 |
| sparse-only | 0.5804 | 0.5886 | 0.3873 | 0.7016 |
| **RRF fusion, no ColBERT** | **0.5843** | 0.5914 | 0.3950 | 0.7101 |

**The weighted sum beats mindex on both corpora, and here it wins everywhere:**

| stratum | n | Δ (weighted sum − mindex) | 95% CI | p |
|---|---|---|---|---|
| all | 360 | **+0.0211** | [+0.010, +0.034] | **0.0005** |
| obvious | 180 | +0.0223 | [+0.007, +0.041] | 0.012 |
| mixed | 95 | +0.0150 | [−0.007, +0.038] | 0.195 |
| non-obvious | 85 | **+0.0255** | [+0.003, +0.053] | 0.043 |

The `non-obvious` reversal seen on django (−0.0137, n = 148) does **not**
reproduce; here the same stratum gains most. Treat django's as noise.

**And the ColBERT-only ordering is now significantly harmful on this corpus:**

| stratum | n | Δ (full − no-colbert) | 95% CI | p |
|---|---|---|---|---|
| all | 360 | **−0.0276** | [−0.048, −0.009] | **0.005** |

So across everything measured: ColBERT-as-sole-final-orderer is **harmful on
long queries (django, p = 0.023), harmful on short queries (sklearn,
p = 0.005), and not distinguishable from zero on django short.** It has never
been shown to help. Replacing that ordering rule — with either the paper's
weighted sum or plain RRF — is now supported by two corpora.

Note the ordering on this corpus: `no-colbert` (0.5843) ≥ `sparse-only`
(0.5804) > `weighted-sum` (0.5778) > `full` (0.5567). Sparse dominates here,
exactly as the BGE-M3 paper's MLDR row predicts for long documents.

## 7b. THE BIGGEST LEVER IS THE EMBEDDER, NOT THE RERANKER

From the CoIR paper's Table 3 (<https://arxiv.org/html/2407.02883v3>),
average nDCG@10 over eight code-retrieval subtasks:

| model | CoIR avg nDCG@10 | vs BM25 |
|---|---|---|
| BM25 | 29.79 | — |
| **BGE-M3 (what mindex uses)** | **39.31** | +9.5 |
| OpenAI Ada-002 | 45.59 | +15.8 |
| E5-Mistral | 55.18 | +25.4 |
| Voyage-Code-002 | 56.26 | +26.5 |

**BGE-M3 sits 17 points below code-specialised models on code retrieval.** That
is fifteen times ColBERT's entire published contribution (+1.1). It is a
general-purpose multilingual model, and the CoIR authors note its scores
*degrade* as document length grows on code, "possibly because although BGE-M3
has been optimized for long documents, the significant differences between code
data and text data result in a performance degradation".

This is consistent with what this benchmark measured independently: mindex
beats a BM25 floor by only +0.036 nDCG.

## 8. What to do next, in order

> **Written 2026-08-05 and largely overtaken.** Items 1, 2 and 4 are **void**:
> §10 replaced the dense leg, and there is no longer a second leg to weight, a
> combination rule to tune, or a ColBERT store to decide about — all three were
> deleted by v3. They are struck rather than removed, because a list of next
> steps that quietly loses the ones that stopped mattering cannot be checked
> against what was actually done. §11 carries the current list.

1. ~~**Confirm the weighted sum on the second corpus.**~~ **Void** — the
   weighted sum combines three heads of a model that is no longer served, and
   the script that ran the arm is deleted (§1).
2. ~~**Sweep the weights.**~~ **Void**, same reason.
3. **Get to ~3 400 short queries.** Still open, and still the only thing that
   would make a δ = 0.01 effect resolvable at all. django (1 115) + sklearn
   (360) = 1 475. Candidates with large Sphinx trees: sympy, pandas, scipy,
   matplotlib, Flask, SQLAlchemy.
4. ~~**Then, and only then, decide on ColBERT.**~~ **Decided, and not by this
   item**: F2 never showed it moving a metric, F7 removed the second leg, and
   v3 deleted the store. The three-way keep/drop/token-pooling framing was the
   right one and became moot when the leg it reranked was replaced.
5. **Re-run F3 at 364 on scikit-learn** — still open, and now doubly so: the
   window result is one corpus *and* was measured under the BGE-M3 tokenizer,
   while the shipped slicer counts in Qwen3's. PROTOCOL §12.10.
6. **Restore the bench server to `bench/bench-config.toml`** after any sweep,
   and drop the bench collections a pass leaves behind.
7. Tier-0 fixture + `ci.yml`; research evaluation; `docs/BENCHMARK.md`.

### Cheap wins available immediately

- `run.py` issues queries **sequentially** (deliberate, for clean `search_ms`).
  A `--query-concurrency` flag would cut a 1 296-query pass from 6.4 min to
  well under a minute. Do it **after** any noise-floor measurement, not during.
- The `weighted-sum` arm re-queries the store five times per query. Caching the
  three score maps per query would make a weight sweep nearly free.

---

## 8a. The option space, with published numbers

Surveyed 2026-08-05 in response to "if not ColBERT, then what". Ordered by
**published effect size per unit of cost**, which is not the order anyone would
guess. Every number here is someone else's measurement on someone else's
corpus — none is code-retrieval-on-this-repo, and the transfer is an assumption
in every row.

### A. Replace the embedder with a code model — the largest lever by far

CoIR Table 3, average nDCG@10 over eight code-retrieval subtasks
(<https://arxiv.org/html/2407.02883v3>):

| model | CoIR avg | over BM25 |
|---|---|---|
| BM25 | 29.79 | — |
| **BGE-M3 (current)** | **39.31** | +9.5 |
| OpenAI Ada-002 | 45.59 | +15.8 |
| E5-Mistral | 55.18 | +25.4 |
| Voyage-Code-002 | 56.26 | +26.5 |

**+17 points available**, against ColBERT's published +1.1. Open-weight
candidates:

| model | params | dim | backbone | notes |
|---|---|---|---|---|
| [jina-code-embeddings-0.5b](https://huggingface.co/jinaai/jina-code-embeddings-0.5b) | 494M | 896, Matryoshka to 64 | Qwen2.5-Coder-0.5B | 78.4% MTEB-Code avg; last-token pooling; text→code, code→code, code→text |
| jina-code-embeddings-1.5b | 1.5B | — | Qwen2.5-Coder-1.5B | same family |
| [Qodo-Embed-1-1.5B](https://www.qodo.ai/blog/qodo-embed-1-code-embedding-code-retrieval/) | 1.5B | — | — | CoIR 68.53 on Qodo's own run (**different evaluation from the CoIR paper's — do not put these two tables side by side**) |
| CodeRankEmbed | 137M | — | — | compact, CoRNStack-trained |

**Four things this costs, and the fourth is the one that decides it:**

1. Full reindex of every project, `COLLECTION_SCHEMA_VERSION` bump, and the
   non-self-healing bump procedure `qdrant.md` documents.
2. `VECTOR_DIM = 1024` is a `const` documented as structural, not a knob. 896
   or 768 means touching it and everything keyed to it.
3. The embedder server is BGE-M3-specific (three heads, custom binary wire
   format). A dense-only model is a *simpler* server, not a harder one.
4. **BGE-M3's real selling point is that one model produces dense + sparse +
   ColBERT.** Every code model above is **dense-only**. Dropping BGE-M3 drops
   the sparse head — and this benchmark measured the sparse head as the
   *strongest single arm* on scikit-learn (0.5804 vs dense 0.5301) and the
   paper measures the same on MLDR (62.2 vs 52.5). The lexical half would have
   to come back as BM25/FTS5, which `bench/baselines/bm25_fts5.py` already
   implements and which scored 0.3934 against mindex's 0.4289.

Also unmeasured and load-bearing here: mindex indexes **markdown**, and its
`MarkdownSlicer` exists for that. A code-specialised model may be worse on
prose. And BGE-M3 is multilingual; a Russian-language query against a
code-only model is untested.

### B. Cross-encoder reranker — the natural replacement for ColBERT

Reported gains of **+5 to +15 nDCG@10 on MTEB/BEIR**, and **storage cost is
exactly zero** — a cross-encoder stores nothing, it scores (query, chunk) pairs
at query time.

| model | BEIR nDCG@10 |
|---|---|
| jina-reranker-v2 | 57.06 |
| bge-reranker-v2-m3 | ~58.7 (implied) |
| [jina-reranker-v3](https://jina.ai/models/jina-reranker-v3/) | **61.94** |

Against ColBERT this is the whole trade in one line: **ColBERT buys ~1 point
for 270× storage and 252 ms; a cross-encoder buys 5–15 points for 0× storage
and a comparable GPU cost.** mindex already pays 252 ms per query for MaxSim —
a cross-encoder over the top 50 is in the same latency envelope, and the
`[research]` runtime and Ollama plumbing show the host can carry another model.

Known caveat from the sources: cross-encoder latency is **linear in candidate
count** (reranking 200 costs 4× reranking 50), whereas ColBERT's is sub-linear
because the document side is precomputed. So the design is "rerank 25–50", not
"rerank 200".

### C. If ColBERT stays: token pooling

[Answer.AI](https://www.answer.ai/posts/colbert-pooling.html) and
[Qdrant's own course](https://qdrant.tech/course/multi-vector-search/module-3/pooling-techniques/):
cluster semantically similar tokens within a document and mean-pool them.
**Pooling factors 2 and 3 give respectively no and little degradation** — so
838 MB/segment becomes ~280–420 MB for the same quality. Hierarchical
clustering (k = 32 or 64) is the current form; it composes with quantization.

This turns "keep or drop" into "how much do we pay", which is the better
question — but note it does not address the finding that the *ordering rule* is
what is wrong (§2.6, §7a).

### D. Late chunking

[Late Chunking, arXiv 2409.04701](https://arxiv.org/pdf/2409.04701): embed the
**whole document** with a long-context model first, then pool token embeddings
into chunks — so every chunk vector carries document context instead of being
embedded in isolation. Reported to beat naive chunking on BEIR and LongEmbed.

Directly relevant to §2.5: the F3 result says narrowing the window helps
because a dense vector is one average over the chunk. Late chunking attacks the
same problem from the other side — it keeps chunks small *and* contextual. It
needs a long-context model (BGE-M3 already is one: 8 192 tokens) and a change
to how `embed.rs` calls `/encode`: one document per call instead of a batch of
chunks.

### E. Already measured here, and cheaper than all of the above

| change | Δ nDCG@10 | evidence | cost |
|---|---|---|---|
| **weighted-sum ordering** instead of ColBERT-only | +0.0080 (django, p=0.046), **+0.0211 (sklearn, p=0.0005)** | §2.6, §7a | one query builder; no reindex, no storage |
| **drop the ColBERT ordering entirely** (plain RRF) | +0.0276 on sklearn short (p=0.005) | §7a | deletes 99.6% of stored bytes and 84% of latency |
| **`max_chunk_tokens` 512 → 364** | +0.0108 (django, p=0.030) | §2.5 | one reindex |

### F. Considered and not pursued

- **Binary/scalar quantization of ColBERT** — `qdrant.md` already names it;
  pointless to optimise the storage of a stage whose contribution is unproven.
- **Query expansion / HyDE / doc2query** — real published gains, but each adds
  an LLM call to the query path, and mindex's whole design premise is that
  search is cheap and `/research` is where the model lives.
- **Fine-tuning an embedder on this codebase** — the largest possible gain and
  the largest possible overfit; needs held-out corpora the benchmark does not
  yet have.
- **An LSP client for exact resolution** — refused on rule 10 in CLAUDE.md,
  for reasons unrelated to quality.

### The recommended order, on this evidence

1. **Fix the combination rule** (E, row 1 or 2). Two corpora, both significant,
   zero storage cost, no reindex. This is the only change currently supported
   by direct measurement on this repo.
2. **Then a cross-encoder reranker** (B). Best published gain per byte in the
   whole table, and it is what would make the ColBERT question moot rather than
   answered.
3. **Then evaluate a code embedder** (A) — largest published gain, largest
   disruption, and the sparse-head loss must be replaced first, which makes
   step 1 a prerequisite rather than an alternative.
4. `max_chunk_tokens = 364` (E, row 3) whenever a reindex happens anyway.
5. Token pooling (C) only if ColBERT survives step 1.

## 7c. F5 — A CODE EMBEDDER BEATS THE ENTIRE PIPELINE (added last)

django, short queries, n = 1 115. **Both sides ranked by exact brute-force
cosine**, over the identical chunk set, with the identical `docs/**` exclusion.
`bench/baselines/external_embedder.py`.

| system | nDCG@10 | MRR@10 | R@1 | R@10 |
|---|---|---|---|---|
| BM25 / FTS5 | 0.2831 | — | — | — |
| BGE-M3, dense head only | 0.3332 | 0.3266 | 0.1539 | 0.4950 |
| **mindex, whole deployed pipeline** | 0.3549 | 0.3468 | 0.1671 | 0.5215 |
| **CodeRankEmbed, one dense head** | **0.4060** | 0.4076 | 0.2110 | 0.5693 |

| comparison | stratum | n | Δ | 95% CI | p |
|---|---|---|---|---|---|
| CodeRankEmbed vs BGE-M3 dense | all | 1115 | **+0.0727** | [+0.055, +0.091] | **0.0001** |
| " | obvious | 604 | +0.0818 | [+0.057, +0.108] | 0.0001 |
| " | mixed | 363 | +0.0644 | [+0.032, +0.097] | 0.0004 |
| " | non-obvious | 148 | +0.0562 | [+0.015, +0.104] | 0.014 |
| CodeRankEmbed vs **the whole pipeline** | all | 1115 | **+0.0511** | [+0.033, +0.070] | **0.0001** |
| " | obvious | 604 | +0.0663 | [+0.041, +0.091] | 0.0001 |
| " | mixed | 363 | +0.0423 | [+0.009, +0.076] | 0.013 |
| " | non-obvious | 148 | +0.0104 | [−0.042, +0.064] | 0.70 |

**A single dense vector from a 137M-parameter, 547 MB, MIT-licensed model beats
three heads plus a ColBERT rerank by +0.051** — about **8× the chunk-window
effect and 14× ColBERT's own contribution**, on the same corpus and metric.

**The comparison is not rigged by search exactness.** Qdrant's HNSW was
measured against exact brute force on the same stored BGE-M3 vectors:
Δ = +0.0004, CI [−0.000, +0.002], p = 0.31. Approximation costs nothing here,
so both arms are effectively exact.

**The one stratum that does not confirm** is `non-obvious` against the full
pipeline (+0.0104, CI through zero, n = 148) — the stratum where query wording
and code share nothing. Against BGE-M3's dense head alone it does confirm
(+0.0562, p = 0.014), so what survives there is the rest of mindex's pipeline,
not BGE-M3's vector.

**Throughput is NOT measured.** 30 chunks/s at fp32 with the naive O(seq²)
attention path (flash-attn absent; the script now says so and records it in
every row's provenance). BGE-M3's 133 chunks/s comes from a hand-tuned server
that bypasses FlagEmbedding's double forward. **These two numbers are not
comparable and neither belongs in a decision.** See error 5.9 — the fp16 arm
was attempted and did not actually run.

**What this does not settle.** One corpus, one language (Python), prose
retrieval unmeasured, and CodeRankEmbed has **no sparse head** — its +0.051 was
won without one, while BGE-M3's sparse head was the strongest single arm on
scikit-learn. A `CodeRankEmbed + FTS5` hybrid could be better than this or
worse; nothing here says which.

## 8b. The heterogeneous proposal — analysis

Proposed 2026-08-05: **BGE-M3 keeps the prose (markdown), a code-specialised
model takes the code, ColBERT is deleted outright, a cross-encoder becomes the
second stage.** Recorded here because it is coherent, it is cheaper than any
single alternative in §8a, and one part of it is load-bearing in a way that is
easy to miss.

### Why the cross-encoder is not an optional second step here

Prose in BGE-M3's space is 1024-d; code in a code model's space is 896-d or
768-d. **These are different vector spaces and their cosines are not
comparable** — no single query can score against both and produce one ordering.

What rescues it: **RRF fuses ranks, not scores**, so it is model-agnostic by
construction. Two ranked lists in, one list out, no calibration assumed. And
then the cross-encoder rescores the top-k **on one scale**, because it reads
(query, text) pairs and does not care which retriever produced them.

So in a heterogeneous index the first stage owes only **recall**; **ordering**
belongs to the cross-encoder. It stops being an improvement and becomes the
component that makes the design correct. Build order follows: the cross-encoder
lands *before* the second embedder, not after.

> **WITHDRAWN 2026-08-05 — both halves of this argument are wrong, and §10
> replaces them.** RRF is model-agnostic *and* strength-blind: it gives every
> leg the same say whatever its quality, so fusing a strong leg with a weak one
> costs accuracy. Measured on the archive, equal-weight RRF of CodeRankEmbed
> with the BGE-M3 sparse head scores **0.3805 against 0.4060 for CodeRankEmbed
> alone**, and no RRF weighting recovers it. Score-normalised weighted fusion
> reaches 0.4210. The published record agrees independently: on APPS code
> generation, hybrid RRF scored 33.54 against **BM25 alone at 38.00**
> ([arXiv 2605.14503](https://arxiv.org/html/2605.14503v1)).
>
> And the cross-encoder does not step into the ordering role this paragraph
> assigns it. `bge-reranker-v2-m3` at depth 50 over the deployed pipeline's own
> candidates scored **0.3175 against 0.3209 unreranked** (n = 118, paired) — no
> gain. CoREB ([arXiv 2605.04615](https://arxiv.org/abs/2605.04615)), which
> reports rerankers as delta-over-first-stage on code, finds every
> off-the-shelf reranker negative on at least one code task and
> jina-reranker-v3 negative on all three despite a CoIR of 70.64.
>
> The build order in the last sentence is therefore reversed: **calibrated
> fusion is what makes a heterogeneous index correct, and the cross-encoder is
> a gated maybe.**

### What mindex already has for this

- **`model_id` is already on `projects`, `project_files` and
  `project_file_chunks`** — per-chunk model attribution exists in the schema.
- **Routing needs no classifier.** `programming_language` is stored per file;
  `markdown` → prose model, everything else → code model. The split is by file
  type, which is a fact, not a guess about the query.
- `[model].query_server_url` already establishes that a second embedder
  endpoint is a supported shape (today for a CPU query-side instance).

### The arithmetic

```
now:    colbert 838 MB + dense 2.6 + sparse 0.5  =  841 MB / segment
after:  dense_prose + dense_code ~2.6 + sparse 0.5 = ~3 MB / segment
```

**~280x less storage.** Latency likely *falls*: 252 ms of MaxSim leaves, two
dense queries (~45 ms, parallelisable) and a cross-encoder over top-50 arrive.

### The hole in the evidence, stated first

**This benchmark cannot evaluate the prose half at all.** Queries are lifted
from documentation and the documentation tree is *excluded from the ranking*, so
gold is always a code file. There is no measurement anywhere in §12 of
retrieval *into* `docs/`. "BGE-M3 stays for prose" is therefore an untested
premise, not a supported one.

The fix is cheap and uses the existing builder: a corpus whose gold is the doc
sections themselves, with the inverse exclusion. Until that exists, the prose
half of this proposal rests on nothing measured here.

### Risks worth writing down before building

1. **The cross-encoder becomes load-bearing for correctness**, not just
   quality. If it is unavailable the two lists still merge by RRF, so the
   system degrades rather than breaks — but the degraded mode is the one whose
   ordering was never validated.
2. **Two models, two derivation-version questions.**
   `CHUNKS_DERIVATION_VERSION` already cannot see the embedder's identity (a
   documented blind spot); with two embedders the blind spot doubles, and a
   chunk's `model_id` becomes the only record of which space it lives in.
3. **A new language is now a routing decision.** The **Languages** checklist in
   CLAUDE.md gains a step, and omitting it fails silently — the file gets
   embedded by whichever model the default names.
4. **Cross-model rank fusion is ordinal only.** A code chunk and a prose chunk
   competing for one slot are ordered by RRF over ranks from different models;
   defensible, but the relative calibration is arbitrary above the
   cross-encoder's depth.
5. **Losing BGE-M3 on code loses its sparse head on code.** §7a measured that
   head as the strongest single arm on scikit-learn. BM25/FTS5 must replace it
   for the code half — `bench/baselines/bm25_fts5.py` is the working prototype.

### What can be measured before anything is built

| question | how | cost |
|---|---|---|
| does a cross-encoder beat the current ordering? | rerank the **result files already on disk** — `baselines/cross_encoder.py` | no reindex, no corpus pass |
| does a code model beat BGE-M3 on our code corpus? | embed the exported chunks with jina-code / CodeRankEmbed into a second collection | one corpus embed, no mindex change |
| is prose retrieval any good? | build the inverse corpus (gold = doc sections) | one build, one query pass |
| what does the sparse head actually carry on code? | already measured: §7a | done |

`baselines/cross_encoder.py` exists and runs `BAAI/bge-reranker-v2-m3`, which
was already in the local HF cache. It reranks a completed run's own candidates
and reports the **ceiling** — the share of queries whose gold file was among
the input's 100 candidates at all — because no reranker can exceed it, and a
gain that merely reaches it means the first stage was already good enough and
the *ordering* was the whole problem.

## 10. SECOND ROUND, 2026-08-05 — the leg is not the model anyone expected

`PROTOCOL.md` §5.3 gained families F5-F9 and §12.12 carries the numbers. This
is the narrative half.

### 10.1 The instrument moved to `ranx` where it could, and the move found a bug

`ranx` (Numba-JIT, 25 fusion algorithms, three significance tests) now backs
fusion and cross-checks the metrics. `tests/test_ranx_equivalence.py` recomputes
seven archived runs both ways: nDCG@10, MRR@10 and recall@{1,5,10,20} agree
**exactly**; `map@20` does not, on **one query in 1 475** — the one with 26 gold
files against a cutoff of 20. `score.py` normalises AP by `min(|gold|, k)`,
`ranx` by `|gold|`, following trec_eval's `map_cut`, which unlike `ndcg_cut`
does not cap its ideal. Ours is the convention argued for in `score.py`'s own
docstring, so it stays and the divergence is pinned rather than resolved.

Three properties of `ranx` are load-bearing here and none is documented; all
three were read out of its source and are now pinned by tests: `fisher`/`student`
apply **no** family-wise correction (only `tukey` does), `n_permutations`
defaults to **1000** against the protocol's 10 000, and `ranx` computes **no
confidence intervals** — so `stats.py`'s BCa bootstrap and Holm cannot be
retired.

### 10.2 A code-specialised model is not what wins

| arm | params | django | sklearn |
|---|---|---|---|
| mindex as deployed | — | 0.3549 | 0.5567 |
| CodeRankEmbed (code-specialised) | 137M | 0.4060 | 0.5918 |
| **granite-embedding-english-r2** | **149M** | **0.4448** | **0.6241** |
| Qwen3-Embedding-0.6B | 595M | 0.4540 | 0.6251 |

**CodeRankEmbed is third on both corpora.** §7c reported it beating the whole
pipeline by +0.051 and that stands — it just is not the ceiling. A 149M
*general-purpose* encoder beats the 137M *code-specialised* one by +0.039
(p = 0.0001), at the same size, so the gain measured in §7c was **a 2026 model,
not a code model**. That was the arm granite was added to produce.

This is what CORE-Bench (arXiv 2606.11864) predicts and what CoIR's ranking does
not: CoIR puts CodeRankEmbed at 60.1 against granite-r2 at 55.3, and the order
reverses here. §8a's whole "replace the embedder with a code model" table was
built on CoIR, and the lesson is not that the table is wrong but that a
leaderboard ordering does not survive contact with a corpus — which is exactly
what Chroma reported independently for jina-v3 against text-embedding-3-large.

granite and Qwen3 are statistically indistinguishable on both corpora, so cost
decides: **granite is 4x smaller and measured 3x faster on the same device**
(267 vs 87 chunks/s, both fp16). granite-embedding-english-r2 is the leg.

### 10.3 And then the second leg stopped paying for itself

With BGE-M3's dense head, the sparse head was worth +0.015 and the whole
heterogeneous-index design in §8b was built around keeping it. With granite:

| direction (weights chosen on the first, reported on the second) | vs granite alone | 95% CI | p |
|---|---|---|---|
| django -> sklearn | +0.0048 | [-0.0080, +0.0186] | 0.47 |
| sklearn -> django | +0.0038 | [-0.0023, +0.0099] | 0.22 |

Both point estimates are +0.004, both intervals contain zero, and the django
interval — the tight one, n = 1 115 — has an **upper bound of +0.0099, below the
protocol's own smallest meaningful effect**. The sparse head was compensating
for a weak dense vector; it was not supplying a lexical signal this task needs.

**RRF is worse than the single leg it fuses**, in both directions (0.4164 vs
0.4448; 0.6200 vs 0.6241). Rank fusion's strength-blindness, measured directly.

### 10.4 What this does to the architecture

The plan this round was written against had N coexisting dense legs, a learned
sparse leg, calibrated weighted fusion, and a routing rule by
`programming_language`. On this evidence the code path collapses to **one dense
vector per chunk**: no fusion rule, no weights to tune and publish, no BGE-M3 in
the code path at all, and the ColBERT deletion comes free with it.

What still stands between that and a decision:

- **Prose is unmeasured (F9).** Gold is always a code file and `docs/**` is
  excluded from the ranking, so "BGE-M3 keeps the markdown" rests on nothing.
  If granite wins there too, the routing question disappears entirely and with
  it the last reason for a second embedder.
- **Both corpora are Python**, and both query sets are documentation prose.
  CoIR measures BM25 varying **56x** across its own datasets, so an
  identifier-heavy query set could restore the lexical leg. The nearest stratum
  here is `obvious`, where fusion is *negative* (-0.0072).
- **Russian queries** against an English-only encoder are untested. granite has
  a multilingual sibling at 311M, already in the registry, unrun.

## 9. Open questions I could not answer

- **Why does the weighted sum lose on `non-obvious`** (−0.0137) while winning
  overall? Underpowered (n = 148), but the sign is opposite to everything else.
- **Is 364 a real optimum or a plateau?** 256 is not distinguishable from
  either 512 or 364. A 320/400/448 sweep would say, at one reindex each.
- **Does the query-length effect transfer to code-shaped queries?** All of
  §2.4's length analysis is on documentation prose.
- **What does a caller lose from a narrower chunk?** Less surrounding code per
  hit. File-level nDCG is blind to it and this harness cannot see it. It is a
  real cost of the F3 recommendation.
- **Does the sparse head's dominance on scikit-learn and MLDR mean the fusion
  weights should be corpus-adaptive?** That is a much larger question and
  probably the wrong ladder rung.

---

## 11. THIRD ROUND, 2026-08-06 — the evidence was acted on, and what it does not cover

§§0–9 say in the present tense that nothing has been decided. That was true when
they were written and stopped being true the next day. **Retrieval v3 shipped**:
one dense leg from a registry of Qwen3-Embedding models over an
OpenAI-compatible `/v1/embeddings`, no sparse leg, no ColBERT, no fusion, no
rerank, per-`(project, model)` Qdrant collections at `_v3`, and a restarted
SQLite lineage. This section is what a reader needs in order to check that
decision against the evidence rather than against the narrative that preceded it.

### 11.1 The number the release rests on, which none of §§0–10 contains

Everything above is an **arm**: a model scored offline by brute-force cosine
over exported chunks, or a stage of the old pipeline switched off. None of it is
mindex. The shipped system had never been measured end to end, and the release
was first written around §10.2's `0.4540` — an offline numpy arm at window 512.

Measured 2026-08-06, both systems driven through `POST /v0/{guid}/search`,
same corpus, same query set, same instance ids, same cutoff
(PROTOCOL §12.15, `results/v3-vs-v2__django-docs-short.stats.json`):

| | v3 shipped | v2 shipped | Δ | 95% CI | p |
|---|---|---|---|---|---|
| django-docs-short, n = 1 115 | **0.4563** | 0.3549 | **+0.1014** | [+0.0832, +0.1190] | 0.0001 |

Three things worth saying plainly.

- **The lower bound of the interval is 8× the δ this project uses and ~3× the
  strictest δ it ever measured.** That matters because the δ change is itself a
  recorded deviation (PROTOCOL §11): this result does not depend on it.
- **It is a system comparison and not a model ablation.** The embedder, the
  chunk window and the tokenizer that measures it all move together. §10.2 is
  where the model is isolated; nothing attributes +0.1014 to any one of them.
- **The win is not confined to the stratum a lexical baseline already wins.**
  `obvious` +0.1343, `mixed` +0.0728 (both clear of zero), `non-obvious` +0.0375
  with a CI through zero at n = 148. §3.0.1 exists to catch exactly the failure
  where a pooled win lives entirely in `obvious`; this is not that.

**One corpus.** There is no v3 run on scikit-learn — the sklearn Qwen3 arm in
§10.2 is the offline embedder, not the server.

### 11.2 The shipped model is not the arm F5 selected

§10.2 concluded **granite-embedding-english-r2 is the leg**: indistinguishable
from Qwen3 on both corpora (p = 0.20 django, 0.91 sklearn) and cheaper, so the
tie broke on cost. v3 ships **Qwen3-Embedding**.

The reason is in `docs/claude/retrieval-v3.md` §0 and it is a preference with a
rationale, not a measurement: multilingual queries — this deployment's are often
Russian, and no corpus here has ever contained one — plus a ladder of three
sizes (0.6B/4B/8B) sharing a single tokenizer, which is what makes a size change
a re-embed (`mindex-index --vectors-only`) rather than a re-slice.

It is stated here because a reader who checks this document against the code
finds the mismatch either way, and §9's own list already named the Russian case
as untested. What would settle it is a Russian query set against granite's
multilingual sibling (311M, already surveyed, unrun).

### 11.3 What F10 established, and what it did not

Round 3 built the identifier query set (PROTOCOL §3.4) to ask the one question
§10's limits left standing: both corpora are Python and both query sets are
documentation prose, so does a lexical leg earn its slot when the *query* is an
identifier? Declared as a family with three pre-registered routes to a negative
result before the corpus existed.

**F10 has no verdict.** Only scikit-learn has been run; django, the held-out
fusion in both directions, and a v3 noise floor are all outstanding
(PROTOCOL §12.14). What the scikit-learn arms did show, and what makes the
question worth finishing:

- **The identifier projection is the dense leg's best arm, not its worst** —
  0.5915 against 0.5222 on the full bug report. The corpus was built on the
  premise that identifier queries are where a lexical leg would earn its slot.
- **Perturb the literal string and BM25 collapses** to 0.0651, near the random
  floor, while dense holds 0.4581. Whatever a lexical leg offers is conditional
  on the caller spelling the name correctly.
- **`symbols` recovers nearly all of BM25's signal** (0.2513 against 0.2796).
  If a positive result ever arrives, it argues for routing to a tool that
  already ships as readily as for a second leg.

**F6 was withdrawn unrun** — its comparator was BGE-M3's sparse head, which no
longer exists.

### 11.4 Corrections made to this record on 2026-08-06

None of these changes a conclusion; all of them were things a reader could not
check.

- The **v3-vs-v2 comparison** did not exist as a written result. It does now
  (§11.1, PROTOCOL §12.15).
- **"TOST" and "Holm–Bonferroni"** named procedures the code does not implement.
  Relabelled and disclosed; PROTOCOL §11.
- **The noise floor** was run without reindexing, which is not what §5.1
  specifies, and δ moved 30× on the result. Filed as an amendment, and PROTOCOL
  §12.5 now separates same-index from cross-index comparisons — the second has
  no measured floor, and the window result is one of them.
- **The 364 window** was reported with different digits here and in PROTOCOL.
  Re-derived from the artefact: `0.3657`, Δ **+0.0108**, CI [+0.0012, +0.0208],
  p = **0.030**, and `results/F3-364-vs-512__django-docs-short.stats.json` is
  now the record both documents quote. It is also relabelled **exploratory**:
  the F3 that was pre-registered is a different experiment.
- **`pipeline_ablation.py`** was cited as runnable in three places after being
  deleted. Marked as deleted, with what it produced and where those numbers are
  frozen.
- **Machine state** — database paths, a live server's config file, derived
  project GUIDs — was published in §1 as if it were a handoff note. Replaced
  with the commands that regenerate it.

### 11.5 What to do next, replacing §8

1. **A second corpus for the shipped system.** scikit-learn under v3, end to
   end, paired against its v2 baseline. One number, and it is the difference
   between "measured" and "measured once".
2. **The v3 noise floor, with reindexing** (`noise_floor.py --index-repeats 5`).
   Until it exists, no cross-index comparison in this project has a floor, and
   that includes the chunk window.
3. **Re-sweep the chunk window under the Qwen3 tokenizer**, on both corpora.
   364 is currently a default chosen by an exploratory sweep measured with a
   different tokenizer.
4. **Finish F10**: django's identifier corpus, then the held-out fusion.
5. **A Russian query set**, against Qwen3 and granite's multilingual sibling.
   It is the stated reason for the shipped model and the largest untested claim
   in this file.
6. **Judge 50 sampled queries by hand** (PROTOCOL §9.1) to size the
   unjudged-but-relevant rate. Every absolute number here is a floor until then.
7. **Commit what a reader can check**: the tier-0 fixture, `ci.yml`, and the
   qrels/summary/stats artefacts behind each published number.
