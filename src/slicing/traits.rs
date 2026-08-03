use thiserror::Error;
use tokenizers::Tokenizer;
use tokio_util::sync::CancellationToken;
use tree_sitter::{Language, LanguageError, Node, Parser};

/// Version of everything that turns a file into `project_file_chunks` rows: the
/// AST walk and node-selection logic below, the left-extension rule, and the
/// tokenizer the token window is measured with.
///
/// **Bump this whenever a change would produce different chunk boundaries for the
/// same source text.** Unlike [`SYMBOLS_DERIVATION_VERSION`](crate::slicing::symbols::SYMBOLS_DERIVATION_VERSION),
/// a bump here is expensive: every affected file is re-sliced, re-embedded on the
/// GPU, and re-upserted to Qdrant. The `[slicer]` token window is *not* covered —
/// it is config, and changing it is the operator's call, not a code change.
///
/// Stored per file in `project_files.chunks_version` and compared by the
/// prepare-phase skip, so a bump self-heals on the next ordinary indexer run.
///
/// # The notation, which every internal version in mindex shares
///
/// `MAJOR.MINOR`, as a string. MINOR moves when the *way* derived data is
/// produced changes; MAJOR when its *shape* does, so that old rows cannot be read
/// rather than merely recomputed. Both are compared by plain equality and both
/// therefore trigger the same rebuild — the split is for whoever reads the release
/// notes, not for the code, and pretending otherwise would be a lie the compiler
/// cannot catch. The siblings are [`SYMBOLS_DERIVATION_VERSION`](crate::slicing::symbols::SYMBOLS_DERIVATION_VERSION),
/// [`PROMPT_VERSION`](crate::research::PROMPT_VERSION) and the collection-name
/// suffix `COLLECTION_SCHEMA_VERSION`, which is a single token because it is a
/// name component rather than a compared value.
pub const CHUNKS_DERIVATION_VERSION: &str = "1.0";

/// Node kinds a chunk absorbs when extending leftward.
///
/// Matched as substrings so this needs no per-language table — the codebase's
/// stated constraint. Attributes are here with comments because in most
/// languages they sit *between* the doc comment and the item it documents
/// (`#[derive(...)]`, `@decorator`, `@Override`), so a walk that stops at them
/// never reaches the prose: skipping them was measured to take doc-comment
/// coverage from 30% to 40%, where stopping took it from 28% to 30%.
const ABSORBED_KINDS: &[&str] = &["comment", "attribute", "decorator", "annotation"];

/// Smallest gap worth its own chunk. Below this a chunk is a fragment — a
/// closing brace, one `use` line — that costs a vector and answers nothing.
const GAP_MIN_TOKENS: usize = 24;

/// Two tokens for the `[CLS]`/`[SEP]` pair the embedder adds around every text.
/// They are not in the tokenizer offsets a slicer measures with (which are taken
/// with `add_special_tokens = false`) but they *are* in the ColBERT output, so a
/// chunk cut to exactly the ceiling below comes back two rows over it.
const SPECIAL_TOKENS: usize = 2;

/// Tokens whose ColBERT rows still fit in one Qdrant point: a multivector may
/// hold at most 1 048 576 elements, ColBERT emits one [`VECTOR_DIM`]-element row
/// per token, and [`SPECIAL_TOKENS`] of those rows are not the chunk's own.
///
/// **Structural, not configurable** — like [`VECTOR_DIM`] itself. A chunk above
/// it is not a coarse chunk, it is one Qdrant refuses, and the refusal fails the
/// whole upsert batch rather than the offending file.
pub(crate) const STORABLE_TOKENS_CEILING: usize =
    (1_048_576 / crate::db::qdrant::VECTOR_DIM as usize) - SPECIAL_TOKENS;

/// What a slicer may aim for, which is not the ceiling itself.
///
/// Both slicers measure spans against *whole-file* token offsets, while the
/// embedder re-tokenizes each chunk on its own — and tokenization is
/// context-dependent, so the two disagree by an edge token or two (the same fact
/// `WINDOW_SLACK` exists for in the window test). Aiming at the ceiling
/// therefore lands just over it, measured: a cut of 1022 came back 1023.
const RETOKENIZATION_SLACK: usize = 2;

/// Hard ceiling both slicers clamp their configured window to, rather than
/// trusting config validation to have caught it: `[slicer].max_doc_chunk_tokens`
/// defaults to exactly 1024, which is already over
/// [`STORABLE_TOKENS_CEILING`] — so a validation rule would refuse a
/// configuration the operator never chose. The code window (512) is far below it
/// and clamping never reaches it.
pub(crate) const MAX_STORABLE_TOKENS: usize = STORABLE_TOKENS_CEILING - RETOKENIZATION_SLACK;

/// The single tokenizer capability the slicer needs: the byte-offset span of each
/// token in `text`. Abstracted behind a trait so the AST-walk/selection logic can
/// be tested with a cheap deterministic tokenizer instead of downloading the real
/// BGE-M3 tokenizer. The production implementation is `tokenizers::Tokenizer`.
pub trait Tokenizing {
    fn token_offsets(&self, text: &str) -> Result<Vec<(usize, usize)>, SlicerError>;
}

impl Tokenizing for Tokenizer {
    fn token_offsets(&self, text: &str) -> Result<Vec<(usize, usize)>, SlicerError> {
        Ok(self.encode(text, false)?.get_offsets().to_vec())
    }
}

pub struct Slicer<'a> {
    pub tokenizer: &'a dyn Tokenizing,
    pub parser: Parser,
    /// Inclusive token window a node must fall in to be selected (from `[slicer]`
    /// config). BGE-M3 performs best in this range; the window is measured, not
    /// computed, because tokenization is context-dependent.
    min_tokens: usize,
    max_tokens: usize,
    /// Whether to emit chunks for the lines the AST walk selected nothing for.
    /// Roughly doubles the chunk count, and is what makes the corpus complete.
    fill_gaps: bool,
}

#[derive(Error, Debug)]
pub enum SlicerError {
    #[error("{0}")]
    Tokenizer(#[from] tokenizers::Error),

    #[error("{0}")]
    Language(#[from] LanguageError),

    #[error("Tree-sitter parse failed.")]
    Parse,

    #[error("Cancelled.")]
    Cancelled,
}

#[derive(Debug)]
pub struct SlicedChunk {
    /// Source text of `start_byte..end_byte`: the selected node, extended left
    /// over its indentation and any doc comment or attribute above it.
    pub code: String,
    // Only read by this module's own unit tests (to verify `code` lines up byte-for-byte
    // with the source); production code never persists these, so cfg-gate them out of
    // non-test builds rather than carry a permanent dead_code warning.
    #[cfg(test)]
    pub start_byte: usize,
    #[cfg(test)]
    pub end_byte: usize,
    /// Whether this chunk came from gap filling rather than from a selected AST
    /// node. Only the latter is governed by the token window, so the window test
    /// needs to tell them apart.
    #[cfg(test)]
    pub from_gap: bool,
    pub start_line: usize,   // 1-indexed
    pub end_line: usize,     // 1-indexed
    pub start_column: usize, // byte offset of the node within its start line
    pub end_column: usize,   // byte offset of the exclusive end within its end line
}

impl<'a> Slicer<'a> {
    pub fn new(
        language: Language,
        tokenizer: &'a dyn Tokenizing,
        min_tokens: usize,
        max_tokens: usize,
        fill_gaps: bool,
    ) -> Result<Self, SlicerError> {
        let mut parser = Parser::new();

        parser.set_language(&language)?;

        Ok(Self {
            parser,
            tokenizer,
            min_tokens,
            max_tokens: max_tokens.min(MAX_STORABLE_TOKENS),
            fill_gaps,
        })
    }

    pub fn parse(
        &mut self,
        code: &str,
        token: CancellationToken,
    ) -> Result<Vec<SlicedChunk>, SlicerError> {
        let offsets = self.tokenizer.token_offsets(code)?;

        /* Important: the tokenization is statistical. Token boundaries do not
        necessarily align with AST node boundaries. Furthermore,
        tokenization is context-dependent: the tokens for "x + y" are not
        simply the union of tokens for "x", "+", and "y",
        i.e. "tokenize(x + y) != tokenize(x) + tokenize(y)".
        */

        let mut res: Vec<SlicedChunk> = Vec::new();
        // Byte ranges the walk actually emitted, so the gap pass below can find
        // the lines it left behind.
        let mut spans: Vec<(usize, usize)> = Vec::new();
        // Highest byte any chunk has reached. Absorption must not reach back
        // past it — see `absorb_preceding`.
        let mut emitted_end = 0usize;

        let tree = self.parser.parse(code, None).ok_or(SlicerError::Parse)?;
        let mut cursor = tree.walk();
        if !cursor.goto_first_child() {
            return Ok(Vec::new());
        }

        'l: loop {
            if token.is_cancelled() {
                return Err(SlicerError::Cancelled);
            }

            let node = cursor.node();
            let mut descend = true;

            if node.is_named() {
                let start_token = offsets.partition_point(|&(s, _)| s < node.start_byte());
                let end_token = offsets.partition_point(|&(_, e)| e < node.end_byte());
                if start_token < end_token {
                    let len = end_token - start_token;

                    /* In practice, BGE-M3 models perform best with input sequences
                     * within this length range to balance context and semantic density.
                     */
                    if (self.min_tokens..=self.max_tokens).contains(&len) {
                        let line_start = line_start_of(code, node.start_byte());
                        // Only extend to line_start when the intervening bytes are pure
                        // whitespace (indentation).  Mid-line nodes (e.g. a block body
                        // that begins after `) -> T {`) must not pull in non-whitespace.
                        let is_pure_indent = code[line_start..node.start_byte()]
                            .bytes()
                            .all(|b| b == b' ' || b == b'\t');
                        let mut code_start = if is_pure_indent {
                            line_start
                        } else {
                            node.start_byte()
                        };
                        // Then keep going left over the doc comment that explains
                        // this item. It is a *preceding sibling*, never a child, so
                        // without this the reasoning is dropped from the chunk —
                        // which is why a plain-English question used to retrieve the
                        // test that names a behaviour instead of the code that
                        // implements it. Only from a line-start position, so a
                        // mid-line node cannot swallow code to its left.
                        if is_pure_indent {
                            code_start = self.absorb_preceding(
                                code,
                                &offsets,
                                node,
                                end_token,
                                code_start,
                                emitted_end,
                            );
                        }
                        emitted_end = emitted_end.max(node.end_byte());
                        res.push(new_chunk(code, code_start, node.end_byte(), false));
                        spans.push((code_start, node.end_byte()));
                        // Do not descend: children would produce overlapping chunks.
                        descend = false;
                    } else if len < self.min_tokens {
                        // Children are strictly smaller; no qualifying node below.
                        descend = false;
                    }
                    // len > max_tokens: keep descending to find qualifying sub-nodes.
                } else {
                    descend = false;
                }
            }

            if descend && cursor.goto_first_child() {
                continue 'l;
            }

            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'l;
                }
            }
        }

        if self.fill_gaps {
            self.pack_gaps(code, &offsets, &mut spans, &mut res);
            res.sort_by_key(|c| c.start_line);
        }
        Ok(res)
    }

    /// Extends `code_start` left over the contiguous comment/attribute block
    /// above `node`, returning the new start.
    ///
    /// Stops at the first sibling that is neither, at a blank line (one blank
    /// line means the comment is not attached to this item), at a sibling that
    /// does not start its own line, and before the chunk would exceed
    /// `max_tokens` — so absorbing prose never breaks the window.
    ///
    /// It also stops at `emitted_end`, the furthest byte already covered by a
    /// chunk. An absorbed node can be one that was *itself* selected: a big
    /// `#[utoipa::path(...)]` attribute clears `min_tokens` on its own, becomes
    /// a chunk, and is then also the preceding sibling of the function below it.
    /// Without this bound both chunks contain it, which breaks the
    /// non-overlapping invariant and pays to embed the same bytes twice.
    fn absorb_preceding(
        &self,
        code: &str,
        offsets: &[(usize, usize)],
        node: Node,
        end_token: usize,
        mut code_start: usize,
        emitted_end: usize,
    ) -> usize {
        let mut probe = node;
        while let Some(prev) = probe.prev_sibling() {
            let kind = prev.kind();
            if !ABSORBED_KINDS.iter().any(|k| kind.contains(k)) {
                break;
            }
            let between = &code[prev.end_byte()..probe.start_byte()];
            if !between.bytes().all(|b| b.is_ascii_whitespace())
                || between.bytes().filter(|&b| b == b'\n').count() > 1
            {
                break;
            }
            let candidate = line_start_of(code, prev.start_byte());
            if candidate < emitted_end {
                break;
            }
            if !code[candidate..prev.start_byte()]
                .bytes()
                .all(|b| b == b' ' || b == b'\t')
            {
                break;
            }
            if end_token.saturating_sub(offsets.partition_point(|&(s, _)| s < candidate))
                > self.max_tokens
            {
                break;
            }
            code_start = candidate;
            probe = prev;
        }
        code_start
    }

    /// Emits chunks for the lines the AST walk covered with nothing.
    ///
    /// Two whole classes of line are otherwise absent from the index: anything
    /// inside a node *below* `min_tokens` (consts, type aliases, small helpers,
    /// trait signatures — and the doc comments attached to them, which no amount
    /// of left-extension reaches because there is no chunk to extend), and
    /// everything between the selected children of an oversized node. Measured
    /// on this repository that is 48% of all source lines and 60% of doc
    /// comments; filling it took line coverage to 97% and doc-comment coverage
    /// to 100%.
    fn pack_gaps(
        &self,
        code: &str,
        offsets: &[(usize, usize)],
        spans: &mut [(usize, usize)],
        res: &mut Vec<SlicedChunk>,
    ) {
        // Left-extension can start a chunk before the previous one ended, so the
        // emitted spans are not sorted and may overlap. Both must be handled
        // before differencing, or the subtraction yields an inverted range.
        spans.sort_unstable();

        let mut gaps: Vec<(usize, usize)> = Vec::new();
        let mut cursor = 0usize;
        for &(start, end) in spans.iter() {
            let gap_end = line_start_of(code, start);
            if gap_end > cursor {
                gaps.push((cursor, gap_end));
            }
            cursor = cursor.max(next_line_start(code, end));
        }
        if cursor < code.len() {
            gaps.push((cursor, code.len()));
        }

        for (start, end) in gaps {
            for (window_start, window_end) in self.pack_window(code, offsets, start, end) {
                // Whitespace carries no meaning and still costs a vector.
                if code[window_start..window_end].trim().is_empty() {
                    continue;
                }
                res.push(new_chunk(code, window_start, window_end, true));
            }
        }
    }

    /// Splits one gap into windows of at most `max_tokens`, preferring to break
    /// at a blank line so a chunk does not begin mid-sentence inside a comment.
    fn pack_window(
        &self,
        code: &str,
        offsets: &[(usize, usize)],
        start: usize,
        end: usize,
    ) -> Vec<(usize, usize)> {
        let tokens_between = |a: usize, b: usize| -> usize {
            offsets
                .partition_point(|&(_, e)| e < b)
                .saturating_sub(offsets.partition_point(|&(s, _)| s < a))
        };
        if start >= end || tokens_between(start, end) < GAP_MIN_TOKENS {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut window_start = start;
        let mut last_fit = start;
        let mut last_blank: Option<usize> = None;

        for line_end in line_starts(code, start, end) {
            if tokens_between(window_start, line_end) > self.max_tokens {
                if last_fit > window_start {
                    // Prefer the last paragraph break inside the window, so a chunk
                    // does not begin mid-sentence inside a comment — but only if
                    // cutting there still leaves a chunk worth having. A blank line
                    // immediately after `window_start` would otherwise emit a chunk
                    // that is one newline.
                    let cut = last_blank
                        .filter(|b| tokens_between(window_start, *b) >= GAP_MIN_TOKENS)
                        .unwrap_or(last_fit);
                    out.push((window_start, cut));
                    window_start = cut;
                    last_blank = None;
                }
                // What is left is a single line, and a line is not bounded by
                // anything: a minified file is one line for its whole length,
                // so cutting only at line boundaries left the window
                // unenforced exactly where it matters most. Cut on token
                // boundaries instead. An over-window chunk is not merely
                // coarse, it is unusable downstream — see
                // `STORABLE_TOKENS_CEILING`, and note that both the Qdrant
                // refusal and the embedder's out-of-memory fail the whole batch
                // the chunk travelled in, not just its own file.
                while tokens_between(window_start, line_end) > self.max_tokens {
                    let Some(cut) =
                        token_boundary(code, offsets, window_start, line_end, self.max_tokens)
                    else {
                        break;
                    };
                    out.push((window_start, cut));
                    window_start = cut;
                    last_blank = None;
                }
            }
            if code[last_fit..line_end].trim().is_empty() {
                last_blank = Some(line_end);
            }
            last_fit = line_end;
        }

        if tokens_between(window_start, end) >= GAP_MIN_TOKENS {
            out.push((window_start, end));
        } else if let Some(last) = out.last_mut() {
            // A tail too small to stand alone joins the previous window rather
            // than being dropped — dropping it would silently lose coverage.
            if tokens_between(last.0, end) <= self.max_tokens {
                last.1 = end;
            }
        }
        out
    }
}

/// Byte offset of the start of the line containing `at`.
fn line_start_of(code: &str, at: usize) -> usize {
    code[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// Byte offset of the start of the first line after `at`.
fn next_line_start(code: &str, at: usize) -> usize {
    code[at..]
        .find('\n')
        .map_or(code.len(), |i| at + i + 1)
        .min(code.len())
}

/// Byte offset that closes the `max_tokens`-th token at or after `from`, or
/// `None` when `from..upto` needs no cut — fewer tokens remain, or the cut would
/// make no progress.
///
/// The last resort of both slicers, for the case a structural boundary cannot
/// answer: a line (or a documentation block) longer on its own than the window
/// it must fit. Cutting mid-token would be the same defect one byte smaller, so
/// the cut lands on a boundary the tokenizer itself reported, ceiled to a `char`
/// boundary because `code[..cut]` is sliced afterwards and a fake tokenizer in
/// tests is under no obligation to respect UTF-8.
pub(crate) fn token_boundary(
    code: &str,
    offsets: &[(usize, usize)],
    from: usize,
    upto: usize,
    max_tokens: usize,
) -> Option<usize> {
    let first = offsets.partition_point(|&(s, _)| s < from);
    let last = first + max_tokens.max(1) - 1;
    let cut = char_boundary(code, offsets.get(last)?.1);
    (from < cut && cut < upto).then_some(cut)
}

/// `at`, moved forward to the next `char` boundary of `code`.
fn char_boundary(code: &str, at: usize) -> usize {
    let mut at = at.min(code.len());
    while at < code.len() && !code.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The start of every line after the first within `start..end`, then `end`.
fn line_starts(code: &str, start: usize, end: usize) -> Vec<usize> {
    let mut out: Vec<usize> = code[start..end]
        .match_indices('\n')
        .map(|(i, _)| start + i + 1)
        .collect();
    out.push(end);
    out
}

/// Builds a chunk for `code[start..end]`, with every position derived from that
/// range so the text and the span it reports can never disagree.
fn new_chunk(code: &str, start: usize, end: usize, #[allow(unused)] from_gap: bool) -> SlicedChunk {
    let text = &code[start..end];
    SlicedChunk {
        code: text.into(),
        #[cfg(test)]
        start_byte: start,
        #[cfg(test)]
        end_byte: end,
        #[cfg(test)]
        from_gap,
        start_line: code[..start].bytes().filter(|&b| b == b'\n').count() + 1,
        end_line: code[..end].bytes().filter(|&b| b == b'\n').count() + 1,
        start_column: start - line_start_of(code, start),
        end_column: end - line_start_of(code, end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use tokenizers::Tokenizer;
    use tree_sitter::Language;

    static TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();

    fn tokenizer() -> &'static Tokenizer {
        TOKENIZER.get_or_init(|| Tokenizer::from_pretrained("BAAI/bge-m3", None).unwrap())
    }

    /// The production shape: gap filling on.
    fn slicer() -> Slicer<'static> {
        Slicer::new(
            Language::new(tree_sitter_rust::LANGUAGE),
            tokenizer(),
            128,
            512,
            true,
        )
        .unwrap()
    }

    /// Node selection alone, for the tests that are about the AST walk.
    fn ast_slicer() -> Slicer<'static> {
        Slicer::new(
            Language::new(tree_sitter_rust::LANGUAGE),
            tokenizer(),
            128,
            512,
            false,
        )
        .unwrap()
    }

    fn all_source_files() -> Vec<(String, String)> {
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        collect_rs(&src_root, &src_root, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn collect_rs(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_rs(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let name = path.strip_prefix(root).unwrap().display().to_string();
                let code = std::fs::read_to_string(&path).unwrap();
                out.push((name, code));
            }
        }
    }

    /// One token per byte: a node of B bytes counts as B tokens. Lets the
    /// selection logic be exercised with no real tokenizer (no HF download).
    struct OnePerByte;
    impl Tokenizing for OnePerByte {
        fn token_offsets(&self, text: &str) -> Result<Vec<(usize, usize)>, SlicerError> {
            Ok((0..text.len()).map(|i| (i, i + 1)).collect())
        }
    }

    #[test]
    fn slices_with_a_fake_tokenizer() {
        // ~280-byte fn body → in the 128–512 window under one-token-per-byte, so the
        // function node is selected — demonstrates A3's seam without the real tokenizer.
        let src = "fn demo() {\n".to_string()
            + &"    let _ = compute_something_meaningful(1, 2, 3);\n".repeat(5)
            + "}\n";
        let mut slicer = Slicer::new(
            Language::new(tree_sitter_rust::LANGUAGE),
            &OnePerByte,
            128,
            512,
            false,
        )
        .unwrap();
        let chunks = slicer.parse(&src, CancellationToken::new()).unwrap();
        assert!(!chunks.is_empty(), "the fn node should have been selected");
    }

    /// A minified file is one line for its whole length, so the gap pass has no
    /// line boundary to cut at and used to emit the file as a single chunk of
    /// any size. Downstream that is not a coarse chunk but an unstorable one:
    /// Qdrant refuses a multivector point above 1 048 576 elements (1024 ColBERT
    /// tokens × 1024 dimensions) and the embedder runs out of GPU memory on a
    /// large one, either of which fails the whole batch, not just this file.
    #[test]
    fn one_line_longer_than_the_window_is_still_cut_to_it() {
        // One token per byte, so the assertion below reads in bytes.
        let src = format!("const D: &str = \"{}\";\n", "ab,".repeat(1200));
        let mut s = Slicer::new(
            Language::new(tree_sitter_rust::LANGUAGE),
            &OnePerByte,
            128,
            512,
            true,
        )
        .unwrap();
        let chunks = s.parse(&src, CancellationToken::new()).unwrap();
        assert!(
            chunks.len() > 5,
            "one line produced {} chunks",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.code.len() <= 512,
                "chunk at line {} has {} tokens (window is 512)",
                c.start_line,
                c.code.len()
            );
        }
        // The cuts tile the line rather than sampling it: consecutive chunks are
        // contiguous, so nothing between them is silently absent from the index.
        for pair in chunks.windows(2) {
            assert_eq!(
                pair[0].end_byte, pair[1].start_byte,
                "a gap opened between chunks at bytes {} and {}",
                pair[0].end_byte, pair[1].start_byte
            );
        }
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        let mut s = slicer();
        assert!(s.parse("", CancellationToken::new()).unwrap().is_empty());
    }

    #[test]
    fn cancelled_token_errors_immediately() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            slicer().parse("fn main() {}", token),
            Err(SlicerError::Cancelled)
        ));
    }

    #[test]
    fn mindex_sources_produce_at_least_one_chunk() {
        let total: usize = all_source_files()
            .iter()
            .map(|(_, src)| slicer().parse(src, CancellationToken::new()).unwrap().len())
            .sum();
        assert!(
            total > 0,
            "no chunks produced across all mindex source files"
        );
    }

    #[test]
    fn chunk_code_is_exactly_its_reported_byte_range() {
        for (name, src) in all_source_files() {
            for chunk in slicer().parse(&src, CancellationToken::new()).unwrap() {
                assert_eq!(
                    chunk.code,
                    src[chunk.start_byte..chunk.end_byte],
                    "{name}: chunk.code is not src[start_byte..end_byte] at line {}",
                    chunk.start_line
                );
            }
        }
    }

    /// A chunk keeps the indentation of the code it holds, so the retrieved text
    /// reads as it does in the file. Since a chunk now starts at a line start,
    /// that indentation is *inside* `code` rather than stripped off in front of
    /// it — which is what this checks.
    #[test]
    fn indentation_is_preserved_in_code() {
        let mut checked = 0usize;
        for (name, src) in all_source_files() {
            for chunk in slicer().parse(&src, CancellationToken::new()).unwrap() {
                let first_line = chunk.code.lines().next().unwrap_or("");
                let indent: String = first_line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                if indent.is_empty() {
                    continue;
                }
                let line_start = src[..chunk.start_byte].rfind('\n').map_or(0, |i| i + 1);
                assert_eq!(
                    chunk.start_byte, line_start,
                    "{name}: an indented chunk at line {} does not start at its line start",
                    chunk.start_line
                );
                assert!(
                    src[chunk.start_byte..].starts_with(&indent),
                    "{name}: indentation at line {} was not taken from the source",
                    chunk.start_line
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no indented chunks found in mindex source files — test is vacuous"
        );
    }

    #[test]
    fn line_numbers_consistent_with_byte_ranges() {
        for (name, src) in all_source_files() {
            for chunk in slicer().parse(&src, CancellationToken::new()).unwrap() {
                let before_start = &src[..chunk.start_byte];
                let expected_start_line = before_start.bytes().filter(|&b| b == b'\n').count() + 1;
                let line_start = before_start.rfind('\n').map_or(0, |i| i + 1);
                let expected_start_col = chunk.start_byte - line_start;

                assert_eq!(
                    chunk.start_line, expected_start_line,
                    "{name}: start_line mismatch at byte {}",
                    chunk.start_byte
                );
                assert_eq!(
                    chunk.start_column, expected_start_col,
                    "{name}: start_column mismatch at byte {}",
                    chunk.start_byte
                );

                let before_end = &src[..chunk.end_byte];
                let expected_end_line = before_end.bytes().filter(|&b| b == b'\n').count() + 1;
                let end_line_start = before_end.rfind('\n').map_or(0, |i| i + 1);
                let expected_end_col = chunk.end_byte - end_line_start;

                assert_eq!(
                    chunk.end_line, expected_end_line,
                    "{name}: end_line mismatch at byte {}",
                    chunk.end_byte
                );
                assert_eq!(
                    chunk.end_column, expected_end_col,
                    "{name}: end_column mismatch at byte {}",
                    chunk.end_byte
                );
            }
        }
    }

    // Artificial fixture: a module with an indented function large enough to hit the
    // 128-token threshold.  We verify that the selected chunk's code includes the 4-space
    // indentation that precedes the function keyword on its line.
    const INDENTED_FIXTURE: &str = r#"mod analytics {
    pub fn transform_records(
        records: &[(String, Vec<i64>)],
        config: &ProcessConfig,
        output: &mut Vec<TransformedRecord>,
    ) -> Result<Statistics, PipelineError> {
        let mut stats = Statistics::default();
        let batch_size = config.batch_size.unwrap_or(DEFAULT_BATCH);
        let max_retries = config.max_retries.unwrap_or(DEFAULT_RETRIES);
        for (batch_idx, batch) in records.chunks(batch_size).enumerate() {
            let mut attempt = 0usize;
            loop {
                match transform_batch(batch, config) {
                    Ok(transformed) => {
                        output.extend(transformed);
                        stats.processed += batch.len();
                        stats.batches += 1;
                        break;
                    }
                    Err(err) if attempt < max_retries => {
                        attempt += 1;
                        stats.retries += 1;
                        eprintln!("batch {} retry {}: {}", batch_idx, attempt, err);
                    }
                    Err(err) => {
                        return Err(PipelineError::BatchFailed {
                            batch_index: batch_idx,
                            source: err,
                        });
                    }
                }
            }
        }
        Ok(stats)
    }
}"#;

    #[test]
    fn artificial_indented_chunk_preserves_whitespace() {
        let src = INDENTED_FIXTURE;
        let chunks = slicer().parse(src, CancellationToken::new()).unwrap();
        assert!(
            !chunks.is_empty(),
            "INDENTED_FIXTURE produced no chunks; the fixture may need more content to reach 128 tokens"
        );
        for chunk in &chunks {
            // The node text must be present at the end of chunk.code.
            let node_text = &src[chunk.start_byte..chunk.end_byte];
            assert!(
                chunk.code.ends_with(node_text),
                "chunk.code should end with the node's raw text"
            );
            // Any prefix before the node text must be pure indentation whitespace.
            let prefix = &chunk.code[..chunk.code.len() - node_text.len()];
            assert!(
                prefix.bytes().all(|b| b == b' ' || b == b'\t'),
                "code prefix {prefix:?} before node text is not pure whitespace"
            );
            assert!(chunk.start_line >= 1, "start_line must be at least 1");
        }
    }

    /// The slicer counts a node's tokens by *whole-file* offsets; this re-encodes
    /// the chunk on its own. Those are not the same measurement — tokenization is
    /// context-dependent, so a token that straddles a chunk edge splits differently
    /// once the surrounding text is gone, and a node the slicer measured at exactly
    /// 512 can re-encode at 513. (Measured: `src/research.rs` produced one.) The
    /// slack is what keeps this a guard on the window rather than a tripwire on
    /// whichever source file happens to land on a boundary — the guard is that no
    /// chunk is a fragment or a whole file, and two tokens do not change that.
    const WINDOW_SLACK: usize = 2;

    #[test]
    fn chunks_satisfy_token_window() {
        let t = tokenizer();
        let (min, max) = (128 - WINDOW_SLACK, 512 + WINDOW_SLACK);
        for (name, src) in all_source_files() {
            for chunk in slicer().parse(&src, CancellationToken::new()).unwrap() {
                let n = t.encode(chunk.code.as_str(), false).unwrap().len();
                assert!(n <= max, "{name}: chunk has {n} tokens (maximum is {max})");
                // The window governs *node selection*. A gap chunk is what is
                // left over between selected nodes, so it has no lower bound
                // beyond being large enough to be worth a vector — holding it to
                // 128 would mean discarding the lines again.
                let floor = if chunk.from_gap {
                    GAP_MIN_TOKENS - WINDOW_SLACK
                } else {
                    min
                };
                assert!(
                    n >= floor,
                    "{name}: chunk at line {} has {n} tokens (minimum is {floor}, from_gap = {})",
                    chunk.start_line,
                    chunk.from_gap
                );
            }
        }
    }

    /// The point of the whole gap pass: the corpus must be nearly complete.
    /// Before it, 48% of lines were in no chunk at all — including 72% of the
    /// doc comments, which is where this codebase keeps its reasoning.
    #[test]
    fn gap_filling_covers_nearly_every_line() {
        let (mut covered, mut total) = (0usize, 0usize);
        for (_, src) in all_source_files() {
            let chunks = slicer().parse(&src, CancellationToken::new()).unwrap();
            total += src.lines().filter(|l| !l.trim().is_empty()).count();
            covered += chunks
                .iter()
                .map(|c| c.end_line - c.start_line + 1)
                .sum::<usize>();
        }
        assert!(
            covered * 100 >= total * 90,
            "only {covered} lines covered of {total} non-blank — gap filling regressed"
        );
    }

    /// Without gap filling the same corpus is roughly half indexed. This is the
    /// control for the test above: it pins the defect being fixed, so the
    /// coverage number cannot be mistaken for something the AST walk already did.
    #[test]
    fn the_ast_walk_alone_leaves_most_lines_uncovered() {
        let (mut covered, mut total) = (0usize, 0usize);
        for (_, src) in all_source_files() {
            let chunks = ast_slicer().parse(&src, CancellationToken::new()).unwrap();
            total += src.lines().filter(|l| !l.trim().is_empty()).count();
            covered += chunks
                .iter()
                .map(|c| c.end_line - c.start_line + 1)
                .sum::<usize>();
        }
        assert!(
            covered * 100 < total * 80,
            "node selection alone covered {covered} of {total} lines; if the walk now \
             covers the file on its own, gap filling has stopped being the thing that does it"
        );
    }

    /// A doc comment is a *preceding sibling* of the item it documents, so
    /// without left-extension the explanation never enters the chunk. This is
    /// the defect that made a plain-English query retrieve the test that names a
    /// behaviour rather than the code that implements it.
    #[test]
    fn a_chunk_absorbs_the_doc_comment_above_it() {
        let src = format!(
            "/// The answering sentence lives here and nowhere else.\n\
             #[derive(Debug)]\n\
             pub struct Demo {{\n{}}}\n",
            "    pub field_with_a_long_enough_name: std::collections::HashMap<String, Vec<u8>>,\n"
                .repeat(6)
        );
        let chunks = ast_slicer().parse(&src, CancellationToken::new()).unwrap();
        assert!(
            chunks
                .iter()
                .any(|c| c.code.contains("The answering sentence")
                    && c.code.contains("pub struct Demo")),
            "the doc comment and its struct should be one chunk, got {chunks:#?}"
        );
    }

    /// The attribute between a doc comment and its item is the reason the first
    /// attempt at this gained almost nothing: in Rust the immediately preceding
    /// sibling of a struct is `#[derive(...)]`, not the prose.
    #[test]
    fn absorption_reaches_past_an_attribute() {
        let src = format!(
            "/// Explanation above an attribute.\n\
             #[derive(Debug, Clone)]\n\
             pub struct Demo {{\n{}}}\n",
            "    pub field_with_a_long_enough_name: std::collections::HashMap<String, Vec<u8>>,\n"
                .repeat(6)
        );
        let chunks = ast_slicer().parse(&src, CancellationToken::new()).unwrap();
        assert!(
            chunks
                .iter()
                .any(|c| c.code.contains("Explanation above an attribute")),
            "absorption stopped at the attribute, got {chunks:#?}"
        );
    }

    /// A comment separated by a blank line documents nothing in particular, and
    /// absorbing it would attach unrelated prose to the chunk's vectors.
    #[test]
    fn absorption_stops_at_a_blank_line() {
        let src = format!(
            "// Unrelated note about the module.\n\n\
             pub struct Demo {{\n{}}}\n",
            "    pub field_with_a_long_enough_name: std::collections::HashMap<String, Vec<u8>>,\n"
                .repeat(6)
        );
        let chunks = ast_slicer().parse(&src, CancellationToken::new()).unwrap();
        assert!(
            !chunks.iter().any(|c| c.code.contains("Unrelated note")),
            "a detached comment must not be absorbed, got {chunks:#?}"
        );
    }

    #[test]
    fn chunks_do_not_overlap() {
        for (name, src) in all_source_files() {
            let chunks = slicer().parse(&src, CancellationToken::new()).unwrap();
            let mut ranges: Vec<(usize, usize)> =
                chunks.iter().map(|c| (c.start_byte, c.end_byte)).collect();
            ranges.sort_by_key(|&(start, _)| start);
            for w in ranges.windows(2) {
                let (_, prev_end) = w[0];
                let (next_start, _) = w[1];
                assert!(
                    next_start >= prev_end,
                    "{name}: overlapping chunks — prev ends at byte {prev_end}, next starts at {next_start}"
                );
            }
        }
    }
}
