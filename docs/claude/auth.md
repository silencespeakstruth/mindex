# Authorization: scoped bearer tokens

Companion to `.claude/CLAUDE.md`'s **Authorization** section. Read this before
modifying `src/backend/auth.rs`, `src/backend/extract.rs`'s scope extractors, or
`ROUTE_POLICY`.

## What problem this solves, and what it does not

Two, and they turned out to be one.

**Isolation.** A credential that passed the gateway reached every project.
`GET /projects` enumerates every GUID in a response *body*, so no proxy can
filter it without parsing JSON — and a GUID is a bearer identifier, so leaking
one hands over that project's whole data plane. Authorization therefore had to
live in the server; there was never a gateway-only version of this.

**A credential in a model's context.** Every shipped client already holds its
key in a process, so an ordinary agent never sees one. The leak was `/llms.txt`:
an agent handed a URL has no process holding anything, so the only way it can
send a credential is for a human to paste one into a chat. That is acceptable
only if what gets pasted is narrow and expiring, which is what a scoped token is.

**What it does not solve:** nothing here stops a token entering a context. No
server can. It makes what enters one cheap to lose.

## The API key is gone, and that is part of the design

The gateway used to check a shared `X-Api-Key` that mindex ignored, and it was
removed rather than kept as a second layer. Two credentials where one is
strictly stronger is not defence in depth — the weaker one sets the floor, and
this one had no scope, no expiry, and no way to withdraw a single holder.

The decisive argument is the second problem above. A token is worth pasting into
a context precisely because it is narrow; a deployment that *also* demanded the
API key would put the shared secret back into that same context, which is the
leak the token was introduced to close. Requiring both makes the token pointless
for its one hard case.

So the gateway (`deploy/gate/`) now admits on the token's **presence** — it
cannot verify a signature and does not try — and every question about validity,
scope and action is answered here. One consequence, written into that file:
**`[auth].enabled = true` is mandatory for any deployment reachable through a
gateway.** With authorization off, a caller sending the literal string
`Authorization: Bearer x` is admitted by the gateway and served everything.
`enabled = false` is now exactly one thing: a server on a trusted network that
authorizes nothing, which is what the Docker test stack and a loopback-only
install are.

## The shape

HS256, signed and verified by the same process — asymmetry buys nothing and
costs a key format. The TLS certificate's key is deliberately not reused: a key
serving two protocols is a known anti-pattern, and the certificate rotates on a
schedule unrelated to token lifetimes.

```json
{ "iss": "mindex", "sub": "<label>", "jti": "…", "iat": …, "nbf": …, "exp": …,
  "prj": ["c2d7e2c1316542f593660ff1492b4bab"], "act": ["search", "research"] }
```

`prj` holds dashless GUIDs, normalized at mint. `["*"]` is legal and **must be
spelled**: an empty list reaches nothing. Actions: `search`, `research`,
`index`, `delete`, `admin`, `mint`.

There is no `gc` action. `POST /gc` holds the process-wide `GcGuard`, walks every
collection and names other callers' collections in `failed_phases` — a project
list cannot describe it, so scoping it per project would be a promise the
endpoint cannot keep. It is `admin`, with `/status` and `/metrics`.

## Why it is written here rather than taken from a JWT crate

The requirement was control over the signing secret's copies in memory. A
library that copies the key into its own `EncodingKey`/`DecodingKey` puts a copy
beyond reach of any zeroing this code can do. Owning ~100 lines means owning
every byte — and it closes the algorithm-confusion family *by construction*
rather than by configuration: `verify` reads `kid` and nothing else before
checking the MAC, so `alg: none` and an RS256-header-over-HMAC token both simply
fail the signature. `the_algorithm_header_cannot_select_the_algorithm` pins it,
and was checked by reintroducing header-driven dispatch.

## Two enforcement layers, and why both

**The extractors** (`SearchScope`, `IndexScope`, …) are the mechanism: every
project-keyed handler takes one, and it checks `covers(guid)` then
`permits(action)`. In that order — a caller that cannot see the project must
learn nothing about the action vocabulary, which is what
`a_foreign_project_is_refused_before_the_action_is_considered` pins.

**The default-deny layer** (`enforce_route_policy`) is the runtime half. A
routed path with no `ROUTE_POLICY` row is **refused**, not served. The
build-time guard catches the same mistake when the suite runs; this catches it
when the request arrives, and that is the difference between a leak that fails a
test and a leak that ships.

The layer deliberately does not do the project check: `/drift` must answer an
out-of-scope project as it answers an unknown one, `/index` must be able to
create, and the two listings filter a body. A blanket answer would need a
per-route exception table — the hand-kept fifth copy of the route list that
nothing checks.

## The refusals, and which one is deliberate

| condition | answer |
|---|---|
| no token | 401 `auth.token_missing` |
| bad signature / unknown `kid` / malformed | 401 `auth.token_invalid` |
| expired | 401 `auth.token_expired` |
| valid, holds the project, lacks the action | 403 `auth.action_not_permitted` |
| valid, does not hold the project | **404 `project.not_found`** |
| routed path with no policy row | 403 `auth.route_not_configured` + `error!` |

The 404 is the load-bearing one. It is **byte-identical** to the answer for a
project that was never indexed, and a client must not render it as plain
absence. A distinguishable refusal confirms which GUIDs exist, and an error
`code` is exactly the field clients are told to key on — so `auth.forbidden`
cannot exist on that path however much better it would read in a log.

The action, by contrast, *is* named: the caller already proved it holds the
project, so naming what it lacks tells it nothing it could not read out of its
own token, while hiding it would leave an under-scoped credential
indistinguishable from a wrong one.

## Public routes

`/health`, `/version`, `/config`, `/llms.txt`, `/.well-known/mindex.json`, plus
everything under `/swagger-ui` and `/api-docs/` (`PUBLIC_PATH_PREFIXES` — they
are `merge`d rather than routed, so they are not in `ROUTE_POLICY` and the
default-deny layer would otherwise refuse them as build defects).

Liveness first: a probe that needs a credential reports the credential's health,
not the server's. Discovery second, and the reasoning is circular by nature — a
document that tells a caller it needs a credential cannot itself require one.
They describe the API's shape and hold no project, no chunk and no report.

## `aud`: a label, and the one claim the server does not check

`--for cli,vscode,agent` writes an `aud` list into the token, and **nothing in
the server ever reads it**. That is not an omission to be corrected later: no
part of an HTTP request identifies the process behind it, a `User-Agent` is a
client-supplied string, and a bearer token works from anywhere by construction.
A server-side check would be theatre.

The clients check it, and what that buys is narrow and real: pasting the
editor's credential into a shell profile, or the agent's into the editor's
keychain, is refused by the thing receiving it, in a sentence naming both
audiences. It stops an accident. It stops no attacker — anything holding the
token simply does not run the check, and every action the token names still
works. So whatever must genuinely be refused belongs in `act` and `prj`.

Three consequences, each pinned by a test:

- **An empty or absent `aud` means every audience.** The claim is
  `skip_serializing_if = "Vec::is_empty"`, so an unlabelled token does not carry
  the key at all — a client keying on presence must not meet `"aud": []` and read
  it as an allow-list reaching nobody. Reading absence as "nobody" would have
  locked out every existing holder on the day the field shipped.
- **`may_mint` does not contain it**, unlike actions, projects and expiry. A
  wider audience is not more authority, and delegation is a change of *holder* by
  definition: the motivating case is the VS Code button minting an `agent` token
  from a `vscode` one, which containment would refuse outright — with an error
  about exceeding the minter, which reads like a security decision.
  `the_audience_is_not_an_authority_axis_and_does_not_bind_delegation` is what
  stops a reviewer "fixing" the inconsistency.
- **There is no `Claims::intended_for` in the server.** A predicate there would
  have no production caller and would read, to whoever found it, as a check the
  server performs. The clients hold their own: `mindexfile::token::audience_refusal`
  for the Rust CLIs (the indexer and watcher call it once, where the token is
  fully resolved, and refuse rather than warn — the request would otherwise
  succeed, and a warning followed by success is read once) and
  `token.ts`'s `audienceRefusal` for the extension, which is overridable through a
  modal because the label is a hint and the person holding the token may know
  better than it does.

**Two clients deliberately do not check it**, and the omission is stated here so
it is not mistaken for one: `mindex-search.sh` and the two MCP servers. Both are
configured once by an operator and then never touched, which is the case the
check buys least in — the mistake it catches is a person moving a credential
between places, and neither of these is a place a credential gets moved *to* by
hand. Building it anyway would mean a base64/JSON decoder in bash and a third and
fourth copy in Python, for a mechanism that enforces nothing. If they ever gain
it, it belongs beside `_headers()` in each server, not in a fifth parser.

## Minting write actions

`POST /auth/tokens` will issue `index` and `delete`, and the VS Code flow offers
them behind a second modal naming what they cost — `index` also through a
read-and-write preset, `delete` only by ticking it. The rejected
alternative was a read-only vocabulary at the network endpoint. It does not
prevent a write token existing — it moves the minting to a shell on the host,
where what actually gets issued is usually *wider* than what was asked for. What
keeps this safe is the containment rule, exhaustively tested: a minter without
`index` cannot pass it on. `admin` and `mint` stay off the VS Code menu, which is
a different call — neither is something an agent needs to work on a project, and
`mint` in particular is the power to keep issuing credentials after this one
expires, which is the bound a short lifetime was buying.

## Revocation, which is the weak point

A signed token is valid until it expires. There is no denylist, by design: that
is the per-request state this whole mechanism exists without. Two remedies:

- **Expiry.** `[auth].max_token_days` is the ceiling. Guest tokens want days,
  not months.
- **Delete a key id.** The key file holds several keys; a holder minted under its
  own `kid` is revoked by deleting one table, and tokens under other ids keep
  verifying. `dropping_one_key_id_revokes_only_its_tokens` pins it. This is why
  `--key-id … --new-key` exists: per-holder ids are one flag rather than a
  hand-edited file, and they are what makes revocation something other than
  rotating every credential at once.

```toml
# [auth].signing_key_file — created 0600 with one key on first start
active = "working"
[keys.working]
secret = "…"
note   = "the machines that hold the repositories"
[keys.guest-2026-08]
secret = "…"
note   = "external review agent; delete this table to revoke"
```

If real revocation is ever wanted, the next rung is a `jti` denylist — and it
brings back the state. Do not add it casually.

## Runbook

### Turning it on

```toml
[auth]
enabled          = true
signing_key_file = "/var/lib/mindex/signing-keys.toml"
max_token_days   = 90
leeway_seconds   = 60
```

Startup creates the key file 0600 if absent (`O_EXCL`, so two servers racing to
first start cannot both believe they own the secret) and **refuses to start** if
it cannot read or create it. That is deliberate: a server that comes up and then
refuses every request is a total outage disguised as a client-side credential
problem.

### Minting

```sh
# The working credential for a machine holding repositories. Never paste this
# into a model's context — the command warns about exactly that.
mindex mint-token --sub cli@$(hostname) --project '*' \
  --can search,research,index,delete --for cli --days 90

# A guest. One project, read-only, its own key id so it can be revoked alone.
mindex mint-token --sub 'guest:review-bot' \
  --project c2d7e2c1-3165-42f5-9366-0ff1492b4bab \
  --can search,research --for agent --days 14 --key-id guest-2026-08

# The editor's own. `mint` is what makes the agent-token button work, and it
# cannot escalate: `may_mint` caps every axis at this token's own.
mindex mint-token --sub 'vscode@laptop' --project '*' \
  --can search,research,index,delete,mint --for vscode --days 90
```

`--for` is optional and omitting it produces a token every client accepts, which
is the right default for a deployment that has never met the claim.

The token goes to stdout and everything else to stderr, so it pipes. It is
printed once and stored nowhere.

`POST /auth/tokens` is the network form, requiring a token that carries `mint`.
A minted token can never exceed its minter — not a wider action set, not a wider
project list, not a later expiry. Without that rule, a read-only `mint`
credential becomes `admin` one call later.

### Where a client keeps it

Four sources, first one wins, the same order in every client:

1. `--token` (CLI only) — visible in `ps`, so it is the debugging spelling.
2. `$MINDEX_TOKEN`.
3. `$MINDEX_TOKEN_FILE` — a **path** to a 0600 file holding the token. It exists
   for a caller configured by an environment block written into someone else's
   configuration file: an MCP server list lives in an editor's own JSON, where a
   token would sit in plaintext under no permission check. A path does not.
   Note the ordering trap: `MINDEX_TOKEN` wins, and a shell that exports it for
   the CLI passes it down to every child — so an MCP block that wants its own
   narrow token must set `MINDEX_TOKEN` to the empty string alongside the path.
4. `~/.config/mindex/credentials.toml`, mode 0600, keyed by server URL — read by
   `mindex-index` and `mindex-watch`. The right home for a long-lived credential
   on a machine that talks to more than one deployment.

```toml
["https://127.0.0.1:11111"]
token = "eyJhbGciOi…"
```

**VS Code is the exception and keeps its own copy in `SecretStorage`.** Not
because a keychain is the answer to "who holds the credential" — it cannot be,
since the CLI must read the same kind of credential and no extension's keychain
is readable from a shell — but because the alternative *within the extension* was
a settings string, which lands in a plaintext `settings.json` and is carried to
every other machine by Settings Sync. The two homes are separate copies on
purpose; mint one token per holder rather than sharing one, so a lost laptop is
one `kid` to delete.

The extension also watches the token's own clock (`src/token.ts`): a status-bar
entry appears `mindex.tokenWarningHours` before expiry (24 by default, `0` off)
and turns red under an hour or once expired. It is absent while the token is
healthy, which is what makes its presence informative. It reads `exp` out of the
payload and **verifies nothing** — a client asserting a token's validity would be
claiming a fact only the server establishes.

### Metrics, once authorization is on

`GET /metrics` is `admin`. It carries `project_guid` as a label and per-project
chunk counts, so on a multi-caller deployment it is a cross-caller leak — and
mindex cannot tell a loopback scraper from a gated one, so **the local scrape
breaks unless it sends a token**.

```sh
mindex mint-token --sub victoriametrics@$(hostname) --project '*' --can admin \
  --days 0 --key-id metrics-scraper --new-key > /tmp/vm.jwt
sudo install -m 0640 -o root -g victoriametrics /tmp/vm.jwt \
  /etc/victoriametrics/mindex-token && shred -u /tmp/vm.jwt
```

```yaml
# deploy/victoriametrics/mindex.scrape.yml
  bearer_token_file: /etc/victoriametrics/mindex-token
```

**Not under `$HOME`.** The VictoriaMetrics unit runs `ProtectHome=true`, so a
path there reads as absent — and the symptom is a scrape that simply stops,
never a message about permissions. `/etc/victoriametrics/` is the natural home;
0640 owned by the unit's user (`DynamicUser=true` still allocates a stable uid
for a named `User=`).

**`--days 0` is right here and almost nowhere else**, and its own `--key-id` is
what makes it right. This is a machine-local credential in a 0640 file that no
context ever sees, and an expiry would blank every dashboard at an hour nobody is
watching; the separate key id means withdrawing it is one table deleted from the
key file rather than a rotation that logs out every other client. The earlier
advice here — a year plus a calendar reminder — was strictly worse: it kept the
outage, just on a schedule.

### A read-only extension, and the new-project trap

The VS Code extension runs on whatever its token carries and never refuses to
start. A `search`-only credential is exactly what a narrow token is *for*, and it
is the most useful thing to hand somebody who should not be able to reindex, so
refusing to activate would delete the feature in the surface that serves it best.

What it does instead is the mechanism that was already there for a failing
dependency: `tokenAvailability` folds the token's `act`/`prj` into the health
verdict through `mergeAvailability`, the mode's controls freeze, and the notice
inside the tab names what is missing. **The tabs stay live in every state** —
CLAUDE.md's rule, for the same reason as with Ollama: a disabled tab is a dead
end whose explanation lives behind it. Reading it as a *hint* is deliberate and
matches the language pickers: this code reads the payload of a credential it
cannot verify, so it decides what to offer, and the server decides what to serve.
The token reason wins over the health reason — but only while the server can
still serve something. A dependency comes back by itself and a missing action
does not, which is the argument for preferring the token; it stops applying when
`ask` is already false, because then the server is the whole story and naming the
token sends the user to re-mint a credential that was never the problem.

Beyond the Ask form, `reindex()` checks `index` up front. Without it a batch of
uploads 403s file by file, which renders as a partial reindex with per-file
failures when the one true sentence is that this credential does not index.

**The sharp case is a project the extension has just created.** The Drift view's
welcome button writes a fresh UUID into `.mindex`, and no token names it. From
then on every request answers 404 `project.not_found` — byte-identical to a GUID
nobody has ever indexed, deliberately, so nothing in any later response can tell
the two apart. It is perfectly answerable *here*, though: the extension holds the
token and wrote the GUID a line ago, so `createProjectFile` hands the GUID back
and the caller says so.

There is deliberately no "issue a token for it" button on that message, because
there is never one to offer: a wildcard token already covers the new GUID and
never reaches the warning, and a token scoped to named projects is refused by
`may_mint` when it asks for a project it does not hold. The remedy really is a
`mint-token` on the server's host, and saying anything else would be a dead end
with a button on it.

## Honest limitations

1. **Revocation is expiry or `kid` deletion.** See above.
2. **A pasted token is still a bearer credential.** Scoped and expiring makes it
   cheap to lose, not safe.
3. **`prj: ["*"]` is exactly as dangerous as the old API key.** It exists so a
   working token does not need re-minting per project. Never mint one for a
   guest.
4. **The secret is resident for the process's life.** Zeroing helps on the
   short-lived `mint-token` process and on rotation, and almost nothing on the
   server. `mlock` is not attempted: keeping one copy out of swap while the
   kernel pages the rest is a partial measure that reads as a complete one. If
   swap is in the threat model, encrypt the swap device.
5. **`POST /index` on an unknown GUID is a mild oracle** — a fresh GUID the
   token names creates and 200s. It is inherent: GUIDs must be globally unique
   because the Qdrant collection is named from the GUID alone. Unexploitable by
   guessing at 128 bits.
