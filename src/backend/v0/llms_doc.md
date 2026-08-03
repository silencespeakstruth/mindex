# mindex — how to use this server (for AI agents)

You have been handed a URL to a **mindex** server: a local semantic code index
with a built-in research agent. It indexes source trees (tree-sitter AST
chunking, BGE-M3 multi-vector embeddings, hybrid dense+sparse+ColBERT
retrieval) and can run whole cited investigations over them on a local model.
This document is the complete workflow; read it once and you can drive the
server with plain HTTPS requests. Full request/response schemas live at
`/api-docs/openapi.json` (interactive at `/swagger-ui`) — you rarely need
them beyond what is written here.

Ground rules:

- Internal service: TLS is the only transport security, there is **no
  authentication**. The certificate may be locally issued — if your HTTP
  client refuses it, that is a trust-store problem, not a server error.
- Every non-2xx response is RFC 7807 `application/problem+json` with a
  stable machine `code` (e.g. `validation.top_k_out_of_range`,
  `research.busy`). Branch on `code`, not on the prose.
- **You do not index.** Indexing, reindexing and drift detection belong to
  repo-side clients (`mindex-index`, the file watcher, the VS Code
  extension, the MCP tools). You read what they indexed.

## Bootstrap

1. `GET /projects` — every indexed project with its `guid`. Pick your
   project; the `guid` goes into every data-plane path below.
2. `GET /config` — live inventory: supported languages, search bounds, the
   research model list, effort ladder and measured run costs. Two fields are
   worker-refreshed (`research.models`, `research.observed`) — re-read this
   endpoint rather than caching it once. A machine-readable snapshot of the
   same numbers is appended to this document under "Live configuration".
3. `GET /projects/{project_guid}` — per-project stats: which languages have
   active chunks (only those are searchable), file counts, failure counts.

## Search

`POST /v0/{project_guid}/search` — hybrid semantic + lexical retrieval over
the project's indexed chunks.

Body: `{"query": "...", "top_k": 5}` plus optional `include`/`exclude`
filters, each `{"paths": [globs], "programming_languages": [names]}`.
Results come back scored, best first, with file path, line span and the
chunk's code. **404 means "no match", not an error.**

One measured retrieval hint: queries carrying real identifiers retrieve
implementations; pure natural-language queries tend to retrieve tests and
docs. If you know a symbol name, put it in the query.

`POST /v0/{project_guid}/symbols` — exact-name lookup over **definitions**,
returning ranked candidates plus full totals. Name collisions are part of
the contract: you get candidates, never a single "the" answer. An empty
result is a definitive "not defined in the index". It does not answer "who
uses this name" — that is a lexical question, and `grep` answers it and says
so. The body rejects unknown fields, so a `role` filter from an older client
is a `400` rather than a plausible wrong answer.

## Research — the core feature

`POST /v0/{project_guid}/research` starts a full investigation on the
server's local model: it plans, loops over internal tools (search, grep,
symbols, outline, list_files, read_chunks, file history, prior research),
and streams back a cited Markdown report. You ask one substantial question;
the server does the reading.

Request: `{"question": "..."}` plus optional `model` (must be in
`research.models` from `GET /config`), `effort` (`low` / `medium` / `high`),
`budget` (per-axis overrides, capped by the published ceilings),
`include`/`exclude` scope filters, `seed`, and `context_run_ids` (see
chaining below). The documentation corpus is English — phrase questions
about documentation in English.

The response is a **one-way SSE stream**; each `data:` line is one JSON
frame. Event order: `started` (carries `run_id`) → any number of `step`
(each tool call, with the file spans it landed on) and `progress` (spent vs
granted per budget axis; `binding` names the axis with the largest share
spent — a maximum, not a warning; `shares` gives all four percentages) →
the report text → `summary` → `citations` → `excerpts` (only when something
verified) → `done`. Disconnecting cancels the run.

How to read the result:

- `citations` is the server's own provenance check on the report:
  `verified` (the model was actually shown that location), `path_only` (the
  file yes, that line range no), `unverified` (the run never saw that path —
  the model invented it). Trust verified claims; discount unverified ones.
  `stale` marks citations whose file changed mid-run.
  `server_written: true` means the server assembled the report from banked
  findings (forced synthesis) — such a report cites nothing by construction,
  so `verified: 0` on it is not evidence of fabrication.
- `done` carries `reason` (`finalized` is a natural finish; the
  `*_exhausted` reasons mean a budget stopped it — the report is still
  real, just bounded), plus the stored `run_id`/`seq` (null = the journal
  write failed and the run is not saved).
- Cost: an effort level's grant is in the ladder; what a run actually
  takes is in `research.observed` (measured p50/p90 per model+effort).
  The longest possible wait is the level's `worst_case_seconds`.

Concurrency: slots are few (see `max_concurrent` below; on single-GPU hosts
it is usually 1). A 429 with code `research.busy` means every slot is taken —
inspect `GET /research/active` (oldest first; a suspected wedge sorts to the
top) and, if a run was abandoned by its caller, cancel it with
`DELETE /research/active/{run_id}`.

**Chain, don't re-ask.** A follow-up question should name prior runs in
`context_run_ids`: their reports are injected as context (as hearsay — the
new run still re-verifies anything it cites). This is much cheaper than
re-deriving the same ground from cold.

## Challenge — adversarial verification

`POST /v0/{project_guid}/research/{run_id}/challenge` runs a second
investigation whose subject is a stored report: same loop, same budgets, its
own citation gate. The stream is identical plus one extra event before
`done`: `verdict` — per-claim `CONFIRMED` / `DISPUTED` / `REFUTED` and an
`overall` verdict.

Reading rules, both load-bearing:

- **Inconclusive is not an acquittal.** A null verdict means the challenge
  could not score the claims, not that they survived.
- **An ungrounded challenge can dispute but never refute.** If the
  challenge's own report verified zero citations, its verdict is capped at
  `disputed` — an unshown accusation refutes nothing.

Every stored run carries a derived `trust` field aggregated from valid
challenges: `refuted` > `disputed` > `confirmed` > `unchallenged` (severity
wins). A refuted report must not be read as settled knowledge.

## Stored research

Every finished run is journalled and browsable:

- `GET /projects/{project_guid}/research` — keyset-paged summaries (cursor
  `seq`), searchable by question and report text. Each summary carries
  `valid`/`invalid_reason` (a run goes invalid when the files it was shown
  have changed, or an ancestor run was deleted), `trust`, citation counts,
  and reference counts in both directions.
- `GET /projects/{project_guid}/research/{run_id}` — the full report plus
  metadata.
- `GET /projects/{project_guid}/research/{run_id}/verification` — offline
  re-verification: provenance re-checked from the journal (no model
  involved) and staleness recomputed against the index as of now.
- `POST /projects/{project_guid}/research/{run_id}/pin` — exempt a run from
  retention (`{}` pins; `{"pinned": false}` unpins).
- `DELETE /projects/{project_guid}/research` — batch delete by ids. Check
  `referenced_by_count` first: deleting a run invalidates every run built
  on it.

## Health

`GET /health` — readiness of the stores, the embedder and Ollama, plus
research slot occupancy. HTTP is always 200; read `status`, which the server
computes so you do not have to: `ok`; `degraded`, meaning only the
**optional** Ollama is failing — keep offering search, stop offering
research; `unhealthy`, meaning a required dependency failed or a run is
wedged. Severity wins, so Ollama down *and* Qdrant down is `unhealthy`.
Each entry in `checks` is exactly `"ok"` or `"error"` — test `== "ok"`,
never a prefix, since an older server spells it `"error: <reason>"`. A busy
research slot is never a degradation; a wedged one is.
