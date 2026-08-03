"""The two agent-facing discovery documents, checked on the wire.

`/llms.txt` and `/.well-known/mindex.json` are how a client that was handed
nothing but a URL learns what this server is. Unit tests pin their contents; what
only a live server can show is that they are actually routed, carry the right
content type, and that the endpoint inventory the descriptor advertises matches
the spec the same server serves — a path escaped differently by axum than by
utoipa would pass every in-process test and still hand callers a 404.
"""

import httpx
from conftest import MINDEX_URL


def test_descriptor_is_served_as_json(client: httpx.Client) -> None:
    r = client.get(f"{MINDEX_URL}/.well-known/mindex.json")
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("application/json")

    body = r.json()
    assert body["service"] == "mindex"
    assert body["version"]
    assert body["descriptor_version"] >= 1
    # Explicitly null rather than absent: "authenticates nothing" and "too old to
    # say" must not look the same to a client.
    assert "authentication" in body
    assert body["authentication"] is None
    assert body["transport"]["tls"] is True
    assert "h2" in body["transport"]["alpn"]


def test_descriptor_inlines_the_config_endpoints_snapshot(client: httpx.Client) -> None:
    """One request is enough to bootstrap: the descriptor carries /config's body."""
    descriptor = client.get(f"{MINDEX_URL}/.well-known/mindex.json").json()
    config = client.get(f"{MINDEX_URL}/config").json()

    # `research.models`/`research.observed` are worker-refreshed and may legitimately
    # differ between two calls, so the stable fields are what this compares.
    assert descriptor["config"]["version"] == config["version"]
    assert descriptor["config"]["languages"] == config["languages"]
    assert descriptor["config"]["search"] == config["search"]
    assert descriptor["version"] == config["version"]


def test_every_documented_endpoint_exists_in_the_served_spec(
    client: httpx.Client,
) -> None:
    descriptor = client.get(f"{MINDEX_URL}/.well-known/mindex.json").json()

    spec_url = descriptor["documents"]["openapi"]
    spec = client.get(f"{MINDEX_URL}{spec_url}")
    assert spec.status_code == 200
    paths = spec.json()["paths"]

    documented = [e for e in descriptor["endpoints"] if e["documented"]]
    assert documented, "the descriptor advertised no documented endpoint at all"
    for e in documented:
        assert e["path"] in paths, f"{e['path']} is advertised but absent from the spec"
        assert e["method"].lower() in paths[e["path"]]
        assert e["summary"]


def test_undocumented_endpoints_are_routed_too(client: httpx.Client) -> None:
    """`documented: false` means "no JSON contract", never "not served"."""
    descriptor = client.get(f"{MINDEX_URL}/.well-known/mindex.json").json()

    undocumented = [e for e in descriptor["endpoints"] if not e["documented"]]
    assert undocumented
    for e in undocumented:
        path = e["path"]
        r = client.get(f"{MINDEX_URL}{path}", follow_redirects=True)
        assert r.status_code == 200, f"{path} is advertised, answers {r.status_code}"


def test_narrative_is_served_as_markdown(client: httpx.Client) -> None:
    r = client.get(f"{MINDEX_URL}/llms.txt")
    assert r.status_code == 200
    assert r.headers["content-type"] == "text/markdown; charset=utf-8"
    assert r.text.startswith("# mindex")
    # The live section is generated per request; its absence would mean the two
    # halves of the document stopped being assembled together.
    assert "## Live configuration" in r.text
