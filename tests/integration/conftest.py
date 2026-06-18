import os
import time
import uuid
from collections.abc import Callable, Iterator

import httpx
import pytest

MINDEX_URL = os.environ.get("MINDEX_URL", "https://localhost:11111")
MOCK_EMBEDDER_URL = os.environ.get("MOCK_EMBEDDER_URL", "http://localhost:11211")
STARTUP_TIMEOUT = 120  # seconds


@pytest.fixture(scope="session", autouse=True)
def wait_for_mindex() -> None:
    """Block until mindex accepts connections (any HTTP response counts)."""
    deadline = time.monotonic() + STARTUP_TIMEOUT
    last_exc: Exception | None = None

    while time.monotonic() < deadline:
        try:
            # Any route — we just want a TCP connection + TLS handshake.
            httpx.post(
                f"{MINDEX_URL}/v0/{'0' * 32}/search",
                json={"query": "warmup"},
                verify=False,
                timeout=3.0,
            )
            return
        except Exception as exc:
            last_exc = exc
            time.sleep(1)

    raise RuntimeError(
        f"mindex did not become ready within {STARTUP_TIMEOUT}s: {last_exc}"
    )


@pytest.fixture
def client() -> Iterator[httpx.Client]:
    with httpx.Client(verify=False, timeout=30.0) as c:
        yield c


@pytest.fixture
def project(client: httpx.Client) -> str:
    """Return a fresh project GUID (32-char hex, no hyphens) for each test."""
    return uuid.uuid4().hex


@pytest.fixture
def embed_delay() -> Iterator[Callable[[float], None]]:
    """Set the mock embedder's per-/encode delay, always resetting it to 0 after.

    Lets a test widen the window a file stays 'indexing' so an /index request can be
    caught in-flight. Yields a setter ``set(secs)``.
    """

    def set_delay(secs: float) -> None:
        httpx.post(
            f"{MOCK_EMBEDDER_URL}/config",
            json={"encode_delay_secs": secs},
            timeout=5.0,
        ).raise_for_status()

    try:
        yield set_delay
    finally:
        set_delay(0.0)
