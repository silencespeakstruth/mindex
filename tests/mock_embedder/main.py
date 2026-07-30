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
import struct
from typing import Any

# fastapi is only present inside this component's Docker image, never alongside
# the local/CI mypy run, so its stubs are legitimately unresolvable here.
from fastapi import FastAPI, HTTPException, Response  # type: ignore[import-not-found]

app = FastAPI()

# Binary /encode wire format, mirroring embedder/src/bge_m3_api/__main__.py and the
# Rust parser in src/models/bge_m3.rs — keep all three in lockstep. Little-endian:
#   magic b"BM3\x01" | n u32 | dim u32 | dense n*dim f32 |
#   sparse per chunk: count u32, count*u32 ids, count*f32 weights |
#   colbert per chunk: tokens u32, tokens*dim f32
_MAGIC = b"BM3\x01"

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


def _pack(
    dense: list[list[float]],
    sparse: list[dict[int, float]],
    colbert: list[list[list[float]]],
) -> bytes:
    parts: list[bytes] = [_MAGIC, struct.pack("<II", len(dense), DENSE_DIM)]
    for row in dense:
        parts.append(struct.pack(f"<{len(row)}f", *row))
    for d in sparse:
        items = list(d.items())
        parts.append(struct.pack("<I", len(items)))
        parts.append(struct.pack(f"<{len(items)}I", *(k for k, _ in items)))
        parts.append(struct.pack(f"<{len(items)}f", *(v for _, v in items)))
    for chunk in colbert:
        parts.append(struct.pack("<I", len(chunk)))
        for tok in chunk:
            parts.append(struct.pack(f"<{len(tok)}f", *tok))
    return b"".join(parts)


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
async def encode(payload: dict[str, Any]) -> Response:
    delay = _config["encode_delay_secs"]
    if delay > 0:
        await asyncio.sleep(delay)
    # Inject an embed failure (503) so a test can drive a file to 'failed'. The delay
    # above runs first so the file is observably 'indexing' before the failure lands.
    if _config["fail_next_encodes"] > 0:
        _config["fail_next_encodes"] -= 1
        raise HTTPException(status_code=503, detail="injected embed failure")
    texts: list[str] = payload["texts"]
    blob = _pack(
        [_dense(t) for t in texts],
        [_sparse(t) for t in texts],
        [_colbert(t) for t in texts],
    )
    return Response(content=blob, media_type="application/octet-stream")
