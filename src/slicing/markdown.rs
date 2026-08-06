//! Chunking for documentation, which is not chunking for code.
//!
//! Every load-bearing rule of the code slicer ([`crate::slicing::traits::Slicer`])
//! inverts here, and each inversion was measured rather than assumed:
//!
//! | code | markdown |
//! |---|---|
//! | 128-token **floor**, anything smaller is dropped | **no floor** — a 40-token section is a complete claim, and dropping it drops the answer |
//! | select one node, do not descend | sections *nest*; the unit is a run of sibling blocks, chosen by cost |
//! | one AST node per chunk | one chunk may hold several blocks, packed up to the cap |
//!
//! **Boundaries come from two signals: the headings the author wrote, and the
//! semantic shift between blocks.** The second is a *refinement* of the first,
//! and the distinction is what makes it safe. Structure sets the hard rules — a
//! chunk may never cross a level-1/2 heading, and swallowing a deeper one costs
//! — while the embedding decides where to cut among the choices structure
//! leaves open.
//!
//! On *this* repository the semantic term was measured to change nothing: it
//! moved 7-13% of boundaries and altered the retrieved rank of **zero** of 23
//! documentation questions (MRR@10 0.3931 with and without, identical per-case
//! ranks). That is not evidence it is useless — it is evidence that this
//! corpus is densely and deliberately headed, so the author had already marked
//! every topic change and left the embedding nothing to find. The signal it
//! adds is exactly the one that structure cannot supply, so it matters most
//! where structure is weakest: a document with sparse or absent headings gives
//! the structural rules nothing to work with, and packing degenerates to
//! "fill to the cap", cutting mid-topic. Documentation of that quality is the
//! common case across projects; this repository is not a fair sample of it.
//!
//! What it costs is one `/encode` per document, and that is the whole reason it
//! is a refinement rather than a requirement: when the embedder is unreachable
//! the slicer falls back to structure alone, which is measured to be equally
//! good here and is never worse than not indexing the file. Two consequences to
//! keep in mind: chunk boundaries then depend on the embedder's model and
//! precision, which `CHUNKS_DERIVATION_VERSION` cannot see; and block
//! embedding must happen *outside* the prepare transaction, which is why the
//! API below is two-phase ([`MarkdownSlicer::plan`] → [`MarkdownSlicer::segment`])
//! rather than one call like the code slicer.
//!
//! What the block structure buys over a line-based `#` splitter is separately
//! measured and real: MRR@10 0.3714 → 0.3931, recall@10 18/23 → 20/23.

use tokio_util::sync::CancellationToken;
use tree_sitter::{Node, Parser};

use crate::slicing::traits::{SlicedChunk, SlicerError, Tokenizing, token_boundary};

/// Blocks that stand on their own as a unit of meaning.
///
/// `list` is deliberately absent: measured on this repo's `CLAUDE.md`, a single
/// list runs 359 lines, so the walk descends to its `list_item` children instead.
const ATOMIC_BLOCKS: &[&str] = &[
    "paragraph",
    "fenced_code_block",
    "indented_code_block",
    "atx_heading",
    "setext_heading",
    "html_block",
    "table",
    "block_quote",
    "thematic_break",
];

/// A chunk may never span a heading this shallow. A level-1 or level-2 heading is
/// the author stating that the topic changed, which outranks any packing gain.
const HARD_HEADING_LEVEL: usize = 2;

/// Cost of opening one more chunk, in the same units as the heading penalty
/// below. This is what stops the optimum from being "every block on its own":
/// without it, a cost built only from penalties is minimised by never merging.
const CHUNK_COST: f64 = 0.35;

/// Cost of swallowing a level-3-or-deeper heading into a chunk that did not
/// start with it. Below [`CHUNK_COST`], so packing two small subsections
/// together is still preferred to emitting both alone — but a chunk that would
/// swallow two headings to save one chunk is not.
const SWALLOWED_HEADING_COST: f64 = 0.25;

/// One block of the document: the unit the segmentation packs.
///
/// Byte offsets only — every line and column a chunk reports is derived from its
/// final, trimmed byte range in [`push_span`], so carrying the block's own
/// positions here would just be a second source of truth that can disagree.
struct Block {
    start_byte: usize,
    end_byte: usize,
    /// Heading depth, or 0 for anything that is not a heading.
    level: usize,
}

/// A document parsed into blocks, awaiting segmentation.
///
/// This exists so the embedder call can sit between the two CPU halves: block
/// embedding is network I/O and must not happen inside the prepare transaction.
pub struct DocumentPlan {
    blocks: Vec<Block>,
    /// Whole-file token offsets, kept so `segment` measures spans the same way
    /// `plan` did rather than re-tokenizing.
    offsets: Vec<(usize, usize)>,
    /// The slicer's own cap, carried so `block_texts` truncates what it sends
    /// to the embedder with the same number `segment` will cut to.
    max_tokens: usize,
}

impl DocumentPlan {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The text of each block, in order — what to embed for the semantic term.
    ///
    /// Truncated to what a chunk may hold, because a block is not: one
    /// unwrapped paragraph can be the whole document. The vector is only ever
    /// compared against its neighbours' to price a boundary, and a block this
    /// long is going to be cut regardless — so its opening is enough, and
    /// sending the rest costs an attention matrix over the embedder's whole
    /// input window. That was measured to exhaust GPU memory, which the slicer
    /// survives (the semantic term degrades to structure alone, with a `warn!`)
    /// but the embedder pays for by dropping and reloading its model.
    pub fn block_texts<'c>(&self, code: &'c str) -> Vec<&'c str> {
        self.blocks
            .iter()
            .map(|b| {
                let end = token_boundary(
                    code,
                    &self.offsets,
                    b.start_byte,
                    b.end_byte,
                    self.max_tokens,
                )
                .unwrap_or(b.end_byte);
                &code[b.start_byte..end]
            })
            .collect()
    }
}

pub struct MarkdownSlicer<'a> {
    tokenizer: &'a dyn Tokenizing,
    parser: Parser,
    /// Hard cap on a chunk's tokens. Measured: 512 is too small for prose (it
    /// cuts explanations away from what they explain, DOC 15/23 against 18/23),
    /// and past 1024 nothing improves while each retrieved hit costs four times
    /// as much of a `/research` transcript.
    max_tokens: usize,
    /// Weight of the semantic-shift term relative to [`CHUNK_COST`]. 0 disables
    /// it, leaving pure structure. Above ~4 it outvotes the cost of opening a
    /// chunk and the segmentation degenerates into near-singletons (measured:
    /// 69 chunks at weight 1-2, 94 at 4, 149 at 8 for the same document).
    semantic_weight: f64,
}

impl<'a> MarkdownSlicer<'a> {
    pub fn new(
        language: tree_sitter::Language,
        tokenizer: &'a dyn Tokenizing,
        max_tokens: usize,
        semantic_weight: f64,
    ) -> Result<Self, SlicerError> {
        let mut parser = Parser::new();
        parser.set_language(&language)?;
        Ok(Self {
            tokenizer,
            parser,
            max_tokens,
            semantic_weight,
        })
    }

    /// Phase 1 (CPU): parse the document into blocks.
    pub fn plan(
        &mut self,
        code: &str,
        token: CancellationToken,
    ) -> Result<DocumentPlan, SlicerError> {
        let offsets = self.tokenizer.token_offsets(code)?;
        let tree = self.parser.parse(code, None).ok_or(SlicerError::Parse)?;
        let mut blocks = Vec::new();
        collect_blocks(tree.root_node(), code, &mut blocks);
        if token.is_cancelled() {
            return Err(SlicerError::Cancelled);
        }
        Ok(DocumentPlan {
            blocks,
            offsets,
            max_tokens: self.max_tokens,
        })
    }

    /// Phase 2 (CPU): pack the blocks into chunks.
    ///
    /// `vectors` is one dense embedding per block, in `plan`'s order. `None` —
    /// or a length that does not match the block count, which is the shape an
    /// embedder disagreement would take — falls back to structure alone rather
    /// than to a wrong answer.
    pub fn segment(
        &self,
        code: &str,
        plan: &DocumentPlan,
        vectors: Option<&[Vec<f32>]>,
        token: CancellationToken,
    ) -> Result<Vec<SlicedChunk>, SlicerError> {
        let blocks = &plan.blocks;
        if blocks.is_empty() {
            return Ok(Vec::new());
        }
        // Tokens in `code[a..b]`, measured on whole-file offsets like the code
        // slicer's window — see its note on tokenization being context-dependent.
        let tokens_between = |a: usize, b: usize| -> usize {
            plan.offsets
                .partition_point(|&(_, e)| e < b)
                .saturating_sub(plan.offsets.partition_point(|&(s, _)| s < a))
        };

        let coherence = vectors
            .filter(|v| self.semantic_weight > 0.0 && v.len() == blocks.len())
            .map(|v| Coherence::new(v, blocks, &tokens_between, self.max_tokens));

        let cuts = self.pack(blocks, &tokens_between, coherence.as_ref());

        let mut out = Vec::with_capacity(cuts.len());
        for (i, j) in cuts {
            if token.is_cancelled() {
                return Err(SlicerError::Cancelled);
            }
            let (first, last) = (&blocks[i], &blocks[j - 1]);
            if tokens_between(first.start_byte, last.end_byte) > self.max_tokens {
                // A single block over the cap. Measured: 2 of 138 blocks in this
                // repo's largest document, so this is a fallback, not the path.
                self.split_by_lines(code, &plan.offsets, first, last, &tokens_between, &mut out);
                continue;
            }
            push_chunk(&mut out, code, &plan.offsets, first, last);
        }
        Ok(out)
    }

    /// Structure-only convenience: plan and segment in one call, no embedder.
    #[cfg(test)]
    pub fn parse(
        &mut self,
        code: &str,
        token: CancellationToken,
    ) -> Result<Vec<SlicedChunk>, SlicerError> {
        let plan = self.plan(code, token.clone())?;
        self.segment(code, &plan, None, token)
    }

    /// The optimal packing of blocks into chunks.
    ///
    /// `best[j] = min over i of best[i] + cost(i..j)`, exactly solved. Two terms
    /// compete — one more chunk versus one more swallowed heading — so the
    /// greedy "fill until the cap" answer is not the optimum: greedy would
    /// happily bury a subsection heading mid-chunk to save a chunk it did not
    /// need to save. O(n²) candidates with O(1) cost each, and n is the block
    /// count of one document.
    fn pack(
        &self,
        blocks: &[Block],
        tokens_between: &dyn Fn(usize, usize) -> usize,
        coherence: Option<&Coherence>,
    ) -> Vec<(usize, usize)> {
        let n = blocks.len();
        let mut best = vec![f64::INFINITY; n + 1];
        let mut back = vec![0usize; n + 1];
        best[0] = 0.0;

        for j in 1..=n {
            for i in (0..j).rev() {
                if best[i].is_infinite() {
                    continue;
                }
                // Blocks [i, j) as one chunk. `i < j - 1` keeps a single
                // over-cap block representable; it is line-split on the way out.
                if i < j - 1
                    && tokens_between(blocks[i].start_byte, blocks[j - 1].end_byte)
                        > self.max_tokens
                {
                    break; // every earlier `i` only spans more
                }
                let Some(c) = self.cost(blocks, i, j, coherence) else {
                    continue;
                };
                if best[i] + c < best[j] {
                    best[j] = best[i] + c;
                    back[j] = i;
                }
            }
        }

        let mut cuts = Vec::new();
        let mut j = n;
        while j > 0 {
            cuts.push((back[j], j));
            j = back[j];
        }
        cuts.reverse();
        cuts
    }

    /// Cost of making blocks `[i, j)` one chunk, or `None` if forbidden.
    fn cost(
        &self,
        blocks: &[Block],
        i: usize,
        j: usize,
        coherence: Option<&Coherence>,
    ) -> Option<f64> {
        let mut penalty = 0.0;
        for b in &blocks[i + 1..j] {
            if b.level == 0 {
                continue;
            }
            if b.level <= HARD_HEADING_LEVEL {
                return None;
            }
            penalty += SWALLOWED_HEADING_COST;
        }
        let shift = coherence.map_or(0.0, |c| self.semantic_weight * c.incoherence(i, j));
        Some(CHUNK_COST + penalty + shift)
    }

    /// Last resort for one block above the cap: cut on line boundaries, and on
    /// token boundaries where there is no line boundary to cut at.
    ///
    /// The second half is not a refinement of the first. Prose wraps where its
    /// author chose to, so one paragraph on one line is ordinary, and a block
    /// like that used to leave here still over the cap — which downstream is
    /// silently truncated by the embedder past the model's context, or fails
    /// the whole batch when it exhausts GPU memory (see the code slicer's
    /// [`pack_window`](crate::slicing::traits) note).
    fn split_by_lines(
        &self,
        code: &str,
        offsets: &[(usize, usize)],
        first: &Block,
        last: &Block,
        tokens_between: &dyn Fn(usize, usize) -> usize,
        out: &mut Vec<SlicedChunk>,
    ) {
        let (start, end) = (first.start_byte, last.end_byte);
        let mut line_ends: Vec<usize> = code[start..end]
            .match_indices('\n')
            .map(|(i, _)| start + i + 1)
            .collect();
        line_ends.push(end);

        let mut w_start = start;
        let mut last_fit = start;
        for &le in &line_ends {
            if tokens_between(w_start, le) > self.max_tokens {
                if last_fit > w_start {
                    push_span(out, code, offsets, w_start, last_fit);
                    w_start = last_fit;
                }
                while tokens_between(w_start, le) > self.max_tokens {
                    let Some(cut) = token_boundary(code, offsets, w_start, le, self.max_tokens)
                    else {
                        break;
                    };
                    push_span(out, code, offsets, w_start, cut);
                    w_start = cut;
                }
            }
            last_fit = le;
        }
        if w_start < end {
            push_span(out, code, offsets, w_start, end);
        }
    }
}

/// How much a run of blocks disagrees with itself, from their embeddings.
///
/// The measure is `W - ||S||`, where each block contributes weight `w_k` (its
/// token mass, so a long block counts for more than a one-line one) and `S` is
/// the weighted sum of the blocks' **unit** vectors. It is the closed form of
/// "distance from every block to the segment's own centroid":
///
/// ```text
/// Σ_k w_k (1 - v̂_k · µ̂)  =  W - (Σ_k w_k v̂_k) · µ̂  =  W - ||S||     (µ̂ = S/||S||)
/// ```
///
/// Two things follow, and both matter. It is **zero** when every block points
/// the same way and grows exactly as they diverge — so it says "this run reads
/// as one topic" rather than merely "these two neighbours differ", which is
/// what a pairwise-distance rule would say and is noisier. And because it needs
/// only `Σ w_k` and `Σ w_k v̂_k`, both prefix sums, any of the O(n²) candidate
/// segments is priced in O(dimension) with no per-candidate embedding — which
/// is what lets the packing be solved exactly instead of by lookahead.
struct Coherence {
    /// Prefix sums of the weights.
    weights: Vec<f64>,
    /// Prefix sums of `w_k * unit(v_k)`, laid out row-major by block.
    sums: Vec<f64>,
    dim: usize,
}

impl Coherence {
    fn new(
        vectors: &[Vec<f32>],
        blocks: &[Block],
        tokens_between: &dyn Fn(usize, usize) -> usize,
        max_tokens: usize,
    ) -> Self {
        // Weights are relative to half the cap, so the term is on the same scale
        // whatever the cap is set to and `CHUNK_COST` keeps its meaning.
        let target = (max_tokens as f64 / 2.0).max(1.0);
        let dim = vectors.first().map_or(0, Vec::len);
        let mut weights = Vec::with_capacity(blocks.len() + 1);
        let mut sums = vec![0.0; (blocks.len() + 1) * dim];
        weights.push(0.0);

        for (k, (block, vector)) in blocks.iter().zip(vectors).enumerate() {
            let w = tokens_between(block.start_byte, block.end_byte) as f64 / target;
            weights.push(weights[k] + w);
            let norm = vector
                .iter()
                .map(|x| (*x as f64) * (*x as f64))
                .sum::<f64>()
                .sqrt();
            let scale = if norm > 0.0 { w / norm } else { 0.0 };
            let (prev, next) = (k * dim, (k + 1) * dim);
            for d in 0..dim.min(vector.len()) {
                sums[next + d] = sums[prev + d] + (vector[d] as f64) * scale;
            }
        }
        Self { weights, sums, dim }
    }

    /// Incoherence of blocks `[i, j)`; 0.0 when they are perfectly aligned.
    fn incoherence(&self, i: usize, j: usize) -> f64 {
        let span = self.weights[j] - self.weights[i];
        let (lo, hi) = (i * self.dim, j * self.dim);
        let norm = (0..self.dim)
            .map(|d| {
                let x = self.sums[hi + d] - self.sums[lo + d];
                x * x
            })
            .sum::<f64>()
            .sqrt();
        // Floating-point error can make ||S|| a hair larger than W when the
        // blocks are identical; a negative cost would be a packing incentive.
        (span - norm).max(0.0)
    }
}

/// Walks the block grammar down to the atomic blocks, in document order.
fn collect_blocks(node: Node, code: &str, out: &mut Vec<Block>) {
    let kind = node.kind();
    if ATOMIC_BLOCKS.contains(&kind) {
        if code[node.start_byte()..node.end_byte()].trim().is_empty() {
            return;
        }
        out.push(Block {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            level: heading_level(node),
        });
        return;
    }
    let mut cursor = node.walk();
    if kind == "list" {
        for child in node.children(&mut cursor) {
            if child.kind() == "list_item" {
                if code[child.start_byte()..child.end_byte()].trim().is_empty() {
                    continue;
                }
                out.push(Block {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    level: 0,
                });
            } else {
                collect_blocks(child, code, out);
            }
        }
        return;
    }
    for child in node.children(&mut cursor) {
        collect_blocks(child, code, out);
    }
}

/// Heading depth from the marker node (`atx_h3_marker` → 3), 0 for a non-heading.
///
/// A `setext_heading` is underlined rather than marked, and only two depths
/// exist; both are shallow enough to be hard boundaries, so it reports 1.
fn heading_level(node: Node) -> usize {
    match node.kind() {
        "setext_heading" => 1,
        "atx_heading" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find_map(|c| {
                    c.kind()
                        .strip_prefix("atx_h")?
                        .strip_suffix("_marker")?
                        .parse()
                        .ok()
                })
                .unwrap_or(1)
        }
        _ => 0,
    }
}

/// Emit blocks `[first, last]` as one chunk.
///
/// The line/column span is derived from the *trimmed* byte range rather than
/// from the blocks' own positions: a block node's end often includes its
/// trailing newline, so `last.end_line` can be one line past where the stored
/// text actually stops. Citations and `read_chunks` are keyed on that span
/// agreeing with the bytes, so there is one place that computes it.
fn push_chunk(
    out: &mut Vec<SlicedChunk>,
    code: &str,
    offsets: &[(usize, usize)],
    first: &Block,
    last: &Block,
) {
    push_span(out, code, offsets, first.start_byte, last.end_byte);
}

/// Emit `code[start..end]`, computing the line/column span from the bytes.
fn push_span(
    out: &mut Vec<SlicedChunk>,
    code: &str,
    offsets: &[(usize, usize)],
    start: usize,
    end: usize,
) {
    let text = code[start..end].trim_end();
    if text.trim().is_empty() {
        return;
    }
    let start_line = code[..start].bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = code[..start].rfind('\n').map_or(0, |i| i + 1);
    let end_byte = start + text.len();
    let end_line = code[..end_byte].bytes().filter(|&b| b == b'\n').count() + 1;
    let end_line_start = code[..end_byte].rfind('\n').map_or(0, |i| i + 1);
    let tokens = offsets
        .partition_point(|&(_, e)| e < end_byte)
        .saturating_sub(offsets.partition_point(|&(s, _)| s < start))
        .max(1);
    out.push(SlicedChunk {
        code: text.into(),
        tokens,
        #[cfg(test)]
        start_byte: start,
        #[cfg(test)]
        end_byte,
        // Documentation chunks are never AST-node selections, so they are not
        // governed by the code slicer's token window.
        #[cfg(test)]
        from_gap: true,
        start_line,
        end_line,
        start_column: start - line_start,
        end_column: end_byte - end_line_start,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use tokenizers::Tokenizer;

    static TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();

    fn tokenizer() -> &'static Tokenizer {
        TOKENIZER
            .get_or_init(|| Tokenizer::from_pretrained("Qwen/Qwen3-Embedding-0.6B", None).unwrap())
    }

    fn slicer_with(max: usize) -> MarkdownSlicer<'static> {
        semantic_slicer(max, 0.0)
    }

    fn semantic_slicer(max: usize, weight: f64) -> MarkdownSlicer<'static> {
        MarkdownSlicer::new(
            tree_sitter::Language::new(tree_sitter_md::LANGUAGE),
            tokenizer(),
            max,
            weight,
        )
        .unwrap()
    }

    /// Deterministic stand-in for BGE-M3: blocks sharing a marker word get the
    /// same direction, so "these two belong together" is expressible without a
    /// GPU. Dimension 4 is enough to place three distinct topics apart.
    fn topic_vectors(texts: &[&str]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; 4];
                for (i, topic) in ["alpha", "beta", "gamma"].iter().enumerate() {
                    if t.contains(topic) {
                        v[i] = 1.0;
                    }
                }
                if v.iter().all(|x| *x == 0.0) {
                    v[3] = 1.0;
                }
                v
            })
            .collect()
    }

    /// What a caller can actually observe about a chunk, for comparing two
    /// segmentations. `SlicedChunk` has no `PartialEq` and does not need one.
    fn shape(chunks: &[SlicedChunk]) -> Vec<(usize, usize, &str)> {
        chunks
            .iter()
            .map(|c| (c.start_line, c.end_line, c.code.as_str()))
            .collect()
    }

    /// Token length of a whole document, for sizing a test's cap to it.
    fn doc_tokens(src: &str) -> usize {
        tokenizer().encode(src, false).unwrap().len()
    }

    fn semantic_chunks(src: &str, max: usize, weight: f64) -> Vec<SlicedChunk> {
        let mut slicer = semantic_slicer(max, weight);
        let plan = slicer.plan(src, CancellationToken::new()).unwrap();
        let texts = plan.block_texts(src);
        let vectors = topic_vectors(&texts);
        slicer
            .segment(src, &plan, Some(&vectors), CancellationToken::new())
            .unwrap()
    }

    fn chunks(src: &str, max: usize) -> Vec<SlicedChunk> {
        slicer_with(max)
            .parse(src, CancellationToken::new())
            .unwrap()
    }

    fn repo_docs() -> Vec<(String, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = Vec::new();
        for rel in [
            "README.md",
            ".claude/CLAUDE.md",
            "embedder/README.md",
            "perf/README.md",
            "tools/hooks/README.md",
            "tools/mcp/mindex/README.md",
            "tools/mcp/scout/README.md",
            "tools/vscode/README.md",
        ] {
            if let Ok(text) = std::fs::read_to_string(root.join(rel)) {
                out.push((rel.to_string(), text));
            }
        }
        out
    }

    /// The defect that motivated parsing instead of scanning for `#`: this repo's
    /// documentation is full of shell blocks whose comments start a line with `#`.
    /// A line-based splitter cuts there and severs the command from its heading.
    #[test]
    fn a_hash_inside_a_fenced_block_is_not_a_heading() {
        let src = "## Real heading\n\nProse before.\n\n```sh\n# 1. not a heading\nls -la\n# 2. also not\ncat x\n```\n\nProse after.\n";
        let out = chunks(src, 4096);
        assert_eq!(
            out.len(),
            1,
            "the fence's `#` lines must not open new chunks, got {out:#?}"
        );
        assert!(out[0].code.contains("# 1. not a heading"));
        assert!(out[0].code.contains("Prose after."));
    }

    /// The inversion of the code slicer's 128-token floor: a short section is a
    /// complete claim. Nothing may be dropped for being small.
    #[test]
    fn short_sections_are_kept_not_dropped() {
        let src = "# T\n\nTiny.\n\n## A\n\nOne.\n\n## B\n\nTwo.\n";
        let out = chunks(src, 4096);
        let all: String = out.iter().map(|c| c.code.as_str()).collect();
        for needle in ["Tiny.", "One.", "Two."] {
            assert!(all.contains(needle), "{needle} was dropped from {all:?}");
        }
    }

    /// A level-1/2 heading is the author saying the topic changed; packing may
    /// never cross it, however much room is left in the chunk.
    #[test]
    fn a_chunk_never_spans_a_top_level_heading() {
        let src = "# One\n\nAlpha.\n\n# Two\n\nBeta.\n\n## Three\n\nGamma.\n";
        for c in chunks(src, 4096) {
            let spans_two = c.code.contains("Alpha.") && c.code.contains("Beta.");
            assert!(!spans_two, "chunk crossed a level-1 heading: {:?}", c.code);
            let spans_three = c.code.contains("Beta.") && c.code.contains("Gamma.");
            assert!(
                !spans_three,
                "chunk crossed a level-2 heading: {:?}",
                c.code
            );
        }
    }

    /// The cap is hard, including on the line-splitting fallback for a single
    /// oversized block.
    #[test]
    fn no_chunk_exceeds_the_cap() {
        let cap = 160;
        for (name, src) in repo_docs() {
            for c in chunks(&src, cap) {
                let n = tokenizer().encode(c.code.as_str(), false).unwrap().len();
                assert!(
                    n <= cap + 8,
                    "{name}: chunk at line {} has {n} tokens (cap {cap})",
                    c.start_line
                );
            }
        }
    }

    /// One paragraph on one line — prose wraps where its author chose to, and
    /// plenty of authors choose never — leaves the line-splitting fallback
    /// nothing to cut at. It must still come out under the cap: an unenforced
    /// cap is silent truncation by the embedder, or a batch-failing GPU
    /// exhaustion on a big enough document.
    #[test]
    fn a_block_on_a_single_line_is_cut_to_the_cap() {
        let cap = 64;
        let src = format!(
            "# Title\n\n{}\n",
            (0..600).map(|i| format!("word{i} ")).collect::<String>()
        );
        let out = chunks(&src, cap);
        assert!(out.len() > 5, "a long line produced {} chunks", out.len());
        for c in &out {
            let n = tokenizer().encode(c.code.as_str(), false).unwrap().len();
            assert!(
                n <= cap + 8,
                "chunk at line {} has {n} tokens (cap {cap})",
                c.start_line
            );
        }
    }

    /// The cap is genuinely a knob now. Under the v2 layout both slicers
    /// silently clamped it to 1020 — a Qdrant multivector limit that died with
    /// ColBERT — so `max_doc_chunk_tokens` above that was quietly ignored.
    /// This pins the clamp's absence: a large configured cap is honored, and
    /// chunks above the old ceiling exist.
    #[test]
    fn a_cap_above_the_old_colbert_ceiling_is_honored() {
        let src = format!(
            "# Title\n\n{}\n",
            (0..4000).map(|i| format!("word{i} ")).collect::<String>()
        );
        let out = chunks(&src, 4096);
        let mut over_old_ceiling = 0usize;
        for c in &out {
            let n = tokenizer().encode(c.code.as_str(), false).unwrap().len();
            assert!(
                n <= 4096 + 8,
                "chunk at line {} has {n} tokens (cap 4096)",
                c.start_line
            );
            if n > 1020 {
                over_old_ceiling += 1;
            }
        }
        assert!(
            over_old_ceiling > 0,
            "no chunk exceeded the old 1020-token clamp; either the fixture is \
             too small or the clamp is back: {out:#?}"
        );
    }

    /// A chunk's stored text is the file's own bytes, and the line span it
    /// reports is where those bytes are. Citations and `read_chunks` are keyed on
    /// that correspondence.
    #[test]
    fn chunk_text_is_verbatim_and_lines_match_bytes() {
        for (name, src) in repo_docs() {
            for c in chunks(&src, 1024) {
                assert_eq!(
                    &src[c.start_byte..c.end_byte],
                    c.code,
                    "{name}: chunk text is not the file's bytes"
                );
                let expected_start =
                    src[..c.start_byte].bytes().filter(|&b| b == b'\n').count() + 1;
                assert_eq!(
                    c.start_line, expected_start,
                    "{name}: start_line disagrees with start_byte"
                );
                let expected_end = src[..c.end_byte].bytes().filter(|&b| b == b'\n').count() + 1;
                assert_eq!(
                    c.end_line, expected_end,
                    "{name}: end_line disagrees with end_byte"
                );
            }
        }
    }

    #[test]
    fn chunks_are_ordered_and_do_not_overlap() {
        for (name, src) in repo_docs() {
            let out = chunks(&src, 1024);
            for w in out.windows(2) {
                assert!(
                    w[1].start_byte >= w[0].end_byte,
                    "{name}: chunk at line {} overlaps the previous one",
                    w[1].start_line
                );
            }
        }
    }

    /// A file that `list_files` shows and every other tool reports empty is
    /// worse than one that was never indexed: it reads as "this document is
    /// blank" rather than "look elsewhere".
    #[test]
    fn every_repo_document_yields_at_least_one_chunk() {
        let docs = repo_docs();
        assert!(!docs.is_empty(), "no documents found — test is vacuous");
        for (name, src) in docs {
            assert!(
                !chunks(&src, 1024).is_empty(),
                "{name} produced no chunks; it would be listed but unreadable"
            );
        }
    }

    /// Documentation's whole point is that prose reaches the index. The code
    /// slicer leaves ~46% of lines in no chunk at all; this one must not.
    #[test]
    fn chunks_cover_nearly_every_non_blank_line() {
        for (name, src) in repo_docs() {
            let covered: usize = chunks(&src, 1024)
                .iter()
                .map(|c| c.end_line - c.start_line + 1)
                .sum();
            let non_blank = src.lines().filter(|l| !l.trim().is_empty()).count();
            assert!(
                covered * 100 >= non_blank * 90,
                "{name}: only {covered} lines covered of {non_blank} non-blank"
            );
        }
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(chunks("", 1024).is_empty());
        assert!(chunks("   \n\n  \n", 1024).is_empty());
    }

    #[test]
    fn cancelled_token_errors_immediately() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            slicer_with(1024).parse("# x\n\nbody\n", token),
            Err(SlicerError::Cancelled)
        ));
    }

    /// The case the semantic term exists for: a document with no headings for
    /// the structural rules to work with. Structure alone can only pack to the
    /// cap; the embedding is what puts the topic change on a boundary.
    #[test]
    fn semantic_shift_cuts_an_unheaded_document_on_the_topic_change() {
        let src = "alpha alpha alpha one.\n\nalpha alpha alpha two.\n\n\
                   beta beta beta one.\n\nbeta beta beta two.\n";
        // The term is weighted by token mass against the cap, so it only
        // outvotes the cost of opening a chunk on a segment that is substantial
        // relative to that cap — it decides where to cut a document that has to
        // be cut, and never fragments a small one. So size the cap to the
        // document, which is the situation a real oversized document is in.
        let cap = doc_tokens(src);
        let out = semantic_chunks(src, cap, 1.0);
        assert_eq!(out.len(), 2, "expected one chunk per topic, got {out:#?}");
        assert!(out[0].code.contains("alpha") && !out[0].code.contains("beta"));
        assert!(out[1].code.contains("beta") && !out[1].code.contains("alpha"));

        // And the control: with the term off there is nothing but the cap, so
        // the whole document packs into one chunk that straddles both topics.
        assert_eq!(
            chunks(src, cap).len(),
            1,
            "structure alone should not find the seam"
        );
    }

    /// Weight is a dial, not a switch: enough of it outvotes the cost of opening
    /// a chunk and the segmentation collapses toward one block each. This is the
    /// behaviour `[slicer].doc_semantic_weight`'s upper bound guards against.
    #[test]
    fn an_excessive_semantic_weight_over_splits() {
        let src = "alpha one.\n\nbeta two.\n\ngamma three.\n\nalpha four.\n";
        let cap = doc_tokens(src);
        let moderate = semantic_chunks(src, cap, 1.0).len();
        let extreme = semantic_chunks(src, cap, 64.0).len();
        assert!(
            extreme > moderate,
            "a large weight should fragment: {moderate} vs {extreme}"
        );
    }

    /// Structure outranks the embedding: no weight buys a chunk across a
    /// level-1 heading, however alike the two sides look.
    #[test]
    fn semantic_shift_never_overrides_a_top_level_heading() {
        let src = "# One\n\nalpha alpha alpha.\n\n# Two\n\nalpha alpha alpha.\n";
        for c in semantic_chunks(src, 4096, 1.0) {
            assert!(
                !(c.code.contains("# One") && c.code.contains("# Two")),
                "chunk crossed a level-1 heading: {:?}",
                c.code
            );
        }
    }

    /// The degradation path that makes the embedder optional. A vector list that
    /// does not match the block count is what a disagreeing embedder looks like,
    /// and it must fall back to structure rather than mis-index or panic.
    #[test]
    fn a_mismatched_vector_count_falls_back_to_structure() {
        let src = "alpha alpha alpha one.\n\nbeta beta beta two.\n";
        let mut slicer = semantic_slicer(4096, 1.0);
        let plan = slicer.plan(src, CancellationToken::new()).unwrap();
        let truncated = vec![vec![1.0f32, 0.0, 0.0, 0.0]];
        let out = slicer
            .segment(src, &plan, Some(&truncated), CancellationToken::new())
            .unwrap();
        assert_eq!(shape(&out), shape(&chunks(src, 4096)));
    }

    /// Zero weight must be exactly the structure-only answer, so turning the
    /// term off is a real off switch and not a slightly different algorithm.
    #[test]
    fn zero_weight_ignores_the_vectors_entirely() {
        let src = "alpha alpha alpha one.\n\nbeta beta beta two.\n";
        assert_eq!(
            shape(&semantic_chunks(src, 4096, 0.0)),
            shape(&chunks(src, 4096))
        );
    }

    /// Identical blocks must cost nothing to merge; floating-point error in the
    /// closed form could otherwise make incoherence negative and pay for merges.
    #[test]
    fn identical_blocks_have_no_incoherence() {
        let src = "alpha one.\n\nalpha one.\n\nalpha one.\n";
        let mut slicer = semantic_slicer(4096, 1.0);
        let plan = slicer.plan(src, CancellationToken::new()).unwrap();
        let vectors = topic_vectors(&plan.block_texts(src));
        let tokens_between = |a: usize, b: usize| -> usize {
            plan.offsets
                .partition_point(|&(_, e)| e < b)
                .saturating_sub(plan.offsets.partition_point(|&(s, _)| s < a))
        };
        let coherence = Coherence::new(&vectors, &plan.blocks, &tokens_between, 4096);
        let inc = coherence.incoherence(0, plan.blocks.len());
        assert!(inc.abs() < 1e-9, "expected ~0 incoherence, got {inc}");
    }

    #[test]
    fn heading_levels_are_read_from_the_marker() {
        let src = "# a\n\n## b\n\n### c\n\n###### f\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter::Language::new(tree_sitter_md::LANGUAGE))
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut blocks = Vec::new();
        collect_blocks(tree.root_node(), src, &mut blocks);
        let levels: Vec<usize> = blocks.iter().map(|b| b.level).collect();
        assert_eq!(levels, vec![1, 2, 3, 6]);
    }
}
