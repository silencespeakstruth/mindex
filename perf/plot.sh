#!/usr/bin/env bash
#
# Optional convenience plots from the per-run result CSVs (the CSVs are the source of
# truth — graph them however you like; this just renders the standard views).
#
# --csv may be a single CSV or a DIRECTORY of them (the default: perf/results/, where
# run.sh writes one timestamped file per run). A directory is concatenated so you can
# compare across runs. Columns are looked up by header name, so it survives column
# reorderings. Rows with NA in a plotted column are skipped. Points are scatter; for
# per-series lines (e.g. one line per embed_batch) filter first or use a spreadsheet.
#
# Deps: gnuplot, awk.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
csv="$here/results" # file or directory of per-run CSVs
outdir="$here/plots"

usage() {
    cat <<'EOF'
Usage: plot.sh [--csv FILE|DIR] [--out DIR]
  --csv FILE|DIR  results CSV or directory of them (default: perf/results/)
  --out DIR       PNG output                       (default: perf/plots)
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --csv)
            csv="$2"
            shift 2
            ;;
        --out)
            outdir="$2"
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

for dep in gnuplot awk; do
    command -v "$dep" >/dev/null 2>&1 || {
        echo "Missing dependency: $dep" >&2
        exit 1
    }
done

# Combine the input into one CSV (header once, then all data rows).
combined="$(mktemp)"
trap 'rm -f "$combined"' EXIT
if [ -d "$csv" ]; then
    first=1
    for f in "$csv"/*.csv; do
        [ -e "$f" ] || {
            echo "No CSVs in $csv — run run.sh first." >&2
            exit 1
        }
        if [ "$first" -eq 1 ]; then
            cat "$f" >"$combined"
            first=0
        else
            tail -n +2 "$f" >>"$combined"
        fi
    done
elif [ -f "$csv" ]; then
    cat "$csv" >"$combined"
else
    echo "No CSV at $csv — run run.sh first." >&2
    exit 1
fi
mkdir -p "$outdir"

# pair XCOL YCOL -> TSV of (x y) on stdout, skipping header and NA cells.
pair() {
    awk -F, -v xn="$1" -v yn="$2" '
        NR == 1 { for (i = 1; i <= NF; i++) { if ($i == xn) xi = i; if ($i == yn) yi = i }; next }
        xi && yi && $xi != "NA" && $yi != "NA" { print $xi "\t" $yi }
    ' "$combined"
}

render() { # OUT TITLE XLABEL YLABEL XCOL YCOL
    local png="$outdir/$1" title="$2" xlab="$3" ylab="$4"
    local tmp
    tmp="$(mktemp)"
    pair "$5" "$6" >"$tmp"
    if [ ! -s "$tmp" ]; then
        echo "  skip $1 (no data for $5/$6)" >&2
        rm -f "$tmp"
        return
    fi
    gnuplot <<EOF
set datafile separator "\t"
set terminal pngcairo size 960,640
set output "$png"
set title "$title"
set xlabel "$xlab"
set ylabel "$ylab"
set grid
set key off
plot "$tmp" using 1:2 with points pt 7 ps 1.3 lc rgb "#1f77b4"
EOF
    rm -f "$tmp"
    echo "  wrote $png" >&2
}

# The three standard views (see README "Reading the results").
#
# THERE WERE FOUR. `throughput_vs_embedder_batch` and `fwd_batch_vs_throughput`
# plotted `embedder_batch` and `fwd_batch_mean`, which came from the vendored
# BGE-M3 server's `/stats` and have been `NA` on every row since v3 deleted it —
# so both rendered a blank chart with axes, which is a worse answer than no chart.
# The columns are gone from run.sh's header; these went with them.
#
# What they were for is not gone: how full the GPU's forward pass gets is still
# the lever, it is just no longer readable from the client side. Sweep
# `--shard-files` and mindex's `--embed-batch`, and read the effect off
# `chunks_per_s` in the first plot below.
render throughput_vs_concurrency.png \
    "Throughput vs concurrency" \
    "concurrency (VUs)" "chunks/s" concurrency chunks_per_s
render latency_vs_throughput.png \
    "Latency vs throughput (knee = optimum)" \
    "chunks/s" "p95 /index latency (ms)" chunks_per_s req_dur_p95
render throughput_vs_embed_batch.png \
    "Throughput vs mindex --embed-batch" \
    "embed_batch (chunks per /v1/embeddings call)" "chunks/s" embed_batch chunks_per_s

echo "Plots in $outdir" >&2
