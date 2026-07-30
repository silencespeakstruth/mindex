# embedder — BGE-M3 three-head embedding server

A vendored wrapper exposing BGE-M3's **three** outputs — dense, sparse (SPLADE-style
lexical weights) and ColBERT multivectors — over `POST /encode` + `GET /health`, which
mindex consumes via `--model-server`.

**Why it exists, and why it's temporary:** no general-purpose model server (vLLM,
Ollama, …) returns all three heads together, and mindex's hybrid retrieval needs them.
The `/encode` contract is kept minimal so this can be deleted the day one does.

**It is not in the Docker image and not in compose.** torch alone is ~8 GB and it wants
direct GPU access, so it runs on the **host**; a containerized mindex reaches it via
`host.docker.internal`. `tests/mock_embedder/` is the lightweight CI stand-in.

## Run

```sh
cd embedder
uv sync                                    # installs everything EXCEPT torch (see below)
uv run python -m bge_m3_api --port 11211   # binds 0.0.0.0; ~4–6 GB VRAM, or CPU
```

Useful flags (`--help`): `--device` (`cuda` / `cuda:0` / `xpu` / `cpu`), `--batch`,
`--max-inflight` (429 beyond it), `--idle-timeout` (unload after N idle seconds).
The device string is the only backend-aware thing in the server: `accelerator()`
maps it to `torch.cuda` (NVIDIA, and AMD via ROCm) or `torch.xpu` (Intel), and
nothing else branches on it.

> Bind `0.0.0.0` (the default), **not** `127.0.0.1` — a Dockerized mindex arrives via
> the bridge gateway, which `127.0.0.1` excludes.

## Example: two GPUs, two instances

Everything in this section is a **worked example** from one development machine, not a
requirement — a single instance on one device is the ordinary case. The units under
`systemd/` are there to be copied and edited, and the numbers below are what that
machine measured, not a spec.

It has both a discrete AMD card (32 GiB, ROCm) and an idle Intel Arc iGPU (Xe2,
sharing system RAM). The embedder and the research LLM cannot both fit on the discrete
card once `[research].max_num_ctx_tokens` is generous, and of the two the embedder is
the one that does not need it: a query is a single ~20-token text and is
latency-bound.

So it runs as a **systemd template**, `mindex-embedder@.service`, with two instances:

| instance | device | venv | when |
| --- | --- | --- | --- |
| `@igpu` | `xpu` | `.venv-igpu` | **default** — leaves the whole discrete card to the LLM |
| `@egpu` | `cuda` (ROCm) | `.venv-egpu` | bulk reindexing, where throughput dominates |

They are **mutually exclusive** — both bind port 11211 — and systemd enforces it
rather than the port doing so by accident: the template carries a symmetric
`Conflicts=` + `After=` naming both instances, and systemd drops the self-reference
on each line (`systemd-analyze --user verify` prints "Dependency … is dropped").
`Conflicts=` stops the other instance, `After=` orders that stop before this start
so the port is free by the time we bind.

Install (the unit lives in the repo, `~/.config` gets symlinks):

```sh
ln -sfn "$PWD/systemd/mindex-embedder@.service" ~/.config/systemd/user/
install -Dm644 systemd/embedder-egpu.env systemd/embedder-igpu.env -t ~/.config/mindex/
systemctl --user daemon-reload
systemctl --user enable --now mindex-embedder@igpu.service
```

Switching. Enable exactly **one** instance — two enabled instances race for the port
on boot, which `Conflicts=` cannot prevent (it excludes, it does not choose):

```sh
systemctl --user enable --now mindex-embedder@egpu.service   # Conflicts= stops @igpu
systemctl --user disable      mindex-embedder@igpu.service   # so it doesn't return on boot
```

The swap costs ~30–60 s of embedder downtime (model load). That is already covered by
the contract: in-flight files go `failed` and the retry worker picks them up, `/search`
returns 503 `EmbedderUnavailable` in the meantime.

Everything else is shared and lives in the template; the two `.env` files hold only
`EMBEDDER_DEVICE`, `EMBEDDER_BATCH` and the backend's own environment (ROCm's
`HSA_*`/`MIOPEN_*` on one side, `ONEAPI_DEVICE_SELECTOR` on the other). The venv is
selected by `%i` **in the ExecStart path**, not by a variable — the first token of
`ExecStart=` must be a literal absolute path.

### The XPU attention trap (read before touching `attention_backend`)

On torch 2.13 + Xe2, `scaled_dot_product_attention`'s **default** XPU backend returns
**NaN** for any row carrying padding, in fp16 with an attention mask — that is, for
every batch of more than one text, since they are padded to the longest. It fails
**silently**: the request still returns 200, the NaN just lands in that row's heads.
`encode_direct` therefore runs the forward inside `attention_backend()`, which pins
XPU to `EFFICIENT_ATTENTION` then `MATH`. Do not remove it without re-running the
parity check below; do re-test it on torch upgrades, since it disappears the day the
default is fixed.

Measured on the machine above, ROCm fp16 vs XPU fp16, 86 chunks of ~1000 tokens:

| | default XPU backend | with `attention_backend()` |
| --- | --- | --- |
| dense cosine (mean / min) | 0.275 / 0.225 | **0.999996 / 0.999976** |
| ColBERT MaxSim (mean) | 0.391 | **0.999964** |
| sparse token-set Jaccard (mean) | 0.807 | **0.9968** |
| disagreeing sparse ids | 18.6 % of the set | 0.26 % / 0.07 %, weights ≤ 0.0036 |

So the two backends *are* interchangeable once the workaround is in: what disagrees
is a fraction of a percent of sparse ids carrying near-zero weight. The correctness
costs about 2× the (wrong) default's time.

### Cost of the swap

Same 86-chunk batch, warm: **2.2 s on @egpu, 39 s on @igpu** (~17×). A single query
of ~20 tokens, warm: **~28 ms on @igpu** — which is the whole point. The iGPU is
priced for the query path and for incremental reindexing of a few touched files;
a full `mindex-index --force` over a repo belongs on `@egpu`.

> Parity is measured, not guaranteed, and nothing checks it at runtime. If search ever
> starts missing the obvious thing after a torch or model change, re-run the comparison
> above before suspecting anything else — per `.claude/CLAUDE.md` a sparse head that
> disagrees between the indexing and query sides presents exactly that way, and never
> as an error.

## torch is supplied out-of-band

`pyproject.toml` deliberately drops torch from resolution (a never-true
`[tool.uv] override-dependencies` marker), because the right build is per-machine
(ROCm / CUDA / XPU / CPU) and the default wheel drags in multi-GB CUDA libs. So
`uv sync` never installs *or reverts* torch — you put it in the venv yourself.

That is also what makes **one venv per backend** cheap: they share this project's
pure-Python dependencies and differ only in the torch inside them. Build both with
`UV_PROJECT_ENVIRONMENT`, which picks the venv directory:

```sh
UV_PROJECT_ENVIRONMENT=.venv-egpu uv sync
UV_PROJECT_ENVIRONMENT=.venv-igpu uv sync
```

**eGPU (ROCm).** Keep **one** ROCm 7.2 torch in a project-neutral home and symlink it
in, so the ~14 GB lives on disk once (`CANON` is wherever you put it):

```sh
CANON=/path/to/shared/rocm-torch-py313
DST=.venv-egpu/lib/python3.13/site-packages
for p in "$CANON"/torch "$CANON"/functorch "$CANON"/torchgen "$CANON"/torch-*.dist-info \
         "$CANON"/triton "$CANON"/triton_rocm-*.dist-info; do
    ln -sfn "$p" "$DST/$(basename "$p")"
done
```

**iGPU (Intel XPU).** The wheel is small enough (~3 GB) not to be worth sharing yet:

```sh
uv pip install --no-config --python .venv-igpu/bin/python torch \
    --index-url https://download.pytorch.org/whl/xpu
```

`--no-config` is **required**: without it `uv pip` reads this project's
`override-dependencies` marker and drops torch from the install as well, reporting
success ("Checked 1 package") while installing nothing. The host also needs
`intel-compute-runtime` and `level-zero-loader`; nothing else.

`$CANON` must be the **same Python minor** (3.13) as the venv. Verify each with

```sh
.venv-egpu/bin/python -c "import torch; print(torch.__version__, torch.cuda.is_available())"
.venv-igpu/bin/python -c "import torch; print(torch.__version__, torch.xpu.is_available())"
```

— on ROCm, `cuda` device names and `is_available() == True` are correct.

> The Python pin (3.13 only) is narrow on purpose: the ROCm builds this targets are
> finicky on Pro-class AMD cards. Don't widen it without testing on your GPU.

## Contract (frozen)

`POST /encode {"texts": [...]}` → a **little-endian binary body**, not JSON: ColBERT is
one 1024-d vector *per token*, so JSON ran to hundreds of MB per call and serialization
dominated request time. The layout is documented on `pack_encode` in
`src/bge_m3_api/__main__.py`; `src/models/bge_m3.rs::parse_encode_response` and
`tests/mock_embedder/main.py` mirror it **byte-for-byte** — change one, change all three
in the same commit.

`GET /stats` / `POST /stats/reset` expose rolling counters (forward passes, batch fill,
429s, queue high-water) for `perf/run.sh` only; mindex never calls them and they are not
part of the frozen contract.
