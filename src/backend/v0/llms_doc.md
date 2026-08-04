# mindex — semantic code index with a local research agent

## The problem this solves

Answering a question about an unfamiliar codebase normally means reading it:
a chain of searches, a walk through files, a few false leads, and a growing
transcript that has to be re-sent on every subsequent turn. The reading is
what costs — and most of what gets read turns out not to bear on the answer.

mindex moves that work off the expensive model and onto local hardware. The
index is cheap to query and holds the whole tree, chunked along syntax
boundaries rather than line counts. The research agent runs on a local model:
it plans, calls eleven internal tools, follows its own leads, and returns one
Markdown report with `path:start-end` citations — for the price of asking a
question. The intended division of labour is therefore:

- **One question to `POST /v0/{project_guid}/research`** in place of an
  investigation. The run does the reading; the caller reads a report.
- **`POST /v0/{project_guid}/search` and `POST /v0/{project_guid}/symbols`**
  for the narrow, paid half: the byte-exact text of a place already known to
  matter, typically one a report has just cited and something is about to
  edit.
- **The caller steers rather than executes**: it frames the question, sets the
  scope and effort, reads `citations` and `done_reason` to see what the run
  actually established, and sends a follow-up chained to the first when the
  answer falls short.

What makes this safe to act on is that the server checks its own output.
Every citation in a report is scored against what the run's tools actually
returned, so a report arrives with a machine-readable statement of how much
of it is grounded. That check is the thing a reader would otherwise have to
perform by re-reading the files — which is the cost the whole design exists to
avoid.

## What a caller needs: a token and a project GUID

A handful of routes answer anyone — this document, `GET /config`,
`GET /version`, `GET /health` and `/.well-known/mindex.json`. They describe the
API's shape and hold no code, no report and no project. Everything else needs
two values, and neither can be guessed or derived:

**1. A bearer token**, sent as `Authorization: Bearer <token>`. Deployments that
run with authorization on issue these; the server signs them itself, and each one
names the projects it reaches and the actions it permits (`search`, `research`,
`index`, `delete`, `admin`, `mint`). There is no endpoint that hands one out
without already holding one, and no default value — a token comes from whoever
runs the deployment, minted with `mindex mint-token` on the server's host.

A caller that already holds a token carrying the `mint` action can derive a
narrower one over `POST /auth/tokens`, which is worth knowing before delegating:
a token handed to a subprocess or pasted into another context is better issued
for that purpose than shared. What comes back can never exceed the token that
asked for it — not in actions, not in projects, not in lifetime — so this widens
nothing, and a non-expiring token is refused here regardless, being issuable only
from the server's own host.

A deployment reachable from outside a host usually also sits behind a gateway,
and a gateway answers a credential-less request in one of two ways. It may
reply `401` with a `WWW-Authenticate: Bearer` header and a `problem+json` body
carrying `code: auth.token_missing` — the same envelope this server uses, so a
caller that already reads its errors needs no special case. Or it may close the
connection with no status line and no body at all, which an HTTP client reports
as a connection or protocol error rather than as a status. Both mean the same
thing, and neither means the server is down: **an empty reply from this host is
far more often a missing token, a path that is not an endpoint here, or an
address the gateway has blocked, than an outage.** The way to tell is
`GET /health`, which is answered without a token by both the server and any
gateway configured as its documentation describes — a reply there and silence
elsewhere is a credential problem, silence at both is the host.

Where the request reaches the server, the refusals are precise:
`401 auth.token_missing`, `401 auth.token_invalid`, `401 auth.token_expired`,
`403 auth.action_not_permitted` naming the action the token lacks. A `401` from
the gateway and a `401` from the server are not distinguishable by status, and
do not need to be: the remedy for both is a token that this deployment issued.

One refusal is deliberately imprecise and it is worth knowing about. A project a
token does not cover answers **`404 project.not_found`, byte-identical to a
project that was never indexed** — the distinction is withheld on purpose,
because a distinguishable refusal would confirm which GUIDs exist. A caller
seeing that code cannot tell the two apart and should not report it as either
one; "this token does not reach that project, or there is no such project" is
the whole of what is known.

**2. A project GUID**, the `{project_guid}` in every data-plane path. It comes
from `GET /projects`, which lists the projects the token reaches — one request,
and the listing is filtered to the token's own scope, so it is also the cheapest
way to see what a token is for. A repository being indexed carries its GUID in a
committed `.mindex` file at its root, under the `guid:` key.

`GET /health` is the cheapest confirmation that the server is up, though it
answers without a token and so says nothing about whether one works. `GET
/projects` is the check that covers both. A run of requests that all fail
identically before that check has passed is almost always the credential, not
the API.

## Transport and error model

- Internal service. TLS is the only transport security, and the certificate may
  be locally issued; a client that refuses it has a trust-store problem rather
  than a server error. Authorization is optional and off by default — a
  deployment that runs without it authorizes nothing and answers every caller
  that can reach the port, which is why such a deployment belongs on a trusted
  network. `/.well-known/mindex.json` says which of the two this one is, in its
  `authentication` field.
- Every non-2xx response is RFC 7807 `application/problem+json` carrying a
  stable machine `code` (`validation.top_k_out_of_range`, `research.busy`,
  and so on). The `code` is the contract and is pinned by a snapshot test;
  `title` and `detail` are English prose and may be reworded between
  versions. Field-specific errors add `field` and a structured `meta`.
- The API has two planes. The **write plane** — `POST /v0/{project_guid}/index`,
  `POST /projects/{project_guid}/drift`, `DELETE /projects/{project_guid}/files` —
  is driven by clients that hold the working tree: `mindex-index`, the file
  watcher, the VS Code extension and the MCP tools. The **read plane**
  answers over what those clients indexed and needs no filesystem access at
  all. A remote caller with no working tree lives entirely on the read plane,
  and a deployment behind a proxy may not expose the write plane.

## Discovery

`GET /projects` lists every indexed project with its `guid`; that `guid` is
the path component of every data-plane route below.

`GET /config` is the live inventory: supported languages, search bounds, the
research model list, the effort ladder and measured run costs. Two of its
fields are worker-refreshed (`research.models`, `research.observed`), so it
rewards re-reading rather than being cached once.

`GET /projects/{project_guid}` is the per-project inventory: which languages
have active chunks — only those are searchable — plus file and failure
counts.

Machine-readable equivalents of this page: `/.well-known/mindex.json` carries
the service identity, the full endpoint inventory and the same `GET /config`
snapshot as one JSON document, and `/api-docs/openapi.json` carries the full
request/response schemas (rendered at `/swagger-ui`).

## Research

`POST /v0/{project_guid}/research` starts an investigation on the server's
local model. It plans, loops over its internal tools — search, grep, symbols,
outline, list_files, read_chunks, file history, prior research, and two that
let it take notes and revise its own plan — and answers with a cited Markdown
report.

Request: `{"question": "..."}` plus optional `model` (from `research.models`
in `GET /config`), `effort` (`low` / `medium` / `high`), `budget` (per-axis
overrides, capped by the published ceilings), `include`/`exclude` scope
filters, `seed`, and `context_run_ids`. The documentation corpus is written
in English, so questions about documentation retrieve best in English.

One substantial question outperforms several narrow ones: the run's own loop
is what finds the adjacent material, and a question narrowed in advance to
what the caller already suspects removes exactly that. Scope filters are the
place to be specific instead — and a question whose answer might live outside
the scope is better left unscoped.

### The response

By default the answer is **one JSON body**, sent when the run ends: `run_id`,
the resolved `model`/`effort` and their grants, the Markdown `report`, and the
`citations`, `excerpts` and `done` objects described below. Nothing arrives
before then, so the connection is silent for as long as the run takes — up to
the level's `worst_case_seconds`, published per effort level in the live section
at the end of this page.

`?stream=yes` asks for the same run as a **one-way SSE stream** instead, one
JSON frame per `data:` line. Event order is fixed: `started` (carries `run_id`)
→ any number of `step` and `progress` → the report text → `summary` →
`citations` → `excerpts` (only when something verified) → `done`.

Which one to ask for follows from a single fact: **disconnecting cancels the
run**. Frames are worth it to a caller that is showing the run to somebody, or
that wants to stop it early on what it sees — and such a caller is reading to
`done` anyway. A caller that will not read every frame to `done` is better
served by the default, because for it the stream is not a feature: leaving early
spends the whole budget and returns nothing, and that failure raises no error
anywhere. The two modes run the same investigation and store the same report;
only the delivery differs.

The stream-only frames are the ones there is nothing to watch without.
`step` reports each tool call together with the file spans it landed on.
`progress` reports spent against granted on every budget axis; its `binding`
field names the axis with the **largest share spent** — a maximum, not a
warning about what is about to stop the run — and `shares` gives all four
percentages it was chosen from. The JSON body omits both.

Failures differ in one way, and only because they can. On a stream the status is
already `200` by the time a run fails, so the failure is an `error` frame. With
no stream open it is the response: `503 ollama.unavailable` (the model server
did not answer), `503 ollama.error` (it answered with an error — nearly always
a model that is not pulled) or `500 research.no_report`.

`excerpts` carries the indexed code at every verified citation, verbatim.
That is what makes the report's own prose free to describe rather than
reproduce: the bytes travel in their own channel, already scope-checked.

### Reading the result

`citations` is the server's provenance check on its own report, computed from
the spans the run's tools actually returned. Each cited location scores
`verified` (the run was shown that location), `path_only` (the run was shown
the file but not that line range) or `unverified` (no tool returned that path
during the run — the model produced it unaided). The three values state how
much of a claim's source the server could corroborate; they are not a claim
about whether the report is correct. A location that is `verified` has
already been checked against the run's own evidence, so checking it again by
re-reading the file is work the server has done. An `unverified` one has no
such backing, and the claims resting on it are the ones worth discounting.

Two fields decide what a zero means. `shown_paths` is the denominator: how
many files the run was shown the **inside** of. `verified: 0` over
`shown_paths: 12` is a report that cited none of what it read; over
`shown_paths: 0` it is the honest "nothing in this scope was shown to me",
which is a different statement entirely. `hearsay_only: true` marks the case
where the run held prior reports or a challenge subject but looked at nothing
itself — the one shape in which uncited prose is somebody else's answer
restated. `server_written: true` means the server assembled the report from
banked findings rather than a model writing it; such a report cites nothing
by construction, so `verified: 0` on it measures nothing about the model.
`stale` marks citations whose file was reindexed mid-run: the claim usually
still holds, the line range may not.

`done` carries `reason`. `finalized` is a natural finish — the run decided it
had enough. The `*_exhausted` reasons mean a budget stopped it: the report is
real but bounded, and the same question at a higher effort, or a follow-up
chained to this run, is what recovers the rest.

Cost is published in two forms, and they answer different questions. The
effort ladder says what a level **grants**; `research.observed` says what runs
at that level have actually **taken** lately (measured p50/p90 per model and
effort). The longest possible wait is the level's `worst_case_seconds`.

### Concurrency

Slots are few — see `max_concurrent` in the live section below; on a
single-GPU host it is usually 1. A 429 with code `research.busy` means every
slot is held, and its `detail` names `/research/active`.
`GET /research/active` lists the holders oldest first, so a suspected wedge
sorts to the top. `DELETE /research/active/{run_id}` cancels a named run and
frees its slot; it exists for the case where a caller abandoned a run while
its socket stayed open.

### Chaining

`context_run_ids` names prior stored runs whose reports are injected into a
new run as hearsay — material it may read but may not cite, since anything it
cites it re-derives through its own tools. A chained follow-up therefore
starts from the earlier run's conclusions instead of from cold, which is
measurably cheaper than re-deriving the same ground, and it records the
lineage: transitive ancestry is stored, so deleting an ancestor marks the
descendants invalid rather than silently leaving them standing.

## Challenge — adversarial verification

`POST /v0/{project_guid}/research/{run_id}/challenge` runs a second
investigation whose subject is a stored report: same loop, same budgets, its
own citation gate. The subject is injected as hearsay under examination and
never seeds the evidence — re-deriving every location through the tools is
what the refutation consists of. `?stream=` means what it means on
`POST /v0/{project_guid}/research`, and the answer is identical plus one thing: `verdict`,
carrying per-claim `CONFIRMED` / `DISPUTED` / `REFUTED` and an `overall` — a
field of the JSON body, an event before `done` on a stream.

Two properties of `overall` govern how it reads:

- `null` is a distinct value, not a quiet `CONFIRMED`. It means the verdict
  turn produced no parseable verdict lines, so the challenge scored nothing.
  It does not aggregate into `trust`, and it is not an acquittal.
- The verdict is capped by the challenge's own grounding (`verified > 0` and
  `unverified <= verified`). An ungrounded `REFUTED` is emitted as
  `DISPUTED`, and an ungrounded `CONFIRMED` resolves to `null`: a challenge
  that was shown nothing can raise a doubt, but it can settle one in neither
  direction. `DISPUTED` passes through either way.

Every stored run carries a derived `trust` aggregated over *valid* challenges
only, severity first: `refuted` > `disputed` > `confirmed` > `unchallenged`.
A `refuted` report is one whose claims a second investigation contradicted,
which is a different standing from one nobody has examined.

## Stored research

Every finished run is journalled and browsable, which is what makes the
corpus worth accumulating rather than a stream of one-off answers.

- `GET /projects/{project_guid}/research` — keyset-paged summaries (cursor
  `seq`), searchable across question and report text. Each summary carries
  `valid`/`invalid_reason` (a run goes invalid when the files it was shown
  have changed, or an ancestor was deleted), `trust`, citation counts, and
  reference counts in both directions. Filters apply before the page limit,
  so a short page means there is no more.
- `GET /projects/{project_guid}/research/{run_id}` — the full report plus its
  metadata.
- `GET /projects/{project_guid}/research/{run_id}/verification` — offline
  re-verification: provenance recomputed from the journal with no model
  involved, and staleness recomputed against the index as of now. The two are
  reported separately because they answer different questions — provenance is
  immutable, staleness is what moves.
- `POST /projects/{project_guid}/research/{run_id}/pin` — exempt a run from
  retention. `{}` pins; `{"pinned": false}` unpins.
- `DELETE /projects/{project_guid}/research` — batch delete by ids.
  `referenced_by_count` is what makes a deletion decision informed: removing
  a run invalidates every run built on it.

## Search and symbols

`POST /v0/{project_guid}/search` is hybrid semantic + lexical retrieval over
a project's indexed chunks. Body: `{"query": "...", "top_k": 5}` plus optional
`include`/`exclude` filters, each `{"paths": [globs], "programming_languages":
[names]}`. Results come back scored, best first, with file path, line span and
the chunk's code. A 404 with `search.no_match` is this endpoint's empty
result, distinct from a missing project.

Measured retrieval property: queries containing real identifiers rank
implementation chunks first, while purely natural-language queries rank tests
and documentation first. An identifier in the query text is what moves the
ranking.

`POST /v0/{project_guid}/symbols` is exact-name lookup over **definitions**,
returning ranked candidates plus full totals. Name collisions are part of the
contract, so the answer is a candidate list rather than a single "the" match,
and an empty list is definitive: the name is defined nowhere in the index.
It does not answer who *uses* a name — nothing here resolves references, and
that question is lexical, which `grep` answers and says so. The body rejects
unknown fields, so a `role` filter from an older client is a 400 rather than
a plausible wrong answer.

Several searches in a row to piece something together is an investigation,
and an investigation is what `POST /v0/{project_guid}/research` does in one
request on hardware that is not billed per token.

## Health

`GET /health` reports the readiness of the stores, the embedder and Ollama,
plus research slot occupancy. HTTP is always 200; the verdict is the `status`
field, which the server computes so that every client reads one word rather
than deriving it from `checks`:

- `ok` — everything required and optional is answering.
- `degraded` — only the **optional** Ollama is failing. Search and the stored
  corpus still answer; `POST /v0/{project_guid}/research` does not.
- `unhealthy` — a required dependency failed (SQLite, Qdrant, the embedder,
  and the query embedder when present), or a research run is past its wedge
  deadline.

Severity wins, so Ollama down together with Qdrant down is `unhealthy`. Each
entry in `checks` is exactly `"ok"` or `"error"`; servers predating that
contract spell the failing value `"error: <reason>"`, so equality against
`"ok"` is the only comparison stable across versions. A *busy* research slot
is not a degradation; a *wedged* one is.
