# asciidoc-lsp

A tree-sitter based AsciiDoc parser that extracts structural information from `.adoc` files. Built as a proof-of-concept for an AsciiDoc language server.

## What it does

Parses AsciiDoc files using a **dual-grammar approach** (block + inline tree-sitter grammars) and extracts:

- **Sections** — headings with nesting level and line numbers
- **Includes** — `include::` directives with attributes
- **Attributes** — document attribute definitions (`:key: value`)
- **Xrefs** — cross-references (`<<target>>`, `xref:target[]`)
- **Anchors** — anchor definitions (`[[id]]`)

In `--tree` mode, it recursively follows `include::` directives and displays the full document structure as a tree. Attribute substitution in include paths (e.g. `{docdir}`) is supported via `.asciidoctorconfig`.

## Build

```sh
cargo build --release
```

## Usage

```
asciidoc-lsp [OPTIONS] <PATH>

Arguments:
  <PATH>  Path to AsciiDoc file

Options:
      --tree  Show include tree
      --all   Show all extracted information (sections, includes, xrefs, attributes, anchors)
  -h, --help  Print help
```

### Single-file mode

Parse one file and show sections, includes, and attributes:

```sh
$ asciidoc-lsp docs/index.adoc
File: docs/index.adoc

Sections (1):
  L0: asciidoc-lsp Architecture Documentation (line 1)

Includes (6):
  chapters/01_introduction_and_goals.adoc (line 10)
  chapters/04_solution_strategy.adoc (line 12)
  chapters/05_building_block_view.adoc (line 14)
  chapters/08_concepts.adoc (line 16)
  chapters/09_architecture_decisions.adoc (line 18)
  {docdir}/appendix/glossary.adoc (line 20)

Attributes (4):
  :toc: left (line 2)
  :toclevels: 3 (line 3)
  :sectnums:  (line 4)
  :icons: font (line 5)
```

Add `--all` to include xrefs and anchors in the output.

### Include tree mode

Recursively walk all `include::` directives and display the document structure:

```sh
$ asciidoc-lsp --tree docs/index.adoc
index.adoc (1 sections)
├── 01_introduction_and_goals.adoc (4 sections)
├── 04_solution_strategy.adoc (5 sections)
├── 05_building_block_view.adoc (3 sections)
│   ├── transport_layer.adoc (1 sections)
│   ├── workspace_manager.adoc (1 sections)
│   │   └── main.rs ⚠ Cannot resolve path: No such file or directory (os error 2)
│   └── semantic_layer.adoc (4 sections)
├── 08_concepts.adoc (6 sections)
├── 09_architecture_decisions.adoc (4 sections)
└── glossary.adoc (1 sections)
```

Add `--all` to also show xref/anchor counts per file:

```sh
$ asciidoc-lsp --tree --all docs/index.adoc
index.adoc (1 sections)
├── 01_introduction_and_goals.adoc (4 sections) [1 xrefs, 1 anchors]
├── 04_solution_strategy.adoc (5 sections) [1 xrefs]
├── 05_building_block_view.adoc (3 sections) [1 anchors]
│   ├── transport_layer.adoc (1 sections) [1 anchors]
│   ...
```

## Project structure

```
src/
  main.rs        — CLI entry point, output formatting
  parser.rs      — tree-sitter parser initialization (block + inline)
  extractors.rs  — extract sections, includes, xrefs, anchors, attributes from CST
  walker.rs      — recursive include-tree walker with attribute substitution
  types.rs       — data types (Section, IncludeMacro, Xref, Anchor, IncludeTree)
tests/
  fixtures/      — AsciiDoc test fixtures for edge cases
docs/
  index.adoc     — arc42 architecture documentation
```

## Dependencies

- [tree-sitter](https://tree-sitter.github.io/) — incremental parsing
- [tree-sitter-asciidoc](https://github.com/cathaysia/tree-sitter-asciidoc) — block-level AsciiDoc grammar
- [tree-sitter-asciidoc-inline](https://github.com/cathaysia/tree-sitter-asciidoc) — inline-level AsciiDoc grammar
- [clap](https://docs.rs/clap) — CLI argument parsing
- [anyhow](https://docs.rs/anyhow) — error handling

## License

MIT
