# bench — retrieval and research quality

*`perf/` measures how fast mindex indexes. `bench/` measures whether what it
indexes can be found. The two are kept apart on purpose: throughput and
quality are different questions with different failure modes, and merging the
harnesses would make neither legible.*

**Read [`PROTOCOL.md`](PROTOCOL.md) first.** It is the pre-registration —
metrics, statistical tests, the non-inferiority margin, the stopping rules and
the threats to validity — committed before any number was produced. This file
is only the operating manual.

## Why this exists

mindex's retrieval was never measured. Its documentation carried numbers
(`MRR@10 0.3931`, `recall@10 20/23`, `512 answers 15/23 documentation questions
vs 18/23`) from a one-off evaluation that no longer exists, over a 23-question
set that exists nowhere — and the integration suite runs against a mock embedder
whose vectors are seeded by text hash, so it can assert plumbing and ranking
stability but can never see a semantic regression.

The cost was not rhetorical. Four pipeline decisions were being deferred to a
measurement nobody had: whether the sparse leg earned a slot, whether the
late-interaction rerank earned **99.6% of stored bytes**, what the chunk window
should be, and whether the embedder was the right one. This harness answered
them, and the answers replaced the whole retrieval pipeline in v2.0.0 — see
[`FINDINGS.md`](FINDINGS.md) §11 and `PROTOCOL.md` §12.15.

## What is measured

**Descriptive retrieval from a project's own documentation** (the primary tier).
Given a paragraph of prose a project's maintainers wrote about their own code,
does mindex's search rank the file that defines what the prose is describing?
Gold comes from Sphinx `.. class::` directives and `:class:`/`:func:` roles,
resolved **by AST against the source tree** — a file is gold only if it actually
defines the symbol — and the query is the section's prose with every explicit
code reference and code block stripped out.

This tier replaced **issue localization** as the primary task on 2026-08-05, and
the reason is in `PROTOCOL.md` §11: localizing a bug from its symptoms requires
*inference*, mindex performs *matching*, and the inference belongs to
`/research`. Answering "does ColBERT earn its storage" on a task the component
does not perform can select the wrong configuration with a confidence interval
attached. Issue localization is retained as a secondary tier.

Neither tier is an exam this project wrote. The descriptive gold is derived from
each project's own published documentation at a pinned commit; the issue tier's
is third-party:

| dataset | scope | granularity |
|---|---|---|
| [`czlll/Loc-Bench_V1`](https://huggingface.co/datasets/czlll/Loc-Bench_V1) | 560 instances, Python | file + function; bug / feature / performance / security |
| [`ByteDance-Seed/Multi-SWE-bench`](https://huggingface.co/datasets/ByteDance-Seed/Multi-SWE-bench) | 48 repos, 8 languages, CC0 | file |
| [`SWE-bench`](https://huggingface.co/datasets/SWE-bench/SWE-bench) + Verified | Python, 12 repos | file |

A third tier projects the issue tier's queries into **identifier shape**, keeping
its gold byte-identically, to ask whether a lexical leg earns a slot when the
query is a name rather than prose (`PROTOCOL.md` §3.4, family F10 — declared,
partially run, no verdict).

## What has actually been run, and what has not

The corpora table in `corpora.toml` is a plan. Only **django** and
**scikit-learn** have ever produced a published number, both Python, both from
documentation prose. Every other corpus — `cli`, `clap`, `vue-core`, the whole
tier-2 set — is declared and unrun, as is family F9 (prose retrieval) and the
research-quality tier (`PROTOCOL.md` §7). `FINDINGS.md` opens with the full list
of what this harness does not establish; read it before quoting anything here.

## Setup

```bash
python3 -m venv bench/.venv
bench/.venv/bin/pip install -r bench/requirements.txt
```

Python **3.13+** is required: gold-set globbing uses
`PurePosixPath.full_match`, and the reason is in `build_qrels.py`.

### The benchmark server

A run needs its own mindex, not the one serving your editor:

Three paths are yours and are **not** in the committed config — a benchmark
whose config names one author's home directory is one that runs on one machine:

```bash
export MINDEX_BENCH_DB="$HOME/.local/share/mindex-bench/mindex-bench.db"
export MINDEX_BENCH_CERT="$HOME/.config/mindex/bench-cert.pem"
export MINDEX_BENCH_KEY="$HOME/.config/mindex/bench-key.pem"
mkdir -p "$(dirname "$MINDEX_BENCH_DB")" "$(dirname "$MINDEX_BENCH_CERT")"

# mindex is TLS-only and has no plaintext mode. Any self-signed leaf will do:
# the harness connects with no_verify over loopback (corpora.toml says why).
mkcert -cert-file "$MINDEX_BENCH_CERT" -key-file "$MINDEX_BENCH_KEY" \
       localhost 127.0.0.1                       # or openssl req -x509 ...

cargo build --release --bin mindex
./target/release/mindex --config bench/bench-config.toml --bind 127.0.0.1:11121 \
    --cert-path "$MINDEX_BENCH_CERT" --key-path "$MINDEX_BENCH_KEY" \
    --db-path "$MINDEX_BENCH_DB"
```

It has its own database, its own project GUIDs — which is what keeps its
collections apart from the live index in a shared Qdrant — and
`[auth].enabled = false`, the one setting CLAUDE.md sanctions for a server on
a trusted loopback that authorizes nothing. It shares the host's embedder and
Qdrant, because measuring a different embedder than the one people run would
defeat the purpose.

`run.py` needs `$MINDEX_BENCH_DB` too (or `--db-path`): it reads index state out
of the SQLite file directly, and pointing it at the wrong one reports every
instance as freshly built rather than failing.

Build the binary from the tree you intend to measure. `run.py` records the git
SHA and warns when `src/` is dirty, but it cannot tell that a stale binary on
`$PATH` is serving.

## Running

```bash
# Check the gold-set filters. Fast, no network, no data.
bench/.venv/bin/python bench/build_qrels.py --self-test

# Fetch the smoke corpus (ripgrep, 117 files) and its ground truth.
bench/.venv/bin/python bench/fetch.py --tier 0

# Build the frozen query set.
bench/.venv/bin/python bench/build_qrels.py --tier 0

# Retrieve. Walks the corpus in commit order, one snapshot per instance.
bench/.venv/bin/python bench/run.py --corpus ripgrep --fresh

# Score. Opens no server; re-runnable over an archive forever.
bench/.venv/bin/python bench/score.py bench/results/baseline__ripgrep.jsonl \
    --per-query bench/results/baseline__ripgrep.perquery.jsonl

# The noise floor: five identical runs, then the SD and δ they imply.
bench/.venv/bin/python bench/noise_floor.py --corpus ripgrep
```

`--tier 0` is the smoke corpus, `--tier 1` the five primary corpora, `--tier 2`
adds the optional extensions. `--corpus <name>` selects by name. Both scripts
are idempotent.

Snapshot verification needs the clone. To build a draft against datasets alone,
pass `--no-verify-snapshots` — never for a real run.

## Checking a published number without running anything

Every figure quoted in `PROTOCOL.md` §12 or `FINDINGS.md` has a committed
artefact behind it in [`published/`](published/README.md) — the aggregate
`score.py` produced, or the interval and p-value `stats.py` produced, both a few
kilobytes. The runs themselves are not committed (654 MB for a tier-1 pass), so
this is the difference between a table you can check and a table you must trust.

```bash
# the release headline: v3 vs v2, same corpus, same queries
python -c 'import json;d=json.load(open("bench/published/v3-vs-v2__django-docs-short.stats.json"));print(d["rows"][0])'

# is your rebuilt corpus the one these numbers came from?
sha256sum bench/.data/qrels/django-docs-short.jsonl
python -c 'import json;print(json.load(open("bench/published/qrels.manifest.json"))["django-docs-short.jsonl"]["sha256"])'

# do the committed artefacts still follow from the runs on this machine?
bench/.venv/bin/python bench/publish.py --check
```

## Cost, before you start a tier-1 fetch

Measured on the live index: **~764 KiB per chunk** and **~22 chunks per file**.
django has ~2 905 indexable files, so one configuration of it is roughly
**47 GiB** of Qdrant storage; the five primary corpora are ~113 GiB.

Two consequences that are easy to learn the expensive way:

- **Configurations are run and torn down in sequence, never held at once.**
  Peak storage is one configuration, not the product of the ablation matrix.
- **GC is mandatory between instances, not optional.** mindex's indexing hot
  path is append-only: a reindex marks old chunks deleted and orphans their
  vectors until GC. A django pass is 850 checkouts, so without
  `gc_every_instances` the orphans dwarf the index itself.

## Layout

| file | role |
|---|---|
| `PROTOCOL.md` | the pre-registration; everything else implements it. §11 amendments, §12 results |
| `FINDINGS.md` | the working narrative: what was found, what was wrong, what is still open |
| `CODE_EMBEDDER_SURVEY.md` | the literature read before the model comparison |
| `corpora.toml` | repos, pinned dataset revisions, gold-set filters, run policy |
| `bench-config.toml` | the mindex config the benchmark server runs with (no machine paths — see its header) |
| `fetch.py` | clone repositories, download ground truth, record a sha256 manifest |
| `build_qrels.py` | issue datasets → the frozen query set, plus the drop report |
| `sphinx_docs.py` | Sphinx docs → queries + AST-verified gold. `--self-test` covers parser and resolver |
| `build_docs_qrels.py` | driver for the **primary** descriptive corpus; `--short` emits the short-query variant |
| `build_ident_qrels.py` | the identifier projection (§3.4): four query arms, gold inherited byte-identically |
| `run.py` | checkout → reindex → drift-prune → query → JSONL |
| `score.py` | JSONL → nDCG/Recall/MRR/MAP/Acc@k, per corpus and macro. `--json` writes a summary |
| `stats.py` | paired randomization, BCa interval, non-inferiority against a margin |
| `ranx_bridge.py` | result JSONL → `ranx` Qrels/Run, keeping the chunk→file dedup |
| `noise_floor.py` | §5.1: N identical runs → between-run SD → δ → power |
| `slicer_sweep.py` | rewrite config, restart server, reindex, verify the window by re-tokenizing |
| `publish.py` | regenerate `published/`; `--check` fails when a number and its artefact disagree |
| `baselines/bm25_fts5.py` | the lexical floor, plus `--system random` for calibration |
| `baselines/fusion.py` | chunk-level fusion over `ranx`; `--train`/`--test` refuse the same corpus |
| `baselines/external_embedder.py` | an embedding model scored by exact brute-force cosine over exported chunks |
| `baselines/cross_encoder.py` | a reranker over a first stage, reported as a delta over it |
| `baselines/symbol_lookup.py` | mindex's own `/symbols` as a retrieval arm |
| `published/` | **committed** — the summary and paired-comparison JSON behind every quoted number, plus a qrels hash manifest |
| `tests/` | 18 tests cross-checking `score.py` against `ranx` over archived runs |

Not built, and named in `PROTOCOL.md` §8: the tier-0 replay fixture and
`ci.yml`. Nothing in `.github/workflows/` runs this harness.

Two things `run.py` does that are easy to leave out and impossible to notice
afterwards. A checkout **deletes** files, and mindex is only ever told what a
client finds — so without the per-step `/drift` prune, chunks from commits
already passed keep answering queries for the rest of the corpus. And indexing
is append-only, so `POST /gc` runs on an interval rather than at the end.

`--equivalence-sample` is the one that can fail the run outright: it rebuilds
sampled snapshots from scratch into a scratch project and compares chunk
boundaries against the incrementally-reached ones. If mindex's skip logic is
wrong, that is a mindex bug, and a harness that assumed it away would publish
the bug as a quality score.

## What the first audit found

The query-set audit is a step in the protocol rather than a formality, and it
changed three things before any retrieval was measured. All three are recorded
in `PROTOCOL.md` §11 with their measured effect:

- **`CHANGELOG.md` was in the gold set of 3 of 14 ripgrep instances.** A
  system ranking a changelog first would have scored a hit for "localizing"
  the bug. Records of change are now excluded; documentation is not.
- **The gold-set glob matcher was wrong.** `fnmatch` is not path-aware, so
  `**/tests/**` did not match a root-level `tests/` — which is where django
  keeps its suite. Measured impact on these corpora: **zero**, because both
  dataset families already separate `test_patch` from the fix patch. The
  matcher was wrong; the consequence was not realized. It is fixed and pinned
  by `--self-test`, and it still matters for the in-house commit-derived tier,
  where a raw diff contains everything.
- **Deduplicating across datasets silently emptied SWE-bench Verified.** It is
  a subset of SWE-bench full, so all 231 django instances collapsed and the
  number most comparable to published work became unreportable. The work is
  now deduplicated; the labels are not.

And one the first run found, in mindex rather than in the harness:

- **`POST /search` answered 503 for any query above 1023 BGE-M3 tokens.**
  ColBERT emitted one 1024-wide row per query token and a Qdrant multivector
  holds at most 1 048 576 elements. It failed however far above the limit the
  query sat — the embedder's own `--maxlen` truncation still left more rows
  than the store accepted — and it surfaced as `qdrant.unavailable`, i.e. an
  input-size problem diagnosed as an infrastructure outage. 8.1% of django's
  queries and 14.3% of ripgrep's were over it. Those instances are excluded
  from the corpus (§4.2), because the lexical baseline they are compared
  against accepts a query of any length.

  **The constraint is gone under v2.0.0** — there is no multivector — but the
  exclusion stays, and `corpora.toml` still counts it in BGE-M3 tokens. It is
  part of the *frozen corpus*: it decided which instances every published number
  was computed over, and re-tokenizing it would silently change the query set
  and make old and new results incomparable.

None of these were caught by a test. They were caught by reading forty
instances, which is why the protocol requires it.
