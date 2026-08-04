"""Authorization on the wire, against a server actually running with `[auth]`.

The unit tests establish the rules; these establish that the rules are what the
*deployment* enforces. Everything reachable only from a real stack lives here:
the default-deny layer in front of the real route table, the 404 that must be
byte-identical to a nonexistent project, and the whole minting flow — an agent
token derived over HTTP from another token, used, and found to be exactly as
narrow as it was asked to be.

Every test mints what it needs from the root token, rather than being handed a
pre-made one. That is deliberate: it means `POST /auth/tokens` is exercised
several dozen times as a side effect of setting scenes, and a containment bug
would surface as a token that works where it should not rather than as one
assertion in one test.
"""

import uuid

import httpx
import pytest
from conftest import MINDEX_URL, AuthClient

# `/index` is keyed language -> path -> body, not a flat list. One small file is
# enough everywhere here: these tests are about who may post it, never about what
# slicing does with it.
FILE_PATH = "src/lib.rs"
CODE = "fn a() -> u32 { 1 }\n"


def index_body(path: str = FILE_PATH, code: str = CODE) -> dict:
    return {"files": {"rust": {path: {"code": code}}}}


# ── The refusals, and what each one may say ──────────────────────────────────


def test_no_credential_is_its_own_refusal(auth: AuthClient, project: str) -> None:
    r = auth.post(f"/v0/{project}/search", json={"query": "x"})
    assert r.status_code == 401
    assert r.json()["code"] == "auth.token_missing"
    assert r.headers["content-type"].startswith("application/problem+json")


@pytest.mark.parametrize(
    "bad",
    [
        "not-a-token",
        "a.b",
        "a.b.c.d",
        # Three well-formed segments whose signature is nonsense.
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImRlZmF1bHQifQ.eyJpc3MiOiJtaW5kZXgifQ.x",
    ],
)
def test_an_unusable_token_is_refused_without_saying_which_part_failed(
    auth: AuthClient, project: str, bad: str
) -> None:
    """One code for every unusable token.

    Malformed, wrongly signed and signed-under-an-unknown-key are one answer on
    the wire on purpose. Telling them apart is a probe: "the signature failed"
    confirms the key id exists, which is a fact about the server's key file that
    an unauthenticated caller has no business establishing.
    """
    r = auth.post(f"/v0/{project}/search", bad, json={"query": "x"})
    assert r.status_code == 401
    assert r.json()["code"] == "auth.token_invalid"


def test_a_tampered_payload_does_not_verify(auth: AuthClient, project: str) -> None:
    """The signature is over the payload, and this is the thing it is for.

    Re-encoding a claim set with `prj: ["*"]` and keeping the original signature
    is the attack a JWT exists to refuse; a passing request here would mean the
    MAC was checked over something other than what was read.
    """
    import base64
    import json

    header, _, sig = auth.root.split(".")
    forged = base64.urlsafe_b64encode(
        json.dumps(
            {
                "iss": "mindex",
                "sub": "forged",
                "jti": "j",
                "iat": 0,
                "nbf": 0,
                "prj": ["*"],
                "act": ["admin"],
            }
        ).encode()
    ).rstrip(b"=")
    r = auth.get("/status", f"{header}.{forged.decode()}.{sig}")
    assert r.status_code == 401
    assert r.json()["code"] == "auth.token_invalid"


def test_a_credential_is_never_echoed_back(auth: AuthClient, project: str) -> None:
    """A refusal that quotes the token puts it in every log that keeps bodies."""
    token = auth.token_for([project], ["search"])
    for r in [
        auth.post(f"/v0/{project}/search", "rubbish.token.here", json={"query": "x"}),
        auth.get("/status", token),
        auth.post(
            f"/projects/{uuid.uuid4().hex}/files",
            token,
            json={"include": {"paths": ["**"]}},
        ),
    ]:
        assert token[:24] not in r.text and "rubbish.token" not in r.text, r.text


# ── The 404 that must not be an oracle ───────────────────────────────────────


def test_an_out_of_scope_project_is_byte_identical_to_one_that_never_existed(
    auth: AuthClient,
) -> None:
    """The load-bearing refusal, checked on the bytes rather than the status.

    A project GUID is a bearer identifier: anybody who learns one and holds a
    wildcard token has the project. So "you may not see this project" and "there
    is no such project" must be the same answer — and status alone would pass
    while `detail` quietly said which.

    The comparison is made from *one* token so that only the project varies. The
    first GUID exists and is out of scope; the second exists nowhere at all.
    """
    real = uuid.uuid4().hex
    never = uuid.uuid4().hex
    mine = uuid.uuid4().hex

    # Bring `real` into existence with a credential that reaches it.
    owner = auth.token_for([real], ["index"])
    assert auth.post(f"/v0/{real}/index", owner, json=index_body()).status_code == 200

    stranger = auth.token_for([mine], ["search", "research", "index", "delete"])
    for method, path in [
        ("POST", "/v0/{}/search"),
        ("GET", "/projects/{}"),
        ("GET", "/projects/{}/files"),
    ]:
        a = auth.request(method, path.format(real), stranger, json={"query": "x"})
        b = auth.request(method, path.format(never), stranger, json={"query": "x"})
        assert a.status_code == b.status_code == 404, f"{path}: {a.text} / {b.text}"
        # The GUID itself legitimately differs; nothing else may.
        assert a.text.replace(real, "G") == b.text.replace(never, "G"), (
            f"{path} distinguishes an out-of-scope project from a nonexistent one:\n"
            f"  {a.text}\n  {b.text}"
        )
        assert a.json()["code"] == "project.not_found"


def test_a_missing_action_is_named_because_the_project_was_already_proved(
    auth: AuthClient, project: str
) -> None:
    """The other half of the pair above, and the asymmetry is the design.

    Hiding the action would leave an under-scoped credential indistinguishable
    from a wrong one, and it protects nothing: the caller can read its own token.
    """
    read_only = auth.token_for([project], ["search"])
    r = auth.post(f"/v0/{project}/index", read_only, json=index_body())
    assert r.status_code == 403
    body = r.json()
    assert body["code"] == "auth.action_not_permitted"
    assert "index" in r.text


def test_the_two_refusals_stay_different_answers(auth: AuthClient) -> None:
    """A wrong project and a wrong action must not collapse into one code.

    They did not, and the check is here because the tempting simplification —
    "refuse everything the same way, it leaks less" — would make an operator
    unable to tell a mis-scoped token from a mis-actioned one, which is the
    commonest support question this feature will generate.
    """
    mine, theirs = uuid.uuid4().hex, uuid.uuid4().hex
    token = auth.token_for([mine], ["search"])
    assert (
        auth.post(f"/v0/{theirs}/search", token, json={"query": "x"}).status_code == 404
    )
    assert auth.post(f"/v0/{mine}/index", token, json=index_body()).status_code == 403


# ── Public routes ────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "path", ["/health", "/version", "/config", "/llms.txt", "/.well-known/mindex.json"]
)
def test_the_public_routes_answer_without_a_credential(
    auth: AuthClient, path: str
) -> None:
    """Liveness must not report the credential's health, and a discovery document
    that tells a caller it needs a credential cannot itself require one."""
    r = auth.get(path)
    assert r.status_code == 200, f"{path}: {r.status_code} {r.text[:200]}"


def test_the_descriptor_says_authorization_is_on(auth: AuthClient) -> None:
    """`authentication: null` and "this server checks nothing" must not be the
    same thing on the wire — a client cannot ask for a credential it has not been
    told exists."""
    body = auth.get("/.well-known/mindex.json").json()
    assert body["authentication"] is not None, body
    assert "actions" in body["authentication"], body["authentication"]


def test_the_global_surfaces_need_admin_however_wide_the_project_list(
    auth: AuthClient,
) -> None:
    """`/gc` walks every collection and holds a process-wide guard, so no project
    list can describe it. A wildcard token without `admin` must still be refused,
    or `prj: ["*"]` would silently mean `admin` too."""
    everything_but_admin = auth.token_for(
        ["*"], ["search", "research", "index", "delete", "mint"]
    )
    for method, path in [("POST", "/gc"), ("GET", "/status"), ("GET", "/metrics")]:
        r = auth.request(method, path, everything_but_admin)
        assert r.status_code == 403, f"{path} was served: {r.status_code}"


# ── Listing endpoints filter a body, which is why this lives in the server ───


def test_the_project_listing_shows_only_what_the_token_covers(
    auth: AuthClient,
) -> None:
    """The reason authorization could not have been done in the gateway.

    `GET /projects` enumerates GUIDs in a *response body*; no proxy filters that
    without parsing it, and a GUID is a bearer identifier.
    """
    mine, theirs = uuid.uuid4().hex, uuid.uuid4().hex
    for guid in (mine, theirs):
        owner = auth.token_for([guid], ["index"])
        assert (
            auth.post(f"/v0/{guid}/index", owner, json=index_body()).status_code == 200
        )

    listed = auth.get("/projects", auth.token_for([mine], ["search"])).json()
    guids = {p["project_guid"].replace("-", "") for p in listed["projects"]}
    assert mine in guids
    assert theirs not in guids, "a project outside the token was listed"

    # And a wildcard token sees both, so the filter is a filter and not a bug
    # that happens to hide everything.
    all_guids = {
        p["project_guid"].replace("-", "")
        for p in auth.get("/projects", auth.token_for(["*"], ["search"])).json()[
            "projects"
        ]
    }
    assert {mine, theirs} <= all_guids


# ── Revocation by key id ─────────────────────────────────────────────────────


def test_a_token_under_a_second_key_id_verifies_like_any_other(
    auth: AuthClient, revocable_token: str
) -> None:
    """Half of the revocation story that can be tested without mutating the
    server's key file: a `kid` other than the active one is a first-class token,
    so giving a holder its own key costs nothing until the day it is deleted.

    The other half — that deleting the table withdraws it and nothing else — is a
    unit test, because doing it here would leave the shared stack short a key.
    """
    r = auth.get("/projects", revocable_token)
    assert r.status_code == 200, r.text


# ── The unauthorized deployment ──────────────────────────────────────────────


def test_a_server_with_authorization_off_mints_nothing(client: httpx.Client) -> None:
    """`POST /auth/tokens` on the default deployment.

    With `[auth]` off there is no keyring, so there is nothing to sign with — and
    the endpoint is waved through by `MintScope` precisely because no token was
    required. The dangerous reading of that combination is "no minter, so anyone
    may mint"; the answer is 404, because on such a deployment this genuinely is
    not a thing the server does.

    This runs against the *other* stack in this compose file, which is the one the
    rest of the suite uses — so it is also a statement that adding authorization
    did not turn a credential-free server into one that issues credentials.
    """
    r = client.post(
        f"{MINDEX_URL}/auth/tokens",
        json={"sub": "x", "projects": ["*"], "actions": ["admin"], "days": 1},
    )
    assert r.status_code == 404, r.text
    assert "eyJ" not in r.text, f"an unauthorized server issued a token: {r.text}"


def test_authorization_off_ignores_a_bearer_header_entirely(
    client: httpx.Client, project: str
) -> None:
    """A client-supplied `Authorization` decides nothing when authorization is off.

    Both directions matter. A *bad* token must not start being refused — that
    would break every client the day a header was added — and a *good-looking*
    one must not be treated as authority, or `enabled = false` would quietly
    become "enforce whatever the caller claims".
    """
    for header in [None, "Bearer nonsense", "Bearer eyJhbGciOiJIUzI1NiJ9.e30.x"]:
        headers = {} if header is None else {"Authorization": header}
        r = client.post(
            f"{MINDEX_URL}/v0/{project}/search",
            json={"query": "x"},
            headers=headers,
        )
        # 404 `search.no_match` — an empty project, reached without a credential.
        assert r.status_code == 404, f"{header}: {r.status_code} {r.text[:200]}"
        assert r.json()["code"] == "search.no_match", f"{header}: {r.text}"
