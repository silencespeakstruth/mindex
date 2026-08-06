import os
import time
import uuid
from collections.abc import Callable, Iterator
from typing import Any

import httpx
import pytest

MINDEX_URL = os.environ.get("MINDEX_URL", "https://localhost:11111")
MOCK_EMBEDDER_URL = os.environ.get("MOCK_EMBEDDER_URL", "http://localhost:11211")
STARTUP_TIMEOUT = 120  # seconds

# The second stack: a mindex with [auth].enabled. Absent when the suite is run
# against a single server, which is why every fixture below skips rather than
# fails — an auth test that cannot reach an authorized server has established
# nothing, and reporting that as a failure would train people to ignore it.
MINDEX_AUTH_URL = os.environ.get("MINDEX_AUTH_URL", "")
ROOT_TOKEN_FILE = os.environ.get("MINDEX_ROOT_TOKEN_FILE", "")
REVOCABLE_TOKEN_FILE = os.environ.get("MINDEX_REVOCABLE_TOKEN_FILE", "")


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
        # A readiness poll must survive *anything* the not-yet-listening server
        # throws: connection refused, a TLS handshake against a half-written cert, a
        # truncated read. Narrowing this turns a slow start into a crash.
        except Exception as exc:  # noqa: BLE001
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
    """Set the mock embedder's per-embed-call delay, always resetting it to 0 after.

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


@pytest.fixture
def embed_fail() -> Iterator[Callable[[int], None]]:
    """Make the next ``n`` embed calls fail with 503, always resetting to 0 after.

    Lets a test drive a file to 'failed' (embed failure) and then observe recovery
    (reindex, or the retry worker). Yields a setter ``fail(n)``.
    """

    def set_fail(n: int) -> None:
        httpx.post(
            f"{MOCK_EMBEDDER_URL}/config",
            json={"fail_next_encodes": n},
            timeout=5.0,
        ).raise_for_status()

    try:
        yield set_fail
    finally:
        set_fail(0)


# ── The authorized stack ─────────────────────────────────────────────────────


def _read_token(path: str, what: str) -> str:
    """Read a token the bootstrap container minted, or skip the test.

    Skipping rather than failing: these files come from a service that only
    exists in the full compose stack, and a missing one means the suite is being
    run against a plain server rather than that something is broken.
    """
    if path == "":
        pytest.skip(f"no {what} token: MINDEX_*_TOKEN_FILE is unset")
    try:
        with open(path, encoding="utf-8") as fh:
            token = fh.read().strip()
    except OSError as exc:
        pytest.skip(f"cannot read the {what} token at {path}: {exc}")
    if token == "":
        pytest.skip(f"the {what} token at {path} is empty")
    return token


@pytest.fixture(scope="session")
def auth_url() -> str:
    if MINDEX_AUTH_URL == "":
        pytest.skip("no authorized server: MINDEX_AUTH_URL is unset")
    return MINDEX_AUTH_URL


@pytest.fixture(scope="session")
def root_token(wait_for_auth_server: None) -> str:
    """Wildcard, every action. The minter — never a thing under test itself.

    Depends on the readiness poll explicitly rather than relying on autouse
    ordering: the server mints these files before it starts listening, so
    "health answered" is exactly the signal that they exist.
    """
    return _read_token(ROOT_TOKEN_FILE, "root")


@pytest.fixture(scope="session")
def revocable_token(wait_for_auth_server: None) -> str:
    """Signed under its own `kid`, so deleting that key withdraws it alone."""
    return _read_token(REVOCABLE_TOKEN_FILE, "revocable")


@pytest.fixture(scope="session", autouse=True)
def wait_for_auth_server() -> None:
    """Same readiness poll as above, for the authorized server if there is one.

    Not skipped when absent: this is autouse, and skipping here would skip every
    test in the session rather than only the ones that need it.
    """
    if MINDEX_AUTH_URL == "":
        return
    deadline = time.monotonic() + STARTUP_TIMEOUT
    last_exc: Exception | None = None
    while time.monotonic() < deadline:
        try:
            # `/health` is public by design, so it answers before any credential
            # exists — which is exactly what makes it usable as a readiness probe
            # on a server that authorizes everything else.
            httpx.get(f"{MINDEX_AUTH_URL}/health", verify=False, timeout=3.0)
            return
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
            time.sleep(1)
    raise RuntimeError(
        f"the authorized mindex did not become ready within {STARTUP_TIMEOUT}s: {last_exc}"
    )


class AuthClient:
    """An httpx client against the authorized server, plus a minting helper.

    Deliberately thin. The tests are about what the *server* refuses, so every
    request goes out as written — no retry, no status check, no header this class
    decided the caller wanted.
    """

    def __init__(self, client: httpx.Client, base: str, root: str) -> None:
        self._c = client
        self.base = base
        self.root = root

    def request(
        self, method: str, path: str, token: str | None = None, **kw: Any
    ) -> httpx.Response:
        headers: dict[str, str] = dict(kw.pop("headers", None) or {})
        if token is not None:
            headers["Authorization"] = f"Bearer {token}"
        return self._c.request(method, f"{self.base}{path}", headers=headers, **kw)

    def get(self, path: str, token: str | None = None, **kw: Any) -> httpx.Response:
        return self.request("GET", path, token, **kw)

    def post(self, path: str, token: str | None = None, **kw: Any) -> httpx.Response:
        return self.request("POST", path, token, **kw)

    def delete(self, path: str, token: str | None = None, **kw: Any) -> httpx.Response:
        return self.request("DELETE", path, token, **kw)

    def mint(
        self,
        projects: list[str],
        actions: list[str],
        days: int = 1,
        sub: str = "test",
        audiences: list[str] | None = None,
        minter: str | None = None,
    ) -> httpx.Response:
        """`POST /auth/tokens`. Returns the raw response — refusals are the point."""
        body: dict[str, object] = {
            "sub": sub,
            "projects": projects,
            "actions": actions,
            "days": days,
        }
        if audiences is not None:
            body["audiences"] = audiences
        return self.post(
            "/auth/tokens", minter if minter is not None else self.root, json=body
        )

    def token_for(
        self,
        projects: list[str],
        actions: list[str],
        days: int = 1,
        audiences: list[str] | None = None,
        minter: str | None = None,
    ) -> str:
        """Mint and unwrap, asserting success — for setting a test's scene."""
        r = self.mint(projects, actions, days, audiences=audiences, minter=minter)
        if r.status_code != 200:
            raise AssertionError(
                f"minting {actions} for {projects} failed: {r.status_code} {r.text}"
            )
        return str(r.json()["token"])


@pytest.fixture
def auth(auth_url: str, root_token: str) -> Iterator[AuthClient]:
    with httpx.Client(verify=False, timeout=30.0) as c:
        yield AuthClient(c, auth_url, root_token)
