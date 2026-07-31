use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
    Java,
    CSharp,
    Ruby,
    Php,
    Bash,
    Html,
    Css,
    Json,
    Scala,
    Haskell,
    Ocaml,
    Zig,
    Sql,
    Toml,
    Yaml,
    Markdown,
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Go => "go",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Java => "java",
            Language::CSharp => "csharp",
            Language::Ruby => "ruby",
            Language::Php => "php",
            Language::Bash => "bash",
            Language::Html => "html",
            Language::Css => "css",
            Language::Json => "json",
            Language::Scala => "scala",
            Language::Haskell => "haskell",
            Language::Ocaml => "ocaml",
            Language::Zig => "zig",
            Language::Sql => "sql",
            Language::Toml => "toml",
            Language::Yaml => "yaml",
            Language::Markdown => "markdown",
        }
    }
}

fn detect_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()? {
        "rs" => Some(Language::Rust),
        "py" | "pyw" => Some(Language::Python),
        "js" | "mjs" | "cjs" | "jsx" => Some(Language::JavaScript),
        "ts" | "mts" | "cts" => Some(Language::TypeScript),
        "tsx" => Some(Language::Tsx),
        "go" => Some(Language::Go),
        "c" | "h" => Some(Language::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Some(Language::Cpp),
        "java" => Some(Language::Java),
        "cs" => Some(Language::CSharp),
        "rb" => Some(Language::Ruby),
        "php" | "phtml" => Some(Language::Php),
        "sh" | "bash" => Some(Language::Bash),
        "html" | "htm" | "xhtml" => Some(Language::Html),
        "css" => Some(Language::Css),
        "json" => Some(Language::Json),
        "scala" | "sc" => Some(Language::Scala),
        "hs" | "lhs" => Some(Language::Haskell),
        "ml" | "mli" => Some(Language::Ocaml),
        "zig" => Some(Language::Zig),
        "sql" => Some(Language::Sql),
        "toml" => Some(Language::Toml),
        "yaml" | "yml" => Some(Language::Yaml),
        "md" | "markdown" => Some(Language::Markdown),
        _ => None,
    }
}

pub struct FileEntry {
    pub abs_path: PathBuf,
    /// Forward-slash path relative to the scan root (stored in mindex as-is).
    pub rel_path: String,
    pub language: Language,
}

pub struct ScanResult {
    pub files: Vec<FileEntry>,
    pub skipped_unknown: usize,
    /// Paths dropped for exceeding [`mindexfile::MAX_CODE_BYTES`]. Named rather than
    /// counted: the server would reject the whole batch one of these travelled in,
    /// so the operator has to be able to see *which* file is out of bounds.
    pub skipped_too_large: Vec<String>,
}

/// Walks `root` applying the project scope: excludes first, then includes, then the
/// language filter. Globs are compiled by `mindexfile` so this agrees exactly with
/// the watcher and (by contract) with the VS Code extension — a file the indexer
/// skips but the extension scans would show up forever as drift.
///
/// `languages` holds lowercase mindex language ids; empty means all languages.
pub fn scan(
    root: &Path,
    includes: &[String],
    excludes: &[String],
    languages: &[String],
) -> Result<ScanResult> {
    let (include_set, exclude_set) = mindexfile::build_globsets(includes, excludes)?;
    let language_set: Option<HashSet<&str>> = if languages.is_empty() {
        None
    } else {
        Some(languages.iter().map(String::as_str).collect())
    };

    let mut files = Vec::new();
    let mut skipped_unknown = 0usize;
    let mut skipped_too_large = Vec::new();

    for entry in WalkDir::new(root)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let too_large = entry
            .metadata()
            .map(|m| m.len() > mindexfile::MAX_CODE_BYTES)
            .unwrap_or(false);
        let abs = entry.into_path();

        let rel = match abs.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };

        if let Some(ref excl) = exclude_set
            && excl.is_match(Path::new(rel.as_str()))
        {
            continue;
        }

        if let Some(ref incl) = include_set
            && !incl.is_match(Path::new(rel.as_str()))
        {
            continue;
        }

        let Some(lang) = detect_language(Path::new(rel.as_str())) else {
            skipped_unknown += 1;
            continue;
        };

        // Out-of-scope languages are deliberately *not* counted as "unknown": the
        // extension is recognised, the project just doesn't index that language.
        if let Some(ref allowed) = language_set
            && !allowed.contains(lang.name())
        {
            continue;
        }

        // Over-cap files are dropped *here*, so they reach neither the drift manifest
        // nor an upload: a file the server will refuse must not be claimed as part of
        // the working tree, or it is reported `missing` on every check forever.
        if too_large {
            skipped_too_large.push(rel.to_string());
            continue;
        }

        files.push(FileEntry {
            abs_path: abs,
            rel_path: rel.to_string(),
            language: lang,
        });
    }

    Ok(ScanResult {
        files,
        skipped_unknown,
        skipped_too_large,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tempdir with a small tree: two known-language files, one unknown, one nested
    /// under a directory the exclude tests target.
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, body) in [
            ("src/main.rs", "fn main() {}"),
            ("scripts/run.py", "print(1)"),
            ("tools/gen.rs", "fn g() {}"),
            // Documentation is a supported language, so it is scanned like any
            // other file; `notes.txt` is the one with no detectable language.
            ("README.md", "# readme"),
            ("notes.txt", "plain"),
        ] {
            let abs = dir.path().join(path);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, body).unwrap();
        }
        dir
    }

    fn rel_paths(result: &ScanResult) -> Vec<&str> {
        result.files.iter().map(|f| f.rel_path.as_str()).collect()
    }

    #[test]
    fn scan_detects_languages_and_counts_unknown_files() {
        let dir = tree();
        let result = scan(dir.path(), &[], &[], &[]).unwrap();

        // Forward-slash root-relative paths, in walkdir's sorted order.
        assert_eq!(
            rel_paths(&result),
            vec!["README.md", "scripts/run.py", "src/main.rs", "tools/gen.rs"]
        );
        let langs: Vec<_> = result.files.iter().map(|f| f.language).collect();
        assert_eq!(
            langs,
            vec![
                Language::Markdown,
                Language::Python,
                Language::Rust,
                Language::Rust
            ]
        );
        // notes.txt has no detectable language: skipped and *counted* (the CLI
        // surfaces this so an unexpected extension isn't silently dropped).
        assert_eq!(result.skipped_unknown, 1);
    }

    #[test]
    fn scan_exclude_wins_over_include() {
        let dir = tree();
        let result = scan(
            dir.path(),
            &["**/*.rs".to_string()],
            &["tools/**".to_string()],
            &[],
        )
        .unwrap();

        // tools/gen.rs matches the include but must still be excluded (the
        // "always --exclude tools/** when indexing mindex itself" convention).
        assert_eq!(rel_paths(&result), vec!["src/main.rs"]);
        // Out-of-include files are filtered before language detection: not "unknown".
        assert_eq!(result.skipped_unknown, 0);
    }

    #[test]
    fn scan_include_restricts_scope() {
        let dir = tree();
        let result = scan(dir.path(), &["src/**".to_string()], &[], &[]).unwrap();
        assert_eq!(rel_paths(&result), vec!["src/main.rs"]);
    }

    #[test]
    fn scan_languages_restrict_scope_without_counting_as_unknown() {
        let dir = tree();
        let result = scan(dir.path(), &[], &[], &["python".to_string()]).unwrap();
        assert_eq!(rel_paths(&result), vec!["scripts/run.py"]);
        // Only notes.txt is unknown; the .rs and .md files were recognised, just
        // out of scope.
        assert_eq!(result.skipped_unknown, 1);
    }

    #[test]
    fn scan_star_does_not_cross_a_directory_separator() {
        // mindexfile compiles globs with literal_separator, matching the VS Code
        // side's picomatch. `src/*.rs` must not reach into src/db/.
        let dir = tempfile::tempdir().unwrap();
        for path in ["src/main.rs", "src/db/qdrant.rs"] {
            let abs = dir.path().join(path);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, "fn f() {}").unwrap();
        }
        let result = scan(dir.path(), &["src/*.rs".to_string()], &[], &[]).unwrap();
        assert_eq!(rel_paths(&result), vec!["src/main.rs"]);
    }
}
