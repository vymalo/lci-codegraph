//! `external_service` node extraction (§2.1) plus the `FrameworkCallTarget`s that let a call through a
//! `@FeignClient` interface resolve to that boundary marker (§2.2) instead of being silently dropped —
//! the implementation is not merely absent from this file, it lives in a different repository
//! entirely, which is exactly what an `external_service` node exists to say honestly.

use crate::annotation;
use crate::java_type::text;
use lci_codegraph_model::{FrameworkCallTarget, FrameworkFacts, GraphNode};
use tree_sitter::Node;

/// Emit the `external_service` node + one `FrameworkCallTarget` per method for `interface`, iff it
/// carries `@FeignClient` AND that annotation names a service. `interface` is not required to be an
/// interface by this function alone, but [`crate::walk`] only ever calls it with one.
pub(crate) fn extract(
    interface: &Node,
    source: &[u8],
    source_file: &str,
    facts: &mut FrameworkFacts,
) {
    let interface_annotations = annotation::annotations_of(interface, source);
    let Some(feign) = interface_annotations
        .iter()
        .find(|a| a.name == "FeignClient")
    else {
        return;
    };
    let Some(service_name) = service_name(feign, source) else {
        // `@FeignClient(url = "...")` with no `name`/`value` names no identity this crate can key a
        // node on. Staying silent here — rather than, say, keying on the interface's own Java type
        // name — is the same "resolve when provable, stay silent otherwise" posture the design doc's
        // §5.3 applies to XML config and `@Profile`: a confident wrong identity is worse than none.
        return;
    };
    let Some(interface_name) = interface
        .child_by_field_name("name")
        .and_then(|n| text(&n, source))
    else {
        return;
    };

    let node_id = format!("service:{service_name}");
    // Deviates from docs/design/spring-aware-graph.md §2.1, which says an `external_service` has no
    // `source_file`/`start_line` "in the usual sense" (it names something outside this repo).
    // `GraphNode` has no optional fields, though — something has to go there — and a synthetic empty
    // path would be just as much a fiction as a real one, while strictly less useful: pointing at the
    // `@FeignClient` interface's own declaration means `graph_find_symbol("payment-service")` hands a
    // reviewer the file to open, not a dead end. The duplicate-declaration risk this introduces (the
    // same service name declared as a `@FeignClient` in two different files) is handled
    // deterministically downstream — [`FrameworkFacts`] is additive-only, so two declarations simply
    // contribute two nodes with the same `node_id`, and the host's canonicalising sort+dedup (the same
    // one every other node in the graph already goes through) collapses them.
    let start_line = interface.start_position().row as i64 + 1;
    facts.nodes.push(GraphNode {
        node_id: node_id.clone(),
        label: service_name,
        source_file: source_file.to_string(),
        start_line,
    });

    let Some(body) = interface.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        let Some(name) = member
            .child_by_field_name("name")
            .and_then(|n| text(&n, source))
        else {
            continue;
        };
        facts.call_targets.push(FrameworkCallTarget {
            name,
            node_id: node_id.clone(),
            scope: Some(interface_name.clone()),
            scope_supers: Vec::new(),
        });
    }
}

/// The service name a `@FeignClient` annotation declares: `name = "..."`, `value = "..."` (Spring
/// treats them as aliases), or a bare single string literal (`@FeignClient("payment-service")`).
fn service_name(feign: &annotation::Annotation, source: &[u8]) -> Option<String> {
    let named = annotation::named_string_values(feign.arguments, &["name", "value"], source);
    if let Some(name) = named.into_iter().next() {
        return Some(name);
    }
    annotation::bare_string_values(feign.arguments, source)
        .into_iter()
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("java grammar loads");
        parser.parse(source, None).expect("source parses")
    }

    fn extract_from(source: &str, source_file: &str) -> FrameworkFacts {
        let tree = parse(source);
        let bytes = source.as_bytes();
        let interface = tree
            .root_node()
            .named_child(0)
            .expect("a top-level declaration exists");
        let mut facts = FrameworkFacts::default();
        extract(&interface, bytes, source_file, &mut facts);
        facts
    }

    #[test]
    fn named_name_argument_produces_a_service_node_and_call_targets() {
        let src = r#"
            @FeignClient(name = "payment-service")
            interface PaymentClient {
                void openLedger(String email);
            }
        "#;
        let facts = extract_from(src, "PaymentClient.java");
        assert_eq!(facts.nodes.len(), 1);
        assert_eq!(facts.nodes[0].node_id, "service:payment-service");
        assert_eq!(facts.nodes[0].label, "payment-service");
        assert_eq!(facts.call_targets.len(), 1);
        assert_eq!(facts.call_targets[0].name, "openLedger");
        assert_eq!(facts.call_targets[0].node_id, "service:payment-service");
        assert_eq!(
            facts.call_targets[0].scope.as_deref(),
            Some("PaymentClient")
        );
        assert!(facts.call_targets[0].scope_supers.is_empty());
    }

    #[test]
    fn bare_single_string_argument_also_names_the_service() {
        let src = r#"
            @FeignClient("payment-service")
            interface PaymentClient {
                void openLedger(String email);
            }
        "#;
        let facts = extract_from(src, "PaymentClient.java");
        assert_eq!(facts.nodes[0].node_id, "service:payment-service");
    }

    #[test]
    fn value_named_argument_also_names_the_service() {
        let src = r#"
            @FeignClient(value = "payment-service")
            interface PaymentClient {
                void openLedger(String email);
            }
        "#;
        let facts = extract_from(src, "PaymentClient.java");
        assert_eq!(facts.nodes[0].node_id, "service:payment-service");
    }

    #[test]
    fn feign_client_with_no_name_or_value_stays_silent() {
        let src = r#"
            @FeignClient(url = "http://payment.internal")
            interface PaymentClient {
                void openLedger(String email);
            }
        "#;
        let facts = extract_from(src, "PaymentClient.java");
        assert!(facts.nodes.is_empty());
        assert!(facts.call_targets.is_empty());
    }

    #[test]
    fn every_method_in_the_interface_becomes_its_own_call_target() {
        let src = r#"
            @FeignClient("payment-service")
            interface PaymentClient {
                void openLedger(String email);
                void closeLedger(String email);
            }
        "#;
        let facts = extract_from(src, "PaymentClient.java");
        let names: Vec<_> = facts.call_targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["openLedger", "closeLedger"]);
    }

    #[test]
    fn an_interface_with_no_feign_client_annotation_yields_nothing() {
        let src = r#"
            interface Plain {
                void m();
            }
        "#;
        let facts = extract_from(src, "Plain.java");
        assert!(facts.nodes.is_empty());
        assert!(facts.call_targets.is_empty());
    }

    #[test]
    fn the_service_node_points_at_the_feign_client_interfaces_own_declaration() {
        let src = "@FeignClient(\"payment-service\")\ninterface PaymentClient {\n    void openLedger(String email);\n}\n";
        let facts = extract_from(src, "PaymentClient.java");
        assert_eq!(facts.nodes[0].source_file, "PaymentClient.java");
        // Line 1 carries `@FeignClient(...)`, which is where `interface_declaration`'s own span
        // starts (its `modifiers` field is part of the node).
        assert_eq!(facts.nodes[0].start_line, 1);
    }
}
