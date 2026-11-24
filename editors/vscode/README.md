# AsciiDoc LSP for VS Code

Client for [asciidoc-lsp](https://github.com/seflue/asciidoc-lsp): navigation, completion, hover, diagnostics and refactoring for AsciiDoc.

The platform-specific packages bundle the server. The universal package does not; it uses `asciidoc-lsp` from `PATH` or the path in `asciidoc-lsp.server.path`.

Highlighting comes from the server's semantic tokens, with semantic highlighting turned on for AsciiDoc files whatever the theme says. Colors follow the theme's markup rules; `editor.semanticTokenColorCustomizations` restyles the token types (`heading`, `strong`, `emphasis`, `raw`, `link`, `listMarker`, ...). Nothing is colored until the server has started. The extension also configures `//` and `////` comments and bracket pairs.

If you also install [asciidoctor-vscode](https://marketplace.visualstudio.com/items?itemName=asciidoctor.asciidoctor-vscode), its TextMate grammar and these tokens apply to the same text, and its completion, hover and symbol providers run next to this server, so expect duplicate suggestions.

The language status item of an AsciiDoc file shows the running server version.

## Settings

- `asciidoc-lsp.server.path`: path to the `asciidoc-lsp` binary (empty: bundled binary, then `PATH`).
- `asciidoc-lsp.trace.server`: `off`, `messages` or `verbose`.

## Commands

- `AsciiDoc LSP: Restart Language Server`
- `AsciiDoc LSP: Open Preview to the Side` and `Open Preview in Browser`: open the live preview
- `AsciiDoc LSP: Set Preview TOC`: choose the preview's table of contents

## Building from source

From the repository root, `just install-vscode-plugin` builds the release server, bundles it into a VSIX and installs it with `code --install-extension` (Linux and macOS; needs `npm`, `jq` and the `code` CLI).
