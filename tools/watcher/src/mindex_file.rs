use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

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

    Ok(MindexFile { guid, include_paths, exclude_paths, languages })
}

fn comma_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
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
