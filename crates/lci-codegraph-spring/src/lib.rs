//! Spring-aware structural facts, extracted from a Java file that has **already been parsed** by the
//! core crate.
//!
//! This is a *sibling* pass, not a replacement for anything: `lci-codegraph`'s `graph::extract_file`
//! finds definitions and call sites exactly as it always has, and this crate walks the same
//! `tree_sitter::Tree` a second time looking at node kinds that pass never inspects
//! (`marker_annotation`, `annotation`, `super_interfaces`, `type_arguments`). Nothing here changes
//! how a definition is discovered — only *what else is true* about one already discovered. See
//! `docs/design/spring-aware-graph.md` §3.1/§3.2 for why this is a sibling module rather than a new
//! `GraphStrategy` or a `Classifier` variant.
//!
//! # What this crate is allowed to conclude
//!
//! Everything below is *syntactically provable* from one file's AST. Where Spring's real behaviour
//! depends on information that is not in the source — XML bean definitions, `@ComponentScan`
//! filters, `@Profile`/`@ConditionalOnProperty` — this crate stays silent rather than guessing
//! (§5.3). That is the same precision-favouring posture the core resolver already takes, applied one
//! layer up; a confident wrong answer about wiring is worse than no answer, because it looks
//! authoritative.
//!
//! The output type, [`FrameworkFacts`], is additive-only by construction: it can contribute nodes,
//! edges, and call-target candidates, and it has no way to express "drop this candidate" or
//! "override that resolution". Getting Spring wrong can therefore widen what the graph sees, but
//! never weaken the guarantees the core resolver makes.

mod annotation;
mod feign;
mod java_type;
mod repository;
mod route;

use lci_codegraph_model::FrameworkFacts;
use tree_sitter::{Node, Tree};

/// Extract every Spring fact this crate knows how to prove from one parsed Java file.
///
/// `tree` must be a `tree-sitter-java` parse of `source`; `source_file` is the repo-relative,
/// forward-slashed path the core crate uses as a node id, so ids minted here line up byte-for-byte
/// with the ones `graph::extract_file` minted for the same file.
///
/// Returns [`FrameworkFacts::default()`] — costing one scan of the file's `import_declaration` list
/// and nothing more — for any file that imports neither `org.springframework` nor
/// `jakarta.persistence`. That gate is a genuine content check, not a disabled-by-default feature
/// flag: a non-Spring Java repo pays for one import scan per file and no annotation walk at all.
#[must_use]
pub fn extract_facts(tree: &Tree, source: &str, source_file: &str) -> FrameworkFacts {
    let bytes = source.as_bytes();
    let root = tree.root_node();
    if !imports_spring_or_jakarta(&root, bytes) {
        return FrameworkFacts::default();
    }
    let mut facts = FrameworkFacts::default();
    walk(&root, bytes, source_file, &mut facts);
    facts
}

/// The import gate: true iff some TOP-LEVEL `import_declaration`'s text contains
/// `org.springframework` or `jakarta.persistence`. A plain substring check on the whole import
/// statement's text (rather than parsing it into segments) is deliberate — it is cheap, and it can
/// only ever be too eager (matching, say, an unrelated `org.springframework.example.Foo` import in a
/// file that turns out to use none of the constructs this crate recognises), never too silent; the
/// annotation walk that follows still finds nothing to report either way.
fn imports_spring_or_jakarta(root: &Node, bytes: &[u8]) -> bool {
    let mut cursor = root.walk();
    root.children(&mut cursor)
        .filter(|c| c.kind() == "import_declaration")
        .any(|import| {
            java_type::text(&import, bytes).is_some_and(|text| {
                text.contains("org.springframework") || text.contains("jakarta.persistence")
            })
        })
}

/// A plain DFS over the whole tree, dispatching each `class_declaration`/`interface_declaration` to
/// the extractor(s) that apply to that container kind. Every other node kind is walked through
/// unchanged, purely to reach any declarations nested inside it (an inner class, a class declared
/// inside a method body, …) — this function itself does not interpret any node it does not
/// specifically match.
///
/// The `class_declaration`/`interface_declaration` split IS the enforcement behind [`route::extract`]
/// never running for an interface: `PaymentClient` (`tests/fixtures/spring-repo/PaymentClient.java`)
/// is an `interface_declaration`, so it only ever reaches [`feign::extract`]/[`repository::extract`],
/// never [`route::extract`] — there is no annotation check that could accidentally let a
/// `@FeignClient`'s `@PostMapping` be mistaken for a served route, because the route extractor is
/// simply never invoked with that node at all.
fn walk(node: &Node, bytes: &[u8], source_file: &str, facts: &mut FrameworkFacts) {
    match node.kind() {
        "class_declaration" => route::extract(node, bytes, source_file, facts),
        "interface_declaration" => {
            feign::extract(node, bytes, source_file, facts);
            repository::extract(node, bytes, source_file, facts);
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(&child, bytes, source_file, facts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(source: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("java grammar loads");
        parser.parse(source, None).expect("source parses")
    }

    #[test]
    fn a_non_spring_java_file_produces_completely_empty_facts() {
        // Real Spring-shaped annotation names, but on a file that imports neither
        // `org.springframework` nor `jakarta.persistence` — the import gate must short-circuit
        // before the annotation walk ever looks at them, so this must NOT accidentally produce a
        // route despite the `@GetMapping`-lookalike text below.
        let source = r#"
            package com.example.plain;

            class Greeter {
                String greet(String name) {
                    return "hello " + name;
                }
            }
        "#;
        let tree = parse(source);
        let facts = extract_facts(&tree, source, "Greeter.java");
        assert_eq!(facts, FrameworkFacts::default());
    }

    #[test]
    fn a_file_importing_org_springframework_passes_the_gate() {
        let source = r#"
            package com.example.accounts;

            import org.springframework.web.bind.annotation.RestController;

            @RestController
            class AccountController {
                @GetMapping("/x")
                void m() {}
            }
        "#;
        let tree = parse(source);
        let facts = extract_facts(&tree, source, "AccountController.java");
        assert_eq!(facts.nodes.len(), 1);
        assert_eq!(facts.nodes[0].node_id, "route:GET:/x");
    }

    #[test]
    fn a_file_importing_jakarta_persistence_passes_the_gate_even_with_no_spring_import() {
        // §2.1's gate is an "or", not an "and": `jakarta.persistence` alone (a plain JPA entity file,
        // no Spring Web/Cloud import at all) must still open the annotation walk — even though this
        // crate has no JPA-entity-specific fact to contribute yet, the gate itself must not require
        // BOTH ecosystems' imports to be present.
        let source = r#"
            package com.example.accounts;

            import jakarta.persistence.Entity;

            @Entity
            class Account {}
        "#;
        let tree = parse(source);
        // No route/service/repository facts come out of a bare `@Entity` class today (§2.4 rejects a
        // `persists` relation), but the gate must still have let the walk run rather than
        // short-circuiting on `FrameworkFacts::default()` before even trying.
        let facts = extract_facts(&tree, source, "Account.java");
        assert_eq!(facts, FrameworkFacts::default());
    }

    #[test]
    fn a_non_top_level_import_like_text_does_not_open_the_gate_on_its_own() {
        // A file with no import at all, and an unrelated string literal that happens to contain
        // "org.springframework" — the gate only scans `import_declaration` nodes, never arbitrary
        // string content, so this must stay closed.
        let source = r#"
            class C {
                String s = "org.springframework is just a string here";
            }
        "#;
        let tree = parse(source);
        let facts = extract_facts(&tree, source, "C.java");
        assert_eq!(facts, FrameworkFacts::default());
    }

    #[test]
    fn a_feign_client_interface_produces_no_route_node() {
        // The headline regression this crate exists to prevent: a `@FeignClient` interface's own
        // `@PostMapping` must never be mistaken for a served route, because `route::extract` is only
        // ever reached from a `class_declaration`, never an `interface_declaration`.
        let source = r#"
            package com.example.accounts;

            import org.springframework.cloud.openfeign.FeignClient;
            import org.springframework.web.bind.annotation.PostMapping;

            @FeignClient(name = "payment-service")
            interface PaymentClient {
                @PostMapping("/ledgers")
                void openLedger(String email);
            }
        "#;
        let tree = parse(source);
        let facts = extract_facts(&tree, source, "PaymentClient.java");
        assert!(
            facts.nodes.iter().all(|n| !n.node_id.starts_with("route:")),
            "a FeignClient interface must never produce a route node: {:?}",
            facts.nodes
        );
        assert_eq!(facts.nodes.len(), 1);
        assert_eq!(facts.nodes[0].node_id, "service:payment-service");
    }

    #[test]
    fn nested_class_declarations_are_still_reached_by_the_walk() {
        let source = r#"
            package com.example.accounts;

            import org.springframework.web.bind.annotation.RestController;

            class Outer {
                @RestController
                static class Inner {
                    @GetMapping("/x")
                    void m() {}
                }
            }
        "#;
        let tree = parse(source);
        let facts = extract_facts(&tree, source, "Outer.java");
        assert_eq!(facts.nodes.len(), 1);
        assert_eq!(facts.nodes[0].node_id, "route:GET:/x");
    }
}
