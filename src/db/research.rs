//! Persistence for the `/research` run journal (`research_runs`).
//!
//! One insert per finished run, best-effort: the report has already been streamed
//! to the client by the time this is called, so a write failure costs a row and
//! nothing else. See the migration for why this table is deliberately not
//! foreign-keyed to `project_files`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::backend::metrics::{
    ClassLabels, Metrics, ModelEffortLabels, ModelKindLabels, ModelLabels, ModelReasonLabels,
};
use crate::db::sqlite3::SQLite3Pool;
use crate::research::{ResearchJournal, RunRecord};

/// Everything about a run that the *request* decided rather than the loop.
///
/// Kept apart from [`RunRecord`] so the loop never has to know about efforts,
/// projects or HTTP: whoever builds the journal already holds these.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub project_guid: String,
    pub effort: &'static str,
    pub seed: Option<i64>,
    pub temperature: Option<f64>,
    /// The run's file scope, rendered once by the caller. `None` for an unscoped run —
    /// stored as SQL NULL, so "no scope" and "an empty scope" stay apart.
    pub scope_json: Option<String>,
}

/// Insert one finished run. Logs and swallows every failure — the caller is on
/// the "report already delivered" side of the run.
pub async fn insert_run(
    db_pool: &SQLite3Pool,
    ctx: RunContext,
    record: RunRecord,
    token: CancellationToken,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let cited_paths =
        serde_json::to_string(&record.citations.cited_paths).unwrap_or_else(|_| "[]".to_string());
    let unverified_paths = serde_json::to_string(&record.citations.unverified_paths)
        .unwrap_or_else(|_| "[]".to_string());
    let stale_paths =
        serde_json::to_string(&record.citations.stale_paths).unwrap_or_else(|_| "[]".to_string());

    let res = db_pool
        .transaction(token, move |tx| {
            tx.execute(
                "INSERT INTO research_runs (
                     id, project_guid,
                     question, model, prompt_version, effort, seed, temperature,
                     granted_seconds, granted_tokens, granted_steps, granted_search_top_k,
                     done_reason, steps, turns, elapsed_ms,
                     prompt_tokens, eval_tokens, peak_prompt_tokens, num_ctx,
                     citations_total, citations_verified, citations_path_only,
                     citations_unverified, cited_paths_json, unverified_paths_json,
                     changed_files, removed_files, stale_citations, stale_paths_json,
                     notes_written, notes_rejected, plan_revisions,
                     grep_calls, grep_hits,
                     out_of_scope_refusals, out_of_scope_rows,
                     scoped, scope_json,
                     forced_synthesis, report_window_ms, report_elapsed_ms,
                     report
                 ) VALUES (
                     ?1, ?2,
                     ?3, ?4, ?5, ?6, ?7, ?8,
                     ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20,
                     ?21, ?22, ?23,
                     ?24, ?25, ?26,
                     ?27, ?28, ?29, ?30,
                     ?31, ?32, ?33,
                     ?34, ?35,
                     ?36, ?37,
                     ?38, ?39,
                     ?40, ?41, ?42,
                     ?43
                 )",
                rusqlite::params![
                    id,
                    ctx.project_guid,
                    record.question,
                    record.model,
                    record.prompt_version,
                    ctx.effort,
                    ctx.seed,
                    ctx.temperature,
                    record.budget.max_seconds as i64,
                    record.budget.max_tokens as i64,
                    record.budget.max_steps as i64,
                    record.budget.search_top_k as i64,
                    record.reason.as_str(),
                    record.steps as i64,
                    record.turns as i64,
                    record.elapsed_ms as i64,
                    record.prompt_tokens as i64,
                    record.eval_tokens as i64,
                    record.peak_prompt_tokens as i64,
                    record.num_ctx as i64,
                    record.citations.total as i64,
                    record.citations.verified as i64,
                    record.citations.path_only as i64,
                    record.citations.unverified as i64,
                    cited_paths,
                    unverified_paths,
                    record.staleness.changed_files as i64,
                    record.staleness.removed_files as i64,
                    record.citations.stale as i64,
                    stale_paths,
                    record.tools.notes_written as i64,
                    record.tools.notes_rejected as i64,
                    record.tools.plan_revisions as i64,
                    record.tools.grep_calls as i64,
                    record.tools.grep_hits as i64,
                    record.tools.out_of_scope_refusals as i64,
                    record.tools.out_of_scope_rows as i64,
                    i64::from(ctx.scope_json.is_some()),
                    ctx.scope_json,
                    i64::from(record.tools.forced_synthesis),
                    record.tools.report_window_ms as i64,
                    record.tools.report_elapsed_ms as i64,
                    record.report,
                ],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = res {
        warn!(
            error = ?e,
            "Could not journal a finished research run; the report was delivered but \
             leaves no trace. Check the database is writable."
        );
    }
}

/// Metrics decorator over [`ResearchJournal`](crate::research::ResearchJournal).
///
/// Almost the entire research metric set falls out of this one wrapper, because
/// `RunRecord` already carries everything worth measuring — model, stop reason,
/// steps, turns, elapsed, prompt/eval/peak tokens, `num_ctx` and the full
/// citation report. So `run_research` needs no instrumentation of its own: the
/// journal is called exactly once per finished run, on the aggregation path that
/// already exists.
///
/// Wrapping rather than teaching `SqliteResearchJournal` to count keeps the
/// best-effort contract intact — a metric is recorded whether or not the row
/// lands, which is right: the run happened either way.
pub struct MeteredJournal {
    inner: Arc<dyn ResearchJournal>,
    metrics: Arc<Metrics>,
    /// Closed over from the request, like `RunContext`'s copy — the loop does not
    /// know about effort levels.
    effort: &'static str,
}

impl MeteredJournal {
    pub fn new(
        inner: Arc<dyn ResearchJournal>,
        metrics: Arc<Metrics>,
        effort: &'static str,
    ) -> Self {
        Self {
            inner,
            metrics,
            effort,
        }
    }
}

#[async_trait]
impl ResearchJournal for MeteredJournal {
    async fn record(&self, record: RunRecord) {
        let r = &self.metrics.research;
        let model = record.model.clone();
        let labels = ModelLabels {
            model: model.clone(),
        };

        // Split from `runs_by_effort` rather than crossed with it: `model` is a
        // client-supplied string, so model x effort x reason is the one product
        // in the whole set that could grow without a server-defined bound.
        r.runs
            .get_or_create(&ModelReasonLabels {
                model: model.clone(),
                done_reason: record.reason.as_str(),
            })
            .inc();
        r.runs_by_effort
            .get_or_create(&ModelEffortLabels {
                model: model.clone(),
                effort: self.effort,
            })
            .inc();

        r.duration
            .get_or_create(&labels)
            .observe(record.elapsed_ms as f64 / 1000.0);
        r.steps.get_or_create(&labels).observe(record.steps as f64);
        r.turns.get_or_create(&labels).observe(record.turns as f64);
        r.tokens
            .get_or_create(&ModelKindLabels {
                model: model.clone(),
                kind: "prompt",
            })
            .inc_by(record.prompt_tokens);
        r.tokens
            .get_or_create(&ModelKindLabels {
                model,
                kind: "eval",
            })
            .inc_by(record.eval_tokens);
        // A turn Ollama reported no counts for lands in `turns_unreported` rather
        // than as a zero, so `num_ctx` can legitimately be 0 — skip instead of
        // dividing by it and reporting an infinite ratio.
        if record.num_ctx > 0 {
            r.context_used
                .get_or_create(&labels)
                .observe(record.peak_prompt_tokens as f64 / record.num_ctx as f64);
        }

        let c = &record.citations;
        for (class, n) in [
            ("verified", c.verified),
            ("path_only", c.path_only),
            ("unverified", c.unverified),
            ("stale", c.stale),
        ] {
            if n > 0 {
                r.citations
                    .get_or_create(&ClassLabels { class })
                    .inc_by(n as u64);
            }
        }
        if record.revalidation.is_some() {
            r.revalidations.inc();
        }
        if record.tools.forced_synthesis {
            r.forced_syntheses.inc();
        }

        self.inner.record(record).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::{Budget, CitationReport, DoneReason, RunStaleness};

    fn record() -> RunRecord {
        RunRecord {
            tools: crate::research::RunTools::default(),
            question: "how does GC work?".into(),
            model: "test-model".into(),
            prompt_version: "test.1",
            budget: Budget {
                max_seconds: 240,
                max_tokens: 400_000,
                context_fraction: 0.7,
                max_steps: 20,
                search_top_k: 5,
            },
            reason: DoneReason::Finalized,
            steps: 3,
            turns: 4,
            elapsed_ms: 1234,
            prompt_tokens: 500,
            eval_tokens: 60,
            peak_prompt_tokens: 300,
            num_ctx: 8192,
            citations: CitationReport {
                total: 2,
                verified: 1,
                path_only: 0,
                unverified: 1,
                stale: 1,
                unverified_paths: vec!["src/nope.rs".into()],
                stale_paths: vec!["src/gc.rs".into()],
                cited_paths: vec!["src/gc.rs".into(), "src/nope.rs".into()],
            },
            staleness: RunStaleness {
                changed_files: 1,
                removed_files: 0,
            },
            revalidation: None,
            report: "# Report\n\nIt sweeps.".into(),
        }
    }

    /// `MeteredJournal` is the whole research metric set, so this is the guard on
    /// it staying wired to the numbers `RunRecord` already carries.
    #[tokio::test]
    async fn the_metered_journal_records_a_run_and_still_writes_the_row() {
        struct Inner(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl ResearchJournal for Inner {
            async fn record(&self, _: RunRecord) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let metrics = Arc::new(Metrics::new());
        let inner = Arc::new(Inner(std::sync::atomic::AtomicUsize::new(0)));
        let journal = MeteredJournal::new(
            Arc::clone(&inner) as Arc<dyn ResearchJournal>,
            Arc::clone(&metrics),
            "medium",
        );

        journal.record(record()).await;

        // The decorator must pass the record through, not swallow it.
        assert_eq!(inner.0.load(std::sync::atomic::Ordering::SeqCst), 1);

        let text = metrics.render().expect("renders");
        assert!(
            text.contains(
                r#"mindex_research_runs_total{model="test-model",done_reason="finalized"} 1"#
            ),
            "{text}"
        );
        assert!(
            text.contains(
                r#"mindex_research_runs_by_effort_total{model="test-model",effort="medium"} 1"#
            ),
            "{text}"
        );
        assert!(
            text.contains(r#"mindex_research_tokens_total{model="test-model",kind="prompt"} 500"#),
            "{text}"
        );
        // Every citation class is its own series; `stale` is orthogonal to the
        // three provenance verdicts and must not be folded into them.
        for class in ["verified", "unverified", "stale"] {
            assert!(
                text.contains(&format!(
                    r#"mindex_research_citations_total{{class="{class}"}} 1"#
                )),
                "missing {class}: {text}"
            );
        }
        assert!(
            !text.contains(r#"mindex_research_revalidations_total 1"#),
            "a clean draft was counted as a repair: {text}"
        );
    }

    fn ctx() -> RunContext {
        RunContext {
            scope_json: None,
            project_guid: "c2d7e2c1-3165-42f5-9366-0ff1492b4bab".into(),
            effort: "medium",
            seed: Some(7),
            temperature: Some(0.2),
        }
    }

    // One connection: each ":memory:" connection is its own database, so a
    // larger pool would read a different (empty) one back.
    async fn pool() -> SQLite3Pool {
        let pool = SQLite3Pool::new(std::path::Path::new(":memory:"), 1, 16384, "NORMAL");
        pool.transaction(CancellationToken::new(), |tx| {
            tx.execute_batch(include_str!("migrations/v1.0.0_schema.sql"))?;
            Ok(())
        })
        .await
        .expect("migration applies");
        pool
    }

    #[tokio::test]
    async fn a_finished_run_is_journalled_with_its_cost_and_citation_verdict() {
        let pool = pool().await;
        insert_run(&pool, ctx(), record(), CancellationToken::new()).await;

        let row: (String, String, String, i64, i64, i64, String) = pool
            .transaction(CancellationToken::new(), |tx| {
                Ok(tx.query_row(
                    "SELECT model, prompt_version, done_reason, steps, \
                     citations_verified, citations_unverified, unverified_paths_json \
                     FROM research_runs",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )?)
            })
            .await
            .expect("one row");
        assert_eq!(row.0, "test-model");
        assert_eq!(row.1, "test.1");
        assert_eq!(row.2, "finalized");
        assert_eq!((row.3, row.4, row.5), (3, 1, 1));
        assert_eq!(row.6, r#"["src/nope.rs"]"#);
    }

    /// Staleness rides on the run's own row, so a journalled run always carries the
    /// verdict on whether the index held still under it — including the zero case,
    /// which is a measurement and not an absence.
    #[tokio::test]
    async fn a_run_records_how_far_the_index_moved_underneath_it() {
        let pool = pool().await;
        insert_run(&pool, ctx(), record(), CancellationToken::new()).await;

        let row: (i64, i64, i64, String) = pool
            .transaction(CancellationToken::new(), |tx| {
                Ok(tx.query_row(
                    "SELECT changed_files, removed_files, stale_citations, \
                     stale_paths_json FROM research_runs",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?)
            })
            .await
            .expect("one row");
        assert_eq!((row.0, row.1, row.2), (1, 0, 1));
        assert_eq!(row.3, r#"["src/gc.rs"]"#);
    }

    /// Unset sampling must round-trip as NULL, not as 0: "the model's own default"
    /// and "temperature zero" are different runs, and a measurement corpus that
    /// conflates them is worse than one that says nothing.
    #[tokio::test]
    async fn unset_sampling_is_stored_as_null_not_as_zero() {
        let pool = pool().await;
        let mut c = ctx();
        c.seed = None;
        c.temperature = None;
        insert_run(&pool, c, record(), CancellationToken::new()).await;

        let got: (Option<i64>, Option<f64>) = pool
            .transaction(CancellationToken::new(), |tx| {
                Ok(
                    tx.query_row("SELECT seed, temperature FROM research_runs", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?,
                )
            })
            .await
            .expect("one row");
        assert_eq!(got, (None, None));
    }

    /// The write is on the "report already delivered" side of the run, so a
    /// missing table (an un-migrated DB) must log and return, never propagate.
    #[tokio::test]
    async fn a_failing_write_does_not_propagate() {
        let pool = SQLite3Pool::new(std::path::Path::new(":memory:"), 1, 16384, "NORMAL");
        // No migration applied: the table does not exist.
        insert_run(&pool, ctx(), record(), CancellationToken::new()).await;
    }
}
