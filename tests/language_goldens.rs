//! Per-language golden-graph harness, sibling to `tests/parity.rs` (which owns the original
//! Rust-only `sample-repo` golden and must not be touched here). Each fixture below is a small,
//! hand-readable checkout for one language (or, for `polyglot-repo`, several at once) that
//! exercises cross-file call resolution, def nesting (module/class → method), and at least one
//! genuinely ambiguous same-name call.
//!
//! Regenerate a golden intentionally with `UPDATE_GOLDEN=1 cargo test --test language_goldens`.

use std::path::{Path, PathBuf};

use lci_codegraph::{Graph, WalkOptions, walk_checkout};

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.graph.json"))
}

fn canonical_json(graph: &Graph) -> String {
    serde_json::to_string_pretty(graph).expect("graph serialises")
}

/// Walk `fixture`, and either (`UPDATE_GOLDEN=1`) write the golden, or assert the walk matches the
/// committed one exactly — the same contract as `tests/parity.rs`.
fn assert_matches_golden(fixture: &str) -> Graph {
    let options = WalkOptions::builder().build_graph(true).build();
    let out = walk_checkout(&fixture_root(fixture), &options).expect("walk fixture");
    let actual = canonical_json(&out.graph);

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let path = golden_path(fixture);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{actual}\n")).unwrap();
        eprintln!("golden updated at {}", path.display());
        return out.graph;
    }

    let path = golden_path(fixture);
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read golden {} ({e}); regenerate with UPDATE_GOLDEN=1 cargo test --test language_goldens",
            path.display()
        )
    });
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "structural graph for {fixture} drifted from the committed golden; if intended, \
         regenerate with UPDATE_GOLDEN=1 cargo test --test language_goldens"
    );
    out.graph
}

/// True iff a `calls` edge exists whose source and target live in different files — the high-value
/// case a golden must exercise (ADR-0086 R1/R5), not just intra-file resolution.
fn has_cross_file_call(g: &Graph) -> bool {
    g.edges.iter().any(|e| {
        if e.relation != "calls" {
            return false;
        }
        let src_file = g
            .nodes
            .iter()
            .find(|n| n.node_id == e.source)
            .map(|n| &n.source_file);
        let dst_file = g
            .nodes
            .iter()
            .find(|n| n.node_id == e.target)
            .map(|n| &n.source_file);
        matches!((src_file, dst_file), (Some(a), Some(b)) if a != b)
    })
}

/// True iff at least one `method` edge exists (a type container → a callable it defines) — the
/// nesting shape (module/class → method) every per-language fixture must exercise.
fn has_method_nesting(g: &Graph) -> bool {
    g.edges.iter().any(|e| e.relation == "method")
}

/// Issue #8 assertion helper: the def labelled `caller_label` must resolve to exactly one `calls`
/// edge, targeting the def labelled `target_label`. Used for the instance-call-through-a-variable
/// fixture case (`g.spin()`), which the pre-fix resolver dropped entirely for every tags language.
fn assert_resolves_to(g: &Graph, caller_label: &str, target_label: &str) {
    let caller = g
        .nodes
        .iter()
        .find(|n| n.label == caller_label)
        .unwrap_or_else(|| panic!("{caller_label} node not found; nodes = {:?}", g.nodes));
    let targets: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == caller.node_id)
        .collect();
    assert_eq!(
        targets.len(),
        1,
        "{caller_label} must resolve to exactly one target; got {targets:?}"
    );
    let dst = g
        .nodes
        .iter()
        .find(|n| n.node_id == targets[0].target)
        .expect("call target must be an emitted node");
    assert_eq!(
        dst.label, target_label,
        "{caller_label} resolved to the wrong target; got {dst:?}"
    );
}

/// The negative twin of [`assert_resolves_to`]: the def labelled `caller_label` must produce NO
/// `calls` edge — a value receiver must not turn genuine same-name ambiguity into a guess.
fn assert_call_is_dropped_as_ambiguous(g: &Graph, caller_label: &str) {
    let caller = g
        .nodes
        .iter()
        .find(|n| n.label == caller_label)
        .unwrap_or_else(|| panic!("{caller_label} node not found; nodes = {:?}", g.nodes));
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == caller.node_id),
        "{caller_label} must be dropped as ambiguous, not fanned out; edges = {:?}",
        g.edges
    );
}

// ── Python ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn python_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("python-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
}

#[test]
fn python_repo_run_bare_ambiguous_build_is_dropped() {
    let g = assert_matches_golden("python-repo");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "run_bare()")
        .expect("run_bare node");
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == run_bare.node_id),
        "bare ambiguous build() must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

#[test]
fn python_repo_instance_call_via_variable_receiver_resolves() {
    // Issue #8: `g = Gizmo(); g.spin()` — before the fix, the qualifier `g` never matched the
    // callable's declaring-type scope `Gizmo`, so the resolver's single-candidate branch rejected
    // the one correct candidate and this produced zero `calls` edges.
    let g = assert_matches_golden("python-repo");
    assert_resolves_to(&g, "run_via_variable()", "spin()");
}

#[test]
fn python_repo_ambiguous_call_via_variable_receiver_is_still_dropped() {
    // Negative twin: `l = Lefty(); l.orbit()` where `orbit` is defined on both Lefty and Righty —
    // dropping the qualifier for a value receiver must fall through to the SAME ambiguity policy
    // as a bare call, never guess.
    let g = assert_matches_golden("python-repo");
    assert_call_is_dropped_as_ambiguous(&g, "run_ambiguous_via_variable()");
}

// ── TypeScript ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn typescript_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("typescript-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
}

#[test]
fn typescript_repo_run_bare_ambiguous_build_is_dropped() {
    let g = assert_matches_golden("typescript-repo");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "runBare()")
        .expect("runBare node");
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == run_bare.node_id),
        "bare ambiguous build() must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

#[test]
fn typescript_repo_instance_call_via_variable_receiver_resolves() {
    // Issue #8: `const g = new Gizmo(); g.spin()`.
    let g = assert_matches_golden("typescript-repo");
    assert_resolves_to(&g, "runViaVariable()", "spin()");
}

#[test]
fn typescript_repo_ambiguous_call_via_variable_receiver_is_still_dropped() {
    // Negative twin: `const l = new Lefty(); l.orbit()` where `orbit` is defined on both Lefty and
    // Righty.
    let g = assert_matches_golden("typescript-repo");
    assert_call_is_dropped_as_ambiguous(&g, "runAmbiguousViaVariable()");
}

// ── JavaScript (incl. JSX) ──────────────────────────────────────────────────────────────────────

#[test]
fn javascript_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("javascript-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
    // The .jsx file's arrow component must be extracted and called, proving JSX bodies don't wreck
    // the parse for plain JavaScript.
    assert!(
        g.nodes.iter().any(|n| n.label == "Button()"),
        "jsx arrow component node; nodes = {:?}",
        g.nodes
    );
}

#[test]
fn javascript_repo_run_bare_ambiguous_build_is_dropped() {
    let g = assert_matches_golden("javascript-repo");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "runBare()")
        .expect("runBare node");
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == run_bare.node_id),
        "bare ambiguous build() must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

#[test]
fn javascript_repo_instance_call_via_variable_receiver_resolves() {
    // Issue #8: `const g = new Gizmo(); g.spin()`.
    let g = assert_matches_golden("javascript-repo");
    assert_resolves_to(&g, "runViaVariable()", "spin()");
}

#[test]
fn javascript_repo_ambiguous_call_via_variable_receiver_is_still_dropped() {
    // Negative twin: `const l = new Lefty(); l.orbit()` where `orbit` is defined on both Lefty and
    // Righty.
    let g = assert_matches_golden("javascript-repo");
    assert_call_is_dropped_as_ambiguous(&g, "runAmbiguousViaVariable()");
}

// ── TSX/JSX (React-shaped components) ──────────────────────────────────────────────────────────

#[test]
fn tsx_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("tsx-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
    // The JSX-returning arrow component must itself be a callable node reached through the TSX
    // (not plain TS) grammar.
    assert!(
        g.nodes.iter().any(|n| n.label == "App()"),
        "App component node; nodes = {:?}",
        g.nodes
    );
}

#[test]
fn tsx_repo_run_bare_ambiguous_build_is_dropped() {
    let g = assert_matches_golden("tsx-repo");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "runBare()")
        .expect("runBare node");
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == run_bare.node_id),
        "bare ambiguous build() must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

// ── Java ────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn java_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("java-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
}

#[test]
fn java_repo_run_bare_ambiguous_build_is_dropped() {
    let g = assert_matches_golden("java-repo");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "runBare()")
        .expect("runBare node");
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == run_bare.node_id),
        "bare ambiguous build() must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_repo_instance_call_via_variable_receiver_resolves() {
    // Issue #8: `Gizmo g = new Gizmo(); g.spin()` — before the fix, the qualifier `g` never
    // matched the callable's declaring-type scope `Gizmo`, so the resolver's single-candidate
    // branch rejected the one correct candidate and this produced zero `calls` edges.
    let g = assert_matches_golden("java-repo");
    assert_resolves_to(&g, "runViaVariable()", "spin()");
}

#[test]
fn java_repo_call_via_variable_receiver_is_disambiguated_by_its_declared_type() {
    // `Lefty l = new Lefty(); l.orbit()` where `orbit` is defined on BOTH Lefty and Righty. The
    // declared type resolves it to Lefty's. Asserting the specific target is the point: both
    // candidates share the label `orbit()`, so "an edge exists" would pass just as happily on a
    // mis-attribution to Righty. Only the declared-type tier can resolve this at all — dropping
    // the qualifier (issue #8's rule) leaves two candidates and nothing to choose with.
    let g = assert_matches_golden("java-repo");
    let src = g
        .nodes
        .iter()
        .find(|n| n.label == "runDisambiguatedByDeclaredType()")
        .expect("caller node");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == src.node_id)
        .collect();
    assert_eq!(calls.len(), 1, "exactly one target; got {calls:?}");
    let target = g
        .nodes
        .iter()
        .find(|n| n.node_id == calls[0].target)
        .expect("target must be an emitted node");
    assert_eq!(
        (target.source_file.as_str(), target.start_line),
        ("Lefty.java", 2),
        "must resolve to Lefty.orbit (Lefty.java:2), not Righty.orbit (Lefty.java:8); got {target:?}"
    );
}

#[test]
fn java_repo_call_via_an_interface_typed_receiver_with_two_impls_is_still_dropped() {
    // The negative twin that survives declared-type recovery: `Orbiter o` names an interface both
    // Lefty and Righty implement, so the qualifier matches BOTH through their `implements` clause.
    // Two genuine candidates, nothing to choose between them — dropped, never guessed.
    let g = assert_matches_golden("java-repo");
    assert_call_is_dropped_as_ambiguous(&g, "runAmbiguousViaInterface()");
}

// ── TypeScript: single-impl interface method (issue #5, the tags-path twin of #1) ────────────────

#[test]
fn typescript_interface_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("typescript-interface-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
}

#[test]
fn typescript_interface_repo_declaration_and_impl_are_both_nodes_but_only_one_is_a_call_target() {
    let g = assert_matches_golden("typescript-interface-repo");
    let greets: Vec<_> = g.nodes.iter().filter(|n| n.label == "greet()").collect();
    assert_eq!(
        greets.len(),
        2,
        "the interface declaration and the implementation must both be nodes; got {greets:?}"
    );
    let run = g
        .nodes
        .iter()
        .find(|n| n.label == "run()")
        .expect("run node");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "runBare()")
        .expect("runBare node");
    for caller in [run, run_bare] {
        let targets: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == caller.node_id)
            .collect();
        assert_eq!(
            targets.len(),
            1,
            "{} must resolve to exactly one target; got {targets:?}",
            caller.label
        );
        let dst = g
            .nodes
            .iter()
            .find(|n| n.node_id == targets[0].target)
            .expect("call target must be an emitted node");
        assert_eq!(
            dst.source_file, "src/english-greeter.ts",
            "{} must resolve to the implementation, not the interface declaration; edges = {:?}",
            caller.label, g.edges
        );
    }
}

// ── Java: single-impl interface method (issue #5, the tags-path twin of #1) ──────────────────────

#[test]
fn java_interface_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("java-interface-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
}

#[test]
fn java_interface_repo_declaration_and_impl_are_both_nodes_but_only_one_is_a_call_target() {
    // The actual bug (issue #5): a Java interface method with a single implementation used to
    // produce no `calls` edge at all — the declaration and the implementation both registered as
    // call targets under the bare name `greet`, so the resolver saw two candidates with no
    // disambiguating qualifier and dropped the call as ambiguous.
    let g = assert_matches_golden("java-interface-repo");
    let greets: Vec<_> = g.nodes.iter().filter(|n| n.label == "greet()").collect();
    assert_eq!(
        greets.len(),
        2,
        "the interface declaration and the implementation must both be nodes; got {greets:?}"
    );
    let run = g
        .nodes
        .iter()
        .find(|n| n.label == "run()")
        .expect("run node");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "runBare()")
        .expect("runBare node");
    for caller in [run, run_bare] {
        let targets: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.relation == "calls" && e.source == caller.node_id)
            .collect();
        assert_eq!(
            targets.len(),
            1,
            "{} must resolve to exactly one target; got {targets:?}",
            caller.label
        );
        let dst = g
            .nodes
            .iter()
            .find(|n| n.node_id == targets[0].target)
            .expect("call target must be an emitted node");
        assert_eq!(
            dst.source_file, "EnglishGreeter.java",
            "{} must resolve to the implementation, not the interface declaration; edges = {:?}",
            caller.label, g.edges
        );
    }
}

// ── Java: Spring-aware fixture (docs/design/spring-aware-graph.md) ───────────────────────────────
//
// PROVISIONAL GOLDEN — read before touching this section. This golden is a snapshot pinned to
// whatever `lci-codegraph-spring::extract_facts` produces at the moment it was last regenerated
// (`UPDATE_GOLDEN=1 cargo test --test language_goldens`), not a hand-designed target the crate is
// implemented against — the crate's own test suite (`crates/lci-codegraph-spring`) is the source of
// truth for its extraction rules, and this file just proves the *wiring* through `Indexer`/`resolve`
// is correct end to end: the sibling pass runs, its `FrameworkFacts` reach `graph::resolve`, and the
// result comes out through `graph::resolve`'s `framework` merge exactly like any other input. If
// `lci-codegraph-spring`'s extraction rules change (a route path composition fix, a new marker
// interface added to its repository allowlist, …), this golden drifting is EXPECTED — regenerate it,
// do not hand-edit the JSON to match a change made elsewhere.

#[test]
fn spring_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("spring-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
}

/// Definition-of-done check from the design doc §6: the Phase 1 vocabulary — `route`/`external_service`
/// nodes, and the `calls` edges that route through them with no resolver change — must actually show up
/// through this crate's ordinary output, not just exist in `lci-codegraph-spring`'s own unit tests.
/// Deliberately loose (existence + shape, not exact node_id/line pinning — `assert_matches_golden`
/// above already pins the byte-exact form): this test's job is to keep meaning something even as the
/// Spring crate's extraction rules evolve, by asserting the *concepts* the design doc promised are
/// present, not their current precise encoding.
#[test]
fn spring_repo_exercises_the_full_phase_1_vocabulary() {
    let g = assert_matches_golden("spring-repo");

    // `route` nodes: one per HTTP endpoint `AccountController` serves, not one for `PaymentClient`'s
    // outbound `@PostMapping` (design doc §2.1 — an outbound Feign request declaration must never be
    // mistaken for an inbound route).
    let routes: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.node_id.starts_with("route:"))
        .collect();
    assert!(
        !routes.is_empty(),
        "expected at least one route node for AccountController; nodes = {:?}",
        g.nodes
    );
    assert!(
        routes
            .iter()
            .all(|n| n.source_file == "AccountController.java"),
        "a route node must point at the handler that serves it, never at PaymentClient's outbound \
         @PostMapping; routes = {routes:?}"
    );
    // Each route dispatches to its handler via a `calls` edge (design doc §2.2: "the container
    // dispatches to the handler").
    for route in &routes {
        assert!(
            g.edges
                .iter()
                .any(|e| e.relation == "calls" && e.source == route.node_id),
            "route {} must call its handler; edges = {:?}",
            route.node_id,
            g.edges
        );
    }

    // `external_service` node: one for the `payment-service` Feign boundary, keyed by service name
    // (design doc §2.1), reached by a `calls` edge from the Feign call site.
    let service = g
        .nodes
        .iter()
        .find(|n| n.node_id == "service:payment-service")
        .expect("external_service node for payment-service");
    assert!(
        g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.target == service.node_id),
        "expected a calls edge into the payment-service boundary; edges = {:?}",
        g.edges
    );

    // Spring Data carve-out (design doc §4.3): `AccountRepository.findByEmail` has no body anywhere
    // in source, yet is a valid, reachable call target from `AccountServiceImpl.findByEmail`.
    let repo_method = g
        .nodes
        .iter()
        .find(|n| n.source_file == "AccountRepository.java" && n.label == "findByEmail()")
        .expect("AccountRepository.findByEmail node");
    assert!(
        g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.target == repo_method.node_id),
        "expected a calls edge into the bodiless Spring Data repository method; edges = {:?}",
        g.edges
    );
}

/// The design doc's central, non-Spring finding (§4.2): `AccountController.getAccount` calls
/// `accountService.findByEmail(email)` through a field typed `AccountService` — an interface with one
/// implementation, `AccountServiceImpl`. This resolves end-to-end with **zero** Spring knowledge —
/// `AccountService`/`AccountServiceImpl`/`@Service` never enter the picture — purely from two
/// already-landed, general Java-resolver facts: issue #5 (a bodiless interface declaration is a node
/// but not a call-target candidate, so `AccountService.findByEmail` stops competing with
/// `AccountServiceImpl.findByEmail` for the name) and Phase 0 (the receiver's *declared* type,
/// `AccountService`, counts as a qualifier match against a sole candidate whose scope is
/// `AccountServiceImpl`, because `AccountServiceImpl implements AccountService`). This test pins that
/// finding directly, independent of the provisional-golden caveat above: it must keep passing
/// unchanged once `lci-codegraph-spring` is implemented, since nothing about it depends on Spring
/// facts at all.
#[test]
fn spring_repo_account_controller_resolves_to_service_impl_via_phase_0_declared_type_qualifier_with_zero_spring_knowledge()
 {
    let g = assert_matches_golden("spring-repo");
    let get_account = g
        .nodes
        .iter()
        .find(|n| n.label == "getAccount()")
        .expect("AccountController.getAccount node");
    let targets: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == get_account.node_id)
        .collect();
    assert_eq!(
        targets.len(),
        1,
        "getAccount() must resolve to exactly one target; got {targets:?}"
    );
    let dst = g
        .nodes
        .iter()
        .find(|n| n.node_id == targets[0].target)
        .expect("call target must be an emitted node");
    assert_eq!(
        dst.source_file, "AccountServiceImpl.java",
        "getAccount() must resolve to the implementation, not the AccountService interface \
         declaration or the AccountRepository method of the same name; edges = {:?}",
        g.edges
    );
    assert_eq!(dst.label, "findByEmail()");
}

// ── Polyglot (Python + TypeScript + Rust + Java in one checkout) ──────────────────────────────────

#[test]
fn polyglot_repo_graph_matches_committed_golden() {
    let g = assert_matches_golden("polyglot-repo");
    assert!(has_cross_file_call(&g), "edges = {:?}", g.edges);
    assert!(has_method_nesting(&g), "edges = {:?}", g.edges);
    // Every language in the checkout produced at least one node — the walk genuinely covers all of
    // them in one pass, not just the first language it happens to see.
    for dir in ["python/", "ts/", "rust/", "java/"] {
        assert!(
            g.nodes.iter().any(|n| n.source_file.starts_with(dir)),
            "expected at least one node under {dir}; nodes = {:?}",
            g.nodes
        );
    }
}

#[test]
fn polyglot_repo_python_run_bare_ambiguous_build_is_dropped() {
    let g = assert_matches_golden("polyglot-repo");
    let run_bare = g
        .nodes
        .iter()
        .find(|n| n.label == "run_bare()" && n.source_file.starts_with("python/"))
        .expect("python run_bare node");
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == run_bare.node_id),
        "bare ambiguous build() must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

/// Sanity check independent of any golden: every fixture directory under `tests/fixtures/` this
/// suite exercises must exist and contain no `.git` (fixtures are plain checkouts, not real repos).
#[test]
fn fixtures_have_no_embedded_git_dir() {
    for name in [
        "python-repo",
        "typescript-repo",
        "javascript-repo",
        "tsx-repo",
        "java-repo",
        "typescript-interface-repo",
        "java-interface-repo",
        "spring-repo",
        "polyglot-repo",
    ] {
        let root = fixture_root(name);
        assert!(root.is_dir(), "{name} fixture must exist at {root:?}");
        assert!(
            !Path::new(&root).join(".git").exists(),
            "{name} fixture must not contain a .git directory"
        );
    }
}
