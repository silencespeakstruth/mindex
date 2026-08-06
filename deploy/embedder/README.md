# deploy/embedder — serving the embedding model

mindex does not ship an embedder. It speaks the **OpenAI embeddings API** to
whatever serves the model named by `[model].id`, so llama.cpp, vLLM, TEI or a
hosted endpoint all work. This directory is the contract, plus three recipes and
the numbers that separate them.

Until retrieval v3 this repo carried a *vendored* embedding server
(`embedder/`, BGE-M3), and it existed for one reason: no general model server
returned dense, sparse and ColBERT vectors together. Dense-only retrieval
retired that reason and the server was deleted, as its own README had promised.
What is here instead is a **reference implementation of the standard contract**
(`server.py`, ~200 lines over sentence-transformers) — not a protocol, not a
dependency, and not something mindex knows about.

## The contract

Three endpoints, on one base URL (`[model].server_url`):

| endpoint | used by | requirement |
|---|---|---|
| `POST /v1/embeddings` | every index and every query | one row per input, **in request order**; each row's length must equal the registry `dim` (1024 for `qwen3-embedding-0.6b`) |
| `GET /v1/models` | startup handshake, `GET /health` | must list `[model].served_name` (default: the registry's HF repo id) |
| `GET /health` | `GET /health`'s embedder probe | any 2xx |

And two properties the wire cannot state, both of which mindex trusts blindly:

- **Last-token pooling.** Qwen3-Embedding is a causal LLM whose embedding is the
  last token's hidden state. Mean pooling also returns 1024 plausible numbers
  and simply retrieves worse — there is no error anywhere. **Verify it**
  (below).
- **L2 normalisation.** Scores are cosine; unnormalised vectors do not fail,
  they rank differently.

Startup **refuses** a server that answers `/v1/models` with a different model
than `[model].id` names, and every response row is checked against the registry
dimension. Neither check can see pooling, normalisation, or the precision the
weights are served at — see *What nothing checks*, at the end.

## Which recipe

Measured on the reference host (AMD Radeon AI PRO R9700, ROCm 7.2), same model,
same repository, end to end — `mindex-index --force` over this project, 4604
chunks:

| server | reindex | query latency | notes |
|---|---|---|---|
| **`server.py` (torch, bf16)** | **51 s** | ~16 ms | recipe A |
| llama.cpp b10221, Q8_0 | 410 s | ~30 ms | recipe B |
| llama.cpp b10221, F16 | 481 s | ~30 ms | |
| vLLM | not measured | | recipe C |

**Eight times, on the same card, for the same model.** That gap is why this
directory has a server in it at all, and it is not a configuration mistake:
llama.cpp was measured across `-np` 1/8/32, `--ubatch-size` 512/2048/8192, the
ROCm and Vulkan backends, and 1/4/8 concurrent HTTP clients — every combination
landed between 8.5 and 20.7 chunks/s, while `llama-bench` reports 24 400 tok/s
for the same weights, so even its own backend delivers ~3× what its server does
for many short sequences.

**Queries are unaffected either way** (16 ms against 30 ms, both far below the
Qdrant round trip), which is the shape of the trade: llama.cpp is a fine
*query* embedder and a poor *indexing* one. `[model].query_server_url` exists
precisely so the two paths can be different processes.

Take recipe A when reindexing matters — which is most of the time, because
"reindexing is close to free" is a property the whole design leans on. Take B
when the host already runs llama.cpp, indexes rarely, and you would rather not
maintain a python service. Take C if vLLM installs cleanly on your host; it is
the better-supported version of exactly what A does.

## Recipe A — `server.py` (torch)

```sh
cd deploy/embedder
python -m venv .venv
# torch first, from the vendor's index for your accelerator:
.venv/bin/pip install --index-url https://download.pytorch.org/whl/rocm7.0 torch
.venv/bin/pip install -r requirements.txt

# populate the model cache once, outside the unit (which runs offline):
.venv/bin/python -c "
from huggingface_hub import snapshot_download; snapshot_download('Qwen/Qwen3-Embedding-0.6B')"

cp embedder.env.example ~/.config/mindex/embedder.env    # then edit it
sudo cp mindex-embedder.service /etc/systemd/system/     # edit paths + user first
sudo systemctl enable --now mindex-embedder
```

```toml
[model]
id         = "qwen3-embedding-0.6b"
server_url = "http://127.0.0.1:11212"
```

Four decisions inside it are worth knowing, because each was a measured failure
first and none is visible from mindex:

- **bfloat16, not float16.** Qwen3 is trained in bf16, and in fp16 this model
  returned **NaN** rows for the longest chunks. mindex refused them (`invalid
  type: null, expected f32`), which is the lucky outcome — a NaN that reaches
  Qdrant scores NaN against every query, gets ranked last and counted in
  `search_unscorable_winners`, and reads as a ranking-quality complaint rather
  than a broken embedder.
- **Batches are formed by a token budget, not a text count.** Activation memory
  scales with rows × sequence length, and mindex's chunks are bimodal: code caps
  at ~364 tokens, documentation at 1024. A fixed count of 96 is 35k tokens on
  one pass and 98k on the next, and the second one OOMs. A group that OOMs
  anyway is halved and retried rather than failed.
- **No `empty_cache()`.** The obvious fix for torch's ever-growing allocator
  pool (measured: 20.4 GiB held by a model whose weights are 1.2 GiB) made this
  stack return NaN for the longest chunks — reproducible with the trim on, gone
  with it off. The pool is bounded where it is created instead, by the token
  budget. `MINDEX_EMBED_TRIM_MIB` re-enables it for a stack where you have
  verified the call is harmless.
- **One forward at a time.** mindex-index sends several requests concurrently by
  design; letting them into the model together buys nothing on one GPU and
  multiplies peak memory by the number of clients.

Steady-state cost on this host: **~4.5 GiB of VRAM** and one resident process.

## Recipe B — llama.cpp

Serving goes through [llama-swap](https://github.com/mostlygeek/llama-swap) if
the host already runs it, or a plain `llama-server`. Get the GGUF from upstream
(not an ollama blob — those carry ollama's own architecture strings):

```sh
huggingface-cli download Qwen/Qwen3-Embedding-0.6B-GGUF \
    Qwen3-Embedding-0.6B-Q8_0.gguf --local-dir /path/to/models

llama-server --port 12434 --host 127.0.0.1 --device ROCm0 \
    --model /path/to/models/Qwen3-Embedding-0.6B-Q8_0.gguf \
    --alias qwen3-embedding-0.6b \
    --embedding --pooling last \
    -np 8 -fa on -ngl 999 \
    --ctx-size 16384 --batch-size 4096 --ubatch-size 2048
```

```toml
[model]
id          = "qwen3-embedding-0.6b"
server_url  = "http://127.0.0.1:12434"
served_name = "qwen3-embedding-0.6b"   # the --alias, not the HF repo
```

Four flags are load-bearing:

- **`--embedding`** — without it `/v1/embeddings` answers 501.
- **`--pooling last`** — see the contract. Do not omit it and hope the GGUF
  metadata is right.
- **`--ubatch-size` ≥ the longest input.** A pooled embedding needs the whole
  sequence in one micro-batch; mindex's longest is
  `[slicer].max_doc_chunk_tokens` (1024).
- **`--ctx-size` / `-np`**: each slot gets `ctx / np`, and a request longer than
  one slot is a **400**, not a truncation.

**Q8_0 rather than F16**, which inverts the usual advice about quantizing an
embedder: llama.cpp's HIP MMQ kernels carry Q8_0 and its F16 path on gfx1201
does not — 2.7× on the backend (24 400 against 8 900 tok/s), +17% end to end.
The quality cost, measured against the F16 vectors on 64 real chunks: cosine
min 0.99892, median 0.99948 — the same order as the gap between llama.cpp F16
and `sentence-transformers` fp32 (0.9989).

Under llama-swap, two settings beyond the model entry: `ttl: 0` so the embedder
is never unloaded (it answers interactive requests), and a `routing` group with
`persistent: true, swap: false, exclusive: false`, without which every search
evicts whatever chat model is loaded — two model loads per question.

## Recipe C — vLLM

Heavier to install on ROCm (no PyPI wheel: `aur/python-vllm-rocm` builds from
source, or use the `rocm/vllm` image). Not measured here.

```sh
vllm serve Qwen/Qwen3-Embedding-0.6B --task embed --port 11212 \
    --dtype bfloat16 --max-model-len 8192
```

Two traps: the flag spelling for embedding models moves between vLLM versions
(`--task embed`, `--runner pooling`), and the pooler must be confirmed — if the
journal does not show last-token pooling with normalisation, force it with
`--override-pooler-config '{"pooling_type":"LAST","normalize":true}'`.

## Verify — three checks, in this order

**1. It is the right model.** Start mindex and read the log:

```text
INFO mindex: Embedder handshake ok., role: "index", model: qwen3-embedding-0.6b
```

A server that answers with a different id refuses startup by design.

**2. The pooler is right.** This is the check that catches a silent halving of
retrieval quality, and it takes one minute:

```python
import json, urllib.request
from sentence_transformers import SentenceTransformer

TEXTS = [
    "Instruct: Given a description of desired functionality, retrieve the "
    "source code that implements it\nQuery: how is the api token validated",
    "fn verify(token: &str) -> Result<Claims> { /* hmac check */ }",
]

ref = SentenceTransformer("Qwen/Qwen3-Embedding-0.6B", device="cpu").encode(
    TEXTS, normalize_embeddings=True
)

req = urllib.request.Request(
    "http://127.0.0.1:11212/v1/embeddings",
    data=json.dumps({"model": "qwen3-embedding-0.6b", "input": TEXTS}).encode(),
    headers={"Content-Type": "application/json"},
)
got = [r["embedding"] for r in json.load(urllib.request.urlopen(req))["data"]]

for i in range(len(TEXTS)):
    print(sum(a * b for a, b in zip(ref[i], got[i])))  # expect ~0.999
```

Measured this way: recipe A ~0.999 against the reference, recipe B (Q8_0)
0.9989/1.0005/1.0010, and A against B 0.99850/0.99959 — three implementations
of the same vector space. Anything below ~0.99 means the pooling type or the
normalisation differs; fix the server, not mindex. (Values a hair above 1.0 are
rounding in the dot product of two unit vectors, not an error.)

**3. Retrieval works.** Index one project and search it. The collection should
be `{guid}_{slug}_v3` with a single `dense` vector of the registry width, and
`mindex_search_unscorable_winners` must stay at **0** — it counts NaN scores,
which is what a wrong dtype or a split-precision deployment produces.

## What nothing checks

- **The precision the weights are served at.** `[model].id` names the *model*,
  not its quantization, so switching bf16 ↔ Q8_0 invalidates no stored vector
  and triggers no re-embed. If you change it and care, `mindex-index --force`.
- **That two instances match.** A split deployment whose index and query sides
  differ in precision (or pooling) answers every check identically and presents
  as "search sometimes cannot find the obvious thing".
- **Pooling and normalisation**, per the contract above. Hence check 2.
