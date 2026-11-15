# Manual Neovim Validation Checklist

Manual validation steps for all 12 Acceptance Criteria.
Run against the arc42 fixture (`docs/index.adoc`).

## Prerequisites

```sh
cargo build --release
```

Neovim config (`init.lua` or `ftplugin/asciidoc.lua`):

```lua
vim.lsp.start({
  name = "asciidoc-lsp",
  cmd = { "./target/release/asciidoc-lsp" },
  root_dir = vim.fn.getcwd(),
})
```

## Checklist

- [ ] **AC-1**: `cargo build --release` produces `target/release/asciidoc-lsp` binary
- [ ] **AC-2**: Binary starts as LSP server over stdio (`cargo run -p asciidoc-lsp` waits for input)
- [ ] **AC-3**: Neovim connects via `vim.lsp.start()` — `:LspInfo` shows `asciidoc-lsp` active
- [ ] **AC-4**: `documentSymbol` — Open `docs/index.adoc`, run `:Telescope lsp_document_symbols` or `:lua vim.lsp.buf.document_symbol()` — section hierarchy visible
- [ ] **AC-5**: Go-to-Definition include — Place cursor on `include::chapters/01_introduction_and_goals.adoc[]`, press `gd` — opens the included file
- [ ] **AC-6**: Go-to-Definition xref — Place cursor on a `xref:file.adoc#anchor[text]` reference, press `gd` — jumps to anchor location
- [ ] **AC-7**: Go-to-Definition `<<anchor>>` — Place cursor on a `<<project-goals>>` shorthand xref, press `gd` — jumps to anchor
- [ ] **AC-8**: Diagnostics broken xrefs — Save a file with a broken `<<nonexistent>>` xref — Warning diagnostic appears
- [ ] **AC-9**: Diagnostics broken includes — Save a file with `include::missing.adoc[]` — Error diagnostic appears
- [ ] **AC-10**: Hover attribute — Place cursor on `{imagesdir}` or `{docdir}`, trigger hover (`K`) — shows resolved value
- [ ] **AC-11**: Workspace Symbols — Run `:Telescope lsp_workspace_symbols` with query — finds sections and anchors across files
- [ ] **AC-12**: All above work with the arc42 fixture in `docs/`
