use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust, Python, JavaScript, TypeScript, Tsx,
    Go, C, Cpp, Java, CSharp, Ruby, Php,
    Bash, Html, Css, Json, Scala, Haskell, Ocaml, Zig, Sql,
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust       => "rust",
            Language::Python     => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx        => "tsx",
            Language::Go         => "go",
            Language::C          => "c",
            Language::Cpp        => "cpp",
            Language::Java       => "java",
            Language::CSharp     => "csharp",
            Language::Ruby       => "ruby",
            Language::Php        => "php",
            Language::Bash       => "bash",
            Language::Html       => "html",
            Language::Css        => "css",
            Language::Json       => "json",
            Language::Scala      => "scala",
            Language::Haskell    => "haskell",
            Language::Ocaml      => "ocaml",
            Language::Zig        => "zig",
            Language::Sql        => "sql",
        }
    }
}

fn detect_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()? {
        "rs"                                          => Some(Language::Rust),
        "py" | "pyw"                                  => Some(Language::Python),
        "js" | "mjs" | "cjs" | "jsx"                 => Some(Language::JavaScript),
        "ts" | "mts" | "cts"                          => Some(Language::TypeScript),
        "tsx"                                         => Some(Language::Tsx),
        "go"                                          => Some(Language::Go),
        "c" | "h"                                     => Some(Language::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh"  => Some(Language::Cpp),
        "java"                                        => Some(Language::Java),
        "cs"                                          => Some(Language::CSharp),
        "rb"                                          => Some(Language::Ruby),
        "php" | "phtml"                               => Some(Language::Php),
        "sh" | "bash"                                 => Some(Language::Bash),
        "html" | "htm" | "xhtml"                      => Some(Language::Html),
        "css"                                         => Some(Language::Css),
        "json"                                        => Some(Language::Json),
        "scala" | "sc"                                => Some(Language::Scala),
        "hs" | "lhs"                                  => Some(Language::Haskell),
        "ml" | "mli"                                  => Some(Language::Ocaml),
        "zig"                                         => Some(Language::Zig),
        "sql"                                         => Some(Language::Sql),
        _                                             => None,
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
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat)?);
    }
    Ok(Some(builder.build()?))
}

pub fn scan(root: &Path, includes: &[String], excludes: &[String]) -> Result<ScanResult> {
    let include_set = build_globset(includes)?;
    let exclude_set = build_globset(excludes)?;

    let mut files = Vec::new();
    let mut skipped_unknown = 0usize;

    for entry in WalkDir::new(root)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let abs = entry.into_path();

        let rel = match abs.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };

        if let Some(ref excl) = exclude_set {
            if excl.is_match(Path::new(rel.as_str())) {
                continue;
            }
        }

        if let Some(ref incl) = include_set {
            if !incl.is_match(Path::new(rel.as_str())) {
                continue;
            }
        }

        let Some(lang) = detect_language(Path::new(rel.as_str())) else {
            skipped_unknown += 1;
            continue;
        };

        files.push(FileEntry {
            abs_path: abs,
            rel_path: rel.to_string(),
            language: lang,
        });
    }

    Ok(ScanResult { files, skipped_unknown })
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
            ("README.md", "# readme"),
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
        let result = scan(dir.path(), &[], &[]).unwrap();

        // Forward-slash root-relative paths, in walkdir's sorted order.
        assert_eq!(rel_paths(&result), vec!["scripts/run.py", "src/main.rs", "tools/gen.rs"]);
        let langs: Vec<_> = result.files.iter().map(|f| f.language).collect();
        assert_eq!(langs, vec![Language::Python, Language::Rust, Language::Rust]);
        // README.md has no detectable language: skipped and *counted* (the CLI
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
        let result = scan(dir.path(), &["src/**".to_string()], &[]).unwrap();
        assert_eq!(rel_paths(&result), vec!["src/main.rs"]);
    }
}
