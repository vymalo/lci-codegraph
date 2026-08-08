# Contributing

This repository follows the [ADORSYS-GIS AI Governance](https://adorsys-gis.github.io/ai-governance/) discipline:
**AI may accelerate the work, but humans own intent, verification, and consequences.**

## How to contribute

- **Open issues** with the structured forms — [Epic](.github/ISSUE_TEMPLATE/epic.yml),
  [User Story](.github/ISSUE_TEMPLATE/user-story.yml), or
  [Development Ticket](.github/ISSUE_TEMPLATE/dev-ticket.yml). Blank issues are disabled on purpose.
- **Open pull requests** using the [pull request template](.github/PULL_REQUEST_TEMPLATE.md). Fill in every section.
- Always complete the **AI Usage Declaration**, link a **source of truth**, and attach **verification evidence**.

## Definition of Ready / Done gates

Work is **Ready** only when its intent is clear, its source of truth is linked, its scope and acceptance criteria are explicit, and any AI-generated content has been reviewed by a human. Work is **Done** only when acceptance criteria are met, tests pass, verification evidence is attached, and a named human owner accepts responsibility for the result. A governance CI check enforces that every PR body declares AI usage, references a source of truth, and shows verification evidence — see the [AI Working Agreement](https://adorsys-gis.github.io/ai-governance/12-ai-working-agreement) and the [Doctrine](https://adorsys-gis.github.io/ai-governance/13-doctrine).

## Local development

### Prerequisites

- Rust 1.88+ (the MSRV is `workspace.package.rust-version` in the root `Cargo.toml`, which explains
  why it is 1.88 and not the edition-2024 floor of 1.85)
- Docker daemon (required for container-based tests; optional for basic development)

### Verification commands

Before opening a pull request, verify your changes locally:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --features container-tests   # requires a running Docker daemon
```

The repo root is a Cargo workspace (`lci-codegraph` plus `crates/lci-codegraph-model` and
`crates/lci-codegraph-spring` — see `README.md`'s "Workspace layout"). Plain `cargo test`/
`cargo clippy` without `--workspace` only cover the root package and silently skip the two `crates/`
members' own unit tests and `crates/lci-codegraph-spring/tests/`.

All of these must pass before merging.

### Commit message conventions

Follow [Conventional Commits](https://www.conventionalcommits.org/) for your commit messages:

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `chore`, `ci`. Breaking changes should include `BREAKING CHANGE:` in the footer or append `!` after the type.

Example:

```
feat(chunker): add support for bounded PDF text extraction

This adds a new `PdfChunker` type for chunking PDF text content with
size limits to fit within embedding token windows.

Fixes #42
```

## Working with AI code review

Automated AI reviewers are **advisory — never a merge gate.** Only
**deterministic** checks (the governance CI check, linting, tests) may block a merge, because their
output is reproducible and cannot be confabulated. Keep AI review as a non-required status check.

**Every AI-review finding is a claim, not a verdict.** Before acting on one that asserts a specific
value or behavior, verify it against the actual cited lines. AI reviewers pattern-match known bug
*shapes* and will confidently assert details about code they did not actually read. The doctrine applies to the
reviewer too: **AI output is not truth.**

### When a finding is a false positive, close the loop — don't just ignore it

1. **Reply with the evidence** — the exact lines or command output that disprove it.
2. **React 👎** on the finding.
3. **Resolve the conversation.**

This three-step loop is not busywork; each step does something that silently ignoring does not:

- **👎 is the only lever that reduces recurrence.** It is the reviewer's feedback channel. Without it,
  the same confabulation fires again every time its trigger reappears.
- **Resolving preserves signal-to-noise.** Real findings get buried under known-false ones if threads
  stay open; resolution stops both humans and the bot from re-litigating settled points.
- **The evidence reply is an audit trail.** The next person — or AI — who hits the same flag finds the
  refutation in-thread and does not have to re-verify from scratch. Silently ignoring a false positive
  looks unaddressed, erodes trust in the review, and teaches the bot nothing.
