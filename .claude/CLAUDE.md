# asciidoc-lsp

Tree-sitter based AsciiDoc parser/analyzer CLI in Rust. Proof-of-concept for an AsciiDoc language server.
Extracts sections, includes, xrefs, anchors, and attributes from `.adoc` files.

## Commands

Bevorzuge `just` statt direkte `cargo`-Aufrufe:

```sh
just build               # Build debug binary
just build-release       # Build release binary
just test                # Run all tests
just lint                # Clippy with -D warnings
just fmt                 # Format code
just fmt-check           # Check formatting without changes
just ci                  # All checks: fmt-check + lint + test
just run docs/index.adoc # Run CLI against a file
just lsp                 # Start LSP server
just tree                # Show include tree for docs/index.adoc
just clean               # cargo clean
just fix                 # cargo clippy --fix
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
- Error handling: `thiserror::Error` (typed `CoreError` in asciidoc-core)
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
| `thiserror` 2 | Typed error handling |
| `tempfile` 3 (dev) | Temp dirs for tests |
