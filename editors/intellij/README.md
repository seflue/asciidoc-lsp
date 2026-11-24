# asciidoc-lsp for JetBrains IDEs

Client for [asciidoc-lsp](https://github.com/seflue/asciidoc-lsp) on [LSP4IJ](https://github.com/redhat-developer/lsp4ij), for IntelliJ IDEA, RustRover, PyCharm and the other JetBrains IDEs from 2025.2 on.

The plugin bundles the server for Linux (x86_64, aarch64), macOS (x86_64, arm64) and Windows (x86_64) and starts it for `.adoc` and `.asciidoc` files. With the [AsciiDoc plugin](https://plugins.jetbrains.com/plugin/7391-asciidoc) installed, it also attaches to that plugin's AsciiDoc language, and both run side by side.

LSP4IJ shows the server's state and log in *View | Tool Windows | Language Servers*.

## Preview

*Open AsciiDoc Preview* (editor context menu or *Find Action*) shows the live preview in the *asciidoc-lsp Preview* tool window; *Open AsciiDoc Preview in Browser* opens it in the browser. The page follows the cursor, and scrolling or double-clicking the page moves the editor. The preview needs Asciidoctor on `PATH`. A chapter file shows inside the top-level document that includes it, as Asciidoctor builds that document. The preview runs next to the AsciiDoc plugin's own preview; neither replaces the other.

## Settings

*Settings | Tools | asciidoc-lsp*:

- *Bundled server* (default) or *Custom server binary*: a path to another `asciidoc-lsp` binary. On platforms without a bundled binary, set this.
- *Check* runs the selected binary with `--version` and shows the result.
- *Apply* restarts the server with the new binary.

## Installation

Close the IDEs first: `installPlugins` refuses to run while the IDE is open. `install-jetbrains-dev` checks this before it builds, when an `ide` argument or a single installed IDE says which IDE it is. From the repository root:

- `just install-jetbrains [ide...]` installs LSP4IJ and the plugin from the latest GitHub release.
- `just install-jetbrains-dev [ide...]` builds the plugin from the clone and installs that, replacing an installed asciidoc-lsp. Run it again to update.

Both run `install.sh` (Linux and macOS), which finds IDEs through their `product-info.json` under JetBrains Toolbox, `~/.local/share/JetBrains`, `/opt`, `/snap` and on macOS `~/Applications` and `/Applications`, plus launchers on `PATH`. An `ide` argument picks IDEs by name (`rustrover`) or by launcher path. `install.sh --dry-run` lists the IDEs and the commands without installing. The script gives the IDE's consent to third-party plugins on your behalf (`--give-consent-to-use-third-party-plugins`).

## Building from source

Needs JDK 21, `just` and `jq`. From the repository root:

- `just build-jetbrains`: plugin ZIP with the host's release binary in `editors/intellij/build/distributions/`
- `just run-jetbrains`: a sandbox IDE with the plugin
- `just test-jetbrains`: headless platform tests against the real server, then the `install.sh` tests
- `just verify-jetbrains [ide-dir...]`: JetBrains Plugin Verifier against installed IDEs, or the build platform without arguments
