//! Spring Data repository terminal-call-target carve-out (§4.3): a bodiless `method_declaration`
//! directly inside an interface that `extends` a Spring Data marker interface is kept as a valid call
//! target, even though the general declaration-exclusion rule (issue #5 — a bodiless declaration is
//! indexed but never a call target, because a call dispatches to an implementation, never a
//! declaration) would otherwise make it permanently unreachable.
//!
//! This is not "guess when unsure." Spring Data *positively guarantees* no other implementation will
//! ever exist in source for a method like this — it generates one from the method name at runtime, a
//! documented contract of the framework, not an inference over this repo's code. Without the
//! carve-out, `AccountRepository.findByEmail` would go from "resolves to the (wrong) declaration" to
//! "no candidate at all, silently invisible" — a real coverage regression even though the general
//! rule it collides with is itself correct.

use crate::java_type::{extends_interfaces_names, text};
use lci_codegraph_model::{FrameworkCallTarget, FrameworkFacts, def_node_id};
use tree_sitter::Node;

/// A curated allowlist of Spring Data's own marker interfaces, matched by SIMPLE name only —
/// resolving the fully-qualified type would need classpath information this crate does not have and
/// must not try to get (§4.3, §5.2). This is ongoing curation work, not a one-time cost: Spring Data
/// adds new store-specific marker interfaces (a new backing store, a new reactive variant) across
/// releases without this crate being told. An interface extending a marker not yet listed here simply
/// fails to get the carve-out — a missed opportunity, not a wrong answer, which is the correct
/// precision-favouring failure mode, but worth knowing about the next time this list looks stale.
const REPOSITORY_MARKERS: [&str; 13] = [
    "Repository",
    "CrudRepository",
    "ListCrudRepository",
    "PagingAndSortingRepository",
    "ListPagingAndSortingRepository",
    "JpaRepository",
    "ReactiveCrudRepository",
    "ReactiveSortingRepository",
    "RxJava3CrudRepository",
    "MongoRepository",
    "R2dbcRepository",
    "CassandraRepository",
    "ElasticsearchRepository",
];

/// Emit one [`FrameworkCallTarget`] per bodiless `method_declaration` directly inside `interface`, iff
/// `interface`'s own `extends` clause names a [`REPOSITORY_MARKERS`] entry by simple name.
///
/// Two known limits, deliberately not designed around (§4.3 — noted so they are not rediscovered as a
/// surprise later):
///
/// - **Transitive markers.** `interface AccountRepository extends BaseRepository` where
///   `BaseRepository` (defined in a DIFFERENT file) itself `extends JpaRepository` is NOT recognised —
///   the marker must be named directly in THIS interface's own `extends` clause. Seeing through a
///   user's own base interface needs cross-file knowledge a per-file pass does not have.
/// - **The `<Name>Impl` escape hatch.** Spring Data's own convention for custom logic is a companion
///   `AccountRepositoryImpl` class, which — when present — provides a real bodied implementation that
///   SHOULD win over this carve-out. Not handled here: with both present, the resolver sees two
///   candidates for the same name (the bodied `Impl` method, and this carve-out target) and drops the
///   call as ambiguous. That is the correct, precision-favouring outcome, just not the most useful
///   one.
pub(crate) fn extract(
    interface: &Node,
    source: &[u8],
    source_file: &str,
    facts: &mut FrameworkFacts,
) {
    let supers = extends_interfaces_names(interface, source);
    if !supers
        .iter()
        .any(|s| REPOSITORY_MARKERS.contains(&s.as_str()))
    {
        return;
    }
    let Some(interface_name) = interface
        .child_by_field_name("name")
        .and_then(|n| text(&n, source))
    else {
        return;
    };
    let Some(body) = interface.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        // Only a BODILESS method is the Spring-Data-generated carve-out; a method with a body is an
        // ordinary declaration+implementation (e.g. a `default` method) and needs no framework help —
        // the general rule already resolves it correctly on its own.
        if member.child_by_field_name("body").is_some() {
            continue;
        }
        let Some(name) = member
            .child_by_field_name("name")
            .and_then(|n| text(&n, source))
        else {
            continue;
        };
        let start_line = member.start_position().row as i64 + 1;
        facts.call_targets.push(FrameworkCallTarget {
            node_id: def_node_id(source_file, start_line, &name),
            name,
            scope: Some(interface_name.clone()),
            scope_supers: supers.clone(),
        });
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
    fn a_bodiless_method_on_a_jpa_repository_becomes_a_call_target() {
        let src = r#"
            interface AccountRepository extends JpaRepository<Account, Long> {
                Optional<Account> findByEmail(String email);
            }
        "#;
        let facts = extract_from(src, "AccountRepository.java");
        assert_eq!(facts.call_targets.len(), 1);
        let target = &facts.call_targets[0];
        assert_eq!(target.name, "findByEmail");
        assert_eq!(target.scope.as_deref(), Some("AccountRepository"));
        assert_eq!(target.scope_supers, vec!["JpaRepository".to_string()]);
    }

    #[test]
    fn every_marker_interface_in_the_allowlist_is_recognised() {
        for marker in REPOSITORY_MARKERS {
            let src = format!("interface R extends {marker} {{ Object findIt(); }}");
            let facts = extract_from(&src, "R.java");
            assert_eq!(
                facts.call_targets.len(),
                1,
                "{marker} should trigger the carve-out"
            );
        }
    }

    #[test]
    fn a_non_spring_data_super_interface_does_not_trigger_the_carve_out() {
        let src = "interface Ordering extends Comparable<Ordering> { int compareTo(Ordering o); }";
        let facts = extract_from(src, "Ordering.java");
        assert!(facts.call_targets.is_empty());
    }

    #[test]
    fn an_interface_with_no_extends_clause_at_all_does_not_trigger_the_carve_out() {
        let src = "interface Plain { void m(); }";
        let facts = extract_from(src, "Plain.java");
        assert!(facts.call_targets.is_empty());
    }

    #[test]
    fn a_bodied_method_on_a_repository_interface_is_not_a_carve_out_target() {
        // A `default` method with a real body is an ordinary implementation; the general resolver
        // rule already handles it, so the carve-out must not also claim it.
        let src = r#"
            interface AccountRepository extends JpaRepository<Account, Long> {
                Optional<Account> findByEmail(String email);
                default String describe() { return "account repository"; }
            }
        "#;
        let facts = extract_from(src, "AccountRepository.java");
        assert_eq!(facts.call_targets.len(), 1);
        assert_eq!(facts.call_targets[0].name, "findByEmail");
    }

    #[test]
    fn the_call_targets_node_id_matches_the_shared_def_node_id_format() {
        let src = "interface R extends CrudRepository<Account, Long> {\n    Object findIt();\n}\n";
        let facts = extract_from(src, "R.java");
        // `findIt` is declared on line 2 (1-based); no annotation precedes it, so the
        // `method_declaration`'s own span starts right at the return type.
        assert_eq!(facts.call_targets[0].node_id, "R.java#2:findIt");
    }

    #[test]
    fn several_bodiless_methods_each_become_their_own_call_target() {
        let src = r#"
            interface AccountRepository extends JpaRepository<Account, Long> {
                Optional<Account> findByEmail(String email);
                Optional<Account> findByStatus(String status);
            }
        "#;
        let facts = extract_from(src, "AccountRepository.java");
        let names: Vec<_> = facts.call_targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["findByEmail", "findByStatus"]);
    }
}
