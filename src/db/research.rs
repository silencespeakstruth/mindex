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
use crate::research::{RecordedRun, ResearchJournal, RunRecord};

/// Shared with `worker::gc`, which prunes on the same unit.
const SECONDS_PER_DAY: i64 = 86_400;

/// Everything about a run that the *request* decided rather than the loop.
///
/// Kept apart from [`RunRecord`] so the loop never has to know about efforts,
/// projects or HTTP: whoever builds the journal already holds these.
#[derive(Debug, Clone)]
pub struct RunContext {
    /// The run's identity, minted at **admission** rather than here.
    ///
    /// It used to be created in this function, which meant a run had no name until
    /// the moment it ended: nothing could list it while it ran, and nothing could
    /// cancel it by name. The id now comes from `post_research`, which registers it
    /// in `backend::inflight::ResearchRegistry` and streams it as the `started`
    /// event — so the id on the wire at second zero is the id in this row.
    pub id: String,
    pub project_guid: String,
    pub effort: &'static str,
    pub seed: Option<i64>,
    pub temperature: Option<f64>,
    /// The third sampling axis; NULL = the model's own default, like the two above.
    pub top_p: Option<f64>,
    /// The Ollama blob digest of the resolved model, from the model catalog at
    /// admission; `None` when the catalog had not seen it yet (e.g. within the
    /// first refresh interval after startup). The name in `model` is mutable —
    /// a re-pulled tag is a different artifact — so this is what makes two runs
    /// actually comparable.
    pub model_digest: Option<String>,
    /// The catalog's details object for the model (parameter size, quantization,
    /// family), stored whole as JSON; read by humans and notebooks, never joined.
    pub model_details_json: Option<String>,
    /// Which embedding model the run's file baselines were read under
    /// (`RouterState.model_id`). The staleness join has always bound this from
    /// state; stamping it keeps stored runs interpretable across an embedder swap.
    pub embedder_model_id: String,
    /// `CARGO_PKG_VERSION` of the server that produced the row.
    pub server_version: &'static str,
    /// Wall-clock admission time (unix seconds). `created_at` is the INSERT's
    /// time — the run's end — so without this the corpus never recorded when a
    /// run began.
    pub started_at: i64,
    /// The resolved checkpoint interval for this run (`0` = off) — request
    /// override over `[research].checkpoint_every_steps`, resolved by the
    /// handler like the rest of this struct.
    pub checkpoint_every_steps: usize,
    /// The run's file scope, rendered once by the caller. `None` for an unscoped run —
    /// stored as SQL NULL, so "no scope" and "an empty scope" stay apart.
    pub scope_json: Option<String>,
    /// The same scope as data (serialized `ToolScope`), for the challenge
    /// endpoint to re-inhabit. `None` on unscoped runs.
    pub scope_spec_json: Option<String>,
    /// `"research"` or `"challenge"` — the row's `kind` column.
    pub kind: &'static str,
    /// The run this challenge attacked; `None` on ordinary research runs. No FK
    /// (see the migration): a dangling id means the subject is gone, nothing more.
    pub challenged_run_id: Option<String>,
    /// `[research].retention_days`, threaded rather than read from a global. Stamped
    /// onto the row as an absolute `expires_at` at insert, so a run's deadline is a
    /// property of the run and a later config change moves only future runs.
    pub retention_days: u64,
}

/// Insert one finished run. Logs and swallows every failure — the caller is on
/// the "report already delivered" side of the run.
///
/// Returns [`RecordedRun`] so the `done` event can name what was stored, or `None`
/// if nothing was: the best-effort contract is unchanged, and a `None` simply means
/// the client is not offered a run it cannot later fetch.
///
/// **Two tables, one transaction.** The v1.0.0 comment on this table says "one row,
/// one INSERT", which stopped being literally true when `research_run_files` arrived;
/// the property it was really claiming — a run has all its rows or none — is what the
/// single transaction still guarantees. A half-written run would be worse than no run
/// at all here, since a report stored without its baselines reads as permanently
/// fresh.
pub async fn insert_run(
    db_pool: &SQLite3Pool,
    ctx: RunContext,
    record: RunRecord,
    token: CancellationToken,
) -> Option<RecordedRun> {
    let id = ctx.id.clone();
    let retention_secs = (ctx.retention_days as i64).saturating_mul(SECONDS_PER_DAY);
    let context_run_ids =
        serde_json::to_string(&record.context_run_ids).unwrap_or_else(|_| "[]".to_string());
    let cited_paths =
        serde_json::to_string(&record.citations.cited_paths).unwrap_or_else(|_| "[]".to_string());
    let unverified_paths = serde_json::to_string(&record.citations.unverified_paths)
        .unwrap_or_else(|_| "[]".to_string());
    let stale_paths =
        serde_json::to_string(&record.citations.stale_paths).unwrap_or_else(|_| "[]".to_string());

    let res = db_pool
        .transaction(token, move |tx| {
            // The per-project ordinal, read inside the same transaction as the insert
            // that consumes it. GC reaps the oldest rows, so MAX survives a sweep and
            // the sequence keeps climbing; the UNIQUE index on (project_guid, seq) is
            // the backstop if two runs ever race here.
            let seq: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM research_runs WHERE project_guid = ?1",
                rusqlite::params![ctx.project_guid],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO research_runs (
                     id, project_guid, seq, expires_at, context_run_ids_json,
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
                     title, report,
                     top_p, model_digest, model_details_json,
                     granted_context_fraction, granted_report_words,
                     granted_report_sections, granted_evidence_width,
                     checkpoint_every_steps, checkpoints_taken,
                     revalidation_draft_unverified, revalidation_draft_path_only,
                     revalidation_draft_stale, revalidation_steps,
                     sufficiency_verdict,
                     embedder_model_id, server_version, started_at,
                     scope_spec_json, kind, challenged_run_id,
                     challenge_verdict, claims_total, claims_confirmed,
                     claims_disputed, claims_refuted
                 ) VALUES (
                     ?1, ?2, ?44, unixepoch() + ?45, ?46,
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
                     ?47, ?43,
                     ?48, ?49, ?50,
                     ?51, ?52,
                     ?53, ?54,
                     ?55, ?56,
                     ?57, ?58,
                     ?59, ?60,
                     ?61,
                     ?62, ?63, ?64,
                     ?65, ?66, ?67,
                     ?68, ?69, ?70,
                     ?71, ?72
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
                    seq,
                    retention_secs,
                    context_run_ids,
                    record.title,
                    ctx.top_p,
                    ctx.model_digest,
                    ctx.model_details_json,
                    record.budget.context_fraction,
                    record.budget.max_report_words as i64,
                    record.budget.max_report_sections as i64,
                    record.budget.evidence_width as i64,
                    ctx.checkpoint_every_steps as i64,
                    record.tools.checkpoints_taken as i64,
                    record.revalidation.map(|r| r.draft_unverified as i64),
                    record.revalidation.map(|r| r.draft_path_only as i64),
                    record.revalidation.map(|r| r.draft_stale as i64),
                    record.revalidation.map(|r| r.steps as i64),
                    record.sufficiency_verdict,
                    ctx.embedder_model_id,
                    ctx.server_version,
                    ctx.started_at,
                    ctx.scope_spec_json,
                    ctx.kind,
                    ctx.challenged_run_id,
                    record.challenge.as_ref().and_then(|c| c.verdict),
                    record.challenge.as_ref().map(|c| c.claims.len() as i64),
                    record.challenge.as_ref().map(|c| c.count_of("confirmed")),
                    record.challenge.as_ref().map(|c| c.count_of("disputed")),
                    record.challenge.as_ref().map(|c| c.count_of("refuted")),
                ],
            )?;

            // The baselines, in the same transaction. A prepared statement reused
            // across the loop: a run that read fifty files would otherwise re-parse
            // the same INSERT fifty times.
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO research_run_files (run_id, path, sha256) VALUES (?1, ?2, ?3)",
                )?;
                for b in &record.file_baselines {
                    stmt.execute(rusqlite::params![id, b.path, b.sha256])?;
                }
            }
            // The shown spans — what makes the citation check re-runnable against
            // this row later without a model or a GPU.
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO research_run_evidence (run_id, path, spans_json) \
                     VALUES (?1, ?2, ?3)",
                )?;
                for e in &record.evidence_spans {
                    let spans =
                        serde_json::to_string(&e.spans).unwrap_or_else(|_| "[]".to_string());
                    stmt.execute(rusqlite::params![id, e.path, spans])?;
                }
            }
            // Every citation occurrence with its own verdict, in report order.
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO research_run_citations \
                     (run_id, ord, path, start_line, end_line, verdict, stale) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for (ord, c) in record.citations.details.iter().enumerate() {
                    stmt.execute(rusqlite::params![
                        id,
                        ord as i64,
                        c.path,
                        c.start as i64,
                        c.end as i64,
                        c.verdict,
                        i64::from(c.stale),
                    ])?;
                }
            }
            // The tool-call trace: calls + arguments + landing spans, no bodies.
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO research_run_steps \
                     (run_id, n, phase, action, argument, hits, spans_json, \
                      spans_truncated, at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for s in &record.trace {
                    let spans =
                        serde_json::to_string(&s.spans).unwrap_or_else(|_| "[]".to_string());
                    stmt.execute(rusqlite::params![
                        id,
                        s.n as i64,
                        s.phase,
                        s.action,
                        s.argument,
                        s.hits as i64,
                        spans,
                        i64::from(s.spans_truncated),
                        s.at_ms as i64,
                    ])?;
                }
            }
            Ok(RecordedRun { id, seq })
        })
        .await;
    match res {
        Ok(recorded) => Some(recorded),
        Err(e) => {
            warn!(
                error = ?e,
                "Could not journal a finished research run; the report was delivered but \
                 leaves no trace, and the client cannot be offered it as context for a \
                 later run. Check the database is writable."
            );
            None
        }
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
    async fn record(&self, record: RunRecord) -> Option<RecordedRun> {
        let r = &self.metrics.research;
        let model = record.model.clone();
        let labels = ModelLabels {
            model: model.clone(),
        };

        // Was this run given earlier reports to read, and how many? Counted here
        // rather than at the injection site for the decorator's usual reason: a seam
        // cannot miss a caller, and `RunRecord` already carries the list.
        if !record.context_run_ids.is_empty() {
            r.runs_with_context.get_or_create(&labels).inc();
            r.context_runs_used
                .inc_by(record.context_run_ids.len() as u64);
        }

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
        // A challenge's verdict is a server-defined closed set; `None` — a
        // verdict turn that parsed to nothing — is its own label rather than a
        // dropped event, because "inconclusive" is the value that must not be
        // mistaken for an acquittal.
        if let Some(challenge) = &record.challenge {
            r.challenges
                .get_or_create(&crate::backend::metrics::OutcomeLabels {
                    outcome: challenge.verdict.unwrap_or("inconclusive"),
                })
                .inc();
        }
        if record.tools.forced_synthesis {
            r.forced_syntheses.inc();
        }

        self.inner.record(record).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::{Budget, CitationReport, DoneReason, RunStaleness};

    fn record() -> RunRecord {
        RunRecord {
            tools: crate::research::RunTools::default(),
            file_baselines: Vec::new(),
            context_run_ids: Vec::new(),
            question: "how does GC work?".into(),
            model: "test-model".into(),
            prompt_version: "test.1",
            budget: Budget {
                max_seconds: 240,
                max_tokens: 400_000,
                context_fraction: 0.7,
                max_steps: 20,
                search_top_k: 5,
                max_report_words: 900,
                max_report_sections: 6,
                evidence_width: 1,
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
                // Not journalled — the excerpt channel's input, not the record's.
                verified_locations: Vec::new(),
                details: vec![
                    crate::research::CitationDetail {
                        path: "src/gc.rs".into(),
                        start: 10,
                        end: 20,
                        verdict: "verified",
                        stale: true,
                    },
                    crate::research::CitationDetail {
                        path: "src/nope.rs".into(),
                        start: 1,
                        end: 2,
                        verdict: "unverified",
                        stale: false,
                    },
                ],
            },
            staleness: RunStaleness {
                changed_files: 1,
                removed_files: 0,
            },
            revalidation: None,
            title: Some("Report".into()),
            report: "# Report\n\nIt sweeps.".into(),
            evidence_spans: vec![crate::research::EvidenceSpans {
                path: "src/gc.rs".into(),
                spans: vec![(10, 30)],
            }],
            trace: vec![crate::research::StepTrace {
                n: 1,
                phase: "main",
                action: "grep",
                argument: "sweep".into(),
                hits: 2,
                spans: vec!["src/gc.rs:10-30".into()],
                spans_truncated: false,
                at_ms: 42,
            }],
            sufficiency_verdict: Some("1. ANSWERED".into()),
            challenge: None,
        }
    }

    /// `MeteredJournal` is the whole research metric set, so this is the guard on
    /// it staying wired to the numbers `RunRecord` already carries.
    #[tokio::test]
    async fn the_metered_journal_records_a_run_and_still_writes_the_row() {
        struct Inner(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl ResearchJournal for Inner {
            async fn record(&self, _: RunRecord) -> Option<RecordedRun> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
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
            id: uuid::Uuid::new_v4().to_string(),
            scope_json: None,
            retention_days: 90,
            project_guid: "c2d7e2c1-3165-42f5-9366-0ff1492b4bab".into(),
            effort: "medium",
            seed: Some(7),
            temperature: Some(0.2),
            top_p: Some(0.9),
            model_digest: Some("sha256:abc".into()),
            model_details_json: Some(r#"{"parameter_size":"32B"}"#.into()),
            embedder_model_id: "BAAI/bge-m3".into(),
            server_version: "0.0.0-test",
            started_at: 1_700_000_000,
            checkpoint_every_steps: 6,
            scope_spec_json: None,
            kind: "research",
            challenged_run_id: None,
        }
    }

    // One connection: each ":memory:" connection is its own database, so a
    // larger pool would read a different (empty) one back.
    async fn pool() -> SQLite3Pool {
        let pool = SQLite3Pool::new(std::path::Path::new(":memory:"), 1, 16384, "NORMAL");
        // Every migration, through `migration_transaction`, exactly as startup applies
        // them. Pinning this to v1.0.0 alone was a trap: the schema this module writes
        // to is the migrated one, so a test against the base file would fail on any
        // column a later migration added — and would have passed while production
        // broke, had the columns gone the other way.
        pool.migration_transaction(CancellationToken::new(), |tx| {
            for (_, sql) in crate::MIGRATIONS {
                tx.execute_batch(sql)?;
            }
            Ok(())
        })
        .await
        .expect("migrations apply");
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

    /// The stored title is the record's own — nothing derives or defaults it at
    /// write time, so None must land as NULL, not as an empty string a reader
    /// would render.
    #[tokio::test]
    async fn the_title_is_stored_and_null_when_absent() {
        let pool = pool().await;
        insert_run(&pool, ctx(), record(), CancellationToken::new()).await;
        let mut untitled = record();
        untitled.title = None;
        insert_run(&pool, ctx(), untitled, CancellationToken::new()).await;

        let titles: Vec<Option<String>> = pool
            .transaction(CancellationToken::new(), |tx| {
                let mut stmt = tx.prepare("SELECT title FROM research_runs ORDER BY seq")?;
                let rows = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("two rows");
        assert_eq!(titles, vec![Some("Report".to_string()), None]);
    }

    /// The three structured children land in the same transaction as the row —
    /// spans (what re-verification reconstructs `Evidence` from), per-citation
    /// verdicts, and the tool-call trace.
    #[tokio::test]
    async fn the_structured_children_land_with_the_run() {
        let pool = pool().await;
        let c = ctx();
        let run_id = c.id.clone();
        insert_run(&pool, c, record(), CancellationToken::new()).await;

        type EvidenceRow = (String, String);
        type CitationRow = (i64, String, String, i64);
        type StepRow = (String, String, String, i64);
        let (spans, citation, step): (EvidenceRow, CitationRow, StepRow) = pool
            .transaction(CancellationToken::new(), move |tx| {
                let spans = tx.query_row(
                    "SELECT path, spans_json FROM research_run_evidence WHERE run_id = ?1",
                    rusqlite::params![run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let citation = tx.query_row(
                    "SELECT ord, path, verdict, stale FROM research_run_citations \
                     WHERE run_id = ?1 AND ord = 0",
                    rusqlite::params![run_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
                let step = tx.query_row(
                    "SELECT phase, action, argument, hits FROM research_run_steps \
                     WHERE run_id = ?1 AND n = 1",
                    rusqlite::params![run_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
                Ok((spans, citation, step))
            })
            .await
            .expect("children present");
        assert_eq!(spans, ("src/gc.rs".to_string(), "[[10,30]]".to_string()));
        assert_eq!(
            citation,
            (0, "src/gc.rs".to_string(), "verified".to_string(), 1)
        );
        assert_eq!(
            step,
            (
                "main".to_string(),
                "grep".to_string(),
                "sweep".to_string(),
                2
            )
        );
    }

    /// The metadata that used to be measured and then dropped at the journal's
    /// door — plus the request-decided fields that never had columns. NULL means
    /// "not recorded", so the None case must land as NULL, not zero.
    #[tokio::test]
    async fn the_new_metadata_lands_on_the_row_and_absent_reads_as_null() {
        let pool = pool().await;
        let mut with_reval = record();
        with_reval.revalidation = Some(crate::research::Revalidation {
            draft_unverified: 3,
            draft_path_only: 2,
            draft_stale: 1,
            steps: 4,
        });
        insert_run(&pool, ctx(), with_reval, CancellationToken::new()).await;
        let mut bare = record();
        bare.sufficiency_verdict = None;
        let mut c2 = ctx();
        c2.top_p = None;
        c2.model_digest = None;
        insert_run(&pool, c2, bare, CancellationToken::new()).await;

        type Row = (
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<f64>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<f64>,
            Option<i64>,
        );
        let rows: Vec<Row> = pool
            .transaction(CancellationToken::new(), |tx| {
                let mut stmt = tx.prepare(
                    "SELECT revalidation_draft_unverified, revalidation_steps, \
                     sufficiency_verdict, top_p, model_digest, embedder_model_id, \
                     server_version, started_at, granted_context_fraction, \
                     checkpoint_every_steps \
                     FROM research_runs ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                            r.get(8)?,
                            r.get(9)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("two rows");

        let first = &rows[0];
        assert_eq!((first.0, first.1), (Some(3), Some(4)));
        assert_eq!(first.2.as_deref(), Some("1. ANSWERED"));
        assert_eq!(first.3, Some(0.9));
        assert_eq!(first.4.as_deref(), Some("sha256:abc"));
        assert_eq!(first.5.as_deref(), Some("BAAI/bge-m3"));
        assert_eq!(first.6.as_deref(), Some("0.0.0-test"));
        assert_eq!(first.7, Some(1_700_000_000));
        assert_eq!(first.8, Some(0.7));
        assert_eq!(first.9, Some(6));

        let second = &rows[1];
        // No repair happened, nothing was said, nothing was configured: NULL.
        assert_eq!((second.0, second.1), (None, None));
        assert_eq!(second.2, None);
        assert_eq!(second.3, None);
        assert_eq!(second.4, None);
    }

    /// Every journalled row is an ordinary research run: `kind` takes its
    /// DEFAULT and the challenge columns stay NULL — the challenge endpoint
    /// writes its own rows.
    #[tokio::test]
    async fn an_ordinary_run_is_stored_as_kind_research() {
        let pool = pool().await;
        insert_run(&pool, ctx(), record(), CancellationToken::new()).await;

        let (kind, challenged, verdict): (String, Option<String>, Option<String>) = pool
            .transaction(CancellationToken::new(), |tx| {
                Ok(tx.query_row(
                    "SELECT kind, challenged_run_id, challenge_verdict FROM research_runs",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .await
            .expect("one row");
        assert_eq!(kind, "research");
        assert_eq!((challenged, verdict), (None, None));
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
