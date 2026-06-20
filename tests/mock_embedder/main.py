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
from fastapi import FastAPI, HTTPException  # type: ignore[import-not-found]

app = FastAPI()

DENSE_DIM = 1024
MAX_SPARSE_TOKENS = 16
MAX_COLBERT_TOKENS = 8

# Per-process test knobs, set via POST /config. Defaults leave the rest of the suite
# unaffected.
#   encode_delay_secs  — artificial delay injected into every /encode, to widen the
#                        window a file stays 'indexing' so a request can be caught
#                        mid-flight (POST /cancel, concurrent /index collisions).
#   fail_next_encodes  — number of subsequent /encode calls to fail with HTTP 503
#                        before serving normally again; lets a test drive a file to
#                        'failed' (embed failure) and then watch it recover.
_config: dict[str, float] = {"encode_delay_secs": 0.0, "fail_next_encodes": 0.0}


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
    """Test-only knobs: ``encode_delay_secs`` slows every /encode;
    ``fail_next_encodes`` fails that many subsequent /encode calls with 503."""
    if "encode_delay_secs" in payload:
        _config["encode_delay_secs"] = float(payload["encode_delay_secs"])
    if "fail_next_encodes" in payload:
        _config["fail_next_encodes"] = float(payload["fail_next_encodes"])
    return dict(_config)


@app.post("/encode")
async def encode(payload: dict[str, Any]) -> dict[str, Any]:
    delay = _config["encode_delay_secs"]
    if delay > 0:
        await asyncio.sleep(delay)
    # Inject an embed failure (503) so a test can drive a file to 'failed'. The delay
    # above runs first so the file is observably 'indexing' before the failure lands.
    if _config["fail_next_encodes"] > 0:
        _config["fail_next_encodes"] -= 1
        raise HTTPException(status_code=503, detail="injected embed failure")
    texts: list[str] = payload["texts"]
    return {
        "dense_vecs": [_dense(t) for t in texts],
        "sparse_vecs": [_sparse(t) for t in texts],
        "colbert_vecs": [_colbert(t) for t in texts],
    }
