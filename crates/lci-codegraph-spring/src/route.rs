//! `route` node extraction (§2.1): one node per HTTP endpoint THIS repo *serves*, found on a
//! `@RestController`/`@Controller` class's own `@…Mapping` methods.
//!
//! [`extract`] is deliberately only ever called with a `class_declaration` (see `lib.rs`'s dispatch) —
//! never an `interface_declaration`. That is precisely what stops a `@FeignClient` interface's
//! `@PostMapping` (an OUTBOUND request declaration, `tests/fixtures/spring-repo/PaymentClient.java`)
//! from being mistaken for a served endpoint: the two annotations look identical in isolation, and the
//! only thing that tells them apart is the kind of container they sit in.

use crate::annotation::{self, Annotation};
use crate::java_type::text;
use lci_codegraph_model::{FrameworkFacts, GraphEdge, GraphNode, def_node_id};
use tree_sitter::Node;

/// Class-level stereotypes that make a class's own `@…Mapping` methods real, served endpoints.
const CONTROLLER_MARKERS: [&str; 2] = ["RestController", "Controller"];

/// Method-level mapping annotations that name their HTTP verb outright by their own name.
/// `@RequestMapping` is handled separately in [`http_method_of`]: its verb comes from a
/// `method = RequestMethod.X` argument, or `ANY` when that argument is absent.
const VERB_MAPPINGS: [(&str, &str); 5] = [
    ("GetMapping", "GET"),
    ("PostMapping", "POST"),
    ("PutMapping", "PUT"),
    ("DeleteMapping", "DELETE"),
    ("PatchMapping", "PATCH"),
];

/// Emit one `route` node + one `route → handler` `calls` edge per HTTP endpoint `class` serves, iff
/// `class` carries `@RestController`/`@Controller`. A class with neither annotation contributes
/// nothing — including, notably, a plain class that merely happens to have a `@GetMapping`-annotated
/// method sitting in it (not a real Spring construct, but the class-level gate keeps the extractor
/// from inventing meaning for it anyway).
pub(crate) fn extract(class: &Node, source: &[u8], source_file: &str, facts: &mut FrameworkFacts) {
    let class_annotations = annotation::annotations_of(class, source);
    if !class_annotations
        .iter()
        .any(|a| CONTROLLER_MARKERS.contains(&a.name.as_str()))
    {
        return;
    }
    let prefix = class_prefix(&class_annotations, source);
    let Some(body) = class.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() == "method_declaration" {
            emit_routes(&member, &prefix, source, source_file, facts);
        }
    }
}

/// The class-level path prefix from `@RequestMapping` on the class itself. Absent → empty prefix
/// (composing with a method's own suffix still produces a valid, unprefixed route).
fn class_prefix(class_annotations: &[Annotation], source: &[u8]) -> String {
    class_annotations
        .iter()
        .find(|a| a.name == "RequestMapping")
        .map(|a| {
            mapping_path(a, source)
                .into_iter()
                .next()
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Emit every route `method` declares — one `@…Mapping` annotation can name several paths
/// (`@GetMapping({"/a", "/b"})`), each of which becomes its own `route` node pointing at the SAME
/// handler.
fn emit_routes(
    method: &Node,
    prefix: &str,
    source: &[u8],
    source_file: &str,
    facts: &mut FrameworkFacts,
) {
    let method_annotations = annotation::annotations_of(method, source);
    let Some(mapping) = method_annotations.iter().find(|a| is_mapping(&a.name)) else {
        return;
    };
    let Some(http_method) = http_method_of(mapping, source) else {
        return;
    };
    let Some(handler_name) = method
        .child_by_field_name("name")
        .and_then(|n| text(&n, source))
    else {
        return;
    };
    // 1-based line, matching `lci-codegraph`'s own `emit::walk`: the definition node it classifies
    // for this same method is the WHOLE `method_declaration` (tags.scm's `@definition.method` capture
    // includes the `modifiers` field), so its `start_position()` is the annotation's own line, not the
    // return-type line. Using that exact node here — not the name node, not the first
    // non-annotation token — is what keeps `target` byte-identical to the id `graph::extract_file`
    // mints for the very same handler.
    let start_line = method.start_position().row as i64 + 1;
    let target = def_node_id(source_file, start_line, &handler_name);

    let suffixes = mapping_path(mapping, source);
    let suffixes = if suffixes.is_empty() {
        vec![String::new()]
    } else {
        suffixes
    };
    for suffix in suffixes {
        let path = compose_path(prefix, &suffix);
        let node_id = format!("route:{http_method}:{path}");
        facts.nodes.push(GraphNode {
            node_id: node_id.clone(),
            label: format!("{http_method} {path}"),
            source_file: source_file.to_string(),
            start_line,
        });
        // Honest, not a fiction: something really does call this handler — the `DispatcherServlet` —
        // it is just not another symbol in this file (docs/design/spring-aware-graph.md §2.1/§2.2).
        facts.edges.push(GraphEdge {
            source: node_id,
            target: target.clone(),
            relation: "calls".to_string(),
        });
    }
}

fn is_mapping(name: &str) -> bool {
    name == "RequestMapping" || VERB_MAPPINGS.iter().any(|(n, _)| *n == name)
}

/// The HTTP method a mapping annotation names. `@GetMapping`-family annotations name their verb by
/// their own annotation name; `@RequestMapping` takes its verb from a `method = RequestMethod.X`
/// argument when present, or `ANY` when absent — a bare `@RequestMapping` matches every verb in
/// Spring, so `ANY` is the honest answer, not a guess at one verb or a silently dropped route.
fn http_method_of(mapping: &Annotation, source: &[u8]) -> Option<String> {
    if mapping.name == "RequestMapping" {
        Some(
            annotation::named_identifier_tail(mapping.arguments, "method", source)
                .unwrap_or_else(|| "ANY".to_string()),
        )
    } else {
        VERB_MAPPINGS
            .iter()
            .find(|(n, _)| *n == mapping.name)
            .map(|(_, verb)| (*verb).to_string())
    }
}

/// A mapping annotation's path argument(s), in whichever of the three shapes it used: a bare string, a
/// bare array, or `value =`/`path =`. `value =` is tried before `path =`; Spring itself treats them as
/// aliases for the same thing, so which one a codebase happens to use does not change the outcome.
fn mapping_path(annotation: &Annotation, source: &[u8]) -> Vec<String> {
    let bare = annotation::bare_string_values(annotation.arguments, source);
    if !bare.is_empty() {
        return bare;
    }
    annotation::named_string_values(annotation.arguments, &["value", "path"], source)
}

/// Join a class-level prefix and a method-level suffix with exactly one `/`, no trailing slash;
/// empty + empty composes to `/` (the root path), never an empty string.
fn compose_path(prefix: &str, suffix: &str) -> String {
    let p = prefix.trim_matches('/');
    let s = suffix.trim_matches('/');
    let joined = match (p.is_empty(), s.is_empty()) {
        (true, true) => String::new(),
        (true, false) => s.to_string(),
        (false, true) => p.to_string(),
        (false, false) => format!("{p}/{s}"),
    };
    format!("/{joined}")
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
        let class = tree
            .root_node()
            .named_child(0)
            .expect("a top-level declaration exists");
        let mut facts = FrameworkFacts::default();
        extract(&class, bytes, source_file, &mut facts);
        facts
    }

    #[test]
    fn compose_path_joins_a_prefix_and_a_suffix_with_one_slash() {
        assert_eq!(
            compose_path("/api/accounts", "/{email}"),
            "/api/accounts/{email}"
        );
    }

    #[test]
    fn compose_path_of_an_empty_prefix_and_suffix_is_the_root() {
        assert_eq!(compose_path("", ""), "/");
    }

    #[test]
    fn compose_path_of_a_prefix_alone_has_no_trailing_slash() {
        assert_eq!(compose_path("/api/accounts", ""), "/api/accounts");
    }

    #[test]
    fn compose_path_of_a_suffix_alone_needs_no_prefix() {
        assert_eq!(compose_path("", "/foo"), "/foo");
    }

    #[test]
    fn rest_controller_with_class_and_method_prefix_composes_a_full_route() {
        let src = r#"
            @RestController
            @RequestMapping("/api/accounts")
            class AccountController {
                @GetMapping("/{email}")
                Optional<Account> getAccount(String email) { return null; }
            }
        "#;
        let facts = extract_from(src, "AccountController.java");
        assert_eq!(facts.nodes.len(), 1);
        assert_eq!(facts.nodes[0].node_id, "route:GET:/api/accounts/{email}");
        assert_eq!(facts.nodes[0].label, "GET /api/accounts/{email}");
        assert_eq!(facts.edges.len(), 1);
        assert_eq!(facts.edges[0].source, "route:GET:/api/accounts/{email}");
        assert_eq!(facts.edges[0].relation, "calls");
    }

    #[test]
    fn bare_post_mapping_with_no_path_uses_the_class_prefix_alone() {
        let src = r#"
            @RestController
            @RequestMapping("/api/accounts")
            class AccountController {
                @PostMapping
                Account createAccount(String email) { return null; }
            }
        "#;
        let facts = extract_from(src, "AccountController.java");
        assert_eq!(facts.nodes[0].node_id, "route:POST:/api/accounts");
    }

    #[test]
    fn controller_with_no_class_level_request_mapping_has_an_empty_prefix() {
        let src = r#"
            @RestController
            class Bare {
                @GetMapping("/x")
                void m() {}
            }
        "#;
        let facts = extract_from(src, "Bare.java");
        assert_eq!(facts.nodes[0].node_id, "route:GET:/x");
    }

    #[test]
    fn plain_controller_annotation_also_yields_routes() {
        let src = r#"
            @Controller
            class ViewController {
                @GetMapping("/home")
                void m() {}
            }
        "#;
        let facts = extract_from(src, "ViewController.java");
        assert_eq!(facts.nodes.len(), 1);
    }

    #[test]
    fn value_named_argument_composes_the_same_as_a_bare_string() {
        let src = r#"
            @RestController
            class C {
                @RequestMapping(value = "/x")
                void m() {}
            }
        "#;
        let facts = extract_from(src, "C.java");
        assert_eq!(facts.nodes[0].node_id, "route:ANY:/x");
    }

    #[test]
    fn path_named_argument_composes_the_same_as_a_bare_string() {
        let src = r#"
            @RestController
            class C {
                @RequestMapping(path = "/y")
                void m() {}
            }
        "#;
        let facts = extract_from(src, "C.java");
        assert_eq!(facts.nodes[0].node_id, "route:ANY:/y");
    }

    #[test]
    fn request_mapping_with_an_explicit_method_uses_that_verb() {
        let src = r#"
            @RestController
            class C {
                @RequestMapping(value = "/x", method = RequestMethod.PUT)
                void m() {}
            }
        "#;
        let facts = extract_from(src, "C.java");
        assert_eq!(facts.nodes[0].node_id, "route:PUT:/x");
    }

    #[test]
    fn bare_request_mapping_with_no_method_argument_matches_every_verb() {
        let src = r#"
            @RestController
            @RequestMapping("/x")
            class C {
                @RequestMapping
                void m() {}
            }
        "#;
        let facts = extract_from(src, "C.java");
        assert_eq!(facts.nodes[0].node_id, "route:ANY:/x");
    }

    #[test]
    fn array_argument_produces_one_route_node_per_path() {
        let src = r#"
            @RestController
            class C {
                @GetMapping({"/a", "/b"})
                void m() {}
            }
        "#;
        let facts = extract_from(src, "C.java");
        let ids: Vec<_> = facts.nodes.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(ids, vec!["route:GET:/a", "route:GET:/b"]);
        // Both routes point at the SAME handler.
        assert_eq!(facts.edges[0].target, facts.edges[1].target);
    }

    #[test]
    fn a_class_with_neither_controller_annotation_yields_no_routes() {
        let src = r#"
            @Service
            class AccountServiceImpl {
                @GetMapping("/x")
                void m() {}
            }
        "#;
        let facts = extract_from(src, "AccountServiceImpl.java");
        assert!(facts.nodes.is_empty());
        assert!(facts.edges.is_empty());
    }

    #[test]
    fn route_node_and_edge_target_point_at_the_handlers_own_line_including_its_annotation() {
        let src = "@RestController\nclass C {\n    @GetMapping(\"/x\")\n    void m() {}\n}\n";
        let facts = extract_from(src, "C.java");
        // The `@GetMapping` annotation sits on line 3 (1-based); `method_declaration`'s own span —
        // which is what `graph::extract_file` classifies as the definition — starts there too, not
        // on line 4 where `void m()` itself appears.
        assert_eq!(facts.nodes[0].start_line, 3);
        assert_eq!(facts.edges[0].target, "C.java#3:m");
    }
}
