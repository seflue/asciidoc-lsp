# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Before 1.0, a minor version may contain breaking changes.

<!-- next-header -->

## [Unreleased]

### Added

- VS Code extension: each release attaches VSIX packages that start the
  server, with the matching binary bundled for Linux, Alpine, macOS and
  Windows plus a universal package without one.
- The server reports its name and version in the `initialize` result, so
  clients can show which build is running.

## [0.7.0] - 2026-11-15

### Added

- Hover on an attribute shows its resolved value, including paths set in
  `.asciidoctorconfig`. When several documents include a file, hover and go
  to definition show the value each of them gives it.
- Hover on a line in a nested `.asciidoctorconfig` shows the parent entry it
  overrides.
- Hover, go to definition and diagnostics for inline images, and for xrefs
  and images inside table cells.
- Checks for `pdf-theme`, `pdf-themesdir` and `pdf-fontsdir`: files that
  Asciidoctor PDF cannot load are reported, and hover shows the theme file it
  will use.
- `asciidoc-lsp --version`, and a debug command that shows how the reference
  under the cursor resolves.

### Changed

- The `.asciidoctorconfig` lookup stops at the workspace root and no longer
  reads a config from the home directory.
- The sentence-per-line action handles more paragraphs.

### Fixed

- Attributes, `imagesdir` and `{docdir}` resolve as in Asciidoctor across
  includes, subfolders and symlinks. Shared files included from documents in
  subfolders no longer get false "not found" errors.
- `.asciidoctorconfig` is read as Asciidoctor reads it: relative paths,
  `.asciidoctorconfig.adoc`, unset entries, line continuations and includes.
  Changes to a config on disk apply right away.
- Path completion after `xref:`, `<<`, `include::` and `image:` works behind
  folder prefixes such as `chapters/`, and typing `/` triggers it.
- An unfinished macro such as `image::foo.png` without its `[]`, or a table
  cell spanning several lines, no longer breaks go to definition and hover
  for the rest of the document.

## [0.6.0] - 2026-10-31

### Added

- A code action that rewrites a paragraph to one sentence per line. It is
  offered only where the split is safe: sentences stay whole inside inline
  markup and image alt text; abbreviations (`e.g.`, `i.e.`), initials and
  dates do not end a sentence; closing quotes stay on their sentence's line;
  no line starts with block syntax; it declines when joining lines would
  create block syntax or a list item; bold or italic label lines stay on
  their own.

## [0.5.0] - 2026-05-09

### Added

- Cross-file xrefs with a path and an anchor (`xref:other.adoc#anchor[]`,
  `<<other.adoc#anchor>>`), resolved relative to the source file.
- Description lists.
- Anchors with reftext and anchors inside lists.
- The workspace folders the client sends are honoured.
- Files changed outside the editor are picked up.

## [0.4.0] - 2026-03-07

### Added

- `.asciidoctorconfig` is read per file, from the file's folder up to the
  workspace root; `.asciidoctorconfig.adoc` is recognised as well.
- Image path completion.

### Changed

- Every file is analysed on its own; the server no longer guesses one entry
  document for the workspace.
- `env-*` attributes are not reported as undefined.
- Attributes in `ifdef`/`ifndef` are not validated, since they depend on the
  build.

### Fixed

- Completions work on lines with multi-byte characters.
- A page break (`<<<`) is no longer reported as a broken xref.

## [0.3.0] - 2026-03-05

### Added

- Hover for attributes and references.
- Rename for anchors and attributes.
- Diagnostics for circular includes and undefined attributes.

## [0.2.0] - 2026-03-03

### Added

- Go to definition for attributes and images.
- Find references for attributes, includes and anchors.
- Completions for attributes, include paths, xref targets and anchors, and
  for block macros, admonitions, document attributes and inline macros.
- Includes and attributes in document and workspace symbols.
- Diagnostics for every file in the workspace, not only open ones.
- The `{asciidoctorconfigdir}` attribute.

### Fixed

- Xref navigation across documents.

## [0.1.0] - 2026-02-26

### Added

- Language server over stdio with go to definition for includes, xrefs and
  anchors, document symbols, and diagnostics for broken includes and xrefs.

<!-- next-url -->
[Unreleased]: https://github.com/seflue/asciidoc-lsp/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/seflue/asciidoc-lsp/commits/v0.1.0
