#!/usr/bin/env python3
"""Family F3: the chunk token window, swept by full reindex.

THE QUESTION. `[slicer].max_chunk_tokens` is 512, chosen as "BGE-M3's sweet
spot; measured, not computed" — but measured on a 23-question set that no
longer exists (`CLAUDE.md`, Slicer). F2 then showed the retrieval stack behaves
differently for short queries than for long ones, and mindex's callers ask
SHORT questions: the MCP `search` tool, `mindex-search.sh`, the VS Code Ask
field. A short question carries few content tokens, and a 512-token chunk
averages them against several hundred unrelated ones — so a narrower window is
a hypothesis with a mechanism, not a guess.

WHY THIS COSTS A REINDEX EACH. The window governs node selection in the AST
walk, so every chunk boundary, every embedding and every stored vector changes.
`[slicer]` is server-side and TOML-only, so each arm is: rewrite the config,
restart the bench server, index the corpus into its own project, query it, drop
it. Nothing here is shared between arms except the source tree.

`min_chunk_tokens` is deliberately held at its default. Sweeping both would
change two things at once, and the floor is not what the mechanism above
implicates.

THE CONFOUND THIS CANNOT REMOVE, stated because it points the same way as the
hypothesis. Ground truth is at FILE level and a narrower window makes more
chunks per file, so a file has more tickets in the same lottery — some gain at
`recall@k` is arithmetic rather than retrieval. Two things are therefore
reported beside the metric: the chunk count each arm produced, and `MRR@10`,
which a mere increase in tickets moves far less than recall does. A gain that
appears only in recall and not in MRR is the confound, not the effect.

The other cost is invisible here entirely: a caller reading a hit gets less
surrounding code from a narrower chunk. This benchmark cannot see that, and the
decision is not this file's to make.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

import httpx

sys.path.insert(0, str(Path(__file__).resolve().parent))

from build_qrels import repo_root
from run import Server, project_guid

DEFAULT_WINDOWS = (256, 364, 512)
BIND = "127.0.0.1:11121"
STARTUP_TIMEOUT_S = 60


def write_config(base: Path, out: Path, max_tokens: int) -> None:
    """The bench config with one line changed, and nothing else touched."""
    text = base.read_text()
    lines = []
    seen = False
    for line in text.splitlines():
        if line.strip().startswith("max_chunk_tokens"):
            lines.append(f"max_chunk_tokens     = {max_tokens}")
            seen = True
        else:
            lines.append(line)
    if not seen:
        raise SystemExit(
            f"{base} has no `max_chunk_tokens` line to sweep. Refusing to guess "
            f"where to put one — an arm silently running at the default would "
            f"report the baseline twice under two names."
        )
    out.write_text("\n".join(lines) + "\n")


def restart_server(root: Path, config: Path) -> subprocess.Popen[bytes]:
    subprocess.run(["pkill", "-f", f"mindex --config .*--bind {BIND}"], check=False)
    subprocess.run(["pkill", "-f", f"--bind {BIND}"], check=False)
    time.sleep(2)
    proc = subprocess.Popen(
        [
            str(root / "target" / "release" / "mindex"),
            "--config",
            str(config),
            "--bind",
            BIND,
        ],
        cwd=root,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    deadline = time.time() + STARTUP_TIMEOUT_S
    while time.time() < deadline:
        try:
            r = httpx.get(f"https://{BIND}/health", verify=False, timeout=3)
            if r.status_code == 200:
                return proc
        except httpx.HTTPError:
            pass
        time.sleep(1)
    raise SystemExit(
        f"bench server did not come up on {BIND} within {STARTUP_TIMEOUT_S}s"
    )


def verify_window(db: Path, guid: str, expected: int, sample: int = 400) -> None:
    """The arm must be the arm it says it is, checked on what it PRODUCED.

    A restart that silently kept the old config would report the baseline under
    a new name, and every later comparison would then be a system against
    itself — which presents as a clean null result, the most believable wrong
    answer this sweep can produce. `GET /config` does not publish the slicer
    window, so the check is on the chunks: re-tokenize a sample and assert none
    exceeds the window it claims.

    The tolerance is not slack for sloppiness. `CLAUDE.md` records that the
    window is counted over WHOLE-FILE token offsets, so a chunk re-encoded on
    its own is a different measurement — an edge token splits differently
    without its surroundings, and 512 can re-encode at 513. The repo's own
    slicer test allows the same kind of margin for the same reason.
    """
    import sqlite3

    from tokenizers import Tokenizer

    tok = Tokenizer.from_pretrained("BAAI/bge-m3")
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT code FROM project_file_chunks WHERE project_guid = ? "
        "AND status = 'active' ORDER BY LENGTH(code) DESC LIMIT ?",
        (guid.replace("-", ""), sample),
    ).fetchall()
    conn.close()
    if not rows:
        raise SystemExit(f"window {expected}: the project has no active chunks")
    sizes = [len(tok.encode(r[0]).ids) for r in rows]
    ceiling = expected * 1.05 + 8
    over = [s for s in sizes if s > ceiling]
    print(
        f"  window check: largest {sample} chunks re-encode at "
        f"max {max(sizes)} tokens (claimed window {expected})"
    )
    if over:
        raise SystemExit(
            f"window {expected}: {len(over)} of the {sample} largest chunks "
            f"exceed {ceiling:.0f} tokens (worst {max(over)}). The config did "
            f"not take, or the slicer does not honour it — either way this arm "
            f"is not measurable."
        )


def main() -> int:
    root = repo_root()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--corpus", action="append", dest="corpora", required=True)
    ap.add_argument(
        "--windows",
        type=int,
        nargs="+",
        default=list(DEFAULT_WINDOWS),
        help="max_chunk_tokens values to sweep",
    )
    ap.add_argument("--qrels-suffix", default="-docs-short")
    ap.add_argument(
        "--ablation", action="store_true", help="also run F2 arms per window"
    )
    ap.add_argument("--keep", action="store_true", help="do not drop each project")
    ap.add_argument(
        "--db",
        type=Path,
        default=Path.home() / ".local/share/mindex-bench/mindex-bench.db",
    )
    args = ap.parse_args()

    base = root / "bench" / "bench-config.toml"
    server_url = f"https://{BIND}"
    summary = []

    for window in args.windows:
        label = f"slicer{window}"
        cfg = root / "bench" / f".bench-config-{window}.toml"
        write_config(base, cfg, window)
        print(
            f"\n{'=' * 70}\n=== max_chunk_tokens = {window}  (label {label})\n{'=' * 70}"
        )
        restart_server(root, cfg)

        cmd = [
            sys.executable,
            str(root / "bench" / "run.py"),
            "--label",
            label,
            "--fresh",
            "--equivalence-sample",
            "0",
            f"--qrels-suffix={args.qrels_suffix}",
        ]
        for name in args.corpora:
            cmd += ["--corpus", name]
        if subprocess.run(cmd, check=False).returncode != 0:
            raise SystemExit(f"window {window}: the indexing/query pass failed")

        # Verified on the chunks the arm produced, before anything is measured
        # from it.
        for name in args.corpora:
            verify_window(args.db, project_guid(label, name), window)

        if args.ablation:
            for name in args.corpora:
                subprocess.run(
                    [
                        sys.executable,
                        str(root / "bench" / "baselines" / "pipeline_ablation.py"),
                        "--corpus",
                        name,
                        f"--qrels-suffix={args.qrels_suffix}",
                        "--arm",
                        "all",
                        "--mindex-label",
                        label,
                    ],
                    check=False,
                )
                # The ablation names its files by arm alone, so they would
                # overwrite between windows. Stamp the window in.
                results = root / "bench" / "results"
                for arm in ("full", "no-colbert", "dense-only", "sparse-only"):
                    src = results / f"F2-{arm}__{name}{args.qrels_suffix}.jsonl"
                    if src.exists():
                        src.rename(
                            results
                            / f"F3-{window}-{arm}__{name}{args.qrels_suffix}.jsonl"
                        )

        chunks = {}
        for name in args.corpora:
            guid = project_guid(label, name)
            try:
                stats = httpx.get(
                    f"{server_url}/projects/{guid}", verify=False, timeout=30
                ).json()
                chunks[name] = stats.get("chunks_active")
            except (httpx.HTTPError, ValueError) as exc:
                print(f"  WARN: could not read inventory for {name}: {exc}")
        summary.append({"window": window, "label": label, "chunks_active": chunks})
        print(f"  chunks: {chunks}")

        if not args.keep:
            server = Server(server_url, verify=False)
            for name in args.corpora:
                try:
                    server.delete_project(project_guid(label, name))
                except httpx.HTTPError as exc:
                    print(f"  WARN: could not drop {name}: {exc}")
            server.close()

    out = root / "bench" / "results" / "F3_sweep.json"
    out.write_text(json.dumps({"windows": summary}, indent=2))
    print(f"\nwrote {out.relative_to(root)}")

    # Put the bench server back on the unmodified config. Left on the last
    # arm's, every later run would silently measure a swept window under the
    # baseline's name — the same "system against itself" failure `verify_window`
    # exists to catch, arriving after the sweep instead of during it.
    print("\nrestoring the baseline server")
    restart_server(root, base)
    return 0


if __name__ == "__main__":
    sys.exit(main())
