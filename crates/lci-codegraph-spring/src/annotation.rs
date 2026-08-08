//! Annotation-argument parsing: recovering an annotation's simple name and interpreting the shapes
//! Spring's own annotations use — bare (`@Foo`), a single string (`@Foo("x")`), a named argument
//! (`@Foo(key = "x")`), and an array (`@Foo({"x", "y"})`) — plus the one non-string shape this crate
//! needs (`method = RequestMethod.PUT`). This is the one piece of `tree-sitter-java` surface every
//! extractor in this crate (`route`, `feign`, and `repository` by way of its allowlist check) shares,
//! and none of it is specific to any one annotation.

use crate::java_type::text;
use tree_sitter::Node;

/// One annotation attached to a declaration: `@Foo` (`marker_annotation`, no arguments at all) or
/// `@Foo(...)` (`annotation`, with an `annotation_argument_list` under its `arguments` field).
pub(crate) struct Annotation<'tree> {
    pub(crate) name: String,
    pub(crate) arguments: Option<Node<'tree>>,
}

/// Every annotation directly modifying `node` (a class/interface/method declaration). tree-sitter-java
/// puts a declaration's full modifier list — annotations alongside `public`/`static`/`final`/etc. —
/// under one `modifiers` node, but (verified against tree-sitter-java's `node-types.json`) that node
/// is NOT a tagged field on `class_declaration`/`interface_declaration`/`method_declaration` the way
/// `name`/`body`/`parameters` are — it is an untagged positional child, so it needs a `kind()` search
/// rather than `child_by_field_name`, exactly like `java_type::extends_interfaces_names` has to search
/// for `extends_interfaces` the same way. A declaration with no modifiers at all (no `modifiers` child
/// present) yields an empty `Vec`, not an error.
pub(crate) fn annotations_of<'tree>(node: &Node<'tree>, source: &[u8]) -> Vec<Annotation<'tree>> {
    let mut node_cursor = node.walk();
    let Some(modifiers) = node
        .children(&mut node_cursor)
        .find(|c| c.kind() == "modifiers")
    else {
        return Vec::new();
    };
    let mut cursor = modifiers.walk();
    modifiers
        .children(&mut cursor)
        .filter_map(|child| match child.kind() {
            "marker_annotation" => Some(Annotation {
                name: annotation_name(&child, source)?,
                arguments: None,
            }),
            "annotation" => Some(Annotation {
                name: annotation_name(&child, source)?,
                arguments: child.child_by_field_name("arguments"),
            }),
            _ => None,
        })
        .collect()
}

/// An annotation node's simple name: `@RestController` → `"RestController"`; a (rare, but
/// grammar-legal) fully-qualified `@org.springframework.web.bind.annotation.RestController` resolves
/// via the same tail-of-`scoped_identifier` logic `lci-codegraph`'s own callee/qualifier recovery
/// uses elsewhere, so a fully-qualified annotation matches the allowlist exactly like the common bare
/// form does.
fn annotation_name(node: &Node, source: &[u8]) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    match name_node.kind() {
        "scoped_identifier" => name_node
            .child_by_field_name("name")
            .and_then(|n| text(&n, source)),
        _ => text(&name_node, source),
    }
}

/// The bare string literal(s) an argument list carries with no `key =` prefix: `@Foo("x")` → `["x"]`;
/// `@Foo({"x", "y"})` → `["x", "y"]`. A named-only list (`@Foo(key = "x")`), a marker annotation with
/// no arguments at all, or any other shape yields an empty `Vec` — never a guess at which named
/// argument was "meant".
pub(crate) fn bare_string_values(arguments: Option<Node>, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(args) = arguments {
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            collect_string_values(&child, source, &mut out);
        }
    }
    out
}

/// The string value(s) of a `key = ...` pair whose key matches one of `keys`, tried in the given
/// order. Used for `value =`/`path =`, which Spring's own mapping annotations treat as
/// interchangeable aliases for the same thing.
pub(crate) fn named_string_values(
    arguments: Option<Node>,
    keys: &[&str],
    source: &[u8],
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(pair) = find_named_pair(arguments, keys, source)
        && let Some(value) = pair.child_by_field_name("value")
    {
        collect_string_values(&value, source, &mut out);
    }
    out
}

/// The tail identifier of a `key = Some.Path.TAIL` pair's value — `method = RequestMethod.PUT` →
/// `Some("PUT")`, which is exactly how Spring's `RequestMethod` enum constants are referenced. A bare
/// identifier value (e.g. a statically imported constant used without its enum prefix) falls back to
/// its own text; anything else (a string, a nested annotation, …) is not this shape and yields `None`.
pub(crate) fn named_identifier_tail(
    arguments: Option<Node>,
    key: &str,
    source: &[u8],
) -> Option<String> {
    let pair = find_named_pair(arguments, &[key], source)?;
    let value = pair.child_by_field_name("value")?;
    match value.kind() {
        "field_access" => value
            .child_by_field_name("field")
            .and_then(|n| text(&n, source)),
        "identifier" => text(&value, source),
        _ => None,
    }
}

fn find_named_pair<'tree>(
    arguments: Option<Node<'tree>>,
    keys: &[&str],
    source: &[u8],
) -> Option<Node<'tree>> {
    let args = arguments?;
    let mut cursor = args.walk();
    args.named_children(&mut cursor).find(|c| {
        c.kind() == "element_value_pair"
            && c.child_by_field_name("key")
                .and_then(|k| text(&k, source))
                .is_some_and(|k| keys.contains(&k.as_str()))
    })
}

/// Collect every `string_literal`'s text under `node`, flattening an
/// `element_value_array_initializer` (`{"a", "b"}`) the same way a single bare `string_literal` is
/// handled — one code path for "one string" and "several strings".
fn collect_string_values(node: &Node, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "string_literal" => out.push(string_literal_text(node, source)),
        "element_value_array_initializer" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_string_values(&child, source, out);
            }
        }
        _ => {}
    }
}

/// A `string_literal`'s content, quotes stripped, by concatenating its named children's own text
/// (`string_fragment` for plain text, `escape_sequence` for `\n`-style escapes — kept as their literal
/// source text rather than decoded, which is all every path/name value in this crate's fixtures ever
/// needs). An empty literal (`""`) has NO `string_fragment` child at all — no children, no text —
/// which falls out of this loop naturally rather than needing its own empty-string case.
fn string_literal_text(node: &Node, source: &[u8]) -> String {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter_map(|c| text(&c, source))
        .collect()
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

    /// Parse a one-method class carrying a single annotation on that method, and hand the annotation
    /// to `f`.
    fn with_method_annotation<R>(src: &str, f: impl FnOnce(&Annotation, &[u8]) -> R) -> R {
        let tree = parse(src);
        let bytes = src.as_bytes();
        let method = find_first(&tree.root_node(), "method_declaration")
            .expect("method declaration present");
        let annotations = annotations_of(&method, bytes);
        assert_eq!(
            annotations.len(),
            1,
            "expected exactly one annotation in {src:?}"
        );
        f(&annotations[0], bytes)
    }

    #[test]
    fn marker_annotation_has_no_arguments() {
        with_method_annotation("class C { @PostMapping void m() {} }", |a, source| {
            assert_eq!(a.name, "PostMapping");
            assert!(bare_string_values(a.arguments, source).is_empty());
        });
    }

    #[test]
    fn single_bare_string_argument_is_one_value() {
        with_method_annotation(
            r#"class C { @GetMapping("/x") void m() {} }"#,
            |a, source| {
                assert_eq!(bare_string_values(a.arguments, source), vec!["/x"]);
            },
        );
    }

    #[test]
    fn array_argument_is_every_value_in_order() {
        with_method_annotation(
            r#"class C { @GetMapping({"/a", "/b"}) void m() {} }"#,
            |a, source| {
                assert_eq!(bare_string_values(a.arguments, source), vec!["/a", "/b"]);
            },
        );
    }

    #[test]
    fn named_value_argument_is_found_by_key() {
        with_method_annotation(
            r#"class C { @RequestMapping(value = "/x") void m() {} }"#,
            |a, source| {
                assert_eq!(
                    named_string_values(a.arguments, &["value", "path"], source),
                    vec!["/x"]
                );
                assert!(bare_string_values(a.arguments, source).is_empty());
            },
        );
    }

    #[test]
    fn named_path_argument_is_found_by_key() {
        with_method_annotation(
            r#"class C { @RequestMapping(path = "/y") void m() {} }"#,
            |a, source| {
                assert_eq!(
                    named_string_values(a.arguments, &["value", "path"], source),
                    vec!["/y"]
                );
            },
        );
    }

    #[test]
    fn named_identifier_tail_recovers_an_enum_constants_simple_name() {
        with_method_annotation(
            "class C { @RequestMapping(method = RequestMethod.PUT) void m() {} }",
            |a, source| {
                assert_eq!(
                    named_identifier_tail(a.arguments, "method", source).as_deref(),
                    Some("PUT")
                );
            },
        );
    }

    #[test]
    fn bare_request_mapping_with_no_arguments_has_no_method_and_no_path() {
        with_method_annotation("class C { @RequestMapping void m() {} }", |a, source| {
            assert!(a.arguments.is_none());
            assert!(named_identifier_tail(a.arguments, "method", source).is_none());
            assert!(bare_string_values(a.arguments, source).is_empty());
            assert!(named_string_values(a.arguments, &["value", "path"], source).is_empty());
        });
    }

    #[test]
    fn empty_string_literal_argument_is_an_empty_string_not_absent() {
        with_method_annotation(r#"class C { @GetMapping("") void m() {} }"#, |a, source| {
            assert_eq!(bare_string_values(a.arguments, source), vec![""]);
        });
    }

    #[test]
    fn annotation_name_resolves_a_fully_qualified_annotation_to_its_simple_tail() {
        with_method_annotation(
            "class C { @org.springframework.web.bind.annotation.PostMapping void m() {} }",
            |a, _source| {
                assert_eq!(a.name, "PostMapping");
            },
        );
    }

    #[test]
    fn a_declaration_with_no_modifiers_field_has_no_annotations() {
        let src = "class C { void m() {} }";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let method = find_first(&tree.root_node(), "method_declaration")
            .expect("method declaration present");
        assert!(annotations_of(&method, bytes).is_empty());
    }

    #[test]
    fn several_annotations_on_one_declaration_are_all_recovered() {
        let src = "class C { @Deprecated @GetMapping(\"/x\") void m() {} }";
        let tree = parse(src);
        let bytes = src.as_bytes();
        let method = find_first(&tree.root_node(), "method_declaration")
            .expect("method declaration present");
        let names: Vec<_> = annotations_of(&method, bytes)
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, vec!["Deprecated", "GetMapping"]);
    }
}
