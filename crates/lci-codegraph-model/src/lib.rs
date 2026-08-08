//! Shared graph vocabulary for the `lci-codegraph` workspace.
//!
//! This crate holds the types every extractor in the workspace needs to agree on byte-for-byte:
//! [`GraphNode`], [`GraphEdge`], [`Graph`], the [`def_node_id`] id format, and — since this crate is
//! also where a framework extractor's contract with the core lives — [`FrameworkFacts`] /
//! [`FrameworkCallTarget`].
//!
//! It is deliberately the **bottom** of the workspace's dependency graph: no `tree-sitter`, no
//! `lci-codegraph`, no anything else beyond `serde`. A framework-extractor crate (e.g. one that reads
//! Spring annotations out of a Java `Tree`) and the `lci-codegraph` core crate both depend on this one
//! and neither depends on the other — that is what lets a framework's node/edge/call-target facts flow
//! into the core's resolver without the core ever needing to `use` a framework-specific crate, and
//! without the framework crate needing the whole tree-sitter-parsing core just to describe its output.
//! See `docs/architecture.md` in the `lci-codegraph` package for how the pieces plug together, and
//! `docs/design/spring-aware-graph.md` §5.2 for why that boundary is worth a whole crate rather than a
//! module: a framework's annotation surface is a *curated allowlist that churns every release*
//! (Spring's own annotation set moves every major/minor, and this crate's fixtures already straddle the
//! `javax.persistence` → `jakarta.persistence` migration) — a different and narrower kind of coupling
//! than a tree-sitter grammar, which changes rarely and is maintained by someone else. Putting that
//! coupling behind a crate boundary means "does the core know about Spring?" is answerable by reading a
//! `Cargo.toml` dependency list, not by auditing every module for a stray `use`.

use serde::Serialize;

/// One graph node. Field set mirrors `agent-clients::GraphNodePayload` exactly (`node_id`, `label`,
/// `source_file`, `start_line`) — the schema this crate's output has been payload-compatible with since
/// the retired Python Graphify CLI (ADR-0019 → ADR-0086).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphNode {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    pub start_line: i64,
}

/// One directed edge. Field set mirrors `agent-clients::GraphEdgePayload` exactly (`source`, `target`,
/// `relation`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
}

/// A resolved structural graph, canonicalised (nodes + edges sorted, deduped) so it is stable to
/// snapshot as a golden and stable to submit.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Node id for a definition: `<file>#<line>:<name>`. Line makes it unique within a file even when two
/// defs share a name (e.g. `new` on two impls); the name keeps it human-recognisable.
///
/// Shared rather than duplicated on purpose: a framework extractor emitting an edge to, say, a Spring
/// handler method has to produce a byte-identical id to the one `lci-codegraph`'s own `emit` module
/// produced for that same method — two copies of the same format string is exactly how that drifts
/// silently apart. Callers that classify definitions with a `kind`-fallback (a def with no name falls
/// back to labelling itself by its node kind) resolve that fallback themselves before calling in — e.g.
/// `lci-codegraph::graph::emit` passes `name.unwrap_or(kind)` — so this function itself stays a plain,
/// unconditional formatter.
#[must_use]
pub fn def_node_id(source_file: &str, start_line: i64, name: &str) -> String {
    format!("{source_file}#{start_line}:{name}")
}

/// Facts a framework-aware extractor contributes for ONE file, on top of the language-level symbols
/// `extract_file` already found.
///
/// ## Why `call_targets` exists at all
///
/// `emit::Classifier::is_call_target` excludes bodiless declarations from the resolver's candidate
/// pool, because a call dispatches to an implementation, never a declaration — see that function's own
/// doc comment for why treating a declaration as a candidate would turn a clean single-impl case into a
/// spurious ambiguity. Two framework constructs are genuine exceptions to "a call always needs a bodied
/// implementation to resolve to," and both are *positively provable* rather than guessed:
///
/// - A **Spring Data repository method** (e.g. `AccountRepository.findByEmail`). Spring generates the
///   implementation from the method name at runtime — no other implementation will ever exist in
///   source, by contract of the framework itself, not by inference over this repo's code.
/// - A **`@FeignClient` method**. The implementation is not merely absent from this file — it lives in
///   a *different repository* entirely, so the honest call target is not a definition at all but a
///   boundary marker node (an `external_service` [`GraphNode`]) naming the remote service.
///
/// Contributing these as ordinary call targets through `call_targets` — rather than teaching
/// `emit::Classifier` about Spring, Feign, or any other framework — is exactly what keeps `emit.rs`
/// free of framework knowledge entirely. The classifier still only ever answers "is this a definition,
/// is this a call site, what is the enclosing type scope"; a framework extractor answers a different
/// question ("is there a provable target for a call site the classifier could never resolve on its
/// own") and hands the answer across this seam instead.
///
/// ## Why the whole thing is additive
///
/// `FrameworkFacts` can only ADD nodes, edges, and call-target candidates; there is deliberately no way
/// to express "remove a candidate" or "override a resolution" through this type. A framework extractor
/// cannot weaken the resolver's precision-favouring policy (several same-named candidates with no
/// disambiguating qualifier are dropped, not fanned out) — it can only widen what the resolver is able
/// to see. That is a property worth stating explicitly, because it is what bounds the blast radius of
/// getting a framework's rules wrong: a bug in framework extraction can cause a missed edge or a
/// spurious extra candidate (falling back to the same "ambiguous, drop it" behaviour the resolver
/// already has), never a call silently rerouted to the wrong definition.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FrameworkFacts {
    /// Additive nodes — e.g. an HTTP `route`, or an `external_service` boundary marker.
    pub nodes: Vec<GraphNode>,
    /// Additive edges, already fully resolved (both endpoints are known without cross-file work).
    pub edges: Vec<GraphEdge>,
    /// Call targets the plain language rules cannot see.
    pub call_targets: Vec<FrameworkCallTarget>,
}

/// A call target only framework knowledge can justify — see [`FrameworkFacts::call_targets`] for the
/// two concrete cases this exists for (a Spring Data repository method, a `@FeignClient` boundary).
///
/// `scope`/`scope_supers` deliberately mirror the core resolver's own `Callable` fields so that
/// `resolve::pick`'s qualifier matching treats a framework-contributed target exactly like a
/// language-level one: the same "qualifier names the candidate's own scope, or one of its supers"
/// check, the same "single unqualified match resolves, several same-named matches need a qualifier to
/// narrow to one" policy. No second code path, no framework-specific special case inside the resolver
/// itself — the resolver stays oblivious to *why* a candidate exists, only that it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkCallTarget {
    /// The bare callee name a call site's `CallSite::name` is matched against, exactly like a
    /// language-level `Callable::name`.
    pub name: String,
    /// The node id this call target resolves to when picked — either a definition already emitted by
    /// the language-level extractor (a Spring Data repository method declaration) or a node contributed
    /// by [`FrameworkFacts::nodes`] (an `external_service` boundary marker).
    pub node_id: String,
    /// The enclosing type's simple name, mirroring `Callable::scope` — used only to disambiguate
    /// several same-named candidates via a qualifier.
    pub scope: Option<String>,
    /// Simple names of whatever `scope` extends/implements, mirroring `Callable::scope_supers` — lets a
    /// qualifier naming an interface/superclass count as a match against this target too.
    pub scope_supers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn def_node_id_formats_file_line_and_name() {
        assert_eq!(
            def_node_id("src/lib/math.rs", 9, "add"),
            "src/lib/math.rs#9:add"
        );
    }

    #[test]
    fn def_node_id_treats_the_name_argument_as_opaque() {
        // The kind-fallback (a nameless def labelling itself by its node kind) is a decision the
        // *caller* makes before calling in — see the doc comment. This function itself just formats
        // whatever string it is given.
        assert_eq!(def_node_id("f.rs", 1, "struct"), "f.rs#1:struct");
    }

    #[test]
    fn def_node_id_is_stable_across_repeated_calls_with_the_same_input() {
        // No counters, no UUIDs: the same input always produces the same id.
        let a = def_node_id("src/n.rs", 3, "f");
        let b = def_node_id("src/n.rs", 3, "f");
        assert_eq!(a, b);
    }

    fn sample_node(id: &str) -> GraphNode {
        GraphNode {
            node_id: id.to_string(),
            label: id.to_string(),
            source_file: "f.rs".to_string(),
            start_line: 1,
        }
    }

    fn sample_edge() -> GraphEdge {
        GraphEdge {
            source: "a".to_string(),
            target: "b".to_string(),
            relation: "calls".to_string(),
        }
    }

    #[test]
    fn graph_node_round_trips_through_json() {
        let node = sample_node("f.rs#1:add");
        let json = serde_json::to_string(&node).expect("node serialises");
        assert!(json.contains("\"node_id\":\"f.rs#1:add\""));
    }

    #[test]
    fn graph_edge_round_trips_through_json() {
        let edge = sample_edge();
        let json = serde_json::to_string(&edge).expect("edge serialises");
        assert!(json.contains("\"relation\":\"calls\""));
    }

    #[test]
    fn graph_serialises_its_nodes_and_edges() {
        let graph = Graph {
            nodes: vec![sample_node("f.rs#1:add")],
            edges: vec![sample_edge()],
        };
        let json = serde_json::to_string(&graph).expect("graph serialises");
        assert!(json.contains("\"nodes\""));
        assert!(json.contains("\"edges\""));
    }

    #[test]
    fn default_graph_has_no_nodes_or_edges() {
        let graph = Graph::default();
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
    }

    #[test]
    fn default_framework_facts_are_completely_empty() {
        // The additive-only contract starts from nothing: a framework extractor that finds no
        // framework-specific facts in a file contributes exactly nothing, never a spurious node/edge.
        let facts = FrameworkFacts::default();
        assert!(facts.nodes.is_empty());
        assert!(facts.edges.is_empty());
        assert!(facts.call_targets.is_empty());
    }

    #[test]
    fn framework_facts_carries_nodes_edges_and_call_targets_independently() {
        let facts = FrameworkFacts {
            nodes: vec![sample_node("service:payment-service")],
            edges: vec![sample_edge()],
            call_targets: vec![FrameworkCallTarget {
                name: "findByEmail".to_string(),
                node_id: "AccountRepository.java#4:findByEmail".to_string(),
                scope: Some("AccountRepository".to_string()),
                scope_supers: vec!["CrudRepository".to_string()],
            }],
        };
        assert_eq!(facts.nodes.len(), 1);
        assert_eq!(facts.edges.len(), 1);
        assert_eq!(facts.call_targets.len(), 1);
        assert_eq!(facts.call_targets[0].name, "findByEmail");
        assert_eq!(
            facts.call_targets[0].scope_supers,
            vec!["CrudRepository".to_string()]
        );
    }

    #[test]
    fn framework_facts_equality_is_structural() {
        let a = FrameworkFacts {
            nodes: vec![sample_node("n")],
            ..Default::default()
        };
        let b = FrameworkFacts {
            nodes: vec![sample_node("n")],
            ..Default::default()
        };
        assert_eq!(a, b);
    }
}
