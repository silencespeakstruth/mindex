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

mindex's documentation carries retrieval-quality numbers (`MRR@10 0.3931`,
`recall@10 20/23`, `512 answers 15/23 documentation questions vs 18/23`) that
came from a one-off evaluation which no longer exists, over a 23-question set
that exists nowhere. `docs/claude/qdrant.md` states it plainly: *"there is no
retrieval-quality harness in this repo."*

The cost is not only rhetorical. Three roadmap items — ColBERT binary
quantization, token pooling, and whether ColBERT earns its place at all —
are explicitly gated on a measurement that does not exist, and ColBERT is
**99.6% of stored bytes**. Meanwhile the integration suite runs against a mock
embedder whose vectors are seeded by text hash, so it can assert ranking
stability and plumbing but can never see a semantic regression.

## What is measured

Given a natural-language description of a problem in a repository, does
mindex's search rank the code that must change near the top? That is issue
localization framed as retrieval — the task from LocAgent (ACL 2025) and
SweRank, chosen because it is what mindex is actually used for and because
third-party ground truth exists, so we do not write our own exam.

Ground truth comes from three published datasets, pinned by revision:

| dataset | scope | granularity |
|---|---|---|
| [`czlll/Loc-Bench_V1`](https://huggingface.co/datasets/czlll/Loc-Bench_V1) | 560 instances, Python | file + function; bug / feature / performance / security |
| [`ByteDance-Seed/Multi-SWE-bench`](https://huggingface.co/datasets/ByteDance-Seed/Multi-SWE-bench) | 48 repos, 8 languages, CC0 | file |
| [`SWE-bench`](https://huggingface.co/datasets/SWE-bench/SWE-bench) + Verified | Python, 12 repos | file |

## Setup

```bash
python3 -m venv bench/.venv
bench/.venv/bin/pip install -r bench/requirements.txt
```

Python **3.13+** is required: gold-set globbing uses
`PurePosixPath.full_match`, and the reason is in `build_qrels.py`.

### The benchmark server

A run needs its own mindex, not the one serving your editor:

```bash
cargo build --release --bin mindex
mkdir -p ~/.local/share/mindex-bench
./target/release/mindex --config bench/bench-config.toml --bind 127.0.0.1:11121
```

It has its own database, its own project GUIDs — which is what keeps its
collections apart from the live index in a shared Qdrant — and
`[auth].enabled = false`, the one setting CLAUDE.md sanctions for a server on
a trusted loopback that authorizes nothing. It shares the host's embedder and
Qdrant, because measuring a different embedder than the one people run would
defeat the purpose.

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
| `PROTOCOL.md` | the pre-registration; everything else implements it |
| `corpora.toml` | repos, pinned dataset revisions, gold-set filters, run policy |
| `bench-config.toml` | the mindex config the benchmark server runs with |
| `fetch.py` | clone repositories, download ground truth, record a sha256 manifest |
| `build_qrels.py` | datasets → the frozen query set, plus the drop report |
| `run.py` | checkout → reindex → drift-prune → query → JSONL |
| `score.py` | JSONL → nDCG/Recall/MRR/MAP/Acc@k, per corpus and macro |
| `noise_floor.py` | §5.1: N identical runs → between-run SD → δ → power |

Still to come, in the order `PROTOCOL.md` §8 stages them: `stats.py`,
`baselines/`, the tier-0 fixture and `ci.yml`.

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

- **`POST /search` answers 503 for any query above 1023 BGE-M3 tokens.**
  ColBERT emits one 1024-wide row per query token and a Qdrant multivector
  holds at most 1 048 576 elements. It fails however far above the limit the
  query sits — the embedder's own `--maxlen` truncation still leaves more rows
  than the store accepts — and it surfaces as `qdrant.unavailable`, i.e. an
  input-size problem diagnosed as an infrastructure outage. 8.1% of django's
  queries and 14.3% of ripgrep's are over it. Those instances are excluded from
  the corpus (§4.2), because the lexical baseline they will be compared against
  accepts a query of any length.

None of these were caught by a test. They were caught by reading forty
instances, which is why the protocol requires it.
