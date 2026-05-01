# asciidoc-lsp

A language server for [AsciiDoc](https://asciidoc.org/), built with Rust and [tree-sitter](https://tree-sitter.github.io/).

Provides navigation, diagnostics, completions, and refactoring for AsciiDoc projects — with full workspace awareness including `include::` resolution, cross-references, and `.asciidoctorconfig` support.

## Features

### Navigation

- **Go to Definition** (`gd`) — jump to targets of `include::`, `xref:`, `<<anchor>>`, `image::`, and `{attribute}` references
- **Find References** (`gr`) — find all usages of attributes, includes, and anchors across the workspace
- **Document Symbols** — nested section outline for the current file
- **Workspace Symbols** — search sections, attributes, and includes across all files

### Diagnostics

- Broken cross-references (`xref:` / `<<>>` pointing to non-existent anchors)
- Broken includes (missing target files)
- Duplicate anchor detection
- Live updates on every edit
- Workspace-wide analysis on open and save

### Completions

- **Semantic** — attribute references (`{attr}`), include paths with attribute substitution, xref targets, anchor IDs
- **Syntax** — block macros (`include::`, `image::`, `video::`), admonitions (`NOTE`, `WARNING`, ...), document attributes (`:toc:`, `:sectnums:`, ...), inline formatting (`kbd:`, `btn:`, ...)

### Hover

- Attribute definitions and resolved values
- Include target paths
- Xref and anchor context
- Image paths

### Refactoring

- **Rename** (`vim.lsp.buf.rename()`) — rename anchors and attributes across the entire workspace, with `prepareRename` validation

### Workspace Intelligence

- Automatic workspace root detection (`.asciidoctorconfig` → `.git` → fallback)
- `.asciidoctorconfig` attribute loading and substitution
- Synthetic `{asciidoctorconfigdir}` attribute
- Recursive include-tree resolution with attribute expansion
- Incremental workspace rebuild on file changes

## Installation

### From source

Requires Rust 1.70+.

```sh
git clone --recurse-submodules https://github.com/TODO/asciidoc-lsp.git
cd asciidoc-lsp
cargo install --path crates/asciidoc-lsp
```

Or build without installing:

```sh
cargo build --release
# Binary: target/release/asciidoc-lsp
```

## Editor Setup

### Neovim (nvim-lspconfig)

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')

if not configs.asciidoc_lsp then
  configs.asciidoc_lsp = {
    default_config = {
      cmd = { 'asciidoc-lsp' },
      filetypes = { 'asciidoc', 'asciidoctor' },
      root_dir = lspconfig.util.root_pattern('.asciidoctorconfig', '.git'),
    },
  }
end

lspconfig.asciidoc_lsp.setup {}
```

Recommended companion plugins:

- [trouble.nvim](https://github.com/folke/trouble.nvim) — workspace-wide diagnostics view
- [live-rename.nvim](https://github.com/saecki/live-rename.nvim) — inline rename preview (works with `prepareRename`)

### Other editors

asciidoc-lsp communicates via LSP over stdio. Any editor with LSP support can use it — configure the client to run the `asciidoc-lsp` binary.

## CLI

The project also includes `asciidoc-cli`, a command-line tool for inspecting AsciiDoc document structure:

```sh
cargo install --path crates/asciidoc-cli

# Parse a file and show sections, includes, attributes
asciidoc-cli docs/index.adoc

# Recursively walk include tree
asciidoc-cli --tree docs/index.adoc

# Include xrefs and anchors in output
asciidoc-cli --tree --all docs/index.adoc
```

## Architecture

```
crates/
  asciidoc-core/    # Parser, extractors, workspace model, diagnostics
  asciidoc-lsp/     # LSP server (tower-lsp)
  asciidoc-cli/     # CLI for inspection and debugging
```

**Dual-grammar parsing:** Two tree-sitter grammars cover different levels of AsciiDoc syntax:

- **Block** (`tree-sitter-asciidoc`) — document structure: sections, includes, attributes
- **Inline** (`tree-sitter-asciidoc-inline`) — inline content: xrefs, anchors

## Development

Requires [just](https://github.com/casey/just) and a Rust toolchain (1.70+).

Clone with submodules:

```sh
git clone --recurse-submodules <repo-url>
# Or if already cloned:
git submodule update --init
```

```sh
just build          # Debug build
just test           # Run all tests (300+)
just lint           # Clippy with -D warnings (pedantic)
just fmt-check      # Check formatting
just ci             # All checks: fmt-check + lint + test
just lsp            # Start the LSP server (for manual testing)
```

## Status

Pre-release (`v0.1.0-dev`). Functional and tested with 300+ tests, but API and configuration may change.

## Contributing

Contributions welcome. The project uses a Git submodule for the tree-sitter grammar.
After cloning, ensure submodules are initialized (`git submodule update --init`).
Please run `just ci` before submitting changes.

## License

TBD
