use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

#[derive(Debug)]
pub struct MindexFile {
    /// Project GUID — passed as-is from the file (dashes preserved).
    pub guid: String,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    /// Lowercase mindex language ids; empty means all languages.
    pub languages: Vec<String>,
}

pub fn parse(path: &Path) -> Result<MindexFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read .mindex file at {}", path.display()))?;

    let mut guid = None;
    let mut include_paths = Vec::new();
    let mut exclude_paths = Vec::new();
    let mut languages = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("include_paths:") {
            include_paths = comma_list(line.trim_start_matches("include_paths:").trim());
        } else if line.starts_with("exclude_paths:") {
            exclude_paths = comma_list(line.trim_start_matches("exclude_paths:").trim());
        } else if line.starts_with("languages:") {
            languages = comma_list(line.trim_start_matches("languages:").trim());
        } else if guid.is_none() {
            guid = Some(line.to_string());
        }
    }

    let guid = guid.ok_or_else(|| {
        anyhow::anyhow!(
            "no project GUID found in {} — the first non-comment non-blank line must be the GUID",
            path.display()
        )
    })?;

    Ok(MindexFile {
        guid,
        include_paths,
        exclude_paths,
        languages,
    })
}

fn comma_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

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
        builder.add(Glob::new(pat).with_context(|| format!("invalid glob pattern: {pat}"))?);
    }
    Ok(Some(builder.build()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(text: &str) -> Result<MindexFile> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mindex");
        std::fs::write(&path, text).unwrap();
        parse(&path)
    }

    #[test]
    fn parses_guid_scope_lines_comments_and_blanks() {
        let f = parse_str(
            "# project index id — comments and blank lines are skipped\n\
             \n\
             123e4567-e89b-42d3-a456-426614174000\n\
             include_paths: src/**, tools/mcp/**\n\
             exclude_paths: target/** ,, docs/**\n\
             languages: rust, python\n",
        )
        .unwrap();
        // The GUID is passed through as-is (dashes preserved, no normalization).
        assert_eq!(f.guid, "123e4567-e89b-42d3-a456-426614174000");
        assert_eq!(f.include_paths, vec!["src/**", "tools/mcp/**"]);
        // Whitespace is trimmed and empty comma-list entries dropped.
        assert_eq!(f.exclude_paths, vec!["target/**", "docs/**"]);
        assert_eq!(f.languages, vec!["rust", "python"]);
    }

    #[test]
    fn guid_only_file_has_empty_scope() {
        let f = parse_str("deadbeefdeadbeefdeadbeefdeadbeef\n").unwrap();
        assert_eq!(f.guid, "deadbeefdeadbeefdeadbeefdeadbeef");
        assert!(f.include_paths.is_empty());
        assert!(f.exclude_paths.is_empty());
        assert!(f.languages.is_empty());
    }

    #[test]
    fn missing_guid_is_an_error() {
        // Scope lines alone don't identify a project.
        let err = parse_str("# only a comment\ninclude_paths: src/**\n").unwrap_err();
        assert!(err.to_string().contains("GUID"), "{err}");
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
}
