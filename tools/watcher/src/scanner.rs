//! Language detection by file extension — verbatim copy of the relevant parts of
//! tools/indexer/src/scanner.rs (each tool has its own Cargo.lock, so this is an
//! intentional independent copy rather than a shared crate).

use std::path::Path;

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
        }
    }
}

pub fn detect_language(path: &Path) -> Option<Language> {
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_extensions_map_to_their_language() {
        let cases = [
            ("a.rs", Language::Rust),
            ("a.pyw", Language::Python),
            ("a.mjs", Language::JavaScript),
            ("a.tsx", Language::Tsx),
            ("a.hh", Language::Cpp),
            ("a.phtml", Language::Php),
            ("a.lhs", Language::Haskell),
            ("a.mli", Language::Ocaml),
            ("a.sql", Language::Sql),
        ];
        for (path, want) in cases {
            assert_eq!(detect_language(Path::new(path)), Some(want), "{path}");
        }
    }

    #[test]
    fn unknown_or_missing_extension_detects_nothing() {
        // A silently-skipped file is the failure mode the language checklist warns
        // about — `None` here is what makes the caller count it, not index it.
        for path in ["README.md", "Makefile", "a.rs.bak", ".gitignore"] {
            assert_eq!(detect_language(Path::new(path)), None, "{path}");
        }
    }
}
