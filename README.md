# MINDex — a *mindful* index

**A coding agent should not read your codebase. It should ask.**

A frontier model's context is the expensive resource, and reading files burns it. mindex
is a local-first code index built around that one economics problem. Everything below
runs on your machine: vectors in a local Qdrant, metadata in a local SQLite file,
embeddings from a local [BGE-M3](https://huggingface.co/BAAI/bge-m3) (~4–6 GB VRAM, or
CPU-only). 21 programming languages, plus TOML, YAML and Markdown.

**Search and research are one pair, and the split is the product.**

- **Search** is the paid-precision half: you spend a few chunks and get the byte-exact
  text of one place. Retrieval is three-level and uses all of BGE-M3's heads as they
  come — dense and SPLADE-style sparse run in parallel, RRF-fuse, and a ColBERT
  multivector rerank orders what survives. No head is a fallback for another; each
  catches what the others miss.
- **Research** is the cheap-breadth half, and it costs the caller nothing. You ask a
  question; a **local** model runs the whole investigation — searching, reading code,
  looking up definitions, walking git history — and hands back a cited Markdown report.
  The code it read never enters your model's context. You pay wall-clock time on your
  own hardware and nothing else.

**Every citation is checked before you see it.** A `path:start-end` in a report is
scored against what the run's own tools actually returned — `verified`, `path_only` or
`unverified` — so a location the model invented is labelled rather than trusted. A
report that cites nothing is refused outright.

**Reports are kept, and they can be argued with.** Finished runs form a corpus you can
browse, feed to later questions as prior context, and prune. Validity is derived, never
stored: a report whose files have since moved says so instead of being quietly trusted.
Any report can be **challenged** — a second run whose subject is the first one, which
re-derives every claim through the tools and returns `confirmed` / `disputed` /
`refuted`. And any report can be **re-verified offline**, with no model and no GPU, by
re-scoring its citations against the index as it stands now.

**Reindexing is close to free.** A file is skipped by content hash *and* by the version
of the code that derived its chunks and symbols, so an unchanged tree costs one round
trip. When only the symbol extractor moves, `mindex-index --symbols-only` rebuilds the
symbol table with no slicing, no GPU and no vector writes — measured at ~20× faster than
a full pass on this repository.

**Exact lookup, honestly scoped.** `symbols` answers "where is X **defined**" from a
tree-sitter symbol table built at index time, returning ranked candidates and full
totals, because a name can legitimately live in several places. "Who *mentions* this
name" is `grep`'s question — lexical, and it says so — rather than a call graph the
index cannot honestly claim to have.

**The intended way to run it is as an agent's tool over [MCP](https://modelcontextprotocol.io)**
→ [`tools/mcp/mindex`](tools/mcp/mindex/README.md) (search, symbols, live reindex) and
[`tools/mcp/scout`](tools/mcp/scout/README.md) (research, challenge). A running server
also serves **`/llms.txt`** — the whole workflow as one document, so pointing an agent
at that URL is enough to get it started — and **`/.well-known/mindex.json`**, the same
thing as data: identity, endpoint inventory and the live `/config` snapshot in one JSON
document. Two channels rather than one because some agent harnesses classify a fetched
document that addresses the model as a prompt injection and refuse to read it; JSON has
no register to object to, so discovery still works where the prose does not. A terminal frontend and a
[VS Code extension](tools/vscode/README.md) drive the same API for humans; the extension
ships as a `.vsix` on the
[releases page](https://github.com/silencespeakstruth/mindex/releases), so
`code --install-extension mindex-vscode-1.2.0.vsix` is the whole of installing it.

## Install

**Prebuilt**, from the [releases page](https://github.com/silencespeakstruth/mindex/releases):
`mindex-index` and `mindex-watch` for Linux, Windows and macOS (Intel and Apple
silicon), the `mindex` server for Linux x86-64, and the VS Code `.vsix`. Unpack and put
the binaries on `PATH`; each archive carries a `SHA256SUMS`.

**From source:**

```sh
cargo install --locked --path .              # mindex (server)
cargo install --locked --path tools/indexer  # mindex-index
cargo install --locked --path tools/watcher  # mindex-watch (optional daemon)
ln -sf "$PWD/tools/search/mindex-search.sh" ~/.cargo/bin/mindex-search
```

`~/.cargo/bin` must be on `PATH`. Needs rustup (toolchain auto-installs from
`rust-toolchain.toml`) and the usual native build deps (`cc`/`clang`, `cmake`, `protoc`,
`pkg-config`); `mindex-search` also wants `jq` and, optionally, `pygmentize`.

**Platforms.** The server is developed, run and released on Linux; elsewhere, run it in
Docker rather than building it — it carries twenty tree-sitter grammars, a bundled
SQLite and protobuf codegen, none of which is exercised on another platform.
`mindex-index` is portable Rust and is released for all three. `mindex-watch` is
released for all three too, but its filesystem watching has only ever been exercised on
Linux inotify. The VS Code extension and both MCP servers are platform-agnostic. Two
further Linux-shaped things: `tools/search/mindex-search.sh` is bash and has no Windows
equivalent — use the extension or an MCP client instead — and config discovery follows
the XDG spec via `$XDG_CONFIG_HOME`/`$HOME`, which Windows does not set, so name the
file explicitly there with `--config` or `$MINDEX_CONFIG`.

## Run

Bottom-up: embedder → Qdrant → mindex.

```sh
# 1. Embedder — never in a container (torch is ~8 GB and wants the GPU directly).
cd embedder && uv sync && uv run python -m bge_m3_api --port 11211

# 2. Qdrant + mindex. No host ports by default; add the overlay to reach the API
#    from the host (both loopback-only).
docker compose -f docker-compose.yml -f docker-compose.exposed.yml up -d --build

# 3. Index a repo. .mindex at the repo root is the project id and its scope.
printf 'guid: %s\nexclude_paths:\n  - target/**\n' "$(uuidgen)" > .mindex
mindex-index --root . --no-verify

# 4. Ask.
echo 'where is the auth token validated?' | \
    MINDEX_PROJECT="$(mindex-index --print-guid)" mindex-search --no-verify
```

For research, mindex also needs a local [Ollama](https://ollama.com) with a model pulled
(a thinking model works best). Then wire it into your agent — that is the point:
**[`tools/mcp/mindex`](tools/mcp/mindex/README.md)** and
**[`tools/mcp/scout`](tools/mcp/scout/README.md)**.

## Typical configuration

`~/.config/mindex/config.toml` (XDG-discovered; CLI flags override it; every key is
optional — see [`config.example.toml`](config.example.toml) for the full set). Paths
are absolute; substitute your own:

```toml
[server]
cert_path = "/path/to/config/cert.pem"            # TLS secures the transport
key_path  = "/path/to/config/key.pem"

[auth]
enabled = false                                   # the default; with it off, TLS is
                                                  # the only protection and this is an
                                                  # internal service. Turn it on — and
                                                  # it is mandatory behind a gateway —
                                                  # and a bearer token then decides
                                                  # which projects and which actions
                                                  # each caller reaches.

[model]
server_url = "http://localhost:11211"             # the BGE-M3 embedder

[qdrant]
server_url = "http://localhost:6334"

[database]
path = "/path/to/data/mindex.db"                  # keep it out of the repo

[research]
ollama_url = "http://127.0.0.1:11434"
default_model = "glm-4.7-flash:latest"            # any local model
max_concurrent = 2                                # research runs on its own small pool
```

### `.mindex`

`.mindex` at a repo root ties that checkout to its index. It is YAML, committed, and
read by `mindex-index`, `mindex-watch`, the VS Code extension and the post-commit
hook — so the scope is defined once, not retyped per command:

```yaml
guid: c2d7e2c1-3165-42f5-9366-0ff1492b4bab   # required; dashless is accepted too
exclude_paths:                               # applied before include_paths
  - target/**
  - "**/node_modules/**"
include_paths: []                            # empty = no filter, not "nothing"
languages: []                                # lowercase mindex ids; empty = all
git_refs: [master]                           # which refs the history channel walks
```

An unknown key is an error rather than a silent no-op — a mistyped `exclude_path:`
would otherwise index the tree you meant to keep out. Globs are root-relative with
forward slashes; `*` stops at a directory separator, `**` crosses them. The
implementations (Rust `globset`, the extension's `picomatch`) are pinned to that
subset by a shared fixture table in `tools/mindexfile/src/lib.rs` and
`tools/vscode/src/globContract.test.ts`.

The MCP servers do **not** parse `.mindex`: the agent reads it and passes the GUID
and filters as call arguments.

### Git history

The working tree says what the code *is*; the commits say why it became that way.
`mindex-index --history` walks the refs named above and stores **commit metadata** —
subject, body, author, date, and which paths each commit touched — so a research run
can ask what changed in a file and why, and quote the sha.

It is opt-in and stays off until the flag is passed. Metadata only: no embeddings, no
vectors, no GPU time, because the questions worth asking of history ("what touched
this and why") are SQL questions rather than similarity ones. `--history-only` runs
that phase alone, which is what the post-commit hook uses.

```bash
mindex-index --root . --history          # index the tree and reconcile commits
mindex-index --root . --history-only     # commits only, no slicing or embedding
```

Reconciliation is a **set replace**, not an append: a sha is the hash of its own
content, so a force-push or a rebase is one ordinary sync rather than a special case.
Retention is separate — `DELETE /v0/{guid}/history` with `keep_last` and `older_than`,
intersected — because a sync only drops what your refs no longer reach.

## What else you should know

- **Authorization is opt-in and off by default.** Without it, TLS is the only
  transport security and mindex answers every caller that can reach the port — fine
  for a trusted local machine, and the reason an unconfigured deployment must not
  be exposed. Turning on `[auth]` makes the server mint and verify its own bearer
  tokens: each names the projects it reaches and the actions it permits
  (`search`, `research`, `index`, `delete`, `admin`, `mint`), and the check is one
  HMAC with no server-side session state. Issue one with `mindex mint-token`.

  Every client sends the same header, `Authorization: Bearer` — `--token` for the
  CLI tools, `$MINDEX_TOKEN` for all of them, `$MINDEX_TOKEN_FILE` naming a 0600
  file for anything configured through an environment block (an MCP server list),
  `token` in `indexer.toml`/`watcher.toml`, a `credentials.toml` entry per server
  URL, and the OS keychain in VS Code (**MINDex: Set Bearer Token**, never a
  setting — Settings Sync would carry it between machines). Prefer the file or the
  environment: a flag value is visible in `ps`. **MINDex: Issue a Token for an
  Agent** derives a short-lived read-and-research token for the current project
  from the one the extension holds, so handing an agent a credential does not
  need a shell on the server's host; the server refuses anything the minting
  token does not already hold. Unset sends no header, so a direct
  `https://127.0.0.1:11111` connection to a server with `[auth]` off is unchanged.
  Full rationale and runbook: `docs/claude/auth.md`.
- **Certificates.** A CA the host already trusts needs nothing: every client reads
  the OS trust store, which is where mkcert and corporate roots install themselves.
  A CA that is *not* installed there is named explicitly — `--ca-cert` for the CLI
  tools, `MINDEX_CACERT` for the MCP servers and `mindex-search.sh`, the
  `mindex.caCert` setting in VS Code. `--no-verify` / `MINDEX_NO_VERIFY` verifies
  nothing at all and exists for the self-signed certificate the container generates
  on first start, which no store can vouch for. Where both are set, the skip wins:
  in VS Code `mindex.noVerify` overrides `mindex.caCert`, and a `caCert` naming a
  file this machine does not have is a warning naming the path, not a dead
  extension. One consequence worth planning for: those settings exist only in *our*
  clients. A third-party agent driving the HTTP API has no `--ca-cert` and no
  `noVerify`, so a locally-issued certificate makes the server simply unreachable to
  it. Reaching mindex from a machine whose trust store you do not control wants a
  publicly-trusted certificate; where the listener has no inbound `80`, ACME **DNS-01**
  is the challenge that still works.
- **Everything is a knob, documented once.** `mindex --help` and each tool's `--help`;
  the full HTTP API with schemas is live at **`/swagger-ui`** (OpenAPI at
  `/api-docs/openapi.json`). Errors are RFC 7807 `problem+json` with stable machine
  `code`s — that is the client contract.
- **`GET /health` is tri-state, and the server owns the verdict**: `ok`; `degraded`,
  meaning only the *optional* Ollama is failing, which is exactly the state where a
  client should keep offering search and stop offering research; and `unhealthy`, a
  required dependency. Test `checks.*` for `== "ok"` rather than for a prefix. HTTP is
  always 200.
- **Observability.** OpenMetrics at `/metrics`, with a provisioned Grafana dashboard in
  [`deploy/grafana/`](deploy/grafana/). Two gauges are worth an alert on day one:
  `mindex_stale_collections` (a project holds chunks but its vector collection is
  missing or empty — its search is broken *now*) and `mindex_orphaned_collections`.
  Both are seeded at `-1`, never `0`, so an unreachable Qdrant cannot spell the healthy
  reading.
- **Architecture, invariants and conventions** live in
  [`.claude/CLAUDE.md`](.claude/CLAUDE.md), which also states each accepted limit
  next to the invariant it qualifies.
- **Tests:** `cargo test --bin mindex` (no services), and the full stack against a mock
  embedder:

  ```sh
  docker compose -f docker-compose.test.yml up --build \
      --exit-code-from test-runner --abort-on-container-exit
  ```

- **Why a custom embedder?** No off-the-shelf server (vLLM, Ollama, …) emits BGE-M3's
  three heads together. [`embedder/`](embedder/README.md) exists solely to bridge that
  and is meant to be deleted when one does.
- **Licence:** [MIT](LICENSE).
