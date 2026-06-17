use thiserror::Error;
use tokenizers::Tokenizer;
use tokio_util::sync::CancellationToken;
use tree_sitter::{Language, LanguageError, Parser};

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
    /// Source text from the start of the node's line (includes leading whitespace) to end_byte.
    pub code: String,
    // Only read by this module's own unit tests (to verify `code` lines up byte-for-byte
    // with the source); production code never persists these, so cfg-gate them out of
    // non-test builds rather than carry a permanent dead_code warning.
    #[cfg(test)]
    pub start_byte: usize,
    #[cfg(test)]
    pub end_byte: usize,
    pub start_line: usize,   // 1-indexed
    pub end_line: usize,     // 1-indexed
    pub start_column: usize, // byte offset of the node within its start line
    pub end_column: usize,   // byte offset of the exclusive end within its end line
}

impl<'a> Slicer<'a> {
    pub fn new(language: Language, tokenizer: &'a dyn Tokenizing) -> Result<Self, SlicerError> {
        let mut parser = Parser::new();

        parser.set_language(&language)?;

        Ok(Self { parser, tokenizer })
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
                    if (128..=512).contains(&len) {
                        let line_start = code[..node.start_byte()]
                            .rfind('\n')
                            .map_or(0, |i| i + 1);
                        // Only extend to line_start when the intervening bytes are pure
                        // whitespace (indentation).  Mid-line nodes (e.g. a block body
                        // that begins after `) -> T {`) must not pull in non-whitespace.
                        let is_pure_indent = code[line_start..node.start_byte()]
                            .bytes()
                            .all(|b| b == b' ' || b == b'\t');
                        let code_start =
                            if is_pure_indent { line_start } else { node.start_byte() };
                        res.push(SlicedChunk {
                            code: code[code_start..node.end_byte()].into(),
                            #[cfg(test)]
                            start_byte: node.start_byte(),
                            #[cfg(test)]
                            end_byte: node.end_byte(),
                            start_line: node.start_position().row + 1,
                            end_line: node.end_position().row + 1,
                            start_column: node.start_position().column,
                            end_column: node.end_position().column,
                        });
                        // Do not descend: children would produce overlapping chunks.
                        descend = false;
                    } else if len < 128 {
                        // Children are strictly smaller; no qualifying node below.
                        descend = false;
                    }
                    // len > 512: keep descending to find qualifying sub-nodes.
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

        Ok(res)
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

    fn slicer() -> Slicer<'static> {
        Slicer::new(Language::new(tree_sitter_rust::LANGUAGE), tokenizer()).unwrap()
    }

    fn all_source_files() -> Vec<(String, String)> {
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        collect_rs(&src_root, &src_root, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn collect_rs(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(String, String)>,
    ) {
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
        let mut slicer =
            Slicer::new(Language::new(tree_sitter_rust::LANGUAGE), &OnePerByte).unwrap();
        let chunks = slicer.parse(&src, CancellationToken::new()).unwrap();
        assert!(!chunks.is_empty(), "the fn node should have been selected");
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
            .map(|(_, src)| {
                slicer()
                    .parse(src, CancellationToken::new())
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(total > 0, "no chunks produced across all mindex source files");
    }

    #[test]
    fn chunk_code_matches_reported_byte_range() {
        for (name, src) in all_source_files() {
            for chunk in slicer().parse(&src, CancellationToken::new()).unwrap() {
                // code is extended to the start of the node's line (leading whitespace),
                // so the node text must appear at the end of code.
                let node_text = &src[chunk.start_byte..chunk.end_byte];
                assert!(
                    chunk.code.ends_with(node_text),
                    "{name}: chunk.code does not end with src[start_byte..end_byte]"
                );
                let prefix = &chunk.code[..chunk.code.len() - node_text.len()];
                assert!(
                    prefix.bytes().all(|b| b == b' ' || b == b'\t'),
                    "{name}: code prefix {prefix:?} before node text is not pure whitespace"
                );
            }
        }
    }

    #[test]
    fn indentation_is_preserved_in_code() {
        let mut checked = 0usize;
        for (name, src) in all_source_files() {
            for chunk in slicer().parse(&src, CancellationToken::new()).unwrap() {
                let before = &src[..chunk.start_byte];
                let line_start = before.rfind('\n').map_or(0, |i| i + 1);
                let indent = &src[line_start..chunk.start_byte];
                // Only check nodes that are indented (not at column 0, not mid-line).
                if indent.is_empty() || !indent.bytes().all(|b| b == b' ' || b == b'\t') {
                    continue;
                }
                let node_text = &src[chunk.start_byte..chunk.end_byte];
                let code_prefix = &chunk.code[..chunk.code.len() - node_text.len()];
                assert_eq!(
                    code_prefix, indent,
                    "{name}: leading whitespace not preserved for chunk at line {}",
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
                let expected_start_line =
                    before_start.bytes().filter(|&b| b == b'\n').count() + 1;
                let line_start = before_start.rfind('\n').map_or(0, |i| i + 1);
                let expected_start_col = chunk.start_byte - line_start;

                assert_eq!(
                    chunk.start_line, expected_start_line,
                    "{name}: start_line mismatch at byte {}", chunk.start_byte
                );
                assert_eq!(
                    chunk.start_column, expected_start_col,
                    "{name}: start_column mismatch at byte {}", chunk.start_byte
                );

                let before_end = &src[..chunk.end_byte];
                let expected_end_line =
                    before_end.bytes().filter(|&b| b == b'\n').count() + 1;
                let end_line_start = before_end.rfind('\n').map_or(0, |i| i + 1);
                let expected_end_col = chunk.end_byte - end_line_start;

                assert_eq!(
                    chunk.end_line, expected_end_line,
                    "{name}: end_line mismatch at byte {}", chunk.end_byte
                );
                assert_eq!(
                    chunk.end_column, expected_end_col,
                    "{name}: end_column mismatch at byte {}", chunk.end_byte
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

    #[test]
    fn chunks_satisfy_token_window() {
        let t = tokenizer();
        for (name, src) in all_source_files() {
            for chunk in slicer().parse(&src, CancellationToken::new()).unwrap() {
                let n = t.encode(chunk.code.as_str(), false).unwrap().len();
                assert!(n >= 128, "{name}: chunk has {n} tokens (minimum is 128)");
                assert!(n <= 512, "{name}: chunk has {n} tokens (maximum is 512)");
            }
        }
    }

    #[test]
    fn chunks_do_not_overlap() {
        for (name, src) in all_source_files() {
            let chunks = slicer().parse(&src, CancellationToken::new()).unwrap();
            let mut ranges: Vec<(usize, usize)> = chunks
                .iter()
                .map(|c| (c.start_byte, c.end_byte))
                .collect();
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
