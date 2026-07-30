"""`GET /metrics` — the Prometheus exposition endpoint.

The unit tests already pin the metric name/type contract and the middleware's
labelling. What only an end-to-end run can show is that the endpoint is actually
routed, serves the OpenMetrics content type the scraper needs, and that real
traffic through the real router moves the counters.
"""

import httpx
from conftest import MINDEX_URL


def _metrics(client: httpx.Client) -> str:
    resp = client.get(f"{MINDEX_URL}/metrics")
    assert resp.status_code == 200
    # Not `text/plain`: the body carries an OpenMetrics `# EOF` terminator, which
    # a strict parser rejects under the wrong content type.
    assert resp.headers["content-type"].startswith("application/openmetrics-text")
    return resp.text


def test_metrics_endpoint_serves_openmetrics(client: httpx.Client) -> None:
    body = _metrics(client)
    assert body.endswith("# EOF\n")
    assert "mindex_build_info{" in body
    assert "# TYPE mindex_http_requests counter" in body


def test_a_request_is_counted_under_its_route_template(client: httpx.Client) -> None:
    client.get(f"{MINDEX_URL}/version")
    body = _metrics(client)
    assert (
        'mindex_http_requests_total{route="/version",method="GET",status="200"' in body
    )
    # The template, not the concrete path — a per-project series here would grow
    # the family with the project count.
    assert 'route="/v0/{project_guid}/search"' in body or "/v0/" not in body


def test_an_error_carries_its_stable_code(client: httpx.Client, project: str) -> None:
    # `top_k` above the configured cap is a validation 400 with a stable code.
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/search",
        json={"query": "anything", "top_k": 10_000_000},
    )
    assert resp.status_code == 400
    code = resp.json()["code"]

    body = _metrics(client)
    labelled = f'code="{code}"' in body
    assert labelled, f"the {code} response was not labelled with its code"


def test_indexing_moves_the_domain_counters(client: httpx.Client, project: str) -> None:
    resp = client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        json={"files": {"python": {"m.py": {"code": "def f():\n    return 1\n"}}}},
    )
    assert resp.status_code == 200

    body = _metrics(client)
    assert f'mindex_index_files_total{{project_guid="{project}"' in body
    assert 'language="python"' in body
    # The size distribution is language-labelled but never project-labelled.
    assert 'mindex_index_file_size_bytes_count{language="python"}' in body


def test_the_state_collector_reports_the_project(
    client: httpx.Client, project: str
) -> None:
    client.post(
        f"{MINDEX_URL}/v0/{project}/index",
        json={"files": {"python": {"s.py": {"code": "x = 1\n"}}}},
    )
    # The collector runs on its own interval; the test config keeps it short.
    import time

    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        body = _metrics(client)
        if f'mindex_project_files{{project_guid="{project}"' in body:
            return
        time.sleep(1)

    raise AssertionError(f"the state collector never reported project {project}")
