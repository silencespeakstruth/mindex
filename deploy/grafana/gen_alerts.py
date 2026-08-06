"""Generate deploy/grafana/mindex-alerts.yaml — the alerts worth waking someone for.

WHY THIS EXISTS. The dashboard shipped first and the alerts did not, so for three
releases every signal here was a thing somebody had to *look* at. The two that
matter most are exactly the ones nobody looks at on a normal day: a stale
collection means search is silently returning nothing for a project, and an
absent worker means GC or the retry sweep stopped, which from outside is
indistinguishable from a healthy idle system.

WHY GENERATED. Grafana's unified-alerting provisioning format is ~60 lines of
nested query/reduce/threshold stages per rule. Hand-writing six of those is how
a rule set ends up with two spellings of the same threshold and one rule quietly
evaluating `last` where its neighbour evaluates `max`. The three-stage pipeline
is built once here and every rule is a few arguments.

THE `for:` DURATION IS THE DESIGN, not the threshold. Every rule below fires on a
condition that is *already* true rather than on a spike, so the question each one
answers is "how long may this be true before it is worth an interrupt". A
too-short `for` on a metrics pipeline that refreshes on a 60 s tick alerts on the
gap between ticks.

Usage:

    python3 deploy/grafana/gen_alerts.py --datasource-uid <uid> > mindex-alerts.yaml

Find the uid at Connections -> Data sources -> your Prometheus/VictoriaMetrics ->
the last path segment of the URL. Then drop the file into
`/etc/grafana/provisioning/alerting/` and restart Grafana.
"""

import argparse
import json
import sys

# The scrape job. Every rule is scoped to it, because a rule matching a metric
# name alone would also match a second mindex if one is ever scraped.
JOB = 'job="mindex"'


def rule(
    uid,
    title,
    expr,
    *,
    op,
    threshold,
    for_,
    severity,
    summary,
    reducer="last",
    no_data="OK",
):
    """One alert as Grafana's three-stage query -> reduce -> threshold pipeline.

    `no_data` defaults to OK and that is deliberate for most of these: the server
    being unreachable is one alert (Service down), and having every other rule
    fire at the same moment turns one incident into eight notifications. The
    exceptions pass NoData explicitly and say why at the call site.
    """
    return {
        "uid": uid,
        "title": title,
        "condition": "C",
        "for": for_,
        "noDataState": no_data,
        "execErrState": "Error",
        "isPaused": False,
        "annotations": {"summary": summary},
        "labels": {"severity": severity, "service": "mindex"},
        "data": [
            {
                "refId": "A",
                "relativeTimeRange": {"from": 600, "to": 0},
                "datasourceUid": "${DS}",
                "model": {
                    "refId": "A",
                    "datasource": {"type": "prometheus", "uid": "${DS}"},
                    "expr": expr,
                    "instant": False,
                    "range": True,
                    "editorMode": "code",
                    "intervalMs": 30000,
                    "maxDataPoints": 300,
                },
            },
            {
                "refId": "B",
                "relativeTimeRange": {"from": 600, "to": 0},
                "datasourceUid": "__expr__",
                "model": {
                    "refId": "B",
                    "type": "reduce",
                    "datasource": {"type": "__expr__", "uid": "__expr__"},
                    "expression": "A",
                    "reducer": reducer,
                    "settings": {"mode": "dropNN"},
                },
            },
            {
                "refId": "C",
                "relativeTimeRange": {"from": 600, "to": 0},
                "datasourceUid": "__expr__",
                "model": {
                    "refId": "C",
                    "type": "threshold",
                    "datasource": {"type": "__expr__", "uid": "__expr__"},
                    "expression": "B",
                    "conditions": [
                        {
                            "type": "query",
                            "evaluator": {"type": op, "params": [threshold]},
                            "operator": {"type": "and"},
                            "query": {"params": ["C"]},
                            "reducer": {"type": reducer, "params": []},
                        }
                    ],
                },
            },
        ],
    }


RULES = [
    # ── the two the v2 -> v3 migration existed to be caught by ───────────────
    rule(
        "mindex-stale-collections",
        "mindex: a project's search is broken",
        f"max(mindex_stale_collections{{{JOB}}})",
        op="gt",
        threshold=0,
        for_="10m",
        severity="critical",
        summary=(
            "A project holds active chunks but its current-version Qdrant collection "
            "is missing or empty, so /search answers nothing (or 503) for it right "
            "now while SQLite still reports every file indexed. Almost always a "
            "COLLECTION_SCHEMA_VERSION or [model].id change without the reindex: run "
            "`mindex-index --force` per project, or `--vectors-only` if only the "
            "model moved. The gauge is seeded at -1 and never at 0, so `> 0` cannot "
            "be produced by a pass that failed to run."
        ),
    ),
    rule(
        "mindex-orphaned-collections",
        "mindex: collections left at a superseded schema version",
        f"max(mindex_orphaned_collections{{{JOB}}})",
        op="gt",
        threshold=0,
        # Days, not minutes. Nothing is broken; a whole pre-bump index is sitting
        # on disk unreachable, and NOT deleting it automatically is what makes a
        # rollback possible. This is a reminder, and it must not compete with the
        # rule above for attention during the migration that produces both.
        for_="24h",
        severity="info",
        summary=(
            "One or more Qdrant collections are at a previous COLLECTION_SCHEMA_VERSION: "
            "unreachable by anything, still holding a full index. Deliberately never "
            "dropped automatically — that is what makes a rollback possible — so this "
            "fires only after a day, and clearing it is a manual `DELETE /collections/...` "
            "once the new index is trusted. A collection belonging to another *registered* "
            "model is held rather than orphaned and is not counted here."
        ),
    ),
    # ── things that stop working without failing ─────────────────────────────
    rule(
        "mindex-worker-absent",
        "mindex: a background worker is not running",
        # count(), not sum(): a panicked worker's series goes ABSENT rather than
        # to 0, because supervise() publishes the gauge before starting the task
        # precisely so that a worker which never started is missing. sum() over a
        # series that does not exist is not 0, it is no data.
        f"count(mindex_worker_running{{{JOB}}} == 1)",
        op="lt",
        threshold=7,
        for_="10m",
        severity="critical",
        summary=(
            "Fewer than the seven supervised workers are running (gc, retry, metrics, "
            "collection_check, ollama_catalog, research_stats, research_watchdog). A "
            "worker that panics is not restarted by design — it would panic again and "
            "bury the backtrace — so GC or the retry sweep has stopped permanently and "
            "silently. Check `worker_exits_total{outcome=\"panic\"}` and the journal for "
            "the worker name, then restart the process."
        ),
    ),
    rule(
        "mindex-dependency-down",
        "mindex: a required dependency is failing",
        # Deliberately not `sum(mindex_dependency_up)` against a constant: the
        # expected count depends on whether the query path is split, and a rule
        # that has to be edited when a deployment changes shape is one that gets
        # left wrong. Ollama is excluded because it is optional — its failure is
        # `degraded`, which disables research and keeps search working.
        f'min(mindex_dependency_up{{{JOB}, dependency!="ollama"}})',
        op="lt",
        threshold=1,
        for_="5m",
        severity="critical",
        summary=(
            "sqlite, qdrant or the embedder is failing its health probe. Search and "
            "indexing are down or degrading. Ollama is excluded from this rule on "
            "purpose: it is optional, its failure means `degraded` rather than "
            "`unhealthy`, and it disables research while leaving search working."
        ),
    ),
    rule(
        "mindex-state-stale",
        "mindex: every per-project number is frozen",
        f"time() - max(mindex_state_refreshed_timestamp_seconds{{{JOB}}})",
        op="gt",
        threshold=600,
        for_="15m",
        severity="warning",
        summary=(
            "The metrics worker has not completed a full read for ten minutes. Its "
            "gauges keep their previous values on a failed tick rather than zeroing "
            "(which is right — a zero would look like an empty index), so without this "
            "rule a frozen dashboard is indistinguishable from a quiet one. Usually a "
            "database that has stopped answering, or a Qdrant that cannot be probed "
            "under [metrics].probe_dependencies."
        ),
    ),
    # ── things that are supposed to stay at zero ─────────────────────────────
    rule(
        "mindex-unscorable-winners",
        "mindex: the embedder is returning NaN",
        f"sum(increase(mindex_search_unscorable_winners_total{{{JOB}}}[15m])) or vector(0)",
        op="gt",
        threshold=0,
        for_="5m",
        severity="warning",
        reducer="max",
        summary=(
            "Search scored a chunk NaN. Documented producers: an fp16 embedder "
            "returning NaN for padded rows (Qwen3 does this — serve it bf16), and a "
            "split deployment whose two instances differ in precision. NaN results are "
            "ranked last rather than dropped, so the symptom without this counter is "
            "'search sometimes puts something irrelevant first', which reads as a "
            "ranking-quality complaint and is a misconfigured embedder."
        ),
    ),
    rule(
        "mindex-pool-exhausted",
        "mindex: the SQLite pool is refusing work",
        f"sum(increase(mindex_db_pool_acquire_failures_total{{{JOB}}}[15m])) or vector(0)",
        op="gt",
        threshold=0,
        for_="10m",
        severity="warning",
        reducer="max",
        summary=(
            "Requests are being answered 503 `database.busy` because no connection was "
            "free. Either genuine load — raise [database].pool_size, keeping it above "
            "your client concurrency — or connections have leaked to panicking "
            "transactions, in which case `db_transactions{outcome=\"panic\"}` is "
            "non-zero and the pool never recovers without a restart."
        ),
    ),
    rule(
        "mindex-research-wedged",
        "mindex: a research run is wedged",
        f"max(mindex_research_inflight_oldest_age_seconds{{{JOB}}})",
        # Above the longest legitimate run: `high` grants 3600 s plus a 120 s
        # report window, and the watchdog's own grace sits on top of that. A
        # threshold below the budget alerts on a run that is working.
        op="gt",
        threshold=4200,
        for_="10m",
        severity="warning",
        summary=(
            "A run has held its semaphore permit past `max_seconds + report_timeout_ms "
            "+ the watchdog grace`. With [research].max_concurrent = 1 that is a total "
            "research outage, and /health reports `unhealthy` for it. The watchdog "
            "cancels what it can reach; a run stuck in an await its token cannot reach "
            "stays here until the process restarts. `DELETE /research/active/{run_id}` "
            "is the first thing to try."
        ),
    ),
]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--datasource-uid",
        required=True,
        help="the Prometheus/VictoriaMetrics datasource uid Grafana knows it by",
    )
    ap.add_argument(
        "--folder",
        default="System",
        help="the Grafana folder to provision into (must already exist)",
    )
    args = ap.parse_args()

    doc = {
        "apiVersion": 1,
        "groups": [
            {
                "orgId": 1,
                "name": "mindex",
                "folder": args.folder,
                "interval": "1m",
                "rules": RULES,
            }
        ],
    }
    # JSON is valid YAML, and emitting it avoids a PyYAML dependency for a file
    # that is generated rather than read. Grafana parses either.
    text = json.dumps(doc, indent=2).replace("${DS}", args.datasource_uid)
    sys.stdout.write(text + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
