"""BGE-M3 three-head embedding API (dense / sparse / ColBERT).

Single GPU, single process: all model work is serialized through one worker
thread (one inference touches the GPU at a time — important for ROCm). Requests
queue up to ``--max-inflight``; beyond that the server replies 429. The model is
loaded lazily on first use, kept resident between calls, and unloaded after
``--idle-timeout`` seconds of inactivity.

Run exactly one process (no ``uvicorn --workers N``): multiple workers would each
load their own copy of the model and hit the GPU in parallel.

Why this wrapper exists: general-purpose model servers (vLLM, Ollama, ...) return
only dense embeddings — none expose BGE-M3's sparse lexical weights and ColBERT
token vectors together, which mindex's hybrid retrieval needs. This service exists
solely to bridge that gap and is meant to be removed once an off-the-shelf server
emits all three heads (see embedder/README.md).
"""

import argparse
import asyncio
import functools
import gc
import logging
import struct
import threading
import time
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from contextlib import asynccontextmanager, contextmanager
from typing import Any, Callable, Iterator, List

import numpy as np
import torch
import uvicorn
from fastapi import FastAPI, HTTPException, Response
from fastapi.responses import ORJSONResponse
from pydantic import BaseModel
from FlagEmbedding import BGEM3FlagModel

logging.basicConfig(
    level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
)
log = logging.getLogger("bge_m3_api")

MODEL_NAME = "BAAI/bge-m3"

# ─── Binary wire format ──────────────────────────────────────────────────────
# /encode replies with a raw little-endian byte stream instead of JSON. The old
# JSON path spent ~70% of each request on `.tolist()` (materializing the ColBERT
# multivector — one 1024-d vector PER TOKEN — into millions of Python floats) plus
# orjson.dumps of a ~300 MB text payload, all on the single GPU worker thread. The
# heads have fixed shapes, so a length-prefixed binary needs no schema library and
# is built with numpy `.tobytes()` (one C-level memcpy, zero Python-float boxing):
#
#   magic   b"BM3\x01"                       (4 bytes)
#   n       u32                              chunk count
#   dim     u32                              dense/colbert width (1024)
#   dense   n * dim * f32                    one row per chunk, contiguous
#   sparse  per chunk: count u32, then count*u32 token-ids, then count*f32 weights
#   colbert per chunk: tokens u32, then tokens * dim * f32
#
# The Rust client (src/models/bge_m3.rs) mirrors this byte-for-byte; keep the two
# in lockstep. f32 throughout (dense/colbert are cosine-normalized — f16 would
# halve traffic later, but f32 keeps the format trivial and lossless for now).
_MAGIC = b"BM3\x01"
DENSE_DIM = 1024


def pack_encode(res: dict) -> bytes:
    """Serialize FlagEmbedding's encode() output to the binary wire format above.

    Cheap on purpose: each head becomes a numpy `.tobytes()` (a single contiguous
    copy), joined once. No per-element Python objects, so it stays fast enough to
    run inline on the GPU worker thread — no separate serialization thread needed.
    """
    dense_arr = np.ascontiguousarray(res["dense_vecs"], dtype=np.float32)
    n = dense_arr.shape[0]
    dim = dense_arr.shape[1] if n else DENSE_DIM

    parts: list[bytes] = [_MAGIC, struct.pack("<II", n, dim), dense_arr.tobytes()]

    for d in res["lexical_weights"]:
        ids = np.fromiter((int(k) for k in d), dtype=np.uint32, count=len(d))
        wts = np.fromiter(d.values(), dtype=np.float32, count=len(d))
        parts.append(struct.pack("<I", ids.shape[0]))
        parts.append(ids.tobytes())
        parts.append(wts.tobytes())

    for v in res["colbert_vecs"]:
        arr = np.ascontiguousarray(v, dtype=np.float32).reshape(-1, dim)
        parts.append(struct.pack("<I", arr.shape[0]))
        parts.append(arr.tobytes())

    return b"".join(parts)


# ─── Direct forward fast path ────────────────────────────────────────────────
# We call BGE-M3's GPU head forward ourselves instead of BGEM3FlagModel.encode().
# FlagEmbedding's encode_single_device runs an extra full forward pass on the first
# batch purely to probe for OOM (its "adjust batch size" loop), discards it, then
# forwards the same data again. Since mindex sends one batch per /encode, that
# DOUBLES the GPU work (~87 vs ~174 chunks/s at batch 252). The heads themselves are
# already computed on-GPU by EncoderOnlyEmbedderM3ModelForInference.forward — we
# reuse it as-is and only replace the wasteful orchestration. The dense/sparse/
# colbert post-processing below mirrors M3Embedder verbatim so retrieval is
# byte-identical to the old path.


def _unused_token_ids(tokenizer) -> set:
    """The special tokens excluded from the sparse lexical weights (cls/eos/pad/unk),
    matching FlagEmbedding M3Embedder._process_token_weights."""
    unused: set = set()
    for name in ("cls_token", "eos_token", "pad_token", "unk_token"):
        if name in tokenizer.special_tokens_map:
            unused.add(
                tokenizer.convert_tokens_to_ids(tokenizer.special_tokens_map[name])
            )
    return unused


def _token_weights_to_dict(token_weights, input_ids: list, unused: set) -> dict:
    """Per-chunk sparse weights → {token_id: weight}, dedup by max, drop special
    tokens and non-positive weights. Mirrors _process_token_weights (keys left as
    int — pack_encode emits them as u32 either way)."""
    result: dict = defaultdict(float)
    for w, idx in zip(token_weights, input_ids):
        if idx not in unused and w > 0 and w > result[idx]:
            result[idx] = w
    return result


@torch.no_grad()
def encode_direct(
    flag_model: BGEM3FlagModel, texts: List[str], batch: int, maxlen: int
) -> dict:
    """Single-forward replacement for BGEM3FlagModel.encode() — returns the same
    {dense_vecs, lexical_weights, colbert_vecs} dict shape pack_encode expects."""
    if not texts:
        return {"dense_vecs": [], "lexical_weights": [], "colbert_vecs": []}

    model = flag_model.model
    tokenizer = flag_model.tokenizer
    device = str(getattr(flag_model, "target_devices", [None])[0] or "cpu")
    # BGEM3FlagModel keeps the model on CPU until its own encode() moves it; we
    # bypass that, so place it ourselves. Idempotent (a no-op once resident), so
    # cheap to call per request.
    model.to(device)
    model.eval()
    unused = _unused_token_ids(tokenizer)

    # Tokenize once (no padding), then sort long→short so each padded sub-batch
    # wastes little padding — the one good idea from FlagEmbedding's path, kept
    # without its discarded probe forward.
    enc = tokenizer(
        texts, truncation=True, max_length=maxlen, return_token_type_ids=False
    )
    items = [{k: enc[k][i] for k in enc} for i in range(len(texts))]
    order = np.argsort([-len(x["input_ids"]) for x in items])
    items_sorted = [items[i] for i in order]

    dense_s: list = []
    lexical_s: list = []
    colbert_s: list = []

    i = 0
    bs = max(1, batch)
    while i < len(items_sorted):
        window = items_sorted[i : i + bs]
        try:
            feats = tokenizer.pad(window, padding=True, return_tensors="pt").to(device)
            out = model(
                feats,
                return_dense=True,
                return_sparse=True,
                return_colbert_vecs=True,
            )
            dense = out["dense_vecs"].float().cpu().numpy()
            sparse_w = out["sparse_vecs"].squeeze(-1).float().cpu().numpy()
            colbert = out["colbert_vecs"].float().cpu().numpy()
            ids = feats["input_ids"].cpu().numpy()
            mask = feats["attention_mask"].cpu().numpy()
        except RuntimeError as e:
            # On OOM, shrink the sub-batch and retry the same window (what upstream's
            # probe loop did, minus the wasted full-size forward). Re-raise anything
            # that isn't an out-of-memory condition.
            is_oom = (
                isinstance(e, torch.cuda.OutOfMemoryError)
                or "out of memory" in str(e).lower()
            )
            if not is_oom or bs == 1:
                raise
            if device.startswith("cuda"):
                torch.cuda.empty_cache()
            bs = max(1, bs * 3 // 4)
            continue

        for j in range(len(window)):
            dense_s.append(dense[j])
            lexical_s.append(
                _token_weights_to_dict(sparse_w[j], ids[j].tolist(), unused)
            )
            tokens_num = int(mask[j].sum())
            colbert_s.append(
                colbert[j][: tokens_num - 1]
            )  # drop padding (cls already gone)
        i += bs

    # Restore original input order (results were built in length-sorted order).
    inv = np.argsort(order)
    return {
        "dense_vecs": [dense_s[k] for k in inv],
        "lexical_weights": [lexical_s[k] for k in inv],
        "colbert_vecs": [colbert_s[k] for k in inv],
    }


parser = argparse.ArgumentParser()
parser.add_argument("--port", type=int, default=8000)
parser.add_argument(
    "--device",
    type=str,
    default="cuda" if torch.cuda.is_available() else "cpu",
    help="Single torch device for the model, e.g. 'cuda', 'cuda:0', 'cpu'. "
    "Pin a specific eGPU with 'cuda:N' (or HIP_VISIBLE_DEVICES). Passed to "
    "FlagEmbedding as `devices=` — one device, no multi-process pool.",
)
parser.add_argument("--batch", type=int, default=16)
parser.add_argument("--maxlen", type=int, default=8192)
parser.add_argument(
    "--max-inflight",
    type=int,
    default=256,
    help="Max requests queued + processing before the server returns 429.",
)
parser.add_argument(
    "--idle-timeout",
    type=float,
    default=600.0,
    help="Seconds of inactivity after which the model is unloaded from the GPU.",
)
args = parser.parse_args()


# ─── Model lifecycle ─────────────────────────────────────────────────────────
# Every method here runs on the single worker thread (submitted via EXECUTOR), so
# load / encode / unload are inherently serialized — no locks needed.


class ModelManager:
    def __init__(self, name: str, device: str, batch: int, maxlen: int) -> None:
        self._name = name
        self._device = device
        self._batch = batch
        self._maxlen = maxlen
        self._model: BGEM3FlagModel | None = None
        self._last_used = time.monotonic()

    def is_loaded(self) -> bool:
        return self._model is not None

    def idle_seconds(self) -> float:
        return time.monotonic() - self._last_used

    def _ensure_loaded(self) -> BGEM3FlagModel:
        if self._model is None:
            log.info("Loading model %s on devices=%r …", self._name, self._device)
            t0 = time.monotonic()
            # FlagEmbedding 1.4 takes `devices` (plural). Passing `device=` would be
            # swallowed by **kwargs and ignored, leaving placement to auto-detect —
            # which on a multi-GPU box returns *all* CUDA devices and triggers a
            # multi-process encode pool (breaks single-GPU serialization). A single
            # device string keeps it one in-process device.
            self._model = BGEM3FlagModel(self._name, devices=self._device)
            log.info(
                "Model loaded in %.1fs (target_devices=%s)",
                time.monotonic() - t0,
                getattr(self._model, "target_devices", "?"),
            )
        return self._model

    def _drop(self) -> None:
        """Release the model and free GPU memory."""
        if self._model is None:
            return
        log.info("Unloading model (idle or recovering from error).")
        self._model = None
        gc.collect()
        if self._device.startswith("cuda"):
            # On ROCm, torch.cuda.* maps to HIP.
            torch.cuda.empty_cache()

    def unload_if_idle(self, idle_timeout: float) -> None:
        if self._model is not None and self.idle_seconds() > idle_timeout:
            self._drop()

    def _run(self, work: Callable[[BGEM3FlagModel], Any]) -> Any:
        """Loads (if needed), runs `work`, and on any failure drops the model so a
        poisoned CUDA/HIP context is rebuilt on the next request."""
        model = self._ensure_loaded()
        try:
            result = work(model)
        except Exception:
            self._drop()
            raise
        self._last_used = time.monotonic()
        return result

    def encode(self, texts: List[str]) -> bytes:
        def work(model: BGEM3FlagModel) -> bytes:
            # encode_direct calls the GPU head forward ONCE (no FlagEmbedding probe
            # double-forward); pack to bytes here on the worker thread — a handful of
            # numpy memcpys, not millions of Python-float allocations.
            res = encode_direct(model, texts, self._batch, self._maxlen)
            return pack_encode(res)

        return self._run(work)


# ─── Throughput stats (hardware-agnostic) ────────────────────────────────────
# Logical counters for the perf harness: no GPU/VRAM probing, just request shape.
# Forward-pass batch sizes are derived from len(texts) and --batch, which is exactly
# how FlagEmbedding sub-batches a single encode() internally — so it shows whether
# each /encode call actually fills the configured batch (the GPU-feed signal) without
# hooking the model. Updated from both the worker thread (encode) and the event-loop
# thread (inflight/429), so guarded by a plain lock.


class Stats:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.reset()

    def reset(self) -> None:
        with self._lock:
            self._forward_passes = 0
            self._seqs_total = 0  # Σ texts encoded == Σ forward-pass sizes.
            self._forward_batch_max = 0
            self._encode_calls = 0
            self._encode_seconds_total = 0.0
            self._chunks_encoded_total = 0
            self._requests_429 = 0
            self._queue_depth_highwater = 0

    def record_encode(self, n_texts: int, batch: int, seconds: float) -> None:
        with self._lock:
            self._encode_calls += 1
            self._encode_seconds_total += seconds
            self._chunks_encoded_total += n_texts
            if n_texts > 0:
                passes = (n_texts + batch - 1) // batch  # ceil(n / batch)
                self._forward_passes += passes
                self._seqs_total += n_texts
                self._forward_batch_max = max(
                    self._forward_batch_max, min(batch, n_texts)
                )

    def record_429(self) -> None:
        with self._lock:
            self._requests_429 += 1

    def observe_inflight(self, inflight: int) -> None:
        with self._lock:
            self._queue_depth_highwater = max(self._queue_depth_highwater, inflight)

    def snapshot(self) -> dict:
        with self._lock:
            mean = (
                self._seqs_total / self._forward_passes if self._forward_passes else 0.0
            )
            return {
                "forward_passes": self._forward_passes,
                "forward_batch_mean": round(mean, 2),
                "forward_batch_max": self._forward_batch_max,
                "encode_calls": self._encode_calls,
                "encode_seconds_total": round(self._encode_seconds_total, 3),
                "chunks_encoded_total": self._chunks_encoded_total,
                "requests_429": self._requests_429,
                "queue_depth_highwater": self._queue_depth_highwater,
            }


STATS = Stats()

MANAGER = ModelManager(MODEL_NAME, args.device, args.batch, args.maxlen)

# One worker thread = serialized GPU access; FIFO over its internal queue.
EXECUTOR = ThreadPoolExecutor(max_workers=1, thread_name_prefix="bge-gpu")

# `_inflight` is only ever read/written on the event-loop thread, so a plain int
# is safe without locking.
_inflight = 0


@contextmanager
def capacity_slot() -> Iterator[None]:
    """Reserves one of the `--max-inflight` slots or rejects with 429. Held for the
    whole request so the count reflects queued + processing work."""
    global _inflight
    if _inflight >= args.max_inflight:
        STATS.record_429()
        raise HTTPException(
            status_code=429,
            detail=f"Resource busy: {args.max_inflight} requests already in flight.",
        )
    _inflight += 1
    STATS.observe_inflight(_inflight)
    try:
        yield
    finally:
        _inflight -= 1


async def on_gpu(fn: Callable[..., Any], *fn_args: Any) -> Any:
    """Runs a ModelManager method on the single GPU worker thread."""
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(EXECUTOR, functools.partial(fn, *fn_args))


# ─── Idle unloader + lifespan ────────────────────────────────────────────────


async def _idle_unloader() -> None:
    interval = max(5.0, min(60.0, args.idle_timeout))
    while True:
        await asyncio.sleep(interval)
        # Only unload when nothing is in flight, so we never drop the model out
        # from under a running request.
        if _inflight == 0 and MANAGER.is_loaded():
            try:
                await on_gpu(MANAGER.unload_if_idle, args.idle_timeout)
            except Exception:
                log.exception("Idle unloader failed.")


@asynccontextmanager
async def lifespan(_: FastAPI):
    log.info(
        "Starting (device=%s, max_inflight=%d, idle_timeout=%.0fs). "
        "Model loads lazily on first request.",
        args.device,
        args.max_inflight,
        args.idle_timeout,
    )
    task = asyncio.create_task(_idle_unloader())
    try:
        yield
    finally:
        task.cancel()
        EXECUTOR.shutdown(wait=False, cancel_futures=True)


app = FastAPI(lifespan=lifespan)


# ─── Schemas (unchanged contract) ────────────────────────────────────────────


class EmbedRequest(BaseModel):
    texts: List[str]


# ─── Endpoints ───────────────────────────────────────────────────────────────


@app.get("/health")
async def health() -> dict:
    """Liveness/readiness — never touches (or loads) the model."""
    return {
        "status": "ok",
        "model_loaded": MANAGER.is_loaded(),
        "inflight": _inflight,
        "max_inflight": args.max_inflight,
    }


@app.get("/stats", response_class=ORJSONResponse)
async def stats() -> dict:
    """Throughput counters for the perf harness. ``config`` echoes the launch flags
    (so a benchmark row records the embedder config without the operator retyping
    it); ``runtime`` is the rolling tally since the last ``POST /stats/reset``."""
    return {
        "config": {
            "batch": args.batch,
            "max_inflight": args.max_inflight,
            "maxlen": args.maxlen,
        },
        "runtime": STATS.snapshot(),
        "inflight": _inflight,
    }


@app.post("/stats/reset")
async def stats_reset() -> dict:
    """Zero the rolling runtime counters (config echo is unaffected). The harness
    calls this before each measured run so every result is clean."""
    STATS.reset()
    return {"status": "ok"}


@app.post("/encode")
async def encode(req: EmbedRequest) -> Response:
    with capacity_slot():
        try:
            t0 = time.monotonic()
            blob = await on_gpu(MANAGER.encode, req.texts)
            STATS.record_encode(len(req.texts), args.batch, time.monotonic() - t0)
            return Response(content=blob, media_type="application/octet-stream")
        except HTTPException:
            raise
        except Exception as e:
            log.exception("Encode failed; model dropped for reload.")
            raise HTTPException(status_code=503, detail="Inference failed.") from e


if __name__ == "__main__":
    uvicorn.run(app, host="0.0.0.0", port=args.port)
