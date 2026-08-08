//! End-to-end tests over the `graph` module's public pipeline (`extract_file` + `resolve`) — the
//! contract the rest of the crate (and `agent-runner`) depends on, not the internals of any one
//! submodule. Kept alongside [`super`] rather than split per submodule because every test here drives
//! the whole per-file-extraction → cross-file-resolution flow, mirroring real usage.

use super::*;
use crate::lang;

fn graph_of_lang(language: &str, files: &[(&str, &str)]) -> Graph {
    let facts: Vec<FileSymbols> = files
        .iter()
        .map(|(path, src)| {
            let tree = lang::parse(src, language).expect("source parses");
            extract_file(&tree, path, src, language)
        })
        .collect();
    // No framework facts here: this module drives `extract_file`/`resolve` directly to test the
    // language-level pipeline in isolation. `lci-codegraph-spring` is wired in at the `Indexer`
    // level (`src/input.rs`), not here — see `tests/language_goldens.rs` for the end-to-end path.
    resolve(facts, Vec::new())
}

fn graph_of(files: &[(&str, &str)]) -> Graph {
    graph_of_lang("rust", files)
}

/// Find the single node with `label`, panicking with the graph on failure (test ergonomics).
fn node<'a>(g: &'a Graph, label: &str) -> &'a GraphNode {
    g.nodes
        .iter()
        .find(|n| n.label == label)
        .unwrap_or_else(|| panic!("no node labelled {label:?}; nodes = {:?}", g.nodes))
}

/// True iff a `calls` edge `caller → callee` (by label) exists.
fn has_call(g: &Graph, caller: &str, callee: &str) -> bool {
    let src = node(g, caller).node_id.as_str();
    let dst = node(g, callee).node_id.as_str();
    g.edges
        .iter()
        .any(|e| e.relation == "calls" && e.source == src && e.target == dst)
}

#[test]
fn emits_file_and_function_nodes_with_contains() {
    let g = graph_of(&[("src/math.rs", "fn add(a: i32, b: i32) -> i32 { a + b }\n")]);
    assert!(
        g.nodes
            .iter()
            .any(|n| n.node_id == "src/math.rs" && n.label == "math.rs")
    );
    let add = g
        .nodes
        .iter()
        .find(|n| n.label == "add()")
        .expect("add node");
    assert_eq!(add.source_file, "src/math.rs");
    assert!(
        g.edges.iter().any(|e| e.relation == "contains"
            && e.source == "src/math.rs"
            && e.target == add.node_id),
        "file should contain add"
    );
}

#[test]
fn intra_file_call_edge_is_built() {
    let src = "fn helper() {}\nfn caller() { helper(); }\n";
    let g = graph_of(&[("src/a.rs", src)]);
    let helper = g.nodes.iter().find(|n| n.label == "helper()").unwrap();
    let caller = g.nodes.iter().find(|n| n.label == "caller()").unwrap();
    assert!(
        g.edges.iter().any(|e| e.relation == "calls"
            && e.source == caller.node_id
            && e.target == helper.node_id),
        "caller → helper calls edge, got {:?}",
        g.edges
    );
}

#[test]
fn cross_file_call_resolves_to_definition_in_another_file() {
    // The high-value case (ADR-0086): a reference in file A → a definition in file B.
    let a = ("src/a.rs", "fn caller() { target(); }\n");
    let b = ("src/b.rs", "fn target() {}\n");
    let g = graph_of(&[a, b]);
    let target = g.nodes.iter().find(|n| n.label == "target()").unwrap();
    let caller = g.nodes.iter().find(|n| n.label == "caller()").unwrap();
    assert_eq!(target.source_file, "src/b.rs");
    assert_eq!(caller.source_file, "src/a.rs");
    assert!(
        g.edges.iter().any(|e| e.relation == "calls"
            && e.source == caller.node_id
            && e.target == target.node_id),
        "cross-file caller → target edge missing; edges = {:?}",
        g.edges
    );
}

#[test]
fn method_call_resolves_to_method_definition() {
    let src = "struct S;\nimpl S { fn run(&self) {} }\nfn go(s: &S) { s.run(); }\n";
    let g = graph_of(&[("src/s.rs", src)]);
    let run = g.nodes.iter().find(|n| n.label == "run()").unwrap();
    let go = g.nodes.iter().find(|n| n.label == "go()").unwrap();
    assert!(
        g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == go.node_id && e.target == run.node_id),
        "go → S::run method-call edge missing; edges = {:?}",
        g.edges
    );
}

#[test]
fn ambiguous_global_name_is_not_guessed() {
    // Two files each define `dup`; a third calls `dup` with no local def → ambiguous → no edge.
    let files = &[
        ("src/x.rs", "fn dup() {}\n"),
        ("src/y.rs", "fn dup() {}\n"),
        ("src/z.rs", "fn caller() { dup(); }\n"),
    ];
    let g = graph_of(files);
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "ambiguous name must not produce a guessed calls edge, got {:?}",
        g.edges
    );
}

#[test]
fn method_definition_emits_a_method_edge_not_contains() {
    // Parity with Graphify: a callable inside a type container is joined by `method`, not
    // `contains`.
    let src = "struct S;\nimpl S { fn run(&self) {} }\n";
    let g = graph_of(&[("src/s.rs", src)]);
    let run = g.nodes.iter().find(|n| n.label == "run()").unwrap();
    // The enclosing container is the `impl S` node (codegraph emits an anonymous `impl` node, not
    // the named type — this asserts codegraph's own SELF-CONSISTENCY: the `method` edge's source
    // is exactly that container node. Whether Graphify anchors `method` at the type name instead
    // is a separate side-by-side parity check still open on the #360 gate.
    let impl_node = g
        .nodes
        .iter()
        .find(|n| n.label == "impl")
        .expect("impl container node");
    let method_edge = g
        .edges
        .iter()
        .find(|e| e.relation == "method" && e.target == run.node_id)
        .unwrap_or_else(|| panic!("S::run must be a `method` edge; edges = {:?}", g.edges));
    assert_eq!(
        method_edge.source, impl_node.node_id,
        "method edge must originate from the `impl S` container node, not elsewhere"
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "contains" && e.target == run.node_id),
        "S::run must NOT also be a `contains` target; edges = {:?}",
        g.edges
    );
}

#[test]
fn ambiguous_local_does_not_shadow_a_qualified_global() {
    // file a: two colliding `new`s + a call to `C::new()` (a *different* type). The local set is
    // ambiguous, but the qualifier singles out the global `C::new` in file c — the edge must
    // resolve cross-file, not be dropped (P2-c).
    let a = (
        "src/a.rs",
        "\
struct A;
struct B;
impl A { fn new() -> A { A } }
impl B { fn new() -> B { B } }
fn make() { let _ = C::new(); }
",
    );
    let c = ("src/c.rs", "struct C;\nimpl C { fn new() -> C { C } }\n");
    let g = graph_of(&[a, c]);
    let make = g.nodes.iter().find(|n| n.label == "make()").unwrap();
    let c_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.source_file == "src/c.rs")
        .expect("C::new in c.rs");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == make.node_id)
        .collect();
    assert_eq!(calls.len(), 1, "one cross-file edge; got {calls:?}");
    assert_eq!(
        calls[0].target, c_new.node_id,
        "ambiguous locals must not shadow the qualified global C::new"
    );
}

#[test]
fn labels_and_lines_match_graphify_shape() {
    // 1-based lines + `()` on callables (Graphify parity).
    let g = graph_of(&[("src/m.rs", "\nfn add() {}\n")]); // `add` on source line 2
    let add = g.nodes.iter().find(|n| n.label == "add()").unwrap();
    assert_eq!(add.start_line, 2, "1-based start_line");
    let file = g.nodes.iter().find(|n| n.node_id == "src/m.rs").unwrap();
    assert_eq!(file.start_line, 1, "file node is line 1 like Graphify's L1");
}

#[test]
fn same_file_qualified_call_hits_the_right_constructor() {
    // Two impls in one file both define `new`. `A::new()` must resolve to A's `new` only — the
    // old same-file branch fanned out to BOTH (the bug this fixes).
    let src = "\
struct A;
struct B;
impl A { fn new() -> A { A } }
impl B { fn new() -> B { B } }
fn make() { let _ = A::new(); }
";
    let g = graph_of(&[("src/lib.rs", src)]);
    let make = g.nodes.iter().find(|n| n.label == "make()").unwrap();
    // `impl A { fn new ... }` are one source line: A::new is line 3, B::new line 4.
    let a_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.start_line == 3)
        .expect("A::new at line 3");
    let b_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.start_line == 4)
        .expect("B::new at line 4");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == make.node_id)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "exactly one calls edge (no fan-out); got {calls:?}"
    );
    assert_eq!(calls[0].target, a_new.node_id, "must resolve to A::new");
    assert_ne!(calls[0].target, b_new.node_id, "must NOT also hit B::new");
}

#[test]
fn same_file_bare_duplicate_call_is_dropped_not_fanned_out() {
    // `default` on two impls; a *bare* `default()` can't be attributed → dropped, not one edge
    // to each definition.
    let src = "\
struct A;
struct B;
impl A { fn default() -> A { A } }
impl B { fn default() -> B { B } }
fn make() { let _ = default(); }
";
    let g = graph_of(&[("src/lib.rs", src)]);
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "ambiguous bare call must be dropped, not fanned out; edges = {:?}",
        g.edges
    );
}

#[test]
fn qualified_call_disambiguates_from_and_build_across_impls() {
    // A mix of `from`/`build` constructors on colliding types; each qualified call lands on its
    // own type's method and nothing else.
    let src = "\
struct Meters;
struct Feet;
impl Meters { fn from(v: i32) -> Meters { Meters } fn build() -> Meters { Meters } }
impl Feet { fn from(v: i32) -> Feet { Feet } fn build() -> Feet { Feet } }
fn go() { let _ = Feet::from(3); let _ = Meters::build(); }
";
    let g = graph_of(&[("src/u.rs", src)]);
    let go = g.nodes.iter().find(|n| n.label == "go()").unwrap();
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == go.node_id)
        .collect();
    assert_eq!(calls.len(), 2, "two resolved edges; got {calls:?}");
    // `Feet::from` and `Meters::build` are the only correct targets.
    // `impl Meters {...}` is line 3, `impl Feet {...}` line 4 (each impl + its methods one line).
    let feet_from = g
        .nodes
        .iter()
        .find(|n| n.label == "from()" && n.start_line == 4)
        .expect("Feet::from at line 4");
    let meters_build = g
        .nodes
        .iter()
        .find(|n| n.label == "build()" && n.start_line == 3)
        .expect("Meters::build at line 3");
    assert!(calls.iter().any(|e| e.target == feet_from.node_id));
    assert!(calls.iter().any(|e| e.target == meters_build.node_id));
}

#[test]
fn output_is_deterministic() {
    let files = &[
        ("src/b.rs", "fn target() {}\n"),
        ("src/a.rs", "fn caller() { target(); }\n"),
    ];
    let g1 = graph_of(files);
    let g2 = graph_of(files);
    assert_eq!(g1, g2, "resolve must be order-independent + canonical");
}

// ── Python ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn python_free_function_contains_and_intra_file_call() {
    let src = "def helper():\n    pass\n\ndef caller():\n    helper()\n";
    let g = graph_of_lang("python", &[("app/util.py", src)]);
    let file = node(&g, "util.py");
    assert_eq!(file.node_id, "app/util.py");
    let helper = node(&g, "helper()");
    assert_eq!(helper.start_line, 1, "1-based def line");
    assert!(
        g.edges.iter().any(|e| e.relation == "contains"
            && e.source == "app/util.py"
            && e.target == helper.node_id),
        "file → helper contains edge; edges = {:?}",
        g.edges
    );
    assert!(has_call(&g, "caller()", "helper()"), "caller → helper");
}

#[test]
fn python_class_method_emits_method_edge_and_self_call_resolves() {
    // A class with two methods; `describe` calls `self.area()`. `area` is a `method` edge off the
    // class node, and the self-call resolves to the single `area` despite the `self` receiver.
    let src = "\
class Circle:
    def area(self):
        return 3.14

    def describe(self):
        return self.area()
";
    let g = graph_of_lang("python", &[("shapes.py", src)]);
    let class = node(&g, "Circle");
    let area = node(&g, "area()");
    assert!(
        g.edges.iter().any(|e| e.relation == "method"
            && e.source == class.node_id
            && e.target == area.node_id),
        "Circle → area must be a `method` edge; edges = {:?}",
        g.edges
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "contains" && e.target == area.node_id),
        "a method must not also be a `contains` target"
    );
    assert!(
        has_call(&g, "describe()", "area()"),
        "self.area() must resolve to Circle.area; edges = {:?}",
        g.edges
    );
}

#[test]
fn python_cross_file_call_resolves() {
    let a = ("pkg/a.py", "def caller():\n    target()\n");
    let b = ("pkg/b.py", "def target():\n    pass\n");
    let g = graph_of_lang("python", &[a, b]);
    assert_eq!(node(&g, "target()").source_file, "pkg/b.py");
    assert!(
        has_call(&g, "caller()", "target()"),
        "cross-file caller → target"
    );
}

#[test]
fn python_colliding_method_names_qualified_resolves_bare_dropped() {
    // Two classes each define `new`; `Circle.new()` resolves to Circle's only, a bare `new()`
    // is ambiguous and dropped (never fanned out to both).
    let src = "\
class Circle:
    def new(self):
        return self

class Square:
    def new(self):
        return self

def build():
    return Circle.new()

def build_bare():
    return new()
";
    let g = graph_of_lang("python", &[("shapes.py", src)]);
    let circle_new = g
        .nodes
        .iter()
        .find(|n| n.label == "new()" && n.start_line == 2)
        .expect("Circle.new at line 2");
    let build_calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
        .collect();
    assert_eq!(
        build_calls.len(),
        1,
        "one qualified edge; got {build_calls:?}"
    );
    assert_eq!(
        build_calls[0].target, circle_new.node_id,
        "→ Circle.new only"
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == node(&g, "build_bare()").node_id),
        "bare ambiguous new() must be dropped; edges = {:?}",
        g.edges
    );
}

#[test]
fn python_decorated_def_is_named_not_wrapped() {
    // `@decorator` must not spawn an anonymous wrapper node — the def keeps its real name/line.
    let src = "import functools\n\n@functools.cache\ndef cached():\n    return 1\n";
    let g = graph_of_lang("python", &[("m.py", src)]);
    let cached = node(&g, "cached()");
    assert_eq!(cached.start_line, 4, "def line, not the decorator line");
    assert!(
        g.edges
            .iter()
            .any(|e| e.relation == "contains" && e.source == "m.py" && e.target == cached.node_id),
        "decorated def is contained by the file directly (no wrapper); edges = {:?}",
        g.edges
    );
    assert!(
        !g.nodes.iter().any(|n| n.label == "function"),
        "no anonymous `function` wrapper node; nodes = {:?}",
        g.nodes
    );
}

// ── TypeScript / JavaScript ─────────────────────────────────────────────────────────────────

#[test]
fn ts_function_declaration_and_call() {
    let src = "function helper() {}\nfunction caller() { helper(); }\n";
    let g = graph_of_lang("typescript", &[("src/a.ts", src)]);
    assert!(has_call(&g, "caller()", "helper()"), "caller → helper");
}

#[test]
fn ts_named_arrow_const_is_a_callable() {
    // The dominant modern form: `const helper = () => {}`. It must be a named, callable def.
    let src = "const helper = () => {};\nfunction caller() { helper(); }\n";
    let g = graph_of_lang("typescript", &[("src/a.ts", src)]);
    let helper = node(&g, "helper()");
    assert!(
        g.edges.iter().any(|e| e.relation == "contains"
            && e.source == "src/a.ts"
            && e.target == helper.node_id),
        "file → const-arrow helper; edges = {:?}",
        g.edges
    );
    assert!(
        has_call(&g, "caller()", "helper()"),
        "caller → const-arrow helper"
    );
}

#[test]
fn ts_class_method_edge_and_this_call() {
    let src = "\
class Service {
  run() {}
  go() { this.run(); }
}
";
    let g = graph_of_lang("typescript", &[("src/s.ts", src)]);
    let class = node(&g, "Service");
    let run = node(&g, "run()");
    assert!(
        g.edges.iter().any(|e| e.relation == "method"
            && e.source == class.node_id
            && e.target == run.node_id),
        "Service → run `method` edge; edges = {:?}",
        g.edges
    );
    assert!(
        has_call(&g, "go()", "run()"),
        "this.run() resolves to Service.run"
    );
}

#[test]
fn ts_cross_file_call_resolves() {
    let a = (
        "src/a.ts",
        "import { target } from './b';\nfunction caller() { target(); }\n",
    );
    let b = ("src/b.ts", "export function target() {}\n");
    let g = graph_of_lang("typescript", &[a, b]);
    assert_eq!(node(&g, "target()").source_file, "src/b.ts");
    assert!(
        has_call(&g, "caller()", "target()"),
        "cross-file caller → target"
    );
}

#[test]
fn ts_colliding_methods_qualified_resolves_bare_dropped() {
    let src = "\
class Circle { make() { return this; } }
class Square { make() { return this; } }
function build() { return Circle.make(); }
function buildBare(m) { return make(); }
";
    let g = graph_of_lang("typescript", &[("src/shapes.ts", src)]);
    let circle_make = g
        .nodes
        .iter()
        .find(|n| n.label == "make()" && n.start_line == 1)
        .expect("Circle.make at line 1");
    let build_calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
        .collect();
    assert_eq!(
        build_calls.len(),
        1,
        "one qualified edge; got {build_calls:?}"
    );
    assert_eq!(
        build_calls[0].target, circle_make.node_id,
        "→ Circle.make only"
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == node(&g, "buildBare()").node_id),
        "bare ambiguous make() must be dropped; edges = {:?}",
        g.edges
    );
}

#[test]
fn tsx_component_uses_the_jsx_grammar() {
    // A `.tsx` component returning JSX must parse via the TSX grammar (the plain TS grammar chokes
    // on JSX). Assert the component + a hook call resolve, proving the JSX body didn't wreck the
    // parse.
    let src = "\
function useThing() { return 1; }
export const Widget = () => {
  const n = useThing();
  return <div>{n}</div>;
};
";
    let g = graph_of_lang("tsx", &[("src/Widget.tsx", src)]);
    assert!(
        has_call(&g, "Widget()", "useThing()"),
        "Widget → useThing across a JSX body; edges = {:?}",
        g.edges
    );
}

#[test]
fn javascript_jsx_arrow_component_is_extracted() {
    // JSX in a `.jsx`/`.js` file must still parse + yield the named component and its call.
    let src = "\
const Button = () => { return null; };
function App() { return Button(); }
";
    let g = graph_of_lang("javascript", &[("src/app.jsx", src)]);
    assert!(
        has_call(&g, "App()", "Button()"),
        "App → Button; edges = {:?}",
        g.edges
    );
}

// ── Java (stretch) ──────────────────────────────────────────────────────────────────────────

#[test]
fn java_class_method_edge_and_this_call() {
    let src = "\
class Greeter {
    String hello() { return \"hi\"; }
    String greet() { return this.hello(); }
}
";
    let g = graph_of_lang("java", &[("Greeter.java", src)]);
    let class = node(&g, "Greeter");
    let hello = node(&g, "hello()");
    assert!(
        g.edges.iter().any(|e| e.relation == "method"
            && e.source == class.node_id
            && e.target == hello.node_id),
        "Greeter → hello `method` edge; edges = {:?}",
        g.edges
    );
    assert!(
        has_call(&g, "greet()", "hello()"),
        "this.hello() → Greeter.hello"
    );
}

#[test]
fn java_cross_file_qualified_static_call_resolves() {
    let a = (
        "Caller.java",
        "class Caller { void run() { Helper.help(); } }\n",
    );
    let b = ("Helper.java", "class Helper { static void help() {} }\n");
    let g = graph_of_lang("java", &[a, b]);
    assert_eq!(node(&g, "help()").source_file, "Helper.java");
    assert!(
        has_call(&g, "run()", "help()"),
        "Helper.help() resolves cross-file"
    );
}

#[test]
fn java_colliding_methods_qualified_resolves() {
    let src = "\
class Circle { Circle make() { return this; } }
class Square { Square make() { return this; } }
class Builder { Object build() { return Circle.make(); } }
";
    let g = graph_of_lang("java", &[("Shapes.java", src)]);
    let circle_make = g
        .nodes
        .iter()
        .find(|n| n.label == "make()" && n.start_line == 1)
        .expect("Circle.make at line 1");
    let build_calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == node(&g, "build()").node_id)
        .collect();
    assert_eq!(
        build_calls.len(),
        1,
        "one qualified edge; got {build_calls:?}"
    );
    assert_eq!(
        build_calls[0].target, circle_make.node_id,
        "→ Circle.make only"
    );
}

#[test]
fn a_call_to_a_single_impl_trait_method_still_resolves_to_the_impl() {
    // The end-to-end guard for the decision in `Classifier::is_call_target`. Indexing trait method
    // DECLARATIONS as nodes (so trait interfaces are searchable) must not make them compete with
    // their own implementations for the name: the resolver is precision-favouring and drops a bare
    // name matching several candidates. If declarations were call targets, this call would go from
    // one candidate to two and the `calls` edge below would silently disappear.
    let g = graph_of(&[
        (
            "src/shape.rs",
            "trait Shape {\n    fn describe(&self) -> f64;\n}\n",
        ),
        (
            "src/circle.rs",
            "struct Circle;\nimpl Shape for Circle {\n    fn describe(&self) -> f64 {\n        1.0\n    }\n}\n",
        ),
        (
            "src/main.rs",
            "fn run(c: &Circle) -> f64 {\n    c.describe()\n}\n",
        ),
    ]);

    // Both the declaration and the implementation are indexed as nodes.
    let describes: Vec<&GraphNode> = g.nodes.iter().filter(|n| n.label == "describe()").collect();
    assert_eq!(
        describes.len(),
        2,
        "declaration and implementation must both be nodes; got {describes:?}"
    );
    assert!(describes.iter().any(|n| n.source_file == "src/shape.rs"));

    // ...but the call resolves, and resolves to the IMPLEMENTATION, not the declaration.
    let caller = node(&g, "run()").node_id.clone();
    let targets: Vec<&str> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == caller)
        .map(|e| e.target.as_str())
        .collect();
    assert_eq!(
        targets.len(),
        1,
        "the call must still resolve to exactly one target; got {targets:?}"
    );
    assert!(
        targets[0].starts_with("src/circle.rs"),
        "must resolve to the impl in circle.rs, not the declaration; got {targets:?}"
    );
}

// ── Tags path: declaration vs. call target (issue #5, the tags-path twin of the Rust case above) ──

#[test]
fn java_call_to_a_single_impl_interface_method_resolves_to_the_implementation() {
    // The actual bug (issue #5): before the fix, the interface declaration and its sole
    // implementation both registered as call targets under the bare name `greet`, so the
    // precision-favouring resolver saw two same-named candidates with no disambiguating qualifier
    // and dropped the call as ambiguous. A BARE call is used deliberately (mirroring the Rust
    // twin's `c.describe()`, which also carries no qualifier): it isolates the exact mechanism this
    // PR fixes — one candidate instead of two — from the tags path's separate qualifier heuristic
    // (see `java_call_through_an_interface_typed_variable_now_resolves_issue_8`, below, for the
    // issue's own `g.greet()` receiver-variable phrasing, fixed separately by issue #8).
    let greeter = (
        "Greeter.java",
        "interface Greeter {\n    String greet();\n}\n",
    );
    let english_greeter = (
        "EnglishGreeter.java",
        "class EnglishGreeter implements Greeter {\n    public String greet() { return \"hello\"; }\n}\n",
    );
    let main = (
        "Main.java",
        "class Main {\n    String run() {\n        return greet();\n    }\n}\n",
    );
    let g = graph_of_lang("java", &[greeter, english_greeter, main]);

    // Both the declaration and the implementation are indexed as nodes.
    let greets: Vec<&GraphNode> = g.nodes.iter().filter(|n| n.label == "greet()").collect();
    assert_eq!(
        greets.len(),
        2,
        "declaration and implementation must both be indexed as nodes; got {greets:?}"
    );

    // ...but the call resolves, and resolves to the IMPLEMENTATION, not the declaration.
    let run = node(&g, "run()");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == run.node_id)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "the call must resolve to exactly one target; got {calls:?}"
    );
    let target = g
        .nodes
        .iter()
        .find(|n| n.node_id == calls[0].target)
        .expect("call target must be an emitted node");
    assert_eq!(
        target.source_file, "EnglishGreeter.java",
        "must resolve to the implementation, not the interface declaration; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_call_through_an_interface_typed_parameter_resolves_to_the_implementation_issue_8() {
    // Issue #8's own literal reproduction, and formerly a documented out-of-scope limitation of
    // issue #5. `g.greet()` where `g: Greeter` used to produce NO `calls` edge: the qualifier was
    // the raw receiver identifier `g` — the PARAMETER name, never `EnglishGreeter`, the type that
    // defines the method — and `resolve::pick`'s single-candidate branch rejected the sole real
    // candidate on a qualifier that could never textually match its scope.
    //
    // Two independent fixes both apply here, and it is worth being precise about which one does
    // the work, because they are not interchangeable (see `callee::receiver_qualifier`'s tiers):
    // tier 1 fires, recovering `g`'s DECLARED TYPE `Greeter` from its `formal_parameter`, and
    // `resolve::pick`'s supers-aware matching accepts `EnglishGreeter` because it
    // `implements Greeter`. Issue #8's lowercase-receiver rule (tier 3) would also make this
    // resolve — but only because there happens to be exactly one candidate. Tier 1 is what keeps
    // it resolving when there are several; see
    // `java_two_same_named_methods_are_told_apart_by_the_receivers_declared_type`.
    let greeter = (
        "Greeter.java",
        "interface Greeter {\n    String greet();\n}\n",
    );
    let english_greeter = (
        "EnglishGreeter.java",
        "class EnglishGreeter implements Greeter {\n    public String greet() { return \"hello\"; }\n}\n",
    );
    let main = (
        "Main.java",
        "class Main {\n    void run(Greeter g) {\n        g.greet();\n    }\n}\n",
    );
    let g = graph_of_lang("java", &[greeter, english_greeter, main]);
    let run = node(&g, "run()");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == run.node_id)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "the call must resolve to exactly one target; got {calls:?}"
    );
    let target = g
        .nodes
        .iter()
        .find(|n| n.node_id == calls[0].target)
        .expect("call target must be an emitted node");
    assert_eq!(
        target.source_file, "EnglishGreeter.java",
        "must resolve to the implementation, not the interface declaration; edges = {:?}",
        g.edges
    );
}

// ── Phase 0: declared-type qualifier recovery (docs/design/spring-aware-graph.md §4.2) ────────

#[test]
fn java_two_class_fixture_with_zero_interfaces_now_produces_a_calls_edge() {
    // The headline regression from the design doc's §4.2: `Helper h = new Helper(); h.help();`
    // across two files, with NO interfaces and NO annotations anywhere, used to produce ZERO
    // `calls` edges — the qualifier recovered from `h.help()` was the bare variable name `"h"`,
    // which never equals the candidate's scope `"Helper"`. `callee::declared_type_of` fixes this
    // by recovering `h`'s declared type from its `local_variable_declaration` instead of using the
    // variable name itself.
    let helper = ("Helper.java", "class Helper { void help() {} }\n");
    let caller = (
        "Caller.java",
        "class Caller { void run() { Helper h = new Helper(); h.help(); } }\n",
    );
    let g = graph_of_lang("java", &[helper, caller]);
    assert!(
        has_call(&g, "run()", "help()"),
        "Helper h = new Helper(); h.help(); must now produce a calls edge; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_field_injected_interface_typed_call_resolves_to_the_single_implementation() {
    // The exact motivating shape from the design doc: `accountService.findById(id)`, where
    // `accountService` is a plain field typed `AccountService` (no annotations — Phase 0 is
    // general Java, not Spring-specific) and `AccountService` has exactly one implementation.
    let account_service = (
        "AccountService.java",
        "interface AccountService {\n    void findById();\n}\n",
    );
    let account_service_impl = (
        "AccountServiceImpl.java",
        "class AccountServiceImpl implements AccountService {\n    public void findById() {}\n}\n",
    );
    let account_controller = (
        "AccountController.java",
        "class AccountController {\n    AccountService accountService;\n    void getAccount() {\n        accountService.findById();\n    }\n}\n",
    );
    let g = graph_of_lang(
        "java",
        &[account_service, account_service_impl, account_controller],
    );
    let get_account = node(&g, "getAccount()");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == get_account.node_id)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "getAccount must resolve to exactly one target; got {calls:?}"
    );
    let target = g
        .nodes
        .iter()
        .find(|n| n.node_id == calls[0].target)
        .expect("call target must be an emitted node");
    assert_eq!(
        target.source_file, "AccountServiceImpl.java",
        "accountService.findById() must resolve to the implementation, via the recovered \
         declared-type qualifier AccountService and its supertype match; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_local_shadowing_a_field_of_a_different_type_resolves_to_the_locals_type() {
    // Nearest-enclosing-scope wins: a local variable declared inside the method body shadows a
    // field of the SAME name but a DIFFERENT type — the call must resolve against the local's
    // type, not the field's, proving the walk checks the innermost scope first.
    let widget = ("Widget.java", "class Widget { void render() {} }\n");
    let panel = ("Panel.java", "class Panel { void render() {} }\n");
    let screen = (
        "Screen.java",
        "class Screen {\n    Widget widget;\n    void draw() {\n        Panel widget = new Panel();\n        widget.render();\n    }\n}\n",
    );
    let g = graph_of_lang("java", &[widget, panel, screen]);
    let draw = node(&g, "draw()");
    let panel_render = g
        .nodes
        .iter()
        .find(|n| n.label == "render()" && n.source_file == "Panel.java")
        .expect("Panel.render node");
    let widget_render = g
        .nodes
        .iter()
        .find(|n| n.label == "render()" && n.source_file == "Widget.java")
        .expect("Widget.render node");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == draw.node_id)
        .collect();
    assert_eq!(calls.len(), 1, "exactly one edge; got {calls:?}");
    assert_eq!(
        calls[0].target, panel_render.node_id,
        "the local `Panel widget` must shadow the `Widget widget` field; edges = {:?}",
        g.edges
    );
    assert_ne!(calls[0].target, widget_render.node_id);
}

#[test]
fn java_declared_type_that_contradicts_the_only_candidate_still_yields_no_edge() {
    // Precision preserved: recovering a REAL declared type doesn't turn off contradiction
    // checking. `f` is declared `Other`, an interface `Foo` never implements — the sole candidate
    // `Foo.m` must NOT be guessed at just because a declared-type qualifier now exists.
    let foo = ("Foo.java", "class Foo { void m() {} }\n");
    let other = ("Other.java", "interface Other {}\n");
    let bar = (
        "Bar.java",
        "class Bar {\n    void run(Other f) {\n        f.m();\n    }\n}\n",
    );
    let g = graph_of_lang("java", &[foo, other, bar]);
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "a declared type unrelated to the only candidate must not resolve; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_call_through_an_undeclared_receiver_resolves_on_the_bare_name() {
    // No local/field/parameter named `obj` is declared anywhere in scope, so `declared_type_of`
    // finds nothing and `obj` is lowercase — the qualifier is dropped entirely (issue #8), and the
    // call resolves on the bare name against the single candidate, exactly as a bare `build()`
    // would.
    //
    // This case CHANGED when issue #8's fix merged with the declared-type recovery. Previously the
    // qualifier fell back to the bare receiver text `"obj"`, which could never match `Widget` and
    // so produced no edge. Issue #8 argues that is the wrong trade — a never-matching qualifier
    // manufactures a false negative for the single most common call shape in idiomatic Java, and a
    // bare call already resolves to a lone same-named candidate with zero type checking, so this
    // adds no NEW mis-attribution risk. The negative twin below is what keeps it honest: several
    // candidates and no qualifier is still dropped as ambiguous, never guessed.
    let widget = ("Widget.java", "class Widget { void build() {} }\n");
    let runner = (
        "Runner.java",
        "class Runner {\n    void go() {\n        obj.build();\n    }\n}\n",
    );
    let g = graph_of_lang("java", &[widget, runner]);
    assert!(
        has_call(&g, "go()", "build()"),
        "an undeclared receiver drops its qualifier and resolves on the bare name; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_undeclared_receiver_with_several_same_named_candidates_stays_ambiguous() {
    // The negative twin of the test above, and the reason dropping the qualifier is safe: with no
    // declared type to narrow on and two same-named candidates, the precision-favouring policy
    // still drops the call rather than guessing one.
    let widget = ("Widget.java", "class Widget { void build() {} }\n");
    let gadget = ("Gadget.java", "class Gadget { void build() {} }\n");
    let runner = (
        "Runner.java",
        "class Runner {\n    void go() {\n        obj.build();\n    }\n}\n",
    );
    let g = graph_of_lang("java", &[widget, gadget, runner]);
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "two same-named candidates with no qualifier must stay ambiguous; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_two_implementations_of_one_interface_stay_ambiguous() {
    // Two genuine bodied implementations of the same interface: candidate count stays > 1 no
    // matter how the qualifier is resolved, because there truly are two. The resolver must never
    // guess between them (design doc §4.3 — that is `@Primary`/`@Qualifier` territory, deferred).
    let service = ("Service.java", "interface Service {\n    void run();\n}\n");
    let a = (
        "AImpl.java",
        "class AImpl implements Service {\n    public void run() {}\n}\n",
    );
    let b = (
        "BImpl.java",
        "class BImpl implements Service {\n    public void run() {}\n}\n",
    );
    let client = (
        "Client.java",
        "class Client {\n    void go(Service s) {\n        s.run();\n    }\n}\n",
    );
    let g = graph_of_lang("java", &[service, a, b, client]);
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "two genuine implementations must stay ambiguous, never guessed; edges = {:?}",
        g.edges
    );
}

#[test]
fn java_call_to_an_interface_method_with_no_implementation_does_not_resolve() {
    // The direct converse of the fix: a declaration alone — no implementation anywhere — must never
    // be treated as a call target. Without the fix this would have been the sole (and therefore
    // "successfully resolving") candidate, silently dispatching a call to a declaration that has no
    // body to run.
    let g = graph_of_lang(
        "java",
        &[(
            "Greeter.java",
            "interface Greeter {\n    String greet();\n}\nclass Main {\n    String run() {\n        return greet();\n    }\n}\n",
        )],
    );
    assert!(
        !g.edges.iter().any(|e| e.relation == "calls"),
        "a call to a declaration-only method must not resolve; edges = {:?}",
        g.edges
    );
}

#[test]
fn typescript_call_to_a_single_impl_interface_method_resolves_to_the_implementation() {
    // The TypeScript twin of the Java case above. Verified against `tree-sitter-typescript`'s
    // `node-types.json`: `method_signature` (interface members) and `abstract_method_signature`
    // (abstract-class members) have no `body` field at all, while `method_definition` requires one
    // — the same bodiless-declaration shape as Java, captured by the SAME `is_call_target` check.
    let greeter = (
        "greeter.ts",
        "export interface Greeter {\n  greet(): string;\n}\n",
    );
    let english_greeter = (
        "english-greeter.ts",
        "export class EnglishGreeter implements Greeter {\n  greet(): string {\n    return 'hello';\n  }\n}\n",
    );
    let main = (
        "main.ts",
        "function run(): string {\n  return greet();\n}\n",
    );
    let g = graph_of_lang("typescript", &[greeter, english_greeter, main]);

    let greets: Vec<&GraphNode> = g.nodes.iter().filter(|n| n.label == "greet()").collect();
    assert_eq!(
        greets.len(),
        2,
        "declaration and implementation must both be indexed as nodes; got {greets:?}"
    );

    let run = node(&g, "run()");
    let calls: Vec<_> = g
        .edges
        .iter()
        .filter(|e| e.relation == "calls" && e.source == run.node_id)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "the call must resolve to exactly one target; got {calls:?}"
    );
    let target = g
        .nodes
        .iter()
        .find(|n| n.node_id == calls[0].target)
        .expect("call target must be an emitted node");
    assert_eq!(
        target.source_file, "english-greeter.ts",
        "must resolve to the implementation, not the interface declaration; edges = {:?}",
        g.edges
    );
}
