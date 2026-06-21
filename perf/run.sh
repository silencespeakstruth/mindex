#!/usr/bin/env bash
#
# mindex indexing benchmark orchestrator.
#
# Assumes mindex + embedder + Qdrant are ALREADY RUNNING with whatever flags you are
# testing — this script starts nothing and touches no hardware. It drives one k6 load
# run per concurrency level, auto-detects the live config (mindex GET /config +
# embedder GET /stats), and appends one self-describing row per run to a CSV.
#
# To sweep process-level flags (embedder --batch / --max-inflight, mindex
# --embed-batch / --db-pool-size): change them, restart those services, rerun this
# script. The CSV is append-only, so the matrix grows across reruns. Always use a
# fresh corpus run against a fresh project GUID (handled here) so nothing is
# hash-skipped.
#
# Deps: k6, jq, curl, awk.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mindex_url="https://127.0.0.1:11111"
embedder_url="http://127.0.0.1:11211"
protocol="v0"
corpus="$here/corpus/data/default"
results_dir="$here/results"
out="" # empty => auto-name per run into results_dir (timestamp + config)
concurrency="1 2 4 8"
label=""
script="$here/index_load.js"
poll_interval="0.5"

usage() {
    cat <<'EOF'
Usage: run.sh [options]
  --mindex-url URL     mindex base URL        (default: https://127.0.0.1:11111)
  --embedder-url URL   embedder base URL      (default: http://127.0.0.1:11211)
                       pass "" to skip /stats capture
  --protocol VER       API version segment    (default: v0)
  --corpus DIR         corpus dir (shards + manifest.json)
                       (default: perf/corpus/data/default)
  --out FILE           write CSV here (default: auto perf/results/<stamp>_<cfg>.csv)
  --concurrency "L .." space-separated VU levels (default: "1 2 4 8")
  --label TEXT         free-text tag on every row this run
  -h, --help           this help

The per-run process flags (embedder --batch, mindex --embed-batch, …) are read live
from the servers, not passed here — restart the servers to change them, then rerun.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --mindex-url)
            mindex_url="$2"
            shift 2
            ;;
        --embedder-url)
            embedder_url="$2"
            shift 2
            ;;
        --protocol)
            protocol="$2"
            shift 2
            ;;
        --corpus)
            corpus="$2"
            shift 2
            ;;
        --out)
            out="$2"
            shift 2
            ;;
        --concurrency)
            concurrency="$2"
            shift 2
            ;;
        --label)
            label="$2"
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

for dep in k6 jq curl awk; do
    command -v "$dep" >/dev/null 2>&1 || {
        echo "Missing dependency: $dep" >&2
        exit 1
    }
done

manifest="$corpus/manifest.json"
[ -f "$manifest" ] || {
    echo "No manifest at $manifest — run corpus/fetch.sh first." >&2
    exit 1
}

shard_count=$(jq -r '.shards' "$manifest")
total_files=$(jq -r '.total_files' "$manifest")
total_bytes=$(jq -r '.total_bytes' "$manifest")
corpus_name=$(jq -r '.name' "$manifest")
[ "$shard_count" -gt 0 ] 2>/dev/null || {
    echo "Corpus has no shards." >&2
    exit 1
}

# fdiv NUM DEN [DECIMALS] — safe float divide (0 when DEN<=0).
fdiv() {
    awk -v n="$1" -v d="$2" -v p="${3:-2}" \
        'BEGIN { if (d > 0) printf "%.*f", p, n / d; else printf "0" }'
}

gen_guid() {
    if command -v uuidgen >/dev/null 2>&1; then
        uuidgen | tr '[:upper:]' '[:lower:]' | tr -dc 'a-f0-9'
    elif command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 16
    else
        tr -dc 'a-f0-9' </proc/sys/kernel/random/uuid
    fi
}

jget() { # URL JQ-FILTER DEFAULT — GET + extract, DEFAULT on any failure
    curl -sk --max-time 10 "$1" 2>/dev/null | jq -r "$2 // \"$3\"" 2>/dev/null || echo "$3"
}

header="timestamp,label,mindex_version,model_id,embed_batch,db_pool_size,\
embedder_batch,embedder_max_inflight,embedder_maxlen,\
concurrency,corpus,total_files,total_bytes,total_chunks,\
wall_clock_s,chunks_per_s,files_per_s,mb_per_s,\
req_dur_p50,req_dur_p90,req_dur_p95,req_dur_p99,http_reqs,\
err_429,err_499,err_500,err_503,err_other,\
fwd_batch_mean,fwd_batch_max,embedder_encode_s,queue_highwater,embedder_429,min_pool_available"

safe_label="${label//,/;}"
safe_label="${safe_label//$'\n'/ }"

# Auto-name the output file from the live config so reruns never overwrite each other
# and every file is self-identifying for later comparison. Explicit --out wins.
if [ -z "$out" ]; then
    mkdir -p "$results_dir"
    n_eb="$(jget "$mindex_url/config" '.embed_batch' NA)"
    n_pool="$(jget "$mindex_url/config" '.db_pool_size' NA)"
    n_xb=NA
    [ -n "$embedder_url" ] &&
        n_xb="$(curl -sk --max-time 10 "$embedder_url/stats" 2>/dev/null | jq -r '.config.batch // "NA"')"
    stamp="$(date -u +%Y%m%dT%H%M%SZ)"
    fn="${stamp}_eb${n_eb}_pool${n_pool}_xb${n_xb}"
    # filename-safe label fragment
    lbl="$(printf '%s' "$safe_label" | tr ' /' '--' | tr -cd 'A-Za-z0-9._-')"
    [ -n "$lbl" ] && fn="${fn}_${lbl}"
    out="$results_dir/${fn}.csv"
fi

if [ ! -f "$out" ]; then
    echo "$header" >"$out"
fi

echo "Corpus '$corpus_name': $shard_count shards, $total_files files, $total_bytes bytes" >&2
echo "Output: $out" >&2
echo "Concurrency levels: $concurrency" >&2

for c in $concurrency; do
    echo "── concurrency=$c ──────────────────────────────────────────" >&2

    # k6 shared-iterations needs iterations (= shards) >= VUs. A too-small corpus
    # can't exercise high concurrency — skip with a hint to rebuild it.
    if [ "$c" -gt "$shard_count" ]; then
        echo "  skip: corpus has $shard_count shards < concurrency $c." >&2
        echo "  rebuild bigger: corpus/fetch.sh --shard-files <smaller> (or add repos)." >&2
        continue
    fi

    # Reset embedder rolling counters so this run's /stats are clean.
    if [ -n "$embedder_url" ]; then
        curl -sk --max-time 10 -X POST "$embedder_url/stats/reset" >/dev/null 2>&1 ||
            echo "  warn: embedder /stats/reset failed (stale stats expected)" >&2
    fi

    guid="$(gen_guid)"
    summary="$(mktemp)"
    pool_log="$(mktemp)"

    # Sample mindex pool headroom in the background for the whole run.
    (
        while :; do
            curl -sk --max-time 5 "$mindex_url/status" 2>/dev/null |
                jq -r '.pool_available' 2>/dev/null >>"$pool_log" || true
            sleep "$poll_interval"
        done
    ) &
    poller=$!

    SUMMARY_OUT="$summary" \
        MINDEX_URL="$mindex_url" PROTOCOL="$protocol" PROJECT_GUID="$guid" \
        CORPUS_DIR="$corpus" SHARD_COUNT="$shard_count" CONCURRENCY="$c" \
        INSECURE="true" REQ_TIMEOUT="${REQ_TIMEOUT:-600s}" \
        k6 run "$script" >&2 || echo "  warn: k6 exited non-zero" >&2

    kill "$poller" 2>/dev/null || true
    wait "$poller" 2>/dev/null || true

    # k6 summary (NA-safe).
    sj() { jq -r "$1 // \"NA\"" "$summary" 2>/dev/null || echo NA; }
    wall_ms="$(sj '.wall_clock_ms')"
    chunks="$(sj '.chunks_indexed')"
    http_reqs="$(sj '.http_reqs')"
    p50="$(sj '.req_dur_p50')"
    p90="$(sj '.req_dur_p90')"
    p95="$(sj '.req_dur_p95')"
    p99="$(sj '.req_dur_p99')"
    e429="$(sj '.err_429')"
    e499="$(sj '.err_499')"
    e500="$(sj '.err_500')"
    e503="$(sj '.err_503')"
    eother="$(sj '.err_other')"

    wall_s="$(fdiv "${wall_ms:-0}" 1000 3)"
    chunks_per_s="$(fdiv "${chunks:-0}" "$wall_s" 2)"
    files_per_s="$(fdiv "$total_files" "$wall_s" 2)"
    mb_per_s="$(fdiv "$(fdiv "$total_bytes" 1048576 4)" "$wall_s" 3)"

    # Live mindex config.
    mver="$(jget "$mindex_url/config" '.version' NA)"
    model="$(jget "$mindex_url/config" '.model_id' NA)"
    embed_batch="$(jget "$mindex_url/config" '.embed_batch' NA)"
    db_pool="$(jget "$mindex_url/config" '.db_pool_size' NA)"

    # Live embedder config + rolling stats.
    eb=NA
    emaxinf=NA
    emaxlen=NA
    fwd_mean=NA
    fwd_max=NA
    enc_s=NA
    qhw=NA
    e429srv=NA
    if [ -n "$embedder_url" ]; then
        stats="$(curl -sk --max-time 10 "$embedder_url/stats" 2>/dev/null || echo '{}')"
        eb="$(echo "$stats" | jq -r '.config.batch // "NA"')"
        emaxinf="$(echo "$stats" | jq -r '.config.max_inflight // "NA"')"
        emaxlen="$(echo "$stats" | jq -r '.config.maxlen // "NA"')"
        fwd_mean="$(echo "$stats" | jq -r '.runtime.forward_batch_mean // "NA"')"
        fwd_max="$(echo "$stats" | jq -r '.runtime.forward_batch_max // "NA"')"
        enc_s="$(echo "$stats" | jq -r '.runtime.encode_seconds_total // "NA"')"
        qhw="$(echo "$stats" | jq -r '.runtime.queue_depth_highwater // "NA"')"
        e429srv="$(echo "$stats" | jq -r '.runtime.requests_429 // "NA"')"
    fi

    min_pool="$(sort -n "$pool_log" 2>/dev/null | awk 'NF{print;exit}')"
    [ -z "$min_pool" ] && min_pool=NA

    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$safe_label" "$mver" "$model" \
        "$embed_batch" "$db_pool" "$eb" "$emaxinf" "$emaxlen" \
        "$c" "$corpus_name" "$total_files" "$total_bytes" "$chunks" \
        "$wall_s" "$chunks_per_s" "$files_per_s" "$mb_per_s" \
        "$p50" "$p90" "$p95" "$p99" "$http_reqs" \
        "$e429" "$e499" "$e500" "$e503" "$eother" \
        "$fwd_mean" "$fwd_max" "$enc_s" "$qhw" "$e429srv" "$min_pool" >>"$out"

    echo "  chunks/s=$chunks_per_s  wall=${wall_s}s  encode=${enc_s}s  fwd_batch_mean=$fwd_mean  min_pool=$min_pool  429(client/srv)=$e429/$e429srv" >&2

    # Clean up so the next level starts from an empty project (Qdrant + SQLite).
    curl -sk --max-time 30 -X DELETE "$mindex_url/projects/$guid" >/dev/null 2>&1 ||
        echo "  warn: project cleanup failed for $guid" >&2

    rm -f "$summary" "$pool_log"
done

echo "Wrote results to $out" >&2
