# embedder — BGE-M3 three-head embedding server

Vendored wrapper exposing BGE-M3's **three** outputs — dense, sparse (SPLADE-style
lexical weights), and ColBERT multi-vectors — over a tiny HTTP contract
(`POST /encode`, `GET /health`) that mindex consumes via `--model-server`.

## Why this exists (and why it's temporary)

mindex's hybrid retrieval needs all three BGE-M3 heads at once. Current
general-purpose model servers (vLLM, Ollama, …) return **only dense** embeddings —
none expose the sparse lexical weights and ColBERT token vectors together. This
wrapper exists **solely** to bridge that gap. The plan is to **remove it** once an
off-the-shelf server emits all three heads and point mindex straight at that; the
`/encode` contract is kept intentionally minimal to make that swap cheap.

## Not part of the mindex image

This is **not** built into the mindex Docker image and is **not** in
`docker-compose.yml`. It pulls heavy GPU dependencies (torch alone is ~8 GB) and
needs direct GPU access (ROCm/CUDA), so it runs on the **host** alongside — not
inside — the container; mindex reaches it over `host.docker.internal`. The in-repo
`tests/mock_embedder/` is the lightweight CI stand-in: same contract, no torch.

## Run

```
cd embedder
uv sync
uv run python -m bge_m3_api --port 11211
```

### GPU / torch build (supply torch out-of-band)

`pyproject.toml` does **not** let uv manage torch at all: a never-true override
marker (`[tool.uv] override-dependencies`) drops it from the resolution, so `uv
sync` installs everything **except** torch (and never pulls the default CUDA wheel
with its multi-GB `nvidia-*` libs). The right accelerator build is per-machine (AMD
ROCm vs NVIDIA CUDA vs CPU), so you supply torch into `.venv` yourself — your choice
stays uncommitted, and because uv doesn't track torch, **`uv sync` never reverts
it** (no re-run gotcha). torch's pure-python runtime deps (`sympy`, `networkx`,
`jinja2`, `filelock`, `fsspec`) *are* declared in `pyproject.toml` so the external
torch can import (`torch._dynamo`, which `transformers` triggers, needs `sympy`).

This machine (AMD Radeon AI PRO R9700, `gfx1201`) keeps **one** ROCm 7.2 torch in a
neutral, project-independent home and references it from every venv — so the ~14 GB
is on disk once and no single project "owns" it:

```
CANON=/data/silencespeakstruth/Shared/rocm-torch-py313   # the single source of truth
DST=.venv/lib/python3.13/site-packages
for d in torch functorch torchgen torch-*.dist-info triton triton_rocm-*.dist-info; do
    ln -sfn "$CANON/$(basename $CANON/$d)" "$DST/$(basename $d)"
done
```

(`$CANON` must be the **same Python minor** — 3.13 — as this venv for the compiled
extensions to load; `/opt/rocm` is system-wide, so torch finds its ROCm libs
regardless of which venv imports it.) Every other project — ComfyUI included —
symlinks the same `$CANON`; to seed the home the first time, `mv` an existing ROCm
torch install there and symlink the origin back. If you'd rather install a fresh
ROCm torch into `.venv` instead of symlinking (nightly — note `--pre`):

```
uv pip install --pre torch --index-url https://download.pytorch.org/whl/nightly/rocm7.2
```

Verify:

```
uv run python -c "import torch; print(torch.__version__, torch.cuda.is_available())"
```

ROCm exposes the GPU through the `cuda` device API, so `torch.cuda.is_available()`
returning `True` (and `--device cuda`) is correct on AMD too.

Useful flags (`python -m bge_m3_api --help`): `--device` (`cuda`, `cuda:0`, `cpu`),
`--batch`, `--max-inflight` (429 beyond this), `--idle-timeout` (unload the model
after N idle seconds).

> The Python range in `pyproject.toml` is pinned narrowly (3.13 only) **on purpose**:
> the ROCm torch builds this targets are finicky on flagship/Pro-class AMD cards, and
> a wider range pulls wheels that don't work there. Don't widen it without testing on
> your GPU.

> Bind `0.0.0.0` (the default host), **not** `127.0.0.1`: a Dockerised mindex
> reaches a host-run server through the bridge gateway, which `127.0.0.1` excludes.

## Contract (frozen)

`POST /encode` `{ "texts": [...] }` (JSON request) → a **little-endian binary body**
(`application/octet-stream`) carrying dense / sparse / ColBERT for each input,
positionally aligned. The response is binary, not JSON: ColBERT is a multivector
(one 1024-d vector per token), so a JSON body ran to hundreds of MB per call and
`.tolist()` + `orjson.dumps` of it dominated request time (~70%, all on the GPU
worker thread). The length-prefixed layout is documented on `pack_encode` in
`src/bge_m3_api/__main__.py`; the consumer (`src/models/bge_m3.rs::parse_encode_response`)
and the test mock (`tests/mock_embedder/main.py`) mirror it **byte-for-byte** — change
one and you change all three in the same commit. `GET /health` reports liveness
without loading the model.

## Stats (perf harness only, not part of the frozen contract)

`GET /stats` → `{ "config": { batch, max_inflight, maxlen }, "runtime": { … },
"inflight": N }`. `config` echoes the launch flags; `runtime` is a rolling tally
since the last reset: `forward_passes`, `forward_batch_mean`/`forward_batch_max`
(sequences per GPU forward pass — derived from `len(texts)` and `--batch`, exactly
how FlagEmbedding sub-batches one `encode()`, so it shows whether each call fills the
batch — the GPU-feed signal), `encode_calls`, `encode_seconds_total`,
`chunks_encoded_total`, `requests_429`, `queue_depth_highwater`. `POST /stats/reset`
zeroes the runtime tally (config untouched). Purely logical counters — no GPU/VRAM
probing. Consumed by `perf/run.sh`; mindex itself never calls these.
