#!/usr/bin/env bash
#
# mindex-search.sh — terminal frontend for POST /v0/{project}/search.
#
# Reads a (possibly multi-line) query from --query, $EDITOR (--edit), or stdin,
# POSTs it to a mindex server, and prints matches with syntax-highlighted code,
# line numbers, and score — in ascending score order, so the best match ends up
# at the bottom of the scrollback, right above the prompt.
#
# Every option also has a MINDEX_* environment-variable fallback (see --help), so
# the tool can be driven entirely from the environment (an alias, a CI job, …);
# an explicit flag always wins over the corresponding variable.
#
# Typical usage:
#   mindex-search --project "$PROJECT" --edit         # type the query in $EDITOR
#   mindex-search --project "$PROJECT" < query.txt    # pipe a query in
#   echo 'parse a config file' | mindex-search --project "$PROJECT"
#   MINDEX_PROJECT=$PROJECT MINDEX_QUERY='…' mindex-search   # fully env-driven
#
# Requires: bash, curl, jq. Optional: pygmentize (syntax highlighting).

set -euo pipefail

# ─── Defaults (overridable by MINDEX_* env vars, then by flags) ───────────────

truthy() {
    case "$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')" in
        1 | true | yes | on) return 0 ;;
        *) return 1 ;;
    esac
}

SERVER="${MINDEX_SERVER:-https://127.0.0.1:11111}"
PROTOCOL="${MINDEX_PROTOCOL:-v0}"
PROJECT="${MINDEX_PROJECT:-}"
TOP_K="${MINDEX_TOP_K:-}"
EDIT=0
QUERY="${MINDEX_QUERY:-}"
THEME="${MINDEX_THEME:-monokai}"
COLOR="${MINDEX_COLOR:-auto}"

NO_VERIFY=0
VERBOSE=0
if truthy "${MINDEX_NO_VERIFY:-}"; then NO_VERIFY=1; fi
if truthy "${MINDEX_VERBOSE:-}"; then VERBOSE=1; fi

# Space-separated env lists seed the (repeatable) flag arrays; later --include-*/
# --exclude-* flags append to them. `read -ra` word-splits without glob-expanding.
INC_LANGS=()
INC_PATHS=()
EXC_LANGS=()
EXC_PATHS=()
[[ -n "${MINDEX_INCLUDE_LANG:-}" ]] && read -ra INC_LANGS <<<"$MINDEX_INCLUDE_LANG"
[[ -n "${MINDEX_INCLUDE_PATH:-}" ]] && read -ra INC_PATHS <<<"$MINDEX_INCLUDE_PATH"
[[ -n "${MINDEX_EXCLUDE_LANG:-}" ]] && read -ra EXC_LANGS <<<"$MINDEX_EXCLUDE_LANG"
[[ -n "${MINDEX_EXCLUDE_PATH:-}" ]] && read -ra EXC_PATHS <<<"$MINDEX_EXCLUDE_PATH"

# Offline fallback list of valid languages. The canonical source is the server's
# GET /config (`.languages`); refresh_valid_langs replaces this on demand when the
# server is reachable, so a flag is validated against the live set. This baked-in
# copy is only used when /config cannot be fetched (server down, no curl/jq).
VALID_LANGS=(rust python javascript typescript tsx go c cpp java csharp ruby php bash html css json scala haskell ocaml zig sql)

# ─── Helpers ────────────────────────────────────────────────────────────────

prog=$(basename "$0")

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf '%s\n' "$*" >&2
}

usage() {
    cat <<EOF
Usage: $prog --project GUID [options] < query.txt
       $prog --project GUID --edit
       MINDEX_PROJECT=GUID MINDEX_QUERY='…' $prog

POSTs a (possibly multi-line) query to a mindex server's search endpoint and
prints matches in ascending score order (best match last, at the bottom of the
screen). The query comes from --query, then \$EDITOR (--edit), then stdin.

Options:
  --server URL            mindex server URL (default: $SERVER)
  --project GUID          32-char hex project GUID, no dashes (required)
  --protocol VERSION      API version in the URL path (default: $PROTOCOL)
  --no-verify             skip TLS certificate verification (self-signed cert)
  --top-k N               max results to request (server default: 5)
  --include-lang LANG     restrict to LANG (repeatable)
  --include-path GLOB     restrict to paths matching GLOB (repeatable)
  --exclude-lang LANG     drop LANG (repeatable)
  --exclude-path GLOB     drop paths matching GLOB (repeatable)
  --edit                  open \$EDITOR on a temp file instead of reading stdin
  --query TEXT            use TEXT as the query instead of stdin/--edit
  --theme STYLE           pygments style for highlighting (default: $THEME)
  --color always|auto|never   colorize output (default: auto)
  -v, --verbose           print the outgoing request JSON to stderr
  -h, --help              show this help

Environment (every option has a fallback; an explicit flag always wins):
  MINDEX_SERVER, MINDEX_PROJECT, MINDEX_PROTOCOL, MINDEX_TOP_K,
  MINDEX_THEME, MINDEX_COLOR                 — same as the flags above
  MINDEX_NO_VERIFY, MINDEX_VERBOSE           — truthy: 1/true/yes/on
  MINDEX_INCLUDE_LANG, MINDEX_INCLUDE_PATH,
  MINDEX_EXCLUDE_LANG, MINDEX_EXCLUDE_PATH   — space-separated lists (merged
                                               with any matching flags)
  MINDEX_QUERY                               — query text; skips \$EDITOR/stdin
  EDITOR                                     — editor for --edit (default: vi;
                                               may carry args, e.g. "code --wait")

Valid LANG values: ${VALID_LANGS[*]}
EOF
}

is_valid_lang() {
    local needle="$1" l
    for l in "${VALID_LANGS[@]}"; do
        [[ "$l" == "$needle" ]] && return 0
    done
    return 1
}

# Best-effort refresh of VALID_LANGS from the server's GET /config (the canonical
# language list). On any failure — server unreachable, curl/jq missing, malformed
# JSON — the baked-in fallback above is kept, so offline validation still works.
refresh_valid_langs() {
    command -v curl >/dev/null 2>&1 || return 0
    command -v jq >/dev/null 2>&1 || return 0
    local cfg_opts=(-sS -X GET "${SERVER%/}/config")
    [[ "$NO_VERIFY" -eq 1 ]] && cfg_opts+=(-k)
    local body langs
    body=$(curl "${cfg_opts[@]}" 2>/dev/null) || return 0
    mapfile -t langs < <(printf '%s' "$body" | jq -r '.languages[]?' 2>/dev/null) || return 0
    [[ ${#langs[@]} -gt 0 ]] && VALID_LANGS=("${langs[@]}")
    return 0
}

# ─── Arg parsing ────────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --server)
            SERVER="$2"
            shift 2
            ;;
        --project)
            PROJECT="$2"
            shift 2
            ;;
        --protocol)
            PROTOCOL="$2"
            shift 2
            ;;
        --no-verify)
            NO_VERIFY=1
            shift
            ;;
        --top-k)
            TOP_K="$2"
            shift 2
            ;;
        --include-lang)
            INC_LANGS+=("$2")
            shift 2
            ;;
        --include-path)
            INC_PATHS+=("$2")
            shift 2
            ;;
        --exclude-lang)
            EXC_LANGS+=("$2")
            shift 2
            ;;
        --exclude-path)
            EXC_PATHS+=("$2")
            shift 2
            ;;
        --edit)
            EDIT=1
            shift
            ;;
        --query)
            QUERY="$2"
            shift 2
            ;;
        --theme)
            THEME="$2"
            shift 2
            ;;
        --color)
            COLOR="$2"
            shift 2
            ;;
        -v | --verbose)
            VERBOSE=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

[[ -z "$PROJECT" ]] && die "--project is required"
[[ "$PROJECT" =~ ^[0-9a-f]{32}$ ]] || die "--project must be a 32-char lowercase hex GUID without dashes, got: $PROJECT"

if [[ -n "$TOP_K" ]]; then
    [[ "$TOP_K" =~ ^[0-9]+$ ]] || die "--top-k must be a non-negative integer, got: $TOP_K"
fi

# Only the language flags need validation, so only fetch the canonical list when
# they are present — a plain search never hard-depends on /config being reachable.
if [[ ${#INC_LANGS[@]} -gt 0 || ${#EXC_LANGS[@]} -gt 0 ]]; then
    refresh_valid_langs
fi
for l in "${INC_LANGS[@]}" "${EXC_LANGS[@]}"; do
    [[ -z "$l" ]] && continue
    is_valid_lang "$l" || die "unknown language '$l' (valid: ${VALID_LANGS[*]})"
done

case "$COLOR" in
    always | auto | never) ;;
    *) die "--color must be always, auto, or never" ;;
esac

USE_COLOR=0
if [[ "$COLOR" == "always" ]]; then
    USE_COLOR=1
elif [[ "$COLOR" == "auto" && -t 1 && -z "${NO_COLOR:-}" ]]; then
    USE_COLOR=1
fi

for bin in curl jq; do
    command -v "$bin" >/dev/null 2>&1 || die "required tool not found: $bin"
done
HAVE_PYGMENTIZE=0
command -v pygmentize >/dev/null 2>&1 && HAVE_PYGMENTIZE=1

# ─── Acquire the query ──────────────────────────────────────────────────────

if [[ -n "$QUERY" ]]; then
    : # already set via --query
elif [[ "$EDIT" -eq 1 ]]; then
    tmp_edit=$(mktemp)
    trap 'rm -f "$tmp_edit"' EXIT
    [[ -t 0 && -t 1 ]] || warn "stdin/stdout is not a terminal; \$EDITOR may not behave — consider MINDEX_QUERY."
    # $EDITOR may carry arguments (e.g. "code --wait"); intentional word-splitting.
    # shellcheck disable=SC2086
    ${EDITOR:-vi} "$tmp_edit" || die "editor exited non-zero (\$EDITOR='${EDITOR:-vi}'); aborting"
    QUERY=$(cat "$tmp_edit")
else
    if [[ -t 0 ]]; then
        printf 'Enter query, then press Ctrl-D:\n' >&2
    fi
    QUERY=$(cat)
fi

[[ -z "${QUERY//[$' \t\n']/}" ]] && die "query is empty"

# ─── Build the request body with jq ─────────────────────────────────────────

json_str_array() {
    if [[ $# -eq 0 ]]; then
        printf '[]'
    else
        printf '%s\n' "$@" | jq -R . | jq -s .
    fi
}

inc_langs_json=$(json_str_array "${INC_LANGS[@]}")
inc_paths_json=$(json_str_array "${INC_PATHS[@]}")
exc_langs_json=$(json_str_array "${EXC_LANGS[@]}")
exc_paths_json=$(json_str_array "${EXC_PATHS[@]}")
top_k_json=${TOP_K:-null}

req_file=$(mktemp)
resp_file=$(mktemp)
trap 'rm -f "$req_file" "$resp_file" "${tmp_edit:-}"' EXIT

jq -n \
    --arg query "$QUERY" \
    --argjson top_k "$top_k_json" \
    --argjson inc_langs "$inc_langs_json" \
    --argjson inc_paths "$inc_paths_json" \
    --argjson exc_langs "$exc_langs_json" \
    --argjson exc_paths "$exc_paths_json" \
    '
    def filt(langs; paths):
        if (langs | length) == 0 and (paths | length) == 0 then null
        else {}
            + (if (langs | length) > 0 then {programming_languages: langs} else {} end)
            + (if (paths | length) > 0 then {paths: paths} else {} end)
        end;
    {query: $query}
    + (if $top_k == null then {} else {top_k: $top_k} end)
    + (filt($inc_langs; $inc_paths) as $i | if $i == null then {} else {include: $i} end)
    + (filt($exc_langs; $exc_paths) as $e | if $e == null then {} else {exclude: $e} end)
    ' >"$req_file"

if [[ "$VERBOSE" -eq 1 ]]; then
    printf -- '--- request ---\n' >&2
    jq . "$req_file" >&2
fi

# ─── Fire the request ───────────────────────────────────────────────────────

url="${SERVER%/}/${PROTOCOL}/${PROJECT}/search"

curl_opts=(-sS -o "$resp_file" -w '%{http_code}' -X POST "$url" -H 'Content-Type: application/json' --data-binary "@$req_file")
[[ "$NO_VERIFY" -eq 1 ]] && curl_opts+=(-k)

set +e
http_code=$(curl "${curl_opts[@]}")
curl_exit=$?
set -e

if [[ "$curl_exit" -ne 0 ]]; then
    die "request to $url failed (curl exit $curl_exit) — is the server reachable?"
fi

# ─── Handle non-200 status codes ────────────────────────────────────────────

# Errors are RFC 7807 application/problem+json: pull out "[code] detail" for a tidy
# message, falling back to the raw body for a non-JSON error (or empty if no body).
err_msg() {
    [[ -s "$resp_file" ]] || return 0
    jq -er 'if .code then "[\(.code)] \(.detail // .title // "")" else empty end' \
        "$resp_file" 2>/dev/null || body_text
}

body_text() {
    [[ -s "$resp_file" ]] && cat "$resp_file"
}

case "$http_code" in
    200) ;;
    400)
        msg=$(err_msg)
        die "400 Bad Request — invalid search request.${msg:+ Server said: $msg}"
        ;;
    404)
        printf 'No results: the project has no indexed (active) chunks matching your filters.\n' >&2
        exit 1
        ;;
    422)
        msg=$(err_msg)
        die "422 Unprocessable Entity — request did not match the API schema.${msg:+ Server said: $msg}"
        ;;
    499)
        die "499 — the server cancelled the request (treated it as a client disconnect)."
        ;;
    503)
        msg=$(err_msg)
        die "503 Service Unavailable — the embedding model server or Qdrant is down. Try again shortly.${msg:+ Server said: $msg}"
        ;;
    500)
        msg=$(err_msg)
        die "500 Internal Server Error.${msg:+ Server said: $msg}"
        ;;
    *)
        msg=$(err_msg)
        die "unexpected HTTP $http_code.${msg:+ Server said: $msg}"
        ;;
esac

# ─── Render results ─────────────────────────────────────────────────────────

result_count=$(jq '.results | length' "$resp_file")
if [[ "$result_count" -eq 0 ]]; then
    printf 'No results.\n' >&2
    exit 1
fi

c() { # c CODE TEXT — wraps TEXT in an SGR code if colors are enabled
    if [[ "$USE_COLOR" -eq 1 ]]; then
        printf '\033[%sm%s\033[0m' "$1" "$2"
    else
        printf '%s' "$2"
    fi
}

ext_to_lexer() {
    case "$1" in
        *.rs) echo rust ;;
        *.py) echo python ;;
        *.js | *.mjs | *.cjs | *.jsx) echo javascript ;;
        *.ts | *.mts | *.cts) echo typescript ;;
        *.tsx) echo tsx ;;
        *.go) echo go ;;
        *.c | *.h) echo c ;;
        *.cpp | *.cc | *.cxx | *.hpp) echo cpp ;;
        *.java) echo java ;;
        *.cs) echo csharp ;;
        *.rb) echo ruby ;;
        *.php) echo php ;;
        *.sh | *.bash) echo bash ;;
        *.html | *.htm) echo html ;;
        *.css) echo css ;;
        *.json) echo json ;;
        *.scala | *.sc) echo scala ;;
        *.hs | *.lhs) echo haskell ;;
        *.ml | *.mli) echo ocaml ;;
        *.zig) echo zig ;;
        *.sql) echo sql ;;
        *) echo "" ;;
    esac
}

print_result() {
    local score path code start_line end_line lexer

    score=$(jq -r '.score' <<<"$1")
    path=$(jq -r '.path' <<<"$1")
    code=$(jq -r '.code' <<<"$1")
    start_line=$(jq -r '.start_line' <<<"$1")
    end_line=$(jq -r '.end_line' <<<"$1")

    printf '%s\n' "$(c '2;37' "────────────────────────────────────────────────────────────────")"
    printf '%s%s%s  %s\n' \
        "$(c '1;36' "$path")" \
        "$(c '2;37' ":")" \
        "$(c '1;33' "${start_line}-${end_line}")" \
        "$(c '1;32' "score=$(printf '%.4f' "$score")")"

    lexer=$(ext_to_lexer "$path")

    if [[ "$HAVE_PYGMENTIZE" -eq 1 && "$USE_COLOR" -eq 1 && -n "$lexer" ]]; then
        printf '%s' "$code" | pygmentize -l "$lexer" -f terminal256 -O "style=$THEME" 2>/dev/null |
            awk -v start="$start_line" -v use_color="$USE_COLOR" '
                {
                    if (use_color == 1)
                        printf "\033[2;37m%6d\033[0m \033[2;37m│\033[0m %s\n", start + NR - 1, $0
                    else
                        printf "%6d │ %s\n", start + NR - 1, $0
                }'
    else
        printf '%s' "$code" | awk -v start="$start_line" '
            { printf "%6d │ %s\n", start + NR - 1, $0 }'
    fi
    printf '\n'
}

# Print in ascending score order: lowest first, best match last, right above
# the prompt at the bottom of the screen.
jq -c '.results | sort_by(.score) | .[]' "$resp_file" | while IFS= read -r result; do
    print_result "$result"
done
