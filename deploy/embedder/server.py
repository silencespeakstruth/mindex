"""A minimal OpenAI-compatible embedding server: FastAPI over sentence-transformers.

WHY THIS EXISTS, since mindex deliberately stopped shipping an embedder.

The claim that retired the vendored BGE-M3 server was that any general model
server now returns what dense-only retrieval needs. That is true of the
*protocol* and false of the *throughput*, measured on the reference host (AMD
R9700, ROCm 7.2) with Qwen3-Embedding-0.6B by reindexing this repository end to
end:

    this file, torch bf16                                  51 s   (~16 ms/query)
    llama.cpp b10221, Q8_0, best of every configuration    410 s  (~30 ms/query)

**Eight times**, on the same card, for the same model. (These are the numbers in
README.md's table; earlier drafts of this docstring quoted a chunks/s figure from
a different corpus and said eleven, which is how one file ends up disagreeing
with the one beside it.) Nothing in llama.cpp's configuration closes it: `-np`
1/8/32, three `--ubatch-size` values, ROCm and Vulkan backends, and 1/4/8
concurrent clients were all measured, while `llama-bench` reports 24 400 tok/s
for the same weights — so even its own backend delivers ~3x what its server does
for many short sequences. Indexing is ~91% embedder time, so an 8x embedder is an
8x reindex, and "reindexing is close to free" is the property this project is
built on. Query latency goes the other way and barely matters: 16 ms against
30 ms, which is what `[model].query_server_url` exists to let you split.

So this is a *reference implementation of the contract*, not a return to a
vendored protocol. mindex speaks the same OpenAI endpoints to it as to vLLM or
llama.cpp; nothing in the server knows this file exists. Prefer vLLM if it
installs on your host — it is the better-supported version of exactly this
trade. Prefer llama.cpp when query latency is all you need (it is ~30 ms either
way) and reindexing is rare.

Endpoints, which are the whole contract:
    POST /v1/embeddings   {"model": ..., "input": [str, ...]} -> one row per
                          input, IN REQUEST ORDER, each of the model's width
    GET  /v1/models       must list the id mindex's [model].served_name names
    GET  /health          liveness

Two properties the wire cannot state and this file therefore takes care of:
last-token pooling and L2 normalisation. Both come from the model's own
sentence-transformers configuration, which is why it is loaded that way rather
than as a bare transformer — and both are verified against this file by
deploy/embedder/README.md's check 2, because getting them wrong does not fail,
it just retrieves worse.

Run:
    MINDEX_EMBED_MODEL=Qwen/Qwen3-Embedding-0.6B \
    MINDEX_EMBED_DEVICE=cuda MINDEX_EMBED_DTYPE=bfloat16 \
    uvicorn server:app --host 127.0.0.1 --port 11211
"""

from __future__ import annotations

import logging
import os
import threading
import time
from collections.abc import AsyncGenerator
from contextlib import asynccontextmanager

# The stubs for these live in the venv this server runs in, not in the repo's
# lint environment; the mock embedder under tests/ carries the same ignores.
import numpy as np  # type: ignore[import-not-found]
import orjson  # type: ignore[import-not-found]
import torch  # type: ignore[import-not-found]
from fastapi import FastAPI, HTTPException, Response  # type: ignore[import-not-found]
from pydantic import BaseModel  # type: ignore[import-not-found]
from sentence_transformers import SentenceTransformer  # type: ignore[import-not-found]

LOG = logging.getLogger("mindex-embedder")

MODEL_ID = os.environ.get("MINDEX_EMBED_MODEL", "Qwen/Qwen3-Embedding-0.6B")
# What GET /v1/models reports and what mindex's [model].served_name must match.
# Defaults to the model id, which is what the registry expects.
SERVED_NAME = os.environ.get("MINDEX_EMBED_SERVED_NAME", MODEL_ID)
DEVICE = os.environ.get("MINDEX_EMBED_DEVICE", "cuda")
# bfloat16, not float16, and this one bit was a silent corruption: Qwen3 is
# trained in bf16, and in fp16 this model produced **NaN** rows for the longest
# chunks — which orjson writes as JSON `null`, so mindex refused the batch with
# "invalid type: null, expected f32". The refusal is the lucky outcome. A NaN
# that survives into Qdrant is a vector that scores NaN against everything,
# which mindex ranks last and counts in `search_unscorable_winners` — a quiet
# quality loss rather than an error. Same memory, same speed, no overflow.
DTYPE = os.environ.get("MINDEX_EMBED_DTYPE", "bfloat16")
# The batch is formed by a TOKEN budget, not a text count, and the difference
# is the whole reason this file has a batching loop at all. Activation memory
# scales with rows x sequence length, so a fixed count is sized for the average
# chunk and OOMs on the long tail: mindex emits code chunks capped at ~364
# tokens and documentation chunks capped at 1024, so a batch of 96 is 35k
# tokens on one pass and 98k on the next. Measured: count-based batching at 96
# OOM'd on this repo's own markdown while the same server answered a synthetic
# 256-chunk batch of short texts at 83/s.
#
# Budget in tokens per forward pass; the OOM fallback halves a group rather
# than failing it, so a wrong value here costs throughput and not a request.
TOKEN_BUDGET = int(os.environ.get("MINDEX_EMBED_TOKEN_BUDGET", "16384"))
TRIM_MIB = int(os.environ.get("MINDEX_EMBED_TRIM_MIB", "0"))
# Hard cap on rows per pass regardless of length — short texts would otherwise
# form batches of thousands, where the per-row overhead stops amortising and
# the padding to the group's longest member starts to dominate.
MAX_ROWS = int(os.environ.get("MINDEX_EMBED_MAX_ROWS", "128"))
# Truncation, in tokens. Must be >= mindex's [slicer].max_doc_chunk_tokens
# (1024 by default) or long documentation chunks are silently embedded from
# their first N tokens. Do not raise it "for headroom": it is the second factor
# in the activation peak below.
MAX_SEQ = int(os.environ.get("MINDEX_EMBED_MAX_SEQ", "1024"))
# Return the allocator's pool to the driver after a request that grew it past
# this, in MiB. **Defaults to 0 — off — and the reason is a measured
# corruption, not caution.**
#
# The problem is real: torch never gives freed blocks back on its own, so the
# pool grows to the largest batch ever seen and stays there (measured here: a
# 0.6B model whose weights are 1.2 GiB sat on 20.4 GiB of VRAM after one
# oversized pass, on a card that also runs a compositor and a research LLM).
# Two fixes were tried and both were worse than the problem:
#
#   * `set_per_process_memory_fraction` turns a memory spike into a FAILED
#     REQUEST, which costs the caller a whole batch of files. It fired on this
#     repo's own markdown.
#   * `torch.cuda.empty_cache()` between requests made this stack (ROCm 7.2,
#     torch 2.13-dev, PYTORCH_HIP_ALLOC_CONF=expandable_segments) return **NaN**
#     for the longest chunks — reproducible in the server, absent in-process
#     without the call, and gone the moment the trim was disabled. NaN reaches
#     mindex as JSON `null` and is refused; had it been finite garbage instead
#     it would have been indexed.
#
# So the pool is bounded where it is created: by TOKEN_BUDGET, which caps the
# peak allocation rather than releasing it afterwards. Set this only on a stack
# where you have verified the call is harmless.


def _accelerator():  # type: ignore[no-untyped-def]
    """`torch.cuda`, `torch.xpu`, … for the configured device — or None on CPU.

    Everything torch exposes per-backend (`empty_cache`, `memory_reserved`,
    `is_available`) lives on a module named after the device family, and this
    file used to hardcode `torch.cuda`. That is right for ROCm — its torch build
    keeps the `cuda` spelling, which is why `MINDEX_EMBED_DEVICE=cuda` is correct
    on an AMD card — and wrong for the two fallbacks the env var otherwise
    invites: `xpu` (Intel) would `AttributeError` on the OOM path, and `cpu` has
    no such module at all.

    Returning None for CPU rather than raising is the point: on CPU there is no
    allocator pool to trim and no OOM to recover from by trimming, so every
    caller's correct behaviour is to skip.
    """
    family = DEVICE.split(":", 1)[0]
    if family == "cpu":
        return None
    return getattr(torch, family, None)


def _empty_cache() -> None:
    """Return the allocator's pool to the driver, if this backend has one."""
    accel = _accelerator()
    if accel is not None and hasattr(accel, "empty_cache"):
        accel.empty_cache()


def _memory_reserved() -> int:
    accel = _accelerator()
    if accel is None or not hasattr(accel, "memory_reserved"):
        return 0
    try:
        return int(accel.memory_reserved())
    except Exception:  # noqa: BLE001 — a diagnostic must not fail a request
        return 0


_model: SentenceTransformer | None = None
# One GPU, one forward at a time. mindex-index sends several requests
# concurrently by design (it overlaps slicing and Qdrant upserts with the GPU);
# letting them into the model together buys nothing — the device is already
# saturated by one batch — and multiplies peak activation memory by the number
# of clients, which is how an embedder OOMs under exactly the load it was sized
# for.
_gpu = threading.Lock()


class EmbeddingsRequest(BaseModel):
    input: list[str] | str
    model: str | None = None
    # Accepted and ignored: mindex never asks for base64, and refusing an
    # unknown value would be a worse failure than serving floats.
    encoding_format: str | None = None


def _width(model: SentenceTransformer) -> int:
    """The model's output width, across a sentence-transformers rename.

    `get_sentence_embedding_dimension` is deprecated in favour of
    `get_embedding_dimension` and currently emits a FutureWarning on every start.
    It is only used for a log line, so the fallback costs nothing — and a log
    line is exactly the kind of caller that gets left behind when the old name
    finally goes, taking the startup with it.
    """
    for name in ("get_embedding_dimension", "get_sentence_embedding_dimension"):
        fn = getattr(model, name, None)
        if fn is not None:
            return int(fn())
    return -1


@asynccontextmanager
async def _lifespan(_app: FastAPI) -> AsyncGenerator[None]:
    """Load the model once, before the first request is served.

    A lifespan handler rather than `@app.on_event("startup")`, which is
    deprecated and will be removed. That matters more here than the usual
    deprecation does, because of how this file fails without it: when the hook
    stops running, `_model` stays None forever, every `/v1/embeddings` answers
    503 — and `/health` would have gone on answering 200, so mindex would have
    kept the embedder marked healthy while nothing could be embedded. The
    503-while-loading fix above and this one close the same hole from two sides.
    """
    _load()
    yield


def _load() -> None:
    global _model
    t = time.time()
    LOG.info("loading %s on %s (%s)", MODEL_ID, DEVICE, DTYPE)
    model = SentenceTransformer(
        MODEL_ID, device=DEVICE, model_kwargs={"torch_dtype": getattr(torch, DTYPE)}
    )
    model.max_seq_length = MAX_SEQ
    _model = model
    LOG.info(
        "loaded in %.1fs, width %d, token budget %d, max rows %d, max_seq %d",
        time.time() - t,
        _width(model),
        TOKEN_BUDGET,
        MAX_ROWS,
        MAX_SEQ,
    )


app = FastAPI(title="mindex embedder", lifespan=_lifespan)


def _token_lengths(model: SentenceTransformer, texts: list[str]) -> list[int]:
    """Token count per text, clamped to the truncation length.

    The real tokenizer rather than a chars/4 estimate, because the estimate is
    wrong in exactly the direction that hurts: code and minified documentation
    tokenize far denser than prose, so the guess under-counts the longest
    inputs and the batch that OOMs is the one the estimate called small.
    """
    enc = model.tokenizer(
        texts, add_special_tokens=True, truncation=True, max_length=MAX_SEQ
    )
    return [len(ids) for ids in enc["input_ids"]]


def _groups(lengths: list[int]) -> list[list[int]]:
    """Indices grouped into forward passes, longest first.

    Sorted by length so each pass pads to something close to its own members'
    length, and greedy against `rows x longest <= TOKEN_BUDGET` — the padded
    size of the pass, which is what the GPU actually allocates, rather than the
    sum of the real lengths, which is what it does not.
    """
    order = sorted(range(len(lengths)), key=lambda i: lengths[i], reverse=True)
    out: list[list[int]] = []
    cur: list[int] = []
    for i in order:
        longest = lengths[cur[0]] if cur else lengths[i]
        if cur and ((len(cur) + 1) * longest > TOKEN_BUDGET or len(cur) >= MAX_ROWS):
            out.append(cur)
            cur = []
        cur.append(i)
    if cur:
        out.append(cur)
    return out


@app.get("/health")
def health(response: Response) -> dict[str, str]:
    """Liveness, and it must be **503 while loading**, not 200.

    mindex's client checks `error_for_status()` and nothing else, so a 200 here
    means "the embedder is Ok" — on `GET /health` and in the startup handshake
    alike. A cold load takes up to five minutes (`TimeoutStartSec=300` in the
    unit), and for all of it this returned 200 while every `/v1/embeddings`
    answered 503. Since 503 is on mindex's retry path, files spent their whole
    retry budget against a server it had been told was healthy, and the operator
    saw `checks.embedder: "ok"` throughout.

    The body keeps its word for a human reading it directly; the status is what
    anything automated reads.
    """
    if _model is None:
        response.status_code = 503
        return {"status": "loading"}
    return {"status": "ok"}


@app.get("/v1/models")
def models() -> dict[str, object]:
    return {
        "object": "list",
        "data": [{"id": SERVED_NAME, "object": "model", "owned_by": "mindex"}],
    }


def _encode_into(out: list[object], texts: list[str], group: list[int]) -> None:
    """Encode one group into `out` at its own indices, halving on OOM.

    The halving is not defensive programming, it is what makes TOKEN_BUDGET a
    tuning knob rather than a tripwire: the budget that fits depends on the
    model, the dtype, the allocator's fragmentation and whatever else holds the
    card at that moment, so a value that was right last week can OOM today. A
    failed forward costs the request otherwise — and the caller's whole batch
    of files goes `failed` with it.
    """
    assert _model is not None
    try:
        vecs = _model.encode(
            [texts[i] for i in group],
            batch_size=len(group),
            normalize_embeddings=True,
            convert_to_numpy=True,
            show_progress_bar=False,
        )
    except torch.OutOfMemoryError:
        if len(group) == 1:
            raise
        # YES, THIS IS THE CALL TRIM_MIB SPENDS TWENTY LINES REFUSING TO MAKE,
        # and the difference is not inconsistency. There, it ran *between*
        # healthy requests to shrink a pool nothing needed shrunk, buying tidier
        # `rocm-smi` output at the price of a documented corruption. Here the
        # allocator has already failed, the retry needs the blocks back, and the
        # alternative is a 500 that fails the caller's whole batch of files.
        #
        # What makes it safe rather than merely necessary: each recursive call
        # runs its own `np.isfinite` check below, so the exact failure mode the
        # TRIM_MIB note describes — NaN for the longest chunks after a trim — is
        # caught on the retry rather than shipped. A NaN here costs a recompute;
        # a NaN there would have reached Qdrant.
        _empty_cache()
        half = len(group) // 2
        LOG.warning("out of memory at %d rows; splitting", len(group))
        _encode_into(out, texts, group[:half])
        _encode_into(out, texts, group[half:])
        return
    # A vector that is not finite is not a vector. This costs one pass over the
    # batch and catches the whole class the dtype note above describes — an
    # overflow in a reduced precision, a bad kernel, a half-broken card — at the
    # only place that can still do something about it. Recomputing the group in
    # fp32 is the fix that keeps the request whole; if THAT is not finite the
    # request fails, because shipping a NaN to an index is worse than failing.
    #
    # THE RECOMPUTE HAS TO CHANGE THE WEIGHTS, and the first version of it did
    # not. It passed `precision="float32"` to `encode`, which selects the OUTPUT
    # quantization (`float32|int8|uint8|binary|ubinary`) and is already the
    # default — and wrapped the call in `torch.autocast(enabled=False)`, which
    # does nothing for a model whose parameters are natively bf16, since autocast
    # is not what reduced their precision. So the "recompute in float32" branch
    # recomputed in exactly the precision that had just produced NaN, and the
    # deterministic outcome was a second NaN and a 500: a documented safety net
    # that was not there, in the file published as the reference server.
    #
    # Casting the module is the only thing that actually raises the compute
    # precision. It is expensive (a 0.6B model is 1.2 GiB in bf16, 2.4 GiB in
    # fp32) and it is a rare path — and it is done under `_gpu`, which the caller
    # already holds, so no concurrent request can observe the model mid-cast. The
    # restore is in a `finally` because a model left in fp32 would silently halve
    # throughput for the life of the process.
    if not np.isfinite(vecs).all():
        LOG.warning("non-finite output for %d rows; recomputing in float32", len(group))
        original = next(_model.parameters()).dtype
        try:
            _model.to(torch.float32)
            vecs = _model.encode(
                [texts[i] for i in group],
                batch_size=max(1, len(group) // 2),
                normalize_embeddings=True,
                convert_to_numpy=True,
                show_progress_bar=False,
            )
        finally:
            _model.to(original)
        if not np.isfinite(vecs).all():
            raise HTTPException(
                status_code=500, detail="embedder produced non-finite vectors"
            )
    for slot, vec in zip(group, vecs):
        out[slot] = vec


@app.post("/v1/embeddings")
def embeddings(req: EmbeddingsRequest) -> Response:
    if _model is None:
        raise HTTPException(status_code=503, detail="model still loading")
    texts = [req.input] if isinstance(req.input, str) else req.input
    if not texts:
        raise HTTPException(status_code=400, detail="input is empty")

    t = time.time()
    out: list[object] = [None] * len(texts)
    with _gpu:
        lengths = _token_lengths(_model, texts)
        for group in _groups(lengths):
            _encode_into(out, texts, group)
    if TRIM_MIB and _memory_reserved() > TRIM_MIB * 1024 * 1024:
        _empty_cache()
    dt = time.time() - t
    LOG.info(
        "embedded %d texts (%d tokens) in %.2fs (%.1f/s)",
        len(texts),
        sum(lengths),
        dt,
        len(texts) / max(dt, 1e-9),
    )

    # Serialised here rather than returned as a dict, for two reasons that both
    # matter at this size: FastAPI's own path would validate and re-encode
    # len(texts) x width floats, and orjson writes numpy arrays natively with
    # OPT_SERIALIZE_NUMPY instead of converting each row to a Python list first.
    # JSON encoding is the one CPU cost on this path that scales with the batch.
    payload = orjson.dumps(
        {
            "object": "list",
            "model": SERVED_NAME,
            # `out` is filled by index, so the response is in REQUEST order
            # however the groups were formed — the property mindex relies on,
            # since it matches rows to chunks positionally and by nothing else.
            "data": [
                {"object": "embedding", "index": i, "embedding": v}
                for i, v in enumerate(out)
            ],
            "usage": {"prompt_tokens": 0, "total_tokens": 0},
        },
        option=orjson.OPT_SERIALIZE_NUMPY,
    )
    return Response(content=payload, media_type="application/json")
