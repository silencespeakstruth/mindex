"""A project's whole life under a token, and the mistakes people actually make.

The tests above are about rules. These are about the sequence a person goes
through — create, index, search, delete, recreate — and about what the server
says when they get it wrong in one of the four or five ways that are easy to get
wrong. Each one is here because the *answer* to the mistake is load-bearing: with
an out-of-scope project answering 404 by design, several distinct errors arrive
looking identical, and the ones that do not must be worth telling apart.
"""

import uuid

import pytest
from conftest import AuthClient

# `/index` is keyed language -> path -> body, not a flat list.
FILE_PATH = "src/lib.rs"
OTHER_PATH = "src/two.rs"
FILES = {"files": {"rust": {FILE_PATH: {"code": "fn a() -> u32 { 1 }\n"}}}}
OTHER_FILES = {"files": {"rust": {OTHER_PATH: {"code": "fn b() -> u32 { 2 }\n"}}}}


# ── Creating a project ───────────────────────────────────────────────────────


def test_only_a_token_naming_a_guid_can_bring_that_project_into_existence(
    auth: AuthClient,
) -> None:
    """The enumeration oracle the stateless design closes.

    A `tenant_id` column would have let anyone create any GUID and then own it,
    making `POST /index` a way to ask "does this project exist?" — it answers
    differently for a GUID somebody else already holds. Here the token has to
    name the GUID first, so the question cannot be put.
    """
    guid = uuid.uuid4().hex
    stranger = auth.token_for([uuid.uuid4().hex], ["index"])
    r = auth.post(f"/v0/{guid}/index", stranger, json=FILES)
    assert r.status_code == 404 and r.json()["code"] == "project.not_found", r.text

    # And the project genuinely does not exist afterwards — the refusal did not
    # create it on the way past.
    owner = auth.token_for([guid], ["search"])
    assert auth.get(f"/projects/{guid}", owner).status_code == 404

    named = auth.token_for([guid], ["index"])
    assert auth.post(f"/v0/{guid}/index", named, json=FILES).status_code == 200
    assert auth.get(f"/projects/{guid}", owner).status_code == 200


def test_a_freshly_generated_guid_answers_exactly_like_a_forbidden_one(
    auth: AuthClient,
) -> None:
    """Why the VS Code extension warns locally when it writes a new `.mindex`.

    A GUID nobody has indexed and a GUID this token may not reach are the same
    404, so no client can distinguish them from the response. This test is what
    makes that a *pinned* property rather than an accident — if it ever stopped
    being true, the extension's warning would become the wrong explanation.
    """
    unknown = uuid.uuid4().hex
    forbidden = uuid.uuid4().hex
    owner = auth.token_for([forbidden], ["index"])
    assert auth.post(f"/v0/{forbidden}/index", owner, json=FILES).status_code == 200

    stranger = auth.token_for([uuid.uuid4().hex], ["search"])
    a = auth.post(f"/v0/{unknown}/search", stranger, json={"query": "x"})
    b = auth.post(f"/v0/{forbidden}/search", stranger, json={"query": "x"})
    assert a.status_code == b.status_code == 404
    assert a.text.replace(unknown, "G") == b.text.replace(forbidden, "G")


# ── Deleting and recreating ──────────────────────────────────────────────────


def test_a_project_can_be_deleted_and_rebuilt_under_the_same_token(
    auth: AuthClient,
) -> None:
    """The token is the mapping and there is no stored ownership, so a hard
    delete removes the project's rows and leaves the credential untouched.

    The half worth pinning is what happens *after*: the same token recreates the
    same GUID, which is only true because nothing about the deletion revoked the
    grant. A design that stored ownership would have had to decide whether
    deleting a project also released its name, and either answer is a bug.
    """
    guid = uuid.uuid4().hex
    token = auth.token_for([guid], ["index", "search", "delete"])

    assert auth.post(f"/v0/{guid}/index", token, json=FILES).status_code == 200
    assert auth.get(f"/projects/{guid}", token).status_code == 200

    r = auth.delete(f"/projects/{guid}", token)
    assert r.status_code == 204, r.text
    assert auth.get(f"/projects/{guid}", token).status_code == 404

    # Idempotent: a retry of the delete must not become an error, since the
    # collection drop is last and a client that lost the response retries.
    assert auth.delete(f"/projects/{guid}", token).status_code == 204

    # And rebuilt, by the same credential.
    assert auth.post(f"/v0/{guid}/index", token, json=OTHER_FILES).status_code == 200
    files = auth.get(f"/projects/{guid}/files", token).json()["files"]
    assert [f["path"] for f in files] == [OTHER_PATH], files


def test_deleting_a_project_needs_delete_and_not_merely_index(
    auth: AuthClient,
) -> None:
    """`index` and `delete` are separate actions precisely so a token that keeps
    an index current cannot destroy it. A write token is the one most likely to
    be handed out casually."""
    guid = uuid.uuid4().hex
    writer = auth.token_for([guid], ["index", "search"])
    assert auth.post(f"/v0/{guid}/index", writer, json=FILES).status_code == 200
    assert auth.delete(f"/projects/{guid}", writer).status_code == 403
    assert auth.get(f"/projects/{guid}", writer).status_code == 200


# ── The mistakes ─────────────────────────────────────────────────────────────


def test_how_a_guid_was_spelled_never_decides_access(auth: AuthClient) -> None:
    """A user pasting the hyphenated GUID out of `.mindex` into a mint request,
    then using the dashless one in a URL, must not be told the project does not
    exist. The two spellings address one project everywhere else in the server."""
    guid = uuid.uuid4().hex
    hyphenated = str(uuid.UUID(guid))

    token = auth.token_for([hyphenated], ["index", "search"])
    assert auth.post(f"/v0/{guid}/index", token, json=FILES).status_code == 200
    assert auth.get(f"/projects/{hyphenated}", token).status_code == 200
    assert auth.get(f"/projects/{guid}", token).status_code == 200

    # And the other way round: minted dashless, used hyphenated.
    other = uuid.uuid4().hex
    token = auth.token_for([other], ["index", "search"])
    assert (
        auth.post(f"/v0/{uuid.UUID(other)!s}/index", token, json=FILES).status_code
        == 200
    )


def test_the_scheme_is_read_case_insensitively_and_nothing_else_is(
    auth: AuthClient, project: str
) -> None:
    """RFC 7235 says the scheme is case-insensitive, and clients spell it three
    ways. Everything after it is the credential and is compared exactly."""
    token = auth.token_for([project], ["search"])
    for scheme in ("Bearer", "bearer", "BEARER"):
        r = auth.post(
            f"/v0/{project}/search",
            json={"query": "x"},
            headers={"Authorization": f"{scheme} {token}"},
        )
        assert r.status_code != 401, f"{scheme} was rejected: {r.text}"

    for header in (token, f"Token {token}", f"Basic {token}", "Bearer"):
        r = auth.post(
            f"/v0/{project}/search",
            json={"query": "x"},
            headers={"Authorization": header},
        )
        assert r.status_code == 401, f"{header[:16]!r} was accepted"


def test_extra_space_between_the_scheme_and_a_pasted_token_is_tolerated(
    auth: AuthClient, project: str
) -> None:
    """A token pasted by hand arrives with stray spacing more often than not, and
    "your credential is invalid" is the least helpful true thing a server can say
    about one.

    Only the space *between* scheme and token is exercised: a value with trailing
    whitespace is illegal in HTTP and httpx refuses to send it, so the trailing
    newline case never reaches a server at all — it fails in the client, which is
    the right place and not something this server can improve on.
    """
    token = auth.token_for([project], ["search"])
    r = auth.post(
        f"/v0/{project}/search",
        json={"query": "x"},
        headers={"Authorization": f"Bearer   {token}"},
    )
    assert r.status_code != 401, r.text


@pytest.mark.parametrize(
    "path",
    [
        "/v0/{}/search",
        "/projects/{}",
        "/projects/{}/files",
        "/projects/{}/research",
    ],
)
def test_a_valid_token_and_a_malformed_guid_is_a_request_error_not_a_scope_one(
    auth: AuthClient, path: str
) -> None:
    """A typo in a GUID must not be reported as a scope decision.

    It is the mistake people make most, and reading `project.not_found` for it
    sends them to re-mint a token when the real fix is a corrected path. The
    extractors run in the order that makes this work — the path is parsed before
    the token is asked whether it covers what the path said.
    """
    token = auth.token_for(["*"], ["search", "research"])
    r = auth.request(
        "GET" if not path.startswith("/v0") else "POST",
        path.format("not-a-guid"),
        token,
        json={"query": "x"},
    )
    assert r.status_code == 400, f"{path}: {r.status_code} {r.text[:200]}"
    assert r.json()["code"] == "request.malformed_path", r.text


def test_a_token_for_the_wrong_server_does_not_verify(
    auth: AuthClient, project: str
) -> None:
    """Two deployments have two signing keys, so the credential for one is simply
    an unusable string at the other. The tokens are indistinguishable by eye,
    which is why this is worth a test rather than a comment: the answer must be a
    clean 401 rather than anything that reads as a scope or action problem."""
    # A token from the *unauthorized* stack cannot exist, so the nearest real
    # case is a token whose signature came from a different key: re-sign is
    # impossible from here, so this reuses the malformed path with a payload that
    # is otherwise perfectly well-formed.
    import base64
    import json

    header = base64.urlsafe_b64encode(
        json.dumps({"alg": "HS256", "typ": "JWT", "kid": "default"}).encode()
    ).rstrip(b"=")
    payload = base64.urlsafe_b64encode(
        json.dumps(
            {
                "iss": "mindex",
                "sub": "elsewhere",
                "jti": "j",
                "iat": 0,
                "nbf": 0,
                "prj": [project],
                "act": ["search"],
            }
        ).encode()
    ).rstrip(b"=")
    forged = f"{header.decode()}.{payload.decode()}.{base64.urlsafe_b64encode(b'x' * 32).rstrip(b'=').decode()}"

    r = auth.post(f"/v0/{project}/search", forged, json={"query": "x"})
    assert r.status_code == 401 and r.json()["code"] == "auth.token_invalid", r.text


def test_an_unknown_key_id_is_the_same_answer_as_a_bad_signature(
    auth: AuthClient, project: str
) -> None:
    """Deleting a key id is the revocation mechanism, so a token signed under a
    deleted one arrives here. It must not be distinguishable from a forgery:
    "that key does not exist" tells an unauthenticated caller what is in the key
    file."""
    import base64
    import json

    header = base64.urlsafe_b64encode(
        json.dumps({"alg": "HS256", "typ": "JWT", "kid": "no-such-key"}).encode()
    ).rstrip(b"=")
    r = auth.post(
        f"/v0/{project}/search",
        f"{header.decode()}.eyJpc3MiOiJtaW5kZXgifQ.c2ln",
        json={"query": "x"},
    )
    assert r.status_code == 401 and r.json()["code"] == "auth.token_invalid", r.text
