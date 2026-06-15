use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust, Python, JavaScript, TypeScript, Tsx,
    Go, C, Cpp, Java, CSharp, Ruby, Php,
    Bash, Html, Css, Json, Scala, Haskell, Ocaml, Zig,
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
