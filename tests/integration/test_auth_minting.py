"""Deriving one token from another, over the network, and then using it.

This is the flow the VS Code button and every agent handoff run on, and it is the
one place a mistake escalates rather than merely refusing: a containment bug does
not fail loudly, it hands somebody a working credential wider than the one they
had. So the tests here do not stop at the status code of `POST /auth/tokens` —
each derived token is **used**, and the assertions are about what the server then
serves it.

The unit suite proves `may_mint` over the whole action vocabulary in both roles.
What only a running server can establish is that the endpoint hands `may_mint`
the claim set it is about to sign, rather than a paraphrase of the request: the
handler parses actions, parses audiences, caps days against its own ceiling and
picks a key, and any of those four steps could produce a token that differs from
what was judged.
"""

import uuid

import pytest
from conftest import AuthClient

# `/index` is keyed language -> path -> body, not a flat list.
FILE_PATH = "src/lib.rs"
FILES = {"files": {"rust": {FILE_PATH: {"code": "fn a() -> u32 { 1 }\n"}}}}
ALL_ACTIONS = ["search", "research", "index", "delete", "admin", "mint"]


def claims_of(token: str) -> dict:
    """Decode the payload without verifying — the server already did that."""
    import base64
    import json

    payload = token.split(".")[1]
    padded = payload + "=" * (-len(payload) % 4)
    return dict(json.loads(base64.urlsafe_b64decode(padded)))


# ── The narrow token an agent actually gets ──────────────────────────────────


def test_an_agent_token_reaches_its_project_and_nothing_else(
    auth: AuthClient,
) -> None:
    """The whole point of the mechanism, end to end.

    A read-and-research token for one project: it searches that project, it is
    refused a second one as though that project did not exist, and it cannot
    write to its own.
    """
    mine, theirs = uuid.uuid4().hex, uuid.uuid4().hex
    for guid in (mine, theirs):
        owner = auth.token_for([guid], ["index"])
        assert auth.post(f"/v0/{guid}/index", owner, json=FILES).status_code == 200

    agent = auth.token_for([mine], ["search", "research"], audiences=["agent"])

    # It works where it should. A 404 here would be `search.no_match`, not a
    # scope refusal — so the code is asserted, not merely the status.
    r = auth.post(f"/v0/{mine}/search", agent, json={"query": "fn a", "top_k": 3})
    assert r.status_code in (200, 404), r.text
    if r.status_code == 404:
        assert r.json()["code"] == "search.no_match", r.text

    # It is refused elsewhere, indistinguishably from absence.
    r = auth.post(f"/v0/{theirs}/search", agent, json={"query": "fn a"})
    assert r.status_code == 404 and r.json()["code"] == "project.not_found", r.text

    # And it cannot write, even to the project it holds.
    for method, path, body in [
        ("POST", f"/v0/{mine}/index", FILES),
        ("DELETE", f"/projects/{mine}/files", {"include": {"paths": ["**"]}}),
        ("POST", "/gc", None),
    ]:
        r = auth.request(method, path, agent, json=body)
        assert r.status_code == 403, f"{path} was served to a read-only agent: {r.text}"


def test_the_audience_survives_the_round_trip(auth: AuthClient, project: str) -> None:
    """The label the clients honour has to arrive inside the token, not only in
    the response envelope — a client reads the token, not the mint reply."""
    r = auth.mint([project], ["search"], audiences=["agent"])
    assert r.status_code == 200, r.text
    assert r.json()["audiences"] == ["agent"]
    assert claims_of(r.json()["token"])["aud"] == ["agent"]


def test_an_unlabelled_token_carries_no_audience_key_at_all(
    auth: AuthClient, project: str
) -> None:
    """`"aud": []` and no `aud` are different bytes, and the clients key on
    presence: an empty list read as an allow-list reaches nobody, which would
    lock out every holder of a token minted before the claim existed."""
    r = auth.mint([project], ["search"])
    assert r.status_code == 200, r.text
    assert r.json()["audiences"] == []
    assert "aud" not in claims_of(r.json()["token"])


def test_an_audience_does_not_change_what_the_server_serves(
    auth: AuthClient, project: str
) -> None:
    """The honest half of the feature. `aud` is a label the clients check; if the
    server ever started enforcing it, this test says so — and the docs promising
    that it does not would have to change with it."""
    owner = auth.token_for([project], ["index"])
    assert auth.post(f"/v0/{project}/index", owner, json=FILES).status_code == 200
    for aud in (["agent"], ["vscode"], ["cli"], None):
        token = auth.token_for([project], ["search"], audiences=aud)
        r = auth.post(f"/v0/{project}/search", token, json={"query": "fn a"})
        assert r.status_code in (200, 404), f"aud={aud}: {r.text}"
        if r.status_code == 404:
            assert r.json()["code"] == "search.no_match", f"aud={aud}: {r.text}"


# ── Containment, at the endpoint ─────────────────────────────────────────────


@pytest.mark.parametrize("wanted", ALL_ACTIONS)
def test_a_minter_can_pass_on_exactly_what_it_holds(
    auth: AuthClient, wanted: str
) -> None:
    """Driven from the action vocabulary rather than from an example.

    The failure this shape catches is a hand-written list of "dangerous" actions:
    such a list rejects `admin`, passes `delete`, and looks completely correct in
    a test that only ever tries `admin`. Here every action is tried in both roles,
    so a seventh action added later is covered on the day it is added.
    """
    project = uuid.uuid4().hex
    # A minter holding `wanted` (plus `mint`, without which it cannot ask at all).
    holder = auth.token_for([project], ["mint", wanted], days=1)
    r = auth.mint([project], [wanted], minter=holder)
    assert r.status_code == 200, f"a holder of {wanted} could not pass it on: {r.text}"
    assert wanted in r.json()["actions"]

    # And a minter holding everything *except* `wanted` cannot.
    others = [a for a in ALL_ACTIONS if a != wanted]
    without = auth.token_for([project], ["mint", *others], days=1)
    r = auth.mint([project], [wanted], minter=without)
    if wanted == "mint":
        # `mint` is in `others` by construction there is no "without mint" case
        # that can reach this endpoint at all — the extractor refuses first.
        assert r.status_code == 200, r.text
    else:
        msg = f"{wanted} was minted by a token that lacks it: {r.text}"
        assert r.status_code == 400, msg
        assert "eyJ" not in r.text, f"a refusal carried a token: {r.text}"


def test_a_minter_without_mint_never_reaches_the_endpoint(
    auth: AuthClient, project: str
) -> None:
    """Refused by the extractor, before the body is read — hence 403 naming the
    action rather than 400 about containment."""
    no_mint = auth.token_for([project], ["search", "research", "index", "delete"])
    r = auth.mint([project], ["search"], minter=no_mint)
    assert r.status_code == 403, r.text
    assert r.json()["code"] == "auth.action_not_permitted"
    assert "mint" in r.text


def test_a_named_minter_cannot_reach_a_project_it_does_not_hold(
    auth: AuthClient,
) -> None:
    """Including the wildcard, which is the escalation worth naming: a token for
    one project issuing `["*"]` would be the shared API key back again."""
    mine, theirs = uuid.uuid4().hex, uuid.uuid4().hex
    minter = auth.token_for([mine], ["mint", "search"])

    for projects in ([theirs], ["*"], [mine, theirs]):
        r = auth.mint(projects, ["search"], minter=minter)
        assert r.status_code == 400, f"{projects} was minted: {r.text}"
        assert "eyJ" not in r.text


def test_a_minted_token_cannot_outlive_its_minter(auth: AuthClient) -> None:
    """Expiry is the main bound on a leak, since there is no denylist. A minter
    that can issue a longer-lived token than itself removes that bound one call
    at a time."""
    project = uuid.uuid4().hex
    short = auth.token_for([project], ["mint", "search"], days=1)
    assert auth.mint([project], ["search"], days=7, minter=short).status_code == 400
    assert auth.mint([project], ["search"], days=1, minter=short).status_code == 200


def test_a_non_expiring_token_is_not_mintable_over_the_network(
    auth: AuthClient, project: str
) -> None:
    """`--days 0` exists for a machine-local credential nothing renews. A
    network-reachable way to issue an eternal one is a different and worse thing,
    and it must stay refused even for the root token."""
    r = auth.mint([project], ["search"], days=0)
    assert r.status_code == 400, r.text
    assert "eyJ" not in r.text
    assert "mint-token" in r.text, "the refusal should name the local way to do it"


def test_a_year_is_refused_by_whichever_bound_is_stricter(
    auth: AuthClient, project: str
) -> None:
    """Two ceilings apply and the stricter one binds: `[auth].max_token_days` (30
    in this stack) and the minting token's own remaining life (also 30 days, and
    already a few seconds spent).

    So a request for a year is refused rather than silently capped, and which of
    the two did it is not asserted — that would pin an ordering the code is free
    to change. What is asserted is that no year-long token comes back, which is
    the property that matters: expiry is the only bound on a leak there is.
    """
    r = auth.mint([project], ["search"], days=365)
    assert r.status_code == 400, r.text
    assert "eyJ" not in r.text


# ── Delegation chains ────────────────────────────────────────────────────────


def test_authority_cannot_grow_along_a_chain(auth: AuthClient) -> None:
    """A → B → C. Each step is contained, so the composition is; the check
    exists because "contained at each step" is exactly the property a reviewer
    assumes without testing, and it is the one an off-by-one in the project
    comparison would break."""
    a_prj, other = uuid.uuid4().hex, uuid.uuid4().hex

    a = auth.token_for([a_prj], ["mint", "search", "research"])
    b = auth.token_for([a_prj], ["mint", "search"], minter=a)

    # B cannot pass on what A did not give it.
    assert auth.mint([a_prj], ["research"], minter=b).status_code == 400
    assert auth.mint([other], ["search"], minter=b).status_code == 400
    assert auth.mint(["*"], ["search"], minter=b).status_code == 400

    # But it can pass on what it holds, and C is then no wider than B.
    c = auth.token_for([a_prj], ["search"], minter=b)
    assert claims_of(c)["act"] == ["search"]
    assert claims_of(c)["prj"] == [a_prj]


def test_a_write_token_is_derivable_and_actually_writes(auth: AuthClient) -> None:
    """The case the read-only-vocabulary design would have moved to a shell.

    An agent working on this machine legitimately needs `index`, and the token it
    gets should be the narrow one somebody issued deliberately rather than the
    wide one they had lying around. So this asserts the whole path: derive it,
    use it to index, and confirm the file really landed.
    """
    project = uuid.uuid4().hex
    writer = auth.token_for([project], ["index", "search"], audiences=["agent"])

    r = auth.post(f"/v0/{project}/index", writer, json=FILES)
    assert r.status_code == 200, r.text

    listed = auth.get(f"/projects/{project}/files", writer)
    assert listed.status_code == 200, listed.text
    assert any(f["path"] == FILE_PATH for f in listed.json()["files"]), listed.text

    # And it still cannot delete, which is the narrowing that had to survive
    # being asked for alongside a write action.
    r = auth.delete(
        f"/projects/{project}/files", writer, json={"include": {"paths": ["**"]}}
    )
    assert r.status_code == 403, r.text


def test_a_delete_token_is_derivable_and_actually_deletes(auth: AuthClient) -> None:
    """Its sibling, because `delete` is the action a "dangerous list" is most
    likely to be missing — and a soft delete leaves the file listed as `deleted`
    rather than absent, so the assertion has to look at the status."""
    project = uuid.uuid4().hex
    writer = auth.token_for([project], ["index", "delete", "search"])
    assert auth.post(f"/v0/{project}/index", writer, json=FILES).status_code == 200

    r = auth.delete(
        f"/projects/{project}/files", writer, json={"include": {"paths": ["**"]}}
    )
    assert r.status_code in (200, 204), r.text

    active = [
        f
        for f in auth.get(f"/projects/{project}/files", writer).json()["files"]
        if f["status"] != "deleted"
    ]
    assert active == [], active


# ── Malformed mint requests ──────────────────────────────────────────────────


@pytest.mark.parametrize(
    "body",
    [
        {"sub": "x", "projects": ["not-a-guid"], "actions": ["search"], "days": 1},
        {"sub": "x", "projects": ["*"], "actions": ["read"], "days": 1},
        {
            "sub": "x",
            "projects": ["*"],
            "actions": ["search"],
            "audiences": ["ai"],
            "days": 1,
        },
        # `deny_unknown_fields`: a typo'd key must not be silently ignored, or a
        # caller believes it asked for something it did not.
        {"sub": "x", "projects": ["*"], "actions": ["search"], "day": 1},
        {"sub": "x", "projects": ["*"], "action": ["search"], "days": 1},
    ],
)
def test_a_malformed_mint_request_yields_no_token(auth: AuthClient, body: dict) -> None:
    r = auth.post("/auth/tokens", auth.root, json=body)
    assert r.status_code == 400, f"{body} was accepted: {r.text}"
    assert "eyJ" not in r.text, f"{body} produced a token: {r.text}"


def test_an_empty_project_list_reaches_nothing_rather_than_everything(
    auth: AuthClient, project: str
) -> None:
    """The wildcard must be spelled. An omitted list read as "everything" is how
    a minter hands out full access by accident, and the resulting token looks
    perfectly ordinary."""
    r = auth.mint([], ["search"])
    assert r.status_code == 200, r.text
    nothing = r.json()["token"]
    assert claims_of(nothing)["prj"] == []

    r = auth.post(f"/v0/{project}/search", nothing, json={"query": "x"})
    assert r.status_code == 404 and r.json()["code"] == "project.not_found", r.text
