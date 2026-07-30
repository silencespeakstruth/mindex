//! Request-edge validation: reject malformed input *here* (as a 400 with a stable
//! [`ApiError`] code) instead of letting it surface later as an opaque 500 from a
//! SQLite `CHECK`/trigger, or as unbounded resource use. The format checks
//! (`validate_path`, `validate_sha256_hex`) mirror the schema constraints so the DB
//! stays the last line of defense, not the first. The cap checks take their limits
//! from config (threaded via `RouterState`), so every bound is a tunable knob.

use std::collections::HashMap;

use crate::backend::error::ApiError;
use crate::backend::v0::models::{Code, DriftRequest, IndexRequest, SearchFilter};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::v0::models::{GlobPattern, ProgrammingLanguage};
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
}
