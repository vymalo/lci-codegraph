//! Structural code graph (ADR-0086 §1). Built directly on tree-sitter — the same grammars the chunker
//! uses — as **edges on top of the per-file symbols** (definitions + their call sites).
//!
//! Output is payload-compatible with the graph payload the retired Graphify `graph.json` parse
//! produced (schema history, ADR-0019 → ADR-0086): [`GraphNode`] mirrors
//! `agent-clients::GraphNodePayload` (`node_id`, `label`, `source_file`, `start_line`) and
//! [`GraphEdge`] mirrors `GraphEdgePayload` (`source`, `target`, `relation`). The control plane's
//! generic `:Symbol` + `[:REL {relation}]` Neo4j write and the `graph_find_symbol` /
//! `graph_get_callers` retrieval tools are unchanged behind the seam.
//!
//! [`GraphNode`], [`GraphEdge`], [`Graph`], and the `<file>#<line>:<name>` def-id format now live in
//! the sibling `lci-codegraph-model` crate (re-exported here) rather than in this module directly. That
//! split exists for a consumer this crate doesn't have yet but is built to accommodate: a
//! framework-extractor crate (docs/design/spring-aware-graph.md) needs to speak the exact same node/edge
//! vocabulary and produce byte-identical def ids, without depending on this crate's tree-sitter-parsing
//! internals, and without this crate depending on it back. `lci-codegraph-model` depends on nothing but
//! `serde`, so it is the one place both sides of that seam can meet — see its own crate-level doc for
//! the full reasoning, and docs/architecture.md for how the pieces plug together at the workspace level.
//!
//! Languages with a real extractor today: **Rust** (ADR-0086 "Rust language first"), **Python**,
//! **TypeScript/JavaScript** (incl. JSX/TSX), and **Java**. Every language emits the SAME node/edge
//! vocabulary — the cross-file resolver ([`resolve`]/`pick`) is language-agnostic
//! and shared verbatim. Definitions and call references are identified by each grammar's bundled
//! `tags.scm` query ([`crate::tags`]); Rust keeps its own node-kind extractor for a byte-stable
//! golden. See `emit::Classifier`. Relations emitted:
//! - `contains` — file → top-level def, and container def (mod/struct/trait/enum/class) → nested def.
//! - `method` — a type container (impl/trait/struct/enum/class/interface) → a callable it defines.
//!   This is a specialisation of `contains` kept separate for parity with Graphify (which emits
//!   `method`).
//! - `calls` — caller def → callee def, with **cross-file** resolution (a call in file A resolved to a
//!   definition in file B). `graph_get_callers` traverses this relation, so the name must match.
//!
//! Line numbers are **1-based** and callable labels carry a `()` suffix (`add` → `add()`), both for
//! parity with the Graphify `graph.json` schema this crate replaced.
//!
//! ## Java qualifier resolution (Phase 0, docs/design/spring-aware-graph.md §4.2)
//! A call's qualifier is not the receiver's literal text unless nothing better is recoverable. For
//! Java, `callee::declared_type_of` first tries to recover the receiver's **declared type** from the
//! same file's AST (its local/field/parameter declaration), and `callee::supertype_names` lets a
//! qualifier naming an interface/superclass count as a match against a candidate whose own scope is
//! the concrete implementing/extending type. Together these fix the general (non-Spring) case that
//! motivated the design doc: `accountService.findById(id)`, where `accountService` is a field typed
//! `AccountService`, now resolves to `AccountServiceImpl.findById` — previously the qualifier was the
//! bare identifier `"accountService"`, which never textually matched any candidate's scope. Both are
//! Java-only **in effect** (gated by tree-sitter node kinds that simply don't exist in the other
//! tags-driven grammars), not by a language flag — see the doc comments on those two functions.
//!
//! ## Module layout
//! The module is split by concern, each a single-responsibility slice of the pipeline:
//! - `emit` — the tree-sitter DFS that emits def nodes + `contains`/`method` edges and records call
//!   sites ([`extract_file`], the language `emit::Classifier` dispatch).
//! - `callee` — parses a call site's callee reference (bare name + optional type qualifier) out of
//!   the AST, for both the Rust `call_expression` navigation and the tags-captured callee name node.
//! - `resolve` — the cross-file name resolver ([`resolve`]/`pick`) that turns
//!   recorded call sites into `calls` edges, and merges in whatever a framework-extractor crate
//!   contributed (see below).
//!
//! `emit`, `callee` and `resolve` are private modules; only [`extract_file`] and [`resolve`] are
//! re-exported, so the names above are written as plain code spans rather than intra-doc links.
//!
//! ## Where `FrameworkFacts` joins the graph
//! [`resolve`] takes a second parameter, `framework: Vec<FrameworkFacts>` — one entry per
//! framework-eligible file a caller chose to run a framework extractor over (today: Java files, via
//! `lci-codegraph-spring`; see `src/input.rs`). This module knows nothing about Spring, Feign, or any
//! other framework; it only knows the shape [`FrameworkFacts`]/[`FrameworkCallTarget`] describe —
//! additive nodes, additive edges, and additive call-target candidates — and merges them in
//! alongside the language-level facts, before the same sort+dedup canonicalisation every other node
//! and edge goes through. An empty `framework` vec reproduces this module's pre-framework behaviour
//! exactly, by construction: every non-Java caller, and every Java repo nothing framework-specific
//! was ever extracted for, is unaffected because there is nothing to merge, not because of a flag.

use std::sync::Arc;

mod callee;
mod emit;
mod resolve;
#[cfg(test)]
mod tests;

pub use emit::extract_file;
pub use lci_codegraph_model::{FrameworkCallTarget, FrameworkFacts, Graph, GraphEdge, GraphNode};
pub use resolve::resolve;

/// The per-file structural facts: the file's nodes (file node + defs), its intra-file `contains` /
/// `method` edges, and the unresolved call sites for the cross-file pass.
#[derive(Debug, Default)]
pub struct FileSymbols {
    nodes: Vec<GraphNode>,
    contains: Vec<GraphEdge>,
    /// Call sites attributed to their enclosing caller, resolved to `calls` edges once every file is
    /// known.
    calls: Vec<CallSite>,
    /// Callable defs (functions/methods) in this file, for same-file-first resolution.
    callables: Vec<Callable>,
}

/// A callable definition (`function`/`method`) recorded for call resolution.
#[derive(Debug, Clone)]
struct Callable {
    name: String,
    node_id: String,
    /// Enclosing type/class name (Rust `impl S` → `S`; a Python/TS/Java `class C` → `C`), used
    /// **only** as a tiebreaker to disambiguate several same-named callables. `None` for free
    /// functions.
    scope: Option<String>,
    /// Simple names of whatever `scope`'s type `extends`/`implements` (Java only — see
    /// `callee::supertype_names`), used by `resolve::pick` so a call qualifier naming an
    /// interface/superclass counts as a match against this callable, not a contradiction — the
    /// Phase 0 general-Java fix in docs/design/spring-aware-graph.md §4.2, item 2:
    /// `accountService.findById()`'s recovered qualifier is `AccountService`, but the sole
    /// candidate's `scope` is `AccountServiceImpl`; `scope_supers` is what lets `pick` see that
    /// `AccountServiceImpl implements AccountService` and accept the match. Empty for every
    /// callable with no such clause — free functions, Rust methods, and every non-Java tagged
    /// language. `Arc<[String]>` rather than `Vec<String>`: every method on the same type shares
    /// one enclosing scope's supers, so this is a refcount bump per callable instead of a clone of
    /// the same list.
    scope_supers: Arc<[String]>,
}

/// An unresolved call site: the enclosing caller def id, the bare callee name, and — when the call is
/// qualified by a receiver/path (`A::new`, `Foo.bar`) — that qualifier, used only to break same-name
/// ambiguity.
#[derive(Debug, Clone)]
struct CallSite {
    caller: String,
    name: String,
    qualifier: Option<String>,
}
