//! Request-edge validation: reject malformed input *here* (as a 400 with a stable
//! [`ApiError`] code) instead of letting it surface later as an opaque 500 from a
//! SQLite `CHECK`/trigger, or as unbounded resource use. The format checks
//! (`validate_path`, `validate_sha256_hex`) mirror the schema constraints so the DB
//! stays the last line of defense, not the first. The cap checks take their limits
//! from config (threaded via `RouterState`), so every bound is a tunable knob.

use std::collections::HashMap;

use crate::backend::error::ApiError;
use crate::backend::v0::models::{
    Code, DriftRequest, HistoryPruneQuery, HistoryRequest, IndexRequest, SearchFilter,
};

/// Mirror of the `project_files.path` CHECK plus a `..`-traversal guard: non-empty,
/// repo-relative (no leading `/`), no empty component (`//`), no backslash, no `..`.
pub fn validate_path(path: &str) -> Result<(), ApiError> {
    let invalid = path.is_empty()
        || path.starts_with('/')
        || path.contains("//")
        || path.contains('\\')
        || path.split('/').any(|seg| seg == "..");
    if invalid {
        Err(ApiError::PathInvalid {
            path: path.to_string(),
        })
    } else {
        Ok(())
    }
}

/// A sha256 must be exactly 64 hexadecimal characters (the schema only checks length).
pub fn validate_sha256_hex(path: &str, sha: &str) -> Result<(), ApiError> {
    if sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ApiError::Sha256Invalid {
            path: path.to_string(),
        })
    }
}

/// `top_k`, when present, must be within `1..=max`. (Omitted → server default, fine.)
pub fn validate_top_k(top_k: Option<usize>, max: u64) -> Result<(), ApiError> {
    if let Some(k) = top_k {
        let k = k as u64;
        if k < 1 || k > max {
            return Err(ApiError::TopKOutOfRange { got: k, max });
        }
    }
    Ok(())
}

/// The search query must be non-empty and within the byte cap.
pub fn validate_query(query: &str, max_bytes: usize) -> Result<(), ApiError> {
    if query.is_empty() {
        return Err(ApiError::QueryEmpty);
    }
    if query.len() > max_bytes {
        return Err(ApiError::QueryTooLong {
            got: query.len(),
            max: max_bytes,
        });
    }
    Ok(())
}

/// Ceilings for a research `budget` override (`[research].max_request_*`).
pub struct ResearchBudgetCaps {
    pub max_seconds: u64,
    pub max_tokens: u64,
    pub max_steps: usize,
}

/// A research `budget` override: every present axis must be within `1..=cap`.
///
/// Zero is rejected rather than clamped — `max_steps = 0` would let the model report
/// on no evidence at all, and `max_seconds = 0` would end the run before its first
/// turn. Both are far more likely to be a client bug than an intent, and a budget
/// silently rounded up to 1 is worse than a 400 that says so.
pub fn research_budget(
    budget: &Option<crate::backend::v0::models::ResearchBudgetOverride>,
    caps: &ResearchBudgetCaps,
) -> Result<(), ApiError> {
    let Some(b) = budget else { return Ok(()) };
    for (field, got, max) in [
        ("max_seconds", b.max_seconds, Some(caps.max_seconds)),
        ("max_tokens", b.max_tokens, Some(caps.max_tokens)),
        (
            "max_steps",
            b.max_steps.map(|v| v as u64),
            Some(caps.max_steps as u64),
        ),
    ] {
        let (Some(got), Some(max)) = (got, max) else {
            continue;
        };
        if got < 1 || got > max {
            return Err(ApiError::ResearchBudgetOutOfRange { field, got, max });
        }
    }
    Ok(())
}

/// The `context_run_ids` count, against `[research].max_context_runs`.
///
/// Count only — whether the ids exist and belong to this project needs the database
/// and so happens in the handler.
///
/// Repeats are dropped rather than rejected, and the **de-duplicated** list is what
/// the run is given and what it journals, so nothing downstream can disagree about
/// what the run was shown. Injecting one report twice would cost its characters twice
/// against `max_context_chars` for no information, and a 400 would fail a request
/// whose intent is unambiguous. The cap is applied to what actually gets used.
///
/// A cap of `0` switches the feature off, so any id is then a rejection — which is
/// why the empty case returns early rather than comparing against the cap.
pub fn research_context(ids: &mut Vec<String>, max: usize) -> Result<(), ApiError> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    ids.retain(|id| seen.insert(id.clone()));
    if ids.len() > max {
        return Err(ApiError::ResearchContextTooMany {
            got: ids.len(),
            max,
        });
    }
    Ok(())
}

/// The `limit` on the stored-research list, against `[research].list_page_limit`.
/// Absent = the server's own page size, so only an explicit value is checked.
pub fn research_list_limit(limit: Option<usize>, max: usize) -> Result<(), ApiError> {
    let Some(got) = limit else { return Ok(()) };
    if got < 1 || got > max {
        return Err(ApiError::ResearchListLimitOutOfRange { got, max });
    }
    Ok(())
}

/// One `include`/`exclude` selector: its globs + languages combined must stay within
/// the pattern cap. (Glob *syntax* is already validated when `GlobPattern` deserializes.)
pub fn validate_selector(
    filter: &Option<SearchFilter>,
    max_patterns: usize,
) -> Result<(), ApiError> {
    if let Some(f) = filter {
        let n = f.paths.as_ref().map_or(0, Vec::len)
            + f.programming_languages.as_ref().map_or(0, Vec::len);
        if n > max_patterns {
            return Err(ApiError::SelectorTooLarge {
                got: n,
                max: max_patterns,
            });
        }
    }
    Ok(())
}

/// A `/symbols` body: non-empty `name` within the byte cap, `limit` (when present)
/// within `1..=max_results`.
pub fn validate_symbols_request(
    name: &str,
    limit: Option<usize>,
    max_name_bytes: usize,
    max_results: usize,
) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(ApiError::SymbolNameEmpty);
    }
    if name.len() > max_name_bytes {
        return Err(ApiError::SymbolNameTooLong {
            got: name.len(),
            max: max_name_bytes,
        });
    }
    if let Some(l) = limit
        && (l < 1 || l > max_results)
    {
        return Err(ApiError::SymbolLimitOutOfRange {
            got: l,
            max: max_results,
        });
    }
    Ok(())
}

/// At least one of `include`/`exclude` must carry a non-empty `paths` or
/// `programming_languages` list — guards the destructive management endpoints from an
/// empty selector that would otherwise match the whole project.
pub fn require_nonempty_selector(
    include: &Option<SearchFilter>,
    exclude: &Option<SearchFilter>,
) -> Result<(), ApiError> {
    let nonempty = |f: &Option<SearchFilter>| {
        f.as_ref().is_some_and(|x| {
            x.paths.as_ref().is_some_and(|p| !p.is_empty())
                || x.programming_languages
                    .as_ref()
                    .is_some_and(|l| !l.is_empty())
        })
    };
    if nonempty(include) || nonempty(exclude) {
        Ok(())
    } else {
        Err(ApiError::SelectorEmpty)
    }
}

/// Validate an `/index` body before any work: file-count cap, each path's format, and
/// each file's source size. Fails on the first problem (the response names it).
pub fn validate_index_request(
    req: &IndexRequest,
    max_files: usize,
    max_code_bytes: usize,
) -> Result<(), ApiError> {
    let total: usize = req.files.values().map(HashMap::len).sum();
    if total > max_files {
        return Err(ApiError::TooManyFiles {
            got: total,
            max: max_files,
        });
    }
    for files in req.files.values() {
        for (path, Code { code }) in files {
            validate_path(path)?;
            if code.len() > max_code_bytes {
                return Err(ApiError::CodeTooLarge {
                    path: path.clone(),
                    got: code.len(),
                    max: max_code_bytes,
                });
            }
        }
    }
    Ok(())
}

/// Validate a `/drift` body: entry-count cap, each path's format, each sha256's format.
pub fn validate_drift_request(req: &DriftRequest, max_files: usize) -> Result<(), ApiError> {
    if req.files.len() > max_files {
        return Err(ApiError::TooManyFiles {
            got: req.files.len(),
            max: max_files,
        });
    }
    for (path, sha) in &req.files {
        validate_path(path)?;
        validate_sha256_hex(path, sha)?;
    }
    Ok(())
}

/// A commit sha must be 40 (SHA-1) or 64 (SHA-256) hexadecimal characters.
/// Mirrors the schema's length CHECK and adds the alphabet, which SQLite does
/// not check — the same split as `validate_sha256_hex`.
pub fn validate_git_sha(sha: &str) -> Result<(), ApiError> {
    if matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ApiError::CommitInvalid {
            sha: sha.to_string(),
            reason: "a sha must be 40 or 64 hexadecimal characters",
        })
    }
}

/// Validate a `/history` body: commit-count cap, then per commit its sha, its
/// message size, and every path it names.
///
/// The `old_path` rule is a biconditional rather than a "required for renames"
/// check: a rename with no source is unusable (the lookup that follows the move
/// is exactly what the column exists for), and a modification carrying one is a
/// client that mis-parsed git's raw output — the arity trap in `--raw -z`, where
/// a rename emits two paths and everything else emits one. Silently accepting
/// the second would store a whole desynchronised stream.
pub fn validate_history_request(
    req: &HistoryRequest,
    max_commits: usize,
    max_message_bytes: usize,
) -> Result<(), ApiError> {
    if req.commits.len() > max_commits {
        return Err(ApiError::TooManyCommits {
            got: req.commits.len(),
            max: max_commits,
        });
    }
    for c in &req.commits {
        validate_git_sha(&c.sha)?;
        if c.subject.trim().is_empty() {
            return Err(ApiError::CommitInvalid {
                sha: c.sha.clone(),
                reason: "the subject must not be empty",
            });
        }
        let message_bytes = c.subject.len() + c.body.len();
        if message_bytes > max_message_bytes {
            return Err(ApiError::CommitMessageTooLarge {
                sha: c.sha.clone(),
                got: message_bytes,
                max: max_message_bytes,
            });
        }
        for p in &c.paths {
            validate_path(&p.path)?;
            match (&p.old_path, p.change_type.requires_old_path()) {
                (Some(old), true) => validate_path(old)?,
                (None, false) => {}
                (None, true) => {
                    return Err(ApiError::CommitInvalid {
                        sha: c.sha.clone(),
                        reason: "a renamed or copied path must carry its old_path",
                    });
                }
                (Some(_), false) => {
                    return Err(ApiError::CommitInvalid {
                        sha: c.sha.clone(),
                        reason: "old_path is only meaningful for a rename or a copy",
                    });
                }
            }
        }
    }
    Ok(())
}

/// `DELETE /v0/{guid}/history` — refuse a prune that names no bound.
///
/// The `require_nonempty_selector` rule, for a resource whose bounds are scalars
/// rather than globs: a request that forgot its parameters and a request that
/// means "drop everything" must not be the same request. `?keep_last=0` is the
/// explicit spelling of the second.
pub fn validate_history_prune(q: &HistoryPruneQuery) -> Result<(), ApiError> {
    if q.keep_last.is_none() && q.older_than.is_none() {
        return Err(ApiError::HistoryBoundMissing);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::v0::models::{
        ChangeType, CommitEntry, CommitPath, GlobPattern, ProgrammingLanguage,
    };
    use glob::Pattern;

    fn err_code(e: ApiError) -> &'static str {
        e.code()
    }

    #[test]
    fn path_rules_match_schema() {
        assert!(validate_path("src/main.rs").is_ok());
        assert!(validate_path("a/b/c.py").is_ok());
        for bad in ["", "/etc/passwd", "a//b", "a\\b", "../secrets", "a/../b"] {
            assert_eq!(
                err_code(validate_path(bad).unwrap_err()),
                "validation.path_invalid",
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn sha256_must_be_64_hex() {
        let ok = "a".repeat(64);
        assert!(validate_sha256_hex("p", &ok).is_ok());
        assert!(validate_sha256_hex("p", &"A1b2".repeat(16)).is_ok()); // mixed case hex
        for bad in [&"a".repeat(63), &"a".repeat(65), &"g".repeat(64), ""] {
            assert_eq!(
                err_code(validate_sha256_hex("p", bad).unwrap_err()),
                "validation.sha256_invalid"
            );
        }
    }

    #[test]
    fn research_budget_bounds() {
        use crate::backend::v0::models::ResearchBudgetOverride;
        let caps = ResearchBudgetCaps {
            max_seconds: 1800,
            max_tokens: 4_000_000,
            max_steps: 200,
        };
        assert!(research_budget(&None, &caps).is_ok());
        // A partial override is normal: absent axes keep the effort preset.
        assert!(
            research_budget(
                &Some(ResearchBudgetOverride {
                    max_seconds: Some(1800),
                    ..Default::default()
                }),
                &caps
            )
            .is_ok()
        );
        for bad in [
            ResearchBudgetOverride {
                max_seconds: Some(1801),
                ..Default::default()
            },
            ResearchBudgetOverride {
                max_seconds: Some(0),
                ..Default::default()
            },
            ResearchBudgetOverride {
                max_tokens: Some(4_000_001),
                ..Default::default()
            },
            ResearchBudgetOverride {
                max_steps: Some(201),
                ..Default::default()
            },
        ] {
            assert_eq!(
                err_code(research_budget(&Some(bad), &caps).unwrap_err()),
                "validation.research_budget_out_of_range",
                "{bad:?} must be rejected at the edge"
            );
        }
        // The rejection names the axis — one code for three fields, so `field` is
        // the only thing that tells a client which one to fix.
        let e = research_budget(
            &Some(ResearchBudgetOverride {
                max_steps: Some(0),
                ..Default::default()
            }),
            &caps,
        )
        .unwrap_err();
        assert!(
            matches!(e, ApiError::ResearchBudgetOutOfRange { field, .. } if field == "max_steps")
        );
        // …and points at the right config key: the ceiling for `max_steps` is
        // `max_request_steps`, not `max_request_max_steps`.
        let problem = crate::backend::error::ProblemDetails::from(&e);
        assert!(
            problem.detail.contains("[research].max_request_steps"),
            "the fix-it hint must name a key that exists: {}",
            problem.detail
        );
    }

    #[test]
    fn top_k_bounds() {
        assert!(validate_top_k(None, 100).is_ok());
        assert!(validate_top_k(Some(1), 100).is_ok());
        assert!(validate_top_k(Some(100), 100).is_ok());
        assert_eq!(
            err_code(validate_top_k(Some(0), 100).unwrap_err()),
            "validation.top_k_out_of_range"
        );
        assert_eq!(
            err_code(validate_top_k(Some(101), 100).unwrap_err()),
            "validation.top_k_out_of_range"
        );
    }

    #[test]
    fn symbols_request_bounds() {
        assert!(validate_symbols_request("collection_for", None, 512, 50).is_ok());
        assert!(validate_symbols_request("f", Some(1), 512, 50).is_ok());
        assert!(validate_symbols_request("f", Some(50), 512, 50).is_ok());
        assert_eq!(
            err_code(validate_symbols_request("", None, 512, 50).unwrap_err()),
            "validation.symbol_name_empty"
        );
        assert_eq!(
            err_code(validate_symbols_request(&"x".repeat(513), None, 512, 50).unwrap_err()),
            "validation.symbol_name_too_long"
        );
        assert_eq!(
            err_code(validate_symbols_request("f", Some(0), 512, 50).unwrap_err()),
            "validation.symbol_limit_out_of_range"
        );
        assert_eq!(
            err_code(validate_symbols_request("f", Some(51), 512, 50).unwrap_err()),
            "validation.symbol_limit_out_of_range"
        );
    }

    #[test]
    fn query_non_empty_and_bounded() {
        assert!(validate_query("hello", 1024).is_ok());
        assert_eq!(
            err_code(validate_query("", 1024).unwrap_err()),
            "validation.query_empty"
        );
        assert_eq!(
            err_code(validate_query("abcd", 3).unwrap_err()),
            "validation.query_too_long"
        );
    }

    #[test]
    fn selector_pattern_cap_and_emptiness() {
        let big = SearchFilter {
            paths: Some(
                ["a*", "b*", "c*"]
                    .iter()
                    .map(|p| GlobPattern(Pattern::new(p).unwrap()))
                    .collect(),
            ),
            programming_languages: None,
        };
        assert_eq!(
            err_code(validate_selector(&Some(big), 2).unwrap_err()),
            "validation.selector_too_large"
        );
        assert_eq!(
            err_code(require_nonempty_selector(&None, &None).unwrap_err()),
            "selector.empty"
        );
        let lang = SearchFilter {
            paths: None,
            programming_languages: Some(vec![ProgrammingLanguage::Rust]),
        };
        assert!(require_nonempty_selector(&Some(lang), &None).is_ok());
    }

    #[test]
    fn index_request_caps() {
        let mut files = HashMap::new();
        let mut inner = HashMap::new();
        inner.insert(
            "src/a.rs".to_string(),
            Code {
                code: "x".repeat(10),
            },
        );
        files.insert(ProgrammingLanguage::Rust, inner);
        let req = IndexRequest {
            files,
            force: false,
            symbols_only: false,
        };

        assert!(validate_index_request(&req, 10, 100).is_ok());
        assert_eq!(
            err_code(validate_index_request(&req, 0, 100).unwrap_err()),
            "validation.too_many_files"
        );
        assert_eq!(
            err_code(validate_index_request(&req, 10, 5).unwrap_err()),
            "validation.code_too_large"
        );
    }

    fn commit(paths: Vec<CommitPath>) -> CommitEntry {
        CommitEntry {
            sha: "a".repeat(40),
            author_name: "T".into(),
            author_email: "t@example.com".into(),
            authored_at: 1,
            committed_at: 1,
            parent_count: 1,
            subject: "subject".into(),
            body: "body".into(),
            paths,
        }
    }

    fn touch(path: &str, change_type: ChangeType, old_path: Option<&str>) -> CommitPath {
        CommitPath {
            path: path.to_string(),
            change_type,
            old_path: old_path.map(str::to_string),
        }
    }

    #[test]
    fn git_sha_must_be_40_or_64_hex() {
        assert!(validate_git_sha(&"a".repeat(40)).is_ok());
        assert!(validate_git_sha(&"F".repeat(64)).is_ok());
        for bad in [
            "a".repeat(39),
            "a".repeat(41),
            "a".repeat(63),
            "g".repeat(40),
            String::new(),
        ] {
            assert_eq!(
                err_code(validate_git_sha(&bad).unwrap_err()),
                "validation.commit_invalid",
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn history_request_caps() {
        let req = HistoryRequest {
            since: None,
            commits: vec![commit(vec![touch("src/a.rs", ChangeType::Modified, None)])],
        };
        assert!(validate_history_request(&req, 10, 100).is_ok());
        assert_eq!(
            err_code(validate_history_request(&req, 0, 100).unwrap_err()),
            "validation.too_many_commits"
        );
        assert_eq!(
            err_code(validate_history_request(&req, 10, 3).unwrap_err()),
            "validation.commit_message_too_large"
        );
    }

    /// A prune with no bound is refused, and `keep_last=0` is how "everything"
    /// is spelled — the two must not be the same request, which is exactly the
    /// argument `require_nonempty_selector` makes for the file endpoints.
    #[test]
    fn a_history_prune_must_name_at_least_one_bound() {
        assert_eq!(
            err_code(validate_history_prune(&HistoryPruneQuery::default()).unwrap_err()),
            "validation.history_bound_missing"
        );
        assert!(
            validate_history_prune(&HistoryPruneQuery {
                keep_last: Some(0),
                older_than: None,
            })
            .is_ok()
        );
        assert!(
            validate_history_prune(&HistoryPruneQuery {
                keep_last: None,
                older_than: Some(0),
            })
            .is_ok()
        );
    }

    /// The `old_path` biconditional. The `Some`-on-a-modification half is the one
    /// worth pinning: it is how a client that mis-parsed git's `--raw -z` arity
    /// (a rename emits two paths, everything else emits one) is caught at the
    /// edge rather than storing a whole desynchronised stream.
    #[test]
    fn old_path_is_required_exactly_for_renames_and_copies() {
        let cases = [
            (touch("b.rs", ChangeType::Renamed, Some("a.rs")), true),
            (touch("b.rs", ChangeType::Copied, Some("a.rs")), true),
            (touch("a.rs", ChangeType::Modified, None), true),
            (touch("b.rs", ChangeType::Renamed, None), false),
            (touch("a.rs", ChangeType::Modified, Some("z.rs")), false),
            (touch("a.rs", ChangeType::Added, Some("z.rs")), false),
        ];
        for (path, ok) in cases {
            let label = format!("{:?} old_path={:?}", path.change_type, path.old_path);
            let req = HistoryRequest {
                since: None,
                commits: vec![commit(vec![path])],
            };
            let got = validate_history_request(&req, 10, 1000);
            if ok {
                assert!(got.is_ok(), "{label} should be accepted");
            } else {
                assert_eq!(
                    err_code(got.unwrap_err()),
                    "validation.commit_invalid",
                    "{label} should be rejected"
                );
            }
        }
    }

    #[test]
    fn history_request_rejects_an_empty_subject() {
        let mut c = commit(vec![]);
        c.subject = "   ".into();
        let req = HistoryRequest {
            since: None,
            commits: vec![c],
        };
        assert_eq!(
            err_code(validate_history_request(&req, 10, 1000).unwrap_err()),
            "validation.commit_invalid"
        );
    }
}
