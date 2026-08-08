//! Small Java-type-name helpers shared by the `route`/`feign`/`repository` extractors: recovering a
//! type's *simple* head name (generics/package-qualification/array-ness stripped) and reading an
//! `interface_declaration`'s `extends_interfaces` clause.
//!
//! `lci-codegraph`'s own `src/graph/callee.rs` (`type_head_name`/`supertype_names`) already solves
//! exactly this problem, verified against the same `tree-sitter-java` grammar shapes documented
//! there. This module is a deliberate, narrower re-implementation, not a shared import: this crate is
//! barred from depending on the core `lci-codegraph` crate at all (see this crate's `Cargo.toml`
//! comment) — the whole point of the `lci-codegraph-model` seam is that a framework extractor's facts
//! flow *into* the core's resolver without the core crate ever needing to `use` a framework-specific
//! crate, and without this crate needing the whole tree-sitter-parsing core just to describe a type
//! name. A dependency back on the core crate would create exactly the cycle that boundary exists to
//! avoid.

use tree_sitter::Node;

/// A node's raw source text, decoded as UTF-8. `None` only on genuinely malformed input (a byte range
/// that isn't valid UTF-8) — tree-sitter never hands out an out-of-bounds range for a real parse.
pub(crate) fn text(node: &Node, source: &[u8]) -> Option<String> {
    std::str::from_utf8(&source[node.byte_range()])
        .ok()
        .map(str::to_string)
}

/// The simple head name of a type node, generics/package-qualification/array-ness stripped:
/// `List<Account>` → `List`, `com.example.Account` → `Account`, `Account[]` → `Account`. A primitive
/// type (`int`, `boolean`, `void`, …) has none of the node kinds matched below and falls through to
/// `None` — deliberately: a primitive is never a useful type name here.
pub(crate) fn simple_type_name(node: &Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => text(node, source),
        "generic_type" => {
            // Java's `generic_type` (`List<Account>`) has NO fields at all — an untagged child list
            // of `[base type, type_arguments]` — so the base type is recovered by finding the first
            // named child that is itself a type, skipping `type_arguments`.
            if let Some(base) = node.child_by_field_name("type") {
                return simple_type_name(&base, source);
            }
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|c| matches!(c.kind(), "type_identifier" | "scoped_type_identifier"))
                .and_then(|c| simple_type_name(&c, source))
        }
        "scoped_type_identifier" => {
            // Java's `com.example.Account` has no fields either — it nests recursively (the prefix is
            // itself a `scoped_type_identifier`), so the tail segment is the LAST named
            // `type_identifier` child.
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| text(&n, source))
            {
                return Some(name);
            }
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|c| c.kind() == "type_identifier")
                .last()
                .and_then(|n| text(&n, source))
        }
        // `Account[]` tags the element with an `element` field.
        "array_type" => node
            .child_by_field_name("element")
            .and_then(|n| simple_type_name(&n, source)),
        _ => None,
    }
}

/// The simple names an `interface_declaration`'s `extends_interfaces` clause lists (`interface Foo
/// extends Bar, Baz<Qux>` → `["Bar", "Baz"]`). Verified against tree-sitter-java's `node-types.json`:
/// unlike `class_declaration`'s `superclass`/`interfaces`, `extends_interfaces` is NOT a tagged field
/// — it is an untagged child of the `interface_declaration` node — so this walks the node's direct
/// children positionally instead of using `child_by_field_name`. An interface with no `extends`
/// clause at all yields an empty `Vec`.
pub(crate) fn extends_interfaces_names(interface: &Node, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = interface.walk();
    let Some(extends) = interface
        .children(&mut cursor)
        .find(|c| c.kind() == "extends_interfaces")
    else {
        return names;
    };
    collect_type_names(&extends, source, &mut names);
    names
}

/// Recursively collect every type's simple head name under `node` — flattens the `type_list` an
/// `extends_interfaces` clause wraps (possibly several types, `Foo, Bar<Baz>`) without a separate case
/// for "one type" vs "several".
fn collect_type_names(node: &Node, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "type_identifier" | "generic_type" | "scoped_type_identifier" | "array_type" => {
            if let Some(name) = simple_type_name(node, source) {
                out.push(name);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_type_names(&child, source, out);
            }
        }
    }
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

    fn find_first<'a>(node: &tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
        if node.kind() == kind {
            return Some(*node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = find_first(&child, kind) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn simple_type_name_strips_generic_type_arguments() {
        let src = "class C { List<Account> items; }";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let ty = find_first(&tree.root_node(), "generic_type").expect("generic type present");
        assert_eq!(simple_type_name(&ty, bytes).as_deref(), Some("List"));
    }

    #[test]
    fn simple_type_name_strips_package_qualification() {
        let src = "class C { com.example.Account acc; }";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let ty =
            find_first(&tree.root_node(), "scoped_type_identifier").expect("scoped type present");
        assert_eq!(simple_type_name(&ty, bytes).as_deref(), Some("Account"));
    }

    #[test]
    fn simple_type_name_strips_array_suffix() {
        let src = "class C { Account[] accs; }";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let ty = find_first(&tree.root_node(), "array_type").expect("array type present");
        assert_eq!(simple_type_name(&ty, bytes).as_deref(), Some("Account"));
    }

    #[test]
    fn extends_interfaces_names_collects_a_generic_marker() {
        let src = "interface AccountRepository extends JpaRepository<Account, Long> {}";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let iface = find_first(&tree.root_node(), "interface_declaration")
            .expect("interface declaration present");
        assert_eq!(
            extends_interfaces_names(&iface, bytes),
            vec!["JpaRepository"]
        );
    }

    #[test]
    fn extends_interfaces_names_collects_several_supertypes() {
        let src = "interface Combined extends Foo, Bar {}";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let iface = find_first(&tree.root_node(), "interface_declaration")
            .expect("interface declaration present");
        let mut names = extends_interfaces_names(&iface, bytes);
        names.sort();
        assert_eq!(names, vec!["Bar", "Foo"]);
    }

    #[test]
    fn extends_interfaces_names_of_an_interface_with_no_extends_clause_is_empty() {
        let src = "interface Plain {}";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let iface = find_first(&tree.root_node(), "interface_declaration")
            .expect("interface declaration present");
        assert!(extends_interfaces_names(&iface, bytes).is_empty());
    }
}
