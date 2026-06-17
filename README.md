# MINDex — a *mindful* index

A local-first semantic code search engine built around **[BAAI/bge-m3](https://huggingface.co/BAAI/bge-m3)**.

mindex is purpose-built for BGE-M3 and uses **all three of its heads as-is** — dense
embeddings, SPLADE-style sparse lexical weights, and ColBERT multi-vectors — to do
true hybrid retrieval (RRF fusion + ColBERT reranking) over your codebase. It is aimed
at **local use**, including cutting **token and context cost for expensive coding
agents**: hand the agent the few chunks that actually matter instead of stuffing whole
files into the prompt.

## Highlights

- **Three-head hybrid retrieval, no compromise.** Dense + sparse + ColBERT are combined
  exactly as BGE-M3 produces them — not just cosine over dense vectors.
- **Your code never leaves your machine.** Vectors live in a local Qdrant, metadata in
  a local SQLite file. Nothing is sent to a third party.
- **Cheap to run.** BGE-M3 is light: inference is near-instant even on CPU. The embedder
  fits comfortably on a modest GPU (~4–6 GB VRAM) and runs CPU-only if you have none.
- **Fast indexing of large codebases.** AST-aware chunking (tree-sitter) + batched,
  concurrent uploads. *(Concrete benchmarks are still TODO.)*
- **21 languages** out of the box (Rust, Python, TS/JS, Go, C/C++, Java, C#, SQL, …).

## How it works

```
            ┌─────────────┐   tree-sitter      ┌──────────────┐   3 heads     ┌──────────┐
  source ──▶│  mindex API │──► AST chunking ──▶ │  BGE-M3       │ ───────────▶ │  Qdrant  │  vectors
   files    │  (Rust,     │   (128–512 tok)    │  embedder     │  dense/      │          │
            │   HTTPS)    │                    │  (/encode)    │  sparse/     └──────────┘
            └─────────────┘                    └──────────────┘  colbert      ┌──────────┐
                  │                                                            │  SQLite  │  metadata
                  └──────── search: prefetch dense+sparse → RRF → ColBERT ────▶│          │
                                                              rerank → top-k   └──────────┘
```

Indexing is append-only; reindexed/deleted chunks are soft-deleted and swept by a
background GC. Project isolation is one Qdrant collection per project plus a SQLite-built
`has_id` filter.

## Components

| Piece | What it is |
|-------|-----------|
| **mindex** (`src/`) | The Rust async HTTPS server — the API below. |
| **embedder** (`embedder/`) | The BGE-M3 model server exposing all three heads over `/encode` + `/health`. Runs on the host (GPU) or in the cloud — see below. |
| **mindex-index** (`tools/indexer/`) | CLI that walks a directory tree and uploads files for indexing (`--concurrency`, glob include/exclude, live progress). |
| **mindex-search.sh** (`tools/search/`) | Terminal search frontend: a query in, syntax-highlighted matches out. Configurable by flags or `MINDEX_*` env vars. |

## Running

Three pieces talk to each other: **Qdrant**, the **embedder**, and the **mindex server**.
`docker-compose.yml` wires Qdrant + mindex together and is the **canonical reference for
the server's flags** — read it for the exact values. It is meant as an illustration more
than a prescription; you don't have to run mindex this way.

**1 — Start the embedder** (it is *not* in any image: torch alone is ~8 GB and it needs
direct GPU access, so it runs separately):

```sh
cd embedder
poetry install
poetry run python -m bge_m3_api --port 11211      # binds 0.0.0.0; ~4–6 GB VRAM, or CPU
```

> **No local GPU?** The embedder is a standalone HTTP service, so a natural use case is to
> deploy it to a cloud GPU and point mindex at it via `--model-server`. *(A deployment
> template for this is TODO.)*

**2 — Start Qdrant + mindex.** The compose file brings up Qdrant and the mindex server
(reaching the host embedder via `host.docker.internal:11211`):

```sh
docker compose up -d --build
```

mindex listens on `https://localhost:11111` (a self-signed cert is generated on first
start; mount real certs at `/certs` to override).

**3 — Index a codebase:**

```sh
PROJECT=$(uuidgen | tr -d -)
tools/indexer/target/release/mindex-index \
    --project "$PROJECT" --root /path/to/repo --no-verify \
    --include 'src/**/*.rs' --exclude '**/target/**'
```

**4 — Search:**

```sh
echo 'where do we validate the auth token?' \
    | MINDEX_PROJECT="$PROJECT" tools/search/mindex-search.sh --no-verify
# or open $EDITOR for a multi-line query:
MINDEX_PROJECT="$PROJECT" tools/search/mindex-search.sh --no-verify --edit
```

## HTTP API

All endpoints are HTTPS. TLS is the only transport security — there is **no API auth**
(mindex is meant for a trusted local network).

| Method & path | Purpose |
|---------------|---------|
| `POST /v0/{project}/index` | Index/reindex files (JSON: `{files: {lang: {path: {code}}}}`). |
| `POST /v0/{project}/search` | Hybrid search; returns top-k chunks with scores. |
| `GET /projects/{project}` | Stats: files by status, chunks per language. |
| `DELETE /projects/{project}` | Hard-delete a project (rows + Qdrant collection). |
| `DELETE /projects/{project}/files` | Soft-delete files by an include/exclude selector (body). |
| `POST /gc` | Run garbage collection synchronously. |

## Key configuration

Server flags (see `mindex --help` for the full set; `docker-compose.yml` for defaults in
context):

- `--bind` — listen address (default `127.0.0.1:11111`).
- `--model-server` — embedder URL (default `http://localhost:11211`).
- `--qdrant-server` — Qdrant gRPC URL (default `http://localhost:6334`).
- `--db-path` — SQLite metadata file.
- `--embed-batch` — chunks per `/encode` call (GPU-load lever; match the embedder's `--batch`).

## Why a custom embedder?

General-purpose model servers (vLLM, Ollama, …) return **only dense** embeddings — none
expose BGE-M3's sparse lexical weights and ColBERT token vectors together, which the
hybrid pipeline needs. `embedder/` exists **solely** to bridge that gap and is intended
to be **removed** once an off-the-shelf server emits all three heads. See
[`embedder/README.md`](embedder/README.md).

## Status & roadmap

Early but functional. Tracked deferrals live in [`TODO.md`](TODO.md); the headline ones:

- **Performance benchmarks** for large-codebase indexing — not measured yet.
- **A cloud-GPU deployment template** for the embedder.
- A few accepted limitations (no API auth, single embedding model at a time, the
  `has_id` filter's linear growth on very large projects).

## References

- **BGE-M3** — [BAAI/bge-m3 on Hugging Face](https://huggingface.co/BAAI/bge-m3)
- **Qdrant** — vector store ([qdrant.tech](https://qdrant.tech))
- **tree-sitter** — AST parsing for chunking ([tree-sitter.github.io](https://tree-sitter.github.io))
