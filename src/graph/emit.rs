//! Node/edge emission: the tree-sitter DFS that turns one parsed file into [`super::FileSymbols`] —
//! def nodes, intra-file `contains`/`method` edges, and the unresolved call sites the cross-file
//! [`super::resolve`] pass consumes. The definition/call *classifier* ([`Classifier`]) is the seam
//! between this shared walk and the two ways a language identifies defs + calls: Rust's own
//! tree-sitter node-kind extractor (byte-stable golden), or the grammar's bundled `tags.scm` query
//! ([`crate::tags`]) every other language uses.

use std::sync::Arc;

use tree_sitter::{Node, Tree};

use super::callee::{self, CalleeRef};
use super::{CallSite, Callable, FileSymbols, GraphEdge, GraphNode};
use crate::chunk::interesting_node;
use crate::lang::{self, GraphStrategy, LanguageSupport};
use crate::tags;

/// Extract the structural facts for one **pre-parsed** file. `language` gates which languages produce
/// a graph (Rust, or any language [`crate::tags::extract`] handles); an unsupported language yields
/// empty facts (no structural graph — semantic search still covers it). `source_file` is the
/// repo-relative, forward-slashed path used as the file node id.
#[must_use]
pub fn extract_file(tree: &Tree, source_file: &str, source: &str, language: &str) -> FileSymbols {
    let mut facts = FileSymbols::default();
    // The definition/call *classifier* is chosen by the language's registry [`GraphStrategy`]: Rust
    // keeps its own node-kind extractor (byte-stable golden); every other language identifies defs +
    // calls via the grammar's bundled `tags.scm` query. An unknown language, or one with no graph
    // extractor, yields empty facts. The `tagged` binding owns the query result the
    // `Classifier::Tagged` variant borrows for the walk.
    let tagged;
    let classifier = match lang::by_id(language).and_then(LanguageSupport::graph_strategy) {
        Some(GraphStrategy::RustNative) => Classifier::Rust,
        Some(GraphStrategy::Tags(_)) => match tags::extract(language, tree, source) {
            Some(symbols) => {
                tagged = symbols;
                Classifier::Tagged(&tagged)
            }
            None => return facts,
        },
        None => return facts,
    };

    // The file node: id = the path, label = the file name, line 1 (1-based, matching Graphify's `L1`
    // file node). Top-level defs are `contains`-ed by it. For Python/JS the file *is* the module unit.
    facts.nodes.push(GraphNode {
        node_id: source_file.to_string(),
        label: file_label(source_file),
        source_file: source_file.to_string(),
        start_line: 1,
    });

    let bytes = source.as_bytes();
    let root = tree.root_node();
    // Stack of enclosing definition node ids (innermost last) for `contains` parenting + attributing
    // a call site to the definition it sits inside.
    let mut stack: Vec<String> = Vec::new();
    walk(
        classifier,
        &root,
        bytes,
        source_file,
        &mut stack,
        None,
        None,
        &mut facts,
    );
    facts
}

/// The nearest enclosing type scope threaded down through [`walk`]: the type's own simple name (used
/// to disambiguate same-named callables, as before) plus — since Phase 0
/// (docs/design/spring-aware-graph.md §4.2, item 2) — the simple names of whatever it
/// `extends`/`implements` ([`callee::supertype_names`]), so [`super::resolve::pick`] can accept a
/// qualifier that names an interface/superclass, not only an exact scope match. Bundled into one
/// struct rather than adding a fourth thread-through parameter to `walk`, which already carries
/// `#[allow(clippy::too_many_arguments)]`.
#[derive(Clone, Default)]
struct EnclosingScope {
    name: Option<String>,
    supers: Arc<[String]>,
}

/// DFS that, in one pass, emits def nodes + `contains`/`method` edges and records call sites
/// attributed to the innermost enclosing def. `scope` is the nearest enclosing [`EnclosingScope`] (for
/// same-name method disambiguation, and — Phase 0 — supertype-aware qualifier matching);
/// `enclosing_kind` is the nearest enclosing def kind (to decide `contains` vs `method`).
#[allow(clippy::too_many_arguments)]
fn walk(
    classifier: Classifier<'_>,
    node: &Node<'_>,
    bytes: &[u8],
    source_file: &str,
    stack: &mut Vec<String>,
    scope: Option<&EnclosingScope>,
    enclosing_kind: Option<&str>,
    facts: &mut FileSymbols,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((kind, name)) = classifier.classify(&child, bytes) {
            // 1-based line (Graphify parity).
            let start_line = child.start_position().row as i64 + 1;
            // `def_node_id` lives in `lci-codegraph-model` (shared with any framework extractor that
            // needs to produce byte-identical ids for the same definitions) and is a plain formatter —
            // the kind-fallback for a nameless def is this call site's decision, not the helper's.
            let node_id = lci_codegraph_model::def_node_id(
                source_file,
                start_line,
                name.as_deref().unwrap_or(kind),
            );
            let parent_id = stack
                .last()
                .cloned()
                .unwrap_or_else(|| source_file.to_string());
            let is_callable = kind == "function" || kind == "method";
            // A callable directly inside a type container is a *method* (Graphify emits `method`);
            // everything else nests via `contains`.
            let relation = if is_callable && is_type_container(enclosing_kind) {
                "method"
            } else {
                "contains"
            };
            facts.contains.push(GraphEdge {
                source: parent_id,
                target: node_id.clone(),
                relation: relation.to_string(),
            });
            // Label parity with Graphify: callables carry a `()` suffix (`add` → `add()`).
            let label = match name.clone() {
                Some(n) if is_callable => format!("{n}()"),
                Some(n) => n,
                None => kind.to_string(),
            };
            facts.nodes.push(GraphNode {
                node_id: node_id.clone(),
                label,
                source_file: source_file.to_string(),
                start_line,
            });
            // Record for resolution, tagged with type scope. Note this is `is_call_target`, NOT
            // `is_callable`: a bodiless trait method is callable-shaped (it earned the `()` label
            // and the `method` edge above) but must not compete with its own implementations for
            // the name — see `Classifier::is_call_target`.
            if classifier.is_call_target(&child, kind)
                && let Some(n) = name.clone()
            {
                facts.callables.push(Callable {
                    name: n,
                    node_id: node_id.clone(),
                    scope: scope.and_then(|s| s.name.clone()),
                    scope_supers: scope.map_or_else(|| Arc::from([]), |s| Arc::clone(&s.supers)),
                });
            }
            // The type scope introduced for this def's children (qualifies nested methods, and —
            // Phase 0 — carries its `extends`/`implements` supers for `pick`'s qualifier matching).
            // Computed only when `child` actually introduces a name scope (a type container): a
            // non-container def (e.g. a plain function) resets scope to `None` for its own
            // children, exactly as before this change.
            let child_name = classifier.container_scope(&child, kind, name.as_deref(), bytes);
            let next_scope_owner;
            let next_scope: Option<&EnclosingScope> = match child_name {
                Some(child_name) => {
                    let supers = callee::supertype_names(&child, bytes);
                    next_scope_owner = EnclosingScope {
                        name: Some(child_name),
                        supers: Arc::from(supers),
                    };
                    Some(&next_scope_owner)
                }
                None => None,
            };
            stack.push(node_id);
            walk(
                classifier,
                &child,
                bytes,
                source_file,
                stack,
                next_scope,
                Some(kind),
                facts,
            );
            stack.pop();
        } else {
            if let Some(caller) = stack.last()
                && let Some(callee) = classifier.call_site(&child, bytes)
            {
                facts.calls.push(CallSite {
                    caller: caller.clone(),
                    name: callee.name,
                    qualifier: callee.qualifier,
                });
            }
            walk(
                classifier,
                &child,
                bytes,
                source_file,
                stack,
                scope,
                enclosing_kind,
                facts,
            );
        }
    }
}

/// True when a callable directly inside a def of this kind is a *method* (joined by a `method` edge)
/// rather than a plain nested `contains`. Type containers across languages: Rust `impl`/`trait`/
/// `struct`/`enum`, and the `class`/`interface`/`enum` of Python, TS/JS, and Java.
fn is_type_container(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("impl" | "trait" | "struct" | "enum" | "class" | "interface")
    )
}

/// The definition/call *classifier* the shared walk consults. Two sources, one uniform result.
/// **Rust** keeps its own tree-sitter node-kind extractor (the chunker's `interesting_node` plus the
/// Rust `call_expression` navigation) so chunk and graph symbols stay in lock-step and the committed
/// golden is byte-stable. **Every other language** is classified by the grammar's bundled `tags.scm`
/// query ([`crate::tags`]). Everything downstream is identical regardless of source — the
/// `contains`/`method` edge choice, the type-scope threading, and the cross-file
/// [`super::resolve::resolve`]/[`super::resolve::pick`] resolver — so a call site's qualifier is always
/// tree-derived (tags omit it).
#[derive(Clone, Copy)]
pub(super) enum Classifier<'a> {
    Rust,
    Tagged(&'a tags::TaggedSymbols),
}

impl Classifier<'_> {
    /// Classify a node as a graph definition: `(kind, name)` for functions/methods/classes/containers,
    /// or `None` otherwise. `kind` drives callable-ness (`function`/`method`) and the type scope.
    fn classify(self, node: &Node<'_>, bytes: &[u8]) -> Option<(&'static str, Option<String>)> {
        match self {
            Classifier::Rust => interesting_node(node, bytes),
            Classifier::Tagged(tags) => tags
                .defs
                .get(&node.id())
                .map(|def| (def.kind, def.name.clone())),
        }
    }

    /// Whether a definition is a **call target** — something a call site may resolve to.
    ///
    /// This is deliberately narrower than "is callable-shaped". A callable-shaped def gets the `()`
    /// label suffix and a `method` edge; a *call target* additionally competes for a name in
    /// [`super::resolve::resolve`]. A trait method DECLARATION is the first but not the second: a
    /// call dispatches to an implementation, never to the declaration.
    ///
    /// Keeping declarations out of the candidate set is load-bearing, not tidiness. The resolver is
    /// precision-favouring — several same-named candidates with no disambiguating qualifier are
    /// dropped, not fanned out. A trait with exactly one impl would otherwise go from one candidate
    /// to two the moment declarations were indexed, and every call to that method would stop
    /// resolving. Fixing the missing-symbol bug would then have silently deleted `calls` edges.
    fn is_call_target(self, node: &Node<'_>, kind: &str) -> bool {
        if !matches!(kind, "function" | "method") {
            return false;
        }
        match self {
            // The one Rust node kind that is callable-shaped but bodiless.
            Classifier::Rust => node.kind() != "function_signature_item",
            // A captured definition of one of these node KINDS with no `body` child is a
            // declaration, not an implementation — a call dispatches to an implementation, never to
            // the declaration itself (issue #5, the tags-path twin of the Rust case above). Verified
            // directly against each grammar's `node-types.json`:
            //   - Java `method_declaration`: `body` is an OPTIONAL field, present only when the
            //     method has one — an interface/`abstract` method has none.
            //   - TypeScript `method_signature` / `abstract_method_signature` (interface and
            //     abstract-class members) / `function_signature` (ambient/overload declarations):
            //     these node kinds have no `body` field at all — they are inherently bodiless.
            //
            // The check is scoped to these specific node kinds rather than applied to every `Tagged`
            // definition unconditionally, because `child_by_field_name("body")` can't distinguish "no
            // body field on this node kind at all" from "the field exists and is absent" — and several
            // JS `tags.scm` patterns anchor `@definition.function` on a WRAPPER node with no `body`
            // field concept of its own (`variable_declarator` for `const f = () => {}`,
            // `assignment_expression` for `x.f = function(){}`, `pair` for `{ f() {} }`-style object
            // methods) even though the function VALUE they wrap always has one. Treating those as
            // bodiless would have wrongly dropped every const-arrow-function call target — caught by
            // the `javascript-repo` golden gaining a spurious drop before this list was narrowed.
            // Every other tagged definition shape (JS/TS bodied methods/functions via any capture
            // pattern, and Python's always-bodied `function_definition` — deliberately unaffected,
            // since even a `pass`/`...`-bodied abstract method has a body child) has no
            // bodiless-declaration concept: leave it as a target, matching the pre-fix behaviour.
            Classifier::Tagged(_) => match node.kind() {
                "method_declaration"
                | "method_signature"
                | "abstract_method_signature"
                | "function_signature" => node.child_by_field_name("body").is_some(),
                _ => true,
            },
        }
    }

    /// The type name a container def introduces for its children — used only to disambiguate several
    /// same-named callables (e.g. two classes each with `run`). `None` for non-containers.
    fn container_scope(
        self,
        node: &Node<'_>,
        kind: &str,
        name: Option<&str>,
        bytes: &[u8],
    ) -> Option<String> {
        match self {
            Classifier::Rust => match kind {
                // `impl S` / `impl T for S` — the implementing type is the scope, not the trait.
                "impl" => callee::impl_type_name(node, bytes),
                "trait" | "struct" | "enum" | "class" => name.map(str::to_string),
                _ => None,
            },
            // Tagged languages: a class/interface/enum scopes the methods it contains by its own name.
            Classifier::Tagged(_) => match kind {
                "class" | "interface" | "enum" => name.map(str::to_string),
                _ => None,
            },
        }
    }

    /// If `node` is a call site, return its callee reference (bare name + an optional type qualifier
    /// used solely as an ambiguity tiebreaker). For Rust this is the `call_expression` navigation; for
    /// tagged languages the node is the callee **name node** the query captured, and the qualifier is
    /// recovered from that node's receiver in the tree (tags drop it).
    fn call_site(self, node: &Node<'_>, bytes: &[u8]) -> Option<CalleeRef> {
        match self {
            Classifier::Rust => (node.kind() == "call_expression")
                .then(|| callee::callee_ref_of(node, bytes))
                .flatten(),
            Classifier::Tagged(tags) => tags.calls.get(&node.id()).map(|name| CalleeRef {
                name: name.clone(),
                qualifier: callee::qualifier_from_callee_node(node, bytes),
            }),
        }
    }
}

fn file_label(source_file: &str) -> String {
    source_file
        .rsplit('/')
        .next()
        .unwrap_or(source_file)
        .to_string()
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`extract_file`]'s DFS directly (not through [`super::resolve::resolve`]), plus
    //! the small helpers it uses. End-to-end call-resolution behaviour lives in `graph::tests`; these
    //! isolate the emit pass's own contract: what nodes/edges one file's facts contain.
    use super::*;
    use crate::lang;

    fn facts_for(language: &str, path: &str, src: &str) -> FileSymbols {
        let tree = lang::parse(src, language).expect("source parses");
        extract_file(&tree, path, src, language)
    }

    #[test]
    fn unsupported_language_yields_completely_empty_facts() {
        // A tree exists (any grammar can produce one), but the *language id* has no graph strategy —
        // must short-circuit before even the file node is emitted.
        let tree = lang::parse("fn a() {}\n", "rust").expect("parses");
        let facts = extract_file(&tree, "f.go", "fn a() {}\n", "go");
        assert!(
            facts.nodes.is_empty(),
            "no file node either: {:?}",
            facts.nodes
        );
        assert!(facts.contains.is_empty());
        assert!(facts.calls.is_empty());
        assert!(facts.callables.is_empty());
    }

    #[test]
    fn file_node_is_emitted_with_path_id_and_basename_label() {
        let facts = facts_for("rust", "src/lib/math.rs", "fn add() {}\n");
        let file = facts
            .nodes
            .iter()
            .find(|n| n.node_id == "src/lib/math.rs")
            .expect("file node present");
        assert_eq!(file.label, "math.rs");
        assert_eq!(
            file.start_line, 1,
            "file node is line 1, like Graphify's L1"
        );
    }

    #[test]
    fn nested_module_chain_produces_a_contains_chain() {
        // file → mod a → mod b → fn f, all `contains` (no type container in the chain).
        let src = "mod a {\n    mod b {\n        fn f() {}\n    }\n}\n";
        let facts = facts_for("rust", "src/n.rs", src);
        let mod_a = facts.nodes.iter().find(|n| n.label == "a").unwrap();
        let mod_b = facts.nodes.iter().find(|n| n.label == "b").unwrap();
        let f = facts.nodes.iter().find(|n| n.label == "f()").unwrap();
        assert!(
            facts.contains.iter().any(|e| e.relation == "contains"
                && e.source == "src/n.rs"
                && e.target == mod_a.node_id),
            "file contains mod a"
        );
        assert!(
            facts.contains.iter().any(|e| e.relation == "contains"
                && e.source == mod_a.node_id
                && e.target == mod_b.node_id),
            "mod a contains mod b"
        );
        assert!(
            facts.contains.iter().any(|e| e.relation == "contains"
                && e.source == mod_b.node_id
                && e.target == f.node_id),
            "mod b contains f"
        );
    }

    #[test]
    fn trait_container_emits_a_method_edge_for_its_fn() {
        // A default-bodied trait method (`function_item`).
        let src = "trait T {\n    fn f(&self) {}\n}\n";
        let facts = facts_for("rust", "src/t.rs", src);
        let trait_node = facts.nodes.iter().find(|n| n.label == "T").unwrap();
        let f = facts.nodes.iter().find(|n| n.label == "f()").unwrap();
        assert!(
            facts.contains.iter().any(|e| e.relation == "method"
                && e.source == trait_node.node_id
                && e.target == f.node_id),
            "trait → f must be a `method` edge; got {:?}",
            facts.contains
        );
    }

    #[test]
    fn signature_only_trait_method_is_a_node_with_a_method_edge() {
        // REGRESSION: a trait method with no default body parses as `function_signature_item`, not
        // `function_item`, and used to be classified as nothing at all — so a trait interface
        // contributed zero callable symbols to the graph or to semantic search.
        let src = "trait T {\n    fn f(&self);\n}\n";
        let facts = facts_for("rust", "src/t.rs", src);
        let trait_node = facts.nodes.iter().find(|n| n.label == "T").unwrap();
        let f = facts
            .nodes
            .iter()
            .find(|n| n.label == "f()")
            .expect("signature-only trait method must produce a node");
        assert_eq!(f.start_line, 2);
        assert!(
            facts.contains.iter().any(|e| e.relation == "method"
                && e.source == trait_node.node_id
                && e.target == f.node_id),
            "trait → f must be a `method` edge; got {:?}",
            facts.contains
        );
    }

    #[test]
    fn signature_only_trait_method_is_not_a_call_target() {
        // It is callable-SHAPED (it earned the `()` label and the `method` edge above) but must not
        // enter the resolver's candidate set: a call dispatches to an implementation, never to the
        // declaration. See `Classifier::is_call_target`.
        let src = "trait T {\n    fn f(&self);\n}\n";
        let facts = facts_for("rust", "src/t.rs", src);
        assert!(
            facts.callables.is_empty(),
            "a bodiless declaration must not be a call target; got {:?}",
            facts.callables
        );
    }

    #[test]
    fn a_bodied_method_next_to_its_declaration_is_still_the_only_call_target() {
        // The whole reason declarations are excluded from the candidate set. A trait with exactly
        // ONE impl would otherwise go from one candidate to two the moment declarations were
        // indexed, and the precision-favouring resolver would drop every call to that method —
        // so fixing the missing-symbol bug would have silently deleted `calls` edges.
        let src = "trait T {\n    fn f(&self);\n}\n\
                   struct S;\n\
                   impl T for S {\n    fn f(&self) {}\n}\n";
        let facts = facts_for("rust", "src/t.rs", src);
        let targets: Vec<_> = facts.callables.iter().map(|c| c.node_id.as_str()).collect();
        assert_eq!(
            targets.len(),
            1,
            "exactly one call target (the impl) must survive; got {targets:?}"
        );
        assert!(
            targets[0].ends_with(":f"),
            "the surviving target must be the bodied impl method; got {targets:?}"
        );
        // Both defs still exist as nodes — the declaration is indexed, just not dispatched to.
        assert_eq!(
            facts.nodes.iter().filter(|n| n.label == "f()").count(),
            2,
            "declaration and implementation must BOTH be nodes; got {:?}",
            facts.nodes
        );
    }

    #[test]
    fn tagged_class_container_emits_a_method_edge_for_its_method() {
        let src = "class C:\n    def m(self):\n        pass\n";
        let facts = facts_for("python", "c.py", src);
        let class = facts.nodes.iter().find(|n| n.label == "C").unwrap();
        let m = facts.nodes.iter().find(|n| n.label == "m()").unwrap();
        assert!(
            facts.contains.iter().any(|e| e.relation == "method"
                && e.source == class.node_id
                && e.target == m.node_id),
            "class → m must be a `method` edge; got {:?}",
            facts.contains
        );
    }

    #[test]
    fn tagged_bodiless_interface_method_is_a_node_with_a_method_edge() {
        // REGRESSION (issue #5, the tags-path twin of the Rust
        // `signature_only_trait_method_is_a_node_with_a_method_edge` case above): a Java interface
        // method (`method_declaration` with no `body` field) used to be treated as a call target
        // regardless — it must still be a definition (a node + `method` edge), just not a call
        // target (see the next test).
        let src = "interface Greeter {\n    String greet();\n}\n";
        let facts = facts_for("java", "Greeter.java", src);
        let iface = facts.nodes.iter().find(|n| n.label == "Greeter").unwrap();
        let greet = facts
            .nodes
            .iter()
            .find(|n| n.label == "greet()")
            .expect("bodiless interface method must produce a node");
        assert_eq!(greet.start_line, 2);
        assert!(
            facts.contains.iter().any(|e| e.relation == "method"
                && e.source == iface.node_id
                && e.target == greet.node_id),
            "Greeter → greet must be a `method` edge; got {:?}",
            facts.contains
        );
    }

    #[test]
    fn tagged_bodiless_interface_method_is_not_a_call_target() {
        // It is callable-SHAPED (the node + `method` edge above) but must not enter the resolver's
        // candidate set: a call dispatches to an implementation, never to the declaration. See
        // `Classifier::is_call_target`.
        let src = "interface Greeter {\n    String greet();\n}\n";
        let facts = facts_for("java", "Greeter.java", src);
        assert!(
            facts.callables.is_empty(),
            "a bodiless interface method must not be a call target; got {:?}",
            facts.callables
        );
    }

    #[test]
    fn tagged_bodied_method_next_to_its_interface_declaration_is_still_the_only_call_target() {
        // The tags-path twin of the Rust
        // `a_bodied_method_next_to_its_declaration_is_still_the_only_call_target` case above: a
        // single implementation must remain the ONLY call target even though its declaration is
        // also indexed as a node — otherwise a single-impl interface would go from one candidate to
        // two the moment declarations were indexed, and the precision-favouring resolver would drop
        // every call to it (issue #5).
        let src = "\
interface Greeter {
    String greet();
}

class EnglishGreeter implements Greeter {
    public String greet() { return \"hello\"; }
}
";
        let facts = facts_for("java", "Greeter.java", src);
        assert_eq!(
            facts.callables.len(),
            1,
            "exactly one call target (the impl) must survive; got {:?}",
            facts.callables
        );
        let surviving_line = facts
            .nodes
            .iter()
            .find(|n| n.node_id == facts.callables[0].node_id)
            .expect("callable node id must match an emitted node")
            .start_line;
        let impl_method_line = facts
            .nodes
            .iter()
            .filter(|n| n.label == "greet()")
            .map(|n| n.start_line)
            .max()
            .expect("both declaration and impl must be nodes");
        assert_eq!(
            surviving_line, impl_method_line,
            "the surviving call target must be the later-declared impl, not the interface \
             declaration; got {:?}",
            facts.callables
        );
        // Both defs still exist as nodes — the declaration is indexed, just not dispatched to.
        assert_eq!(
            facts.nodes.iter().filter(|n| n.label == "greet()").count(),
            2,
            "declaration and implementation must BOTH be nodes; got {:?}",
            facts.nodes
        );
    }

    #[test]
    fn tagged_bodied_method_is_still_a_call_target() {
        // Sanity check paired with the bodiless tests above: a normal, bodied Java method (`{ ... }`)
        // must remain a call target exactly as before this fix — the `body`-field check must not
        // exclude ordinary definitions.
        let src = "class C {\n    void m() {}\n}\n";
        let facts = facts_for("java", "C.java", src);
        assert_eq!(
            facts.callables.len(),
            1,
            "a bodied method must still be a call target; got {:?}",
            facts.callables
        );
        assert_eq!(facts.callables[0].name, "m");
    }

    #[test]
    fn def_line_numbers_are_one_based() {
        let facts = facts_for("rust", "src/m.rs", "\n\nfn add() {}\n"); // add on source line 3
        let add = facts.nodes.iter().find(|n| n.label == "add()").unwrap();
        assert_eq!(add.start_line, 3);
    }

    #[test]
    fn callable_labels_carry_a_parens_suffix_but_non_callables_dont() {
        let facts = facts_for("rust", "src/m.rs", "struct S;\nfn f() {}\n");
        assert!(facts.nodes.iter().any(|n| n.label == "f()"));
        assert!(
            facts.nodes.iter().any(|n| n.label == "S"),
            "non-callable def has no parens; nodes = {:?}",
            facts.nodes
        );
    }

    #[test]
    fn file_label_strips_leading_directories() {
        assert_eq!(file_label("src/lib/math.rs"), "math.rs");
    }

    #[test]
    fn file_label_of_a_bare_filename_is_itself() {
        assert_eq!(file_label("math.rs"), "math.rs");
    }

    #[test]
    fn is_type_container_true_only_for_type_kinds() {
        for kind in ["impl", "trait", "struct", "enum", "class", "interface"] {
            assert!(is_type_container(Some(kind)), "{kind} is a type container");
        }
        assert!(!is_type_container(Some("function")));
        assert!(!is_type_container(Some("module")));
        assert!(!is_type_container(None));
    }
}
