# asciidoc-lsp

Tree-sitter based AsciiDoc parser/analyzer CLI in Rust. Proof-of-concept for an AsciiDoc language server.
Extracts sections, includes, xrefs, anchors, and attributes from `.adoc` files.

## Commands

```sh
cargo build              # Build debug binary
cargo build --release    # Build release binary
cargo test               # Run all tests
cargo clippy -- -D warnings  # Lint (warnings = errors)
cargo fmt                # Format code
cargo fmt -- --check     # Check formatting without changes
just lint                # All checks: fmt + clippy + test
just run docs/index.adoc # Run against a file
just tree                # Show include tree for docs/index.adoc
```

## Architecture

```
src/
  main.rs        — CLI (clap derive), output formatting, .asciidoctorconfig loading
  parser.rs      — tree-sitter parser init (block + inline grammars)
  extractors.rs  — CST walkers: extract sections, includes, attributes, xrefs, anchors
  walker.rs      — recursive include-tree walker with attribute substitution + cycle detection
  types.rs       — data structs: Section, IncludeMacro, Xref, Anchor, AttributeDef, IncludeTree
```

### Dual-Grammar Pattern

Two tree-sitter grammars parse different levels of AsciiDoc:

- **Block** (`tree-sitter-asciidoc`): document structure — sections, includes, attributes
- **Inline** (`tree-sitter-asciidoc-inline`): inline content — xrefs, anchors

Flow: block parse -> `collect_inline_ranges()` -> `set_included_ranges()` -> inline parse

### Key Node Types

Block grammar: `document_title`, `title1`..`title5`, `block_macro` (includes), `document_attribute`
Inline grammar: `inline_macro` (xrefs), `id_assignment` (anchors)

### Data Types (`types.rs`)

- `Section { level, title, line }`
- `IncludeMacro { path, attributes, line }`
- `Xref { target, display_text, line }`
- `Anchor { id, line }`
- `AttributeDef { name, value, line }`
- `IncludeTree { file, includes, sections, xrefs, anchors, error }` — recursive tree

## Test Data

- `docs/` — arc42 architecture documentation (dogfooding / integration tests)
- `tests/fixtures/` — targeted edge-case test files

## Code Style

- Rust edition 2021
- Error handling: `anyhow::Result`
- CLI: `clap` derive macros
- Tree-sitter walking: recursive `walk_for_*` functions in `extractors.rs`
- No `unwrap()` in production code; `unwrap()` OK in tests

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tree-sitter` 0.25 | Incremental parsing engine |
| `tree-sitter-asciidoc` 0.6 | Block-level AsciiDoc grammar |
| `tree-sitter-asciidoc-inline` 0.6 | Inline-level AsciiDoc grammar |
| `clap` 4 (derive) | CLI argument parsing |
| `anyhow` 1 | Error handling |
| `tempfile` 3 (dev) | Temp dirs for tests |
