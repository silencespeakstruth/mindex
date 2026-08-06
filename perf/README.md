# perf — indexing benchmark harness

*For contributors and operators tuning a deployment; nothing here is needed to run
mindex. Every number below was measured on one machine and is offered as the shape of
the problem, not as a specification — re-measure on yours.*

> **Status after retrieval v3 (2026-08).** The harness measures what it was built to
> measure — the *mindex* side of indexing throughput — and it now measures **only**
> that. The eight embedder-side columns came from `GET /stats` on the vendored BGE-M3
> server, which is deleted; the OpenAI-compatible contract that replaced it has no
> equivalent, and no general server extends it with one. Those columns
> (`embedder_batch`, `embedder_max_inflight`, `embedder_maxlen`, `fwd_batch_mean`,
> `fwd_batch_max`, `embedder_encode_s`, `queue_highwater`, `embedder_429`) have been
> **removed** rather than left recording `NA`, along with the two plots that charted
> them and the `POST /stats/reset` call that warned on every level of every run. A
> column that is always `NA` reads as a measurement that failed rather than one that
> was never taken.
>
> If you want that signal back, scrape the embedder's own metrics endpoint beside
> mindex — see [`deploy/victoriametrics/`](../deploy/victoriametrics/) — rather than
> sampling it from here. Sampling per run was always the weaker design; it just took
> the removal of one server's private API to make that obvious.
>
> The four findings below are about a pipeline that no longer exists (three heads,
> ColBERT multivectors) and about a signal this harness no longer reads. They are
> retained as the record of *why* the embed path is shaped the way it is, and each
> is marked. The retrieval-quality question they are sometimes mistaken for belongs
> to [`bench/`](../bench/README.md).

A hardware-agnostic load-test harness for tuning **indexing throughput** and GPU
utilization. It simulates real indexing by POSTing source from real GitHub projects
to `/index`, sweeps the tuning knobs, and writes a comparative CSV so the optimum
(max throughput before backpressure / the latency knee) is visible on *your*
hardware. No GPU vendor, device id, or OS specifics are baked in — the only inputs
are endpoint URLs and parameters.

## Why this exists

Indexing was leaving the GPU badly underused. This harness was built to find out why,
and it turned up **four** distinct bottlenecks — none of which was "the GPU is slow."
They are documented in full below; the short version is that the embedder's *response
handling* and *forward orchestration* dominated, not the matmuls.

## What the benchmarks found

The investigation peeled off four layers, in order. Numbers are from one machine and
are **illustrative of the shape**, not a spec — re-measure on yours.

1. **JSON response serialization (≈10× tax).** The embedder returned all three heads
   as JSON. ColBERT is a *multivector* — one 1024-d vector **per token** — so a single
   `/encode` reply ran to hundreds of MB, and `.tolist()` + `orjson.dumps` of it ate
   ~70% of each request, all on the GPU worker thread (so the GPU sat idle during
   serialization). The "GPU encode share" looked high only because that CPU time was
   being timed as encode. **Fix:** a compact length-prefixed **binary `/encode` wire
   format**. *(History: both that server and the wire format are deleted — the
   finding survives as the reason mindex batches at all. Under v3 the response is
   one dense vector per chunk, ~4 KiB, and JSON costs nothing worth measuring.)*
   This alone took the single-stream rate from ~24-25 to the point where the GPU
   was the next limit.

2. **The GPU forward batch is capped by SHARD SIZE, not by `--batch`.** Each `/index`
   request embeds in one shot, so the forward-pass batch equals *chunks-per-request*.
   With the default `corpus/fetch.sh --shard-files 8` that's only ~38 chunks
   (`fwd_batch_mean≈38`), no matter how large `--embed-batch` or the embedder `--batch`
   is. **Fix:** fatten shards (`--shard-files 64`) → `fwd_batch_mean≈252` and a real
   batched forward. Note the bare backbone **saturates around batch ~64-128**, so going
   far past that buys little — pick a shard size that fills the GPU, not the maximum.

3. **FlagEmbedding forwards every batch twice (≈2×).** `BGEM3FlagModel.encode` runs a
   full model forward on the first batch *purely to probe for OOM*, discards it, then
   forwards the same data again in the real loop. Since mindex sends one batch per
   `/encode`, everything was embedded twice. **Fix:** the embedder calls the GPU head
   forward (`EncoderOnlyEmbedderM3ModelForInference.forward`) **directly, once**, and
   reuses FlagEmbedding's exact dense/sparse/colbert post-processing (verified
   byte-identical). Encode rate jumped ~87 → ~133 chunks/s at the same `fwd_batch`.

4. **At low concurrency the pipeline is serial, so a faster GPU just idles more.** Once
   encode is fast, it's only ~35% of wall time at c=1 — the rest is mindex slicing +
   Qdrant upsert + transport. **Concurrency** overlaps those with the GPU (the embedder
   serializes encode on one worker thread, but the *non*-GPU work parallelizes). End to
   end this reached ~117 chunks/s at c=4 on the same box.

**Takeaway / lever order:** the binary protocol and single-forward fixes are baked into
the code. What's left for *you* to tune per machine: **(a)** shard size until the GPU
fills, **(b)** request concurrency until throughput plateaus or the latency knee /
`err_*` appear. `--embed-batch` and the embedder's own batch flag only need to be
≥ chunks-per-request so they don't *re-split* a shard; past that they're not the
lever. This harness lets you find both knees on your hardware.

**How to see (a) now that `fwd_batch_mean` is gone.** The forward batch is still what
matters and it is no longer readable from the client, so read it off the outcome
instead: sweep `--shard-files` and watch `chunks_per_s` at fixed concurrency. It rises
while the GPU is being under-fed and flattens when it is not, which is the same knee
the column used to name directly — one plot instead of one number, and it needs no
cooperation from the embedder.

## Pieces

| File | Role |
| ------ | ------ |
| `corpus/repos.txt` | pinned GitHub repos to benchmark against (edit freely) |
| `corpus/fetch.sh` | clone + pack repos into `/index` payload shards + manifest |
| `index_load.js` | k6 load generator (one shard per iteration, fanned across VUs) |
| `run.sh` | orchestrator: one k6 run per concurrency level → appends a CSV row |
| `plot.sh` | optional gnuplot views of the CSV |
| `analyze.ipynb` | optional Jupyter explorer (set `RESULT_DIRS`, *Run All*) |

The vendored embedder used to expose `GET /stats` + `POST /stats/reset`, which is how
the harness recorded the live embedder config and the **effective forward-pass batch
size** — the direct "is the GPU being fed" signal. That server is gone, and with it any
way for a client to ask an embedder what it is doing; the columns are removed rather
than left `NA`, and the knee is now read off `chunks_per_s` (see above).

Dependencies: `k6`, `jq`, `curl`, `awk`, `git` (and `gnuplot` for plots).

## Operating model

**The harness assumes mindex + embedder + Qdrant are already running** — you launch
them yourself with whatever flags you're testing. The harness starts/stops nothing.
The axes split in two:

- **Within a run (the harness varies this):** request **concurrency** — pass a list
  of VU levels, one CSV row each.
- **Between runs (you change + restart, then rerun):** the embedder's own serving
  knobs (device, dtype, batch and token budget — `MINDEX_EMBED_*` in
  `~/.config/mindex/embedder.env`, see
  [`deploy/embedder/`](../deploy/embedder/README.md)), and mindex's
  `--embed-batch` / `--db-pool-size` / `[model].id`.
  The harness reads the mindex half live from `GET /config`, so every CSV row records
  the mindex config that produced it; the embedder half is no longer readable and is
  what `--label` is for. The CSV is **append-only**, so the matrix accumulates
  across reruns.

Each run uses a **fresh project GUID** and deletes it afterward, so nothing is
hash-skipped (re-indexing identical content does no embedding work) and Qdrant stays
clean between levels.

## Usage

```bash
# 1. Build the corpus once (edit corpus/repos.txt first; pin to SHAs for stability).
perf/corpus/fetch.sh                       # → perf/corpus/data/default/

# 2. Make sure mindex + embedder + Qdrant are up with the flags you want to test.

# 3. Benchmark across concurrency levels. Each run writes ITS OWN file:
#    perf/results/<UTC-stamp>_<model_id>_eb<embed_batch>_pool<db_pool>[_<label>].csv
#    (auto-named from GET /config; runs never overwrite each other).
#    The embedder's own settings are NOT readable over the OpenAI-compatible
#    contract, so put them in --label if you are sweeping them.
perf/run.sh --concurrency "1 2 4 8" --label "embed_batch=256"

# 4. Change something on the embedder (batch, dtype, device), restart it, rerun:
perf/run.sh --concurrency "1 2 4 8" --label "embed_batch=256 dtype=bf16"

# 5. Plot — reads ALL per-run files in perf/results/ and compares them (optional).
perf/plot.sh                               # → perf/plots/
```

k6's native progress bar and end-of-test summary are shown live during each run.
Results files live in `perf/results/` (gitignored — kept for comparison, never
committed). Key `run.sh` flags: `--mindex-url`, `--embedder-url` (pass `""` to skip the
reachability probe), `--corpus`, `--out` (override the auto name), `--concurrency`,
`--label`.
See `--help` on each script.

## Changing mindex config via docker compose (.env profiles)

The mindex container's perf flags are read from environment variables (see
`.env.example`), so you swap a config without editing `docker-compose.yml`:

```bash
docker compose --env-file perf/env/big-batch.env up -d --force-recreate mindex
```

Ready profiles in `perf/env/`: `baseline.env`, `big-batch.env`,
`high-concurrency.env`. Variables: `MINDEX_EMBED_BATCH`, `MINDEX_DB_POOL_SIZE`,
`MINDEX_MAX_BODY_MIB`, `MINDEX_STUCK_GRACE_MINS`, `MINDEX_MODEL_SERVER`,
`MINDEX_RUST_LOG`. Verify what a profile resolves to before starting:

```bash
docker compose --env-file perf/env/big-batch.env config | grep -E 'embed-batch|db-pool-size'
```

**The embedder is not in compose** (it runs on the host for GPU access), so its own
serving knobs are **not** set by these files. Change them in
`~/.config/mindex/embedder.env` (`MINDEX_EMBED_DEVICE`, `_DTYPE`, `_MAX_ROWS`,
`_TOKEN_BUDGET`, `_MAX_SEQ` — see
[`deploy/embedder/embedder.env.example`](../deploy/embedder/embedder.env.example)) and
restart the unit (`systemctl restart mindex-embedder`); `.env` profiles cover only the
mindex side of the matrix.

## Tuning method

The two code-level taxes (JSON serialization, double forward) are already fixed, so
tuning is about **feeding** the GPU and then **keeping it fed**:

1. **Baseline** at current defaults (`perf/env/baseline.env`). Note `chunks_per_s` and
   `req_dur_p95` at each concurrency level.
2. **Grow shard size** to enlarge the GPU forward batch — this is the primary lever.
   Rebuild the corpus with bigger shards and rerun:
   `corpus/fetch.sh --name shard64 --shard-files 64` then `run.sh --corpus
   corpus/data/shard64 …`, and watch `chunks_per_s`. It rises while the forward pass
   is under-filled and flattens once it is not; that flattening is the same knee the
   retired `fwd_batch_mean` column used to name directly. Saturation was measured
   around 64-128 sequences per forward on this hardware — past it, bigger shards add
   latency and not throughput.
3. **Keep `--embed-batch` and the embedder's own batch ≥ chunks-per-request** so a
   shard isn't re-split into several smaller forwards. They are guardrails, not the
   lever.
4. **Push concurrency + `--db-pool-size`** (`perf/env/high-concurrency.env`) to overlap
   slicing / Qdrant upsert with GPU encode. Raise until `chunks_per_s` plateaus, the
   latency knee appears, or `err_500` (pool exhaustion: concurrency > `--db-pool-size`)
   / `err_429` and `err_503` (backpressure — the embedder spells "busy" both ways, and
   mindex retries both) show up.
5. Watch VRAM with *your own* tool (`rocm-smi`/`nvidia-smi`) out of band. Pick the
   config at the **latency knee** with an acceptable error rate — the optimum is
   machine-specific.

## Reading the results (`perf/results/*.csv`)

One file per run, one row per concurrency level. Headline columns:

- `chunks_per_s` — primary throughput metric.
- `model_id` — which embedder produced the row. Two runs at different models are not
  comparable on throughput *or* on anything else; it is in the filename for that reason.
- `min_pool_available` — the low-water mark of the SQLite pool over the run. Approaching
  0 while `err_500` climbs is pool exhaustion, not embedder backpressure, and the fix is
  `--db-pool-size` rather than shard size.
- `req_dur_p95` — per-request latency (= per-shard with the default sharding); the
  knee against `chunks_per_s` marks the optimum. (With a single-shard corpus this is
  just the whole-corpus time, not a meaningful percentile — keep shards small.)
- `err_429` / `err_503` — backpressure. The embedder spells "busy" both ways and
  mindex retries both (`[model].max_429_retries`), so a row carrying either is one
  where indexing was throttled rather than one where it failed. Sustained `err_503`
  with no `err_429` is the embedder being *unreachable* instead of saturated.
  `err_500` — SQLite pool exhaustion (concurrency > `--db-pool-size`).
- `min_pool_available` — lowest SQLite pool headroom seen during the run (0 ⇒
  saturated; sampled by `run.sh` polling `GET /status`).

`plot.sh` renders four scatter views: throughput vs embedder batch, latency vs
throughput, throughput vs concurrency, and forward-pass batch vs throughput. For
per-series lines (e.g. one curve per `embed_batch`), filter the CSV by that column
or load it into a spreadsheet — the CSV is the source of truth.

`analyze.ipynb` is a richer interactive alternative: set `RESULT_DIRS` (one or more
folders of `*.csv`) in the first cell and *Run All*. It concatenates every CSV under
those folders, groups rows into a **config signature** (model_id / embed_batch /
db_pool_size + `label`) with concurrency as the x-axis, and renders a summary table
plus throughput, latency-knee, latency-percentile and error/pool-headroom views —
closing with a few auto-generated takeaways. Deps: `pandas`, `matplotlib` (+ a Jupyter
kernel).

**It has not been re-run since the column removal**, and its config signature still
names `embedder_batch`/`max_inflight`; the GPU-encode-share and forward-pass-batch
cells will KeyError on a v3 CSV. Fix the signature and drop those two cells before
trusting it, or use `plot.sh`, which was updated.

## Notes

- Run against a **non-production** mindex/Qdrant: each run indexes the full corpus
  and deletes its project, but it shares the Qdrant/SQLite instance you point it at.
- Keep the corpus large enough to keep the GPU busy for the whole run (tens of
  thousands of chunks); a too-small corpus finishes before steady state.
- **Nothing here is GPU telemetry.** Every column is measured from the client or read
  from mindex; the embedder is a black box behind `/v1/embeddings`. Correlate with your
  own VRAM/utilization tool (`rocm-smi`, `nvidia-smi`, `intel_gpu_top`) for the hardware
  view, or scrape the embedder's own metrics if it has them.
