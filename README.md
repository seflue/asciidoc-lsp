# asciidoc-lsp

![Latest release](https://img.shields.io/github/v/release/seflue/asciidoc-lsp)

A language server for [AsciiDoc](https://asciidoc.org/), built with Rust and [tree-sitter](https://tree-sitter.github.io/).

Provides navigation, diagnostics, completions, and refactoring for AsciiDoc projects. It follows `include::` and cross-references across files and reads `.asciidoctorconfig`.

## Features

### Navigation

- **Go to Definition** (`gd`): jump to targets of `include::`, `xref:`, `<<anchor>>`, `image::`, and `{attribute}` references
- **Find References** (`gr`): find all usages of attributes, includes, and anchors across the workspace
- **Document Symbols**: nested section outline for the current file
- **Workspace Symbols**: search sections, attributes, and includes across all files

### Diagnostics

- Broken cross-references (`xref:` / `<<>>` pointing to non-existent anchors)
- Broken includes (missing target files)
- Duplicate anchor detection
- Live updates on every edit
- Workspace-wide analysis on open and save

### Completions

- **Semantic**: attribute references (`{attr}`), include paths with attribute substitution, xref targets, anchor IDs
- **Syntax**: block macros (`include::`, `image::`, `video::`), admonitions (`NOTE`, `WARNING`, ...), document attributes (`:toc:`, `:sectnums:`, ...), inline formatting (`kbd:`, `btn:`, ...)

### Hover

- Attribute definitions and resolved values
- Include target paths
- Xref and anchor context
- Image paths

### Highlighting

- **Semantic tokens** (`textDocument/semanticTokens/full`): headings, inline formatting, links, comments, lists, admonitions and block markers, from the highlight queries of the tree-sitter-asciidoc grammar. For editors without their own Tree-sitter highlighter. Code in `[source,lang]` blocks is not syntax-highlighted (it gets a plain raw token).

### Refactoring

- **Rename** (`vim.lsp.buf.rename()`): rename anchors and attributes across the entire workspace, with `prepareRename` validation

### Preview

- Live HTML preview in the browser, converted by `asciidoctor` (must be on `PATH`) and served by the language server on `127.0.0.1`.
- Shows the whole document that includes the current file, with unsaved changes of open buffers, or only the current file.
- The page follows the cursor. Scrolling the page moves the editor, and a double-click on a block opens its source line, also in an included file.
- A TOC select in the toolbar puts the table of contents on the left or right, or hides it. Auto uses the document's `:toc:` or a sidebar for long documents.
- Clients start it with the `asciidoc.preview.open` command; asciidoc.nvim and the VS Code extension provide it.

### Workspace

- Reads `.asciidoctorconfig` and resolves attributes in it, including `{asciidoctorconfigdir}`.
- Follows `include::` chains across files.
- Picks up changes to files on disk.

## Installation

### Prebuilt binary (recommended)

Download the archive for your platform from [GitHub Releases](https://github.com/seflue/asciidoc-lsp/releases). Each release provides one archive per target, containing the single binary `asciidoc-lsp` (`asciidoc-lsp.exe` on Windows) at the archive root:

| Platform | Target | Archive |
|----------|--------|---------|
| Linux x86_64 | `x86_64-unknown-linux-musl` | `asciidoc-lsp-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64 | `aarch64-unknown-linux-musl` | `asciidoc-lsp-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `asciidoc-lsp-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `asciidoc-lsp-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `asciidoc-lsp-x86_64-pc-windows-msvc.zip` |

Linux and macOS (replace `TARGET` with a target from the table):

```sh
TARGET=x86_64-unknown-linux-musl
curl -fsSL "https://github.com/seflue/asciidoc-lsp/releases/latest/download/asciidoc-lsp-${TARGET}.tar.gz" | tar -xz
mkdir -p ~/.local/bin
mv asciidoc-lsp ~/.local/bin/
asciidoc-lsp --version
```

Make sure `~/.local/bin` is on your `PATH`.

Windows (PowerShell):

```powershell
$Target = "x86_64-pc-windows-msvc"
Invoke-WebRequest "https://github.com/seflue/asciidoc-lsp/releases/latest/download/asciidoc-lsp-$Target.zip" -OutFile asciidoc-lsp.zip
Expand-Archive asciidoc-lsp.zip -DestinationPath "$env:LOCALAPPDATA\asciidoc-lsp"
& "$env:LOCALAPPDATA\asciidoc-lsp\asciidoc-lsp.exe" --version
```

Add `%LOCALAPPDATA%\asciidoc-lsp` to your `PATH`, or give editors the full path to `asciidoc-lsp.exe`.

`asciidoc-lsp --version` prints the installed version. Editors and Neovim's `:checkhealth` can use it to verify the installation.

### From source

Requires [just](https://github.com/casey/just). The pinned Rust toolchain (`rust-toolchain.toml`) is installed automatically by `rustup`.

```sh
git clone --recurse-submodules https://github.com/seflue/asciidoc-lsp.git
cd asciidoc-lsp
just build-release
# Binary: target/release/asciidoc-lsp
```

To install into `~/.cargo/bin` instead:

```sh
cargo install --path crates/asciidoc-lsp
```

## Editor Setup

asciidoc-lsp communicates via LSP over stdio. Install the binary first and make sure it is on your `PATH` (check with `asciidoc-lsp --version`).

### Neovim

**With the plugin.** [asciidoc.nvim](https://github.com/seflue/asciidoc.nvim) bundles the Tree-sitter queries and the LSP configuration. See its README for installation and the minimum asciidoc-lsp version it requires. `:checkhealth asciidoc` reports problems with the server setup.

**Without the plugin**, using Neovim's built-in LSP configuration (0.11+):

```lua
vim.lsp.config('asciidoc_lsp', {
  cmd = { 'asciidoc-lsp' },
  filetypes = { 'asciidoc' },
  root_markers = { '.asciidoctorconfig.adoc', '.asciidoctorconfig', '.git' },
  -- Let the server watch .asciidoctorconfig and .adoc files on disk.
  capabilities = {
    workspace = { didChangeWatchedFiles = { dynamicRegistration = true } },
  },
})
vim.lsp.enable('asciidoc_lsp')
```

On Linux, Neovim leaves file watching off by default, which is why the config turns it on. Install `inotifywait` (inotify-tools) as well: without it, changes to files in subdirectories never reached the server in testing. `:checkhealth vim.lsp` shows the backend Neovim uses under "file watching".

The server sends semantic tokens. Neovim already highlights AsciiDoc with Tree-sitter, and the tokens would be drawn over it, so drop the capability when the client initializes: add `on_init = function(client) client.server_capabilities.semanticTokensProvider = nil end` to the config above. The plugin does this itself.

Recommended companion plugins:

- [trouble.nvim](https://github.com/folke/trouble.nvim): workspace-wide diagnostics view
- [live-rename.nvim](https://github.com/saecki/live-rename.nvim): inline rename preview (works with `prepareRename`)

### IntelliJ IDEA and other JetBrains IDEs

The asciidoc-lsp plugin runs on [LSP4IJ](https://github.com/redhat-developer/lsp4ij) (Red Hat) and bundles the server. It needs an IDE from 2025.2 on. From a clone, close the IDE and run `just install-jetbrains`: it finds the installed JetBrains IDEs, asks which to use when there are several, and installs LSP4IJ and the plugin from the latest release. `just install-jetbrains-dev` builds the plugin from the clone and installs that instead. Settings and building are described in the [plugin README](editors/intellij/README.md).

Without the plugin, LSP4IJ can attach the server as a *user-defined language server*:

1. Install the LSP4IJ plugin from the JetBrains Marketplace.
2. Open the *New Language Server* dialog, either with `[+]` at the top of `Settings | Languages & Frameworks | Language Servers (LSP4IJ)` or from the menu in the LSP console.
3. On the *Server* tab, set a name (for example `asciidoc-lsp`) and the command `asciidoc-lsp` (or the full path to the binary). LSP4IJ talks stdio by default.
4. On the *Mappings* tab, add a *File name pattern* `*.adoc` with *Language ID* `asciidoc`.
5. Confirm with OK. LSP4IJ starts the server when you open a matching file; its output is shown in the LSP console.

LSP4IJ can export a user-defined language server as a zip, so you can hand the finished configuration to colleagues. See the [LSP4IJ documentation](https://github.com/redhat-developer/lsp4ij/blob/main/docs/UserDefinedLanguageServer.md).

### VS Code

Download `asciidoc-lsp-<platform>.vsix` from a [GitHub release](https://github.com/seflue/asciidoc-lsp/releases) and run `code --install-extension asciidoc-lsp-<platform>.vsix`. It bundles the server. From a clone, `just install-vscode-plugin` builds and installs it instead.

On platforms without their own VSIX (for example `win32-arm64`), use `asciidoc-lsp-universal.vsix` and install the server as described above. Settings, commands and highlighting are described in the [extension README](editors/vscode/README.md).

### Other editors

Any editor with LSP support can use asciidoc-lsp: configure the client to run the `asciidoc-lsp` binary over stdio for `.adoc` files with language ID `asciidoc`.

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
  asciidoc-lsp/     # LSP server (tower-lsp-server)
  asciidoc-cli/     # CLI for inspection and debugging
```

## Development

Requires [just](https://github.com/casey/just) and a Rust toolchain (the version pinned in `rust-toolchain.toml`; `rustup` installs it automatically).

Clone with submodules:

```sh
git clone --recurse-submodules https://github.com/seflue/asciidoc-lsp.git
# Or if already cloned:
git submodule update --init
```

```sh
just build          # Debug build
just test           # Run all tests
just lint           # Clippy with -D warnings (pedantic)
just fmt-check      # Check formatting
just ci             # All checks: fmt-check + lint + test
just test-js        # Preview page JS tests (needs Node, not part of ci)
just test-jetbrains # JetBrains plugin and install.sh tests (needs JDK 21, not part of ci)
just lsp            # Start the LSP server (for manual testing)
```

## Status

Below 1.0: functional, but API and configuration may change.

## Contributing

Contributions welcome. Run `just ci` before submitting changes; setup is described under Development.

## License

Licensed under the [Apache License 2.0](LICENSE).

## Trademark

AsciiDoc® is a registered trademark of the Eclipse Foundation, Inc.
