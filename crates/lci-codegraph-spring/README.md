# lci-codegraph-spring

Spring-aware structural facts for [`lci-codegraph`](https://github.com/vymalo/lci-codegraph): HTTP
routes, outbound service boundaries, and Spring Data repository methods, read out of a Java file that
has already been parsed.

```rust,ignore
let facts = lci_codegraph_spring::extract_facts(&tree, source, "AccountController.java");
```

## Why this is a separate crate

The core extractor's identity is that it holds no knowledge of any specific library — it parses a
checkout and returns nodes and edges. Spring-awareness does not cross that line (it still only reads
an AST), but it does add something the core deliberately avoids: knowledge of a specific *library's*
API surface. An annotation allowlist is a different and narrower coupling than a tree-sitter grammar.
A grammar changes rarely and is maintained by someone else; Spring's annotation set moves every
release, and Spring Boot 3's `javax` → `jakarta` migration is the kind of churn this signs up for.

Keeping it behind a crate boundary makes *"does the core know about Spring?"* answerable by reading a
manifest instead of by trusting a comment. See `docs/design/spring-aware-graph.md` §5.2.

## What it extracts

- **`route` nodes** — one per HTTP endpoint the repo *serves*, from the `@RequestMapping` family, with
  the class-level prefix composed onto the method-level suffix. `source_file`/`start_line` point at the
  handler method, so finding the route is finding where it is handled.
- **`external_service` nodes** — one per distinct `@FeignClient` target, plus call targets routing that
  interface's methods to the boundary marker. The callee's real implementation lives in another
  repository; the honest target is the boundary, not a symbol.
- **Spring Data call targets** — a bodiless method on an interface extending a Spring Data marker
  (`JpaRepository`, `CrudRepository`, …) stays a valid terminal call target. Spring generates the
  implementation from the method name at runtime, so "no implementation anywhere in source" is a
  documented contract here rather than missing code.

Output is [`FrameworkFacts`](https://docs.rs/lci-codegraph-model), which is **additive-only**: it can
contribute nodes, edges and call-target candidates, and has no way to express "drop this candidate" or
"override that resolution". Getting Spring wrong can widen what the graph sees; it cannot weaken the
core resolver's guarantees.

## What it deliberately does not do

Where Spring's real behaviour depends on information that is not in the source, this crate stays
silent rather than guessing — a confident wrong answer about wiring is worse than no answer, because
it looks authoritative.

- **XML bean configuration** — not parsed. A DI graph built from Java files is exactly that.
- **`@ComponentScan` filters, `@Profile`, `@ConditionalOnProperty`** — which bean is actually active is
  not statically decidable, for any tool.
- **Transitive repository markers** — an interface extending *your own* base interface that extends
  `JpaRepository` is not recognised; that needs cross-file knowledge a per-file pass does not have.
- **The `<Name>Impl` escape hatch** — a Spring Data companion implementation class, when present,
  should win over the carve-out. Not handled: the resolver sees two candidates and drops the call,
  which is correct but not maximally useful.
- **`@Primary`/`@Qualifier` disambiguation and DI wiring edges** — deliberately unbuilt, gated on a
  consumer-side change tracked separately.

## Licence

MIT. Part of the `lci-codegraph` workspace.
