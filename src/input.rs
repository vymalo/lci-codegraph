//! Source-agnostic indexing core (ADR-0086 raw-inputs cutover). Everything in this module operates on
//! a [`RawInput`] — a logical path plus bytes — and knows nothing about where those bytes came from.
//!
//! `crate::walk`'s [`crate::walk::FsSource`] (a filesystem checkout) is one *reader* among several
//! possible ones: a git object store, a tarball, an HTTP fetch, a set of open editor buffers, or rows
//! pulled from a database could each walk their own source and hand [`RawInput`]s to an [`Indexer`] the
//! same way. Nothing here needs to change to support any of them.
//!
//! The operator ignore layer (`crate::ignore_list`) deliberately does **not** live here. Deciding which
//! inputs are even worth handing over — pruning a `.gitignore`d tree, skipping a `node_modules`
//! directory before descending into it, filtering rows a DB reader has no business indexing — is the
//! *reader's* job, because only the reader knows how to avoid paying the cost of producing an input it
//! will just be told to discard. This module only ever sees inputs a reader already decided to hand
//! over (plus a pruned-count via [`Indexer::record_pruned`] for readers that prune before `push`).

use std::borrow::Cow;

use lci_codegraph_model::FrameworkFacts;

use crate::chunk::{self, Chunk};
use crate::graph::{self, FileSymbols, Graph};
use crate::lang;
use crate::pdf::{self, PdfOutcome};
use crate::tuning::IndexTuning;

/// The largest input any path through [`Indexer::push`] will accept —
/// `max(chunk::MAX_FILE_BYTES, pdf::MAX_PDF_BYTES)`. A reader should treat this as the bound to apply
/// at the I/O level when reading a candidate input, so nothing over it is ever pulled into memory
/// whole; `push` itself still enforces the tighter, per-kind cap (source vs PDF) once it knows which
/// one applies.
pub const MAX_INPUT_BYTES: usize = {
    let pdf_max = pdf::MAX_PDF_BYTES as usize;
    if chunk::MAX_FILE_BYTES > pdf_max {
        chunk::MAX_FILE_BYTES
    } else {
        pdf_max
    }
};

/// One raw input to index: a logical path plus its bytes.
#[derive(Debug, Clone)]
pub struct RawInput<'a> {
    /// Logical path, '/'-separated. Drives language detection and graph node ids. Need not exist on
    /// disk — a reader that has no real filesystem path (a DB row, a gist, an editor buffer) can
    /// synthesise one.
    pub path: Cow<'a, str>,
    /// Raw bytes. Decoded as UTF-8 here (or handed to the PDF extractor, for a `.pdf` path).
    pub content: Cow<'a, [u8]>,
    /// Explicit language id override (`"rust"`, `"python"`, …). `None` means detect from `path`'s
    /// extension. This exists because a raw input often has no meaningful filename to detect from at
    /// all — a DB row, a gist, an editor buffer — so a reader that already knows the language can say
    /// so directly instead of faking a path extension.
    pub language: Option<Cow<'a, str>>,
}

impl<'a> RawInput<'a> {
    /// Plain constructor, not a `bon` builder: `RawInput` is a 2-required-field value type (path +
    /// bytes), not a config struct, so a builder would add ceremony without earning it.
    pub fn new(path: impl Into<Cow<'a, str>>, content: impl Into<Cow<'a, [u8]>>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
            language: None,
        }
    }

    /// Convenience for a caller that already has UTF-8 text in hand — borrows `text.as_bytes()` rather
    /// than copying it.
    #[must_use]
    pub fn text(path: impl Into<Cow<'a, str>>, text: &'a str) -> Self {
        Self {
            path: path.into(),
            content: Cow::Borrowed(text.as_bytes()),
            language: None,
        }
    }

    #[must_use]
    pub fn with_language(mut self, language: impl Into<Cow<'a, str>>) -> Self {
        self.language = Some(language.into());
        self
    }
}

/// Content-level indexing options — everything that does not depend on where the bytes came from.
/// (Reader-specific config, like `WalkOptions::respect_gitignore`, lives on the reader instead.)
#[derive(Debug, Clone, bon::Builder)]
pub struct IndexOptions {
    /// Chunking/window tuning (carried over verbatim, ADR-0086).
    #[builder(default)]
    pub tuning: IndexTuning,
    /// Build the in-house structural graph (Rust, Python, TS/JS+TSX, Java). Default `false` so the
    /// crate is behavior-neutral until a host opts in (ADR-0086 migration shape).
    #[builder(default = false)]
    pub build_graph: bool,
    /// Extract text from PDFs and index it. Default true.
    #[builder(default = true)]
    pub extract_pdfs: bool,
}

/// Counters for one indexing run. Logged so a too-broad ignore glob (or a PDF-heavy source) is
/// diagnosable.
#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub files_chunked: usize,
    /// Inputs the reader pruned before they ever reached [`Indexer::push`] — e.g. `FsSource` pruning a
    /// `.gitignore`d or operator-ignored path before walking into it. Generalised beyond the FS walk:
    /// whatever "decided not to hand this over" means for a given reader, it is reported here via
    /// [`Indexer::record_pruned`].
    pub paths_ignored: usize,
    pub pdfs_extracted: usize,
    pub pdfs_skipped: usize,
    /// Inputs rejected as binary, for either of two reasons: the content does not decode as UTF-8 at
    /// all, or it decodes fine but trips the content sniff (a NUL byte in the first 512 bytes) — NUL is
    /// a legal Unicode scalar, so a binary blob can pass UTF-8 decoding and still be junk.
    ///
    /// Counted rather than silently dropped: without it, "this source has fewer indexable inputs than
    /// I expected" and "a generated/binary asset is misnamed with a source extension" look exactly the
    /// same from the summary line — and the second is an actionable problem.
    pub files_skipped_binary: usize,
    /// Inputs over [`chunk::MAX_FILE_BYTES`], rejected before any parse/decode work.
    ///
    /// Counted for the same reason as `files_skipped_binary`: with raw inputs, "nothing got indexed"
    /// and "you handed me something over the byte cap" otherwise look identical from the outside.
    pub files_skipped_too_large: usize,
    /// Inputs for which no language could be determined — `RawInput::language` was `None` and
    /// `lang::from_path` could not classify the path either (and the path was not a PDF).
    ///
    /// Counted for the same reason as `files_skipped_binary`: with raw inputs — which often have no
    /// meaningful filename at all — "nothing got indexed" and "you handed me a path I can't classify"
    /// otherwise look identical from the outside.
    pub files_skipped_unsupported: usize,
    /// Chunks that received an embedding vector. Stays `0` unless [`crate::embed::embed_output`] runs
    /// over this run's [`IndexOutput`] — `Indexer` itself never touches this field, since embedding is
    /// fallible network I/O and deliberately lives outside the indexing core (see
    /// `docs/architecture.md`, "Where embedding sits").
    pub chunks_embedded: usize,
    /// Round-trips made to the embeddings endpoint by [`crate::embed::embed_output`]. Batching is the
    /// caller-supplied batch size, so a too-small batch shows up here as a suspiciously high count.
    /// Stays `0` for the same reason as `chunks_embedded`.
    pub embed_batches: usize,
}

/// The output of an indexing run: chunks to embed and the resolved structural graph.
#[derive(Debug, Default)]
pub struct IndexOutput {
    pub chunks: Vec<Chunk>,
    pub graph: Graph,
    pub stats: IndexStats,
}

/// Streaming, push-based indexer. Feed it raw inputs from any reader, then call [`Indexer::finish`].
pub struct Indexer {
    options: IndexOptions,
    chunks: Vec<Chunk>,
    file_symbols: Vec<FileSymbols>,
    /// One [`FrameworkFacts`] per Java input the one-parse fast path ran
    /// `lci_codegraph_spring::extract_facts` over — a sibling pass over the SAME tree
    /// `graph::extract_file` already consumed, not a second parse (see `push` below). Empty for
    /// every non-Java language, by construction: this crate stays a pure syntactic extractor with
    /// zero framework knowledge of its own (`docs/design/spring-aware-graph.md` §5.2) — the one
    /// framework-shaped thing in this whole crate is this field and the one call that fills it.
    framework_facts: Vec<FrameworkFacts>,
    stats: IndexStats,
}

impl Indexer {
    #[must_use]
    pub fn new(options: IndexOptions) -> Self {
        Self {
            options,
            chunks: Vec::new(),
            file_symbols: Vec::new(),
            framework_facts: Vec::new(),
            stats: IndexStats::default(),
        }
    }

    /// Index one raw input. Never panics, never errors — unusable input is counted (see
    /// [`IndexStats`]) and skipped rather than failing the whole run.
    pub fn push(&mut self, input: RawInput<'_>) {
        // Normalise the path once, up front — every branch below and every id derived from it
        // (chunk `file_path`, graph node ids) uses this form.
        let path = input.path.replace('\\', "/");

        // ── PDFs: bounded text extraction → windowed chunks ──────────────────────────────────────
        if lang::is_pdf(std::path::Path::new(&path)) {
            if !self.options.extract_pdfs {
                // Deliberately uncounted: with extraction off, an unextracted PDF is equivalent to a
                // path `lang::from_path` can't classify, and that (non-PDF) case only became countable
                // once `files_skipped_unsupported` was introduced — a PDF must not start landing there
                // too, or the counter would conflate "no language" with "chose not to extract".
                return;
            }
            if input.content.len() as u64 > pdf::MAX_PDF_BYTES {
                tracing::debug!(path = %path, "codegraph: PDF over byte cap, skipped");
                self.stats.pdfs_skipped += 1;
                return;
            }
            match pdf::extract_from_bytes(&input.content) {
                PdfOutcome::Text(text) => {
                    let file_chunks = chunk::chunk_text(&path, &text, "pdf", self.options.tuning);
                    if !file_chunks.is_empty() {
                        self.chunks.extend(file_chunks);
                        self.stats.files_chunked += 1;
                    }
                    self.stats.pdfs_extracted += 1;
                }
                PdfOutcome::TooLarge => {
                    tracing::debug!(path = %path, "codegraph: PDF over byte cap, skipped");
                    self.stats.pdfs_skipped += 1;
                }
                PdfOutcome::Failed(reason) => {
                    tracing::debug!(path = %path, %reason, "codegraph: PDF extraction failed, skipped");
                    self.stats.pdfs_skipped += 1;
                }
            }
            return;
        }

        // ── Language: explicit override wins, else detect from the path ────────────────────────────
        let language = input
            .language
            .as_deref()
            .or_else(|| lang::from_path(std::path::Path::new(&path)));
        let Some(language) = language else {
            tracing::debug!(path = %path, "codegraph: no language detected, skipped");
            self.stats.files_skipped_unsupported += 1;
            return;
        };

        // ── Byte cap ─────────────────────────────────────────────────────────────────────────────
        if input.content.len() > chunk::MAX_FILE_BYTES {
            tracing::debug!(path = %path, "codegraph: input over byte cap, skipped");
            self.stats.files_skipped_too_large += 1;
            return;
        }

        // ── UTF-8 ────────────────────────────────────────────────────────────────────────────────
        let Ok(source) = std::str::from_utf8(&input.content) else {
            tracing::debug!(path = %path, "codegraph: non-UTF8 content, skipped");
            self.stats.files_skipped_binary += 1;
            return;
        };

        // Binary content, caught by CONTENT rather than encoding: NUL is a legal Unicode scalar, so a
        // binary blob can decode as valid UTF-8 above and still be junk. Rejected here — before either
        // consumer — so the graph never ingests it either, not just the chunker.
        if chunk::is_binary(source) {
            tracing::debug!(path = %path, "codegraph: binary content, skipped");
            self.stats.files_skipped_binary += 1;
            return;
        }

        let file_chunks = if self.options.build_graph && lang::has_graph(language) {
            // Parse ONCE and feed the chunker, the graph builder, and — Java only — the Spring
            // framework sibling pass, all off this one tree (ADR-0086 "parse once").
            if let Some(tree) = lang::parse(source, language) {
                self.file_symbols
                    .push(graph::extract_file(&tree, &path, source, language));
                // A sibling walk over the SAME tree `extract_file` just consumed, not a second
                // parse — see `docs/architecture.md`'s one-parse section. Gated to Java only: this
                // is the only language `lci-codegraph-spring` knows how to read annotations/
                // interfaces from, and every other language must pay exactly nothing for a pass
                // that could never find anything relevant to it.
                if language == "java" {
                    self.framework_facts
                        .push(lci_codegraph_spring::extract_facts(&tree, source, &path));
                }
                let mut cs = chunk::chunk_tree(&tree, &path, source, language, self.options.tuning);
                if cs.is_empty() {
                    cs = chunk::chunk_text(&path, source, language, self.options.tuning);
                }
                cs
            } else {
                chunk::chunk_file(&path, source, language, self.options.tuning)
            }
        } else {
            chunk::chunk_file(&path, source, language, self.options.tuning)
        };

        if !file_chunks.is_empty() {
            self.chunks.extend(file_chunks);
            self.stats.files_chunked += 1;
        }
    }

    /// Record inputs a reader pruned before they ever reached [`Indexer::push`] (bumps
    /// `stats.paths_ignored`).
    pub fn record_pruned(&mut self, count: usize) {
        self.stats.paths_ignored += count;
    }

    /// Resolve the cross-file graph and return everything collected so far.
    #[must_use]
    pub fn finish(self) -> IndexOutput {
        let graph = if self.options.build_graph {
            graph::resolve(self.file_symbols, self.framework_facts)
        } else {
            Graph::default()
        };

        tracing::info!(
            files_chunked = self.stats.files_chunked,
            chunks = self.chunks.len(),
            graph_nodes = graph.nodes.len(),
            graph_edges = graph.edges.len(),
            paths_ignored = self.stats.paths_ignored,
            files_skipped_binary = self.stats.files_skipped_binary,
            files_skipped_too_large = self.stats.files_skipped_too_large,
            files_skipped_unsupported = self.stats.files_skipped_unsupported,
            pdfs_extracted = self.stats.pdfs_extracted,
            pdfs_skipped = self.stats.pdfs_skipped,
            "codegraph: index complete"
        );

        IndexOutput {
            chunks: self.chunks,
            graph,
            stats: self.stats,
        }
    }
}

/// Convenience over [`Indexer`] for a caller that already has an iterator of inputs.
#[must_use]
pub fn index_inputs<'a, I: IntoIterator<Item = RawInput<'a>>>(
    inputs: I,
    options: &IndexOptions,
) -> IndexOutput {
    let mut indexer = Indexer::new(options.clone());
    for input in inputs {
        indexer.push(input);
    }
    indexer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── RawInput constructors ───────────────────────────────────────────────────────────────────────

    #[test]
    fn new_accepts_both_an_owned_vec_and_a_borrowed_slice() {
        let owned = RawInput::new("a.rs", b"fn a() {}\n".to_vec());
        assert_eq!(owned.content.as_ref(), b"fn a() {}\n");

        let borrowed_bytes: &[u8] = b"fn b() {}\n";
        let borrowed = RawInput::new("b.rs", borrowed_bytes);
        assert_eq!(borrowed.content.as_ref(), b"fn b() {}\n");
    }

    #[test]
    fn text_borrows_the_str_bytes_rather_than_copying_them() {
        // `RawInput::text` exists specifically so a caller with `&'a str` already in hand (a test
        // fixture, an editor buffer) doesn't pay a copy just to hand it to the indexer — assert the
        // `Cow` really is `Borrowed`, not merely that the resulting bytes happen to match.
        let text = "fn a() {}\n";
        let input = RawInput::text("a.rs", text);
        assert!(matches!(input.content, Cow::Borrowed(_)));
        assert_eq!(input.content.as_ref(), text.as_bytes());
    }

    #[test]
    fn with_language_sets_an_explicit_override() {
        let input = RawInput::text("buffer", "fn a() {}\n").with_language("rust");
        assert_eq!(input.language.as_deref(), Some("rust"));
    }

    // ── Indexer::push + finish: the raw-input flagship (walk's cross-file-`calls` test, reborn) ──────

    #[test]
    fn indexer_push_and_finish_over_in_memory_sources_produces_a_cross_file_calls_edge() {
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        indexer.push(RawInput::text("src/a.rs", "fn caller() { target(); }\n"));
        indexer.push(RawInput::text("src/b.rs", "fn target() {}\n"));
        let out = indexer.finish();

        assert!(
            out.graph.edges.iter().any(|e| e.relation == "calls"),
            "expected a cross-file calls edge, edges = {:?}",
            out.graph.edges
        );
        assert_eq!(out.stats.files_chunked, 2);
    }

    // ── language override vs. an unclassifiable path ────────────────────────────────────────────────

    #[test]
    fn language_override_on_an_extensionless_path_is_chunked_and_graphed() {
        // A raw input often has no meaningful filename at all (a DB row, a gist, an editor buffer) —
        // `"buffer-1"` has no extension for `lang::from_path` to key off, so only the explicit
        // override can make this indexable.
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        indexer.push(RawInput::text("buffer-1", "fn a() {}\n").with_language("rust"));
        let out = indexer.finish();

        assert_eq!(out.stats.files_chunked, 1);
        assert_eq!(out.stats.files_skipped_unsupported, 0);
        assert!(
            out.graph.nodes.iter().any(|n| n.label == "a()"),
            "expected the override language to drive graph extraction: {:?}",
            out.graph.nodes
        );
    }

    #[test]
    fn same_extensionless_input_without_override_is_skipped_as_unsupported() {
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        indexer.push(RawInput::text("buffer-1", "fn a() {}\n"));
        let out = indexer.finish();

        assert_eq!(out.stats.files_skipped_unsupported, 1);
        assert_eq!(out.stats.files_chunked, 0);
        assert!(out.graph.nodes.is_empty());
    }

    // ── one counter, one cause, each ────────────────────────────────────────────────────────────────

    #[test]
    fn files_skipped_unsupported_counts_an_unclassifiable_path() {
        let mut indexer = Indexer::new(IndexOptions::builder().build());
        indexer.push(RawInput::text("no-extension-at-all", "just some text\n"));
        let stats = indexer.finish().stats;

        assert_eq!(stats.files_skipped_unsupported, 1);
        // The other skip counters must not move — otherwise this test could pass by tripping the
        // wrong bucket instead of the one it names.
        assert_eq!(stats.files_skipped_binary, 0);
        assert_eq!(stats.files_skipped_too_large, 0);
        assert_eq!(stats.files_chunked, 0);
    }

    #[test]
    fn files_skipped_too_large_counts_content_over_the_chunk_byte_cap() {
        let mut indexer = Indexer::new(IndexOptions::builder().build());
        let oversized = vec![b'a'; chunk::MAX_FILE_BYTES + 1];
        indexer.push(RawInput::new("src/big.rs", oversized));
        let stats = indexer.finish().stats;

        assert_eq!(stats.files_skipped_too_large, 1);
        assert_eq!(stats.files_skipped_binary, 0);
        assert_eq!(stats.files_skipped_unsupported, 0);
        assert_eq!(stats.files_chunked, 0);
    }

    #[test]
    fn files_skipped_binary_counts_content_that_does_not_decode_as_utf8() {
        let mut indexer = Indexer::new(IndexOptions::builder().build());
        indexer.push(RawInput::new(
            "src/bad.rs",
            vec![0x66, 0x6e, 0xff, 0xfe, 0x00, 0x62],
        ));
        let stats = indexer.finish().stats;

        assert_eq!(stats.files_skipped_binary, 1);
        assert_eq!(stats.files_skipped_too_large, 0);
        assert_eq!(stats.files_skipped_unsupported, 0);
        assert_eq!(stats.files_chunked, 0);
    }

    #[test]
    fn files_skipped_binary_counts_valid_utf8_content_that_carries_a_nul_byte() {
        // NUL is a legal Unicode scalar, so this decodes as UTF-8 without error — it must still be
        // caught by the content sniff (`chunk::is_binary`), the SECOND of the two `files_skipped_binary`
        // causes, not the UTF-8 decode step above it.
        let mut indexer = Indexer::new(IndexOptions::builder().build());
        let mut body = vec![0u8, 0u8, 0u8];
        body.extend_from_slice(b"fn a() {}\n");
        indexer.push(RawInput::new("src/nul.rs", body));
        let stats = indexer.finish().stats;

        assert_eq!(stats.files_skipped_binary, 1);
        assert_eq!(stats.files_skipped_too_large, 0);
        assert_eq!(stats.files_skipped_unsupported, 0);
        assert_eq!(stats.files_chunked, 0);
    }

    #[test]
    fn record_pruned_accumulates_into_paths_ignored() {
        let mut indexer = Indexer::new(IndexOptions::builder().build());
        indexer.record_pruned(3);
        indexer.record_pruned(2);
        let stats = indexer.finish().stats;

        assert_eq!(stats.paths_ignored, 5);
    }

    // ── PDFs ─────────────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn extract_pdfs_false_skips_a_pdf_input_with_no_counter_movement_at_all() {
        // Deliberate, per the comment in `push`: with extraction off, an unextracted PDF must not
        // start landing in `files_skipped_unsupported` (that would conflate "no language detected"
        // with "chose not to extract") — nor in either PDF counter, since extraction never ran at all.
        let mut indexer = Indexer::new(IndexOptions::builder().extract_pdfs(false).build());
        indexer.push(RawInput::text(
            "docs/manual.pdf",
            "not actually parsed when extraction is off",
        ));
        let stats = indexer.finish().stats;

        assert_eq!(stats.pdfs_extracted, 0);
        assert_eq!(stats.pdfs_skipped, 0);
        assert_eq!(stats.files_skipped_unsupported, 0);
        assert_eq!(stats.files_skipped_binary, 0);
        assert_eq!(stats.files_skipped_too_large, 0);
        assert_eq!(stats.files_chunked, 0);
    }

    #[test]
    fn extract_pdfs_true_counts_garbage_bytes_at_a_pdf_path_as_pdfs_skipped() {
        let mut indexer = Indexer::new(IndexOptions::builder().extract_pdfs(true).build());
        indexer.push(RawInput::text(
            "docs/manual.pdf",
            "this is definitely not a pdf",
        ));
        let stats = indexer.finish().stats;

        assert_eq!(stats.pdfs_skipped, 1);
        assert_eq!(stats.pdfs_extracted, 0);
    }

    #[test]
    fn content_over_the_pdf_byte_cap_lands_in_pdfs_skipped() {
        let mut indexer = Indexer::new(IndexOptions::builder().extract_pdfs(true).build());
        let oversized = vec![b'a'; pdf::MAX_PDF_BYTES as usize + 1];
        indexer.push(RawInput::new("docs/huge.pdf", oversized));
        let stats = indexer.finish().stats;

        assert_eq!(stats.pdfs_skipped, 1);
        assert_eq!(stats.pdfs_extracted, 0);
    }

    #[test]
    fn a_genuinely_valid_pdf_input_is_extracted_and_chunked() {
        let pdf = minimal_pdf_with_text("Hello LCI");
        let mut indexer = Indexer::new(IndexOptions::builder().extract_pdfs(true).build());
        indexer.push(RawInput::new("docs/manual.pdf", pdf));
        let out = indexer.finish();

        assert_eq!(out.stats.pdfs_extracted, 1);
        assert_eq!(out.stats.pdfs_skipped, 0);
        let chunk = out
            .chunks
            .iter()
            .find(|c| c.file_path == "docs/manual.pdf")
            .expect("pdf produced at least one chunk");
        assert!(
            chunk.content.contains("Hello LCI"),
            "extracted text made it into the chunk content: {:?}",
            chunk.content
        );
    }

    /// A minimal, valid single-page PDF whose content stream draws `text`, built with `lopdf` (a
    /// proper xref/trailer is fiddly to hand-write) so extraction through `pdf-extract` is a real
    /// round trip. Copied from `src/pdf.rs`'s own test-only helper (and `tests/common`'s copy)
    /// rather than exported — it's a test-only fixture builder, not part of the public API.
    fn minimal_pdf_with_text(text: &str) -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![20.into(), 100.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).expect("save synthetic pdf");
        buf
    }

    // ── build_graph gating ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn build_graph_false_yields_chunks_but_an_empty_graph() {
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(false).build());
        indexer.push(RawInput::text("src/a.rs", "fn caller() { target(); }\n"));
        indexer.push(RawInput::text("src/b.rs", "fn target() {}\n"));
        let out = indexer.finish();

        assert!(!out.chunks.is_empty(), "chunks are still produced");
        assert!(out.graph.nodes.is_empty(), "graph off ⇒ no nodes");
        assert!(out.graph.edges.is_empty(), "graph off ⇒ no edges");
    }

    // ── index_inputs vs. driving Indexer manually ───────────────────────────────────────────────────

    #[test]
    fn index_inputs_over_an_iterator_matches_driving_the_indexer_manually() {
        let sources = vec![
            RawInput::text("src/a.rs", "fn caller() { target(); }\n"),
            RawInput::text("src/b.rs", "fn target() {}\n"),
        ];
        let options = IndexOptions::builder().build_graph(true).build();

        let via_helper = index_inputs(sources.clone(), &options);

        let mut indexer = Indexer::new(options);
        for input in sources {
            indexer.push(input);
        }
        let via_manual = indexer.finish();

        assert_eq!(via_helper.graph, via_manual.graph);
        assert_eq!(via_helper.chunks, via_manual.chunks);
        assert_eq!(
            via_helper.stats.files_chunked,
            via_manual.stats.files_chunked
        );
    }

    // ── path normalisation ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn windows_style_separators_in_the_path_are_normalised_to_forward_slashes() {
        let mut indexer = Indexer::new(IndexOptions::builder().build());
        indexer.push(RawInput::text(r"src\nested\a.rs", "fn a() {}\n"));
        let out = indexer.finish();

        let chunk = out.chunks.first().expect("one chunk produced");
        assert_eq!(chunk.file_path, "src/nested/a.rs");
    }

    // ── byte-cap boundary (mirrors chunk.rs's own pair of cap-boundary tests) ──────────────────────

    #[test]
    fn content_exactly_at_the_chunk_byte_cap_is_indexed_not_skipped() {
        // `push`'s cap check is strictly `>` (see the comment above it), so content sitting exactly
        // at the ceiling must still be indexed — the companion to
        // `files_skipped_too_large_counts_content_over_the_chunk_byte_cap` above, which only proves
        // the `+1` side of the boundary.
        let mut indexer = Indexer::new(IndexOptions::builder().build());
        let exact = vec![b'a'; chunk::MAX_FILE_BYTES];
        indexer.push(RawInput::new("src/boundary.rs", exact));
        let stats = indexer.finish().stats;

        assert_eq!(stats.files_chunked, 1);
        assert_eq!(stats.files_skipped_too_large, 0);
    }

    // ── MAX_INPUT_BYTES and the PDF-vs-source cap split ─────────────────────────────────────────────

    #[test]
    fn max_input_bytes_is_the_larger_of_the_two_per_kind_caps() {
        // `chunk::MAX_FILE_BYTES` and `pdf::MAX_PDF_BYTES` are both 5 MiB today, so this is written
        // against the relationship rather than a magic number — it still means something (and would
        // still catch a regression) if either cap ever moves independently of the other.
        assert_eq!(
            MAX_INPUT_BYTES,
            chunk::MAX_FILE_BYTES.max(pdf::MAX_PDF_BYTES as usize)
        );
    }

    #[test]
    fn an_oversized_pdf_is_rejected_by_the_pdf_branchs_own_cap_never_by_the_source_cap() {
        // `push` checks `lang::is_pdf` FIRST and returns from that branch unconditionally, so a
        // `.pdf` path can never fall through to the later `chunk::MAX_FILE_BYTES` check below it —
        // it is always `pdf::MAX_PDF_BYTES` that governs, and the rejection always lands in
        // `pdfs_skipped`, never `files_skipped_too_large`. Written against `pdf::MAX_PDF_BYTES`
        // rather than `chunk::MAX_FILE_BYTES` so this keeps meaning what it says even though the two
        // caps happen to be numerically equal today (see the test above).
        let mut indexer = Indexer::new(IndexOptions::builder().extract_pdfs(true).build());
        let oversized = vec![b'a'; pdf::MAX_PDF_BYTES as usize + 1];
        indexer.push(RawInput::new("docs/huge.pdf", oversized));
        let stats = indexer.finish().stats;

        assert_eq!(stats.pdfs_skipped, 1);
        assert_eq!(stats.pdfs_extracted, 0);
        assert_eq!(
            stats.files_skipped_too_large, 0,
            "a PDF must never be counted against the source-text byte cap"
        );
    }

    // ── an Indexer / index_inputs with nothing pushed ───────────────────────────────────────────────

    #[test]
    fn an_indexer_with_nothing_pushed_yields_all_zero_output() {
        let indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        let out = indexer.finish();

        assert!(out.chunks.is_empty());
        assert!(out.graph.nodes.is_empty());
        assert!(out.graph.edges.is_empty());
        assert_eq!(out.stats.files_chunked, 0);
        assert_eq!(out.stats.paths_ignored, 0);
        assert_eq!(out.stats.pdfs_extracted, 0);
        assert_eq!(out.stats.pdfs_skipped, 0);
        assert_eq!(out.stats.files_skipped_binary, 0);
        assert_eq!(out.stats.files_skipped_too_large, 0);
        assert_eq!(out.stats.files_skipped_unsupported, 0);
    }

    #[test]
    fn index_inputs_over_an_empty_iterator_yields_all_zero_output() {
        let out = index_inputs(
            Vec::<RawInput<'_>>::new(),
            &IndexOptions::builder().build_graph(true).build(),
        );

        assert!(out.chunks.is_empty());
        assert!(out.graph.nodes.is_empty());
        assert!(out.graph.edges.is_empty());
        assert_eq!(out.stats.files_chunked, 0);
    }

    // ── an override naming a language with no grammar at all ───────────────────────────────────────

    #[test]
    fn a_language_override_naming_an_unknown_grammar_falls_through_to_windowed_chunking() {
        // "klingon" has no tree-sitter grammar in this crate. `push` never validates the override
        // against the registry — it drives straight into `chunk::chunk_file`, whose
        // `lang::has_grammar` guard fails and falls back to windowed chunking. Pinned here rather
        // than assumed: nothing panics, the resulting chunk carries the override's language tag
        // verbatim, and — because `lang::has_graph` gates on that same registry — it contributes
        // nothing to the graph even with `build_graph` on.
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        indexer.push(RawInput::text("weird.xyz", "fn a() {}\n").with_language("klingon"));
        let out = indexer.finish();

        assert_eq!(out.stats.files_chunked, 1);
        assert_eq!(out.stats.files_skipped_unsupported, 0);
        let chunk = out
            .chunks
            .first()
            .expect("the windowed fallback still produces a chunk");
        assert_eq!(chunk.language, "klingon");
        assert_eq!(chunk.chunk_type, "window");
        assert!(
            out.graph.nodes.is_empty(),
            "an unknown-grammar override must never reach the graph builder: {:?}",
            out.graph.nodes
        );
    }

    // ── an override that contradicts the path extension ─────────────────────────────────────────────

    #[test]
    fn a_language_override_that_contradicts_the_path_extension_wins_over_detection() {
        // `.py` would normally detect as python via `lang::from_path` — the override must win
        // outright rather than merely nudge detection, so a `.py`-named input forced to `"rust"` is
        // parsed with the RUST grammar (which extracts a top-level `fn`; python's would not).
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        indexer.push(RawInput::text("mod.py", "fn a() {}\n").with_language("rust"));
        let out = indexer.finish();

        let chunk = out.chunks.first().expect("one chunk produced");
        assert_eq!(chunk.language, "rust");
        assert_eq!(
            chunk.chunk_type, "function",
            "parsed structurally by the rust grammar, not windowed as unrecognised python"
        );
        assert!(
            out.graph.nodes.iter().any(|n| n.label == "a()"),
            "the rust extractor ran on this content: {:?}",
            out.graph.nodes
        );
    }

    // ── case-insensitive PDF routing ────────────────────────────────────────────────────────────────

    #[test]
    fn uppercase_pdf_extension_still_routes_to_the_pdf_branch() {
        // `lang::is_pdf` is documented case-insensitive; garbage bytes at an uppercase `.PDF` path
        // must land in `pdfs_skipped` via the PDF branch, not fall through to source-text handling
        // and land in `files_skipped_unsupported` or `files_skipped_binary` instead.
        let mut indexer = Indexer::new(IndexOptions::builder().extract_pdfs(true).build());
        indexer.push(RawInput::text("DOC.PDF", "not a real pdf"));
        let stats = indexer.finish().stats;

        assert_eq!(stats.pdfs_skipped, 1);
        assert_eq!(stats.files_skipped_unsupported, 0);
        assert_eq!(stats.files_skipped_binary, 0);
    }

    // ── pushing the same path twice: pinning the ACTUAL behaviour, not the intended one ─────────────

    #[test]
    fn pushing_the_same_path_twice_with_identical_content_duplicates_chunks_but_not_graph_nodes() {
        // Pinned, not designed: `Indexer` has no path-uniqueness check, so nothing stops a caller
        // handing the same `RawInput` to `push` twice — unlike `FsSource`, which by construction
        // visits every filesystem path at most once. The two output layers react differently:
        //   - CHUNKS have no dedup at all, so the corpus ends up with two byte-identical chunks per
        //     symbol. This is a genuine footgun for a raw-input caller whose own retry/dedup logic
        //     might re-push the same logical file.
        //   - GRAPH nodes/edges dedup cleanly, because `graph::extract_file` derives node ids purely
        //     from (path, in-file position); the second push's `FileSymbols` therefore produces
        //     byte-identical node/edge ids to the first, and `graph::resolve`'s sort+dedup collapses
        //     them back down to one of each.
        let body = "fn caller() { target(); }\nfn target() {}\n";
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        indexer.push(RawInput::text("src/a.rs", body));
        indexer.push(RawInput::text("src/a.rs", body));
        let out = indexer.finish();

        assert_eq!(
            out.chunks.len(),
            4,
            "2 symbols x 2 pushes, NOT deduped: {:?}",
            out.chunks
        );
        assert_eq!(
            out.stats.files_chunked, 2,
            "the counter increments once per push, not once per unique path"
        );
        assert_eq!(
            out.graph.nodes.len(),
            3,
            "the file node + caller() + target(), deduped despite two pushes: {:?}",
            out.graph.nodes
        );
        assert_eq!(
            out.graph.edges.len(),
            3,
            "2 contains edges + the 1 calls edge, deduped: {:?}",
            out.graph.edges
        );
    }

    #[test]
    fn pushing_the_same_path_twice_with_different_content_keeps_both_definitions_in_the_graph() {
        // A second footgun shape: when the content genuinely differs across the two pushes, the
        // graph does NOT treat the second as replacing the first — nothing in `Indexer::push` or
        // `graph::resolve` treats `path` as a unique key, so both symbol sets end up coexisting
        // under the same `source_file`. A raw-input caller re-indexing an updated version of a file
        // must not simply re-push the old content alongside the new; there is no "last write wins"
        // here, unlike what a database UPSERT or a filesystem overwrite would give you.
        let mut indexer = Indexer::new(IndexOptions::builder().build_graph(true).build());
        indexer.push(RawInput::text("src/b.rs", "fn one() {}\n"));
        indexer.push(RawInput::text("src/b.rs", "fn two() {}\n"));
        let out = indexer.finish();

        assert_eq!(out.chunks.len(), 2);
        assert_eq!(
            out.graph
                .nodes
                .iter()
                .filter(|n| n.source_file == "src/b.rs")
                .count(),
            3,
            "the file node + BOTH one() and two(), even though they share a path: {:?}",
            out.graph.nodes
        );
        assert!(out.graph.nodes.iter().any(|n| n.label == "one()"));
        assert!(out.graph.nodes.iter().any(|n| n.label == "two()"));
    }
}
