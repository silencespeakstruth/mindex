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
poetry install
poetry run python -m bge_m3_api --port 11211
```

### GPU / torch build (one extra step)

`pyproject.toml` does **not** pin a torch build, so `poetry install` pulls
whatever PyPI serves by default (a CUDA wheel on Linux). That's deliberate: the
right accelerator build is per-machine (AMD ROCm vs NVIDIA CUDA vs CPU), so each
user installs it **into the venv** after `poetry install` — this never touches the
tracked `pyproject.toml`/`poetry.lock`, so your local choice stays uncommitted.

Because torch is already present after `poetry install`, you must **uninstall it
first** — `pip install` alone would report "Requirement already satisfied" and skip
the swap. For AMD ROCm 7.2 (nightly — note the `--pre`):

```
poetry run pip uninstall -y torch
poetry run pip install --pre torch --index-url https://download.pytorch.org/whl/nightly/rocm7.2
```

(Optional: also `pip uninstall` the leftover `nvidia-*` / `triton` wheels the
default CUDA torch dragged in — unused under ROCm.) Verify:

```
poetry run python -c "import torch; print(torch.__version__, torch.cuda.is_available())"
```

ROCm exposes the GPU through the `cuda` device API, so `torch.cuda.is_available()`
returning `True` (and `--device cuda`) is correct on AMD too.

> A later `poetry install` (e.g. after a dependency bump) re-pulls the default
> torch and reverts this — just re-run the two commands above.

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

`POST /encode` `{ "texts": [...] }` → `{ "dense_vecs", "sparse_vecs", "colbert_vecs" }`,
positionally aligned with the input. `GET /health` reports liveness without loading
the model. mindex depends on these exact shapes — don't change them without changing
mindex in the same commit.
