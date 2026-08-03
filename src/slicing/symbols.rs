use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tree_sitter::Language;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

use crate::backend::v0::models::ProgrammingLanguage;

/// Version of everything that turns a file into `project_file_symbols` rows:
/// `queries_for` below, the vendored `.scm` files it embeds, the extraction walk,
/// and the grammar crates the queries are compiled against.
///
/// **Bump this whenever a change would produce different symbol rows for the same
/// source text** — a new/edited/vendored tags query, an `ALL` variant gaining or
/// losing a query, a change to `SymbolExtractor`, or a `tree-sitter-<lang>` bump
/// that alters its tags output. Stored per file in `project_files.symbols_version`
/// and compared by the prepare-phase skip, so a bump makes the next ordinary indexer
/// run rebuild the affected files by itself — no manual reindex, no remembering
/// which projects are behind.
///
/// `MAJOR.MINOR`, the notation documented on
/// [`CHUNKS_DERIVATION_VERSION`](crate::slicing::traits::CHUNKS_DERIVATION_VERSION).
///
/// Not configurable: it describes what the code *is*, not how it is tuned.
pub const SYMBOLS_DERIVATION_VERSION: &str = "1.1";

/// Upstream tags/locals queries for a language, or `None` when the grammar crate
/// ships none. The per-language part is *data* maintained upstream (like the
/// grammars themselves); the extraction algorithm below is one universal
/// implementation. Total over the enum so adding a `ProgrammingLanguage` variant
/// forces a decision here (`None` is a legal one: the language simply yields no
/// symbols).
fn queries_for(pl: ProgrammingLanguage) -> Option<(String, String)> {
    let own = |tags: &str, locals: &str| Some((tags.to_string(), locals.to_string()));
    match pl {
        ProgrammingLanguage::Rust => own(tree_sitter_rust::TAGS_QUERY, ""),
        ProgrammingLanguage::Python => own(tree_sitter_python::TAGS_QUERY, ""),
        ProgrammingLanguage::JavaScript => own(
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        ),
        // The upstream TypeScript tags.scm only covers TS-specific declarations
        // (signatures, interfaces, modules); plain functions/classes/calls are the
        // JS grammar's nodes, which the TS grammar is a superset of — so the JS
        // query is concatenated in (both are upstream data, unmodified).
        ProgrammingLanguage::TypeScript | ProgrammingLanguage::Tsx => Some((
            format!(
                "{}{}",
                tree_sitter_javascript::TAGS_QUERY,
                tree_sitter_typescript::TAGS_QUERY
            ),
            format!(
                "{}{}",
                tree_sitter_javascript::LOCALS_QUERY,
                tree_sitter_typescript::LOCALS_QUERY
            ),
        )),
        ProgrammingLanguage::Go => own(tree_sitter_go::TAGS_QUERY, ""),
        ProgrammingLanguage::C => own(tree_sitter_c::TAGS_QUERY, ""),
        ProgrammingLanguage::Cpp => own(tree_sitter_cpp::TAGS_QUERY, ""),
        ProgrammingLanguage::Java => own(tree_sitter_java::TAGS_QUERY, ""),
        // The csharp crate's tags.scm carries a stray invalid capture; a fixed
        // copy is vendored (see the file's header). Its locals.scm constant is
        // cfg-gated off in the published crate (as is ocaml's), so tags-only.
        ProgrammingLanguage::CSharp => own(include_str!("queries/csharp.tags.scm"), ""),
        ProgrammingLanguage::Ruby => {
            own(tree_sitter_ruby::TAGS_QUERY, tree_sitter_ruby::LOCALS_QUERY)
        }
        ProgrammingLanguage::Php => own(tree_sitter_php::TAGS_QUERY, ""),
        ProgrammingLanguage::Ocaml => own(tree_sitter_ocaml::TAGS_QUERY, ""),
        // The scala crate packages queries/tags.scm but exports no constant.
        ProgrammingLanguage::Scala => own(include_str!("queries/scala.tags.scm"), ""),
        // Documentation defines no symbols: there is no tags query for markdown
        // and there will not be one. Its headings are a table of contents, not
        // definitions, and `outline` would have to mean something different for
        // them than it means everywhere else.
        ProgrammingLanguage::Markdown => None,
        // No upstream tags.scm in these grammar crates: no symbols for now. The
        // three data formats (json, toml, yaml) ship only a highlights query, and
        // whether a key is a "definition" at all is a separate decision from
        // vendoring one — `outline` and `/symbols` mean something specific.
        ProgrammingLanguage::Bash
        | ProgrammingLanguage::Html
        | ProgrammingLanguage::Css
        | ProgrammingLanguage::Json
        | ProgrammingLanguage::Toml
        | ProgrammingLanguage::Yaml
        | ProgrammingLanguage::Haskell
        | ProgrammingLanguage::Zig
        | ProgrammingLanguage::Sql => None,
    }
}

#[derive(Error, Debug)]
pub enum SymbolError {
    #[error("{0}")]
    Tags(#[from] tree_sitter_tags::Error),

    #[error("Cancelled.")]
    Cancelled,
}

/// One **definition** of a name (function/class/…). Spans cover the whole tagged
/// node — a definition's span is its entire body — with 1-indexed lines and byte
/// columns and an exclusive end, the same coordinate conventions as `SlicedChunk`.
///
/// **References are deliberately not extracted.** The tags query emits them, and
/// they were stored until they were measured: 23 810 reference rows against 3 397
/// definitions, 87.5% of the table, serving one tool (`callers`) that ran twice in
/// twenty-five recorded runs at a 50% miss rate. The edges are lexical — a
/// reference row records a token in call position, not which definition it binds
/// to — so on this repo the top referenced names were `assert_eq` (1084), `clone`,
/// `Ok`, `unwrap`, `map`, several of which have exactly one definition in the tree.
/// Nothing aggregates usefully over that, which is why the repo map ranked by it
/// and was withdrawn. Separating a real core abstraction from a name shared with a
/// language builtin needs resolution, i.e. the LSP/SCIP wall this project has
/// declined to climb; `grep` answers "who uses this name" honestly and lexically in
/// the meantime.
#[derive(Debug)]
pub struct ExtractedSymbol {
    pub name: String,
    /// The tags.scm syntax type: `function`, `method`, `class`, `call`, ….
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_column: usize,
    pub end_column: usize,
    /// Nearest enclosing *definition* tag by span containment (one level).
    pub parent_name: Option<String>,
    pub parent_kind: Option<String>,
    /// Doc comment attached to a definition, when the upstream query captures one.
    pub doc: Option<String>,
}

/// Universal symbol extractor: runs the language's upstream tags query via
/// `tree-sitter-tags` and post-processes the flat tag list into
/// `ExtractedSymbol`s. Contains no language-specific logic — parent scopes are
/// derived purely from byte-span containment between tags.
pub struct SymbolExtractor {
    context: TagsContext,
    config: TagsConfiguration,
}

impl SymbolExtractor {
    /// `Ok(None)` when the language has no upstream tags query (no symbols);
    /// `Err` only on a malformed query, which is a build-input defect, not a
    /// per-file condition.
    pub fn for_language(
        pl: ProgrammingLanguage,
        language: Language,
    ) -> Result<Option<Self>, SymbolError> {
        let Some((tags, locals)) = queries_for(pl) else {
            return Ok(None);
        };
        let config = TagsConfiguration::new(language, &tags, &locals)?;
        Ok(Some(Self {
            context: TagsContext::new(),
            config,
        }))
    }

    pub fn extract(
        &mut self,
        code: &str,
        token: &CancellationToken,
    ) -> Result<Vec<ExtractedSymbol>, SymbolError> {
        let source = code.as_bytes();
        let (tags, _has_parse_errors) = self.context.generate_tags(&self.config, source, None)?;

        struct RawTag {
            start_byte: usize,
            end_byte: usize,
            name: String,
            kind: String,
            doc: Option<String>,
        }

        let mut raw: Vec<RawTag> = Vec::new();
        for tag in tags {
            if token.is_cancelled() {
                return Err(SymbolError::Cancelled);
            }
            let tag = tag?;
            // Definitions only — see `ExtractedSymbol`. Dropped here rather than
            // after the parent sweep so references never enter the containment
            // stack: a nested definition's parent must be the definition around it,
            // never a call that happens to span it.
            if !tag.is_definition {
                continue;
            }
            // A name that isn't valid UTF-8 can't be stored or queried; skip the tag.
            let Ok(name) = std::str::from_utf8(&source[tag.name_range.clone()]) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            raw.push(RawTag {
                start_byte: tag.range.start,
                end_byte: tag.range.end,
                name: name.to_string(),
                kind: self.config.syntax_type_name(tag.syntax_type_id).to_string(),
                doc: tag.docs,
            });
        }

        // ── parent assignment: nearest enclosing definition by span containment ──
        // Sweep tags in (start asc, end desc) order keeping a stack of open
        // definitions; the stack top strictly containing the current tag is its
        // parent. Pure span geometry — no language knowledge.
        let mut order: Vec<usize> = (0..raw.len()).collect();
        order.sort_by_key(|&i| (raw[i].start_byte, std::cmp::Reverse(raw[i].end_byte)));

        let mut parents: Vec<Option<usize>> = vec![None; raw.len()];
        let mut open_defs: Vec<usize> = Vec::new();
        for &i in &order {
            while let Some(&top) = open_defs.last() {
                let contains = raw[top].start_byte <= raw[i].start_byte
                    && raw[i].end_byte <= raw[top].end_byte
                    && (raw[top].start_byte, raw[top].end_byte)
                        != (raw[i].start_byte, raw[i].end_byte);
                if contains {
                    break;
                }
                open_defs.pop();
            }
            parents[i] = open_defs.last().copied();
            open_defs.push(i);
        }

        // ── byte offsets → 1-indexed lines + in-line byte columns ──
        let mut line_starts: Vec<usize> = vec![0];
        line_starts.extend(
            code.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i + 1),
        );
        let line_col = |byte: usize| -> (usize, usize) {
            let line_idx = line_starts.partition_point(|&s| s <= byte) - 1;
            (line_idx + 1, byte - line_starts[line_idx])
        };

        let out = raw
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (start_line, start_column) = line_col(t.start_byte);
                let (end_line, end_column) = line_col(t.end_byte);
                ExtractedSymbol {
                    name: t.name.clone(),
                    kind: t.kind.clone(),
                    start_line,
                    end_line,
                    start_column,
                    end_column,
                    parent_name: parents[i].map(|p| raw[p].name.clone()),
                    parent_kind: parents[i].map(|p| raw[p].kind.clone()),
                    doc: t.doc.clone(),
                }
            })
            .collect();

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Language;

    pub(super) fn extract(
        pl: ProgrammingLanguage,
        language: Language,
        code: &str,
    ) -> Vec<ExtractedSymbol> {
        SymbolExtractor::for_language(pl, language)
            .unwrap()
            .expect("language under test must have a tags query")
            .extract(code, &CancellationToken::new())
            .unwrap()
    }

    fn has(symbols: &[ExtractedSymbol], name: &str, kind: &str) -> bool {
        symbols.iter().any(|s| s.name == name && s.kind == kind)
    }

    fn find<'a>(symbols: &'a [ExtractedSymbol], name: &str) -> &'a ExtractedSymbol {
        symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("definition {name} not found"))
    }

    #[test]
    fn rust_defs_and_parent() {
        let code = "pub fn greet() {\n    helper();\n}\n\
                    pub struct Config;\n\
                    impl Config {\n    pub fn load() { greet(); }\n}\n";
        let symbols = extract(
            ProgrammingLanguage::Rust,
            Language::new(tree_sitter_rust::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "greet", "function"));
        assert!(has(&symbols, "Config", "class"));
        assert!(has(&symbols, "load", "method"));

        let greet = find(&symbols, "greet");
        assert_eq!(greet.start_line, 1);
        assert_eq!(greet.end_line, 3);

        // Rust's `impl` block is tagged as a *reference* (reference.implementation),
        // so it is not extracted and cannot be a parent — and `pub struct Config;`
        // spans only its own line, so it does not contain `load` either. A Rust
        // method therefore has NO enclosing definition here. That was already true
        // before references were dropped (a reference was never a parent candidate),
        // so this is a property of the upstream tags query, not of the trim.
        let load = find(&symbols, "load");
        assert_eq!(load.parent_name, None);
    }

    #[test]
    fn python_defs_and_refs() {
        let code = "class Greeter:\n    def greet(self):\n        helper()\n";
        let symbols = extract(
            ProgrammingLanguage::Python,
            Language::new(tree_sitter_python::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "Greeter", "class"));
        assert!(has(&symbols, "greet", "function"));
        let greet = find(&symbols, "greet");
        assert_eq!(greet.parent_name.as_deref(), Some("Greeter"));
    }

    #[test]
    fn javascript_defs_and_refs() {
        let code = "class Greeter {\n  greet() { helper(); }\n}\nfunction helper() {}\n";
        let symbols = extract(
            ProgrammingLanguage::JavaScript,
            Language::new(tree_sitter_javascript::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "Greeter", "class"));
        assert!(has(&symbols, "greet", "method"));
        assert!(has(&symbols, "helper", "function"));
    }

    #[test]
    fn typescript_defs() {
        let code = "interface Shape { area(): number; }\n\
                    function compute(s: Shape): number { return s.area(); }\n";
        let symbols = extract(
            ProgrammingLanguage::TypeScript,
            Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
            code,
        );
        assert!(has(&symbols, "Shape", "interface"));
        assert!(has(&symbols, "compute", "function"));
    }

    #[test]
    fn tsx_defs() {
        let code = "function Widget(): number {\n  return render();\n}\n";
        let symbols = extract(
            ProgrammingLanguage::Tsx,
            Language::new(tree_sitter_typescript::LANGUAGE_TSX),
            code,
        );
        assert!(has(&symbols, "Widget", "function"));
    }

    #[test]
    fn go_defs_refs_and_doc() {
        let code = "package main\n\n// greet says hello.\nfunc greet() {\n\thelper()\n}\n";
        let symbols = extract(
            ProgrammingLanguage::Go,
            Language::new(tree_sitter_go::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "greet", "function"));
        // Go's upstream query captures the preceding comment as @doc (rust's has
        // no @doc captures, so the doc assertion lives here).
        let greet = find(&symbols, "greet");
        assert_eq!(greet.doc.as_deref(), Some("greet says hello."));
    }

    #[test]
    fn c_defs() {
        let code = "struct config { int x; };\nint greet(void) { return 0; }\n";
        let symbols = extract(
            ProgrammingLanguage::C,
            Language::new(tree_sitter_c::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "greet", "function"));
        assert!(has(&symbols, "config", "class"));
    }

    #[test]
    fn cpp_defs() {
        let code = "class Greeter {\npublic:\n  void greet();\n};\nvoid Greeter::greet() {}\n";
        let symbols = extract(
            ProgrammingLanguage::Cpp,
            Language::new(tree_sitter_cpp::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "Greeter", "class"));
        assert!(symbols.iter().any(|s| s.name == "greet"));
    }

    #[test]
    fn java_defs_and_refs() {
        let code = "class Greeter implements Speaker {\n  void greet() { helper(); }\n}\n";
        let symbols = extract(
            ProgrammingLanguage::Java,
            Language::new(tree_sitter_java::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "Greeter", "class"));
        assert!(has(&symbols, "greet", "method"));
    }

    #[test]
    fn csharp_defs() {
        let code = "class Greeter {\n  void Greet() {}\n}\n";
        let symbols = extract(
            ProgrammingLanguage::CSharp,
            Language::new(tree_sitter_c_sharp::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "Greeter", "class"));
        assert!(has(&symbols, "Greet", "method"));
    }

    #[test]
    fn ruby_defs_and_refs() {
        let code = "class Greeter\n  def greet\n    helper\n  end\nend\n";
        let symbols = extract(
            ProgrammingLanguage::Ruby,
            Language::new(tree_sitter_ruby::LANGUAGE),
            code,
        );
        assert!(has(&symbols, "Greeter", "class"));
        assert!(has(&symbols, "greet", "method"));
    }

    #[test]
    fn php_defs_and_refs() {
        // Upstream tags.scm captures scoped/member calls but not a bare `helper()`
        // (its function_call_expression arm only matches qualified/variable callees).
        let code = "<?php\nclass Greeter {\n  function greet() { $this->helper(); }\n}\n";
        let symbols = extract(
            ProgrammingLanguage::Php,
            Language::new(tree_sitter_php::LANGUAGE_PHP),
            code,
        );
        assert!(has(&symbols, "Greeter", "class"));
        assert!(has(&symbols, "greet", "function"));
    }

    #[test]
    fn ocaml_defs() {
        let code = "let greet () = helper ()\n";
        let symbols = extract(
            ProgrammingLanguage::Ocaml,
            Language::new(tree_sitter_ocaml::LANGUAGE_OCAML),
            code,
        );
        assert!(symbols.iter().any(|s| s.name == "greet"));
    }

    #[test]
    fn scala_defs_via_vendored_query() {
        let code = "object Greeter {\n  def greet(): Unit = helper()\n}\n";
        let symbols = extract(
            ProgrammingLanguage::Scala,
            Language::new(tree_sitter_scala::LANGUAGE),
            code,
        );
        assert!(symbols.iter().any(|s| s.name == "Greeter"));
        assert!(has(&symbols, "greet", "function"));
    }

    #[test]
    fn languages_without_tags_yield_no_extractor() {
        for (pl, language) in [
            (
                ProgrammingLanguage::Bash,
                Language::new(tree_sitter_bash::LANGUAGE),
            ),
            (
                ProgrammingLanguage::Json,
                Language::new(tree_sitter_json::LANGUAGE),
            ),
        ] {
            assert!(
                SymbolExtractor::for_language(pl, language)
                    .unwrap()
                    .is_none(),
                "{pl:?} has no tags query and must yield no extractor"
            );
        }
    }

    #[test]
    fn every_language_constructs_or_declines() {
        // Regression guard for the queries themselves: every enum variant either
        // has no query or its query compiles against its grammar (a bumped crate
        // with an incompatible tags.scm fails here, not at runtime).
        for &pl in ProgrammingLanguage::ALL {
            let language = crate::backend::v0::handlers::tree_sitter_language(pl);
            SymbolExtractor::for_language(pl, language)
                .unwrap_or_else(|e| panic!("{pl:?}: tags query failed to compile: {e}"));
        }
    }

    #[test]
    fn cancelled_token_errors() {
        let token = CancellationToken::new();
        token.cancel();
        let mut ex = SymbolExtractor::for_language(
            ProgrammingLanguage::Rust,
            Language::new(tree_sitter_rust::LANGUAGE),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            ex.extract("fn main() { helper(); }", &token),
            Err(SymbolError::Cancelled)
        ));
    }

    #[test]
    fn nested_defs_get_nearest_parent() {
        let code = "mod outer {\n\
                    \x20   pub fn middle() {\n\
                    \x20       inner();\n\
                    \x20   }\n\
                    }\n";
        let symbols = extract(
            ProgrammingLanguage::Rust,
            Language::new(tree_sitter_rust::LANGUAGE),
            code,
        );
        let middle = find(&symbols, "middle");
        assert_eq!(
            middle.parent_name.as_deref(),
            Some("outer"),
            "nearest enclosing definition wins, not the outermost"
        );
        assert!(
            !symbols.iter().any(|s| s.name == "inner"),
            "`inner()` is a call, and calls are no longer extracted"
        );
    }

    #[test]
    fn spans_are_one_indexed_and_consistent() {
        let code = "fn alpha() {}\nfn beta() {\n    alpha();\n}\n";
        let symbols = extract(
            ProgrammingLanguage::Rust,
            Language::new(tree_sitter_rust::LANGUAGE),
            code,
        );
        let alpha = find(&symbols, "alpha");
        assert_eq!((alpha.start_line, alpha.start_column), (1, 0));
        assert_eq!(alpha.end_line, 1);
        assert_eq!(
            symbols.iter().filter(|s| s.name == "alpha").count(),
            1,
            "the call to alpha() on line 3 must not appear as a second row"
        );
    }

    #[test]
    fn mindex_own_sources_yield_expected_symbols() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/db/qdrant.rs"))
            .unwrap();
        let symbols = extract(
            ProgrammingLanguage::Rust,
            Language::new(tree_sitter_rust::LANGUAGE),
            &src,
        );
        assert!(
            symbols.iter().any(|s| s.name == "collection_for"),
            "collection_for must be tagged as a definition in qdrant.rs"
        );
        // The regression this file exists to prevent from coming back. `qdrant.rs`
        // calls `collection_for` many times over; before the trim each call was a
        // stored row, and those rows were 87.5% of the table.
        assert!(
            symbols.iter().all(|s| s.kind != "call"),
            "call sites must not be extracted — see `ExtractedSymbol`"
        );
    }
}
