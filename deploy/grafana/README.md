# `deploy/grafana/` — the dashboard and the alerts

Two generators and one generated file. Both are Python with no dependencies; run
them with any Python 3.11+.

| file | what |
|---|---|
| `gen_dashboard.py` | builds the dashboard. **Run it and redirect; never hand-edit the JSON.** |
| `mindex-service.json` | its output, committed — 87 panels, uid `mindex-service` |
| `gen_alerts.py` | builds the alert rules. Its output is **not** committed; see below |

```sh
python3 deploy/grafana/gen_dashboard.py > deploy/grafana/mindex-service.json
```

## Why the dashboard is committed and the alerts are not

The dashboard reaches its data through a `${ds}` template variable, so the file
is the same on every host. An alert rule cannot: Grafana's provisioning format
binds each query to a datasource **uid**, which is generated when you add the
datasource and is different on every installation. A committed
`mindex-alerts.yaml` would be one machine's identifier presented as a shipped
artefact — the same defect as a benchmark config carrying one author's home
directory, and it fails the same way: silently, on somebody else's machine.

So the rules live in the generator, which is the part that is actually portable,
and you produce the file:

```sh
# Connections -> Data sources -> your Prometheus/VictoriaMetrics; the uid is the
# last path segment of that page's URL.
python3 deploy/grafana/gen_alerts.py --datasource-uid <uid> --folder System \
    | sudo tee /etc/grafana/provisioning/alerting/mindex-alerts.yaml > /dev/null
sudo systemctl reload grafana
```

The folder must already exist; Grafana refuses to provision into one it does not
know. Provisioned rules are read-only in the UI, which is the point — the file is
the source of truth.

## What is alerted on, and what deliberately is not

Eight rules. Three are `critical`, four `warning`, one `info`.

The **critical** ones share a shape: they fire on something that has already
stopped working and produces no error anywhere.

- **Stale collections** — a project holds active chunks but its current-version
  Qdrant collection is missing or empty. Its search is broken *now*, while SQLite
  still reports every file `indexed`. This is what a `COLLECTION_SCHEMA_VERSION`
  or `[model].id` change looks like when the reindex was not run, and it is the
  single most valuable rule here.
- **Worker absent** — fewer than seven supervised workers are running. A worker
  that panics is not restarted by design, so GC or the retry sweep has stopped
  permanently; from outside that is indistinguishable from a healthy idle system.
  The rule counts series rather than summing them, because a panicked worker's
  gauge goes *absent* rather than to zero.
- **Dependency down** — sqlite, qdrant or the embedder is failing its probe.
  Ollama is excluded on purpose: it is optional, its failure is `degraded` rather
  than `unhealthy`, and it disables research while leaving search working.

The **warnings** are things documented to stay at zero, plus one staleness check:
NaN scores out of the embedder, SQLite pool exhaustion, a wedged research run,
and a metrics worker that has stopped completing its read — the last because its
gauges keep their previous values on a failure, which is correct and also
indistinguishable from a healthy tick.

The **info** rule is orphaned collections at 24 hours. Nothing is broken; a
pre-bump index is sitting on disk unreachable. It is never cleaned up
automatically because that is what makes a rollback possible, so it needs a
reminder rather than an interrupt — and during the migration that produces both,
it must not compete with the critical rule for attention.

**`noDataState` is `OK` almost everywhere.** The server being unreachable is one
incident, and a rule set where every rule fires at that moment turns it into
eight notifications.

Deliberately **not** alerted on: request error rate (a 404 from `/search` is the
documented way to say "no match"), research `done_reason` distribution (a
budget-exhausted run is a normal outcome), and anything per-project (the label is
open-ended, and an alert that multiplies by project count is one that gets
silenced).

## When `[auth].enabled` is on

`/metrics` is `admin`-scoped, so the scrape needs its own credential before any
of this has data. `deploy/victoriametrics/mindex.scrape.yml` carries the mint
recipe and the two ways it fails in the direction that looks like it worked.
