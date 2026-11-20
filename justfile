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

# All quality checks (CI / pre-commit)
ci: fmt-check lint test test-release check-version

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
