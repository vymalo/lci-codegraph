//! Callee-reference extraction: given an AST node identified as a call site, recover its bare callee
//! name plus, when the call is qualified by a receiver/path (`A::new`, `Foo.bar`), the qualifier used
//! solely as an ambiguity tiebreaker by [`super::resolve::pick`]. Two independent sources feed the same
//! [`CalleeRef`] shape: the Rust `call_expression` navigation ([`callee_ref_of`]), and the tags-captured
//! callee name node for every other language ([`qualifier_from_callee_node`] — tags identify *that* a
//! node is a callee but drop the qualifier, so it is recovered here from the node's tree position).

use tree_sitter::Node;

/// The callee of a Rust `call_expression`: its bare name plus, for a qualified path, the qualifier
/// segment. `A::new()` → `{name: "new", qualifier: Some("A")}`; `a::b::foo()` → `{"foo", Some("b")}`;
/// `foo()` / `x.foo()` → no qualifier (a method receiver's type is not known without inference).
pub(super) struct CalleeRef {
    pub(super) name: String,
    pub(super) qualifier: Option<String>,
}

pub(super) fn callee_ref_of(call: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
    let func = call.child_by_field_name("function")?;
    callee_ref(&func, bytes)
}

fn callee_ref(node: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => Some(CalleeRef {
            name: text(node, bytes)?,
            qualifier: None,
        }),
        // `a::b::foo` — callable is the final `name`; qualifier is the segment before it (`b`).
        "scoped_identifier" => {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| text(&n, bytes))?;
            let qualifier = node
                .child_by_field_name("path")
                .and_then(|p| path_tail(&p, bytes));
            Some(CalleeRef { name, qualifier })
        }
        // `x.foo` — the method name is the `field`; the receiver type is unknown, so no qualifier.
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| callee_ref(&n, bytes)),
        // `foo::<T>` — the callable is under the `function` field.
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|n| callee_ref(&n, bytes)),
        _ => None,
    }
}

/// The last segment of a path node — the receiver/namespace qualifier (`A` in `A`, `b` in `a::b`).
fn path_tail(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| text(&n, bytes)),
        _ => text(node, bytes),
    }
}

/// The implementing type of an `impl_item` (`impl S` / `impl T for S` / `impl Vec<T>` → `S` / `Vec`),
/// used to scope the methods it contains for same-name disambiguation.
pub(super) fn impl_type_name(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let ty = node.child_by_field_name("type")?;
    type_head_name(&ty, bytes)
}

/// The simple head name of a type node, generics/package-qualification/array-ness stripped:
/// `List<Account>` → `List`, `com.example.Account` → `Account`, `Account[]` → `Account`. Shared by
/// [`impl_type_name`] (Rust `impl` types) and, since Phase 0 (docs/design/spring-aware-graph.md §4.2),
/// [`declared_type_of`]/[`supertype_names`] (Java declared types and `extends`/`implements` clauses) —
/// each language's grammar shapes these node kinds differently, so most arms try a *field* first
/// (Rust's shape) and fall back to a *positional* child search (Java's shape, which tags these nodes
/// with no fields at all). Primitive types (`int`, `boolean`, `void`, …) have none of these node kinds
/// and fall through to `None` — deliberately: a primitive is never a useful qualifier.
fn type_head_name(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => text(node, bytes),
        "generic_type" => {
            // Rust's `generic_type` (`Vec<i32>`) tags the base type with a `type` field. Java's
            // `generic_type` (`List<Account>`) has NO fields at all (verified against
            // tree-sitter-java's node-types.json) — an untagged child list of `[base type,
            // type_arguments]` — so when the field lookup is empty, fall back to the first named
            // child that is itself a type, skipping `type_arguments`.
            if let Some(base) = node.child_by_field_name("type") {
                return type_head_name(&base, bytes);
            }
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|c| matches!(c.kind(), "type_identifier" | "scoped_type_identifier"))
                .and_then(|c| type_head_name(&c, bytes))
        }
        "scoped_type_identifier" => {
            // Rust's `path::Type` tags the tail segment with a `name` field.
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| text(&n, bytes))
            {
                return Some(name);
            }
            // Java's `com.example.Account` also has no fields — it nests recursively (the prefix
            // is itself a `scoped_type_identifier`), so the tail segment is simply the LAST named
            // `type_identifier` child (verified by parsing `com.example.Account` and walking the
            // tree: the innermost/rightmost `type_identifier` is always the simple type name).
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|c| c.kind() == "type_identifier")
                .last()
                .and_then(|n| text(&n, bytes))
        }
        // `Account[]` (Java) / `[T; 3]`-shaped Rust array types — both tag the element with an
        // `element` field, so no fallback is needed here.
        "array_type" => node
            .child_by_field_name("element")
            .and_then(|n| type_head_name(&n, bytes)),
        _ => None,
    }
}

/// The qualifier for a tags-captured callee name node: its receiver, when the name is the property of
/// a member access AND the receiver is plausibly a type (`Foo.bar()` → `Foo`; Java `Obj.m()`). A bare
/// call (`function`-field identifier), a `self`/`this`/`cls`/`super` receiver, or a **value** receiver
/// (a lowercase-initial variable/module, e.g. `obj` in `obj.foo()` — see [`receiver_qualifier`])
/// yields no qualifier — the call resolves on the bare name (single hit) or is dropped as ambiguous,
/// never mis-attributed.
pub(super) fn qualifier_from_callee_node(name_node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    let parent = name_node.parent()?;
    match parent.kind() {
        // JS/TS `obj.foo()`, Python `obj.foo()`, Java `obj.foo()` — the receiver is the `object` field.
        "member_expression" | "attribute" | "method_invocation" => parent
            .child_by_field_name("object")
            .and_then(|object| receiver_qualifier(&object, bytes)),
        _ => None,
    }
}

/// A receiver object as a qualifier, iff it is a plain identifier that could name a *type* (`Foo` in
/// `Foo.bar()`). Implicit-`self` receivers (`self`/`cls`/`this`/`super`) carry no type information, so
/// they yield no qualifier — the call resolves on the bare method name (single hit) or is dropped as
/// ambiguous, never mis-attributed by a bogus `self` qualifier.
///
/// Every other receiver goes through three tiers, most informative first. Issue #8 and
/// docs/design/spring-aware-graph.md §4.2 diagnosed the same defect and fixed it at different
/// depths; both fixes are kept, because they are not redundant — the first can *narrow* a
/// multi-candidate set, and the second and third cannot.
///
/// **1. The receiver's declared type ([`declared_type_of`]), Java only.** `accountService.findById()`
/// used to record the qualifier `"accountService"` — the variable name — which
/// [`super::resolve::pick`] compared against a candidate's `scope` (a *type* name,
/// `"AccountServiceImpl"`) and rejected as a mismatch, even though the call was completely
/// unambiguous. Recovering the declared type makes the qualifier `"AccountService"`, which `pick`
/// matches directly or, via [`supertype_names`], through the implementing type's own
/// `extends`/`implements` clause.
///
/// This tier is what tiers 2 and 3 cannot do: it supplies a real type name, so it still resolves when
/// there are **several** same-named candidates. `AccountServiceImpl.findByEmail` and
/// `AccountRepository.findByEmail` are told apart only by their receivers' declared types; under
/// tier 3 alone both calls would see two candidates, no qualifier, and be dropped as ambiguous.
///
/// **2. A capitalised receiver (`Foo.bar()`).** No declaration found, but the Java/TypeScript/Python
/// naming convention says a capitalised identifier plausibly names a type. This is the pre-existing
/// static-call behaviour and the only tier available to the non-Java tags languages.
///
/// **3. Otherwise, no qualifier at all.** The mechanism below, from issue #8:
///
/// The same is true, and for the same reason, of a **value** receiver — a local variable holding an
/// instance (`a` in `a.helper()`) or a module (`math` in `math.sqrt()`). This extractor has no type
/// inference: it cannot tell what `a` was assigned, so a qualifier built from `a`'s own text would
/// almost never equal the callable's declaring-type `scope` (`A`) and [`super::resolve::pick`]'s
/// single-candidate branch would reject the one correct candidate — issue #8. We use the Java/
/// TypeScript/Python naming convention (types/classes are capitalised; variables and modules are not)
/// as a cheap, no-inference proxy for "is this plausibly a type": a capitalised identifier stays a
/// qualifier (`Foo.bar()`, and the constant-receiver case `CONFIG.get()`, which reads as a type/
/// singular-instance qualifier under the same convention and is accepted on the same terms); a
/// lowercase-initial identifier yields `None` and the call falls through to bare-name resolution,
/// exactly like `foo()` — single candidate resolves, several candidates with no qualifier are dropped
/// as ambiguous.
///
/// Precision trade-off: dropping the qualifier means `a.run()` could resolve to the single `run`
/// defined on a class `a` was never actually assigned. This is deliberately no *new* risk — a bare
/// `run()` call already resolves to a lone same-named candidate with zero type checking, and every
/// value-receiver call the old code silently discarded is textually indistinguishable from a bare
/// call once its (never-matching) qualifier is set aside. This makes value-receiver calls behave like
/// bare calls instead of manufacturing a false negative that dominates real codebases (see issue #8:
/// this is the single most common call shape in idiomatic Java/TS/Python and was recording zero
/// `calls` edges for all of it).
fn receiver_qualifier(object: &Node<'_>, bytes: &[u8]) -> Option<String> {
    if object.kind() != "identifier" {
        return None;
    }
    let name = text(object, bytes)?;
    match name.as_str() {
        "self" | "cls" | "this" | "super" => None,
        // Tier 1: the receiver's declared type (Java). Tier 2: a capitalised receiver, which
        // plausibly names a type. Tier 3: nothing — fall through to bare-name resolution.
        _ => match declared_type_of(object, &name, bytes) {
            Some(declared) => Some(declared),
            None if is_type_like(&name) => Some(name),
            None => None,
        },
    }
}

/// Recover a Java identifier's **declared type** by walking up from its use site through enclosing
/// scopes — the nearest local block, the enclosing method/constructor's parameters, then the
/// enclosing class's fields — checking, at each level, the declaration node kinds that bind a name to
/// a type. This is Phase 0 of docs/design/spring-aware-graph.md §4.2, item 2: it is what turns a call
/// qualifier from the *variable name* (`accountService`) into the *declared type* (`AccountService`)
/// that [`super::resolve::pick`] actually needs to match a candidate's scope.
///
/// Nearest-enclosing-scope wins: the walk goes innermost-out and returns on the first match, so a
/// local shadowing a field of the same name resolves to the *local's* type, never the field's.
///
/// This is a Java-only lookup **in effect**, not by a language flag. Every node kind matched below
/// (`local_variable_declaration`, `formal_parameter`, `field_declaration`, `enhanced_for_statement`,
/// `catch_formal_parameter`) is specific to the Java grammar — none of them exist in the Python/JS/TS
/// grammars [`qualifier_from_callee_node`] also serves, so for those languages every arm below is a
/// dead end and the walk simply returns `None`, falling back to the pre-Phase-0 bare-identifier
/// behaviour. The node-kind match IS the gate: no `language == "java"` check, no feature flag, no
/// extra plumbing required to keep this inert everywhere else.
///
/// Not attempted (left as a documented gap, not silently wrong): try-with-resources `resource`
/// declarations. Unlike every kind above, a `resource` node's `type`/`name` fields are both genuinely
/// optional in the grammar (`try (existingVar)` reuses an outer variable with neither) — resolving
/// which shape applies is not the "one obvious field lookup" every other kind here is, so a call
/// through a try-with-resources variable falls back to the bare-identifier qualifier instead.
fn declared_type_of(use_site: &Node<'_>, name: &str, bytes: &[u8]) -> Option<String> {
    let mut current = *use_site;
    while let Some(parent) = current.parent() {
        let found = match parent.kind() {
            // A block (method/constructor body, or any nested `{ ... }`) or a class/interface body
            // — both hold their locals/fields as direct children.
            "block" | "class_body" | "interface_body" | "constructor_body" | "enum_body" => {
                scan_declarations(&parent, name, bytes)
            }
            "method_declaration" | "constructor_declaration" => parent
                .child_by_field_name("parameters")
                .and_then(|params| scan_formal_parameters(&params, name, bytes)),
            "enhanced_for_statement" => enhanced_for_declared_type(&parent, name, bytes),
            "catch_clause" => catch_declared_type(&parent, name, bytes),
            _ => None,
        };
        if found.is_some() {
            return found;
        }
        current = parent;
    }
    None
}

/// A scope's direct children for a `local_variable_declaration`/`field_declaration` that binds `name`.
/// Only direct children are examined — a nested block's own locals are checked at their own, nearer
/// level of [`declared_type_of`]'s outward walk, never here.
fn scan_declarations(scope: &Node<'_>, name: &str, bytes: &[u8]) -> Option<String> {
    let mut cursor = scope.walk();
    scope
        .children(&mut cursor)
        .filter(|c| matches!(c.kind(), "local_variable_declaration" | "field_declaration"))
        .find_map(|decl| declarator_type_if_named(&decl, name, bytes))
}

/// A `local_variable_declaration`/`field_declaration`'s type, iff one of its (possibly several)
/// `variable_declarator`s binds `name`. Java allows multi-declarator statements sharing one `type`
/// field (`int a, b;`), so every declarator is checked, not just the first.
fn declarator_type_if_named(decl: &Node<'_>, name: &str, bytes: &[u8]) -> Option<String> {
    let ty = decl.child_by_field_name("type")?;
    let mut cursor = decl.walk();
    let binds_name = decl
        .children_by_field_name("declarator", &mut cursor)
        .any(|d| declarator_name(&d, bytes).as_deref() == Some(name));
    binds_name.then(|| type_head_name(&ty, bytes)).flatten()
}

/// A `formal_parameters` node's children for a `formal_parameter` binding `name`.
fn scan_formal_parameters(params: &Node<'_>, name: &str, bytes: &[u8]) -> Option<String> {
    let mut cursor = params.walk();
    params
        .children(&mut cursor)
        .filter(|c| c.kind() == "formal_parameter")
        .find(|p| {
            p.child_by_field_name("name")
                .and_then(|n| text(&n, bytes))
                .as_deref()
                == Some(name)
        })
        .and_then(|p| p.child_by_field_name("type"))
        .and_then(|ty| type_head_name(&ty, bytes))
}

/// `enhanced_for_statement`'s own `name`/`type` fields (`for (Account a : accounts) { ... }`).
fn enhanced_for_declared_type(node: &Node<'_>, name: &str, bytes: &[u8]) -> Option<String> {
    let binds_name = node
        .child_by_field_name("name")
        .and_then(|n| text(&n, bytes))
        .as_deref()
        == Some(name);
    binds_name
        .then(|| node.child_by_field_name("type"))
        .flatten()
        .and_then(|ty| type_head_name(&ty, bytes))
}

/// A `catch_clause`'s `catch_formal_parameter` (`catch (IOException e) { ... }`). Java's multi-catch
/// (`catch (IOException | SQLException e)`) has no single declared type, so it is deliberately left
/// unresolved — never guess one of several alternatives.
fn catch_declared_type(clause: &Node<'_>, name: &str, bytes: &[u8]) -> Option<String> {
    let mut cursor = clause.walk();
    let param = clause
        .children(&mut cursor)
        .find(|c| c.kind() == "catch_formal_parameter")?;
    let binds_name = param
        .child_by_field_name("name")
        .and_then(|n| text(&n, bytes))
        .as_deref()
        == Some(name);
    if !binds_name {
        return None;
    }
    let mut cursor = param.walk();
    let catch_type = param
        .children(&mut cursor)
        .find(|c| c.kind() == "catch_type")?;
    let mut cursor = catch_type.walk();
    let mut alternatives = catch_type.named_children(&mut cursor);
    let only = alternatives.next()?;
    if alternatives.next().is_some() {
        return None; // multi-catch — no single declared type, don't guess.
    }
    type_head_name(&only, bytes)
}

/// A `variable_declarator`'s bound name.
fn declarator_name(declarator: &Node<'_>, bytes: &[u8]) -> Option<String> {
    declarator
        .child_by_field_name("name")
        .and_then(|n| text(&n, bytes))
}

/// The simple names a Java type container's `extends`/`implements` clause names — used by
/// [`super::resolve::pick`] so a call qualifier naming an interface/superclass counts as a match
/// against a candidate whose own scope is the concrete implementing/extending type, not a
/// contradiction. See docs/design/spring-aware-graph.md §4.2, item 2:
/// `accountService.findById()`'s recovered declared-type qualifier is `AccountService`, but the sole
/// candidate's `scope` is `AccountServiceImpl` — without this, a *correct* declared-type qualifier
/// would still read as a mismatch.
///
/// Reads `class_declaration`'s `superclass`/`interfaces` **fields** (`extends`/`implements`) — a node
/// kind lacking a given field simply yields nothing for it, so this half needs no `match` on
/// `node.kind()` at all, and every other language's container node kinds have none of these fields
/// either, so this returns empty there too. `interface_declaration`'s `extends` clause
/// (`extends_interfaces`) is handled separately below: verified against tree-sitter-java's
/// `node-types.json`, it is NOT a tagged field the way the other two are — it's an untagged child, so
/// it needs a positional search instead of `child_by_field_name`.
pub(super) fn supertype_names(node: &Node<'_>, bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for field in ["superclass", "interfaces"] {
        if let Some(clause) = node.child_by_field_name(field) {
            collect_type_head_names(&clause, bytes, &mut names);
        }
    }
    let mut cursor = node.walk();
    if let Some(extends) = node
        .children(&mut cursor)
        .find(|c| c.kind() == "extends_interfaces")
    {
        collect_type_head_names(&extends, bytes, &mut names);
    }
    names
}

/// Recursively collect every type's simple head name under `node` — flattens a `superclass` (wraps one
/// type directly) and a `super_interfaces`/`extends_interfaces` (wraps a `type_list` of possibly
/// several types, `Foo, Bar, Baz`) uniformly, without a separate case for each wrapper shape.
fn collect_type_head_names(node: &Node<'_>, bytes: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "type_identifier" | "generic_type" | "scoped_type_identifier" | "array_type" => {
            if let Some(name) = type_head_name(node, bytes) {
                out.push(name);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_type_head_names(&child, bytes, out);
            }
        }
    }
}

/// Java/TypeScript/Python naming convention: a capitalised identifier plausibly names a *type*
/// (`Foo`, or an all-caps constant like `CONFIG`); a lowercase-initial identifier names a *value* — a
/// local variable or a module (`a`, `math`) — whose actual type this extractor does not track. See
/// [`receiver_qualifier`] for how this is used and why.
fn is_type_like(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn text(node: &Node<'_>, bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(&bytes[node.byte_range()])
        .ok()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    //! Unit tests for callee-reference parsing, isolated from the shared DFS walk in [`super::emit`].
    //! Real tree-sitter parses (never hand-mocked nodes) for both feed paths: the Rust
    //! `call_expression` navigation ([`callee_ref_of`]/[`impl_type_name`]), and the tags-captured
    //! callee name node other languages use ([`qualifier_from_callee_node`]).
    use super::*;
    use crate::lang;

    /// Parse Rust `src`, find the first node of `kind` in the tree (pre-order DFS), and pass it to
    /// `f`. Panics if no such node exists — test ergonomics.
    fn with_rust_node<R>(src: &str, kind: &str, f: impl FnOnce(&Node<'_>, &[u8]) -> R) -> R {
        let tree = lang::parse(src, "rust").expect("rust source parses");
        let bytes = src.as_bytes();
        let node = find_first(&tree.root_node(), kind)
            .unwrap_or_else(|| panic!("no {kind:?} node in {src:?}"));
        f(&node, bytes)
    }

    fn find_first<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
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

    // ── Rust `call_expression` navigation ──────────────────────────────────────────────────────

    #[test]
    fn bare_call_has_name_and_no_qualifier() {
        with_rust_node("fn go() { foo(); }", "call_expression", |call, bytes| {
            let r = callee_ref_of(call, bytes).expect("bare call has a callee ref");
            assert_eq!(r.name, "foo");
            assert_eq!(r.qualifier, None);
        });
    }

    #[test]
    fn scoped_call_captures_name_and_its_qualifier() {
        with_rust_node("fn go() { A::new(); }", "call_expression", |call, bytes| {
            let r = callee_ref_of(call, bytes).expect("scoped call has a callee ref");
            assert_eq!(r.name, "new");
            assert_eq!(r.qualifier.as_deref(), Some("A"));
        });
    }

    #[test]
    fn nested_scoped_call_qualifier_is_the_segment_before_the_name() {
        // `a::b::foo()` — callable is `foo`; the qualifier is the segment directly before it (`b`),
        // not the whole path.
        with_rust_node(
            "fn go() { a::b::foo(); }",
            "call_expression",
            |call, bytes| {
                let r = callee_ref_of(call, bytes).expect("nested scoped call has a callee ref");
                assert_eq!(r.name, "foo");
                assert_eq!(r.qualifier.as_deref(), Some("b"));
            },
        );
    }

    #[test]
    fn method_call_field_expression_has_no_qualifier() {
        // `x.foo()` — the receiver's type is unknown without inference, so no qualifier is recovered
        // (a bogus qualifier would risk mis-attribution).
        with_rust_node(
            "fn go(x: X) { x.foo(); }",
            "call_expression",
            |call, bytes| {
                let r = callee_ref_of(call, bytes).expect("method call has a callee ref");
                assert_eq!(r.name, "foo");
                assert_eq!(r.qualifier, None);
            },
        );
    }

    #[test]
    fn chained_method_call_resolves_to_the_final_segment() {
        // `a.b().c()` — the outer call's callee is `c`; the qualifier is still unknown (chained off
        // another call's result, not an identifier).
        with_rust_node(
            "fn go(a: A) { a.b().c(); }",
            "call_expression",
            |call, bytes| {
                let r = callee_ref_of(call, bytes).expect("outer call has a callee ref");
                assert_eq!(r.name, "c");
                assert_eq!(r.qualifier, None);
            },
        );
    }

    #[test]
    fn generic_function_call_unwraps_to_the_inner_callee() {
        with_rust_node(
            "fn go() { foo::<T>(); }",
            "call_expression",
            |call, bytes| {
                let r = callee_ref_of(call, bytes).expect("turbofish call has a callee ref");
                assert_eq!(r.name, "foo");
                assert_eq!(r.qualifier, None);
            },
        );
    }

    #[test]
    fn non_call_node_yields_no_callee_ref() {
        // An `index_expression` has no `function` field — `callee_ref_of` must yield `None`, not
        // panic or misparse.
        with_rust_node(
            "fn go(arr: [i32; 3]) { let _ = arr[0]; }",
            "index_expression",
            |node, bytes| {
                assert!(callee_ref_of(node, bytes).is_none());
            },
        );
    }

    // ── `impl_type_name` ────────────────────────────────────────────────────────────────────────

    #[test]
    fn impl_type_name_for_a_plain_inherent_impl() {
        with_rust_node("impl S { fn f() {} }", "impl_item", |node, bytes| {
            assert_eq!(impl_type_name(node, bytes).as_deref(), Some("S"));
        });
    }

    #[test]
    fn impl_type_name_for_a_trait_impl_is_the_implementing_type_not_the_trait() {
        with_rust_node("impl T for S { fn f() {} }", "impl_item", |node, bytes| {
            assert_eq!(impl_type_name(node, bytes).as_deref(), Some("S"));
        });
    }

    #[test]
    fn impl_type_name_for_a_generic_impl_uses_the_head_type() {
        with_rust_node("impl Vec<i32> { fn f() {} }", "impl_item", |node, bytes| {
            assert_eq!(impl_type_name(node, bytes).as_deref(), Some("Vec"));
        });
    }

    // ── `qualifier_from_callee_node` (tags-captured callee name node) ─────────────────────────────

    /// Parse `src` with `language`, find the first node of `kind`, and pass it to `f`.
    fn with_tagged_node<R>(
        src: &str,
        language: &str,
        kind: &str,
        f: impl FnOnce(&Node<'_>, &[u8]) -> R,
    ) -> R {
        let tree = lang::parse(src, language).expect("source parses");
        let bytes = src.as_bytes();
        let node = find_first(&tree.root_node(), kind)
            .unwrap_or_else(|| panic!("no {kind:?} node in {src:?}"));
        f(&node, bytes)
    }

    #[test]
    fn js_member_expression_receiver_is_the_qualifier() {
        // `Foo.bar()` — the callee name node is the `property` field; its qualifier is the receiver.
        with_tagged_node(
            "Foo.bar();",
            "javascript",
            "member_expression",
            |member, bytes| {
                let name_node = member.child_by_field_name("property").unwrap();
                assert_eq!(
                    qualifier_from_callee_node(&name_node, bytes).as_deref(),
                    Some("Foo")
                );
            },
        );
    }

    #[test]
    fn python_capitalised_attribute_receiver_is_the_qualifier() {
        // `Obj.m()` — a capitalised receiver plausibly names a type, so it stays a qualifier.
        with_tagged_node("Obj.m()", "python", "attribute", |attr, bytes| {
            let name_node = attr.child_by_field_name("attribute").unwrap();
            assert_eq!(
                qualifier_from_callee_node(&name_node, bytes).as_deref(),
                Some("Obj")
            );
        });
    }

    #[test]
    fn java_capitalised_method_invocation_receiver_is_the_qualifier() {
        // `Obj.m()` — a capitalised receiver plausibly names a type, so it stays a qualifier.
        with_tagged_node(
            "class C { void go() { Obj.m(); } }",
            "java",
            "method_invocation",
            |call, bytes| {
                let name_node = call.child_by_field_name("name").unwrap();
                assert_eq!(
                    qualifier_from_callee_node(&name_node, bytes).as_deref(),
                    Some("Obj")
                );
            },
        );
    }

    // ── issue #8: a lowercase-initial (value) receiver yields NO qualifier ─────────────────────────

    #[test]
    fn python_lowercase_attribute_receiver_yields_no_qualifier() {
        // `obj.m()` — `obj` is a variable, not a type; recording it as a qualifier is exactly issue
        // #8 (the qualifier can never textually match the callable's declaring-type scope, so the
        // one correct candidate gets rejected). No qualifier lets the call fall through to bare-name
        // resolution instead.
        with_tagged_node("obj.m()", "python", "attribute", |attr, bytes| {
            let name_node = attr.child_by_field_name("attribute").unwrap();
            assert_eq!(qualifier_from_callee_node(&name_node, bytes), None);
        });
    }

    #[test]
    fn java_lowercase_method_invocation_receiver_yields_no_qualifier() {
        // `obj.m()` — same as the Python case above: a value receiver, not a type.
        with_tagged_node(
            "class C { void go() { obj.m(); } }",
            "java",
            "method_invocation",
            |call, bytes| {
                let name_node = call.child_by_field_name("name").unwrap();
                assert_eq!(qualifier_from_callee_node(&name_node, bytes), None);
            },
        );
    }

    #[test]
    fn js_lowercase_member_expression_receiver_yields_no_qualifier() {
        // `a.helper()` — the exact reproduction shape from issue #8.
        with_tagged_node(
            "a.helper();",
            "javascript",
            "member_expression",
            |member, bytes| {
                let name_node = member.child_by_field_name("property").unwrap();
                assert_eq!(qualifier_from_callee_node(&name_node, bytes), None);
            },
        );
    }

    #[test]
    fn all_caps_constant_receiver_is_still_treated_as_a_type_qualifier() {
        // `CONFIG.get()` — an all-caps identifier is capitalised, so it stays a qualifier under the
        // same convention as `Foo.bar()`. Documented, accepted trade-off: see `receiver_qualifier`'s
        // doc comment.
        with_tagged_node(
            "CONFIG.get();",
            "javascript",
            "member_expression",
            |member, bytes| {
                let name_node = member.child_by_field_name("property").unwrap();
                assert_eq!(
                    qualifier_from_callee_node(&name_node, bytes).as_deref(),
                    Some("CONFIG")
                );
            },
        );
    }

    #[test]
    fn implicit_self_family_receivers_yield_no_qualifier() {
        // `self`/`cls`/`this`/`super` carry no type information — a call through any of them must
        // resolve on the bare method name, never a bogus qualifier.
        for receiver in ["self", "cls", "this", "super"] {
            let src = format!("{receiver}.m();");
            with_tagged_node(&src, "javascript", "member_expression", |member, bytes| {
                let name_node = member.child_by_field_name("property").unwrap();
                assert_eq!(
                    qualifier_from_callee_node(&name_node, bytes),
                    None,
                    "{receiver} must not yield a qualifier"
                );
            });
        }
    }

    #[test]
    fn non_identifier_receiver_yields_no_qualifier() {
        // `getObj().bar()` — the receiver is a call expression, not a plain identifier that could
        // name a type, so no qualifier is recovered.
        with_tagged_node(
            "getObj().bar();",
            "javascript",
            "member_expression",
            |member, bytes| {
                let name_node = member.child_by_field_name("property").unwrap();
                assert_eq!(qualifier_from_callee_node(&name_node, bytes), None);
            },
        );
    }

    #[test]
    fn bare_call_name_node_yields_no_qualifier() {
        // `foo()` — the name node's parent is the call itself, not a member/attribute access.
        with_tagged_node("foo();", "javascript", "call_expression", |call, bytes| {
            let name_node = call.child_by_field_name("function").unwrap();
            assert_eq!(qualifier_from_callee_node(&name_node, bytes), None);
        });
    }

    // ── `declared_type_of` (Phase 0 declared-type qualifier recovery, via
    //    `qualifier_from_callee_node` — the same entry point `emit::walk` calls, so these tests
    //    exercise the real code path, not the private helper directly) ─────────────────────────────

    fn java_qualifier_of_the_only_call(src: &str) -> Option<String> {
        with_tagged_node(src, "java", "method_invocation", |call, bytes| {
            let name_node = call.child_by_field_name("name").unwrap();
            qualifier_from_callee_node(&name_node, bytes)
        })
    }

    #[test]
    fn java_qualifier_recovers_a_local_variables_declared_type() {
        // `Helper h = new Helper(); h.help();` — the qualifier must be `Helper`, the declared
        // type, not `h`, the variable name. This is the §4.2 headline fixture.
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { void go() { Helper h = new Helper(); h.help(); } }"
            )
            .as_deref(),
            Some("Helper")
        );
    }

    #[test]
    fn java_qualifier_recovers_a_formal_parameters_declared_type() {
        assert_eq!(
            java_qualifier_of_the_only_call("class C { void go(Helper h) { h.help(); } }")
                .as_deref(),
            Some("Helper")
        );
    }

    #[test]
    fn java_qualifier_recovers_a_fields_declared_type() {
        assert_eq!(
            java_qualifier_of_the_only_call("class C { Helper h; void go() { h.help(); } }")
                .as_deref(),
            Some("Helper")
        );
    }

    #[test]
    fn java_qualifier_prefers_a_local_over_a_field_of_the_same_name() {
        // Nearest-enclosing-scope wins: a local `Panel widget` shadows the `Widget widget` field.
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { Widget widget; void go() { \
                 Panel widget = new Panel(); widget.render(); } }"
            )
            .as_deref(),
            Some("Panel"),
            "the local must shadow the field"
        );
    }

    #[test]
    fn java_qualifier_strips_generics_to_the_simple_head_name() {
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { void go() { List<Account> items = null; items.size(); } }"
            )
            .as_deref(),
            Some("List")
        );
    }

    #[test]
    fn java_qualifier_strips_package_qualification_to_the_simple_head_name() {
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { void go() { com.example.Account acc = null; acc.save(); } }"
            )
            .as_deref(),
            Some("Account")
        );
    }

    #[test]
    fn java_qualifier_strips_array_suffix_to_the_element_type() {
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { void go() { Account[] accs = null; accs.clone(); } }"
            )
            .as_deref(),
            Some("Account")
        );
    }

    #[test]
    fn declared_type_of_does_not_map_a_primitive_type_to_a_name() {
        // A primitive-typed receiver's declared type must be `None`, never a bogus `"int"` — a
        // primitive is never a useful qualifier (design doc §4.2, item 2). Uses the exact node
        // shape production code passes (the receiver identifier of a `method_invocation`'s
        // `object` field), not a stand-in.
        with_tagged_node(
            "class C { void go(int x) { x.foo(); } }",
            "java",
            "method_invocation",
            |call, bytes| {
                let receiver = call.child_by_field_name("object").unwrap();
                assert_eq!(declared_type_of(&receiver, "x", bytes), None);
            },
        );
    }

    #[test]
    fn java_qualifier_of_a_primitive_receiver_is_dropped_entirely() {
        // Calling `.foo()` on an `int` isn't valid Java, but the grammar parses it structurally
        // regardless. `declared_type_of` refuses to map the primitive `int` to a name (see the
        // direct test above), so tier 1 finds nothing; `x` is lowercase, so tier 2 declines too;
        // tier 3 drops the qualifier. This is the "no usable declaration found" path, not a
        // special primitive case.
        assert_eq!(
            java_qualifier_of_the_only_call("class C { void go(int x) { x.foo(); } }"),
            None
        );
    }

    #[test]
    fn java_qualifier_is_dropped_when_no_declaration_is_found() {
        // No local/field/parameter named `obj` exists anywhere in scope, so tier 1 finds nothing
        // and `obj` is lowercase, so tier 2 declines. The qualifier is DROPPED rather than set to
        // the bare receiver text (issue #8): a variable name is not a type name, and keeping it
        // could only ever contradict a candidate's scope and reject a correct answer. The call
        // then resolves on the bare name, exactly like `m()` would.
        assert_eq!(
            java_qualifier_of_the_only_call("class C { void go() { obj.m(); } }"),
            None
        );
    }

    #[test]
    fn java_qualifier_recovers_an_enhanced_for_loop_variables_declared_type() {
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { void go() { for (Account a : accounts) { a.save(); } } }"
            )
            .as_deref(),
            Some("Account")
        );
    }

    #[test]
    fn java_qualifier_recovers_a_caught_exceptions_declared_type() {
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { void go() { \
                 try {} catch (IOException e) { e.printStackTrace(); } } }"
            )
            .as_deref(),
            Some("IOException")
        );
    }

    #[test]
    fn java_qualifier_does_not_recover_a_multi_catch_declared_type() {
        // `catch (IOException | SQLException e)` has no SINGLE declared type — must not guess one
        // of the alternatives. With no type recovered and `e` lowercase, the qualifier is dropped.
        assert_eq!(
            java_qualifier_of_the_only_call(
                "class C { void go() { \
                 try {} catch (IOException | SQLException e) { e.printStackTrace(); } } }"
            ),
            None
        );
    }

    // ── `supertype_names` (Phase 0 supertype-aware qualifier matching) ─────────────────────────────

    #[test]
    fn supertype_names_of_a_class_collects_both_the_superclass_and_interfaces() {
        with_tagged_node(
            "class Impl extends Base implements Foo, Bar {}",
            "java",
            "class_declaration",
            |class, bytes| {
                let mut names = supertype_names(class, bytes);
                names.sort();
                assert_eq!(names, vec!["Bar", "Base", "Foo"]);
            },
        );
    }

    #[test]
    fn supertype_names_of_an_interface_collects_extends_interfaces() {
        with_tagged_node(
            "interface Combined extends Foo, Bar {}",
            "java",
            "interface_declaration",
            |iface, bytes| {
                let mut names = supertype_names(iface, bytes);
                names.sort();
                assert_eq!(names, vec!["Bar", "Foo"]);
            },
        );
    }

    #[test]
    fn supertype_names_strips_generic_type_arguments() {
        with_tagged_node(
            "interface AccountRepository extends JpaRepository<Account, Long> {}",
            "java",
            "interface_declaration",
            |iface, bytes| {
                assert_eq!(supertype_names(iface, bytes), vec!["JpaRepository"]);
            },
        );
    }

    #[test]
    fn supertype_names_of_a_container_with_no_clause_is_empty() {
        with_tagged_node(
            "class Plain {}",
            "java",
            "class_declaration",
            |class, bytes| {
                assert!(supertype_names(class, bytes).is_empty());
            },
        );
    }
}
