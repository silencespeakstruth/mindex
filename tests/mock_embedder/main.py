"""
Deterministic BGE-M3 mock.

Returns vectors that are seeded by the input text so that identical texts always
produce identical vectors, and different texts produce different vectors.
This is sufficient for asserting that indexed content is found in search and
that re-indexed (changed) content replaces the old content.
"""
import hashlib
import math
import random
from typing import Any

from fastapi import FastAPI

app = FastAPI()

DENSE_DIM = 1024
MAX_SPARSE_TOKENS = 16
MAX_COLBERT_TOKENS = 8


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


@app.post("/encode")
async def encode(payload: dict[str, Any]) -> dict[str, Any]:
    texts: list[str] = payload["texts"]
    return {
        "dense_vecs":   [_dense(t) for t in texts],
        "sparse_vecs":  [_sparse(t) for t in texts],
        "colbert_vecs": [_colbert(t) for t in texts],
    }
