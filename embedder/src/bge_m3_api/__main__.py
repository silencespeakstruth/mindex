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
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import asynccontextmanager, contextmanager
from typing import Any, Callable, Iterator, List

import torch
import uvicorn
from fastapi import FastAPI, HTTPException
from fastapi.responses import ORJSONResponse
from pydantic import BaseModel
from FlagEmbedding import BGEM3FlagModel

logging.basicConfig(
    level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
)
log = logging.getLogger("bge_m3_api")

MODEL_NAME = "BAAI/bge-m3"

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

    def encode(self, texts: List[str]) -> dict:
        def work(model: BGEM3FlagModel) -> dict:
            res = model.encode(
                texts,
                batch_size=self._batch,
                max_length=self._maxlen,
                return_dense=True,
                return_sparse=True,
                return_colbert_vecs=True,
            )
            return {
                "dense_vecs": [v.tolist() for v in res["dense_vecs"]],
                "sparse_vecs": [
                    {k: float(v) for k, v in d.items()} for d in res["lexical_weights"]
                ],
                "colbert_vecs": [v.tolist() for v in res["colbert_vecs"]],
            }

        return self._run(work)


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
        raise HTTPException(
            status_code=429,
            detail=f"Resource busy: {args.max_inflight} requests already in flight.",
        )
    _inflight += 1
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


@app.post("/encode", response_class=ORJSONResponse)
async def encode(req: EmbedRequest) -> dict:
    with capacity_slot():
        try:
            return await on_gpu(MANAGER.encode, req.texts)
        except HTTPException:
            raise
        except Exception as e:
            log.exception("Encode failed; model dropped for reload.")
            raise HTTPException(status_code=503, detail="Inference failed.") from e


if __name__ == "__main__":
    uvicorn.run(app, host="0.0.0.0", port=args.port)
