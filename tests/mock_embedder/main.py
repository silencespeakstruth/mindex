"""
Deterministic BGE-M3 mock.

Returns vectors that are seeded by the input text so that identical texts always
produce identical vectors, and different texts produce different vectors.
This is sufficient for asserting that indexed content is found in search and
that re-indexed (changed) content replaces the old content.
"""

import asyncio
import hashlib
import math
import random
from typing import Any

# fastapi is only present inside this component's Docker image, never alongside
# the local/CI mypy run, so its stubs are legitimately unresolvable here.
from fastapi import FastAPI  # type: ignore[import-not-found]

app = FastAPI()

DENSE_DIM = 1024
MAX_SPARSE_TOKENS = 16
MAX_COLBERT_TOKENS = 8

# Per-process artificial delay (seconds) injected into every /encode call. Tests set
# it via POST /config to widen the window a file stays 'indexing', so an /index
# request can be caught mid-flight (e.g. to exercise POST /cancel). Defaults to 0 so
# the rest of the suite is unaffected.
_config: dict[str, float] = {"encode_delay_secs": 0.0}


def _dense(text: str) -> list[float]:
    rng = random.Random(hashlib.md5(text.encode()).hexdigest())
    vec = [rng.gauss(0.0, 1.0) for _ in range(DENSE_DIM)]
    norm = math.sqrt(sum(x * x for x in vec)) or 1.0
    return [x / norm for x in vec]


def _sparse(text: str) -> dict[int, float]:
    words = text.split()[:MAX_SPARSE_TOKENS]
    out: dict[int, float] = {}
    for word in words:
        idx = int(hashlib.md5(word.encode()).hexdigest(), 16) % 30000
        out[idx] = max(out.get(idx, 0.0), 0.1 + len(word) * 0.01)
    return out


def _colbert(text: str) -> list[list[float]]:
    tokens = text.split()[:MAX_COLBERT_TOKENS] or [text]
    return [_dense(f"{text}\x00{i}\x00{tok}") for i, tok in enumerate(tokens)]


@app.get("/health")
async def health() -> dict[str, str]:
    return {"status": "ok"}


@app.post("/config")
async def config(payload: dict[str, Any]) -> dict[str, float]:
    """Test-only knob: set ``encode_delay_secs`` to slow every subsequent /encode."""
    if "encode_delay_secs" in payload:
        _config["encode_delay_secs"] = float(payload["encode_delay_secs"])
    return dict(_config)


@app.post("/encode")
async def encode(payload: dict[str, Any]) -> dict[str, Any]:
    delay = _config["encode_delay_secs"]
    if delay > 0:
        await asyncio.sleep(delay)
    texts: list[str] = payload["texts"]
    return {
        "dense_vecs": [_dense(t) for t in texts],
        "sparse_vecs": [_sparse(t) for t in texts],
        "colbert_vecs": [_colbert(t) for t in texts],
    }
