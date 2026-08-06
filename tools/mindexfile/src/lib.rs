//! The file formats every Rust mindex client shares.
//!
//! Two of them: the repo-root `.mindex` project marker (this module) and the
//! per-server credentials file ([`credentials`]). Both live here for the same
//! reason — a second copy of either parser in the watcher would be a copy that
//! drifts, and for the credentials one the drift would be in a permission check.
//!
//! # The `.mindex` project marker
//!
//! This crate is the **reference implementation** of the format: the indexer
//! (`mindex-index`), the watcher (`mindex-watch`) and — via a mirrored parser in
//! TypeScript, `tools/vscode/src/mindexFile.ts` — the VS Code extension all read the
//! same file, and a disagreement between them shows up as phantom drift rather than
//! as an error. Keep the two implementations in step.
//!
//! ```yaml
//! guid: c2d7e2c1-3165-42f5-9366-0ff1492b4bab
//! exclude_paths:
//!   - tools/**
//!   - target/**
//! include_paths: []
//! languages: []
//! ```
//!
//! Unknown keys are a hard error, the same choice the server's TOML config makes: a
//! mistyped `exclude_path:` that is silently ignored means the excluded tree gets
//! indexed, which is the failure this format exists to prevent.

pub mod credentials;
pub mod token;

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::path::Path;

/// File name, at the root of the project tree. One file, no nesting.
pub const FILE_NAME: &str = ".mindex";

/// Largest file body any client may send as one `code` value — the server's
/// `[limits].max_code_bytes` default.
///
/// A client that hashes an over-cap file into its drift manifest reports it as
/// `missing`, posts it, and gets **the whole batch** rejected with a 400; the file
/// then stays `missing` forever and takes its batch-mates with it. So every client
/// must drop such files *before* hashing, exactly as it drops binary ones. The VS
/// Code extension keeps its own copy of this number (`MAX_CODE_BYTES` in
/// `tools/vscode/src/scanner.ts`) — change one, change the other.
pub const MAX_CODE_BYTES: u64 = 16 * 1024 * 1024;

/// The project's identity and standing indexing/search scope.
#[derive(Debug, Clone)]
pub struct MindexFile {
    /// Project GUID, normalized to canonical hyphenated lowercase. The server
    /// parses it into a `Uuid`, so the dashed and dashless spellings address the
    /// same project — normalizing here just makes every tool agree on one.
    pub guid: String,
    /// Root-relative globs; empty means "no filter", not "nothing".
    pub include_paths: Vec<String>,
    /// Root-relative globs, evaluated *before* the includes.
    pub exclude_paths: Vec<String>,
    /// Lowercase mindex language ids; empty means all languages.
    pub languages: Vec<String>,
    /// Ref *patterns* bounding the git-history walk, e.g. `master`, `dev`,
    /// `feat/*`. Empty means the client's own default (the current branch).
    ///
    /// This is a scope key like the three above, not a feature switch: whether a
    /// project's history is indexed at all is the client's `--history` flag. What
    /// belongs in the committed file is *which* refs carry that project's
    /// history — an answer about the project, not about one machine. It matters
    /// more than it looks: a repository whose default branch was squashed keeps
    /// its prose on the feature branches, and walking `HEAD` there finds two
    /// commits.
    pub git_refs: Vec<String>,
}

/// The wire shape. Separate from [`MindexFile`] so the public type can hold the
/// normalized GUID rather than whatever the file happened to spell.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    guid: String,
    #[serde(default)]
    include_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    git_refs: Vec<String>,
}

/// Reads and parses `path`. A missing file is an error — callers that treat
/// "no project marker" as a normal state check for the file themselves.
pub fn parse(path: &Path) -> Result<MindexFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read .mindex file at {}", path.display()))?;
    parse_str(&text).with_context(|| format!("invalid .mindex file at {}", path.display()))
}

/// Parses the file's text. Split out from [`parse`] for tests and for callers that
/// already hold the contents.
pub fn parse_str(text: &str) -> Result<MindexFile> {
    let raw: Raw = serde_yaml_ng::from_str(text).context(
        "the file must be YAML with a `guid:` key and optional \
         `include_paths:`/`exclude_paths:`/`languages:`/`git_refs:` lists",
    )?;

    Ok(MindexFile {
        guid: normalize_guid(&raw.guid).context("bad `guid:` key")?,
        include_paths: raw.include_paths,
        exclude_paths: raw.exclude_paths,
        languages: raw.languages,
        git_refs: raw.git_refs,
    })
}

/// Validates a project GUID in either spelling and returns the canonical hyphenated
/// lowercase form. Shared so a `--project` flag is held to the same standard as the
/// file — a typo'd GUID otherwise indexes into a brand-new empty project in silence.
pub fn normalize_guid(guid: &str) -> Result<String> {
    uuid::Uuid::parse_str(guid.trim())
        .map(|u| u.hyphenated().to_string())
        .with_context(|| {
            format!(
                "`{guid}` is not a UUID — expected e.g. \
                 c2d7e2c1-3165-42f5-9366-0ff1492b4bab (dashless is accepted too); \
                 generate one with `uuidgen`"
            )
        })
}

/// Compiles the two scope lists. `None` means "no filter" — an empty include list
/// must not be mistaken for "include nothing".
pub fn build_globsets(
    include: &[String],
    exclude: &[String],
) -> Result<(Option<GlobSet>, Option<GlobSet>)> {
    Ok((build_globset(include)?, build_globset(exclude)?))
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if pat.starts_with('/') || pat.contains('\\') {
            bail!(
                "invalid glob pattern `{pat}`: patterns are relative to the project \
                 root and use forward slashes (write `src/**`, not `/src/**` or `src\\**`)"
            );
        }
        // `literal_separator` is what makes `*` stop at a path separator. globset
        // defaults it off (so `src/*.rs` would match `src/db/qdrant.rs`), the VS Code
        // side's picomatch defaults it on, and gitignore-style intuition expects it
        // on — one setting reconciles all three.
        let glob = GlobBuilder::new(pat)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid glob pattern: {pat}"))?;
        builder.add(glob);
    }
    Ok(Some(builder.build()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUID: &str = "123e4567-e89b-42d3-a456-426614174000";

    fn parse_file(text: &str) -> Result<MindexFile> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, text).unwrap();
        parse(&path)
    }

    #[test]
    fn parses_guid_scope_lists_and_comments() {
        let f = parse_file(
            "# comments and blank lines are YAML's problem, not ours\n\
             \n\
             guid: 123e4567-e89b-42d3-a456-426614174000\n\
             include_paths:\n  - src/**\n  - tools/mcp/**\n\
             exclude_paths:\n  - target/**\n  - docs/**\n\
             languages:\n  - rust\n  - python\n\
             git_refs:\n  - master\n  - \"feat/*\"\n",
        )
        .unwrap();
        assert_eq!(f.guid, GUID);
        assert_eq!(f.include_paths, vec!["src/**", "tools/mcp/**"]);
        assert_eq!(f.exclude_paths, vec!["target/**", "docs/**"]);
        assert_eq!(f.languages, vec!["rust", "python"]);
        assert_eq!(f.git_refs, vec!["master", "feat/*"]);
    }

    #[test]
    fn guid_only_file_has_empty_scope() {
        let f = parse_file("guid: 123e4567-e89b-42d3-a456-426614174000\n").unwrap();
        assert_eq!(f.guid, GUID);
        assert!(f.include_paths.is_empty());
        assert!(f.exclude_paths.is_empty());
        assert!(f.languages.is_empty());
        assert!(f.git_refs.is_empty());
    }

    #[test]
    fn dashless_guid_is_normalized_to_hyphenated() {
        // Both spellings reach the same project server-side; every tool sees one form.
        let f = parse_file("guid: 123e4567e89b42d3a456426614174000\n").unwrap();
        assert_eq!(f.guid, GUID);
    }

    #[test]
    fn missing_guid_is_an_error() {
        let err = parse_file("exclude_paths:\n  - src/**\n").unwrap_err();
        assert!(format!("{err:#}").contains("guid"), "{err:#}");
    }

    #[test]
    fn non_uuid_guid_is_an_error() {
        let err = parse_file("guid: not-a-uuid\n").unwrap_err();
        assert!(format!("{err:#}").contains("uuidgen"), "{err:#}");
    }

    #[test]
    fn unknown_key_is_an_error() {
        // The whole point of the format change: a typo must not silently widen scope.
        let err = parse_file(&format!("guid: {GUID}\nexclude_path:\n  - target/**\n")).unwrap_err();
        assert!(format!("{err:#}").contains("exclude_path"), "{err:#}");
    }

    #[test]
    fn scalar_instead_of_list_is_an_error() {
        // The old comma-separated form must fail loudly, not parse as one odd glob.
        let err = parse_file(&format!(
            "guid: {GUID}\nexclude_paths: target/**, docs/**\n"
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("exclude_paths"), "{err:#}");
    }

    #[test]
    fn globsets_are_none_when_empty_and_match_when_built() {
        let (inc, exc) = build_globsets(&[], &[]).unwrap();
        assert!(
            inc.is_none() && exc.is_none(),
            "empty patterns must mean 'no filter'"
        );

        let (inc, _) = build_globsets(&["src/**".to_string()], &[]).unwrap();
        let inc = inc.unwrap();
        assert!(inc.is_match("src/a/b.rs"));
        assert!(!inc.is_match("tools/a.rs"));
    }

    #[test]
    fn invalid_glob_is_a_readable_error() {
        let err = build_globsets(&["src/[".to_string()], &[]).unwrap_err();
        assert!(err.to_string().contains("src/["), "{err}");
    }

    #[test]
    fn absolute_and_backslash_globs_are_rejected() {
        // globset would accept these and then silently never match a root-relative path.
        for pat in ["/src/**", "src\\**"] {
            let err = build_globsets(&[pat.to_string()], &[]).unwrap_err();
            assert!(err.to_string().contains("forward slashes"), "{pat}: {err}");
        }
    }

    /// The cross-implementation glob contract. `tools/vscode/src/globContract.test.ts`
    /// runs the identical table through picomatch: the two engines differ, the
    /// supported subset must not. Keep the two tables byte-identical.
    #[test]
    fn glob_contract_matches_the_documented_subset() {
        // (pattern, path, expected). Every path is a FILE path: both scanners match
        // globs against files only, never against a bare directory, and that is
        // where the two engines still differ (picomatch says `tools/**` matches
        // `tools`, globset says it does not).
        let cases: &[(&str, &str, bool)] = &[
            ("tools/**", "tools/a.rs", true),
            ("tools/**", "tools/deep/nested/a.rs", true),
            ("tools/**", "src/tools/a.rs", false),
            ("**/target/**", "a/b/target/x.rs", true),
            ("**/target/**", "target/x.rs", true),
            ("**/*.lock", "Cargo.lock", true),
            ("**/*.lock", "tools/indexer/Cargo.lock", true),
            ("src/*.rs", "src/main.rs", true),
            ("src/*.rs", "src/db/qdrant.rs", false),
            ("src/?.rs", "src/a.rs", true),
            ("src/?.rs", "src/ab.rs", false),
            ("src/[ab].rs", "src/a.rs", true),
            ("src/[ab].rs", "src/c.rs", false),
            (".claude/**", ".claude/settings.json", true),
            ("**/.venv/**", "tools/mcp/.venv/lib/x.py", true),
        ];
        for (pat, path, expected) in cases {
            let (_, exc) = build_globsets(&[], &[(*pat).to_string()]).unwrap();
            assert_eq!(
                exc.unwrap().is_match(path),
                *expected,
                "pattern `{pat}` vs path `{path}`"
            );
        }
    }
}
