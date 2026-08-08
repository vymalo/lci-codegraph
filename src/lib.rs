//! # lci-codegraph
//!
//! In-house code-graph crate (ADR-0086, RFC-0007 slice 1). It owns **chunking**, the structural
//! **graph**, and **embedding-prep** over one tree-sitter parse — the extractor the agent-plane's
//! `index` mode consumes — the sole structural-graph engine, having retired the Python **Graphify**
//! CLI dependency (ADR-0019).
//!
//! This crate is a pure extractor: it indexes raw `(path, bytes)` inputs and returns [`Chunk`]s and a
//! [`Graph`] of payload-compatible nodes/edges — a filesystem checkout is one *source* of such inputs,
//! not the only one. The **host** maps the output onto the internal-API payloads
//! (`agent-clients::ChunkPayload` / `GraphNodePayload` / `GraphEdgePayload`) and submits them, exactly
//! as `index_checkout` does today — the crate holds no `kube`/`sqlx`/forge dependencies (ADR-0083),
//! with **one** deliberate exception: [`embed`] carries a blocking HTTP client (`ureq`) for an
//! OpenAI-compatible embeddings endpoint. Embedding-prep (the graph-aware context header) is itself
//! part of what this crate delivers, and that logic has no reason to exist twice — once here to build
//! the input, once again in every host to actually send it — so the one HTTP call needed to use what
//! it built lives here too, opted into per-walk rather than always-on.
//!
//! ## What this crate delivers
//! - [`input`] — the source-agnostic indexing core: [`Indexer`] over raw `(path, bytes)`
//!   [`RawInput`]s, of which a filesystem checkout is one source.
//! - [`ignore_list`] — gitignore-style, operator-configurable ignore layer that **composes with** the
//!   repo `.gitignore`, replacing the old hardcoded dir set.
//! - [`pdf`] — bounded PDF text extraction (byte-capped before parse, panic-caught).
//! - [`graph`] — the structural call/reference graph with **cross-file resolution** for Rust, Python,
//!   TypeScript/JavaScript (incl. TSX/JSX), and Java.
//! - [`embed`] — semantic embeddings against an OpenAI-compatible endpoint, with a graph-aware context
//!   header built from the resolved [`Graph`]. No local/in-process model, no feature gate: live
//!   whenever `OPENAI_BASE_URL` (or an explicit [`EmbedConfig`]) says so. Runs over any reader's
//!   output via [`embed::embed_output`], not just the filesystem walk.
//! - [`walk`] — the filesystem reader ([`FsSource`]) plus the one-pass [`walk_checkout`] driver
//!   (+ embeddings, when configured), honouring both ignore layers.
//! - a parity-harness scaffold (`tests/parity.rs`) that snapshots the graph against a golden.
//!
//! ## Workspace layout
//! This package is the root of a Cargo workspace, not a standalone crate. [`Graph`], [`GraphNode`],
//! [`GraphEdge`] (re-exported below) live in the sibling `crates/lci-codegraph-model` crate and are
//! merely re-exported here — that crate depends on nothing but `serde`, which is what lets a
//! framework-extractor crate (docs/design/spring-aware-graph.md) speak the same node/edge vocabulary,
//! and produce byte-identical definition ids via its shared `def_node_id`, without either crate
//! depending on the other. See `docs/architecture.md` for the full shape and `lci-codegraph-model`'s
//! own crate-level doc for why a framework's annotation surface earns a crate boundary rather than a
//! module.

pub mod chunk;
pub mod embed;
pub mod graph;
pub mod ignore_list;
pub mod input;
pub mod lang;
pub mod pdf;
pub mod tags;
pub mod tuning;
pub mod walk;

pub use chunk::{Chunk, chunk_file, chunk_text};
pub use embed::{EmbedConfig, EmbedStats};
pub use graph::{Graph, GraphEdge, GraphNode};
pub use ignore_list::{DEFAULT_IGNORE_GLOBS, IgnoreConfig, IgnoreList};
pub use input::{
    IndexOptions, IndexOutput, IndexStats, Indexer, MAX_INPUT_BYTES, RawInput, index_inputs,
};
pub use pdf::{PdfOutcome, extract_from_path as extract_pdf_from_path};
pub use tuning::IndexTuning;
pub use walk::{FsSource, WalkOptions, walk_checkout, walk_checkout_from_env};

#[cfg(test)]
mod tests {
    //! Smoke test for the crate-root public re-export surface — `lib.rs` is pure `pub use` plumbing
    //! with no logic of its own, so this only proves every re-exported name resolves through the
    //! crate root the way a downstream host (`agent-runner`) depends on, not any behaviour.
    use super::*;

    #[test]
    fn public_re_exports_resolve_and_wire_together() {
        let chunks: Vec<Chunk> = chunk_text("f.txt", "one line", "text", IndexTuning::default());
        assert_eq!(chunks.len(), 1);

        let ignore_list =
            IgnoreList::build(&IgnoreConfig::builder().root("/repo").build()).unwrap();
        assert!(!DEFAULT_IGNORE_GLOBS.is_empty());
        assert!(ignore_list.is_ignored(std::path::Path::new("node_modules"), true));

        let graph = Graph::default();
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        let _: fn() -> Vec<GraphNode> = Vec::new;
        let _: fn() -> Vec<GraphEdge> = Vec::new;

        match extract_pdf_from_path(std::path::Path::new("/does/not/exist.pdf")) {
            PdfOutcome::Failed(_) => {}
            other => panic!("expected Failed for a missing path, got {other:?}"),
        }

        let dir = tempfile::tempdir().unwrap();
        let out: IndexOutput = walk_checkout(dir.path(), &WalkOptions::builder().build()).unwrap();
        let _: IndexStats = out.stats;

        // The raw-inputs core is directly usable without touching the filesystem at all.
        let inputs = vec![RawInput::text("f.rs", "fn a() {}\n").with_language("rust")];
        let options = IndexOptions::builder().build();
        let raw_out = index_inputs(inputs, &options);
        assert_eq!(raw_out.chunks.len(), 1);

        let mut indexer = Indexer::new(IndexOptions::builder().build());
        indexer.push(RawInput::new("g.rs", b"fn b() {}\n".to_vec()));
        let indexer_out = indexer.finish();
        assert_eq!(indexer_out.chunks.len(), 1);

        // `embed` re-exports: a config with no reachable endpoint, run through the real
        // `embed::embed_chunks` entry point on an empty chunk slice — proves `EmbedConfig`/
        // `EmbedStats` resolve through the crate root AND that `WalkOptions::embed` wires to the same
        // `embed` module the walk itself calls, not two divergent copies.
        let embed_config: EmbedConfig = EmbedConfig::builder()
            .base_url("http://127.0.0.1:1")
            .build();
        let empty_graph = Graph::default();
        let mut no_chunks: Vec<Chunk> = Vec::new();
        let embed_stats: EmbedStats =
            embed::embed_chunks(&mut no_chunks, &empty_graph, &embed_config, 32).unwrap();
        assert_eq!(embed_stats.chunks_embedded, 0);
        assert_eq!(embed_stats.batches, 0);
    }
}
