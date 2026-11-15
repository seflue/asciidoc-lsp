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

# All quality checks (CI / pre-commit)
ci: fmt-check lint test

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
