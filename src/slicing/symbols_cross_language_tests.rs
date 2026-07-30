//! Cross-language guard for the one property the `callers` tool rests on.
//!
//! `callers` answers "who calls X" by reading `parent_name` off `role='reference'`
//! rows — the lexically enclosing definition, assigned in `extract` by pure byte-
//! span containment. That assignment contains no language knowledge, but whether
//! it *produces* anything depends on each language's upstream tags query: the
//! enclosing function has to be tagged as a definition whose span covers the call.
//! Nothing guarantees that, and a language where it does not hold makes `callers`
//! return an empty list that reads as "nothing calls this".
//!
//! That failure is silent, per-language and invisible in Rust — which is exactly
//! why it gets its own file. The table below is deliberately Rust-free: Rust is
//! covered by `rust_defs_refs_and_parent` next door, and the claim under test here
//! is that the mechanism is *not* Rust-specific.

use super::tests::{extract, find};
use super::*;
// Reused rather than re-mapped: the grammar per language is already a total match
// there, so this table cannot drift from the one the indexer actually parses with.
use crate::backend::v0::handlers::tree_sitter_language;

/// One language's minimal "a call sits inside a definition" fixture.
///
/// Every fixture spells the same shape and uses the same two names, so the
/// assertions are identical across languages and a failure names only the
/// language: a definition `outer`, containing a call to `target`.
struct ParentCase {
    pl: ProgrammingLanguage,
    code: &'static str,
}

const PARENT_CASES: &[ParentCase] = &[
    ParentCase {
        pl: ProgrammingLanguage::Python,
        code: "def outer():\n    target()\n",
    },
    ParentCase {
        pl: ProgrammingLanguage::TypeScript,
        code: "class C {\n  outer() {\n    target();\n  }\n}\n",
    },
    ParentCase {
        // Go files do not parse without a package clause.
        pl: ProgrammingLanguage::Go,
        code: "package main\n\nfunc outer() {\n\ttarget()\n}\n",
    },
    ParentCase {
        pl: ProgrammingLanguage::Java,
        code: "class C {\n  void outer() {\n    target();\n  }\n}\n",
    },
    ParentCase {
        // Parenthesised on purpose: a bare `target` is an identifier, not a call,
        // and would not be tagged as a reference at all.
        pl: ProgrammingLanguage::Ruby,
        code: "def outer\n  target()\nend\n",
    },
];

/// The languages that have a tags query but are **not** in `PARENT_CASES`.
///
/// Asserted exactly, so adding a language with a tags query fails here until
/// someone decides whether `callers` works for it — the same forcing function
/// `queries_for`'s total match provides for symbols themselves. Shrinking the
/// table also fails, which is the other half of the guard.
const UNCOVERED_WITH_TAGS: &[ProgrammingLanguage] = &[
    // Covered by `rust_defs_refs_and_parent`; excluded here by design.
    ProgrammingLanguage::Rust,
    // Covered by `javascript_defs_and_refs`; TypeScript stands in for the shared
    // query in the table above.
    ProgrammingLanguage::JavaScript,
    ProgrammingLanguage::Tsx,
    ProgrammingLanguage::C,
    ProgrammingLanguage::Cpp,
    ProgrammingLanguage::CSharp,
    ProgrammingLanguage::Php,
    ProgrammingLanguage::Ocaml,
    ProgrammingLanguage::Scala,
];

#[test]
fn a_call_inside_a_definition_carries_its_parent_in_every_covered_language() {
    for case in PARENT_CASES {
        let symbols = extract(case.pl, tree_sitter_language(case.pl), case.code);

        assert!(
            symbols
                .iter()
                .any(|s| s.name == "outer" && s.role == SymbolRole::Definition),
            "{:?}: `outer` must be tagged as a definition",
            case.pl
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name == "target" && s.role == SymbolRole::Reference),
            "{:?}: the call to `target` must be tagged as a reference",
            case.pl
        );

        // The property `callers` is built on.
        let call = find(&symbols, "target", SymbolRole::Reference);
        assert_eq!(
            call.parent_name.as_deref(),
            Some("outer"),
            "{:?}: a reference inside `outer` must carry it as parent_name, \
             or `callers` silently returns nothing for this language",
            case.pl
        );
    }
}

#[test]
fn the_language_table_covers_every_language_with_a_tags_query() {
    let covered: Vec<ProgrammingLanguage> = PARENT_CASES.iter().map(|c| c.pl).collect();

    assert!(
        covered.len() >= 5,
        "the cross-language table must keep at least 5 languages; a smaller one \
         stops being evidence that `callers` is language-agnostic"
    );
    for (i, pl) in covered.iter().enumerate() {
        assert!(
            !covered[..i].contains(pl),
            "{pl:?} appears twice in PARENT_CASES — duplicates inflate the count \
             without adding coverage"
        );
    }

    let mut uncovered: Vec<ProgrammingLanguage> = ProgrammingLanguage::ALL
        .iter()
        .copied()
        .filter(|&pl| queries_for(pl).is_some() && !covered.contains(&pl))
        .collect();
    let mut expected = UNCOVERED_WITH_TAGS.to_vec();

    uncovered.sort_by_key(|pl| format!("{pl:?}"));
    expected.sort_by_key(|pl| format!("{pl:?}"));
    assert_eq!(
        uncovered, expected,
        "a language with a tags query changed sides. Either add it to \
         PARENT_CASES with a fixture, or record it in UNCOVERED_WITH_TAGS with a \
         comment saying why `callers` is untested for it."
    );
}
