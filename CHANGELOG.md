# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Source-agnostic indexing core (`src/input.rs`, ADR-0086): `RawInput` (a logical path plus bytes,
  with an optional explicit `language` override), `IndexOptions`, a push-based `Indexer`
  (`new`/`push`/`record_pruned`/`finish`), and the `index_inputs(iter, &options)` convenience. A
  filesystem checkout is now one *reader* among several possible ones — a caller with content already
  in memory (a git object store, a tarball stream, an HTTP fetch, editor buffers, DB rows) can index it
  directly, with no checkout materialised on disk.
- `FsSource`: the filesystem reader, now public and directly usable as an `Iterator<Item = RawInput>`,
  so filesystem inputs can be mixed with in-memory ones in a single `Indexer`.
- `IndexStats::files_skipped_too_large` and `IndexStats::files_skipped_unsupported` counters, so a
  too-large input and an input with no determinable language are each individually diagnosable instead
  of both looking like "nothing got indexed."
- Semantic embeddings against an OpenAI-compatible `/embeddings` endpoint (`src/embed/`), enabled by
  `OPENAI_BASE_URL` (`EmbedConfig::from_env`) or a caller-supplied `WalkOptions::embed`. No local/
  in-process model and no Cargo feature gate — embedding is the live path whenever configured, not a
  dormant one. The embedded text is graph-aware: a small deterministic header (enclosing container,
  callees, callers) derived from the resolved `Graph` is prepended to each chunk's own content before
  it's sent. `embed::embed_output` runs the step over any `IndexOutput` — not just `walk_checkout`'s —
  after the graph is resolved, since the header needs cross-file callers/callees that don't exist until
  every input has been indexed (see `docs/architecture.md`).
- `IndexStats::chunks_embedded` and `IndexStats::embed_batches` counters, logged at the end of a run
  that embeds.
- `Chunk::embedding: Option<Vec<f32>>` and `Chunk::embed_input: Option<String>` (both
  `#[serde(skip_serializing_if = "Option::is_none")]`, so existing JSON output is unchanged when
  embedding is off).
- `crates/lci-codegraph-model` and `crates/lci-codegraph-spring`: the repo root is now a Cargo
  workspace, not a single crate. `lci-codegraph` (this package) keeps its existing paths (`src/`,
  `tests/`, `examples/`, `docs/`) and public API unchanged — nothing about depending on it moved.
  `lci-codegraph-model` holds the shared `GraphNode`/`GraphEdge`/`Graph`/`def_node_id` vocabulary plus
  the new `FrameworkFacts`/`FrameworkCallTarget` types (below), depending on nothing but `serde` — the
  bottom of the workspace's dependency graph, so a framework-extractor crate and the core crate can
  both depend on it without depending on each other. `lci-codegraph-spring` is
  `extract_facts(&Tree, source, source_file) -> FrameworkFacts`, a Spring-aware sibling pass run over
  the same parse `graph::extract_file` already has for a Java file, gated on the file importing
  `org.springframework`/`jakarta.persistence` — a non-Spring Java file pays for one import scan and
  nothing more. This is a crate boundary rather than a module because a framework's annotation surface
  is a curated allowlist that churns every release (Spring's own annotation set moves every
  major/minor; this repo's Spring fixtures already straddle the `javax.persistence` →
  `jakarta.persistence` migration) — a materially different, narrower coupling than a tree-sitter
  grammar, which changes rarely and is maintained by someone else. See
  `docs/design/spring-aware-graph.md` §5.2 and `docs/architecture.md` for the full reasoning.
- Two additive `GraphNode` kinds for Spring-annotated Java source, contributed by
  `lci-codegraph-spring` and merged into the graph by `graph::resolve`. Neither is a new relation —
  both are ordinary nodes, reached through the existing `calls` relation, so anything that already
  matches nodes by `node_id`/`label`/`source_file` finds them the same way it finds any other node:
  - `route` — one node per HTTP endpoint (the `@GetMapping`/`@PostMapping`/`@PutMapping`/
    `@DeleteMapping`/`@PatchMapping` family, or a bare `@RequestMapping`), keyed by HTTP method plus
    the composed class+method path (`route:GET:/api/accounts/{email}`). `source_file`/`start_line`
    point at the handler method itself, not a synthetic location, and a `calls` edge runs from the
    route to the handler.
  - `external_service` — one node per distinct `@FeignClient(name = "...")` target
    (`service:payment-service`), pointing at the interface declaration (`GraphNode` has no optional
    fields, so a real location was chosen over a fictional empty one). A call site reaching one of its
    methods resolves to this node instead of silently dropping. Two files declaring the same service
    name collapse deterministically to one node — the lexicographically lowest `(source_file,
    start_line)` wins — rather than producing duplicate `node_id`s.
- A Spring Data repository carve-out in the call-target rule: a bodiless method on an interface whose
  `extends`/`implements` clause names a Spring Data marker interface (`Repository`, `CrudRepository`,
  `PagingAndSortingRepository`, `JpaRepository`, `ReactiveCrudRepository`, `MongoRepository`, …,
  matched by simple name) stays a valid `calls` target instead of being excluded as an unimplemented
  declaration. Spring generates the implementation from the method name at runtime, so no body will
  ever exist anywhere in source — treating the declaration as unreachable, which the general
  declaration-exclusion rule otherwise would, was a real coverage regression, not a correct
  application of caution. The carve-out is per-file: it does not follow a *transitive* marker
  interface, and it does not recognise the `<Name>Impl` companion-class escape hatch Spring Data
  itself supports (see `docs/design/spring-aware-graph.md` §4.3, "What was built").

### Fixed

- **Java call resolution through a field/parameter/local-typed receiver.** A call like `h.help()`
  used to qualify on the *variable name* (`h`) rather than the receiver's *declared type* (`Helper`),
  which the resolver then rejected as a scope mismatch (`"h" != "Helper"`) — so
  `Helper h = new Helper(); h.help();` produced **no** `calls` edge at all, in any Java codebase,
  framework or not. The qualifier is now recovered as the receiver's declared type, resolved by
  walking the same file's AST back to a field, formal parameter, local variable, enhanced-`for`
  variable, or caught exception's declaration (`src/graph/callee.rs::declared_type_of`); a qualifier
  naming an interface/superclass a candidate's enclosing type `extends`/`implements` now also counts
  as a match rather than a contradiction (`Callable::scope_supers`, `resolve::qualifier_matches`).
  This is a **general Java-resolution fix, not a Spring feature** — most of its value lands on
  ordinary, non-Spring Java repositories calling through an interface-typed field or local, which is
  most instance calls in idiomatic Java. It is also the prerequisite the two new node kinds above
  build on: it is what lets a controller's call through a field-injected service interface resolve to
  that interface's sole implementation with zero Spring-specific knowledge involved anywhere in the
  edge (`docs/design/spring-aware-graph.md` §4.2).

### Fixed

- Instance calls through a **variable receiver** (`a.helper()`) now resolve on every tags-driven
  language (Java, TypeScript, JavaScript, Python) — previously the most common call shape in
  idiomatic code produced **zero** `calls` edges (issue #8). The tags-path qualifier extractor
  (`graph::callee::receiver_qualifier`) recorded the receiver's own text as a type qualifier
  (`a`), which could almost never textually equal the callable's declaring-type `scope` (`A`), so
  `resolve::pick`'s single-candidate branch rejected the one correct candidate. A receiver is now
  treated as a type qualifier only when it plausibly names a type (a capitalised identifier, per
  Java/TS/Python convention — matching the existing `Foo.bar()` behaviour); a lowercase-initial
  receiver (a variable or module) yields no qualifier, so the call falls through to bare-name
  resolution — one candidate resolves, several are dropped as ambiguous, exactly like a bare call.

### Changed

- `src/walk.rs` is now a thin filesystem-reader driver: `walk_checkout` builds an `FsSource`, pushes
  every input it yields into an `Indexer`, records the pruned count, calls `finish`, and — when
  `WalkOptions::embed` is set — calls `embed::embed_output` on the result. The one-parse chunk/graph
  logic itself now lives in `Indexer::push`, not in `walk.rs`; embedding never did and still doesn't,
  because it's fallible network I/O and the indexing core is infallible CPU work.
- `IndexStats::files_skipped_binary` now also covers content that fails UTF-8 decoding outright;
  previously that path incremented no counter at all.
- The operator/gitignore path-filtering layer now lives entirely in the reader (`FsSource`), not in the
  indexing core — deciding which inputs are worth handing over is the reader's job, because only the
  reader can avoid paying to produce an input that would just be discarded.
- **Breaking:** `Chunk` no longer derives `Eq` (only `PartialEq`) — a chunk carrying an
  `embedding: Vec<f32>` has no total equality, since floats aren't `Eq`. Downstream code relying on
  `Chunk: Eq` (a `HashSet<Chunk>`/`BTreeSet<Chunk>`, an `Ord`/`dedup` bound requiring it, etc.) will
  fail to compile. Verified none of that exists in this crate's own `src/`, `tests/`, or `examples/`.
- **Breaking:** `Chunk` gained the two fields above. Any downstream construction site using an
  exhaustive `Chunk { .. }` struct literal (rather than `..Default::default()` or a builder) will fail
  to compile until it sets `embedding` and `embed_input` too.
- The crate's dependency boundary is revised: `ureq` is now a dependency, used **solely** for the
  optional embeddings HTTP call — the one part of this crate's job that is inherently a network
  round-trip to a model endpoint. Every other part of the boundary (no `kube`/`sqlx`/forge client) is
  unchanged.

### BREAKING

- `WalkOutput` and `WalkStats` are renamed to `IndexOutput` and `IndexStats`. Migration: replace
  `WalkOutput`/`WalkStats` with `IndexOutput`/`IndexStats` wherever they're named (the field shapes are
  unchanged, plus the new counters above) — `walk_checkout`/`walk_checkout_from_env`'s signatures are
  otherwise unchanged.
- `graph::resolve` gained a second parameter: `resolve(files: Vec<FileSymbols>) -> Graph` is now
  `resolve(files: Vec<FileSymbols>, framework: Vec<FrameworkFacts>) -> Graph`. Migration:
  `resolve(files)` → `resolve(files, Vec::new())` — an empty `framework` vec reproduces the previous
  behaviour exactly, so any caller with no framework facts to contribute (every non-Java caller, and
  every Java repo `lci-codegraph-spring` found nothing Spring-specific in) is unaffected by
  construction, not by a flag.

## [0.1.0] - 2026-08-07

### Added

- Initial standalone release, exported from the Lightbridge Code Intelligence monorepo
- Semantic chunking for source code with bounded token window support
- Cross-file structural call graph extraction for:
  - Rust
  - Python
  - TypeScript/JavaScript (including TSX/JSX)
  - Java
- Composable gitignore-style ignore-list that layers on top of the repository's own `.gitignore`
  rather than replacing it
- Bounded PDF text extraction, guarded for untrusted input (byte cap before parse, pre-flight
  decompression-bomb check, `catch_unwind`, parse timeout)
- Test suite of 182 tests: 130 unit (93.8% line coverage), 47 integration across per-language
  fixtures with committed goldens, and 5 Docker-backed container tests — a Neo4j round-trip
  asserting the downstream retrieval queries, glibc and musl build/run containers, and pinned
  real-world repository clones
- Governance-based contribution workflow with AI usage declarations
- CI covering fmt, clippy, tests, MSRV, coverage floor, rustdoc, `cargo-deny` and a publish
  dry-run, plus a tag-driven crates.io release workflow

### Fixed

- Rust trait methods declared without a default body (`fn greet(&self);`) are now extracted. They
  parse as `function_signature_item` rather than `function_item` and were previously classified as
  nothing at all, so a trait interface produced no graph node and no chunk — invisible to both
  structural and semantic search. Such declarations are indexed as definitions but deliberately are
  **not** call targets: a call dispatches to an implementation, and letting declarations compete for
  the same name would have made every single-impl trait method call ambiguous, and therefore dropped.
- Binary content can no longer reach chunk output. The NUL sniff lived only in `chunk_file`, which
  the graph-enabled walk never calls, so a binary blob that happens to be valid UTF-8 was windowed
  with raw NUL bytes in `Chunk::content`. The guard now sits where a chunk is produced and in the
  walk ahead of both consumers, so the graph no longer ingests binary content either. Skipped files
  are reported via the new `WalkStats::files_skipped_binary` counter instead of vanishing.

### Security

- `pdf-extract` is floored at 0.12, the first release depending on `lopdf >= 0.42`, which patches
  [RUSTSEC-2026-0187](https://rustsec.org/advisories/RUSTSEC-2026-0187) — unbounded recursion on
  deeply nested PDF objects that aborts the process with `SIGABRT`. Because that is an abort and
  not a panic, the crate's `catch_unwind` guard could not contain it.

[0.1.0]: https://github.com/vymalo/lci-codegraph/releases/tag/v0.1.0
