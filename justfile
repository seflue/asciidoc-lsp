# asciidoc-lsp workspace task runner

default:
    @just --list

# Build all crates (debug)
build:
    cargo build

# Build all crates (release)
build-release:
    cargo build --release

# Type-check without building
check:
    cargo check

# Run all tests
test:
    cargo test

# Test the preview page's JS (needs Node 20+, not part of ci)
test-js:
    node --test crates/asciidoc-lsp/tests/js/*.test.js

# Run clippy lints (warnings = errors)
lint:
    cargo clippy -- -D warnings

# Format code
fmt:
    cargo fmt

# Check formatting without changes
fmt-check:
    cargo fmt -- --check

# Auto-fix: format + apply clippy suggestions
fix:
    cargo fmt
    cargo clippy --fix --allow-dirty -- -D warnings

# Test the release scripts in scratch repos
test-release:
    scripts/release_test.sh

# Fail when HEAD carries a v* tag that differs from the Cargo version
check-version:
    scripts/check-version.sh

# Check the architecture rules in arc-rules.toml (frozen: arc-baseline.toml)
arch-check *ARGS:
    cargo arc check {{ARGS}}

# Rewrite arc-baseline.toml from the current violations (review the diff!)
arch-baseline:
    cargo arc check --generate-baseline

# All quality checks (CI / pre-commit)
ci: fmt-check lint test test-release check-version arch-check

# All checks + release build
all: ci build-release

# Run CLI against a file (e.g.: just run docs/index.adoc)
run *ARGS:
    cargo run -p asciidoc-cli -- {{ARGS}}

# Show include tree for docs/index.adoc
tree:
    cargo run -p asciidoc-cli -- --tree docs/index.adoc

# Start the LSP server (stdio)
lsp:
    cargo run -p asciidoc-lsp

# Print the path of the debug LSP binary (honours any configured target-dir)
bin-path:
    @cargo metadata --no-deps --format-version 1 | jq -r '.target_directory + "/debug/asciidoc-lsp"'

# Show lint warnings without -D (for iteration)
lint-warn:
    cargo clippy --workspace 2>&1 | grep -E "^(warning|error)\[" | sort | uniq -c | sort -rn

# Reset manual test plan (checkboxes + renamed test data)
reset-testplan:
    git checkout -- tests/manual-test-plan.adoc

# Remove build artifacts
clean:
    cargo clean

# Cut a release: bump is patch, minor or major (gate: fmt-check, lint, test on main)
release bump:
    RELEASE_LINT_CMD="just fmt-check lint" scripts/release.sh {{bump}}

# Update tree-sitter-asciidoc submodule to latest upstream commit
update-grammar:
    git submodule update --remote vendor/tree-sitter-asciidoc

# Build the VS Code extension with the release server bundled and install it
install-vscode-plugin: build-release
    mkdir -p editors/vscode/server
    cp "$(cargo metadata --no-deps --format-version 1 | jq -r '.target_directory')/release/asciidoc-lsp" editors/vscode/server/
    npm --prefix editors/vscode ci
    npm --prefix editors/vscode run package -- --out "{{justfile_directory()}}/editors/vscode/asciidoc-lsp.vsix"
    code --install-extension "{{justfile_directory()}}/editors/vscode/asciidoc-lsp.vsix" --force

# Copy the host's release binary to editors/intellij/server/<target>/. On Linux the local
# build is glibc-linked but goes under the musl target name the plugin looks up; such a
# ZIP is for this machine only.
[private]
jetbrains-stage: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) target=x86_64-unknown-linux-musl ;;
        Linux-aarch64) target=aarch64-unknown-linux-musl ;;
        Darwin-x86_64) target=x86_64-apple-darwin ;;
        Darwin-arm64) target=aarch64-apple-darwin ;;
        *) echo "no JetBrains plugin target for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
    esac
    rm -rf editors/intellij/server
    mkdir -p "editors/intellij/server/$target"
    cp "$(cargo metadata --no-deps --format-version 1 | jq -r '.target_directory')/release/asciidoc-lsp" "editors/intellij/server/$target/"

# Version of a local plugin build: the server version, the commit, and whether tracked files changed
[private]
jetbrains-dev-version:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "asciidoc-lsp") | .version')-dev"
    if commit="$(git -C "{{justfile_directory()}}" rev-parse --short HEAD 2>/dev/null)"; then
        version="$version+$commit"
        git -C "{{justfile_directory()}}" diff --quiet HEAD || version="$version.dirty"
    fi
    printf '%s\n' "$version"

# Run Gradle for the JetBrains plugin with the local build version and staged binaries
[private]
jetbrains-gradle *args:
    editors/intellij/gradlew -p editors/intellij --console=plain \
        -PpluginVersion="$(just jetbrains-dev-version)" \
        -PserverDir="{{justfile_directory()}}/editors/intellij/server" {{args}}

# Build the JetBrains plugin ZIP with the host's release binary (editors/intellij/build/distributions/)
build-jetbrains: jetbrains-stage
    just jetbrains-gradle buildPlugin

# Check the JetBrains plugin against installed IDEs (install dirs), or the build platform without any
[positional-arguments]
verify-jetbrains *ides: jetbrains-stage
    #!/usr/bin/env bash
    set -euo pipefail
    ides="$(IFS=:; echo "$*")"
    just jetbrains-gradle verifyPlugin ${ides:+"-PverifyIdes=$ides"}

# Start a sandbox IDE with the JetBrains plugin
run-jetbrains: jetbrains-stage
    just jetbrains-gradle runIde

# Test the JetBrains plugin against the real server, then install.sh (needs JDK 21, not part of ci)
test-jetbrains: jetbrains-stage
    just jetbrains-gradle test \
        -PtestBinary="$(cargo metadata --no-deps --format-version 1 | jq -r '.target_directory')/release/asciidoc-lsp"
    shellcheck editors/intellij/install.sh editors/intellij/tests/install_test.sh
    editors/intellij/tests/install_test.sh

# Install LSP4IJ and the latest released plugin into installed JetBrains IDEs (IDEs closed)
[positional-arguments]
install-jetbrains *ides:
    editors/intellij/install.sh "$@"

# Build the plugin from this checkout and install it into installed JetBrains IDEs (IDEs closed)
[positional-arguments]
install-jetbrains-dev *ides:
    editors/intellij/install.sh --check "$@"
    just build-jetbrains
    editors/intellij/install.sh --dev \
        "{{justfile_directory()}}/editors/intellij/build/distributions/asciidoc-lsp-$(just jetbrains-dev-version).zip" "$@"
