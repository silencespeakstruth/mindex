#!/usr/bin/env python3
"""Fetch everything a benchmark run reads: repository clones and ground truth.

Idempotent and re-runnable. Clones are full (not shallow, not blob-filtered):
run.py checks out one commit per instance — up to 850 of them for django — and
a filtered clone would turn that loop into 850 network round trips whose timing
depends on someone else's server. Reproducibility here is worth the disk.

Ground-truth files are pulled at the revision pinned in corpora.toml and their
sha256 recorded in a manifest, so "the dataset moved" is a thing the harness
can notice rather than a thing that silently changes results.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

import tomllib

HF_RESOLVE = "https://huggingface.co/datasets/{hf_id}/resolve/{revision}/{path}"

# Big enough that a 200 MB parquet does not become a million syscalls, small
# enough not to hold a whole dataset in memory.
CHUNK_BYTES = 1 << 20


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def load_config(path: Path) -> dict[str, Any]:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def run_git(args: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} failed ({proc.returncode}): {proc.stderr.strip()}"
        )
    return proc.stdout


def clone_or_update(url: str, dest: Path, *, force: bool) -> None:
    """Full clone, or fetch if it already exists."""
    if dest.exists() and not force:
        # A benchmark must not silently drift onto new upstream history between
        # runs, but instances name commits by SHA, so fetching only ever adds
        # reachable objects — it cannot change what an instance resolves to.
        print(f"  fetch  {dest.name}", flush=True)
        run_git(["fetch", "--all", "--tags", "--quiet"], cwd=dest)
        return

    if dest.exists():
        raise RuntimeError(f"{dest} exists; remove it by hand rather than --force")

    dest.parent.mkdir(parents=True, exist_ok=True)
    print(f"  clone  {url} -> {dest.name}", flush=True)
    run_git(["clone", "--quiet", url, str(dest)])


def download(url: str, dest: Path) -> str:
    """Stream a URL to disk, returning its sha256."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256()
    tmp = dest.with_suffix(dest.suffix + ".partial")

    req = urllib.request.Request(url, headers={"User-Agent": "mindex-bench/1"})
    try:
        with urllib.request.urlopen(req, timeout=300) as resp, tmp.open("wb") as out:
            while chunk := resp.read(CHUNK_BYTES):
                digest.update(chunk)
                out.write(chunk)
    except urllib.error.HTTPError as exc:
        tmp.unlink(missing_ok=True)
        raise RuntimeError(f"GET {url} -> HTTP {exc.code}") from exc

    tmp.replace(dest)
    return digest.hexdigest()


def dataset_files(
    name: str, spec: dict[str, Any], corpora: list[dict[str, Any]]
) -> list[str]:
    """Paths to fetch inside a dataset repo, for the corpora actually selected."""
    kind = spec.get("kind")

    if kind == "swebench_like":
        # One parquet per split; only the configured split is ever read.
        return [f"data/{spec['split']}-00000-of-00001.parquet"]

    if kind == "multi_swebench":
        # One JSONL per repository, so a run downloads only what it scores.
        template = spec["path_template"]
        wanted = []
        for corpus in corpora:
            if name not in corpus["datasets"]:
                continue
            org, repo = corpus["repo"].split("/", 1)
            wanted.append(
                template.format(lang=corpus["multi_lang"], org=org, name=repo)
            )
        return wanted

    raise RuntimeError(f"dataset {name}: unknown kind {kind!r}")


def select_corpora(
    config: dict[str, Any], names: list[str] | None, max_tier: int
) -> list[dict[str, Any]]:
    corpora = [c for c in config["corpus"] if c["tier"] <= max_tier]
    if names:
        by_name = {c["name"]: c for c in config["corpus"]}
        missing = [n for n in names if n not in by_name]
        if missing:
            raise SystemExit(f"unknown corpus name(s): {', '.join(missing)}")
        corpora = [by_name[n] for n in names]
    return corpora


def main() -> int:
    root = repo_root()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=root / "bench" / "corpora.toml")
    parser.add_argument(
        "--corpus", action="append", dest="corpora", help="repeatable; default: by tier"
    )
    parser.add_argument(
        "--tier",
        type=int,
        default=1,
        help="fetch corpora at or below this tier (0=smoke, 1=primary, 2=optional)",
    )
    parser.add_argument("--repos-only", action="store_true")
    parser.add_argument("--datasets-only", action="store_true")
    parser.add_argument("--force", action="store_true", help="re-clone from scratch")
    args = parser.parse_args()

    config = load_config(args.config)
    corpora = select_corpora(config, args.corpora, args.tier)
    if not corpora:
        raise SystemExit("no corpora selected")

    clone_dir = root / config["run"]["clone_dir"]
    data_dir = root / config["run"]["data_dir"]
    manifest_path = data_dir / "manifest.json"
    manifest: dict[str, Any] = {}
    if manifest_path.exists():
        manifest = json.loads(manifest_path.read_text())

    print(f"corpora: {', '.join(c['name'] for c in corpora)}\n")

    if not args.datasets_only:
        print("repositories:")
        for corpus in corpora:
            clone_or_update(corpus["url"], clone_dir / corpus["name"], force=args.force)
        print()

    if not args.repos_only:
        print("ground truth:")
        needed = {d for c in corpora for d in c["datasets"]}
        for name in sorted(needed):
            spec = config["datasets"][name]
            for path in dataset_files(name, spec, corpora):
                dest = data_dir / "datasets" / name / Path(path).name
                key = f"{name}/{Path(path).name}"
                if dest.exists() and key in manifest and not args.force:
                    print(f"  have   {key}")
                    continue
                url = HF_RESOLVE.format(
                    hf_id=spec["hf_id"], revision=spec["revision"], path=path
                )
                digest = download(url, dest)
                manifest[key] = {
                    "hf_id": spec["hf_id"],
                    "revision": spec["revision"],
                    "path": path,
                    "sha256": digest,
                    "bytes": dest.stat().st_size,
                }
                print(f"  got    {key}  sha256={digest[:16]}…")

        manifest_path.parent.mkdir(parents=True, exist_ok=True)
        manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        print(f"\nmanifest: {manifest_path.relative_to(root)}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
