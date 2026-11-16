# AsciiDoc LSP for VS Code

Client for [asciidoc-lsp](https://github.com/seflue/asciidoc-lsp): navigation, completion, hover, diagnostics and refactoring for AsciiDoc.

The platform-specific packages bundle the server. The universal package does not; it uses `asciidoc-lsp` from `PATH` or the path in `asciidoc-lsp.server.path`.

Highlighting comes from the server's semantic tokens, with semantic highlighting turned on for AsciiDoc files whatever the theme says. Colors follow the theme's markup rules; `editor.semanticTokenColorCustomizations` restyles the token types (`heading`, `strong`, `emphasis`, `raw`, `link`, `listMarker`, ...). Nothing is colored until the server has started.

## Settings

- `asciidoc-lsp.server.path`: path to the `asciidoc-lsp` binary (empty: bundled binary, then `PATH`).
- `asciidoc-lsp.trace.server`: `off`, `messages` or `verbose`.

## Commands

- `AsciiDoc LSP: Restart Language Server`
