# lci-codegraph-model

The shared vocabulary of the [`lci-codegraph`](https://github.com/vymalo/lci-codegraph) workspace:
the graph's node/edge types, the node-id format, and the contract a framework-aware extractor speaks.

Deliberately tiny, with `serde` as its only dependency. It sits at the bottom of the workspace's
dependency graph so that the core extractor and a framework extractor can both speak it without
depending on each other — that is what makes a plugin-shaped crate like
[`lci-codegraph-spring`](https://crates.io/crates/lci-codegraph-spring) possible without a cycle.

## Contents

- **`GraphNode`, `GraphEdge`, `Graph`** — a flat, canonicalised (sorted, deduplicated) structural
  graph. Field sets mirror the payload shape a downstream host submits.
- **`def_node_id`** — the `<file>#<line>:<name>` id format, shared rather than duplicated. Anything
  emitting an edge to a definition has to produce byte-identical ids to the ones the extraction pass
  produced, and two copies of a format string is exactly how that silently drifts.
- **`FrameworkFacts` / `FrameworkCallTarget`** — what a framework extractor may contribute for one
  file: additive nodes, additive edges, and call-target candidates the plain language rules cannot
  see (a Spring Data repository method, a Feign client method).

`FrameworkFacts` is **additive-only** by construction. There is no way to express "remove this
candidate" or "override that resolution", so a framework extractor can widen what the graph sees but
can never weaken the resolver's precision-favouring guarantees. That property is what bounds the blast
radius of getting a framework wrong.

## Licence

MIT. Part of the `lci-codegraph` workspace.
