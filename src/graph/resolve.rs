//! The cross-file name resolver: turns every file's recorded call sites into `calls` edges and returns
//! the canonicalised whole-repo [`Graph`]. Resolution policy (precision-favouring, ADR-0086 R5):
//! same-file definitions win; otherwise the global table is consulted. In **both** tables a call
//! resolves only to a **single** matching definition — a bare name matching several same-named defs is
//! **dropped and counted, not fanned out** to every candidate ([`pick`]); a path qualifier (`A::new`)
//! is used solely as a tiebreaker to single out the right one when there is genuine ambiguity.
//!
//! Since Phase 0 (docs/design/spring-aware-graph.md §4.2, item 2), a qualifier doesn't have to name a
//! candidate's scope *exactly* to count as a match — naming one of the candidate's supers
//! (`extends`/`implements`, Java only — [`super::callee::supertype_names`]) is equally valid
//! ([`qualifier_matches`]). This is what lets an interface-typed qualifier match its sole concrete
//! implementation instead of reading as a contradiction; several genuine implementations still leave
//! [`pick`] at [`Pick::Ambiguous`] — this loosens what counts as a *match*, not the "never fan out"
//! policy itself.
//!
//! [`resolve`] also merges in whatever a framework-extractor crate contributed
//! ([`lci_codegraph_model::FrameworkFacts`], docs/design/spring-aware-graph.md §3.3) — additive
//! nodes/edges, plus call-target candidates that join the **global** callable table only (see the
//! doc comment where they're merged, in [`resolve`] itself, for why). This module still has no idea
//! *why* a `FrameworkCallTarget` exists — it is not "Spring", it is just one more candidate `pick`
//! can match a call site against, through the exact same `scope`/`scope_supers` fields a
//! language-level [`Callable`] carries.

use std::collections::HashMap;
use std::sync::Arc;

use lci_codegraph_model::FrameworkFacts;

use super::{Callable, FileSymbols, Graph, GraphEdge, GraphNode};

/// Outcome of resolving one call against a candidate set.
enum Pick<'a> {
    /// Exactly one definition matched — emit the edge.
    One(&'a str),
    /// Several same-named defs and no qualifier singles one out — drop and count (never fan out).
    Ambiguous,
    /// Nothing matched here — try the next table (or count as unresolved).
    None,
}

/// True when qualifier `q` is consistent with candidate `c`: either it names `c`'s own scope exactly,
/// or it names one of `c`'s supers — an interface/superclass `c`'s enclosing type
/// `extends`/`implements` (Phase 0, docs/design/spring-aware-graph.md §4.2, item 2). The second half is
/// what lets `accountService.findById()` — declared-type qualifier `AccountService` — match a sole
/// candidate whose `scope` is `AccountServiceImpl`: `AccountServiceImpl implements AccountService`, so
/// the qualifier names a super, not a different, contradicting type.
fn qualifier_matches(c: &Callable, q: &str) -> bool {
    c.scope.as_deref() == Some(q) || c.scope_supers.iter().any(|s| s.as_str() == q)
}

/// Resolve a call against `candidates` (all defs sharing the callee's bare name). A single candidate
/// resolves unless a **type** qualifier positively contradicts it (a *module* qualifier — the
/// candidate has no type scope — still matches, so `math::add` resolves to a free `add`; a qualifier
/// naming the candidate's scope OR one of its supers, [`qualifier_matches`], also still matches).
/// Several candidates are [`Pick::Ambiguous`] unless the qualifier singles exactly one out — we never
/// emit an edge to every same-named def (the same-file fan-out bug). Two-or-more candidates *all*
/// matching via scope/supers after narrowing stays [`Pick::Ambiguous`] too: several genuine
/// implementations of one interface is a real ambiguity Spring itself would refuse to guess through
/// (`@Primary`/`@Qualifier` territory, out of scope here — see the design doc §4.3), not something this
/// resolver invents an answer for.
fn pick<'a>(candidates: Option<&[&'a Callable]>, qualifier: Option<&str>) -> Pick<'a> {
    let candidates = match candidates {
        Some(c) if !c.is_empty() => c,
        _ => return Pick::None,
    };
    if let [only] = candidates {
        if let Some(q) = qualifier
            && only.scope.is_some()
            && !qualifier_matches(only, q)
        {
            return Pick::None;
        }
        return Pick::One(only.node_id.as_str());
    }
    // Genuine same-name ambiguity: a type qualifier is the only thing that can break it.
    if let Some(q) = qualifier {
        let mut narrowed = candidates.iter().filter(|c| qualifier_matches(c, q));
        if let Some(first) = narrowed.next()
            && narrowed.next().is_none()
        {
            return Pick::One(first.node_id.as_str());
        }
    }
    Pick::Ambiguous
}

/// Resolve every file's call sites into `calls` edges with cross-file resolution, merge in whatever a
/// framework extractor contributed, and return the canonicalised whole-repo [`Graph`].
///
/// `framework` is one [`FrameworkFacts`] per file a caller ran a framework extractor over (today:
/// Java files, via `lci-codegraph-spring` — see `src/input.rs`). An empty vec reproduces this
/// function's pre-framework behaviour exactly: every non-Java caller, and every Java repo nothing
/// framework-specific was extracted for, is unaffected by construction, not by a flag.
#[must_use]
pub fn resolve(files: Vec<FileSymbols>, framework: Vec<FrameworkFacts>) -> Graph {
    // Framework call targets, converted once into the same `Callable` shape the language-level
    // resolver already matches candidates with. `FrameworkCallTarget::scope`/`scope_supers` mirror
    // `Callable`'s fields exactly (see both types' doc comments), so `pick`'s qualifier matching
    // treats a framework-contributed target identically to a language-level one — no second code
    // path, no framework-specific branch anywhere below this point.
    let framework_callables: Vec<Callable> = framework
        .iter()
        .flat_map(|f| f.call_targets.iter())
        .map(|ct| Callable {
            name: ct.name.clone(),
            node_id: ct.node_id.clone(),
            scope: ct.scope.clone(),
            scope_supers: Arc::from(ct.scope_supers.clone()),
        })
        .collect();

    // Global callable table: name → all callables across files (for cross-file resolution).
    let mut global: HashMap<&str, Vec<&Callable>> = HashMap::new();
    for f in &files {
        for c in &f.callables {
            global.entry(c.name.as_str()).or_default().push(c);
        }
    }
    // Framework-contributed targets join the GLOBAL table only — never any file's LOCAL table. A
    // Spring Data repository method or a `@FeignClient` boundary is cross-file by nature: the caller
    // is essentially never in the same file as the interface declaring it, unlike a plain method call
    // where "does this file define it too" is a meaningful question. The local table exists
    // specifically to let a file's OWN definitions win over anything else sharing that name — a rule
    // about that file's own definitions, which a framework target was never "found in" to begin with.
    // Keeping it global-only also means a framework target can never shadow a genuine local
    // definition: local-first resolution below still tries the local table first, unconditionally.
    for c in &framework_callables {
        global.entry(c.name.as_str()).or_default().push(c);
    }

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut ambiguous = 0usize;
    let mut unresolved = 0usize;

    for f in &files {
        nodes.extend(f.nodes.iter().cloned());
        edges.extend(f.contains.iter().cloned());

        // Per-file callable table for same-file-first resolution.
        let mut local: HashMap<&str, Vec<&Callable>> = HashMap::new();
        for c in &f.callables {
            local.entry(c.name.as_str()).or_default().push(c);
        }

        for call in &f.calls {
            let qualifier = call.qualifier.as_deref();
            let local_pick = pick(local.get(call.name.as_str()).map(Vec::as_slice), qualifier);
            // Same-file definitions win on a clean single hit. Otherwise — no local match, OR a local
            // set too ambiguous to attribute — consult the global table, where a qualifier may still
            // single out exactly one definition (e.g. locals `A::new`+`B::new` don't shadow a global
            // `C::new()` call). A local ambiguity is only *counted* ambiguous if the global fails too.
            let local_ambiguous = matches!(local_pick, Pick::Ambiguous);
            let target = match local_pick {
                Pick::One(id) => Some(id.to_string()),
                Pick::None | Pick::Ambiguous => {
                    match pick(global.get(call.name.as_str()).map(Vec::as_slice), qualifier) {
                        Pick::One(id) => Some(id.to_string()),
                        Pick::Ambiguous => {
                            ambiguous += 1;
                            None
                        }
                        Pick::None => {
                            if local_ambiguous {
                                ambiguous += 1;
                            } else {
                                unresolved += 1;
                            }
                            None
                        }
                    }
                }
            };
            if let Some(target) = target {
                edges.push(GraphEdge {
                    source: call.caller.clone(),
                    target,
                    relation: "calls".to_string(),
                });
            }
        }
    }

    // Framework nodes/edges are additive and already fully resolved (both endpoints known without
    // any cross-file work) — see `FrameworkFacts`'s own doc comment. Appended after every file's
    // `calls` edges are in, alongside the language-level nodes/edges above, so they go through the
    // exact same canonicalisation as everything else.
    let framework_node_count: usize = framework.iter().map(|f| f.nodes.len()).sum();
    let framework_edge_count: usize = framework.iter().map(|f| f.edges.len()).sum();
    for f in &framework {
        nodes.extend(f.nodes.iter().cloned());
        edges.extend(f.edges.iter().cloned());
    }

    // Canonicalise: sort + dedup so output is deterministic (stable goldens, stable submit).
    // `dedup_colliding_node_ids` folds in a second pass sort+dedup already can't do on its own — see
    // its own doc comment — so plain `nodes.sort()/nodes.dedup()` is subsumed by it.
    let nodes = dedup_colliding_node_ids(nodes);
    edges.sort();
    edges.dedup();

    tracing::debug!(
        nodes = nodes.len(),
        edges = edges.len(),
        ambiguous_calls = ambiguous,
        unresolved_calls = unresolved,
        framework_nodes = framework_node_count,
        framework_edges = framework_edge_count,
        "codegraph: resolved structural graph"
    );

    Graph { nodes, edges }
}

/// Collapse any `GraphNode`s that still share a `node_id` after the natural sort, keeping exactly
/// one per id.
///
/// Plain `sort()` + `dedup()` only removes **exact** duplicates — same `label`/`source_file`/
/// `start_line` too — which is all the language-level extractors ever produce: a def's id already
/// embeds its own file and line, so two *different* definitions can never collide on it. A
/// framework-contributed node can, legitimately: the same `@FeignClient(name = "payment-service")`
/// declared in two files, or two controllers both mapping `GET /api/accounts/{id}` (a real app bug,
/// but not one this crate gets to crash on) both mint the same `node_id` with genuinely different
/// `source_file`/`start_line` — two structurally different `GraphNode`s that an exact-equality dedup
/// cannot tell apart as "the same thing", and `tests/container_repos.rs` asserts `node_id`s are
/// unique.
///
/// The winner is deterministic — lowest `(source_file, start_line)` — rather than "whichever survived
/// merge order" or "first seen": an arbitrary winner would make the emitted graph depend on the order
/// files happened to be pushed/walked in, which breaks both the committed golden (a diff with no
/// underlying code change) and the "two runs over the same checkout produce byte-identical JSON"
/// determinism guarantee `tests/container_repos.rs` also checks.
fn dedup_colliding_node_ids(mut nodes: Vec<GraphNode>) -> Vec<GraphNode> {
    nodes.sort_by(|a, b| {
        a.node_id
            .cmp(&b.node_id)
            .then_with(|| a.source_file.cmp(&b.source_file))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    nodes.dedup_by(|a, b| a.node_id == b.node_id);
    nodes
}

#[cfg(test)]
mod tests {
    //! Unit tests over the resolver internals: [`pick`]'s tie-break policy in isolation (no parser,
    //! no tags — just hand-built [`Callable`] candidate sets), plus [`resolve`]'s canonicalisation
    //! (dedup + sort) exercised directly against hand-built [`FileSymbols`]. End-to-end
    //! parse-through-resolve behaviour lives in `graph::tests`; these tests isolate the resolver logic
    //! itself so a regression here points straight at [`pick`]/[`resolve`], not the whole pipeline.
    use std::sync::Arc;

    use super::*;
    use crate::graph::CallSite;

    /// Test-only [`Callable`] builder: a bare name + node id + optional scope, no supers. Covers
    /// every pre-Phase-0 test case, where `scope_supers` never enters the picture.
    fn callable(name: &str, node_id: &str, scope: Option<&str>) -> Callable {
        Callable {
            name: name.to_string(),
            node_id: node_id.to_string(),
            scope: scope.map(str::to_string),
            scope_supers: Arc::from([]),
        }
    }

    /// Test-only [`Callable`] builder with an explicit `scope_supers` list, for the Phase 0
    /// supertype-matching tests below.
    fn callable_with_supers(name: &str, node_id: &str, scope: &str, supers: &[&str]) -> Callable {
        Callable {
            name: name.to_string(),
            node_id: node_id.to_string(),
            scope: Some(scope.to_string()),
            scope_supers: supers.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// `resolve` with an empty `framework` vec — every test below predates `FrameworkFacts` and
    /// exercises the language-only path, so this keeps the churn from the signature change to one
    /// line per call site rather than a `Vec::new()` repeated at every call.
    fn resolve_no_framework(files: Vec<FileSymbols>) -> Graph {
        resolve(files, Vec::new())
    }

    #[test]
    fn pick_with_no_candidates_returns_none() {
        assert!(matches!(pick(None, None), Pick::None));
    }

    #[test]
    fn pick_with_empty_candidate_slice_returns_none() {
        let empty: Vec<&Callable> = Vec::new();
        assert!(matches!(pick(Some(&empty), None), Pick::None));
    }

    #[test]
    fn pick_single_candidate_with_no_qualifier_resolves() {
        let c = callable("f", "id1", None);
        let cands = [&c];
        assert!(matches!(pick(Some(&cands), None), Pick::One(id) if id == "id1"));
    }

    #[test]
    fn pick_single_candidate_with_matching_type_qualifier_resolves() {
        // `A::new()` against a single `new` scoped to `A` — the qualifier agrees, so it resolves.
        let c = callable("new", "a.rs#1:new", Some("A"));
        let cands = [&c];
        assert!(matches!(pick(Some(&cands), Some("A")), Pick::One(id) if id == "a.rs#1:new"));
    }

    #[test]
    fn pick_single_candidate_with_mismatched_type_qualifier_is_none() {
        // `B::new()` against a single `new` scoped to `A` — a *type* qualifier that positively
        // contradicts the only candidate must not resolve to it.
        let c = callable("new", "a.rs#1:new", Some("A"));
        let cands = [&c];
        assert!(matches!(pick(Some(&cands), Some("B")), Pick::None));
    }

    #[test]
    fn pick_single_candidate_with_module_qualifier_and_no_scope_still_resolves() {
        // `math::add()` against a free function `add` (no type scope) — a *module* qualifier doesn't
        // contradict a scopeless candidate, so it still resolves (the doc example on `pick`).
        let c = callable("add", "math.rs#1:add", None);
        let cands = [&c];
        assert!(matches!(pick(Some(&cands), Some("math")), Pick::One(id) if id == "math.rs#1:add"));
    }

    #[test]
    fn pick_single_candidate_whose_supers_include_the_qualifier_resolves() {
        // Phase 0 (docs/design/spring-aware-graph.md §4.2, item 2): `accountService.findById()`'s
        // recovered declared-type qualifier is `AccountService`; the sole candidate's scope is
        // `AccountServiceImpl`, which `implements AccountService`. The qualifier names a super, not
        // a contradicting type, so this must still resolve — the headline fix this PR ships.
        let c = callable_with_supers(
            "findById",
            "AccountServiceImpl.java#1:findById",
            "AccountServiceImpl",
            &["AccountService"],
        );
        let cands = [&c];
        assert!(
            matches!(pick(Some(&cands), Some("AccountService")), Pick::One(id) if id == "AccountServiceImpl.java#1:findById")
        );
    }

    #[test]
    fn pick_single_candidate_whose_supers_do_not_include_the_qualifier_is_none() {
        // The converse: a qualifier that names neither the candidate's own scope nor any of its
        // supers is a genuine contradiction — precision preserved, no edge guessed.
        let c = callable_with_supers(
            "findById",
            "AccountServiceImpl.java#1:findById",
            "AccountServiceImpl",
            &["AccountService"],
        );
        let cands = [&c];
        assert!(matches!(
            pick(Some(&cands), Some("SomethingElse")),
            Pick::None
        ));
    }

    #[test]
    fn pick_multiple_candidates_with_no_qualifier_is_ambiguous() {
        let a = callable("new", "a", Some("A"));
        let b = callable("new", "b", Some("B"));
        let cands = [&a, &b];
        assert!(matches!(pick(Some(&cands), None), Pick::Ambiguous));
    }

    #[test]
    fn pick_multiple_candidates_qualifier_singles_out_exactly_one() {
        let a = callable("new", "a", Some("A"));
        let b = callable("new", "b", Some("B"));
        let cands = [&a, &b];
        assert!(matches!(pick(Some(&cands), Some("A")), Pick::One(id) if id == "a"));
    }

    #[test]
    fn pick_multiple_candidates_qualifier_matching_none_is_ambiguous() {
        let a = callable("new", "a", Some("A"));
        let b = callable("new", "b", Some("B"));
        let cands = [&a, &b];
        assert!(matches!(pick(Some(&cands), Some("C")), Pick::Ambiguous));
    }

    #[test]
    fn pick_multiple_candidates_qualifier_matching_more_than_one_is_still_ambiguous() {
        // Two `new`s both scoped to `A` (e.g. two impls somehow sharing a scope name) — the qualifier
        // narrows to more than one, so it must stay ambiguous, never guess the first.
        let a1 = callable("new", "a1", Some("A"));
        let a2 = callable("new", "a2", Some("A"));
        let cands = [&a1, &a2];
        assert!(matches!(pick(Some(&cands), Some("A")), Pick::Ambiguous));
    }

    #[test]
    fn pick_multiple_candidates_qualifier_narrows_via_a_super_to_exactly_one() {
        // Only one of two same-named candidates implements the qualifier's interface — the
        // supers-aware narrowing must single it out, exactly like an exact-scope match would.
        let a = callable_with_supers("run", "a", "A", &["Task"]);
        let b = callable("run", "b", Some("B"));
        let cands = [&a, &b];
        assert!(matches!(pick(Some(&cands), Some("Task")), Pick::One(id) if id == "a"));
    }

    #[test]
    fn pick_multiple_candidates_qualifier_matching_more_than_one_via_supers_is_still_ambiguous() {
        // Two genuine implementations of the same interface — Spring itself would refuse to start
        // rather than guess which bean wins (design doc §4.3, `@Primary`/`@Qualifier`, out of scope
        // here). The resolver must mirror that: stay Ambiguous, never guess one.
        let a = callable_with_supers("run", "a", "A", &["Task"]);
        let b = callable_with_supers("run", "b", "B", &["Task"]);
        let cands = [&a, &b];
        assert!(matches!(pick(Some(&cands), Some("Task")), Pick::Ambiguous));
    }

    #[test]
    fn resolve_dedups_identical_call_sites_into_a_single_edge() {
        // Two identical `CallSite`s attributed to the same caller (e.g. `helper(); helper();`) must
        // collapse to one `calls` edge, not two.
        let mut fs = FileSymbols::default();
        fs.nodes.push(GraphNode {
            node_id: "f.rs".to_string(),
            label: "f.rs".to_string(),
            source_file: "f.rs".to_string(),
            start_line: 1,
        });
        fs.callables.push(callable("helper", "f.rs#1:helper", None));
        for _ in 0..2 {
            fs.calls.push(CallSite {
                caller: "f.rs#2:caller".to_string(),
                name: "helper".to_string(),
                qualifier: None,
            });
        }
        let g = resolve_no_framework(vec![fs]);
        let calls: Vec<_> = g.edges.iter().filter(|e| e.relation == "calls").collect();
        assert_eq!(
            calls.len(),
            1,
            "duplicate identical call sites must dedup to one edge; got {calls:?}"
        );
    }

    #[test]
    fn resolve_sorts_and_dedups_nodes_for_deterministic_output() {
        let n_b = GraphNode {
            node_id: "b.rs".to_string(),
            label: "b.rs".to_string(),
            source_file: "b.rs".to_string(),
            start_line: 1,
        };
        let n_a = GraphNode {
            node_id: "a.rs".to_string(),
            label: "a.rs".to_string(),
            source_file: "a.rs".to_string(),
            start_line: 1,
        };
        let mut fs = FileSymbols::default();
        fs.nodes.push(n_b.clone());
        fs.nodes.push(n_a.clone());
        fs.nodes.push(n_b.clone()); // duplicate, out of order
        let g = resolve_no_framework(vec![fs]);
        assert_eq!(
            g.nodes,
            vec![n_a, n_b],
            "nodes must be sorted by node_id and deduped regardless of input order"
        );
    }

    #[test]
    fn resolve_unresolvable_callee_produces_no_edge() {
        // A call to a name with no matching def anywhere (local or global) is dropped silently — no
        // guessed edge, and it must not be conflated with the `Ambiguous` counting path.
        let mut fs = FileSymbols::default();
        fs.nodes.push(GraphNode {
            node_id: "f.rs".to_string(),
            label: "f.rs".to_string(),
            source_file: "f.rs".to_string(),
            start_line: 1,
        });
        fs.calls.push(CallSite {
            caller: "f.rs#1:caller".to_string(),
            name: "nowhere_defined".to_string(),
            qualifier: None,
        });
        let g = resolve_no_framework(vec![fs]);
        assert!(
            !g.edges.iter().any(|e| e.relation == "calls"),
            "an unresolvable callee must not produce a calls edge; got {:?}",
            g.edges
        );
    }

    // ── FrameworkFacts merge ────────────────────────────────────────────────────────────────────

    use lci_codegraph_model::FrameworkCallTarget;

    fn framework_node(id: &str) -> GraphNode {
        GraphNode {
            node_id: id.to_string(),
            label: id.to_string(),
            source_file: "PaymentClient.java".to_string(),
            start_line: 1,
        }
    }

    #[test]
    fn resolve_appends_framework_nodes_and_edges_additively() {
        let facts = FrameworkFacts {
            nodes: vec![framework_node("service:payment-service")],
            edges: vec![GraphEdge {
                source: "AccountServiceImpl.java#1:create".to_string(),
                target: "service:payment-service".to_string(),
                relation: "calls".to_string(),
            }],
            call_targets: vec![],
        };
        let g = resolve(vec![FileSymbols::default()], vec![facts]);
        assert!(
            g.nodes
                .iter()
                .any(|n| n.node_id == "service:payment-service"),
            "framework node must be appended; nodes = {:?}",
            g.nodes
        );
        assert!(
            g.edges.iter().any(|e| e.relation == "calls"
                && e.source == "AccountServiceImpl.java#1:create"
                && e.target == "service:payment-service"),
            "framework edge must be appended; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn resolve_matches_a_call_site_against_a_framework_call_target_through_the_global_table() {
        // The Spring Data shape: `AccountRepository.findByEmail` has no bodied implementation
        // anywhere, so the only candidate is the framework-contributed target. No qualifier needed —
        // a single candidate with no scope resolves unconditionally, exactly like a language-level
        // free function would.
        let mut fs = FileSymbols::default();
        fs.calls.push(CallSite {
            caller: "AccountServiceImpl.java#1:findByEmail".to_string(),
            name: "findByEmail".to_string(),
            qualifier: None,
        });
        let facts = FrameworkFacts {
            nodes: vec![],
            edges: vec![],
            call_targets: vec![FrameworkCallTarget {
                name: "findByEmail".to_string(),
                node_id: "AccountRepository.java#8:findByEmail".to_string(),
                scope: Some("AccountRepository".to_string()),
                scope_supers: vec!["CrudRepository".to_string()],
            }],
        };
        let g = resolve(vec![fs], vec![facts]);
        assert!(
            g.edges.iter().any(|e| e.relation == "calls"
                && e.source == "AccountServiceImpl.java#1:findByEmail"
                && e.target == "AccountRepository.java#8:findByEmail"),
            "call site must resolve to the framework call target; edges = {:?}",
            g.edges
        );
    }

    #[test]
    fn resolve_never_lets_a_framework_call_target_shadow_a_same_file_local_definition() {
        // A framework target joins the GLOBAL table only. A local, unqualified definition sharing the
        // same bare name must still win outright — the local table is tried first and short-circuits
        // before the global (framework-augmented) table is ever consulted for this call.
        let mut fs = FileSymbols::default();
        fs.callables.push(callable(
            "findByEmail",
            "AccountServiceImpl.java#1:findByEmail",
            None,
        ));
        fs.calls.push(CallSite {
            caller: "AccountServiceImpl.java#9:caller".to_string(),
            name: "findByEmail".to_string(),
            qualifier: None,
        });
        let facts = FrameworkFacts {
            nodes: vec![],
            edges: vec![],
            call_targets: vec![FrameworkCallTarget {
                name: "findByEmail".to_string(),
                node_id: "AccountRepository.java#8:findByEmail".to_string(),
                scope: Some("AccountRepository".to_string()),
                scope_supers: vec![],
            }],
        };
        let g = resolve(vec![fs], vec![facts]);
        let targets: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == "AccountServiceImpl.java#9:caller")
            .collect();
        assert_eq!(
            targets.len(),
            1,
            "exactly one target expected; got {targets:?}"
        );
        assert_eq!(
            targets[0].target, "AccountServiceImpl.java#1:findByEmail",
            "the same-file local definition must win over the global-only framework target"
        );
    }

    #[test]
    fn empty_framework_vec_reproduces_pre_framework_behaviour_exactly() {
        // `resolve_no_framework` (used by every pre-existing test above) is just `resolve(files,
        // Vec::new())` — pin that equivalence explicitly, once, rather than only relying on it
        // implicitly through the helper.
        fn make_fs() -> FileSymbols {
            let mut fs = FileSymbols::default();
            fs.nodes.push(GraphNode {
                node_id: "f.rs".to_string(),
                label: "f.rs".to_string(),
                source_file: "f.rs".to_string(),
                start_line: 1,
            });
            fs.callables.push(callable("helper", "f.rs#1:helper", None));
            fs.calls.push(CallSite {
                caller: "f.rs#2:caller".to_string(),
                name: "helper".to_string(),
                qualifier: None,
            });
            fs
        }
        assert_eq!(
            resolve_no_framework(vec![make_fs()]),
            resolve(vec![make_fs()], Vec::new())
        );
    }

    #[test]
    fn dedup_colliding_node_ids_keeps_the_lowest_source_file_and_start_line_deterministically() {
        // Two framework nodes minted for the same `@FeignClient(name = "payment-service")` declared
        // in two different files — same node_id, genuinely different source_file/start_line, which
        // plain exact-equality dedup cannot collapse (see the function's own doc comment).
        let winner = GraphNode {
            node_id: "service:payment-service".to_string(),
            label: "payment-service".to_string(),
            source_file: "AccountServiceImpl.java".to_string(),
            start_line: 5,
        };
        let loser = GraphNode {
            node_id: "service:payment-service".to_string(),
            label: "payment-service".to_string(),
            source_file: "PaymentClient.java".to_string(),
            start_line: 9,
        };
        // Fed in the "wrong" order on purpose — the winner is chosen by (source_file, start_line),
        // not by input order.
        let deduped = dedup_colliding_node_ids(vec![loser.clone(), winner.clone()]);
        assert_eq!(
            deduped,
            vec![winner],
            "the node with the lexicographically-lowest (source_file, start_line) must win, \
             regardless of input order; got {deduped:?}"
        );
    }

    #[test]
    fn dedup_colliding_node_ids_is_a_no_op_for_already_unique_ids() {
        let a = framework_node("service:a");
        let mut b = framework_node("service:b");
        b.source_file = "Other.java".to_string();
        let deduped = dedup_colliding_node_ids(vec![a.clone(), b.clone()]);
        assert_eq!(deduped, vec![a, b]);
    }

    #[test]
    fn resolve_collapses_colliding_framework_node_ids_from_two_files_into_one() {
        // End-to-end: two files each contribute a `FrameworkFacts` node with the same node_id (e.g.
        // two `@FeignClient(name = "payment-service")` declarations) — `resolve`'s output must carry
        // exactly one node for that id, chosen deterministically, never two.
        let facts_a = FrameworkFacts {
            nodes: vec![GraphNode {
                node_id: "service:payment-service".to_string(),
                label: "payment-service".to_string(),
                source_file: "b/PaymentClient.java".to_string(),
                start_line: 1,
            }],
            edges: vec![],
            call_targets: vec![],
        };
        let facts_b = FrameworkFacts {
            nodes: vec![GraphNode {
                node_id: "service:payment-service".to_string(),
                label: "payment-service".to_string(),
                source_file: "a/PaymentClient.java".to_string(),
                start_line: 1,
            }],
            edges: vec![],
            call_targets: vec![],
        };
        let g = resolve(
            vec![FileSymbols::default(), FileSymbols::default()],
            vec![facts_a, facts_b],
        );
        let matches: Vec<_> = g
            .nodes
            .iter()
            .filter(|n| n.node_id == "service:payment-service")
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "colliding framework node_ids must collapse to exactly one node; got {matches:?}"
        );
        assert_eq!(
            matches[0].source_file, "a/PaymentClient.java",
            "the lexicographically-lower source_file must be the deterministic winner"
        );
    }
}
