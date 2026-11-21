# asciidoc-lsp

Language server for AsciiDoc, built on tree-sitter. The LSP server is the product; the other crates serve it.

## Crates

- `crates/asciidoc-core`: parsing, extraction, workspace model, diagnostics. Editor-agnostic library; all AsciiDoc knowledge lives here.
- `crates/asciidoc-lsp`: the LSP server (`tower-lsp-server`). Translates core results into LSP types.
- `crates/asciidoc-cli`: inspection and debugging CLI on top of core.

## Commands

Run every build, test, lint and format step through a `just` target (`justfile` at the repo root; `just --list` shows them all). Key targets: `build`, `test`, `lint`, `fmt`, `ci` (fmt-check + lint + test, same as CI), `test-js` (Node tests for the preview page's `scroll-map.js` in `crates/asciidoc-lsp/tests/js/`; needs Node, not part of `ci`, CI runs it as its own job), `run <file>` (CLI), `lsp`.

- Call `just`, never `cargo` directly. When no target covers what you need (for example, `just test` takes no filter argument), stop and tell the user what you need instead of working around it.
- `just lint` runs clippy with workspace-wide `pedantic` lints and `-D warnings`: every pedantic warning fails the build.
- Run `just ci` before declaring a change done.
- `just arch-check` (part of `ci`) needs cargo-arc 0.6.2 installed. Regenerate `arc-baseline.toml` only with `just arch-baseline`, and only from a green state.

## Grammar: two trees per document

The grammar is the git submodule `vendor/tree-sitter-asciidoc` (a fork of cathaysia/tree-sitter-asciidoc, pinned to a commit). The root `Cargo.toml` patches both grammar crates to the submodule via `[patch.crates-io]`, so a fresh clone needs `git submodule update --init` before it builds. `just update-grammar` moves the pin to the fork's latest commit; run it only when asked.

The grammar ships as two languages, and every document is parsed twice:

1. **Block** grammar (`tree-sitter-asciidoc`): sections, attributes, block macros such as `include::`.
2. **Inline** grammar (`tree-sitter-asciidoc-inline`): xrefs, anchors and other inline markup. Paragraph text is opaque in the block tree.

`collect_inline_ranges()` gathers the text ranges from the block tree; the inline parser then parses each range on its own via `set_included_ranges`. Parsing all ranges in one call lets markup in one paragraph pair with markup in another and swallow nodes. Before writing a query or extractor, check which tree holds the node and what the grammar actually names it.

## Test data

- `docs/`: the project's arc42 documentation, used as a real-world corpus by integration tests. Changing it can break tests.
- `tests/fixtures/`: small targeted cases, one directory per edge case (broken xrefs, circular includes, ...).
- `tests/*.adoc` and `tests/manual-test-plan.adoc`: data for manual testing in an editor.

## Code style

- Production code handles every `Option`/`Result` with `?`, `let ... else` or an explicit match. `unwrap()` belongs in tests only.
- `asciidoc-core` reports errors as typed `thiserror` enums (`CoreError` in `error.rs`); the binaries may wrap them with `anyhow`.
- Unit tests live in a `#[cfg(test)] mod tests` at the bottom of each module.

## Commits

Commit messages carry no `Co-Authored-By` trailer and no other AI attribution.
