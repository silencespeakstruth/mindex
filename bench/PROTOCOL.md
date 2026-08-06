# bench — evaluation protocol (pre-registration)

**Status: pre-registered. Committed before any measurement was taken.**

This document fixes what will be measured, how, and what will count as a
result — *before* the numbers exist. Everything downstream (`corpora.toml`,
`run.py`, `score.py`, `stats.py`, `docs/BENCHMARK.md`) implements this file.
Where a later measurement contradicts a claim made here, the claim is
retracted in §12 rather than quietly edited.

The reason for the ceremony is specific to this repository. mindex's
documentation currently carries retrieval-quality numbers — `MRR@10 0.3931`,
`recall@10 20/23`, `512 answers 15/23 documentation questions vs 18/23` —
that come from a one-off evaluation which no longer exists as a runnable
thing, over a 23-question set that exists nowhere. `docs/claude/qdrant.md`
states the situation plainly: *"there is no retrieval-quality harness in this
repo."* Those numbers are not necessarily wrong; they are simply not
checkable, and three roadmap items are blocked behind them. This protocol
exists so that the next set of numbers is checkable by someone who does not
trust us.

---

## 1. What is being measured

**One question:** given a natural-language description of behaviour that
already exists in a repository, does mindex's search rank the code that
implements it near the top?

"Show me how SQL caching works", not "here is a bug report, name the files to
fix". The distinction is the whole design, and getting it wrong was this
protocol's largest error (§11).

**mindex is a search engine, not an agent.** It matches a description against
code. It does not reason from a symptom to a cause — that is inference, it
needs a model, and in this system it is `/research`, a separate endpoint with
a separate budget. Scoring `search` on issue localization measures the gap
between matching and inferring, and would answer this release's actual
questions — does ColBERT earn 99.6% of the stored bytes? does `fill_gaps` pay
for itself? — **on a task the component does not perform**. A configuration
that wins there need not win on the queries callers send.

So the primary corpus is **descriptive retrieval**, built from each project's
own documentation (§3). Issue localization survives as a secondary tier
(§6.3), labelled as what it is: behaviour on an unrefined query, useful for
regression detection and for comparability with published work, not evidence
about how the component performs in use.

### 1.1 Explicit non-goals

- **No claim of absolute retrieval quality.** Relevance judgments here are
  incomplete by construction (§9.1). The numbers are valid for *comparing*
  systems and configurations, and that is how they will be reported.
- **No end-to-end issue resolution.** Whether a patch would fix the bug is
  SWE-bench's question, not ours. We measure only whether the right files are
  retrieved.
- **No LLM-judged prose as a headline metric** (see §7).
- **No retrieval change is made on the strength of these numbers in the
  release that introduces them.** Build the instrument, then use it.

---

## 2. Corpora

Index cost is per repository; query count is per instance. So corpora are
selected by a mechanical rule — **highest instance count per repository,
subject to language coverage** — and the rule is stated here so that the
selection cannot be mistaken for a choice made after seeing results.

Instance counts below were measured from the datasets on 2026-08-04 and are
the counts *before* the retention filter of §3.3.

| repo | language | instances | source |
|---|---|---|---|
| `django/django` | Python | 850 · 231 · 35 | SWE-bench full · Verified · Loc-Bench |
| `scikit-learn/scikit-learn` | Python | 229 · 32 · 19 | SWE-bench full · Verified · Loc-Bench |
| `cli/cli` | Go | 397 | Multi-SWE-bench |
| `clap-rs/clap` | Rust | 132 | Multi-SWE-bench |
| `vuejs/core` | TypeScript | 48 | Multi-SWE-bench |
| `BurntSushi/ripgrep` | Rust | 14 | Multi-SWE-bench — **smoke corpus** (§8.1) |

Four languages, ~1 700 instances with third-party ground truth. `django` and
`scikit-learn` are each scored by **two independent ground truths of
different character** — SWE-bench is bug-fix-only, Loc-Bench is 43% feature
requests, performance issues and security vulnerabilities. Agreement or
disagreement between them on the same index is itself a reportable finding.

**Optional extensions**, admitted only by the same density rule and only if
tier-1 wall-clock allows: `sympy/sympy` (386 · 75 · 7, Python),
`sveltejs/svelte` (106, JavaScript), `nlohmann/json` (55, C++),
`fasterxml/jackson-databind` (42, Java).

### 2.1 Corpora considered and rejected

- **SWE-bench Multilingual** — 300 instances over 42 repositories, ≈ 7 each.
  Index cost is per repository, so the ratio is wrong.
- **CoIR / CodeSearchNet** — function-level synthetic corpora, not
  repositories. They measure an embedder, not mindex, and would require
  indexing something that is not a project.
- **The Loc-Bench long tail** — Loc-Bench_V1 spreads 560 instances over
  **165** repositories; over 100 of them contribute exactly one instance.
  Only the head is usable, and taking the head is a declared selection, not
  an accident: the reported Loc-Bench numbers therefore describe the head of
  that benchmark and must not be compared directly against published
  whole-benchmark Acc@k without saying so.
- **C#** (`dotnet/efcore`) — no published localization dataset covers C#. It
  is reachable only through the in-house tier of §6, which is never pooled
  with the published tiers.

---

## 3. Ground truth

Two tiers with two constructions. **§3.0 is the primary one**; §3.1–§3.3 build
the secondary issue-localization tier and are kept numbered as they were so
that existing cross-references still resolve.

### 3.0 The descriptive corpus — a project's own documentation (primary)

A project's documentation is prose written by its maintainers describing its
own code, and Sphinx makes the link machine-readable in two forms:

    .. class:: BaseCache                       directives — the API blocks
    :class:`~django.template.Engine`           inline roles — prose pointers

django carries **3 212** of the first and **8 589** of the second. Nobody wrote
them for a benchmark, which is the same property that made the published
datasets worth using: we do not write our own exam.

**The central rule: an explicit code reference is answer key, never query.**
Every directive and every dotted role is removed from the query and used as
gold; what remains is the natural-language description, which is what a caller
supplies. Code blocks are removed too — a doctest reading
`from django.core.cache import cache` hands over the answer, and a query
containing code measures code-to-code matching rather than the
description-to-code retrieval under test. Measured effect: the leakage rate
(§9.2) falls from 15.5% on the issue tier to **0.1%**.

**Resolution is verified against the source by AST, never assumed.** A
reference is believed only if the file it resolves to really defines the
symbol. Three rules came out of auditing the first build, each of which had
produced a plausible corpus rather than an error:

- **A file that defines nothing cannot be gold.** `django/db/models/__init__.py`
  is 138 lines with zero definitions — a re-export list — and it was gold **146
  times**. Its chunks are import statements: retrieving it answers no question.
  This alone rejected 2 967 of 9 445 references.
- **`currentmodule` is context, not a claim.** It sets the namespace for the
  directives that follow and says nothing about what the prose is about.
- **A dotted path disambiguates a common name.** `CharField` is defined in both
  `db/models/fields` and `forms/fields`; the path in the reference already says
  which, so owners under the named module's directory win.
- **A re-exported name resolves to where it is defined, however it was
  spelled.** The public API rarely lives in the module the documentation names:
  `sklearn.decomposition.PCA` is defined in `_pca.py` and re-exported by
  `__init__.py`. The dotted branch took this step and the *bare-name* branch —
  a name written under a `currentmodule::`, which is how scikit-learn writes
  nearly all of them — did not, so **904 of 1 966 references, 46%,** resolved to
  a package `__init__.py` that defines nothing and were discarded as shims. One
  rule, applied in one place, and the corpus went from 239 instances to 422.
- **A test file is not a definition.** `sgd.rst` describes `SGDRegressor`;
  scikit-learn's `linear_model/tests/test_sgd.py` defines a subclass of that
  name, so the real class scored *ambiguous* and was dropped — while `Ridge`
  and `Lasso`, named in the same section only as alternatives the reader might
  prefer, survived. The gold set came out naming everything the section was not
  about, and it took reading six sampled instances to see it. The rule is
  deliberately narrow: `tests/` (plural) and the two test-file spellings, but
  **not** `test/` — `django.test.Client` is public API, and the wider rule
  silently deleted real gold while fixing the ambiguity.

Every rule above has a case in `sphinx_docs.py`'s self-test, over a synthetic
package built to have exactly these shapes. The resolver is where all five
defects were, and until this it was the one part with no test at all.

**Release notes are excluded**, on the argument that already keeps `CHANGELOG`
out of the issue tier's gold: they describe what *changed*, not what *is*, and
at a commit years later the behaviour they describe may not exist. They were
**28%** of the first build. `internals/` (project process) and `faq/` go with
them.

**The docs tree is excluded from the ranking, not from the index.** The query
is lifted out of a file mindex indexes on purpose, so it would return first by
near-exact match — a tautology. It is dropped through `/search`'s own
`exclude: {paths: […]}`, so the index stays the deployed one and the exclusion
is declared and identical for every system and baseline.

**One commit per corpus, not one per query.** A description of existing
behaviour has no "before the fix" to snapshot, so each corpus is indexed once.
That is what makes the ablation matrix affordable: django is one ~47 GiB index
instead of 1 063 rebuilds.

**Language coverage is the price, and it is real.** A separate documentation
tree exists for Python projects here; Rust and Go put documentation *inside*
the source, where mindex deliberately absorbs a doc comment into the chunk it
documents — so querying with it would retrieve its own chunk by near-exact
match. The descriptive tier is therefore Python-only, and multilingual
coverage stays with the secondary tier.

#### 3.0.1 The difficulty axis, and why it is not optional

Every instance carries **`lexical_overlap`**: the share of the query's content
words (length > 3, split on snake_case and CamelCase) that already appear in
the gold files' identifiers and paths. Buckets: `obvious` ≥ 0.25, `mixed`
≥ 0.10, `non-obvious` below.

A query whose words are the file's own identifiers is one BM25 wins by
construction. Everything dense and ColBERT retrieval exists for lives in the
queries where the wording and the code share nothing. **Pooled, the two average
the only effect worth measuring into invisibility** — so every confirmatory
result in §5.3 is reported per bucket, and a claim about family F1 or F2 that
holds only in the pooled mean is not a claim.

### 3.1 Sources and revisions

| dataset | revision | rows | ground truth |
|---|---|---|---|
| `czlll/Loc-Bench_V1` | pinned in `corpora.toml` | 560 | `patch` → files; `edit_functions` → functions |
| `SWE-bench/SWE-bench` (`test`) | pinned in `corpora.toml` | 2 294 | `patch` → files |
| `SWE-bench/SWE-bench_Verified` | pinned in `corpora.toml` | 500 | `patch` → files |
| `ByteDance-Seed/Multi-SWE-bench` | pinned in `corpora.toml` | 48 per-repo JSONL | `fix_patch` → files |

Revisions are pinned by commit SHA. A dataset that has moved is a changed
experiment and gets a new results archive, not a silent re-run.

### 3.2 Query and gold-set construction

- **Query text is `problem_statement` only** (Multi-SWE-bench: issue title +
  body). `hints_text` is **excluded** — it contains pull-request review
  comments written *after* the fix, which routinely name the file to change.
  Including it would measure a leak.
- **Gold file set** = paths modified by the fix patch, subject to three
  exclusions, each applied uniformly to every system and baseline:
  - **Created files are not gold.** A patch that creates `foo/bar.py` names a
    path that does not exist at `base_commit`; no retriever can return it, and
    counting it would depress every system's recall by a constant nobody could
    explain. Read from the `--- /dev/null` side of the diff, not the
    `diff --git` header, which cannot distinguish creation from modification.
  - **Test files are not gold** — a test is where a bug is *demonstrated*, not
    where it lives. Note that both dataset families already separate
    `test_patch` from the fix patch, so this filter is measured to remove
    nothing on the published corpora (§11). It is retained as
    defence-in-depth and because the in-house commit-derived tier (§6) has no
    such separation: there, a commit's diff contains everything.
  - **Records of change are not gold** — `CHANGELOG`, `RELEASE*`, `NEWS`,
    `AUTHORS`, `LICENSE` and the like. A changelog entry is where a fix is
    *announced*; a system ranking `CHANGELOG.md` first has localized nothing.
    Deliberately narrow: it covers records, not documentation. `doc/rg.1.md`
    stays gold, because a man page describing a flag genuinely is part of the
    change surface.
- **Instances whose gold set is empty after those exclusions are dropped**,
  and each drop reason is counted and published separately.
- **The same fix reaching two datasets is one query with two labels.**
  SWE-bench Verified is a human-validated subset of SWE-bench full, so every
  Verified instance also appears in full. Running it twice would waste a
  reindex and double-count it in any pooled figure; dropping one would make
  "Verified nDCG" — the number comparable to published work — unreportable.
  The work is deduplicated on `(base_commit, gold set)`; the labels are not.
- **Gold function set** (Loc-Bench only) = `edit_functions`.

### 3.3 Snapshot discipline

Every instance carries its own `base_commit`. Indexing one snapshot per
repository would either place an instance's own fix inside the index — which
inflates the score, since the query describes what the fix changed — or lose
gold files that did not yet exist. Because indexing is cheap here, we take
the exact option, matching LocAgent and SweRank:

**Per-instance checkout.** Instances are grouped by repository and ordered by
`base_commit` committer date. For each in turn: check out `base_commit`,
reindex, prune deletions, query, record.

This is affordable only because mindex skips unchanged files by sha256 *and*
derivation version, so a step between adjacent base commits reindexes the
diff rather than the tree. One full index per repository, then N cheap deltas.

Two properties are asserted per instance and recorded in the results:

1. `snapshot_sha == base_commit` — exact, so the fix commit is by
   construction not reachable from the index.
2. Every gold path exists at that snapshot. Instances failing this are
   dropped; the **retention rate is published per corpus**.

**Deleted files must be pruned.** A checkout that removes files leaves them
indexed. The runner calls `POST /drift` after each reindex and issues
`DELETE /projects/{guid}/files` for the `orphaned` bucket. Without this the
candidate set accumulates paths that no longer exist and every later query is
scored against a tree that is not the one on disk.

**Garbage collection is mandatory between instances, not optional.** mindex's
indexing hot path is append-only: on reindex, old chunks are marked `deleted`
in SQLite and new vectors are upserted, with old vectors orphaned in Qdrant
until GC. Measured on the live index, storage runs at ~764 KiB per chunk
(3.7 GiB for 5 079 chunks) and ~22 chunks per file. A django pass is 850
checkouts over ~2 905 indexable files; at even ten changed files per step the
orphaned vectors would exceed the base index many times over. The runner
therefore calls `POST /gc` on a fixed interval (`gc_every_instances` in
`corpora.toml`), and the interval is part of the recorded configuration
because it affects nothing but disk — if it ever appears to affect a metric,
that is a bug worth finding.

**Index-state equivalence is verified, not assumed.** For an evenly spread
sample of 20 instances per corpus, the incrementally-reached index state is
compared against a from-scratch index of the same commit: file set, chunk
count, and chunk boundaries must match. A divergence is a mindex bug, not a
harness artifact, and blocks the run.

**The sample is 1 on a single-snapshot corpus**, which is the descriptive tier:
one checkout, one index, and every later instance querying a byte-identical
state (`index=0ms` on each, nothing soft-deleted, every GC tick reporting
`chunks_removed: 0`). There is exactly one index state in such a run, so twenty
samples verify it once and re-verify it nineteen times. Measured on
django-docs-short before the rule existed: 20 x ~148 s of cold rebuild inside a
55-minute run whose 1 115 queries cost 45 ms each — about 45 of those 55 minutes
bought nothing. The single check runs at the **first** instance, so a wrong
index costs three minutes rather than an hour, and the periodic GC is skipped on
the same grounds. `run.py` prints which of the two regimes it chose; a run that
quietly did less work than the protocol says would be the exact failure this
section exists to prevent.

### 3.4 The identifier query set — a projection, not a new gold set

Declared 2026-08-06, for family F10 (§5.3). Everything measured before it asked
its questions in **documentation prose**: §3.0's descriptive corpus is written
prose by construction, and §3.2's issue tier takes `problem_statement`, which is
a bug report. §12.12 closed the second-leg question on that evidence and named
this as its own limit — CoIR measures BM25 varying **56×** across datasets, so a
corpus of identifier-heavy queries is the one shape that could return a
different answer. This section builds it.

**It is not a new ground truth.** The builder (`build_ident_qrels.py`) reads the
frozen issue-tier qrels, copies `gold_files`, `gold_functions`, `base_commit`,
`repo` and `datasets` **byte-identically**, and changes exactly one field:
`query`. The copy is asserted per instance and a mismatch is a hard failure.
Provenance, the §3.2 exclusions and the §3.3 snapshot verification are therefore
inherited whole and are not re-litigated — which is the only reason a query set
nobody published can carry any weight at all.

**Why the issue tier and not the descriptive one.** This inverts §1's own
argument and the inversion is deliberate. The descriptive tier's gold *is* the
file defining the symbol a section references (`build_docs_qrels.py` resolves
`:class:`/`:func:` roles to their definitions), so an identifier query over it
is gold-by-definition — it would measure that exact strings match exact strings.
The issue tier's gold is the files a fix patch **touched**, and the identifiers
a bug report names are symptom-side: the public API the reporter called, not
the module that has to change. It is the only source here where an identifier
query has non-definitional gold *and* published provenance. §1's objection to
the issue tier was that its queries demand inference; the projection strips the
symptom narrative that demanded it, and the claim is comparative between two
legs on one fixed query set rather than an absolute statement about quality.

**Arms.** All arms of one source instance carry its `base_commit`, so they cost
one checkout between them; `instance_id` is `<source_id>#<projection>`.

| arm | `projection` | query text |
|---|---|---|
| A0 | `prose` | `problem_statement`, verbatim. The reference — and *not* a re-run of §12.12, which measured the descriptive corpus; fusion has never been measured on this tier at all. |
| A1 | `ident` | identifier-shaped tokens only, first-appearance order, deduplicated, space-joined. |
| A2 | `ident-mangled` | A1 with one seeded perturbation per token. |
| A3 | `ident-intent` | A1 plus the issue's title line. The realistic mixed query. |
| C | `symbol-defn` | **positive control**, built from the descriptive corpus: bare symbol name, gold = its defining file. Deliberately tautological. |

**Extraction (A1).** A candidate must carry an underscore, a dot or a camel
boundary — `snake_case`, `CamelCase`, `dotted.name` — be 4 to 40 characters,
and not be an English word of a short stop list. Order is first appearance,
duplicates are dropped, and the list is cut at **12** identifiers: past that a
query stops being a caller naming a few things and becomes a bag of tokens that
retrieves well because it quotes half the file.

The one exemption from the shape rule is an **inline backtick**, which is an
author writing "this token is a name" — so `bisect` counts there and not in a
sentence. **A fenced block gets no such exemption**, and that rule was written
by the audit rather than by reasoning: fences in these reports are dumps, and
under the first draft a single ripgrep instance contributed its version banner,
its `--version` output, a commit SHA and an entire pasted DNA sequence to what
was supposed to be an identifier query. Requiring shape inside fences keeps what
should be kept — `_fetch_all` and `query.py` out of a traceback, the richest
identifier source these reports have — and drops the rest by construction.

**A directory path is not an identifier.** Tokenization does not cross `/`, so
a cited `django/db/models/query.py` contributes its basename and nothing more.
Admitting whole paths would maximise §9.2 leakage by construction — the query
would spell the answer — and that axis already has its own stratum; merging it
into this one would make the corpus tautological in the way this section exists
to avoid.

**Perturbation (A2).** One rule per token, chosen by a seed recorded on the
instance: camel↔snake flip, vowel-drop abbreviation, or a single-character
transposition. A2 is a **synthetic stress arm** — real users mistype in ways a
seeded rule only approximates — so it is reported beside A1 and never merged
into a pooled headline.

**Arm C is calibration and carries no claim.** If FTS5 does not beat the dense
leg decisively there, the apparatus is broken and nothing else in F10 may be
read. It is simultaneously the test of whether a positive result would argue
for a *leg* at all or merely for `symbols`, which already ships.

**Inherited but inert: §4.2's vector-limit exclusion.** The source qrels carry
it, so it is inherited. It was a Qdrant multivector bound on ColBERT and has no
v3 analogue, and identifier queries fall far below it regardless. It is stated
rather than quietly undone: re-deriving the corpus to drop it would break the
byte-identical inheritance that is this section's whole argument.

**`lexical_overlap` is recomputed for the projected query** and will move
sharply upward from A0 to A1 — that is the point of the projection and is
reported in §12.13, not treated as a defect. Bucket thresholds are §3.0.1's,
imported rather than restated.

---

## 4. Metrics

mindex returns chunks carrying `path`, `start_line`, `end_line`. Ground truth
is at file level everywhere and additionally at function level in Loc-Bench,
so both are scored.

Let the ranked result list be deduplicated to files by first occurrence: a
chunk at rank *i* credits its file, and later chunks of an already-credited
file are dropped. Relevance is **binary**: `rel(f) = 1` iff `f` is in the
gold set.

- **Primary — nDCG@10, file level.** `DCG@10 = Σ rel(fᵢ)/log₂(i+1)` over the
  deduplicated list; `IDCG@10` uses `min(|gold|, 10)` ones.
- **Recall@k**, k ∈ {1, 5, 10, 20}: `|retrieved@k ∩ gold| / |gold|`.
- **MRR@10**: reciprocal rank of the first gold file, 0 if none in 10.
- **MAP**: over the deduplicated list, truncated at 20.
- **Recall@k curve**, k = 1..20, reported in full. The consumer is an agent
  that reads the top few results, so the head of the curve is what matters
  and a single cut-off hides it.
- **Acc@k** as LocAgent defines it — 1 iff *every* gold location is within
  top-k — at file and function granularity, for comparability with published
  numbers. **Reported, never gated on**: it is all-or-nothing and therefore
  too coarse to detect the regressions this harness exists to catch.
- **Function-level** scoring on Loc-Bench: a retrieved chunk covers a gold
  function iff their line spans overlap, resolved against
  `project_file_symbols`.

**Aggregation is macro across corpora, never micro.** django's instance count
would otherwise decide every pooled number. Per-corpus, per-language and
per-category figures are always shown beside the macro average.

### 4.1 A metric that was planned and is not being used

The plan for this work named **bpref** as a robustness metric for incomplete
judgments. It is not implemented, because it cannot be: bpref is defined over
judged-relevant *and* judged-non-relevant documents, and these datasets supply
only positives. Every unretrieved-but-relevant file is unjudged, and there are
no judged negatives at all. Reporting bpref here would require inventing the
negatives it is supposed to protect against.

The incompleteness it was meant to address is handled instead by §9.1 — by
measuring the bias rather than by choosing a metric that claims immunity to it.

### 4.2 A query the hardware cannot hold

ColBERT emits one 1024-wide row per query token and a Qdrant multivector holds
at most 1 048 576 elements, so **a query above 1023 BGE-M3 tokens cannot be
scored by any configuration**: `POST /search` answers 503 `qdrant.unavailable`
however far above the limit it sits, because the embedder's own `--maxlen`
truncation still leaves more rows than the store accepts. The bound belongs to
the vector store — mindex derives `STORABLE_TOKENS_CEILING` from the same
constant and documents it as *structural, not configurable, like `VECTOR_DIM`*.

**Such instances are excluded from the corpus**, at build time, counted with
the real tokenizer rather than estimated from bytes, and published. The
alternative — running them and scoring the refusal as zero — is rejected for
one reason: the BM25/FTS5 floor of family F1 accepts a query of any length, so
those zeros would charge mindex, in the comparison specifically meant to be
about retrieval quality, a penalty arising from a storage constraint no
baseline pays. `--keep-over-limit-queries` builds the corpus that includes them
for anyone who wants to check.

**The cost, which must be stated wherever these numbers appear.** The excluded
instances are not a random 8%; they are the longest problem statements. Every
figure from this corpus is therefore a claim about queries **under** that
limit, and says nothing about how any system handles a four-thousand-token
issue report. `build_qrels.py` prints the count on its own line rather than
inside the drop list, because it is the one exclusion here that removes a
systematic slice instead of removing rows nothing could be scored on.

**The runtime backstop stays.** A 5xx during a run is retried three times; what
survives every attempt is recorded as a `refusal` code on the row, scored as an
empty ranking, and printed by `score.py` above every metric. Nothing should
reach it now that the corpus is filtered, and that is the point — if it fires,
either the token count and the store disagree or something else broke, and
neither may pass as a low score.

### 4.3 How deep the scored ranking actually is

Ground truth is at file level and mindex returns chunks, so the scored ranking
is the chunk list deduplicated to files — and **`top_k` chunks is not `top_k`
files**. Measured at `top_k = 20`: a mean of 8.9 distinct files, so recall@20
was recall@9 under another name and the flat k=10→20 tail that produced was
nearly reported as an observation about the retrieval pipeline (§11).

Queries are therefore issued at **`top_k = 100`**, the server's `max_top_k`
default, which yields ~21.7 distinct files. `top_k` is a truncation rather than
a retrieval decision — the pipeline prefetches 200 dense + 200 sparse, fuses and
reranks before cutting — so any value at or below `fusion_limit` (200) returns a
deeper slice of the same ranking, and every configuration and baseline is cut at
the same depth.

`score.py` reports the ranking depth and names any cutoff a query failed to
reach. This is required rather than informative: a short ranking and a badly
ordered one produce the same number, so without it the metric silently redefines
itself.

---

## 5. Statistical procedure

### 5.1 The noise floor comes first

Before any comparison is run or reported, the identical configuration is run
end to end **five times, including reindexing**, and the between-run standard
deviation of every metric is published.

This is not a formality. The embedder is fp16 on a GPU; CLAUDE.md documents
that this host's two backends (`egpu`/`igpu`) are *not* bit-identical and
that the XPU backend returns NaN for padded rows off its default attention
kernel. A benchmark that cannot state its own reproducibility cannot support
a claim about a two-point difference.

**Any difference smaller than 2× the pooled between-run SD is not reported as
a finding**, whatever its p-value.

### 5.2 Comparisons

- **Paired two-sided randomization (permutation) test**, B = 10 000, over
  per-query metric differences, on the shared query set. This is the IR
  standard following Smucker, Allan & Carterette (CIKM 2007). The paired
  t-test is reported alongside for readers who expect it; Wilcoxon is not
  used, as nDCG differences violate its symmetry assumption.
- **Effect size with a BCa bootstrap 95% confidence interval** over queries,
  B = 10 000, for every reported difference. **A p-value is never reported
  without its interval.**
- **Holm–Bonferroni** correction within each pre-declared family (§5.3).
- **Power**: from the observed per-query variance, the query count required to
  detect δ = 0.02 nDCG@10 at 80% power, α = 0.05, is computed and published.
  This is what justifies the corpus size instead of leaving it arbitrary.

### 5.3 Confirmatory families, declared in advance

Everything not in this list is **exploratory**, is labelled as such, and
carries no claim.

- **F1 — lexical floor.** mindex full pipeline vs BM25/FTS5 over the same
  chunk set. One test per corpus.
- **F2 — ColBERT's contribution.** Full pipeline vs dense+sparse RRF without
  the ColBERT rerank; and vs dense-only. Two tests per corpus.
- **F3 — slicer.** `fill_gaps` on vs off; `max_doc_chunk_tokens` 512 vs 1024;
  `doc_semantic_weight` 0 vs 1. Three tests per corpus.
- **F4 — embedder.** BGE-M3 vs each external embedder on the identical chunk
  set. One test per embedder per corpus.

F2 is the measurement `docs/claude/qdrant.md` calls *"the one to build
first"*: ColBERT is 99.6% of stored bytes (838 MB/segment against 2.6 MB
dense), and whether binary quantization and token pooling are worth building
depends entirely on its answer.

**Second round, declared 2026-08-05** (§11), after F1–F3 answered and the
architecture question moved from "tune this pipeline" to "what should the
pipeline be". These families decide a **retrieval leg** — a named,
independently-versioned way of scoring a chunk — and the rule that combines
several of them.

- **F5 — the dense leg.** Each candidate dense embedder vs the BGE-M3 dense
  head, on the identical chunk set, both sides ranked by exact brute force.
  One test per model per corpus. Candidates are MIT/Apache only and named in
  `baselines/external_embedder.py::MODELS`; a model whose prompting convention
  is not recorded there cannot be run.
- **F6 — the sparse leg. WITHDRAWN 2026-08-06, unrun** (§11). It was framed as
  "each candidate sparse retriever vs the BGE-M3 sparse head", and that
  incumbent no longer exists: F7 removed the second leg and the v3 migration
  removed BGE-M3 with it. A family whose comparator cannot be built is not
  pending, it is void, and leaving it standing would read as work still owed.
  The question it was for — *should anything lexical score a chunk here* — is
  live and is asked by F10 on the corpus F7 named as its own limit.
- **F7 — the combination rule.** The chosen fusion rule vs the deployed
  ordering, and vs the best single leg it contains. One test per corpus.
- **F8 — reranking.** A reranker over the first stage's own candidates vs that
  first stage unreranked, at each declared depth. One test per model per depth
  per corpus. Reported as **delta over first stage** — the CoREB convention —
  and always beside the **ceiling**, the share of queries whose gold was among
  the input candidates at all, because a reranker cannot exceed it and a gain
  that merely reaches it is a statement about the first stage.
- **F9 — prose routing.** On the prose corpus (gold = documentation sections,
  the inverse exclusion of §3.0), each candidate vs BGE-M3. One test per model
  per corpus. This family cannot be run until that corpus exists, and no
  routing claim may be made before it does.

**Third round, declared 2026-08-06** (§11), after F5 and F7 chose one dense leg
and removed the second.

- **F10 — the lexical leg, on identifier-shaped queries.** The chosen fusion
  rule over the dense leg plus an FTS5/BM25 leg, vs the dense leg alone, on the
  §3.4 identifier corpus. One test per corpus per direction, weights trained on
  the corpus the result is not reported on. Reported per arm, per the
  `ident_in_gold` stratum of §9.6, and per §3.0.1 bucket. This family exists
  because F7's verdict is sound and narrow at once: it PASSed TOST at δ = 0.01
  in both directions, and it did so on documentation prose, which is not the
  query shape a lexical leg would exist to serve. The incumbent is the shipped
  dense-only pipeline; **BM25 is the challenger**, which is the exact reverse of
  withdrawn F6's framing and follows from there being no learned sparse head
  left to defend.

**Two rules bind every family from F5 onward, F10 included, and both exist
because of what they cost if omitted.**

*The held-out rule.* Any weight, depth, α or normalisation is chosen on one
corpus and **reported on another**. The effects in play (~0.015 for a fusion
weight) are the size at which tuning and reporting on the same queries stops
being a technicality and becomes the result. `baselines/fusion.py` refuses a
run whose `--train` and `--test` name the same corpus; there is no default that
could quietly be one corpus.

*Search is exploratory; the comparison is confirmatory.* A sweep over fusion
methods, normalisations, weights or rerank depths is a **search**, is labelled
exploratory, and carries no claim — whatever its p-values. What is confirmatory
is the single pre-declared comparison of the **one** rule the search selected,
against the incumbent, on the corpus the search did not see. Reporting the best
cell of a 36-cell sweep as a finding is the same error as reporting an
unadjusted maximum, and no correction repairs it.

### 5.4 Stratified reporting

Every confirmatory result is additionally broken down by language, by
Loc-Bench issue category (bug / feature / performance / security), by gold-set
size, and by the leakage stratum of §9.2. A regression confined to one
language or to feature requests must not be masked by a pooled mean.

### 5.5 The regression gate is a non-inferiority test

A CI gate built on a significance test is useless: it fails on "no
significant improvement", which is the normal state of a correct change. The
gate is **TOST** (two one-sided tests) against the stored baseline, with a
pre-registered margin δ:

> H₀: the candidate is worse than baseline by ≥ δ. Rejecting H₀ passes.

**δ is fixed by a rule declared now and a number filled in later:**
δ = 2 × the pooled between-run SD from §5.1, rounded up to the nearest 0.005,
with a floor of 0.01 nDCG@10. The rule is fixed before the data exists; the
value is derived from a study that involves no comparison. The resulting
numbers are recorded in §12 when §5.1 completes.

### 5.6 Stopping rules

- The corpus is frozen when `build_qrels.py` completes and its output is
  committed. Instances are never added or removed after a result is seen.
- No interim looks. A tier-1 run is analysed once, when complete.
- A run that aborts is discarded whole and rerun; partial runs are never
  analysed, because their instance ordering is chronological and therefore
  not exchangeable.

### 5.7 Feasibility gate for F10, checked before its comparison

An identifier projection can destroy a query rather than reshape it. If it
does, every arm sits near the floor, all differences are noise, and a fusion
comparison run there would report a confident number about nothing.

So one gate is checked **after the first corpus's dense-only arm and before any
comparison**, and it is a property of a single arm, never a difference — which
is what keeps it outside the confirmatory accounting:

> On the corpus being gated, the dense-only arm on A1 must (a) reach nDCG@10 ≥
> **0.5 ×** its own A0 score, and (b) beat the `random` baseline on the same
> queries with a 95% CI lower bound above 0, by `stats.py`'s ordinary paired
> comparison.

Half (a) says the projection reshaped the query rather than destroying it;
half (b) says the arm is off the floor. Neither is a comparison between the two
legs F10 exists to compare, which is what keeps the gate outside the
confirmatory accounting.

Half (b) replaces a "10 × random" threshold this section carried for one day
(§11). That number was arithmetically impossible: `random`'s nDCG@10 is not
near zero on a corpus of a few hundred files — the ripgrep smoke measured it at
**0.1475** — so ten times it exceeds the metric's maximum and no arm could ever
pass. A significance test against the same floor is what the threshold was
reaching for and needs no magic multiplier.

The gate applies to the **confirmatory corpora** (django, scikit-learn). ripgrep
is the tier-0 smoke corpus at n = 11 per arm and carries no F10 claim.

Failing it **withdraws F10 and is itself a publishable §12 result**:
*identifier-only queries at this corpus's difficulty are not answerable by any
leg*, which is a finding about the projection and about what `/search` can be
asked, not a null result about fusion. It is stated this way so that the cheap
outcome cannot be quietly reported as the expensive one.

The gate is deliberately checked on scikit-learn first (§12.13 order): it is the
smaller issue-tier corpus, and a projection that fails there fails everywhere,
so django's 812 per-instance checkouts are never spent to learn it.

**The decision rule, fixed before any F10 arm is scored.**

*Ship a lexical leg* only if **all** of the following hold:

- the held-out Δ is ≥ **+0.01** nDCG@10 with a 95% CI lower bound above 0, in
  **both** train→test directions;
- the effect **survives on the `ident_in_gold = false` stratum**. §12.13
  measures that stratum at 146 instances on django and **10 on scikit-learn**,
  which is below the n ≥ 20 floor `stats.py` applies to every stratum before
  it will report one — so this criterion is testable on django
  alone, and **a positive result reported only on scikit-learn is not a pass**.
  Recorded now, because after the fact it would read as choosing which corpus
  to believe. Confined to `ident_in_gold = true`, a gain is the §9.2 verdict:
  the method has been shown to be good at string matching;
- the lexical leg beats `baselines/symbol_lookup.py` on the same queries. If
  exact symbol lookup recovers the same gold, the answer is "route to
  `symbols`", which already ships, and not "add a leg to `/search`";
- Δ ≥ 2 × the pooled between-run SD of §5.1. **That number does not exist for
  the v3 pipeline** — §12.5 still lists it as pending — so `noise_floor.py`
  must be re-run before any F10 verdict is final. Stated as a gap rather than
  quietly replaced by the 0.01 floor, because a threshold with no measured
  noise behind it is an assumption wearing a rule's clothes.

*Close the question* if TOST at δ = 0.01 PASSes in both directions on the larger
identifier arm (A1 or A3). That is the verdict §12.12 already reached, delivered
on the query shape §12.12 named as its own limit — at which point `llms.txt`'s
retrieval prose stays as it is, F6 stays withdrawn, and the answer is that a
lexical leg was measured where it was supposed to win and did not.

*Underpowered middle* — the CI contains 0 but its upper bound exceeds +0.01:
report as underpowered, compute n for 80% power at δ = 0.02 per §5.2, and leave
the question open. Nothing ships on a point estimate.

---

## 6. In-house tiers (secondary; never pooled with published tiers)

Two gaps the published data cannot cover. Both are labelled in-house, used
for **regression detection** — where consistency matters more than external
validity — and never used for a headline quality claim.

- **Commit-derived localization**, for languages no published set covers
  (C#/`efcore` in particular). Bench4BL tradition: query = commit subject and
  body with paths and code stripped; gold = files touched. Filters: 1–5
  non-test files, no merges, no reverts, no pure-formatting commits, message
  ≥ 20 words. Indexed at the parent commit, so the leakage guard of §3.3
  holds unchanged.
- **Documentation retrieval.** No published set touches markdown, yet mindex
  runs a *second slicer* for it — `MarkdownSlicer`, 1024-token cap, no lower
  bound, semantic-shift boundaries — every claim about which rests on the
  23-question set that no longer exists. A replacement graded question set
  restores the ability to defend, or retract, those numbers.

---

## 7. Research quality

The trap is to score report prose with an LLM judge and present the number as
quality. Three layers instead, in decreasing order of how much they can be
trusted.

- **Objective outcome — gold-location recall.** Loc-Bench instances are
  re-used as research questions. Score per question: does the final report
  cite at least one gold location with a server-assigned verdict of
  `verified`? Binary, so a proportion with a **Wilson score interval**. No
  judge is involved; the server already computes the verdict.
- **Process metrics, already journalled and therefore free.** `research_runs`
  and its children already carry `done_reason`,
  `citations_{total,verified,path_only,unverified}`, `stale_citations`,
  `forced_synthesis`, `out_of_scope_refusals`, steps, turns, tokens, and the
  comparability columns v1.3.0 added (`model_digest`, `embedder_model_id`,
  `server_version`, `prompt_version`, `seed`). A rise in the `unverified` or
  `forced_synthesis` rate is a regression on its own terms.
- **Reliability.** Research is stochastic: n ≥ 5 seeds per question, and the
  between-seed SD is published. No claim about a prompt change may be smaller
  than it — the §5.1 discipline, applied to the other half of the system.
- **The opponent, checked against itself.** `challenge` is run k times on the
  same report and inter-run verdict agreement is measured
  (Krippendorff's α). If the opponent does not agree with itself, its
  verdicts are noise and the derived `trust` column is decoration.

**LLM-as-judge is optional and secondary.** If used at all, it is calibrated
against human labels on a ~50-item subset with Cohen's κ reported; below
κ = 0.6 it is declared uncalibrated and gates nothing.

---

## 8. Tiers

### 8.1 Tier 0 — every pull request, no GPU, minutes

A change to fusion, reranking, ranking or filtering changes no *vector*. So
the indexed state of the smoke corpus (`ripgrep`, one pinned commit) is
frozen as a fixture — SQLite file plus Qdrant snapshot — and the query
embeddings are cached to a file. They are served back through the
**already-existing `[model].query_server_url`** config key, the seam that
exists for a CPU-only query-side embedder. No mindex code changes.

Gate: TOST non-inferiority on nDCG@10 at δ, against the stored baseline.

A change touching slicing, embedding or the collection schema invalidates the
fixture. This is **detected**, by comparing `CHUNKS_DERIVATION_VERSION`,
`SYMBOLS_DERIVATION_VERSION`, `COLLECTION_SCHEMA_VERSION` and the embedder
digest against the values recorded in the fixture, and the job is routed to
tier 1 with an explicit skip message — never a silently stale pass.

### 8.2 Tier 1 — nightly / pre-release, hours, GPU

All corpora, all ground-truth tiers, all baselines and ablations, full
statistics, all stratifications. Emits `docs/BENCHMARK.md` and a versioned
JSONL archive.

### 8.3 Tier 2 — release, manual, GPU-heavy

Research evaluation (§7).

---

## 9. Threats to validity

Stated here rather than discovered by a reader.

### 9.1 Judgments are incomplete, and the bias is downward

Gold sets list the files a fix *touched*, not every file a competent engineer
would call relevant. A system that retrieves a genuinely helpful file scores
zero for it. Absolute Recall and nDCG are therefore **lower bounds**, and the
headline numbers must never be presented as "mindex finds X% of relevant
code".

The bias is broadly consistent across systems, which is what keeps
*comparisons* valid — but "broadly" is an assumption, so it is measured
rather than asserted: for a random sample of 50 queries, the top-10 retrieved
files that are **not** in the gold set are hand-judged, and the resulting
"unjudged-but-relevant" rate is published per system. If that rate differs
markedly between two systems being compared, the comparison between them is
withdrawn.

### 9.2 Queries can leak the answer

A `problem_statement` may contain a traceback naming the file to change. This
is realistic — users paste tracebacks — so such instances are kept, but the
fraction whose problem statement contains a gold path as a literal substring
is **measured and published**, and every confirmatory result is reported
separately on the leaking and non-leaking strata. A method that wins only on
the leaking stratum has been shown to be good at string matching.

### 9.3 The corpus selection is a choice

Repositories were chosen by instance density and language coverage, and the
rule is stated in §2 before results exist. It is still a choice, and the
Loc-Bench head-only restriction (§2.1) in particular means our Loc-Bench
numbers are not directly comparable to published whole-benchmark figures.
Both are labelled at the point of reporting.

### 9.4 Single hardware, single embedder backend

Every number comes from one machine. The embedder backend, precision and
model digest are recorded per result row precisely because they are known to
matter here and are not portable claims.

### 9.5 Chronological ordering breaks exchangeability across a partial run

Instances are processed in commit order, so a truncated run is a biased
sample of a repository's history. Hence the discard-and-rerun rule in §5.6.

### 9.6 The identifier corpus can only confirm unless it is stratified

§3.4's corpus is built to give a lexical leg its best case, and a corpus built
to give a method its best case will report that the method works. That is not a
reason to build a different corpus — the point of F10 is precisely to look
where F7 said it had not looked — but it *is* a reason to declare, before the
runs, the ways the result can come back negative. A design with no such route is
a demonstration wearing a confidence interval.

Three routes, each measured rather than argued:

1. **The `ident_in_gold` stratum.** Per instance, the builder records whether
   any query identifier occurs as a literal case-sensitive substring in any gold
   file at `base_commit`. Where it does not, the lexical leg has no string to
   match while the dense leg still has meaning to match — so a gain confined to
   the `true` stratum is the §9.2 verdict in another costume: the method has
   been shown to be good at string matching. **The stratum's size is measured in
   §12.13, before any comparison exists to fit it to.**
2. **A2 (`ident-mangled`).** The literal string is absent from the whole
   repository by construction. `unicode61` must fail there; `trigram` may partly
   survive. This is the arm where the lexical leg is *expected* to lose, and it
   answers a question the pooled number cannot: whether "identifier query" means
   "identifier the caller spelled correctly".
3. **Collisions, where matching is free and ranking is the task.** Each instance
   carries `ident_df_min`: the document frequency of its **rarest** identifier —
   the number of files at the snapshot containing it. The minimum is the
   discriminating statistic, not the maximum: it says that even the most
   selective string the caller supplied still matches that many files, so
   matching is free for the whole query and `bm25()` has only IDF and length
   normalisation left to order them with. That is where a dense vector which
   read the surrounding intent should win. Computed by literal case-sensitive
   substring against the tree at `base_commit`, the same predicate as
   `ident_in_gold`, over every identifier the query kept.

   **Only identifiers that occur at all are counted**, and that is not a
   detail. Over the whole list the minimum is 0 for nearly every instance,
   because a bug report reliably contributes one token the tree has never
   contained — so the statistic would silently become route 1's, reported under
   route 3's name. How many identifiers were absent is published separately in
   §12.13, and an instance whose identifiers are *all* absent has no `df_min`
   at all rather than a zero.

Arm C (§3.4) is not one of these routes. It is instrument calibration and
carries no claim in either direction.

---

## 10. Reproducibility

Every result is one JSONL record carrying: corpus repo and `snapshot_sha`,
instance id, dataset name and revision, mindex git SHA,
`CHUNKS_DERIVATION_VERSION`, `SYMBOLS_DERIVATION_VERSION`,
`COLLECTION_SCHEMA_VERSION`, embedder model id, digest, precision and
backend, Qdrant version, full config hash, seed, and the ranked result list
as returned.

Analysis is a separate pass over the JSONL, so re-analysis never requires
re-running. Two tier-0 runs at the same fixture and seed must produce
byte-identical JSONL.

The bench runs against a **dedicated mindex instance** — its own `--bind`,
its own `[database].path`, its own project GUIDs. Collection names derive
from the GUID, so distinct GUIDs are what keep the benchmark off the live
index.

---

## 11. Amendments

Any change to this document after the first measurement is recorded here with
a date and a reason, and the affected results are re-run or withdrawn. Silent
edits defeat the purpose of the file.

The **2026-08-04 rows** were all made *before any retrieval was measured* —
they came out of building and auditing the query set, which is what that step
is for. No result existed that they could have been fitted to. Later rows are
each dated and say for themselves what had already been measured when they were
made, because that is the fact a reader needs and a blanket assurance covering
the whole table would stop being true the moment one row stopped qualifying.

| date | change | reason |
|---|---|---|
| 2026-08-04 | initial pre-registration | — |
| 2026-08-04 | added the records-of-change exclusion (§3.2) | The audit of the built query set found `CHANGELOG.md` in the gold set of **3 of 14** ripgrep instances. A system ranking a changelog first would have scored a hit for "localizing" the bug. Effect: those three gold sets went from 2 files to 1. |
| 2026-08-04 | gold-set glob matching moved from `fnmatch` to `PurePosixPath.full_match` | `fnmatch` is not path-aware: `**/tests/**` compiles to something requiring a literal `/`, so it did not match a **root-level** `tests/` — which is where django keeps its suite. **Measured impact on the published corpora: zero.** Both SWE-bench and Multi-SWE-bench already separate `test_patch` from the fix patch, so no test file ever reached a gold set by this route. The matcher was wrong; the consequence was not realized. Kept because the in-house tier (§6) reads raw commit diffs, where nothing separates tests. Pinned by `build_qrels.py --self-test`. |
| 2026-08-04 | one query, many dataset labels (§3.2) | The first implementation deduplicated across datasets and kept the first, which silently reduced SWE-bench Verified on django to **0 usable instances** — it is entirely a subset of full. Per-dataset reporting is a requirement, so the work is deduplicated and the labels are not. |
| 2026-08-04 | a query above the vector-store limit is **excluded** from the corpus (§4.2) | The smoke run found that `POST /search` answers **503 `qdrant.unavailable`** for any query above 1023 BGE-M3 tokens — 8.1% of django's and 14.3% of ripgrep's. The bound is Qdrant's, not mindex's: ColBERT emits one 1024-wide row per query token and a multivector holds at most 1 048 576 elements, so the request fails however far above it sits. Excluded, counted, and published on its own line, because unlike every other exclusion here it removes a systematic slice — the longest problem statements. `--keep-over-limit-queries` builds the other corpus. |
| 2026-08-04 | **reversal** — the row above replaces one written hours earlier that scored these queries **zero** instead of excluding them | The first decision looked only at mindex, and it is wrong for the comparisons this corpus exists to support. The BM25/FTS5 floor of family F1 accepts a query of any length, so scoring mindex zero on queries no vector store could hold would charge it — in the one comparison meant to be about retrieval quality — a penalty arising from a storage constraint no baseline pays. Recorded as a reversal rather than an edit because the original reasoning was published, and because the change *raises* mindex's headline number, which is exactly the direction a reader should be able to audit. Made before any comparison was run. |
| 2026-08-04 | `top_k` raised from 20 to 100 chunks per query (§4.3) | Ground truth is at file level and mindex returns chunks, so the scored ranking is the chunk list deduplicated to files — and the two depths are not close. At `top_k = 20` the 12 ripgrep queries returned a **mean of 8.9 distinct files** (min 6, max 12): **12 of 12 could not reach the deepest cutoff and 9 of 12 could not reach the primary one**, so recall@20 was recall@9 wearing another name, and the flat k=10→20 tail it produced was about to be reported as an observation about the retrieval pipeline. 100 is the server's `max_top_k` default and yields ~21.7 distinct files. Safe because `top_k` is a **truncation, not a retrieval decision**: the pipeline prefetches 200 dense + 200 sparse, fuses and reranks before cutting, so any value at or below `fusion_limit` (200) is a deeper slice of the identical ranking, and every configuration and baseline is cut at the same depth. `score.py` now reports the ranking depth and names the cutoffs it fails to reach, because this class of error is invisible in the output — a short ranking and a bad ranking score the same. |
| 2026-08-05 | two resolver rules added to §3.0 (re-export through a bare name; a test file is not a definition) | Both found by **reading sampled instances**, both silent. The first cost scikit-learn 46% of its references, because that project writes `.. currentmodule:: sklearn.decomposition` and then `:class:`PCA``, and only the dotted branch knew how to follow a re-export — corpus 239 → 422 instances. The second **inverted** gold sets: a test double named `SGDRegressor` made the real class ambiguous, so the section describing it was scored against `Ridge` and `Lasso`, mentioned there only as alternatives. Neither would have failed a test, so the resolver now has one — a synthetic package carrying every shape, exercised by `sphinx_docs.py`'s self-test. Made before any descriptive retrieval was scored. |
| 2026-08-05 | **families F5–F9 declared** (§5.3), with the held-out rule and the search-vs-comparison rule | The first round asked what each stage of the deployed pipeline contributes. It answered, and the answer moved the question: the largest measured effect in the whole investigation is the *embedder* (+0.051 nDCG@10 for a 137M code model over the entire three-head pipeline), and the second largest is the *combination rule*, not any stage it combines. So the second round decides a set of retrieval legs and the rule that fuses them, and it needs families declared before the runs that choose them. Declared **after** F1–F3 were published and **before** any F5–F9 arm was scored. The two binding rules are not decoration: a fusion weight is worth ~0.015, which is the scale at which choosing and reporting on the same queries *is* the result, and a 36-cell method sweep reports a maximum that no multiple-comparison correction can turn back into a test. |
| 2026-08-05 | `map@20` is retained as `score.py`'s and excluded from the `ranx` migration | The metric arithmetic was cross-checked against `ranx` over seven archived runs on both corpora (`tests/test_ranx_equivalence.py`). nDCG@10, MRR@10 and recall@{1,5,10,20} agree **exactly**. `map@20` does not, on exactly one query in 1 475: `score.py` normalises AP by `min(\|gold\|, k)` and `ranx` — following trec_eval's `map_cut`, which unlike `ndcg_cut` does not cap its ideal — normalises by `\|gold\|`. The one query has 26 gold files against a cutoff of 20. `score.py`'s convention is the one argued for in its own docstring (a query with more gold than the cutoff must still be able to score 1.0, or the metric is measuring the query), so it stays and the divergence is pinned by a test rather than resolved. No published number changes. |
| 2026-08-05 | **the measured task changed**: descriptive retrieval from project documentation replaces issue localization as the primary corpus (§1, §3) | The original §1 justified issue localization as "the task mindex is actually used for". That claim was wrong, and it was load-bearing — it chose the entire corpus. Localizing a bug from its symptoms requires **inference**; mindex performs **matching**, and the inference belongs to `/research`, a different endpoint. The consequence is not that the numbers were merely less relevant: family F2 asks whether ColBERT earns 99.6% of stored bytes, and answering it on a task the component does not perform can select the wrong configuration with a confidence interval attached. Ground truth for the replacement is each project's own Sphinx documentation — prose written by its maintainers describing its own code, with the link made explicit by directives and roles (django: 3 212 and 8 589 of them). Still not an exam we wrote. Issue localization is retained as a secondary tier and relabelled. Raised by the author of the request, not by the harness. |
| 2026-08-06 | **family F10 declared** (§5.3) with the identifier query set (§3.4), its stratification requirement (§9.6) and its feasibility gate (§5.7) | §12.12 answered the second-leg question and stated its own limit in the same breath: both corpora are Python and **both query sets are documentation prose**, while CoIR measures BM25 varying 56× across datasets. That limit was left standing, and it is the one shape where the answer could differ — so it is now a family rather than a caveat. Two things make it a measurement rather than a demonstration: the gold set is inherited byte-identically from the frozen issue tier (only `query` changes), and the three routes to a negative result are declared in §9.6 before any run. Declared **before** the corpus was built and before any identifier retrieval was scored. |
| 2026-08-06 | F10's feasibility gate: "10 × the `random` baseline" replaced by a significance test against it (§5.7) | The threshold was arithmetically impossible and would have withdrawn F10 unconditionally. `random`'s nDCG@10 is nowhere near zero on a corpus of a few hundred files — the ripgrep plumbing smoke measured **0.1475** — so ten times it exceeds the metric's maximum. What the threshold reached for was "this arm is off the floor", and a paired comparison against the same `random` run says that without a multiplier. Found by running the tier-0 smoke, which is a plumbing check and not a comparison; **no F10 arm has been scored on a confirmatory corpus**, so there is no result this could have been fitted to. The other half of the gate (A1 ≥ 0.5 × A0) is unchanged. |
| 2026-08-06 | **family F6 withdrawn, unrun** (§5.3) | It was declared as "each candidate sparse retriever vs the BGE-M3 sparse head". F7 then removed the second leg entirely and the v3 migration removed BGE-M3, so the family's comparator cannot be constructed. Recorded as a withdrawal rather than deleted, because an unrun pre-registration that silently disappears is indistinguishable from one that was run and disliked. |
| 2026-08-06 | **retraction**: `top_k = 100` is no longer justified by "a truncation, not a retrieval decision" (§4.3, and the 2026-08-04 row above) | That argument rested on the pipeline prefetching 200 dense + 200 sparse, fusing and reranking before cutting, so that any depth ≤ `fusion_limit` was a deeper slice of one ranking. Under v3 `/search` asks Qdrant for `top_k` directly — no prefetch, no fusion, no rerank — so the depth **is** a retrieval decision and a deeper cut is a different HNSW search. **No published number changes and no result is withdrawn**: every arm within a comparison is cut at the same depth, which is what those comparisons rest on. What is withdrawn is the reason the choice was safe; the value stays at 100. The comment carrying the false claim in `run.py` is annotated rather than silently corrected. Should F10 ship a lexical leg, the original argument becomes true again and this row is what says why. |
| 2026-08-06 | `baselines/pipeline_ablation.py` deleted; `slicer_sweep.py --ablation` removed with it | The script queried Qdrant directly through BGE-M3's binary protocol (`MAGIC = b"BM3\x01"`) and its arms were `full` / `no-colbert` / `dense-only` / `sparse-only` — a pipeline v3 does not have. It could not run against the current server, and a benchmark script that produces nothing while *looking* runnable is worse than an absent one. F2 and F3 remain fully readable: their arms are frozen in `results/F2__*.json` and `results/F3__*.json`, and re-running a family whose incumbent no longer exists was never possible anyway. §12.7 and §12.10 keep their references to the script as the historical record of how those numbers were produced. |
| 2026-08-06 | `run.py` reindexes on commit change alone, not on `single_snapshot and` commit change (§3.3) | The guard fired only for the descriptive tier, where every instance shares one commit. The condition it actually needs is the equality by itself: two instances naming the same sha cannot have different trees. This became load-bearing with §3.4, which emits four arms per source instance — on django the old conjunct would have bought 3 248 full reindexes where 812 are required. No published number changes: the skipped work was verifying a state that had not moved. `single_snapshot` still governs the equivalence sample and the GC cadence, which genuinely ask whether the corpus is one tree. |
| 2026-08-06 | **the §5.1 noise floor was not run as §5.1 specifies**, and δ moved 30× on the result | §5.1 requires the identical configuration end to end **five times including reindexing**; §12.9 ran `--index-repeats 0`, i.e. seven passes of the query path over one fixed index. It found exactly what that measures — the query path is deterministic — and δ fell from the provisional 0.030 to the §5.5 floor of 0.01. The deviation is recorded now rather than when it happened, and it matters in one direction: the excluded variance is *indexing* non-determinism, so δ = 0.01 is justified for same-index comparisons (F1, F2, F7) and **is not measured for cross-index ones** (F3's window, the v3-vs-v2 system comparison), where the only figure that exists is the retired 0.030. §12.5 now splits the two cases and carries the missing run; §12.10 and §12.15 each state which side they are on. No result is withdrawn: §12.15 clears 0.030 by 3×, and §12.10 is relabelled exploratory by the row below rather than by this one. |
| 2026-08-06 | **F3 was not run as declared**, and the shipped 364 default comes from a sweep | §5.3 declared F3 as `fill_gaps` on/off, `max_doc_chunk_tokens` 512 vs 1024, `doc_semantic_weight` 0 vs 1. What §12.10 ran is a three-arm sweep of `max_chunk_tokens` (512/364/256) — a different knob, and under §5.3's own search-vs-comparison rule a search, which carries no claim and takes no multiple-comparison correction. `[slicer].max_chunk_tokens = 364` nevertheless shipped as the default in v3. Recorded rather than reversed, because the alternative is a default chosen by nothing at all: the sweep is the only evidence anyone has, its effect is in the right direction, and the cost of being wrong is a re-slice. It is labelled exploratory in §12.10 and in the release notes, and the confirmatory version — both remaining corpora, under the Qwen3 tokenizer that actually ships — is on the open list. The pre-registered F3 was never run and is not withdrawn. |
| 2026-08-06 | **"TOST" renamed to what `stats.py` computes**; **Holm–Bonferroni disclosed as off-tool** | Two labels described procedures the code does not contain. `stats.py` tests non-inferiority as `ci95_lower > -δ` — a BCa interval clearing the margin — which is a CI-based non-inferiority check and not two one-sided tests; scipy was listed as a dependency "because TOST needs the t distribution" and is imported nowhere. The column is now `non-inferior (CI)`, and the difference is not cosmetic: on §12.10's `non-obvious` stratum the check reports FAIL where a reader expecting TOST would read a test that was never run. Separately, §5.6's Holm–Bonferroni exists in no script; the adjusted p-values in §12.6 were computed by hand, and every other family reports unadjusted p — F5 alone is nine comparisons. Both are disclosed rather than implemented, because relabelling is checkable today and an implementation would silently restate published numbers. Neither changes a reported effect or an interval. |
| 2026-08-06 | **the shipped model is not the one F5 selected**, and the reason is outside this harness | §12.12 broke a statistical tie (p = 0.20 django, 0.91 sklearn) on cost and named `granite-embedding-english-r2` the leg. v3 ships `Qwen3-Embedding`. The grounds are recorded in `docs/claude/retrieval-v3.md` §0 and are not retrieval quality: multilingual queries — the deployment's own are frequently Russian, which no corpus here has ever contained — and a ladder of three sizes sharing one tokenizer, which is what makes a size change a re-embed instead of a re-slice. Recorded here because a reader who checks the document against the code finds the mismatch, and a benchmark whose selected arm quietly loses to an unstated preference is worth less than one that says where the preference entered. The untested half is in §12.5. |
| 2026-08-06 | **equivalence sampling is 1 on a single-snapshot corpus**, and the periodic GC is skipped there (§3) | The check compares an incrementally-reached index against a cold build. The descriptive tier reaches its index in one step and never moves it, so nineteen of the twenty samples re-verified a state that could not have changed — `index=0ms` on every instance, `chunks_removed: 0` on every GC tick. Measured cost on django-docs-short: 20 x ~148 s of rebuild in a 55-minute run whose queries take 45 ms. No published number changes: the same index is measured by the same queries, and the one surviving check covers the one state that exists. |

---

## 12. Results

Opened as the record of the **pre-measurement studies** — §§12.1–12.5 and 12.9
are that, filled in as each completed and before any comparison ran, and their
heading still said so. It stopped being true at §12.6, and §§12.6–12.8 and
12.10–12.15 are family results and system comparisons. The heading is corrected
rather than the sections moved: splitting the file now would renumber sections
that other documents cite by number, and the dates in each heading already say
which came first.

### 12.1 Query set (`build_qrels.py`, 2026-08-04)

Snapshot verification (§3.3) and the vector-limit exclusion (§4.2) applied to
both corpora against their clones.

| corpus | dataset | published | usable | new queries | dropped |
|---|---|---|---|---|---|
| ripgrep | Multi-SWE-bench | 14 | 12 | 12 | 2 query over limit |
| django | SWE-bench full | 850 | 778 | 778 | 1 created-files-only, 71 query over limit |
| django | SWE-bench Verified | 231 | 215 | 0 | 16 query over limit (all also in full) |
| django | Loc-Bench | 35 | 34 | 34 | 1 query over limit |

- **django: 812 unique queries** carrying 1 027 dataset labels; **ripgrep: 12**.
- **Gold-set size**, django: 641 single-file, 92 two, 33 three, 14 four, 32
  five-or-more. ripgrep: 6 / 1 / 1 / — / 4.
- **Loc-Bench categories present on django**: 13 feature requests, 11 bug
  reports, 8 performance issues, 2 security vulnerabilities.

**Snapshot verification dropped nothing.** Every `base_commit` resolves in the
clone and every gold path exists in the tree at that commit — checked over all
884 pre-exclusion django instances and their 1 048 gold paths. Two things this
rules out, both invisible in the metrics: a diff parser reading the wrong side
of a rename (it would produce paths absent from the tree), and dataset rows
naming commits upstream has since rewritten. Retained rather than retired: it
costs one `git cat-file --batch-check` per instance and is the only thing
between a silent upstream change and a corpus that scores every system slightly
wrong.

The single `added_only` drop is correct: that patch creates files and modifies
none, so there is nothing at `base_commit` for any retriever to return.

**The vector-limit exclusion cost 72 unique django queries and 2 ripgrep
queries** (8.1% and 14.3%). The per-dataset counters sum to 88 on django and
that number is wrong for the corpus: SWE-bench Verified is a subset of full, so
16 of its exclusions are also full's. The unique count is what is reported —
the same double-counting trap the `merged` counter exists for.

### 12.2 Leakage strata (§9.2)

| corpus | query spells a full gold path | query spells a gold basename |
|---|---|---|
| django | 126 / 812 (15.5%) | 147 / 812 (18.1%) |
| ripgrep | 2 / 12 (16.7%) | 2 / 12 (16.7%) |

Roughly a sixth of django queries already name a file in the answer, which is
why every confirmatory result is reported on both strata. A method that wins
only on the leaking stratum has been shown to be good at string matching.

Both leakage rates fell when the over-limit queries were excluded (18.8% →
15.5% on django). That is the expected direction and worth stating: a long
problem statement carries more text, so it is likelier to spell a path
somewhere. The exclusion therefore removed queries that were, on average,
*easier* to leak the answer to — which slightly hardens the corpus rather than
flattering it.

### 12.3 Index-state equivalence (`run.py`, 2026-08-04)

The claim the whole snapshot design rests on — that an index walked forward one
commit at a time equals one built from scratch at the same commit — **holds on
every snapshot tested**: 12 of 12 on ripgrep, compared on the active file set
and on every chunk's `(start_line, end_line)`, read from SQLite because no
endpoint publishes chunk boundaries for a whole project.

The check is not vacuous, and that was worth establishing separately, since a
comparison of two empty states passes. The bench server's log carries **15
distinct project GUIDs** for the run — the corpus project plus one scratch
project per checked snapshot (plus two from earlier interrupted runs) — and the
compared state held 125 files and 1 826 chunks. A scratch project that failed
to index would have surfaced as every file "only in incremental", not as a pass.

Cost, for sizing the tier-1 sweep: the check doubles a corpus. ripgrep's 12
instances took **8.3 minutes** with it, on `@egpu`.

### 12.4 Noise floor (`noise_floor.py`, 2026-08-04, ripgrep/@egpu)

Five identical runs, reindexing from scratch each time, 12 queries. **This is a
provisional figure on a corpus that is being replaced** (§11) and on far too few
queries to estimate a standard deviation well; it is recorded because what it
says about the instrument is not provisional at all.

| quantity | value |
|---|---|
| macro nDCG@10 per repetition | 0.3953, 0.4148, 0.4099, 0.4099, 0.3841 |
| between-run SD (pooled) | **0.0127** |
| queries whose score moved at all | **4 / 12 (33%)** |
| δ by the §5.5 rule | **0.030 nDCG@10** |
| queries for 80% power at δ = 0.02 | 119 (optimistic) … 4 268 (conservative) |

**The instrument cannot currently detect the effect it was designed around.**
§5.2 asks for the query count needed to detect δ = 0.02, and the noise floor
came back at 0.030 — larger. By our own rule nothing under 0.030 is reportable
whatever its p-value, so a 0.02 improvement is not merely underpowered here, it
is unreportable. Either the noise floor comes down or the detectable effect goes
up; both must be stated before any comparison is read.

**Where the noise lives**, narrowed by two direct probes rather than assumed:

- **The query embedding is deterministic.** The same text sent to `/encode` four
  times returned byte-identical responses. A query is always a batch of one, so
  this half is settled.
- **Retrieval on a fixed index is not.** The same query asked twice in a row
  against the *same* project returned a different ranking for **1 of 12**
  queries. Nothing was reindexed and no vector changed, so this is Qdrant —
  score ties, segment ordering, or search parallelism.
- **Reindexing adds the rest**: 4 of 12 across full rebuilds against 1 of 12
  within one index.

The leading hypothesis for the reindex half is **batch composition**. Chunks are
embedded in batches of up to 2 048 with `mindex-index --concurrency 4`, which
splits files across four upload streams, so which chunks share a batch — and
therefore how each is padded — depends on timing. Padding demonstrably matters to
this stack: CLAUDE.md records the XPU backend returning NaN for padded fp16 rows.
Untested, and worth testing, because `--concurrency 1` would make batch
composition deterministic and trade indexing throughput for a lower noise floor,
which is the only lever that buys statistical power without more queries.

### 12.6 The descriptive corpus, and family F1 (2026-08-05)

The first confirmatory comparison. **F1 was pre-declared** (§5.3) and this is
its result, favourable or not.

| corpus | queries | `obvious` | `mixed` | `non-obvious` |
|---|---|---|---|---|
| django | 1 296 | 356 (27%) | 695 (54%) | 245 (19%) |
| scikit-learn | 430 | 20 (5%) | 202 (47%) | 208 (48%) |

The two sit at opposite ends of the difficulty axis by accident of house style:
django's documentation names identifiers, scikit-learn's describes algorithms.
That contrast is the most useful property of the pair, and neither corpus alone
would have it.

**mindex, as deployed:**

| corpus | nDCG@10 | MRR@10 | R@1 | R@10 | R@20 |
|---|---|---|---|---|---|
| django | 0.4289 | 0.4262 | 0.2212 | 0.6026 | 0.7207 |
| scikit-learn | 0.6621 | 0.6731 | 0.4692 | 0.7727 | 0.8380 |

**Against the lexical floor** — SQLite FTS5 `bm25()` over the *identical* chunk
set read out of mindex's own database, the same `TOP_K = 100`, the same
`docs/**` exclusion, the same scorer. Paired randomization test, B = 10 000;
BCa bootstrap 95% CI; both validated by `stats.py --self-test` against analytic
power and nominal coverage before being used.

| corpus | stratum | n | mindex | BM25 | Δ | 95% CI | p |
|---|---|---|---|---|---|---|---|
| django | **all** | 1296 | 0.4289 | 0.3934 | **+0.0355** | [+0.018, +0.053] | 0.0002 |
| django | obvious | 356 | 0.4623 | 0.4409 | +0.0214 | [−0.013, +0.057] | 0.230 |
| django | mixed | 695 | 0.4460 | 0.3984 | **+0.0476** | [+0.024, +0.071] | 0.0001 |
| django | non-obvious | 245 | 0.3316 | 0.3100 | +0.0216 | [−0.019, +0.062] | 0.287 |
| sklearn | **all** | 430 | 0.6621 | 0.6070 | **+0.0551** | [+0.036, +0.075] | 0.0001 |
| sklearn | obvious | 20 | 0.5814 | 0.5046 | +0.0767 | [+0.001, +0.178] | 0.104 |
| sklearn | mixed | 202 | 0.7456 | 0.6668 | **+0.0788** | [+0.048, +0.110] | 0.0001 |
| sklearn | non-obvious | 208 | 0.5887 | 0.5588 | **+0.0299** | [+0.005, +0.054] | 0.020 |

**The finding, stated against the architecture's own premise.** The pipeline
beats the lexical floor on both corpora, and the sign is established. The size
is +0.036 to +0.055 nDCG@10 — and it is **concentrated in `mixed`**. In
`non-obvious`, the stratum where query wording and code share nothing and
therefore the one dense and ColBERT retrieval exists to serve, the advantage is
the *smallest* of the three on both corpora (+0.022, not established; +0.030,
barely). That is the opposite of the predicted pattern: if semantic retrieval
were doing what it is for, the gap should **widen** as lexical overlap falls.

This does not yet indict ColBERT specifically — F1 compares the whole pipeline
to BM25 and cannot attribute. Attribution is family F2, which `qdrant.md`
already names as the measurement to build first, and this result raises its
stakes rather than answering it.

**The absolute numbers are depressed by corpus composition, and by how much is
measurable.** Two thirds of django's indexed chunks (66.4%) are its test suite,
against 41.0% for scikit-learn — and a test file can never be gold here, since
gold resolves to definitions. Removing test files from the ranking as a
diagnostic raises django's nDCG@10 from 0.4289 to 0.5145 and recall@10 from
0.6026 to 0.6846. **This is not evidence of a ranking pathology, and reading it
that way was the first mistake made with it:** tests are 66.4% of the index and
only 42.3% of top-10 slots, so the ranker demotes them by about a third
relative to chance. It is a fact about what the corpus contains, it applies
identically to every system compared, and it is a large part of the difference
between the two corpora's headline numbers.

**The floor of the scale is not 0.5.** django has 2 701 indexed files and a
median gold set of 1, so a random ranker scores recall@10 ≈ 10/2701 = 0.4%.
Nothing here justifies reading 0.43 as "43% right"; §9.1's incomplete-judgment
bias points the same way, and an audited example is in the run log — a section
on admin facets whose gold is `options.py` (where `show_facets` is defined) and
whose top result was `contrib/admin/filters.py`, a defensible answer that the
gold set cannot contain.

**Caveats that cut against mindex, recorded so they are not discovered later.**
The BM25 baseline searches *mindex's own chunks*, so it inherits the AST slicer
and the gap-fill pass; a plain file-level BM25 would be a weaker floor and a
more flattering comparison, and was not used. Its stopword list is 60 common
English words, which helps it. It is therefore a **conservative** floor, and
the measured advantage is the smaller of the two available readings.

### 12.7 Family F2 — what each retrieval stage contributes (2026-08-05)

The measurement `docs/claude/qdrant.md` calls "the one to build first", and the
first evidence in this repository that bears on it either way.

**Method.** `baselines/pipeline_ablation.py` queries **the collection mindex
built**, with the vectors mindex stored, transcribing `db/qdrant.rs`'s nested
prefetch. mindex exposes one retrieval shape and no flag disables a stage, so
the alternative was editing the path under test. Each query is encoded **once**
and sent to all four arms, so the arms differ only in the query put to the
store.

*The `full` arm reproduces mindex to four decimals over all 1 296 django
queries — 0.4289 against 0.4289, and identical top-10 orderings on a checked
sample of 120.* Without that agreement this file would be measuring something
else, so it is reported first.

| arm | django nDCG@10 | R@1 | R@10 | sklearn nDCG@10 | R@1 | R@10 |
|---|---|---|---|---|---|---|
| full (deployed) | 0.4289 | 0.2212 | 0.6026 | 0.6621 | 0.4692 | 0.7727 |
| no-colbert (RRF only) | **0.4344** | 0.2252 | 0.6019 | 0.6703 | 0.4852 | 0.7722 |
| dense-only | 0.4066 | 0.2019 | 0.5811 | 0.6350 | 0.4445 | 0.7524 |
| sparse-only | 0.4004 | 0.2000 | 0.5608 | **0.6829** | 0.5072 | 0.7827 |

**Removing the ColBERT rerank, paired** (Holm–Bonferroni across the three
pre-declared F2 comparisons per corpus; adjusted p in brackets):

| corpus | stratum | n | Δ (full − no-colbert) | 95% CI | p [adj] |
|---|---|---|---|---|---|
| django | all | 1296 | −0.0056 | [−0.018, +0.006] | 0.355 [0.355] |
| django | obvious | 356 | +0.0122 | [−0.010, +0.034] | 0.286 |
| django | mixed | 695 | −0.0082 | [−0.025, +0.008] | 0.329 |
| django | non-obvious | 245 | −0.0238 | [−0.051, +0.002] | 0.071 |
| sklearn | all | 430 | −0.0083 | [−0.021, +0.004] | 0.205 [0.205] |
| sklearn | mixed | 202 | −0.0093 | [−0.029, +0.011] | 0.352 |
| sklearn | non-obvious | 208 | −0.0044 | [−0.021, +0.013] | 0.601 |

**The finding: ColBERT's measured contribution is indistinguishable from zero
on both corpora, and the point estimate is negative on both.** The 95% upper
bound is +0.006 nDCG@10 on django and +0.004 on scikit-learn. It costs, per
`qdrant.md`'s own measurement, **838 MB per segment against 2.6 MB dense —
99.6% of stored bytes, ~322× dense**, one 1024-wide row per token.

On django the damage is concentrated in `non-obvious` (−0.024, p = 0.07) —
the stratum the rerank exists to serve — while `obvious` improves slightly.
That pattern is what a *lexical* refinement over an already-agreed pool looks
like, not a semantic one. It is one corpus and it is not significant; it is
recorded because it is a direction, and directions are what the next
measurement should be aimed at.

**Fusion, by contrast, earns its 3.1 MB — but not everywhere.**

| corpus | comparison | Δ | 95% CI | p [adj] |
|---|---|---|---|---|
| django | RRF vs dense-only | **+0.0279** | [+0.018, +0.038] | 0.0001 [0.0003] |
| django | RRF vs sparse-only | **+0.0341** | [+0.024, +0.045] | 0.0001 [0.0003] |
| sklearn | RRF vs dense-only | **+0.0354** | [+0.025, +0.047] | 0.0001 [0.0003] |
| sklearn | RRF vs sparse-only | **−0.0126** | [−0.023, −0.002] | 0.0196 [0.039] |

**The last row is the one to read twice, and it reverses a sign.** On
scikit-learn, sparse retrieval **alone** beats the fused pool, and beats the
deployed pipeline outright: 0.6829 against 0.6621, Δ = **+0.0208**
[+0.006, +0.037], p = 0.008. On django the same comparison goes the other way
by −0.034. So "hybrid fusion helps" is **corpus-dependent**, and neither corpus
alone would have shown that — which is the argument for having built a pair
that sits at opposite ends of the difficulty axis (§12.6) rather than one
larger corpus.

A plausible mechanism, and it is a hypothesis: BGE-M3's sparse head is
SPLADE-style, so it expands terms rather than matching them literally, and
scikit-learn's documentation is long prose about algorithms — the case term
expansion is strongest on. `lexical_overlap` measures overlap with the gold
file's *identifiers*, which is a different thing from what an expanded sparse
query can reach. That is a caveat on the difficulty axis, not a defect in it,
and it is stated because the two are easy to conflate.

**What this does not establish.** One task (descriptive retrieval), two Python
corpora, one embedder, one hardware backend. The noise floor on this corpus is
still unmeasured (§12.5), so no TOST margin has been applied and none of the
null results above are "equivalence" in the formal sense — they are failures to
distinguish, at the stated intervals. No retrieval change is made on the
strength of these numbers, per the release's own non-goals: the instrument was
built first, and this is its first reading.

### 12.8 Query length, and a construct-validity defect in §12.6's corpus

**§12.7's conclusion is narrowed here, by a challenge to it rather than by new
agreement.** The reading offered was "ColBERT's contribution is
indistinguishable from zero"; the objection was that ColBERT is the model's
headline capability, so a null result is more likely a broken measurement than
a broken feature. Checking that produced a defect — in the corpus, not in
ColBERT.

**ColBERT is not broken, and this was established before anything was
reinterpreted.** The `/encode` body parses to exactly its own length
(41 044 of 41 044 bytes); the query's dense vector has norm 1.0001 and every
ColBERT row is 1.000000; there are no NaNs in the query or in the 192 stored
rows of a sampled point; and the `full` arm reproduces mindex to four decimals.
The decisive check: **querying with a chunk's own verbatim text returns that
chunk first at MaxSim 191.999 out of a 192-token query** — the arithmetic
maximum, every query token matching itself at cosine 1.0, runner-up at 153.7.
The mechanism works.

**What it does depends on query length, and the dependence is real:**

| band | n | Δ (full − no-colbert) | 95% CI | p |
|---|---|---|---|---|
| short, < 300 B | 336 | +0.0234 | [−0.002, +0.048] | 0.067 |
| long, ≥ 300 B | 960 | **−0.0157** | [−0.029, −0.003] | **0.023** |

Permuting the band label rather than the sign — an explicit test of the
interaction — gives a difference of 0.0391 at **p = 0.0035**. The sign of
ColBERT's contribution genuinely changes across this axis.

A mechanism consistent with all four length quartiles: MaxSim **sums** over
query tokens, so a 300-token query contributes a few content tokens and several
hundred function words, each of which finds *some* maximum somewhere. The
signal is averaged into the bulk. The competing explanation — that MaxSim
favours long documents, which would also explain the harm — was tested and
**rejected**: the rerank puts slightly *shorter* chunks in the top 10 than the
fused pool does (27.8 lines against 28.7).

**The defect is in §12.6's corpus, and it is ours.** mindex's callers ask short
questions: the MCP `search` tool, `mindex-search.sh`, the VS Code Ask field.
The descriptive corpus has a median query of 562 bytes on django and 1 089 on
scikit-learn, because documentation sections are long — so it measured the
retriever in a regime no caller occupies, and specifically in the regime where
the rerank does harm.

**The short-query corpus** (`-docs-short`, `--short`) fixes it with one
variable changed: the same sections, **the gold set still decided by the whole
section**, and only then the question cut at a sentence boundary to ≤ 200
characters. django 1 115 instances, median 144 B; scikit-learn 360, median
134 B. They read like search input — *"Management Commands. Management commands
can be tested with the call_command function."*

Re-running F2 there:

| arm | nDCG@10 | R@1 | R@10 | obvious | mixed | non-obvious |
|---|---|---|---|---|---|---|
| full | **0.3549** | 0.1671 | 0.5215 | 0.4023 | 0.3420 | **0.1933** |
| no-colbert | 0.3484 | 0.1638 | 0.5088 | 0.3959 | 0.3421 | 0.1701 |
| dense-only | 0.3329 | 0.1539 | 0.4941 | 0.3868 | 0.3198 | 0.1449 |
| sparse-only | 0.3183 | 0.1475 | 0.4628 | 0.3613 | 0.3023 | 0.1819 |

| stratum | n | Δ (full − no-colbert) | 95% CI | p |
|---|---|---|---|---|
| all | 1115 | +0.0065 | [−0.006, +0.019] | 0.30 |
| obvious | 604 | +0.0064 | [−0.009, +0.023] | 0.43 |
| mixed | 363 | −0.0001 | [−0.023, +0.023] | 0.99 |
| non-obvious | 148 | +0.0232 | [−0.008, +0.057] | 0.18 |

**The harm is gone and the ordering by stratum inverts to the predicted one** —
the largest gain now sits in `non-obvious`, the stratum the rerank exists for,
where on long queries it was the largest *loss*. Nothing here is significant on
its own, so this is a direction, not a result.

**What now stands, and what is withdrawn.** Withdrawn: "ColBERT's contribution
is indistinguishable from zero", as stated in §12.7 — it was measured almost
entirely outside the operating regime. Standing: **on queries above 300 bytes
the rerank significantly harms retrieval** (−0.016, p = 0.023), and **on short
queries no benefit has been established either** (+0.0065, CI [−0.006, +0.019]),
which still leaves 99.6% of stored bytes unaccounted for. The two corpora are
not comparable in aggregate — cutting to the first sentence moves the `obvious`
stratum from 27% to 54%, because the leading sentence is where the defined name
sits — so only the per-stratum rows above may be read across them.

Both corpora are retained and reported. The short one is primary for anything
about the deployed service; the long one is what makes the length dependence
visible, and deleting it would delete the finding.

### 12.9 The noise floor is zero, and what that settles (2026-08-05)

`noise_floor.py --index-repeats 0 --query-repeats N --reuse-label baseline`,
django, 1 296 long queries, 7 complete passes over the same index.

| passes | nDCG@10 each | between-run SD | queries whose score moved |
|---|---|---|---|
| 7 | 0.428854 (identical to six decimals) | **0.000000** | **0 of 1 296** |

The query path is deterministic given a set of vectors. So **every same-index
comparison in this document — all of F1 and all of F2 — carries zero
measurement noise**, and δ falls to the protocol's floor of 0.01 rather than to
anything measured. The rule was fixed in §5.5 before the data existed: δ =
2 × pooled SD, rounded up to 0.005, floor 0.01.

This **retires the provisional δ = 0.030** from §12.4, which came from 12
ripgrep queries and included full index rebuilds. For same-index comparisons it
overstated the noise by a factor of thirty, and it was on its way into the
write-up as the reason a 0.02 effect could not be reported.

**Stopped at 7 of 10 passes**, deliberately and recorded here rather than
quietly: 7 × 1 296 = 9 072 observations were identically equal, the remaining
passes were blocking the experiment the run existed to enable, and the risk is
one-sided and stated — more passes could only ever *raise* δ, so an early stop
is the direction that flatters results. Anything the missing passes would have
found is rarer than 1 in 9 072, and δ is at its floor regardless of whether the
true SD is 0 or 0.001.

Re-reading F2 through the formal gate, in the operational direction — *can the
ColBERT rerank be removed?*

| corpus | Δ (no-colbert − full) | 95% CI | TOST at δ = 0.01 |
|---|---|---|---|
| django, long queries | +0.0056 | [−0.006, +0.018] | **PASS** |
| django, short queries | −0.0065 | [−0.019, +0.006] | **FAIL** |

Removing it is formally safe on long queries. On short queries **neither
direction is established** — the interval is wider than the margin, which is a
statement about this corpus's size and not about ColBERT.

**Power, now computable.** σ_d = 0.21 per query for the ColBERT contrast. At
80% power, α = 0.05: **~850 queries to detect δ = 0.02, ~3 400 to detect
δ = 0.01.** The short corpus has 1 115. So an effect of the size actually
observed (+0.0065) is beyond this corpus's reach by a factor of three, and no
amount of re-running changes that — only more queries do. scikit-learn's short
corpus adds 360; reaching 3 400 needs two or three more corpora of this size.
Until then the honest verdict on ColBERT at short queries is **not
established**, never "does not work".

### 12.10 Family F3 — the chunk token window (2026-08-05)

Raised as a hypothesis with a mechanism: a short query carries few content
tokens, and a 512-token chunk averages them against several hundred unrelated
ones, so a narrower window should sharpen the vector. Measured by full reindex,
short-query corpus, django, n = 1 115.

| | 512 | 364 | Δ |
|---|---|---|---|
| nDCG@10 | 0.3549 | **0.3657** | **+0.0108** |
| MRR@10 | 0.3468 | 0.3581 | +0.0113 |
| recall@1 | 0.1671 | 0.1767 | +0.0096 |
| recall@5 | 0.4062 | 0.4193 | +0.0130 |
| recall@10 | 0.5215 | 0.5275 | +0.0060 |
| recall@20 | 0.6378 | 0.6379 | **+0.0001** |
| chunks/file | 9.71 | 10.90 | +12% |

Paired: **Δ = +0.0108, 95% CI [+0.0012, +0.0208], p = 0.030**, and it clears
the non-inferiority margin at δ = 0.01.

*(Re-derived from the artefact on 2026-08-06 and corrected. This table and
`FINDINGS.md` §2.5 had been transcribed by hand and disagreed in the last digit
of every figure — 0.3658/+0.0109/p = 0.028 here against 0.3657/+0.0108/p = 0.030
there. `results/F3-364-vs-512__django-docs-short.stats.json` is now the record
and both documents quote it. Nothing about the conclusion moves; what moved is
that two numbers for one run could not both be checked.)*

**Three qualifications, and together they are why this default is carried as
exploratory rather than confirmed.**

- **This is a cross-index comparison, and no noise floor exists for one.** The
  arms are separate builds — a different window is a different set of stored
  vectors — so §12.9's zero SD, measured over a fixed index, does not apply. The
  only δ ever measured across a reindex is §12.4's provisional **0.030**, which
  this effect does not clear. §12.5 carries the missing measurement.
- **No stratum's interval clears zero**: `mixed` +0.0150 [−0.0017, +0.0333],
  `obvious` +0.0095 [−0.0037, +0.0226], `non-obvious` +0.0056 [−0.0189, +0.0388].
  The pooled effect is real at n = 1 115 and the breakdown localizes it nowhere,
  which is what a small effect spread evenly across strata looks like — and also
  what an underpowered breakdown of nothing looks like.
- **F3 as pre-registered in §5.3 is a different experiment.** It named
  `fill_gaps`, `max_doc_chunk_tokens` and `doc_semantic_weight`. This is a
  three-arm sweep of `max_chunk_tokens`, which under §5.3's own rule is a search
  and carries no claim. See §11, 2026-08-06.

**The tickets confound was pre-registered against this result and is absent.**
A narrower window puts 12% more chunks in each file, and since gold is at file
level, some gain could be arithmetic — more tickets in the same lottery. That
mechanism helps *more* as k grows. The observed profile is the exact opposite:
the gain is largest at the head (+0.0096 at k=1, +0.0130 at k=5) and is
**+0.0001 at k=20** — twelve percent more chunks bought no additional file that
was not already found. And MRR@10, the headline metric least sensitive to
ticket count, moved as much as nDCG did. This is a change in *ordering*, not in
coverage.

What this does not settle: whether the improvement continues below 364 (the 256
arm is running), and how it decomposes across the retrieval stages — the F2
arms at window 364 are what answer the second, and specifically whether the
rerank does better against narrower chunks, which is the other half of the
hypothesis.

The cost this benchmark cannot see is unchanged and worth restating: a narrower
chunk hands a caller less surrounding code per hit, and file-level nDCG is
blind to that. Nor does it save ColBERT storage — one row per token, and the
token count of a corpus does not depend on how it is cut.

### 12.11 The combination rule itself (2026-08-05)

Raised by a challenge to §12.7's null result: BGE-M3's multi-vector head is the
model's headline capability, so a null is more likely a wrong measurement than
a broken feature. Checking the literature produced two corrections, and the
second is a finding about mindex.

**First: the null result agrees with the published one.** From
[M3-Embedding](https://arxiv.org/html/2402.03216v5), the authors' own ablation —
ColBERT's marginal contribution *over dense+sparse*:

| benchmark | metric | Dense | Sparse | Multi-vec | D+S | All | All − (D+S) |
|---|---|---|---|---|---|---|---|
| MIRACL | nDCG@10 | 69.2 | 53.9 | 70.5 | 70.4 | 71.5 | **+1.1** |
| MKQA | R@100 | 67.8 | 36.3 | 68.4 | 68.1 | 68.8 | **+0.7** |
| NarrativeQA | nDCG@10 | 48.7 | 57.5 | 55.4 | 60.1 | 61.7 | **+1.6** |
| MLDR (long docs) | nDCG@10 | 52.5 | **62.2** | 57.6 | 64.8 | 65.0 | **+0.2** |

+0.2 to +1.6 points, 0.3–2.7% relative. **On MLDR — long documents, the nearest
published analogue to code chunks — it is +0.2**, and the head ordering inverts
there: sparse beats dense by ten points, which is exactly what §12.7 measured
on scikit-learn. The measurement here (+0.0065 at window 512, +0.0084 at 364,
CI [−0.004, +0.021]) **contains the paper's +0.011**. There is no contradiction
with the published result; §12.7's phrasing implied one and is withdrawn on
that point.

**Second: mindex does not implement the paper's combination.** The paper ranks
by a weighted sum of all three heads —

    s_rank = w1*s_dense + w2*s_lex + w3*s_mul     w = [1, 0.3, 1] (MIRACL, MKQA)
                                                  w = [0.15, 0.5, 0.35] (MLDR)

— while `db/qdrant.rs` fuses dense+sparse by RRF into a 200-candidate pool and
then orders that pool by **ColBERT alone**, discarding the other two scores at
the final step. In the paper's table that is the `Multi-vec` row, not `All`.

Measured as a fifth arm (django, short queries, n = 1 115, window 512):

| stratum | n | Δ (weighted sum − mindex) | 95% CI | p | TOST δ=0.01 |
|---|---|---|---|---|---|
| **all** | 1115 | **+0.0080** | [+0.0001, +0.0159] | **0.046** | PASS |
| obvious | 604 | +0.0108 | [−0.000, +0.021] | 0.054 | PASS |
| mixed | 363 | +0.0123 | [−0.002, +0.026] | 0.084 | PASS |
| non-obvious | 148 | −0.0137 | [−0.034, +0.005] | 0.171 | FAIL |

**The combination rule is worth more than the head it combines.** Replacing the
final ordering is a change to one query builder; it needs no reindex, no schema
change and no new storage. The `non-obvious` reversal is unexplained and
underpowered, and is the reason this is reported as a candidate rather than a
recommendation.

One implementation detail is load-bearing: Qdrant's `max_sim` returns the
**sum** over query tokens (a 192-token query scores 191.999 against its own
text), while the paper's weights assume FlagEmbedding's `colbert_score`, which
divides by the query token count. Un-normalised, the ColBERT term outweighs the
other two by two orders of magnitude and the arm is `full` under another name.

**What this does not establish.** None of the published benchmarks are code —
MIRACL is multilingual QA, MLDR long documents, NarrativeQA fiction. The
transfer is assumed. And the weighted-sum arm has been run on one corpus.

### 12.5 Still pending

Two of the four rows below were answered by §12.9 and are struck here rather
than deleted, because this table read as "nothing is known" for a day after they
were. The distinction §12.9 established is the one that decides which:
**`noise_floor.py --index-repeats 0` measures the query path over a fixed set of
vectors, and says nothing about a comparison whose arms were indexed
separately.**

| quantity | value |
|---|---|
| between-run SD, nDCG@10, **same index** | **0.000000** over 7 × 1 296 observations — §12.9 |
| δ for nDCG@10, **same-index comparisons** | **0.01**, the §5.5 floor — §12.9 |
| between-run SD, nDCG@10, **across a reindex** | *pending — §5.1 requires `--index-repeats ≥ 1`, and no such run exists under v3* |
| δ for **cross-index** comparisons | *pending — the only measured value is §12.4's provisional 0.030, from 12 ripgrep queries under BGE-M3* |
| queries needed for 80% power at δ = 0.02 | **~850**; ~3 400 at δ = 0.01 — §12.9 |
| unjudged-but-relevant rate per §9.1 | *pending — needs the hand-judging of 50 sampled queries* |

Which comparisons are which is not a detail. F1, F2 and F7 rank a fixed corpus
of vectors and are same-index. **F3 (§12.10) and the v3-vs-v2 comparison
(§12.15) are cross-index** — a chunk window and an embedder both change what is
stored, so their arms are separate builds and the only δ ever measured for that
case is 0.030. §12.15's interval clears it; §12.10's +0.0108 does not, which is
recorded at that section.

Retention after snapshot verification is no longer pending; it is in §12.1,
and it dropped nothing.

### 12.12 Families F5 and F7 — the leg, and whether it needs a second one (2026-08-05)

Declared in §5.3 before any arm below was scored. Paired randomization,
B = 10 000; BCa bootstrap 95% CI; δ = 0.01.

**F5 — the dense leg.** Every arm ranks the identical chunk set by exact
brute-force cosine, so approximation is credited to nobody. All models fp16 on
one device, dtype and device asserted rather than requested.

| arm | params | django nDCG@10 (n=1115) | scikit-learn (n=360) | chunks/s |
|---|---|---|---|---|
| BM25 / FTS5 | — | 0.2831 | — | — |
| BGE-M3 sparse head | 568M | 0.3183 | 0.5804 | — |
| BGE-M3 dense head | 568M | 0.3332 | 0.5308 | 133 (3 heads) |
| mindex as deployed | — | 0.3549 | 0.5567 | — |
| CodeRankEmbed | 137M | 0.4060 | 0.5918 | 326 |
| **granite-embedding-english-r2** | **149M** | **0.4448** | **0.6241** | **267** |
| Qwen3-Embedding-0.6B | 595M | 0.4540 | 0.6251 | 87 |

| comparison | corpus | Δ | 95% CI | p |
|---|---|---|---|---|
| Qwen3 vs BGE-M3 dense | django | +0.1208 | [+0.1032, +0.1385] | 0.0001 |
| granite vs BGE-M3 dense | django | +0.1116 | [+0.0946, +0.1292] | 0.0001 |
| Qwen3 vs BGE-M3 dense | sklearn | +0.0943 | [+0.0689, +0.1212] | 0.0001 |
| granite vs BGE-M3 dense | sklearn | +0.0933 | [+0.0674, +0.1212] | 0.0001 |
| **Qwen3 vs CodeRankEmbed** | django | **+0.0480** | [+0.0332, +0.0628] | **0.0001** |
| **granite vs CodeRankEmbed** | django | **+0.0389** | [+0.0231, +0.0547] | **0.0001** |
| Qwen3 vs CodeRankEmbed | sklearn | +0.0334 | [+0.0120, +0.0553] | 0.0029 |
| **Qwen3 vs granite** | django | +0.0092 | [−0.0052, +0.0234] | 0.20 |
| **Qwen3 vs granite** | sklearn | +0.0010 | [−0.0180, +0.0200] | 0.91 |

**The code-specialised model is third, on both corpora.** That is the arm
`granite` was added to produce: the question was whether the gain is *a code
model* or merely *a 2026 model*, and a 149M general-purpose encoder beating a
137M code-specialised one by +0.039 at the same size answers it. It is also
what CORE-Bench (arXiv 2606.11864) predicts and what CoIR's ranking does not —
CoIR puts CodeRankEmbed at 60.1 and granite-r2 at 55.3.

**granite and Qwen3 are indistinguishable** (both CIs contain 0, on both
corpora), so the choice falls to cost: granite is 4× smaller and measured 3×
faster on the same device (267 vs 87 chunks/s, both fp16, flash-attn absent for
both). **granite-embedding-english-r2 is the leg.**

**F7 — does a second leg still earn its slot?** The rule and its weights are
chosen on one corpus by the train metric and reported on the other; both
directions were run.

| direction | selected rule | vs granite alone | 95% CI | p | TOST δ=0.01 |
|---|---|---|---|---|---|
| django → sklearn | wsum/sum, w=(0.75, 0.25) | **+0.0048** | [−0.0080, +0.0186] | 0.47 | PASS |
| sklearn → django | wsum/sum, w=(0.80, 0.20) | **+0.0038** | [−0.0023, +0.0099] | 0.22 | PASS |

**The sparse head stops paying once the dense head is good.** Its contribution
is +0.004 in both directions, both CIs contain zero, and the django CI — the
tight one, n = 1 115 — has an upper bound of +0.0099, i.e. **bounded above by
the protocol's own smallest meaningful effect**. Against BGE-M3's dense head
the same leg was worth +0.015; it was compensating for a weak dense vector, not
adding a lexical signal the task needs.

**RRF is worse than the single leg it fuses**, in both directions: 0.4164 vs
0.4448 (django) and 0.6200 vs 0.6241 (sklearn). This is the strength-blindness
of rank fusion, measured directly, and it is why §5.3 named F7 at all.

*Limits.* Both corpora are Python and both query sets are derived from
documentation prose. CoIR measures BM25 varying **56×** across its own datasets,
so a corpus of identifier-heavy queries could restore the lexical leg's value;
the `obvious` stratum here is the nearest thing to that and fusion is *negative*
on it (−0.0072, sklearn). Prose retrieval remains unmeasured (F9), so no routing
claim follows from any of this.

### 12.13 The identifier query set (`build_ident_qrels.py`, 2026-08-06)

A pre-measurement study in the §12.1 sense: it involves no comparison, so
nothing here can have been fitted to a result. **No identifier retrieval has
been scored at the time of writing.**

`scikit-learn`'s issue tier was built today with `build_qrels.py` under the
unchanged §3.2 rules — including §4.2's vector-limit exclusion, which is inert
under v3 but is retained so both corpora are constructed alike (229 · 32 · 19
published → **203 usable**, 45 excluded over limit).

| corpus | sources | no idents | projected | ×4 arms | `ident_in_gold` |
|---|---|---|---|---|---|
| django | 812 | 30 | 782 | 3 128 | 636 (81.3%) |
| scikit-learn | 203 | 3 | 200 | 800 | 190 (95.0%) |
| ripgrep | 12 | 1 | 11 | 44 | 8 (72.7%) |

An instance with no extractable identifier is skipped **whole** rather than
dropped from the identifier arms alone: an empty query scores as an empty
ranking, and keeping its prose arm would leave the arms measuring different
instance sets.

**This table is what freezes the corpus.** `bench/.data/` is gitignored, so
§5.6's "frozen when its output is committed" has no committed artifact to point
at on this tier or any other — the counts published here, before any comparison
exists, are the record. A rebuild that disagrees with them is a different corpus
however recent the file is. The build is deterministic given the source qrels:
instances are read in their stored order and each mangle seed is
`--seed + position`.

**The projection did what it was built to do.** Median query length collapses
from documentation-scale prose to a handful of names — django 864 → 97
characters, scikit-learn 1 124 → 128, ripgrep 1 573 → 35 — and
`lexical_overlap` moves the way §3.4 predicted: django's median rises 0.035 →
0.083 and its `obvious` bucket from 0 to 129 instances, scikit-learn's 0.033 →
0.167 with `obvious` 3 → 54. Arm A2 undoes it, as its premise requires: django's
median overlap returns to **0.000** and its `obvious` bucket falls to 38.

**Route 3 has range.** `ident_df_min` — the document frequency of the rarest
identifier that occurs at all — has median 3 on both large corpora, p90 25
(django) and 15 (scikit-learn), max **1 971** (django). So the corpus does
contain queries whose every term matches most of the repository, which is the
condition under which matching is free and ranking is the whole task. Of the
identifiers probed, **77% (django, 4 186/5 458) and 85% (scikit-learn,
1 466/1 718) occur in the tree at all**; 12 and 2 instances respectively have
none, and carry no `df_min` rather than a zero.

**Route 1 is evaluable on django and NOT on scikit-learn, and this bounds the
decision rule.** The `ident_in_gold = false` stratum holds **146 instances on
django** and **10 on scikit-learn**. Ten is below the n ≥ 20 floor `stats.py`
applies before it will report any stratum at all, so the criterion "the effect must survive where the
lexical leg has no string to match" can be tested on django only. That is
recorded here, before any comparison, rather than discovered afterwards when it
would look like a choice of which corpus to believe. Two consequences, both
pre-registered in §5.7: the held-out direction that *reports* on django is the
one carrying route 1, and a positive result reported only on scikit-learn is
explicitly not a pass.

The predicate is deliberately permissive — a literal case-sensitive substring
of any of up to twelve identifiers in any gold file, which is what a trigram
tokenizer would match — and its lopsidedness is a real property of the task,
not a construction artifact: a bug report about scikit-learn usually does name
something the fixed file contains. It is reported rather than tuned, because a
threshold chosen to balance the strata would be chosen against the very
comparison the strata exist to referee.

**Audit.** The corpora were built, read and rebuilt twice before this section
was written. Both defects were found by reading instances and neither would
have failed a test — the §11 pattern, again. First, fenced blocks were exempt
from the identifier-shape rule on the theory that a code fence marks a name; in
bug reports it marks a dump, and one ripgrep instance contributed its version
banner, a commit SHA, console output and a 45-character DNA sequence to what was
meant to be an identifier query. Second, `ident_df_min` was taken over every
identifier, which made it 0 for nearly every instance — a bug report reliably
names one token the tree has never contained — so it was silently reporting
route 1's statistic under route 3's name. Both rules are now pinned by
`--self-test`.

### 12.14 F10 — the feasibility gate, and the arm-level picture on scikit-learn (2026-08-06)

**This is not F10's test.** F10 asks whether *fusing* a lexical leg into the
dense one adds anything, held out across two corpora. django has not been run,
so no confirmatory comparison exists yet and nothing below carries a claim. What
is recorded here is the §5.7 gate — which is checked *before* the comparison by
construction — and the single-corpus arm scores that came with it, written down
now so they cannot later look chosen.

Run: `v3-ident`, scikit-learn, 800 instances over 184 snapshots, 69.6 min.
**Index-state equivalence passed** on all 20 sampled rebuilds. That is worth its
own line: the descriptive tier is a single snapshot, so this is the first v3 run
that walks history at all, and incremental indexing over 184 commits was never
before compared against cold builds under this pipeline.

**The gate passes, both halves.**

| half | requirement | measured |
|---|---|---|
| (a) the projection reshaped rather than destroyed the query | A1 ≥ 0.5 × A0 | **0.5915 ≥ 0.2611** |
| (b) the arm is off the floor | A1 beats `random`, CI lower bound > 0 | **Δ +0.5803, CI [+0.5268, +0.6303]** |

Note for the record that the withdrawn "10 × random" threshold would also have
passed here — `random` scores 0.0157 on a corpus of ~900 files, not the 0.1475
it scored on ripgrep's 123. The §11 amendment corrected an arithmetic
impossibility on small corpora; it did not change this outcome.

**Arm scores, nDCG@10 (n = 200 per arm).**

| arm | dense | BM25 unicode61 | BM25 trigram | `symbols` | random |
|---|---|---|---|---|---|
| A0 `prose` | 0.5222 | 0.2599 | 0.2548 | 0.2582 | 0.0202 |
| A1 `ident` | **0.5915** | 0.2796 | 0.2481 | 0.2513 | 0.0157 |
| A2 `ident-mangled` | 0.4581 | 0.0651 | 0.0622 | 0.0050 | 0.0073 |
| A3 `ident-intent` | **0.6319** | 0.2904 | 0.2645 | 0.2512 | 0.0164 |

Four observations, none of them a family verdict:

1. **The identifier projection is the dense leg's best arm, not its worst.**
   0.5915 against 0.5222 for the full bug report, and 0.6319 when the title is
   added back. The corpus was built on the premise that identifier queries are
   where a lexical leg would earn its slot; on these arms they are where the
   *dense* leg does best.
2. **A2 behaved exactly as §9.6 route 2 declared.** Perturb the literal string
   and BM25 collapses to 0.0651 — near the random floor — while dense holds
   0.4581. Whatever a lexical leg offers is conditional on the caller spelling
   the name correctly.
3. **`symbols` recovers nearly all of BM25's signal** (0.2513 against 0.2796 on
   A1, and 0.2015 against 0.2294 on the `ident_in_gold = true` stratum). This is
   the §5.7 criterion asking whether a positive result would argue for a leg or
   for routing, and on this corpus the already-shipping tool is within a few
   points of the challenger.
4. **The `ident_in_gold = false` stratum is n = 40 across four arms — ten source
   instances**, as §12.13 recorded in advance. It is reported (dense 0.2405,
   BM25 0.1172, `symbols` 0.0000) and it decides nothing: four arms are not
   independent observations of ten sources. django's 146 is where this stratum
   is testable.

**Still required before any F10 verdict**: the django run, the held-out fusion
in both directions, and `noise_floor.py` against the v3 pipeline — §12.5 still
carries the between-run SD as pending, and the decision rule's "Δ ≥ 2 × SD" term
has no number until it exists.

### 12.15 The shipped v3 system against the shipped v2 system (2026-08-06)

Every comparison above this line is an *arm* — a model scored offline by
brute-force cosine over exported chunks (§12.12), or one stage of a pipeline
switched off (§12.7). None of them is mindex. This section is the one number
that describes the software a reader downloads: **the deployed v3 server against
the deployed v2 server**, both driven through `POST /v0/{guid}/search`, both
including the slicer, the tokenizer, the instruct prefix, Qdrant and HNSW.

It is recorded here because it was missing. The release was written around
§12.12's `0.4540`, which is an offline numpy arm at window 512 — the right
number for choosing a model and the wrong one for describing a system.

**Runs.** `results/v3-qwen06b-torch__django-docs-short.jsonl` (v3: one dense leg,
`qwen3-embedding-0.6b` served by `deploy/embedder/server.py`, window 364,
`Instruct:`/`Query:` prefix, `top_k` 100) against
`results/F2-full__django-docs-short.jsonl` (v2: BGE-M3 dense + sparse fused by
RRF, ColBERT rerank, window 512). Same corpus, same query set, same instance
ids, same cutoff. Paired randomization B = 10 000, BCa bootstrap 95% CI, seed
20260805. Output: `results/v3-vs-v2__django-docs-short.stats.json`.

| stratum | n | v3 | v2 | Δ mean | 95% CI | p |
|---|---|---|---|---|---|---|
| **ALL** | **1115** | **0.4563** | **0.3549** | **+0.1014** | **[+0.0832, +0.1190]** | **0.0001** |
| `obvious` | 604 | 0.5366 | 0.4023 | +0.1343 | [+0.1098, +0.1596] | 0.0001 |
| `mixed` | 363 | 0.4148 | 0.3420 | +0.0728 | [+0.0409, +0.1047] | 0.0001 |
| `non-obvious` | 148 | 0.2308 | 0.1933 | +0.0375 | [−0.0033, +0.0806] | 0.085 |

Three things this does and does not say.

1. **The effect is an order of magnitude above every δ this document has ever
   used.** The interval's *lower* bound (+0.0832) clears the current δ = 0.01 by
   8× and the retired reindex-inclusive δ = 0.030 (§12.4) by nearly 3×. That
   matters because §11 records the δ change as a deviation that flattered
   marginal results: this result is not one of them, and it is the only result
   the release headline rests on.
2. **It is a system comparison, not a model ablation.** Three things move
   together — the embedder, the chunk window (512 → 364) and the tokenizer that
   measured it — and the design cannot separate them. §12.12 is where the model
   is isolated; nothing here attributes the +0.1014 to any one of the three.
3. **The gain is not confined to the stratum the cheap baseline already wins.**
   §3.0.1 exists because a pooled win living entirely in `obvious` is a win at
   string matching. It is largest there (+0.1343), but `mixed` is +0.0728 with a
   CI clear of zero. `non-obvious` is +0.0375 with a CI through zero at n = 148 —
   consistent with the pooled effect, establishing nothing on its own, and it is
   also where both systems are worst in absolute terms (0.23 and 0.19).

**One corpus.** django's documentation prose, Python, single snapshot. There is
no v3 run on scikit-learn: the sklearn v3 arm in §12.12 is the offline embedder,
not the server. So this number is the shipped system measured once, on one
project's docs, and the second corpus is on the open list.

**What the release ships is not what F5 selected, and that is a decision rather
than a result.** §12.12 concluded `granite-embedding-english-r2` is the leg —
indistinguishable from Qwen3 on both corpora (p = 0.20 and 0.91) and cheaper, so
the tie broke on cost. v3 ships Qwen3-Embedding. The reason is recorded in
`docs/claude/retrieval-v3.md` §0 and is not a retrieval measurement: multilingual
queries (the deployment's own are frequently Russian, which nothing in this
harness has ever scored) and a size ladder of three models sharing one tokenizer,
which is what makes `--vectors-only` a re-embed rather than a re-slice. A reader
comparing this document against the code will find that mismatch; it is stated
here so they find the reason with it. The untested half — whether granite's
English-only training would actually have cost anything on a Russian query — is
in §12.5.
