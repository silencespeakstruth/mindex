# perf — indexing benchmark harness

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
   format** (see `embedder/README.md` and `src/models/bge_m3.rs`). This alone took the
   single-stream rate from ~24-25 to the point where the GPU was the next limit.

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
fills (`fwd_batch_mean`), **(b)** request concurrency until throughput plateaus or the
latency knee / `err_*` appear. `--embed-batch` and the embedder `--batch` only need to
be ≥ chunks-per-request so they don't *re-split* a shard; past that they're not the
lever. This harness lets you find both knees on your hardware.

## Pieces

| File | Role |
|------|------|
| `corpus/repos.txt`   | pinned GitHub repos to benchmark against (edit freely) |
| `corpus/fetch.sh`    | clone + pack repos into `/index` payload shards + manifest |
| `index_load.js`      | k6 load generator (one shard per iteration, fanned across VUs) |
| `run.sh`             | orchestrator: one k6 run per concurrency level → appends a CSV row |
| `plot.sh`            | optional gnuplot views of the CSV |
| `analyze.ipynb`      | optional Jupyter explorer (set `RESULT_DIRS`, *Run All*) |

The embedder exposes `GET /stats` + `POST /stats/reset` (added in `embedder/`) so the
harness records the live embedder config and the **effective forward-pass batch
size** — the direct "is the GPU being fed" signal — without touching hardware.

Dependencies: `k6`, `jq`, `curl`, `awk`, `git` (and `gnuplot` for plots).

## Operating model

**The harness assumes mindex + embedder + Qdrant are already running** — you launch
them yourself with whatever flags you're testing. The harness starts/stops nothing.
The axes split in two:

- **Within a run (the harness varies this):** request **concurrency** — pass a list
  of VU levels, one CSV row each.
- **Between runs (you change + restart, then rerun):** embedder `--batch` /
  `--max-inflight`, mindex `--embed-batch` / `--db-pool-size`. The harness reads
  these live from `GET /config` and `GET /stats`, so every CSV row records exactly
  the config that produced it. The CSV is **append-only**, so the matrix accumulates
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
#    perf/results/<UTC-stamp>_eb<embed_batch>_pool<db_pool>_xb<embedder_batch>[_<label>].csv
#    (auto-named from the live config; runs never overwrite each other).
perf/run.sh --concurrency "1 2 4 8" --label "embedder_batch=16"

# 4. Change embedder --batch (e.g. 64), restart the embedder, rerun — new file:
perf/run.sh --concurrency "1 2 4 8" --label "embedder_batch=64"

# 5. Plot — reads ALL per-run files in perf/results/ and compares them (optional).
perf/plot.sh                               # → perf/plots/
```

k6's native progress bar and end-of-test summary are shown live during each run.
Results files live in `perf/results/` (gitignored — kept for comparison, never
committed). Key `run.sh` flags: `--mindex-url`, `--embedder-url` (pass `""` to skip
`/stats`), `--corpus`, `--out` (override the auto name), `--concurrency`, `--label`.
See `--help` on each script.

## Changing mindex config via docker compose (.env profiles)

The mindex container's perf flags are read from environment variables (see
`.env.example`), so you swap a config without editing `docker-compose.yml`:

```bash
docker compose --env-file perf/env/big-batch.env up -d --force-recreate mindex
```

Ready profiles in `perf/env/`: `baseline.env`, `big-batch.env`,
`high-concurrency.env`. Variables: `MINDEX_EMBED_BATCH`, `MINDEX_DB_POOL_SIZE`,
`MINDEX_MAX_BODY_MB`, `MINDEX_STUCK_GRACE_MINS`, `MINDEX_MODEL_SERVER`,
`MINDEX_RUST_LOG`. Verify what a profile resolves to before starting:

```bash
docker compose --env-file perf/env/big-batch.env config | grep -E 'embed-batch|db-pool-size'
```

**The embedder is not in compose** (it runs on the host for GPU access), so its
`--batch` / `--max-inflight` / `--maxlen` — the main GPU lever — are **not** set by
these files. Change them on the embedder's launch command and restart it by hand;
`.env` profiles cover only the mindex side of the matrix.

## Tuning method

The two code-level taxes (JSON serialization, double forward) are already fixed, so
tuning is about **feeding** the GPU and then **keeping it fed**:

1. **Baseline** at current defaults (`perf/env/baseline.env`). Note `fwd_batch_mean`,
   `chunks_per_s`, and the `embedder_encode_s / wall_clock_s` ratio.
2. **Grow shard size** to enlarge the GPU forward batch — this is the primary lever.
   Rebuild the corpus with bigger shards and rerun:
   `corpus/fetch.sh --name shard64 --shard-files 64` then `run.sh --corpus
   corpus/data/shard64 …`. Watch `fwd_batch_mean` climb and `chunks_per_s` with it.
   The bare backbone **saturates around batch ~64-128**, so stop once `fwd_batch_mean`
   is in that range — bigger shards past that add latency, not throughput.
3. **Keep `--embed-batch` and the embedder `--batch` ≥ chunks-per-request** so a shard
   isn't re-split into several smaller forwards. They are guardrails, not the lever.
4. **Push concurrency + `--db-pool-size`** (`perf/env/high-concurrency.env`) to overlap
   slicing / Qdrant upsert with GPU encode. Raise until `chunks_per_s` plateaus, the
   latency knee appears, or `err_500` (pool exhaustion: concurrency > `--db-pool-size`)
   / `err_429`/`embedder_429` (backpressure) show up.
5. Watch VRAM with *your own* tool (`rocm-smi`/`nvidia-smi`) out of band. Pick the
   config at the **latency knee** with an acceptable error rate — the optimum is
   machine-specific.

## Reading the results (`perf/results/*.csv`)

One file per run, one row per concurrency level. Headline columns:

- `chunks_per_s` — primary throughput metric.
- `fwd_batch_mean` / `fwd_batch_max` — sequences per GPU forward pass. If this stays
  small while you raise `--embed-batch`/`--batch`, the cap is **shard size**, not the
  batch flags — fatten shards (`--shard-files`). Aim to fill the GPU (~64-128), no more.
- `embedder_encode_s` vs `wall_clock_s` — how much of the wall time is actual GPU
  encoding vs overhead (slicing / Qdrant upsert / transport). `encode_s ≈ wall_s` ⇒
  GPU-bound; much lower ⇒ the GPU is idle waiting on the rest of the pipeline.
- `req_dur_p95` — per-request latency (= per-shard with the default sharding); the
  knee against `chunks_per_s` marks the optimum. (With a single-shard corpus this is
  just the whole-corpus time, not a meaningful percentile — keep shards small.)
- `err_429`/`embedder_429` — backpressure (embedder saturated). `err_500` — SQLite
  pool exhaustion (concurrency > `--db-pool-size`). `err_503` — embedder unreachable.
- `min_pool_available` — lowest SQLite pool headroom seen during the run (0 ⇒
  saturated; sampled by `run.sh` polling `GET /status`).

`plot.sh` renders four scatter views: throughput vs embedder batch, latency vs
throughput, throughput vs concurrency, and forward-pass batch vs throughput. For
per-series lines (e.g. one curve per `embed_batch`), filter the CSV by that column
or load it into a spreadsheet — the CSV is the source of truth.

`analyze.ipynb` is a richer interactive alternative: set `RESULT_DIRS` (one or more
folders of `*.csv`) in the first cell and *Run All*. It concatenates every CSV under
those folders, groups rows into a **config signature** (embed_batch / db_pool_size /
embedder_batch / max_inflight + `label`) with concurrency as the x-axis, and renders a
summary table plus throughput, latency-knee, latency-percentile, GPU-encode-share
(`embedder_encode_s / wall_clock_s`), forward-pass-batch (vs `chunks_per_req` and the
configured `--batch`), and error/pool-headroom views — closing with a few
auto-generated takeaways. Deps: `pandas`, `matplotlib` (+ a Jupyter kernel).

## Notes

- Run against a **non-production** mindex/Qdrant: each run indexes the full corpus
  and deletes its project, but it shares the Qdrant/SQLite instance you point it at.
- Keep the corpus large enough to keep the GPU busy for the whole run (tens of
  thousands of chunks); a too-small corpus finishes before steady state.
- The embedder `/stats` numbers are **logical** (derived from request shape and
  `--batch`, matching how the embedder sub-batches a call) — not GPU telemetry. Correlate
  them with your own VRAM/utilization tool for the hardware view.
