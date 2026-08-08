//! Acceptance test: the exact [`FrameworkFacts`] this crate produces for each of the six files under
//! `tests/fixtures/spring-repo/` — the same fixture set `docs/design/spring-aware-graph.md` builds its
//! whole argument on. Every fixture is read from disk with `std::fs::read_to_string`, never copied
//! into this test's source, specifically so the fixtures and the extractor cannot silently drift apart
//! (a change to one without the other fails this test, not a stale assertion that happens to still
//! compile).

use std::path::{Path, PathBuf};

use lci_codegraph_model::{FrameworkFacts, def_node_id};
use tree_sitter::Parser;

/// The fixture directory, resolved relative to this crate's own manifest — NOT relative to the
/// process's current working directory, which `cargo test` does not guarantee.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/spring-repo")
}

/// Parse `name` (a filename under the fixture directory) and extract its facts, using
/// `tests/fixtures/spring-repo/<name>` as the `source_file` — the repo-relative path a real host would
/// use, so the ids asserted below read exactly as they would in the real graph.
fn extract(name: &str) -> FrameworkFacts {
    let path = fixtures_dir().join(name);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("java grammar loads");
    let tree = parser.parse(&source, None).expect("fixture source parses");
    let source_file = format!("tests/fixtures/spring-repo/{name}");
    lci_codegraph_spring::extract_facts(&tree, &source, &source_file)
}

#[test]
fn account_java_is_a_plain_jpa_entity_and_produces_no_facts() {
    // `jakarta.persistence` alone opens the import gate, but a bare `@Entity`/`@Id`-annotated class
    // matches none of this crate's three constructs — and docs/design/spring-aware-graph.md §2.4
    // explicitly rejects a `persists` relation, so this file staying silent is the intended,
    // documented behaviour, not a gap.
    let facts = extract("Account.java");
    assert_eq!(facts, FrameworkFacts::default());
}

#[test]
fn account_service_java_has_no_spring_import_and_produces_no_facts() {
    // `AccountService` is a plain Java interface with no `org.springframework`/`jakarta.persistence`
    // import at all — the import gate must stay closed for it, regardless of anything else true about
    // the file (it is even the interface `AccountServiceImpl` implements).
    let facts = extract("AccountService.java");
    assert_eq!(facts, FrameworkFacts::default());
}

#[test]
fn account_service_impl_java_is_a_service_class_not_a_controller_and_produces_no_facts() {
    // `@Service` opens the import gate (via `org.springframework.stereotype.Service`) but is not
    // `@RestController`/`@Controller`, so `route::extract` contributes nothing; it is a
    // `class_declaration`, not an `interface_declaration`, so `feign`/`repository` never even run on
    // it. The calls this file makes (`repository.findByEmail(...)`, `paymentClient.openLedger(...)`)
    // are resolved by the CORE resolver consuming the call targets `AccountRepository.java`'s and
    // `PaymentClient.java`'s OWN facts contribute — not by anything this file's own facts say.
    let facts = extract("AccountServiceImpl.java");
    assert_eq!(facts, FrameworkFacts::default());
}

#[test]
fn account_controller_java_produces_exactly_the_two_routes_it_serves() {
    let facts = extract("AccountController.java");
    let source_file = "tests/fixtures/spring-repo/AccountController.java";

    assert_eq!(facts.call_targets, Vec::new());
    assert_eq!(facts.nodes.len(), 2);
    assert_eq!(facts.edges.len(), 2);

    // `@GetMapping("/{email}")` on `getAccount`, line 24 (1-based) — the line the `@GetMapping`
    // annotation itself sits on, since `method_declaration`'s span (what `graph::extract_file`
    // classifies as the definition) includes its `modifiers`.
    let get_route = facts
        .nodes
        .iter()
        .find(|n| n.node_id == "route:GET:/api/accounts/{email}")
        .expect("GET route node is present");
    assert_eq!(get_route.label, "GET /api/accounts/{email}");
    assert_eq!(get_route.source_file, source_file);
    assert_eq!(get_route.start_line, 24);
    let get_edge = facts
        .edges
        .iter()
        .find(|e| e.source == "route:GET:/api/accounts/{email}")
        .expect("GET route edge is present");
    assert_eq!(get_edge.relation, "calls");
    assert_eq!(get_edge.target, def_node_id(source_file, 24, "getAccount"));

    // A bare `@PostMapping` (no path argument) on `createAccount`, line 30 — the route is the
    // class-level prefix alone.
    let post_route = facts
        .nodes
        .iter()
        .find(|n| n.node_id == "route:POST:/api/accounts")
        .expect("POST route node is present");
    assert_eq!(post_route.label, "POST /api/accounts");
    assert_eq!(post_route.source_file, source_file);
    assert_eq!(post_route.start_line, 30);
    let post_edge = facts
        .edges
        .iter()
        .find(|e| e.source == "route:POST:/api/accounts")
        .expect("POST route edge is present");
    assert_eq!(post_edge.relation, "calls");
    assert_eq!(
        post_edge.target,
        def_node_id(source_file, 30, "createAccount")
    );
}

#[test]
fn account_repository_java_keeps_its_bodiless_derived_query_as_a_call_target() {
    let facts = extract("AccountRepository.java");
    let source_file = "tests/fixtures/spring-repo/AccountRepository.java";

    // No route/service nodes — a repository interface contributes only call targets.
    assert!(facts.nodes.is_empty());
    assert!(facts.edges.is_empty());

    assert_eq!(facts.call_targets.len(), 1);
    let target = &facts.call_targets[0];
    assert_eq!(target.name, "findByEmail");
    // `findByEmail` is declared on line 15 (1-based) — no annotation precedes it in this fixture, so
    // the method_declaration's own span starts right at its return type.
    assert_eq!(target.node_id, def_node_id(source_file, 15, "findByEmail"));
    assert_eq!(target.scope.as_deref(), Some("AccountRepository"));
    assert_eq!(target.scope_supers, vec!["JpaRepository".to_string()]);
}

#[test]
fn payment_client_java_is_an_external_service_with_no_route_node() {
    let facts = extract("PaymentClient.java");
    let source_file = "tests/fixtures/spring-repo/PaymentClient.java";

    // The headline fixture for the declaration-vs-implementation distinction: `@PostMapping` on a
    // `@FeignClient` INTERFACE must never become a `route` node.
    assert!(
        facts.nodes.iter().all(|n| !n.node_id.starts_with("route:")),
        "PaymentClient.java must never produce a route node: {:?}",
        facts.nodes
    );
    assert!(facts.edges.is_empty());

    assert_eq!(facts.nodes.len(), 1);
    let service = &facts.nodes[0];
    assert_eq!(service.node_id, "service:payment-service");
    assert_eq!(service.label, "payment-service");
    assert_eq!(service.source_file, source_file);
    // `@FeignClient(name = "payment-service")` sits on line 10 (1-based) — the
    // `interface_declaration`'s own span starts there, since its `modifiers` child is part of it.
    assert_eq!(service.start_line, 10);

    assert_eq!(facts.call_targets.len(), 1);
    let target = &facts.call_targets[0];
    assert_eq!(target.name, "openLedger");
    assert_eq!(target.node_id, "service:payment-service");
    assert_eq!(target.scope.as_deref(), Some("PaymentClient"));
    assert!(target.scope_supers.is_empty());
}
