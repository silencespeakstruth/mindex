#!/usr/bin/env bash
#
# Build a reproducible /index payload corpus from real GitHub repos.
#
# Clones each repo from repos.txt at its pinned ref, walks source files, detects the
# language by extension (mirroring tools/indexer/src/scanner.rs), JSON-escapes every
# file, and packs them into pre-batched shards in the exact POST /index body shape:
#
#   { "files": { "<lang>": { "<rel/path>": { "code": "<source>" } } } }
#
# k6 then just loads a shard and POSTs it — no file walking at load time. Output goes
# to <out>/<name>/shard-NNN.json plus a manifest.json with totals (used by run.sh to
# label rows and compute files/sec and MB/sec).
#
# Hardware-agnostic: no GPU, no OS assumptions. Deps: git, jq, awk, find.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

repos="$here/repos.txt"
out_root="$here/data"
work_dir="$here/.clones"
name="default"
shard_mb=32
shard_files=8   # max files per shard (whichever of files/MiB hits first)
max_file_kb=512 # skip single files larger than this (huge/minified — wasted batch)

usage() {
    cat <<'EOF'
Usage: fetch.sh [options]
  --repos FILE      repo manifest         (default: perf/corpus/repos.txt)
  --out DIR         output root           (default: perf/corpus/data)
  --name NAME       corpus subdir name    (default: default)
  --work DIR        clone cache dir       (default: perf/corpus/.clones)
  --shard-mb N      max raw code per shard, MiB   (default: 32)
  --shard-files N   max files per shard           (default: 8)
  --max-file-kb N   skip files larger than this   (default: 512)
  -h, --help        this help

Reruns reuse the clone cache (--work); delete it to re-fetch from scratch.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --repos)
            repos="$2"
            shift 2
            ;;
        --out)
            out_root="$2"
            shift 2
            ;;
        --name)
            name="$2"
            shift 2
            ;;
        --work)
            work_dir="$2"
            shift 2
            ;;
        --shard-mb)
            shard_mb="$2"
            shift 2
            ;;
        --shard-files)
            shard_files="$2"
            shift 2
            ;;
        --max-file-kb)
            max_file_kb="$2"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

for dep in git jq awk find; do
    command -v "$dep" >/dev/null 2>&1 || {
        echo "Missing dependency: $dep" >&2
        exit 1
    }
done

# Extension -> language, mirroring tools/indexer/src/scanner.rs::detect_language.
# Empty string = unknown (skip).
ext_to_lang() {
    case "$1" in
        rs) echo rust ;;
        py | pyw) echo python ;;
        js | mjs | cjs | jsx) echo javascript ;;
        ts | mts | cts) echo typescript ;;
        tsx) echo tsx ;;
        go) echo go ;;
        c | h) echo c ;;
        cpp | cc | cxx | hpp | hxx | hh) echo cpp ;;
        java) echo java ;;
        cs) echo csharp ;;
        rb) echo ruby ;;
        php | phtml) echo php ;;
        sh | bash) echo bash ;;
        html | htm | xhtml) echo html ;;
        css) echo css ;;
        json) echo json ;;
        scala | sc) echo scala ;;
        hs | lhs) echo haskell ;;
        ml | mli) echo ocaml ;;
        zig) echo zig ;;
        sql) echo sql ;;
        *) echo "" ;;
    esac
}

out_dir="$out_root/$name"
rm -rf "$out_dir"
mkdir -p "$out_dir" "$work_dir"

records="$out_dir/.files.jsonl" # intermediate: one JSON object per source file
: >"$records"

clone_repo() {
    local url="$1" ref="$2" dest="$3"
    if [ -d "$dest/.git" ]; then
        echo "  cache hit: $dest" >&2
    else
        echo "  cloning $url" >&2
        git clone --quiet "$url" "$dest"
    fi
    git -C "$dest" checkout --quiet "$ref" 2>/dev/null ||
        git -C "$dest" checkout --quiet "FETCH_HEAD" 2>/dev/null || {
        echo "  fetching ref $ref" >&2
        git -C "$dest" fetch --quiet origin "$ref"
        git -C "$dest" checkout --quiet FETCH_HEAD
    }
}

# Walk one repo subtree and append a JSONL record per recognised source file.
ingest_tree() {
    local repo_root="$1" walk_root="$2" max_bytes=$((max_file_kb * 1024))
    local abs rel ext lang bytes
    while IFS= read -r -d '' abs; do
        rel="${abs#"$repo_root"/}"
        case "$rel" in
            .git/* | */.git/* | */node_modules/* | */vendor/* | */target/* | \
                */dist/* | */build/* | */.venv/* | */__pycache__/*) continue ;;
        esac
        ext="${abs##*.}"
        [ "$ext" = "$abs" ] && continue # no extension
        lang="$(ext_to_lang "$ext")"
        [ -z "$lang" ] && continue
        bytes=$(wc -c <"$abs")
        [ "$bytes" -eq 0 ] && continue
        [ "$bytes" -gt "$max_bytes" ] && continue
        grep -Iq . "$abs" || continue # skip binary
        jq -cn --arg lang "$lang" --arg path "$rel" \
            --argjson bytes "$bytes" --rawfile code "$abs" \
            '{lang:$lang, path:$path, bytes:$bytes, code:$code}' >>"$records"
    done < <(find "$walk_root" -type f -print0)
}

echo "Building corpus '$name' -> $out_dir" >&2
while IFS= read -r line || [ -n "$line" ]; do
    line="${line%%#*}" # drop trailing (and whole-line) comments
    # shellcheck disable=SC2086         # intentional word-split into fields
    set -- $line
    [ $# -eq 0 ] && continue
    url="$1"
    ref="${2:-}"
    subdirs=""
    [ $# -gt 2 ] && subdirs="${*:3}"
    [ -z "$ref" ] && {
        echo "  skip (no ref): $url" >&2
        continue
    }
    slug="$(echo "$url" | sed -E 's#.*/([^/]+/[^/]+?)(\.git)?$#\1#; s#/#__#g')"
    dest="$work_dir/$slug"
    clone_repo "$url" "$ref" "$dest"
    if [ -n "${subdirs:-}" ]; then
        for sd in $subdirs; do
            [ -d "$dest/$sd" ] && ingest_tree "$dest" "$dest/$sd" ||
                echo "  warn: missing subdir $sd in $slug" >&2
        done
    else
        ingest_tree "$dest" "$dest"
    fi
done <"$repos"

total_files=$(wc -l <"$records" | tr -d ' ')
if [ "$total_files" -eq 0 ]; then
    echo "No source files collected — check repos.txt refs/subdirs." >&2
    exit 1
fi

# Greedy pack into shards, starting a new shard when EITHER the byte budget or the
# file-count cap is hit (the cap guarantees enough shards for concurrency testing on
# small corpora). jq -c keeps each record on one physical line (tabs/newlines are
# escaped), so splitting on the first TAB below is safe.
limit=$((shard_mb * 1024 * 1024))
paste <(jq -r '.bytes' "$records") <(jq -c '.' "$records") |
    awk -F '\t' -v limit="$limit" -v fmax="$shard_files" -v dir="$out_dir" '
    BEGIN { shard = 0; cum = 0; cnt = 0 }
    {
        b = $1; json = $2
        if (cnt > 0 && (cum + b > limit || cnt >= fmax)) { shard++; cum = 0; cnt = 0 }
        printf "%s\n", json >> sprintf("%s/.shard-%03d.jsonl", dir, shard)
        cum += b; cnt++
    }'

shard_count=0
for j in "$out_dir"/.shard-*.jsonl; do
    [ -e "$j" ] || break
    n=$(printf '%s' "$j" | sed -E 's#.*\.shard-([0-9]+)\.jsonl#\1#')
    jq -s 'reduce .[] as $r ({files:{}}; .files[$r.lang][$r.path] = {code:$r.code})' \
        "$j" >"$out_dir/shard-$n.json"
    rm -f "$j"
    shard_count=$((shard_count + 1))
done

# Manifest: totals + per-language file counts (drives run.sh throughput math).
jq -s '{
    name: $name, generated_at: $ts, shard_mb: ($shard_mb|tonumber),
    shards: ($shards|tonumber),
    total_files: length,
    total_bytes: (map(.bytes) | add),
    languages: (group_by(.lang) | map({(.[0].lang): length}) | add)
}' --arg name "$name" --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg shard_mb "$shard_mb" --arg shards "$shard_count" \
    "$records" >"$out_dir/manifest.json"
rm -f "$records"

echo "Done: $total_files files, $shard_count shards -> $out_dir" >&2
jq -C '.' "$out_dir/manifest.json" >&2
