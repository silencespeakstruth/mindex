#!/usr/bin/env python3
"""Family F5: a different embedding model, scored on the same corpus.

THE QUESTION. The CoIR paper puts BGE-M3 at 39.31 average nDCG@10 on code
retrieval against 60.1 for CodeRankEmbed — a 137M-parameter, MIT-licensed model
that also beats Voyage-Code-002 (56.26) and a 1.3B CodeSage. If even half of
that transfers to this corpus, the embedder is a far larger lever than anything
in F2 or F3, and the ColBERT question becomes a footnote.

WHY IT TOUCHES NO PART OF MINDEX. The chunks are read out of mindex's own
SQLite, embedded here, ranked here, and written in the same result schema the
rest of the harness consumes. No `VECTOR_DIM` change, no
`COLLECTION_SCHEMA_VERSION` bump, no Qdrant collection, no server restart. If
the model does not win here, none of the integration work is worth planning.

RANKING IS EXACT, ON BOTH SIDES, AND THAT IS THE POINT. Qdrant searches 26 000
points through an HNSW graph, which is approximate; a brute-force matmul is
not. Comparing an exact new model against an approximate incumbent would credit
the new model for the incumbent's recall loss. So `--baseline-from-qdrant`
pulls the STORED BGE-M3 dense vectors back out of the collection and ranks them
by the same exact matmul, in the same script. The two arms then differ in the
model and in nothing else.

THE COMPARISON THAT MEANS SOMETHING is dense-against-dense. CodeRankEmbed has
one head; mindex's deployed pipeline has three plus a rerank. Putting this
model's raw dense score against the full pipeline would flatter whichever side
one wanted — the honest baseline is `F2-dense-only`, or the exact re-ranking
this script produces from the same stored vectors.

THE QUERY PREFIX IS NOT OPTIONAL. CodeRankEmbed was trained with
"Represent this query for searching relevant code" on queries and nothing on
documents. Omitting it degrades silently — no error, just worse numbers — so it
is a required argument with no default that could be quietly wrong, and it is
recorded in every output row's provenance.
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from bm25_fts5 import TOP_K, glob_to_regex
from build_qrels import load_config, repo_root

# Registry of what each model needs. A model whose prompting convention is not
# recorded here cannot be run: guessing it is the one failure mode that looks
# like an honest bad result.
MODELS = {
    "nomic-ai/CodeRankEmbed": {
        "query_prefix": "Represent this query for searching relevant code",
        "doc_prefix": "",
        "trust_remote_code": True,
        # NOT the model's 8192: attention is O(seq^2) and materialises
        # `batch x heads x seq x seq` as ONE tensor. At batch 128 that asked
        # for 3.17 GiB and OOM'd a 32 GiB card — and it is the same shape
        # that would meet the iGPU's 4 GiB per-allocation ceiling. The
        # slicer caps a chunk at 512 tokens, so 1024 is slack, not a limit.
        "max_seq_length": 1024,
    },
    "BAAI/bge-m3": {
        "query_prefix": "",
        "doc_prefix": "",
        "trust_remote_code": False,
        "max_seq_length": 8192,
    },
    # Apache-2.0, 1024-d (Matryoshka 32..1024), 32k context. In the arm because
    # CORE-Bench (arXiv 2606.11864) has this generic model BEATING the
    # code-specialised CodeRankEmbed at repo scale — L2 issue->edit 17.0 against
    # 12.1, L3 32.6 against 22.5 — while CodeRankEmbed leads it on CoIR. Two
    # benchmarks disagreeing about which model is better for code is exactly the
    # question this corpus is here to arbitrate.
    "Qwen/Qwen3-Embedding-0.6B": {
        # Not a decorative prefix: the model is instruction-tuned and its card
        # specifies "Instruct: {task}\nQuery: {query}" on the query side and
        # NOTHING on the document side. The task sentence is the one degree of
        # freedom, and it is written here rather than passed at a call site so
        # every run records the same one.
        "query_prefix": (
            "Instruct: Given a description of desired functionality, retrieve "
            "the source code that implements it\nQuery: "
        ),
        "doc_prefix": "",
        "trust_remote_code": False,
        "max_seq_length": 1024,
    },
    # Apache-2.0, 149M, 768-d, 8192 context, no prefix of any kind. Its card
    # reports CoIR 55.3 — between BGE-M3's 39.31 and CodeRankEmbed's 60.1 — so
    # it is the arm that says whether the gain is "a code model" or merely "a
    # 2026 model", which the archive currently cannot separate.
    "ibm-granite/granite-embedding-english-r2": {
        "query_prefix": "",
        "doc_prefix": "",
        "trust_remote_code": False,
        "max_seq_length": 1024,
    },
    # The multilingual sibling, Apache-2.0, 311M. Present for one reason the
    # English arm cannot cover: this index is queried in Russian as well as
    # English, and every code-specialised candidate is English-only. If a code
    # model wins on English and loses the multilingual arm, that is a routing
    # question, not a tie-break.
    "ibm-granite/granite-embedding-311m-multilingual-r2": {
        "query_prefix": "",
        "doc_prefix": "",
        "trust_remote_code": False,
        "max_seq_length": 1024,
    },
}


def resolve_device(requested: str, torch) -> str:
    """The accelerator, named rather than guessed.

    THIS EXISTS BECAUSE A SILENT CPU FALLBACK ALREADY COST A DECISION. The
    cross-encoder arm ran its whole smoke test on CPU — `torch.cuda.is_available()`
    is False in the XPU venv — and reported 6.6 s per query, a number that says
    nothing about the model and everything about where it ran. A latency figure
    from the wrong device is worse than none, because it looks like a result.

    This host has two accelerators reached by two different torch builds: the
    ROCm venv answers `torch.cuda.is_available()` (AMD presents as `cuda`), the
    XPU venv answers `torch.xpu.is_available()`. So `auto` picks whichever this
    interpreter actually has, and `cpu` has to be asked for by name — it is a
    legitimate choice for a quality-only arm and never an accident.
    """
    have_cuda = torch.cuda.is_available()
    have_xpu = hasattr(torch, "xpu") and torch.xpu.is_available()
    if requested == "auto":
        if have_cuda:
            return "cuda"
        if have_xpu:
            return "xpu"
        raise SystemExit(
            "no accelerator: this interpreter has neither CUDA/ROCm nor XPU. "
            "Run under embedder/.venv-egpu or .venv-igpu, or pass --device cpu "
            "explicitly and accept that no timing from this run is comparable."
        )
    if requested == "cuda" and not have_cuda:
        raise SystemExit("--device cuda, but torch.cuda.is_available() is False")
    if requested == "xpu" and not have_xpu:
        raise SystemExit("--device xpu, but torch.xpu.is_available() is False")
    return requested


def load_chunks(db: Path, guid: str, exclude: list[str]):
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT qdrant_guid, file_path, code, start_line, end_line "
        "FROM project_file_chunks WHERE project_guid = ? AND status = 'active' "
        "ORDER BY qdrant_guid",
        (guid.replace("-", ""),),
    ).fetchall()
    conn.close()
    if exclude:
        pats = [re.compile(glob_to_regex(g)) for g in exclude]
        rows = [r for r in rows if not any(p.match(r[1]) for p in pats)]
    return rows


def main() -> int:
    root = repo_root()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--qrels-suffix", default="-docs-short")
    ap.add_argument("--model", default="nomic-ai/CodeRankEmbed")
    ap.add_argument("--label", default=None)
    ap.add_argument("--mindex-label", default="baseline")
    ap.add_argument("--batch-size", type=int, default=64)
    ap.add_argument(
        "--db",
        type=Path,
        default=Path.home() / ".local/share/mindex-bench/mindex-bench.db",
    )
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument(
        "--baseline-from-qdrant",
        action="store_true",
        help="rank the STORED BGE-M3 dense vectors by the same exact matmul "
        "instead of embedding anything. This is the arm the new model must be "
        "compared against: Qdrant's own search is HNSW and therefore "
        "approximate, so scoring an exact new model against an approximate "
        "incumbent would credit the newcomer with the incumbent's recall loss.",
    )
    ap.add_argument("--qdrant", default="http://127.0.0.1:6333")
    ap.add_argument(
        "--dtype",
        choices=["float32", "float16", "bfloat16"],
        default="float16",
        help="CodeRankEmbed's config says float32, which is NOT what it should "
        "be measured at: mindex runs BGE-M3 in fp16, so an fp32 arm reports a "
        "throughput number that says more about the harness than the model. "
        "Quality is checked at both, because fp16 is a claim about the model "
        "and not a free lunch.",
    )
    ap.add_argument(
        "--device",
        choices=["auto", "cuda", "xpu", "cpu"],
        default="auto",
        help="`auto` refuses to fall back to CPU. See resolve_device().",
    )
    args = ap.parse_args()

    if args.baseline_from_qdrant:
        args.model = "BAAI/bge-m3"
    if args.model not in MODELS:
        raise SystemExit(
            f"{args.model} has no entry in MODELS. Its query/document prompting "
            f"convention has to be recorded before it can be run — guessing it "
            f"produces a result that looks like an honest loss."
        )
    spec = MODELS[args.model]

    from run import project_guid

    config = load_config(args.config)
    corpus = next(c for c in config["corpus"] if c["name"] == args.corpus)
    guid = project_guid(args.mindex_label, args.corpus)
    exclude = corpus.get("search_exclude_paths", [])

    rows = load_chunks(args.db, guid, exclude)
    if not rows:
        raise SystemExit(f"no active chunks for {args.corpus} ({guid}) in {args.db}")
    print(
        f"  {len(rows)} chunks, {len({r[1] for r in rows})} files, excluding {exclude}"
    )

    qrels = (
        root
        / config["run"]["data_dir"]
        / "qrels"
        / f"{args.corpus}{args.qrels_suffix}.jsonl"
    )
    instances = [json.loads(line) for line in qrels.open()]
    if args.limit:
        instances = instances[: args.limit]

    import numpy as np
    import torch
    from sentence_transformers import SentenceTransformer

    device = resolve_device(args.device, torch)
    if args.baseline_from_qdrant:
        # The document side is already computed and stored — fetching it costs
        # no GPU and, more importantly, guarantees these are the SAME vectors
        # mindex searches rather than a re-embedding that might differ.
        import httpx

        print("  fetching the stored BGE-M3 dense vectors from Qdrant ...", flush=True)
        started = time.perf_counter()
        coll = f"{guid.replace('-', '')}_v2"
        by_id: dict[str, list[float]] = {}
        offset = None
        with httpx.Client(timeout=300) as cl:
            while True:
                body = {"limit": 4096, "with_vector": ["dense"], "with_payload": False}
                if offset is not None:
                    body["offset"] = offset
                r = cl.post(
                    f"{args.qdrant}/collections/{coll}/points/scroll", json=body
                ).json()["result"]
                for p in r["points"]:
                    by_id[p["id"].replace("-", "")] = p["vector"]["dense"]
                offset = r.get("next_page_offset")
                if offset is None:
                    break
        missing = [r[0] for r in rows if r[0] not in by_id]
        if missing:
            raise SystemExit(
                f"{len(missing)} chunks have no stored vector (first {missing[0]}). "
                f"The baseline must cover exactly the candidate set the new model "
                f"sees, or the two arms are ranking different corpora."
            )
        doc_vecs = np.asarray([by_id[r[0]] for r in rows], dtype=np.float32)
        doc_vecs /= np.linalg.norm(doc_vecs, axis=1, keepdims=True)
        embed_s = time.perf_counter() - started
        dim = doc_vecs.shape[1]
        print(f"  {len(doc_vecs)} stored vectors, dim={dim}, in {embed_s:.1f}s")

        # The query side still has to be computed, and it must come from the
        # SAME embedder the server uses — not a local re-load of the weights,
        # which is a different process at a different precision.
        print("  embedding queries via the running /encode ...", flush=True)
        import struct

        qv = []
        with httpx.Client(timeout=300) as cl:
            for i in range(0, len(instances), 64):
                batch = [x["query"] for x in instances[i : i + 64]]
                buf = cl.post(
                    "http://127.0.0.1:11211/encode", json={"texts": batch}
                ).content
                off = 4
                n, d = struct.unpack_from("<II", buf, off)
                off += 8
                for k in range(n):
                    qv.append(list(struct.unpack_from(f"<{d}f", buf, off + k * d * 4)))
        q_vecs = np.asarray(qv, dtype=np.float32)
        q_vecs /= np.linalg.norm(q_vecs, axis=1, keepdims=True)
    else:
        print(f"  loading {args.model} on {device} ...", flush=True)
        model = SentenceTransformer(
            args.model, trust_remote_code=spec["trust_remote_code"], device=device
        )
        # `model_kwargs={"torch_dtype": ...}` is SILENTLY IGNORED by this
        # model's `trust_remote_code` loader — it builds the module itself.
        # Measured: asking for float16 produced float32 parameters, two
        # byte-identical runs, and a "fp16 changes nothing" conclusion that was
        # about a precision never used. So cast explicitly and then ASSERT, the
        # only version of this that cannot lie.
        want = getattr(torch, args.dtype)
        if want is not torch.float32:
            model = model.to(want)
        got = next(model[0].auto_model.parameters()).dtype
        if got is not want:
            raise SystemExit(
                f"asked for {args.dtype}, parameters are {got}. A precision arm "
                f"that did not take reports the other precision under this "
                f"one's name."
            )
        model.max_seq_length = spec["max_seq_length"]
        dim = model.get_sentence_embedding_dimension()
        # The device is asserted, not read back and reported. `device=` is a
        # request like `torch_dtype` was, and the recorded failure is that a
        # request which did not take still produces a full, plausible result
        # file — here it would be a throughput number off by ~30x.
        landed = str(model.device).split(":")[0]
        if landed != device:
            raise SystemExit(
                f"asked for --device {device}, the model is on {landed}. Every "
                f"timing in this run would describe the wrong hardware."
            )
        print(
            f"  dim={dim} max_seq={model.max_seq_length} device={device} "
            f"dtype={args.dtype}"
        )
        # `use_flash_attn: true` in the config is aspirational: without
        # flash-attn installed the vendored modeling code falls back to a path
        # that MATERIALISES `batch x heads x seq x seq` and softmaxes it. That
        # tensor is what OOM'd a 32 GiB card at batch 128, and it is the shape
        # that would meet the iGPU's 4 GiB per-allocation ceiling. Reported
        # rather than worked around, because it is a real deployment cost.
        try:
            import flash_attn  # noqa: F401

            print("  flash-attn: present")
        except ImportError:
            print(
                "  flash-attn: ABSENT — attention is the naive O(seq^2) path, "
                "so throughput here is a floor, not the model's ceiling"
            )

        started = time.perf_counter()
        doc_texts = [spec["doc_prefix"] + r[2] for r in rows]
        doc_vecs = model.encode(
            doc_texts,
            batch_size=args.batch_size,
            convert_to_numpy=True,
            normalize_embeddings=True,
            show_progress_bar=True,
        )
        embed_s = time.perf_counter() - started
        print(
            f"  embedded the corpus in {embed_s / 60:.1f} min "
            f"({len(rows) / embed_s:.0f} chunks/s)"
        )

        q_texts = [spec["query_prefix"] + i["query"] for i in instances]
        q_vecs = model.encode(
            q_texts,
            batch_size=args.batch_size,
            convert_to_numpy=True,
            normalize_embeddings=True,
            show_progress_bar=False,
        )

    # Exact search: both sides are L2-normalised, so a matmul IS cosine, and
    # `topk` over it is the true ranking rather than an HNSW approximation.
    docs = torch.from_numpy(np.asarray(doc_vecs)).to(device)
    label = args.label or (
        "bgem3-exact" if args.baseline_from_qdrant else args.model.split("/")[-1]
    )
    out_path = (
        root
        / config["run"]["results_dir"]
        / f"{label}__{args.corpus}{args.qrels_suffix}.jsonl"
    )

    with out_path.open("w") as out:
        for pos, (inst, qv) in enumerate(zip(instances, q_vecs, strict=True), start=1):
            t0 = time.perf_counter()
            q = torch.from_numpy(np.asarray(qv)).to(device)
            scores = docs @ q
            top = torch.topk(scores, k=min(TOP_K, len(rows)))
            latency = (time.perf_counter() - t0) * 1000
            results = [
                {
                    "path": rows[i][1],
                    "score": float(s),
                    "start_line": rows[i][3],
                    "end_line": rows[i][4],
                }
                for s, i in zip(top.values.tolist(), top.indices.tolist(), strict=True)
            ]
            out.write(
                json.dumps(
                    {
                        "schema": 1,
                        "corpus": args.corpus,
                        "language": corpus["language"],
                        "instance_id": inst["instance_id"],
                        "datasets": inst["datasets"],
                        "base_commit": inst["base_commit"],
                        "snapshot_sha": inst["base_commit"],
                        "query_bytes": len(inst["query"].encode()),
                        "gold_files": inst["gold_files"],
                        "gold_functions": inst.get("gold_functions"),
                        "n_gold": inst["n_gold"],
                        "category": inst.get("category"),
                        "leaks_gold_path": inst["leaks_gold_path"],
                        "leaks_gold_basename": inst["leaks_gold_basename"],
                        "lexical_overlap": inst.get("lexical_overlap"),
                        "overlap_bucket": inst.get("overlap_bucket"),
                        "doc_path": inst.get("doc_path"),
                        "results": results,
                        "n_results": len(results),
                        "distinct_files": len({r["path"] for r in results}),
                        "refusal": None,
                        "search_ms": round(latency, 2),
                        "index_ms": 0,
                        "orphans_pruned": 0,
                        "failed_files": 0,
                        "failed_paths": [],
                        "prov": {
                            "label": label,
                            "system": "external-embedder",
                            "model": args.model,
                            "dim": dim,
                            "query_prefix": spec["query_prefix"],
                            "doc_prefix": spec["doc_prefix"],
                            "search": "exact brute-force cosine",
                            "dtype": args.dtype,
                            "vectors": (
                                "stored, from Qdrant"
                                if args.baseline_from_qdrant
                                else "computed here"
                            ),
                            "chunks": len(rows),
                            "exclude": exclude,
                            "corpus_embed_seconds": round(embed_s, 1),
                        },
                    },
                    sort_keys=True,
                )
                + "\n"
            )
            if pos % 200 == 0:
                print(f"  [{pos}/{len(instances)}]", flush=True)

    print(f"  wrote {out_path.relative_to(root)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
