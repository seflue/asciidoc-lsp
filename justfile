# asciidoc-lsp task runner

default:
    @just --list

# Build debug binary
build:
    cargo build

# Build optimized release binary
release:
    cargo build --release

# Run all tests
test:
    cargo test

# Run clippy lints
clippy:
    cargo clippy -- -D warnings

# Type-check without building
check:
    cargo check

# Format code
fmt:
    cargo fmt

# Auto-fix: format + apply clippy suggestions
fix:
    cargo fmt
    cargo clippy --fix --allow-dirty -- -D warnings

# Check all: formatting, lints, tests (CI/pre-commit)
lint:
    cargo fmt -- --check
    cargo clippy -- -D warnings
    cargo test

# Run against a file (e.g.: just run docs/index.adoc)
run *ARGS:
    cargo run -- {{ARGS}}

# Show include tree for docs/index.adoc
tree:
    cargo run -- --tree docs/index.adoc

# Run all quality checks + build release
all:
    cargo fmt -- --check
    cargo clippy -- -D warnings
    cargo test
    cargo build --release

# Remove build artifacts
clean:
    cargo clean

# Open arc42 architecture docs
arc42:
    @echo "docs/index.adoc"
    @echo "Chapters:"
    @ls docs/chapters/
