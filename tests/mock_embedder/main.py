"""
Deterministic OpenAI-compatible embedder mock (what vLLM serves in production).

Returns vectors that are seeded by the input text so that identical texts always
produce identical vectors, and different texts produce different vectors.
This is sufficient for asserting that indexed content is found in search and
that re-indexed (changed) content replaces the old content.

The surface mirrors what mindex's client actually calls:
  GET  /health         — liveness (vLLM serves one too)
  GET  /v1/models      — the identity handshake; must name MOCK_MODEL or the
                         server refuses to start / /health reports the embedder
                         as failing
  POST /v1/embeddings  — {"model", "input", "encoding_format"} in, the OpenAI
                         embeddings list out, one MOCK_DIM-wide row per input

MOCK_MODEL and MOCK_DIM come from the environment so the compose file can pin
them to whatever registry entry the test config names.
"""

import asyncio
import hashlib
import math
import os
import random
from typing import Any

# fastapi is only present inside this component's Docker image, never alongside
# the local/CI mypy run, so its stubs are legitimately unresolvable here.
from fastapi import FastAPI, HTTPException  # type: ignore[import-not-found]

app = FastAPI()

# What /v1/models reports and what the request's "model" field is expected to
# carry — the served name of the registry's default entry unless overridden.
MOCK_MODEL = os.environ.get("MOCK_MODEL", "Qwen/Qwen3-Embedding-0.6B")
# The width of every returned vector. Must match the registry dim of the model
# the test config names, or the client (correctly) refuses every row.
MOCK_DIM = int(os.environ.get("MOCK_DIM", "1024"))

# Per-process test knobs, set via POST /config. Defaults leave the rest of the suite
# unaffected.
#   encode_delay_secs  — artificial delay injected into every embed call, to widen
#                        the window a file stays 'indexing' so a request can be
#                        caught mid-flight (POST /cancel, concurrent /index
#                        collisions).
#   fail_next_encodes  — number of subsequent embed calls to fail with HTTP 500
#                        before serving normally again; lets a test drive a file to
#                        'failed' (embed failure) and then watch it recover. 500,
#                        not 503: the client treats 429/503 as "busy" and retries
#                        them, so an injected 503 would be quietly absorbed.
_config: dict[str, float] = {"encode_delay_secs": 0.0, "fail_next_encodes": 0.0}


def _dense(text: str) -> list[float]:
    rng = random.Random(hashlib.md5(text.encode()).hexdigest())
    vec = [rng.gauss(0.0, 1.0) for _ in range(MOCK_DIM)]
    norm = math.sqrt(sum(x * x for x in vec)) or 1.0
    return [x / norm for x in vec]


@app.get("/health")
async def health() -> dict[str, str]:
    return {"status": "ok"}


@app.get("/v1/models")
async def models() -> dict[str, Any]:
    return {
        "object": "list",
        "data": [{"id": MOCK_MODEL, "object": "model", "owned_by": "mock"}],
    }


@app.post("/config")
async def config(payload: dict[str, Any]) -> dict[str, float]:
    """Test-only knobs: ``encode_delay_secs`` slows every embed call;
    ``fail_next_encodes`` fails that many subsequent calls with 500."""
    if "encode_delay_secs" in payload:
        _config["encode_delay_secs"] = float(payload["encode_delay_secs"])
    if "fail_next_encodes" in payload:
        _config["fail_next_encodes"] = float(payload["fail_next_encodes"])
    return dict(_config)


@app.post("/v1/embeddings")
async def embeddings(payload: dict[str, Any]) -> dict[str, Any]:
    delay = _config["encode_delay_secs"]
    if delay > 0:
        await asyncio.sleep(delay)
    # Inject an embed failure (500) so a test can drive a file to 'failed'. The delay
    # above runs first so the file is observably 'indexing' before the failure lands.
    if _config["fail_next_encodes"] > 0:
        _config["fail_next_encodes"] -= 1
        raise HTTPException(status_code=500, detail="injected embed failure")
    texts = payload["input"]
    if isinstance(texts, str):
        texts = [texts]
    return {
        "object": "list",
        "model": payload.get("model", MOCK_MODEL),
        "data": [
            {"object": "embedding", "index": i, "embedding": _dense(t)}
            for i, t in enumerate(texts)
        ],
        "usage": {"prompt_tokens": 0, "total_tokens": 0},
    }
