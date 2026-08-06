"""A minimal OpenAI-compatible embedding server: FastAPI over sentence-transformers.

WHY THIS EXISTS, since mindex deliberately stopped shipping an embedder.

The claim that retired the vendored BGE-M3 server was that any general model
server now returns what dense-only retrieval needs. That is true of the
*protocol* and false of the *throughput*, measured on the reference host (AMD
R9700, ROCm 7.2) with Qwen3-Embedding-0.6B:

    llama.cpp b10221, Q8_0, best of every configuration tried    ~11 chunks/s
    this file, torch fp16, batch 128                            ~119 chunks/s

Eleven times, on the same card, for the same model. Nothing in llama.cpp's
configuration closes it: `-np` 1/8/32, three `--ubatch-size` values, ROCm and
Vulkan backends, and 1/4/8 concurrent clients all landed between 8.5 and 20.7
chunks/s, while `llama-bench` reports 24 400 tok/s for the same weights — so
even its own backend delivers ~3x what its server does for many short
sequences. Indexing is ~91% embedder time, so an 11x embedder is an 11x
reindex, and "reindexing is close to free" is the property this project is
built on.

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
    MINDEX_EMBED_DEVICE=cuda MINDEX_EMBED_DTYPE=float16 \
    uvicorn server:app --host 127.0.0.1 --port 11212
"""

from __future__ import annotations

import logging
import os
import threading
import time

# The stubs for these live in the venv this server runs in, not in the repo's
# lint environment; the mock embedder under tests/ carries the same ignores.
import numpy as np  # type: ignore[import-not-found]
import torch  # type: ignore[import-not-found]
from fastapi import FastAPI, HTTPException  # type: ignore[import-not-found]
from fastapi.responses import ORJSONResponse  # type: ignore[import-not-found]
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

_model: SentenceTransformer | None = None
# One GPU, one forward at a time. mindex-index sends several requests
# concurrently by design (it overlaps slicing and Qdrant upserts with the GPU);
# letting them into the model together buys nothing — the device is already
# saturated by one batch — and multiplies peak activation memory by the number
# of clients, which is how an embedder OOMs under exactly the load it was sized
# for.
_gpu = threading.Lock()

app = FastAPI(title="mindex embedder", default_response_class=ORJSONResponse)


class EmbeddingsRequest(BaseModel):
    input: list[str] | str
    model: str | None = None
    # Accepted and ignored: mindex never asks for base64, and refusing an
    # unknown value would be a worse failure than serving floats.
    encoding_format: str | None = None


@app.on_event("startup")
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
        model.get_sentence_embedding_dimension(),
        TOKEN_BUDGET,
        MAX_ROWS,
        MAX_SEQ,
    )


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
def health() -> dict[str, str]:
    return {"status": "ok" if _model is not None else "loading"}


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
        torch.cuda.empty_cache()
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
    if not np.isfinite(vecs).all():
        LOG.warning("non-finite output for %d rows; recomputing in float32", len(group))
        with torch.autocast(device_type="cuda", enabled=False):
            vecs = _model.encode(
                [texts[i] for i in group],
                batch_size=max(1, len(group) // 2),
                normalize_embeddings=True,
                convert_to_numpy=True,
                show_progress_bar=False,
                precision="float32",
            )
        if not np.isfinite(vecs).all():
            raise HTTPException(
                status_code=500, detail="embedder produced non-finite vectors"
            )
    for slot, vec in zip(group, vecs):
        out[slot] = vec


@app.post("/v1/embeddings")
def embeddings(req: EmbeddingsRequest) -> ORJSONResponse:
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
    if (
        TRIM_MIB
        and torch.cuda.is_available()
        and (torch.cuda.memory_reserved() > TRIM_MIB * 1024 * 1024)
    ):
        torch.cuda.empty_cache()
    dt = time.time() - t
    LOG.info(
        "embedded %d texts (%d tokens) in %.2fs (%.1f/s)",
        len(texts),
        sum(lengths),
        dt,
        len(texts) / max(dt, 1e-9),
    )

    # orjson serialises numpy arrays natively, which matters: this response is
    # len(texts) x width floats and JSON encoding of it is the one CPU cost on
    # the path that scales with the batch.
    return ORJSONResponse(
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
        }
    )
