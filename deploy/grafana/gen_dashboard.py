"""Generate deploy/grafana/mindex-service.json.

Hand-writing 45 Grafana panels is how a dashboard ends up with three different
units for seconds and two spellings of the same query. This builds them from a
few helpers instead, so the conventions hold by construction:

  * every query goes through the `$ds` datasource variable and filters with
    `=~"$project"` / `=~"$route"` / `=~"$model"`, so "All" and a hand-typed regex
    both work;
  * one unit per panel and never two y-axes;
  * colours follow the *entity* (a `done_reason`, a file `status`) via explicit
    overrides, so filtering a series out never repaints the survivors, and status
    hues are reserved for things that actually mean good/bad.
"""

import json

DS = "${ds}"
# Named Grafana colours rather than hex: they are theme-aware, so the dashboard
# reads correctly in both light and dark without a second palette.
FILE_STATUS = {
    "indexed": "green",
    "indexing": "blue",
    "just_uploaded": "light-blue",
    "cancelled": "yellow",
    "failed": "red",
    "deleted": "text",
}
DONE_REASON = {
    "finalized": "green",
    "time_exhausted": "yellow",
    "tokens_exhausted": "orange",
    "budget_exhausted": "purple",
    "context_exhausted": "blue",
    "unparseable": "red",
    "repeated_calls": "dark-red",
}
RETRY_OUTCOME = {
    "indexed": "green",
    "failed": "red",
    "skipped_claim": "blue",
    "non_retryable": "purple",
    "zero_chunk": "yellow",
    "corrupt_guid": "dark-red",
    "cancelled": "text",
    "error": "orange",
}
CITATION_CLASS = {
    "verified": "green",
    "path_only": "yellow",
    "unverified": "red",
    "stale": "orange",
}
INDEX_OUTCOME = {
    "indexed": "green",
    "skipped_unchanged": "blue",
    "in_flight": "purple",
    "cancelled": "yellow",
    "failed": "red",
}

_id = [0]


def nid():
    _id[0] += 1
    return _id[0]


def target(expr, legend="", instant=False):
    return {
        "datasource": {"type": "prometheus", "uid": DS},
        "editorMode": "code",
        "expr": expr,
        "legendFormat": legend or "__auto",
        "range": not instant,
        "instant": instant,
        "refId": "A",
    }


def targets(*specs):
    out = []
    for i, (expr, legend) in enumerate(specs):
        t = target(expr, legend)
        t["refId"] = chr(65 + i)
        out.append(t)
    return out


def by_name_overrides(mapping):
    """Pin a colour to a series *name*, so identity survives a filter."""
    return [
        {
            "matcher": {"id": "byName", "options": name},
            "properties": [
                {"id": "color", "value": {"mode": "fixed", "fixedColor": colour}}
            ],
        }
        for name, colour in mapping.items()
    ]


def panel(
    kind,
    title,
    gp,
    tgts,
    unit=None,
    desc="",
    opts=None,
    custom=None,
    overrides=None,
    thresholds=None,
    mappings=None,
    decimals=None,
):
    defaults = {"color": {"mode": "palette-classic"}, "custom": custom or {}}
    if unit:
        defaults["unit"] = unit
    if decimals is not None:
        defaults["decimals"] = decimals
    if thresholds:
        defaults["color"] = {"mode": "thresholds"}
        defaults["thresholds"] = {"mode": "absolute", "steps": thresholds}
    else:
        defaults["thresholds"] = {
            "mode": "absolute",
            "steps": [{"color": "green", "value": None}],
        }
    if mappings:
        defaults["mappings"] = mappings
    p = {
        "id": nid(),
        "type": kind,
        "title": title,
        "description": desc,
        "datasource": {"type": "prometheus", "uid": DS},
        "gridPos": gp,
        "targets": tgts,
        "fieldConfig": {"defaults": defaults, "overrides": overrides or []},
        "options": opts or {},
    }
    return p


# ── panel shorthands ────────────────────────────────────────────────────────

LEGEND_TABLE = {
    "showLegend": True,
    "displayMode": "table",
    "placement": "bottom",
    "calcs": ["lastNotNull", "max"],
}
LEGEND_LIST = {
    "showLegend": True,
    "displayMode": "list",
    "placement": "bottom",
    "calcs": [],
}
# For counted events the useful legend column is the range total, not the last
# bar — a panel showing three bars in six hours has no meaningful "last value".
LEGEND_TABLE_SUM = {
    "showLegend": True,
    "displayMode": "table",
    "placement": "bottom",
    "calcs": ["sum", "max"],
}

# Thin marks, recessive fills: the line carries the series, the fill only groups.
TS_CUSTOM = {
    "drawStyle": "line",
    "lineWidth": 2,
    "fillOpacity": 8,
    "showPoints": "never",
    "spanNulls": True,
    "axisSoftMin": 0,
}
STACK_CUSTOM = dict(
    TS_CUSTOM, fillOpacity=55, lineWidth=1, stacking={"mode": "normal", "group": "A"}
)
# Per-run events are COUNTED, never rated, and that is a correctness matter
# rather than a taste one. Every research family is a labelled `Family` created
# lazily by the first run carrying that label set, so its first scraped sample
# is already 1: there is no preceding 0 for `rate()` to subtract from, and a
# label set that sees exactly one run in a process lifetime — the normal case
# for {model, done_reason} — stays flat at 1 until restart. `rate()` over that
# is 0 for the whole life of the series, which is how a row of research panels
# reads as empty while `research_runs` plainly holds the runs. `increase()`
# counts the first sample of a newly-appearing counter, so it sees them.
# Bars, because a handful of runs a day drawn as a per-second line is a needle.
BARS_CUSTOM = dict(
    TS_CUSTOM, drawStyle="bars", fillOpacity=70, lineWidth=1, barAlignment=0
)
BARS_STACK_CUSTOM = dict(BARS_CUSTOM, stacking={"mode": "normal", "group": "A"})
# A quantile over a rare histogram is defined only in the windows that contain a
# run, so the marks must be points and the gaps must stay gaps.
POINTS_CUSTOM = dict(TS_CUSTOM, showPoints="always", pointSize=7, spanNulls=False)


def ev(expr, window="$__rate_interval"):
    """Counted-event form of a counter — see BARS_CUSTOM for why not `rate`."""
    return f"increase({expr}[{window}])"


def ts(
    title, gp, tgts, unit="short", desc="", custom=None, overrides=None, legend=None
):
    return panel(
        "timeseries",
        title,
        gp,
        tgts,
        unit=unit,
        desc=desc,
        custom=dict(TS_CUSTOM, **(custom or {})),
        overrides=overrides,
        # A shared crosshair across the whole dashboard makes two panels
        # comparable at a glance; a single-series panel needs no legend box.
        opts={
            "legend": legend or LEGEND_LIST,
            "tooltip": {"mode": "multi", "sort": "desc"},
        },
    )


def stat(
    title,
    gp,
    expr,
    unit="short",
    desc="",
    thresholds=None,
    mappings=None,
    decimals=None,
    colour_mode="value",
):
    return panel(
        "stat",
        title,
        gp,
        [target(expr, instant=True)],
        unit=unit,
        desc=desc,
        thresholds=thresholds,
        mappings=mappings,
        decimals=decimals,
        opts={
            "reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
            "colorMode": colour_mode,
            "graphMode": "none",
            "textMode": "auto",
            "justifyMode": "auto",
        },
    )


def heat(title, gp, expr, unit="short", desc=""):
    """A histogram over time. One hue, light→dark — magnitude, not identity."""
    return {
        "id": nid(),
        "type": "heatmap",
        "title": title,
        "description": desc,
        "datasource": {"type": "prometheus", "uid": DS},
        "gridPos": gp,
        "targets": [
            dict(target(expr, "{{le}}"), format="heatmap"),
        ],
        "options": {
            "calculate": False,
            "cellGap": 1,
            "color": {
                "mode": "scheme",
                "scheme": "Blues",
                "steps": 32,
                "reverse": False,
                "exponent": 0.5,
                "fill": "dark-blue",
            },
            "yAxis": {"unit": unit, "axisPlacement": "left"},
            "legend": {"show": True},
            "tooltip": {"mode": "single", "yHistogram": True},
            "rowsFrame": {"layout": "auto"},
            "filterValues": {"le": 1e-9},
        },
        "fieldConfig": {"defaults": {"custom": {"hideFrom": {}}}, "overrides": []},
    }


def row(title, y):
    return {
        "id": nid(),
        "type": "row",
        "title": title,
        "collapsed": False,
        "gridPos": {"h": 1, "w": 24, "x": 0, "y": y},
        "panels": [],
    }


def gp(h, w, x, y):
    return {"h": h, "w": w, "x": x, "y": y}


P = []
y = 0

# ─── Row 1 — Overview ───────────────────────────────────────────────────────
P.append(row("Overview", y))
y += 1

P += [
    stat(
        "Service",
        gp(4, 3, 0, y),
        'up{job="mindex"}',
        mappings=[
            {
                "type": "value",
                "options": {
                    "0": {"text": "DOWN", "index": 0},
                    "1": {"text": "UP", "index": 1},
                },
            }
        ],
        thresholds=[{"color": "red", "value": None}, {"color": "green", "value": 1}],
        colour_mode="background",
        desc="Is the scrape succeeding at all. Everything else on this dashboard "
        "is meaningless while this is DOWN.",
    ),
    stat(
        "Uptime",
        gp(4, 3, 3, y),
        'time() - mindex_start_time_seconds{job="mindex"}',
        unit="s",
        desc="Since the process started serving. A sawtooth here is a crash loop.",
    ),
    stat(
        "Projects",
        gp(4, 3, 6, y),
        # Counted over per-project series, not `mindex_projects`: that gauge counts
        # database rows, and a project with zero files emits no series at all — so
        # the stat read 3 while the dropdown and the Projects table both showed 2,
        # which looked like "All" being counted as a project. This form always
        # agrees with the rest of the dashboard and respects the filter.
        'count(count by (project_guid) (mindex_project_files{project_guid=~"$project"}))',
        desc="Projects with at least one file, matching $project. "
        "`mindex_projects` (the raw DB row count) can be higher when a "
        "project exists but holds no files.",
    ),
    stat(
        "Indexed files",
        gp(4, 3, 9, y),
        'sum(mindex_project_files{project_guid=~"$project", status="indexed"})',
        desc="Files currently in the `indexed` state, across $project.",
    ),
    stat(
        "Active chunks",
        gp(4, 3, 12, y),
        'sum(mindex_project_chunks_active{project_guid=~"$project", language=~"$language"})',
        desc="Chunks with a live vector — the searchable corpus.",
    ),
    stat(
        "Failed files",
        gp(4, 3, 15, y),
        'sum(mindex_project_files{project_guid=~"$project", status="failed"})',
        thresholds=[{"color": "green", "value": None}, {"color": "orange", "value": 1}],
        colour_mode="background",
        desc="Files in `failed`. The retry worker is still trying, unless they "
        "also appear under 'Permanently failed' in GC & retry.",
    ),
    stat(
        "SQLite DB",
        gp(4, 3, 18, y),
        "mindex_db_size_bytes",
        unit="bytes",
        desc="page_count x page_size.",
    ),
    stat(
        "In flight",
        gp(4, 3, 21, y),
        "sum(mindex_http_requests_in_flight)",
        desc="Requests being served right now. Long-lived /research streams live "
        "here for minutes by design.",
    ),
]
y += 4

P += [
    ts(
        "Request rate",
        gp(7, 12, 0, y),
        targets(
            (
                'sum(rate(mindex_http_requests_total{route=~"$route"}[$__rate_interval]))',
                "all",
            ),
            (
                'sum(rate(mindex_http_requests_total{route=~"$route", status=~"4.."}[$__rate_interval]))',
                "4xx",
            ),
            (
                'sum(rate(mindex_http_requests_total{route=~"$route", status=~"5.."}[$__rate_interval]))',
                "5xx",
            ),
        ),
        unit="reqps",
        desc="Total, client errors and server errors on one axis — they share a unit.",
        overrides=by_name_overrides({"all": "blue", "4xx": "yellow", "5xx": "red"}),
    ),
    {
        "id": nid(),
        "type": "state-timeline",
        "title": "Dependencies",
        "description": "Sampled by the metrics collector on its own tick, off the "
        "request path — so this is availability over time, not a "
        "snapshot taken when someone happened to call /health. "
        "`query_embedder` appears only when the deployment is split.",
        "datasource": {"type": "prometheus", "uid": DS},
        "gridPos": gp(7, 12, 12, y),
        "targets": [target("mindex_dependency_up", "{{dependency}}")],
        "fieldConfig": {
            "defaults": {
                "color": {"mode": "thresholds"},
                "custom": {"fillOpacity": 80, "lineWidth": 0},
                "thresholds": {
                    "mode": "absolute",
                    "steps": [
                        {"color": "red", "value": None},
                        {"color": "green", "value": 1},
                    ],
                },
                "mappings": [
                    {
                        "type": "value",
                        "options": {
                            "0": {"text": "down", "index": 0},
                            "1": {"text": "up", "index": 1},
                        },
                    }
                ],
            },
            "overrides": [],
        },
        "options": {
            "showValue": "never",
            "mergeValues": True,
            "alignValue": "left",
            "rowHeight": 0.9,
            "legend": LEGEND_LIST,
            "tooltip": {"mode": "single"},
        },
    },
]
y += 7

P += [
    ts(
        "SQLite pool",
        gp(6, 8, 0, y),
        targets(
            ("mindex_db_pool_available", "available"),
            ("mindex_db_pool_size", "size"),
        ),
        desc="`available` riding at zero is the shape that precedes PoolEmpty. "
        "The counter below it is the confirmation.",
        overrides=by_name_overrides({"available": "blue", "size": "text"}),
    ),
    ts(
        "Pool exhaustion & claim conflicts",
        gp(6, 8, 8, y),
        targets(
            (
                "rate(mindex_db_pool_acquire_failures_total[$__rate_interval])",
                "pool empty",
            ),
            (
                "rate(mindex_index_claim_conflicts_total[$__rate_interval])",
                "claim conflict",
            ),
        ),
        unit="cps",
        desc="Pool exhaustion answers 503 `database.busy` and logs a hint, so "
        "this is the trend rather than the only evidence. A claim conflict is "
        "two writers on one file; the request still succeeds for the rest of "
        "its batch.",
        overrides=by_name_overrides({"pool empty": "red", "claim conflict": "orange"}),
    ),
    ts(
        "Concurrency",
        gp(6, 8, 16, y),
        targets(
            ("mindex_indexing_claims", "indexing claims"),
            ("mindex_research_active", "research active"),
            ("mindex_gc_running", "gc running"),
            # Minutes rather than raw seconds: the number matters only when it is
            # large, and at panel scale a healthy run's seconds would flatten the
            # lock lines beside it.
            ("mindex_research_inflight_oldest_age_seconds / 60", "oldest run (min)"),
            # Expected to stay absent (a never-incremented counter has no series):
            # any point here means a run outlived every deadline it had and the
            # watchdog freed its slot.
            (
                "increase(mindex_research_watchdog_cancels_total[$__rate_interval])",
                "watchdog cancel",
            ),
        ),
        desc="The three in-process locks, plus the two research-slot pathology "
        "signals. `research_active` is derived from the semaphore rather than "
        "counted, because a run's normal exit is a dropped stream that no "
        "decrement would survive; `oldest run` is what tells a busy slot from a "
        "wedged one, and `watchdog cancel` should never fire.",
        overrides=by_name_overrides(
            {
                "indexing claims": "blue",
                "research active": "purple",
                "gc running": "yellow",
                "oldest run (min)": "green",
                "watchdog cancel": "red",
            }
        ),
    ),
]
y += 6

P += [
    ts(
        "Background workers alive",
        gp(6, 12, 0, y),
        targets(("mindex_worker_running", "{{worker}}")),
        desc="Every worker is a detached task: a panic inside one stops it "
        "permanently, and the only other symptom is some unrelated gauge "
        "quietly ceasing to move. A line that drops to 0 while the process "
        "keeps serving is that, and it needs a restart.",
    ),
    ts(
        "Worker deaths",
        gp(6, 12, 12, y),
        targets(
            (
                (
                    'increase(mindex_worker_exits_total{outcome="panic"}'
                    "[$__rate_interval])"
                ),
                "{{worker}}",
            ),
        ),
        unit="short",
        desc="`increase`, not `rate`: a worker dies at most once per process "
        "lifetime, so the series is flat at 1 forever and a rate over it reads "
        "as zero. Expected to stay empty.",
    ),
]
y += 6

# ─── Row 2 — HTTP API ───────────────────────────────────────────────────────
P.append(row("HTTP API", y))
y += 1

P += [
    ts(
        "Requests by route",
        gp(8, 12, 0, y),
        targets(
            (
                'sum by (route) (rate(mindex_http_requests_total{route=~"$route"}[$__rate_interval]))',
                "{{route}}",
            ),
        ),
        unit="reqps",
        custom=STACK_CUSTOM,
        legend=LEGEND_TABLE,
    ),
    ts(
        "Latency percentiles",
        gp(8, 12, 12, y),
        targets(
            (
                'histogram_quantile(0.50, sum by (le) (rate(mindex_http_request_duration_seconds_bucket{route=~"$route"}[$__rate_interval])))',
                "p50",
            ),
            (
                'histogram_quantile(0.95, sum by (le) (rate(mindex_http_request_duration_seconds_bucket{route=~"$route"}[$__rate_interval])))',
                "p95",
            ),
            (
                'histogram_quantile(0.99, sum by (le) (rate(mindex_http_request_duration_seconds_bucket{route=~"$route"}[$__rate_interval])))',
                "p99",
            ),
        ),
        unit="s",
        overrides=by_name_overrides({"p50": "green", "p95": "yellow", "p99": "orange"}),
    ),
]
y += 8

P += [
    heat(
        "Latency distribution",
        gp(8, 12, 0, y),
        'sum by (le) (rate(mindex_http_request_duration_seconds_bucket{route=~"$route"}[$__rate_interval]))',
        unit="s",
        desc="The percentile lines hide bimodality; this does not.",
    ),
    {
        "id": nid(),
        "type": "table",
        "title": "Error codes",
        "description": "The stable machine `code` from the problem+json envelope, "
        "carried on the response extensions. `request.cancelled` at "
        "499 is a client disconnect, not a fault.",
        "datasource": {"type": "prometheus", "uid": DS},
        "gridPos": gp(8, 12, 12, y),
        "targets": [
            dict(
                target(
                    'sum by (code, route, status) (increase(mindex_http_requests_total{code!="", route=~"$route"}[$__range]))',
                    instant=True,
                ),
                format="table",
            )
        ],
        "transformations": [
            {
                "id": "organize",
                "options": {
                    "excludeByName": {"Time": True, "job": True, "instance": True},
                    "renameByName": {"Value": "count"},
                },
            },
            {
                "id": "sortBy",
                "options": {"fields": {}, "sort": [{"field": "count", "desc": True}]},
            },
        ],
        "fieldConfig": {
            "defaults": {
                "custom": {"align": "auto", "filterable": True},
                "decimals": 0,
            },
            "overrides": [],
        },
        "options": {"showHeader": True, "footer": {"show": False}},
    },
]
y += 8

P += [
    ts(
        "Transport",
        gp(6, 8, 0, y),
        targets(
            (
                "sum by (proto) (rate(mindex_http_requests_by_proto_total[$__rate_interval]))",
                "{{proto}}",
            ),
        ),
        unit="reqps",
        desc="Whether anyone actually reaches the HTTP/3 listener.",
        custom=STACK_CUSTOM,
    ),
    ts(
        "In-flight by route",
        gp(6, 8, 8, y),
        targets(("mindex_http_requests_in_flight", "{{route}}")),
        desc="A route stuck above zero with no traffic is a leaked gauge — which "
        "is exactly what the Drop guard exists to prevent.",
    ),
    ts(
        "Error ratio",
        gp(6, 8, 16, y),
        targets(
            (
                'sum(rate(mindex_http_requests_total{status=~"5..", route=~"$route"}[$__rate_interval])) / clamp_min(sum(rate(mindex_http_requests_total{route=~"$route"}[$__rate_interval])), 0.0001)',
                "5xx share",
            ),
        ),
        unit="percentunit",
        overrides=by_name_overrides({"5xx share": "red"}),
    ),
]
y += 6

# ─── Row 3 — Indexing ───────────────────────────────────────────────────────
P.append(row("Indexing", y))
y += 1

P += [
    ts(
        "Files by outcome",
        gp(8, 12, 0, y),
        targets(
            (
                'sum by (outcome) (rate(mindex_index_files_total{project_guid=~"$project", language=~"$language"}[$__rate_interval]))',
                "{{outcome}}",
            ),
        ),
        unit="cps",
        desc="`skipped_unchanged` dominating is the hash skip working. "
        "`in_flight` is claim contention, not an error.",
        custom=STACK_CUSTOM,
        overrides=by_name_overrides(INDEX_OUTCOME),
        legend=LEGEND_TABLE,
    ),
    ts(
        "Derived rows produced",
        gp(8, 12, 12, y),
        targets(
            (
                'sum(rate(mindex_index_chunks_total{project_guid=~"$project", language=~"$language"}[$__rate_interval]))',
                "chunks",
            ),
            (
                'sum(rate(mindex_index_symbols_total{project_guid=~"$project", language=~"$language"}[$__rate_interval]))',
                "symbols",
            ),
        ),
        unit="cps",
        overrides=by_name_overrides({"chunks": "blue", "symbols": "purple"}),
    ),
]
y += 8

P += [
    ts(
        "Chunks by language",
        gp(8, 12, 0, y),
        targets(
            (
                'sum by (language) (rate(mindex_index_chunks_total{project_guid=~"$project", language=~"$language"}[$__rate_interval]))',
                "{{language}}",
            ),
        ),
        unit="cps",
        custom=STACK_CUSTOM,
        legend=LEGEND_TABLE,
    ),
    ts(
        "Phase latency (p95)",
        gp(8, 12, 12, y),
        targets(
            (
                "histogram_quantile(0.95, sum by (le, phase) (rate(mindex_index_phase_duration_seconds_bucket[$__rate_interval])))",
                "{{phase}}",
            ),
        ),
        unit="s",
        desc="prepare = slice + insert; embed = the GPU pass; mark = the status "
        "writes. An embed phase that dwarfs the others is the normal shape.",
        overrides=by_name_overrides(
            {"prepare": "blue", "embed": "purple", "mark": "green"}
        ),
    ),
]
y += 8

P += [
    heat(
        "Submitted file size",
        gp(8, 12, 0, y),
        'sum by (le) (rate(mindex_index_file_size_bytes_bucket{language=~"$language"}[$__rate_interval]))',
        unit="bytes",
        desc="Language-labelled but never project-labelled — a histogram per "
        "project would multiply the series count for a breakdown nobody reads.",
    ),
    heat(
        "Chunks per file",
        gp(8, 12, 12, y),
        'sum by (le) (rate(mindex_index_file_chunks_bucket{language=~"$language"}[$__rate_interval]))',
        desc="A spike at the 1-2 bucket means files are slicing to almost nothing.",
    ),
]
y += 8

# ─── Row 4 — Embedder & Qdrant ──────────────────────────────────────────────
P.append(row("Embedder & Qdrant", y))
y += 1

P += [
    heat(
        "Embed batch size",
        gp(8, 12, 0, y),
        "sum by (le) (rate(mindex_embed_batch_texts_bucket[$__rate_interval]))",
        desc="Texts per /encode call. Indexing should cluster near "
        "`[indexing].embed_batch_chunks`; queries are always 1.",
    ),
    ts(
        "Embed latency (p95)",
        gp(8, 12, 12, y),
        targets(
            (
                "histogram_quantile(0.95, sum by (le, embedder) (rate(mindex_embed_duration_seconds_bucket[$__rate_interval])))",
                "{{embedder}}",
            ),
        ),
        unit="s",
        desc="`index` and `query` are separate series even when one server does "
        "both — that is the split `[model].query_server_url` would create.",
        overrides=by_name_overrides({"index": "purple", "query": "blue"}),
    ),
]
y += 8

P += [
    ts(
        "Embedder outcomes",
        gp(7, 8, 0, y),
        targets(
            (
                "sum by (outcome) (rate(mindex_embed_requests_total[$__rate_interval]))",
                "{{outcome}}",
            ),
        ),
        unit="cps",
        custom=STACK_CUSTOM,
        overrides=by_name_overrides(
            {"ok": "green", "cancelled": "text", "request": "red", "decode": "dark-red"}
        ),
    ),
    ts(
        "Embedder 429 backoffs",
        gp(7, 8, 8, y),
        targets(
            ("sum(rate(mindex_embed_retries_total[$__rate_interval]))", "retries"),
        ),
        unit="cps",
        desc="Counted inside the client: from outside, three retries then a "
        "success is indistinguishable from one success.",
        overrides=by_name_overrides({"retries": "orange"}),
    ),
    ts(
        "Qdrant points",
        gp(7, 8, 16, y),
        targets(
            (
                "sum by (op) (rate(mindex_qdrant_points_total[$__rate_interval]))",
                "{{op}}",
            ),
        ),
        unit="cps",
        overrides=by_name_overrides(
            {"insert_batch": "green", "delete_batch": "orange"}
        ),
    ),
]
y += 7

P += [
    ts(
        "Qdrant latency (p95)",
        gp(7, 12, 0, y),
        targets(
            (
                "histogram_quantile(0.95, sum by (le, op) (rate(mindex_qdrant_op_duration_seconds_bucket[$__rate_interval])))",
                "{{op}}",
            ),
        ),
        unit="s",
        legend=LEGEND_TABLE,
    ),
    ts(
        "Qdrant errors",
        gp(7, 12, 12, y),
        targets(
            (
                'sum by (op) (rate(mindex_qdrant_ops_total{outcome="error"}[$__rate_interval]))',
                "{{op}}",
            ),
        ),
        unit="cps",
        desc="`delete_batch` errors here are the GC failures that keep chunk rows "
        "marked deleted for the next sweep — deliberately, since deleting the "
        "row first would orphan the vector forever.",
    ),
]
y += 7

# ─── Row 5 — Search ─────────────────────────────────────────────────────────
P.append(row("Search", y))
y += 1

P += [
    ts(
        "Searches by outcome",
        gp(8, 8, 0, y),
        targets(
            (
                'sum by (outcome) (rate(mindex_search_requests_total{project_guid=~"$project"}[$__rate_interval]))',
                "{{outcome}}",
            ),
        ),
        unit="cps",
        desc="`no_match` is an answer, not a failure — an empty candidate set "
        "returns 404 without ever calling Qdrant.",
        custom=STACK_CUSTOM,
        overrides=by_name_overrides(
            {"hit": "green", "no_match": "yellow", "error": "red"}
        ),
    ),
    ts(
        "Stage latency (p95)",
        gp(8, 8, 8, y),
        targets(
            (
                "histogram_quantile(0.95, sum by (le, stage) (rate(mindex_search_stage_duration_seconds_bucket[$__rate_interval])))",
                "{{stage}}",
            ),
        ),
        unit="s",
        desc="The two-SQL-queries-around-Qdrant shape, made visible: embed -> "
        "candidates -> qdrant -> fetch.",
        overrides=by_name_overrides(
            {
                "embed": "purple",
                "candidates": "blue",
                "qdrant": "orange",
                "fetch": "green",
            }
        ),
    ),
    ts(
        "No-match ratio",
        gp(8, 8, 16, y),
        targets(
            (
                'sum(rate(mindex_search_requests_total{outcome="no_match", project_guid=~"$project"}[$__rate_interval])) / clamp_min(sum(rate(mindex_search_requests_total{project_guid=~"$project"}[$__rate_interval])), 0.0001)',
                "no match",
            ),
        ),
        unit="percentunit",
        overrides=by_name_overrides({"no match": "yellow"}),
    ),
]
y += 8

P += [
    heat(
        "Candidate set size",
        gp(8, 12, 0, y),
        "sum by (le) (rate(mindex_search_candidates_bucket[$__rate_interval]))",
        desc="The has_id filter lists every candidate GUID, so this grows linearly "
        "with a project's active-chunk count — the known scaling limit.",
    ),
    heat(
        "Results returned",
        gp(8, 12, 12, y),
        "sum by (le) (rate(mindex_search_results_bucket[$__rate_interval]))",
    ),
]
y += 8

# ─── Row 6 — Research ───────────────────────────────────────────────────────
P.append(row("Research", y))
y += 1

P += [
    ts(
        "Runs by stop reason",
        gp(8, 12, 0, y),
        targets(
            (
                "sum by (done_reason) ("
                + ev('mindex_research_runs_total{model=~"$model"}')
                + ")",
                "{{done_reason}}",
            ),
        ),
        unit="short",
        desc="The 'is a budget binding?' panel. `finalized` means the model judged "
        "the evidence sufficient; anything else is a wall it hit. Counted, not "
        "rated: one run per label set per process lifetime is invisible to "
        "`rate()`.",
        custom=BARS_STACK_CUSTOM,
        overrides=by_name_overrides(DONE_REASON),
        legend=LEGEND_TABLE_SUM,
    ),
    ts(
        "Run duration",
        gp(8, 12, 12, y),
        targets(
            (
                "histogram_quantile(0.50, sum by (le, model) ("
                + ev('mindex_research_duration_seconds_bucket{model=~"$model"}')
                + "))",
                "p50 {{model}}",
            ),
            (
                "histogram_quantile(0.95, sum by (le, model) ("
                + ev('mindex_research_duration_seconds_bucket{model=~"$model"}')
                + "))",
                "p95 {{model}}",
            ),
        ),
        unit="s",
        desc="One point per window that actually contained a run — with a few "
        "runs a day there is nothing to join into a line.",
        custom=POINTS_CUSTOM,
    ),
]
y += 8

# These three were heatmaps and read as permanently empty: a handful of runs a
# day paints isolated one-interval columns, and the bucket axis runs to 8192
# while real values sit in the tens — the cells were invisible twice over. A
# rare histogram is a quantile-points panel here (the Run duration precedent),
# where the axis follows the data instead of the bucket table.
P += [
    ts(
        "Steps per run",
        gp(7, 8, 0, y),
        targets(
            (
                "histogram_quantile(0.50, sum by (le, model) ("
                + ev('mindex_research_steps_bucket{model=~"$model"}')
                + "))",
                "p50 {{model}}",
            ),
            (
                "histogram_quantile(0.95, sum by (le, model) ("
                + ev('mindex_research_steps_bucket{model=~"$model"}')
                + "))",
                "p95 {{model}}",
            ),
        ),
        unit="short",
        desc="A step is a poor unit — one turn may execute several, and `outline` "
        "is one indexed SELECT while `search` is a GPU embed plus a vector "
        "query. That is why it is the backstop and not the budget. One point "
        "per window that contained a run; the gaps are real.",
        custom=POINTS_CUSTOM,
    ),
    ts(
        "Turns per run",
        gp(7, 8, 8, y),
        targets(
            (
                "histogram_quantile(0.50, sum by (le, model) ("
                + ev('mindex_research_turns_bucket{model=~"$model"}')
                + "))",
                "p50 {{model}}",
            ),
            (
                "histogram_quantile(0.95, sum by (le, model) ("
                + ev('mindex_research_turns_bucket{model=~"$model"}')
                + "))",
                "p95 {{model}}",
            ),
        ),
        unit="short",
        desc="Turns above steps means turns that produced no step: rejected "
        "duplicates, or a model rephrasing instead of learning a name.",
        custom=POINTS_CUSTOM,
    ),
    ts(
        "Context used",
        gp(7, 8, 16, y),
        targets(
            (
                "histogram_quantile(0.50, sum by (le, model) ("
                + ev('mindex_research_context_used_ratio_bucket{model=~"$model"}')
                + "))",
                "p50 {{model}}",
            ),
            (
                "histogram_quantile(0.95, sum by (le, model) ("
                + ev('mindex_research_context_used_ratio_bucket{model=~"$model"}')
                + "))",
                "p95 {{model}}",
            ),
        ),
        unit="percentunit",
        desc="Peak prompt tokens over the run's num_ctx. Approaching 1.0 means "
        "Ollama is about to trim the transcript in silence.",
        custom=dict(POINTS_CUSTOM, axisSoftMax=1),
    ),
]
y += 7

P += [
    ts(
        "Tokens processed",
        gp(7, 8, 0, y),
        targets(
            (
                "sum by (kind) ("
                + ev('mindex_research_tokens_total{model=~"$model"}')
                + ")",
                "{{kind}}",
            ),
        ),
        unit="short",
        desc="The whole transcript is resent every turn, so prompt tokens grow "
        "super-linearly with turns. This is the real cost axis.",
        custom=BARS_STACK_CUSTOM,
        overrides=by_name_overrides({"prompt": "blue", "eval": "purple"}),
    ),
    ts(
        "Tool calls",
        gp(7, 8, 8, y),
        targets(
            (
                "sum by (tool) (" + ev("mindex_research_tool_calls_total") + ")",
                "{{tool}}",
            ),
        ),
        unit="short",
        desc="The intended path is list_files -> outline -> symbols/search/callers "
        "-> read_chunks. A run that only ever calls `search` never learned a name.",
        custom=BARS_STACK_CUSTOM,
        legend=LEGEND_TABLE_SUM,
    ),
    ts(
        "Tool latency (p95)",
        gp(7, 8, 16, y),
        targets(
            (
                "histogram_quantile(0.95, sum by (le, tool) ("
                + ev("mindex_research_tool_duration_seconds_bucket")
                + "))",
                "{{tool}}",
            ),
        ),
        unit="s",
        custom=POINTS_CUSTOM,
    ),
]
y += 7

P += [
    ts(
        "Citations by provenance",
        gp(7, 12, 0, y),
        targets(
            (
                "sum by (class) (" + ev("mindex_research_citations_total") + ")",
                "{{class}}",
            ),
        ),
        unit="short",
        desc="`unverified` = a path no tool returned this run, i.e. invented. "
        "`stale` is orthogonal to the other three: a citation can be "
        "impeccably verified and stale.",
        custom=BARS_STACK_CUSTOM,
        overrides=by_name_overrides(CITATION_CLASS),
        legend=LEGEND_TABLE_SUM,
    ),
    stat(
        "Unverified citation share",
        gp(7, 4, 12, y),
        # `or vector(0)` because the healthy case is that no `unverified` series
        # exists at all — without it the best possible reading renders as "No
        # data", which is indistinguishable from a broken query.
        '(sum(increase(mindex_research_citations_total{class="unverified"}[$__range])) or vector(0))'
        " / clamp_min(sum(increase(mindex_research_citations_total[$__range])), 1)",
        unit="percentunit",
        thresholds=[
            {"color": "green", "value": None},
            {"color": "yellow", "value": 0.02},
            {"color": "red", "value": 0.1},
        ],
        colour_mode="background",
        desc="Over the dashboard's time range. A model that answers from its "
        "weights rather than from the index shows up here and nowhere else.",
    ),
    ts(
        "Repairs & upstream faults",
        gp(7, 8, 16, y),
        targets(
            (
                ev("mindex_research_revalidations_total"),
                "revalidations",
            ),
            (
                ev("mindex_research_tool_call_parse_retries_total"),
                "ollama parse retries",
            ),
            (
                ev("mindex_research_transcript_truncations_total"),
                "transcript truncations",
            ),
        ),
        unit="short",
        desc="A revalidation is a draft sent back because its citations did not "
        "check out. A truncation means Ollama trimmed the transcript and "
        "streamed on — otherwise a completely silent failure.",
        overrides=by_name_overrides(
            {
                "revalidations": "yellow",
                "ollama parse retries": "orange",
                "transcript truncations": "red",
            }
        ),
        custom=BARS_CUSTOM,
    ),
]
y += 7

# The report phase, which is where runs were measured to fail: retrieval found the
# right files every time and the writing did not survive. These four families exist
# to answer whether bounding and sectioning the output changed that.
P += [
    ts(
        "Report length (words)",
        gp(7, 8, 0, y),
        targets(
            (
                "histogram_quantile(0.50, sum by (le, model) ("
                + ev('mindex_research_report_words_bucket{model=~"$model"}')
                + "))",
                "p50 {{model}}",
            ),
            (
                "histogram_quantile(0.95, sum by (le, model) ("
                + ev('mindex_research_report_words_bucket{model=~"$model"}')
                + "))",
                "p95 {{model}}",
            ),
        ),
        unit="short",
        custom=POINTS_CUSTOM,
        desc="Granted versus actual is the whole measurement: the per-effort "
        "max_report_words is announced to the model as a ceiling, and nothing "
        "makes it obey. If this sits wherever it likes regardless of the grant, "
        "the prompt half of that knob is dead weight and only num_predict earns "
        "its place.",
    ),
    ts(
        "Report sections",
        gp(7, 8, 8, y),
        targets(
            (
                "sum by (outcome) ("
                + ev("mindex_research_report_sections_total")
                + ")",
                "{{outcome}}",
            ),
        ),
        unit="short",
        desc="A report of 3+ plan items is written one section per turn, so one "
        "failing costs a section rather than the document. This says how often "
        "that trade is actually made. Rising `empty` means the per-section word "
        "budget or the model is wrong; `timed_out`/`skipped` mean "
        "report_timeout_ms is too tight for the plans being produced.",
        custom=BARS_STACK_CUSTOM,
        overrides=by_name_overrides(
            {
                "written": "green",
                "empty": "red",
                "timed_out": "orange",
                "skipped": "yellow",
            }
        ),
        legend=LEGEND_TABLE_SUM,
    ),
    ts(
        "Report-phase faults",
        gp(7, 8, 16, y),
        targets(
            (ev("mindex_research_report_length_caps_total"), "generation cut off"),
            (ev("mindex_research_report_context_sheds_total"), "prompt shed to fit"),
            (ev("mindex_research_forced_syntheses_total"), "server wrote the report"),
        ),
        unit="short",
        desc="All three are expected to stay at zero. `generation cut off` means "
        "num_predict fired, so REPORT_WORDS_TO_TOKENS or the model is wrong and "
        "a cut landed mid-token — which can sever a fence and cost a rewrite. "
        "`prompt shed` means the report turn's transcript was over the context "
        "ceiling and the server dropped old tool output rather than letting "
        "Ollama trim it in silence; on this hardware it may never fire, in which "
        "case it is insurance. `server wrote the report` means the report window "
        "expired first — those runs cite nothing, which is why the citation "
        "panels beside this one must be read together with it.",
        overrides=by_name_overrides(
            {
                "generation cut off": "red",
                "prompt shed to fit": "orange",
                "server wrote the report": "yellow",
            }
        ),
        custom=BARS_CUSTOM,
        legend=LEGEND_TABLE_SUM,
    ),
]
y += 7

P += [
    ts(
        "Stored research",
        gp(7, 12, 0, y),
        targets(
            ("sum(mindex_project_research_runs)", "stored"),
            ("sum(mindex_project_research_stale)", "outdated"),
            ("sum(mindex_project_research_pinned)", "pinned"),
        ),
        unit="short",
        desc="The corpus a new run can be given as context. `outdated` counts runs "
        "at least one of whose files has changed since — still useful for names, "
        "unreliable on specifics. `pinned` runs have no expiry and GC never "
        "reaps them, so a rising pinned count is the thing that stops retention "
        "from bounding this table.",
        overrides=by_name_overrides(
            {"stored": "blue", "outdated": "yellow", "pinned": "green"}
        ),
    ),
    ts(
        "Context reuse",
        gp(7, 12, 12, y),
        targets(
            (
                "sum(" + ev("mindex_research_runs_with_context_total") + ")",
                "runs given context",
            ),
            (ev("mindex_research_context_runs_used_total"), "reports injected"),
            (ev("mindex_research_context_truncations_total"), "truncated to fit"),
            (ev("mindex_gc_research_pruned_total"), "reaped by GC"),
        ),
        unit="short",
        desc="Does prior research get reused, and does the character cap bite? "
        "Counted with increase() and drawn as bars: these are a handful of "
        "events a day, and a label set seeing one event in a process lifetime "
        "is flat at 1 under rate() forever.",
        overrides=by_name_overrides(
            {
                "runs given context": "blue",
                "reports injected": "purple",
                "truncated to fit": "orange",
                "reaped by GC": "text",
            }
        ),
        custom=BARS_CUSTOM,
        legend=LEGEND_TABLE_SUM,
    ),
]
y += 7

# ─── Row 7 — GC & retry ─────────────────────────────────────────────────────
P.append(row("GC & retry", y))
y += 1

P += [
    ts(
        "GC rows removed",
        gp(7, 8, 0, y),
        targets(
            ("rate(mindex_gc_chunks_removed_total[$__rate_interval])", "chunks"),
            ("rate(mindex_gc_files_pruned_total[$__rate_interval])", "files"),
            ("rate(mindex_gc_status_log_pruned_total[$__rate_interval])", "status log"),
        ),
        unit="cps",
        desc="Only chunks whose Qdrant delete was confirmed are hard-deleted; the "
        "rest keep their rows for the next sweep.",
        overrides=by_name_overrides(
            {"chunks": "blue", "files": "purple", "status log": "text"}
        ),
    ),
    ts(
        "GC backlog",
        gp(7, 8, 8, y),
        targets(
            (
                'sum(mindex_project_chunks_deleted{project_guid=~"$project"})',
                "awaiting GC",
            ),
        ),
        desc="Soft-deleted chunks whose vectors still sit in Qdrant. Indexing is "
        "append-only, so this rises on every reindex and falls on the hourly "
        "sweep. A monotonic rise means the sweep is failing.",
        overrides=by_name_overrides({"awaiting GC": "orange"}),
    ),
    ts(
        "GC passes",
        gp(7, 8, 16, y),
        targets(
            (
                "sum by (trigger) (rate(mindex_gc_runs_total[$__rate_interval]))",
                "{{trigger}}",
            ),
        ),
        unit="cps",
        overrides=by_name_overrides({"worker": "blue", "manual": "purple"}),
    ),
]
y += 7

P += [
    ts(
        "Retry outcomes",
        gp(7, 12, 0, y),
        targets(
            (
                "sum by (outcome) (rate(mindex_retry_files_total[$__rate_interval]))",
                "{{outcome}}",
            ),
        ),
        unit="cps",
        desc="`skipped_claim` dominating means the worker is racing live /index "
        "traffic, which is different from it failing.",
        custom=STACK_CUSTOM,
        overrides=by_name_overrides(RETRY_OUTCOME),
        legend=LEGEND_TABLE,
    ),
    ts(
        "Permanently failed files",
        gp(7, 12, 12, y),
        targets(
            (
                'sum by (project_guid) (mindex_project_files_permanently_failed{project_guid=~"$project"})',
                "{{project_guid}}",
            ),
        ),
        desc="Files that exhausted `[workers].max_retries`. The retry worker will "
        "never touch them again — they need a re-push. Until now this existed "
        "only as an hourly WARN.",
        overrides=by_name_overrides({}),
    ),
]
y += 7

# ─── Row 8 — Per-project state ──────────────────────────────────────────────
P.append(row("Per-project state", y))
y += 1

P.append(
    {
        "id": nid(),
        "type": "table",
        "title": "Projects",
        "description": "Recomputed by the metrics collector every "
        "`[metrics].refresh_interval_seconds`, and cleared and "
        "repopulated each tick — so a deleted project disappears "
        "here rather than reporting its last known state forever.",
        "datasource": {"type": "prometheus", "uid": DS},
        "gridPos": gp(9, 24, 0, y),
        "targets": [
            dict(
                target(
                    f'sum by (project_guid) (mindex_project_files{{project_guid=~"$project", status="{s}"}})',
                    instant=True,
                ),
                format="table",
                refId=r,
            )
            for s, r in [
                ("indexed", "A"),
                ("indexing", "B"),
                ("failed", "C"),
                ("deleted", "D"),
            ]
        ]
        + [
            dict(
                target(
                    'sum by (project_guid) (mindex_project_chunks_active{project_guid=~"$project"})',
                    instant=True,
                ),
                format="table",
                refId="E",
            ),
            dict(
                target(
                    'sum by (project_guid) (mindex_project_symbols{project_guid=~"$project"})',
                    instant=True,
                ),
                format="table",
                refId="F",
            ),
            dict(
                target(
                    'sum by (project_guid) (mindex_project_chunks_deleted{project_guid=~"$project"})',
                    instant=True,
                ),
                format="table",
                refId="G",
            ),
            dict(
                target(
                    'time() - max by (project_guid) (mindex_project_last_indexed_timestamp_seconds{project_guid=~"$project"})',
                    instant=True,
                ),
                format="table",
                refId="H",
            ),
        ],
        "transformations": [
            {
                "id": "joinByField",
                "options": {"byField": "project_guid", "mode": "outer"},
            },
            {
                "id": "organize",
                "options": {
                    "excludeByName": {
                        "Time": True,
                        "Time 1": True,
                        "Time 2": True,
                        "Time 3": True,
                        "Time 4": True,
                        "Time 5": True,
                        "Time 6": True,
                        "Time 7": True,
                        "Time 8": True,
                    },
                    "renameByName": {
                        "project_guid": "project",
                        "Value #A": "indexed",
                        "Value #B": "indexing",
                        "Value #C": "failed",
                        "Value #D": "deleted",
                        "Value #E": "chunks",
                        "Value #F": "symbols",
                        "Value #G": "awaiting GC",
                        "Value #H": "last indexed",
                    },
                },
            },
        ],
        "fieldConfig": {
            "defaults": {
                "custom": {"align": "auto", "filterable": True},
                "decimals": 0,
            },
            "overrides": [
                {
                    "matcher": {"id": "byName", "options": "last indexed"},
                    "properties": [
                        {"id": "unit", "value": "s"},
                        {"id": "custom.cellOptions", "value": {"type": "color-text"}},
                        {
                            "id": "thresholds",
                            "value": {
                                "mode": "absolute",
                                "steps": [
                                    {"color": "green", "value": None},
                                    {"color": "yellow", "value": 86400},
                                    {"color": "orange", "value": 604800},
                                ],
                            },
                        },
                    ],
                },
                {
                    "matcher": {"id": "byName", "options": "failed"},
                    "properties": [
                        {"id": "custom.cellOptions", "value": {"type": "color-text"}},
                        {
                            "id": "thresholds",
                            "value": {
                                "mode": "absolute",
                                "steps": [
                                    {"color": "text", "value": None},
                                    {"color": "orange", "value": 1},
                                ],
                            },
                        },
                    ],
                },
            ],
        },
        "options": {
            "showHeader": True,
            "footer": {"show": False},
            "sortBy": [{"displayName": "chunks", "desc": True}],
        },
    }
)
y += 9

P += [
    {
        "id": nid(),
        "type": "bargauge",
        "title": "Files by language",
        "description": "A bar rather than a pie: there are more than a handful of "
        "languages and their values are close, which is exactly "
        "where a pie stops being readable.",
        "datasource": {"type": "prometheus", "uid": DS},
        "gridPos": gp(8, 8, 0, y),
        "targets": [
            target(
                'sum by (language) (mindex_project_files_by_language{project_guid=~"$project", language=~"$language"})',
                "{{language}}",
                instant=True,
            )
        ],
        "fieldConfig": {
            "defaults": {
                # One series, one colour: the bar length already encodes magnitude,
                # so spending hue on it too would say nothing new.
                "color": {"mode": "fixed", "fixedColor": "blue"},
                "decimals": 0,
                "thresholds": {
                    "mode": "absolute",
                    "steps": [{"color": "blue", "value": None}],
                },
            },
            "overrides": [],
        },
        "options": {
            "displayMode": "gradient",
            "orientation": "horizontal",
            "reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
            "showUnfilled": True,
        },
    },
    ts(
        "Files by status",
        gp(8, 8, 8, y),
        targets(
            (
                'sum by (status) (mindex_project_files{project_guid=~"$project"})',
                "{{status}}",
            ),
        ),
        custom=STACK_CUSTOM,
        overrides=by_name_overrides(FILE_STATUS),
        legend=LEGEND_TABLE,
    ),
    ts(
        "Drift reported per check",
        gp(8, 8, 16, y),
        targets(
            (
                'sum by (class) (rate(mindex_drift_files_reported_total{project_guid=~"$project"}[$__rate_interval]))',
                "{{class}}",
            ),
        ),
        unit="cps",
        desc="A counter, not a gauge: /drift compares against a manifest only the "
        "client can produce, so there is no server-side drift level. This is "
        "what the checks that ran reported.",
        custom=STACK_CUSTOM,
        overrides=by_name_overrides(
            {
                "stale": "orange",
                "missing": "red",
                "orphaned": "yellow",
                "indexing": "blue",
            }
        ),
    ),
]
y += 8

dashboard = {
    "uid": "mindex-service",
    "title": "mindex",
    "tags": ["mindex", "service"],
    "timezone": "browser",
    "schemaVersion": 39,
    "version": 1,
    "editable": False,
    "graphTooltip": 1,  # shared crosshair across every panel
    "refresh": "30s",
    "time": {"from": "now-6h", "to": "now"},
    "templating": {
        "list": [
            {
                "name": "ds",
                "label": "Datasource",
                "type": "datasource",
                "query": "prometheus",
                "current": {"text": "VictoriaMetrics", "value": "dfjycbyn0696of"},
                "hide": 0,
                "refresh": 1,
            },
            {
                "name": "project",
                "label": "Project",
                "type": "query",
                "datasource": {"type": "prometheus", "uid": DS},
                "definition": "label_values(mindex_project_files, project_guid)",
                "query": {
                    "query": "label_values(mindex_project_files, project_guid)",
                    "refId": "project",
                },
                "multi": True,
                "includeAll": True,
                "allValue": ".*",
                "current": {"text": ["All"], "value": ["$__all"]},
                "refresh": 2,
                "sort": 1,
            },
            {
                "name": "language",
                "label": "Language",
                "type": "query",
                "datasource": {"type": "prometheus", "uid": DS},
                "definition": "label_values(mindex_project_chunks_active, language)",
                "query": {
                    "query": "label_values(mindex_project_chunks_active, language)",
                    "refId": "language",
                },
                "multi": True,
                "includeAll": True,
                "allValue": ".*",
                "current": {"text": ["All"], "value": ["$__all"]},
                "refresh": 2,
                "sort": 1,
            },
            {
                "name": "route",
                "label": "Route",
                "type": "query",
                "datasource": {"type": "prometheus", "uid": DS},
                "definition": "label_values(mindex_http_requests_total, route)",
                "query": {
                    "query": "label_values(mindex_http_requests_total, route)",
                    "refId": "route",
                },
                "multi": True,
                "includeAll": True,
                "allValue": ".*",
                "current": {"text": ["All"], "value": ["$__all"]},
                "refresh": 2,
                "sort": 1,
            },
            {
                "name": "model",
                "label": "Research model",
                "type": "query",
                "datasource": {"type": "prometheus", "uid": DS},
                "definition": "label_values(mindex_research_runs_total, model)",
                "query": {
                    "query": "label_values(mindex_research_runs_total, model)",
                    "refId": "model",
                },
                "multi": True,
                "includeAll": True,
                "allValue": ".*",
                "current": {"text": ["All"], "value": ["$__all"]},
                "refresh": 2,
                "sort": 1,
            },
        ]
    },
    "panels": P,
}

print(json.dumps(dashboard, indent=2))
