# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Before 1.0, a minor version may contain breaking changes.

<!-- next-header -->

## [Unreleased]

## [0.10.0] - 2026-11-23

## [0.10.0] - 2026-11-23

### Added

- Live preview with two-way scroll sync between editor and page.
- VS Code: live preview in a panel or the browser.
- Warnings for missing images, per top-level document that includes the
  file. Hover on such an image says the same.
- Missing files referenced in a PDF theme (`extends`, images, fonts) are
  reported at the `pdf-theme` line.
- Find references on a section heading lists the xrefs to it and the
  includes that bring its file in. Natural xrefs (`<<Section title>>`) are
  listed as references.
- The sentence-per-line action works in quote and open blocks.

### Changed

- The server answers `initialize` before it scans the workspace, so the
  editor attaches at once.
- While typing, diagnostics are recomputed after a short pause instead of on
  every keystroke.
- The server exits when the editor process dies.
- PDF settings are checked only where Asciidoctor PDF reads them: the last
  entry in the document header.
- A broken include names the documents that miss its target. Quick-fixes
  offer only targets that resolve in every one of them.
- Local builds print their version as `X.Y.Z+<commits>.<hash>`.

### Fixed

- An include whose target uses an attribute that references another one
  (`:partsdir: {docdir}/../parts`, `include::{partsdir}/part.adoc[]`)
  resolves. The included file was not linked to its document: the preview
  showed it on its own, and its diagnostics left out the document's build.
- Section IDs use the `idprefix` and `idseparator` set by the including
  document, so xrefs to them resolve.
- Find references matches file xrefs by the path Asciidoctor resolves, not
  by file name.
- No image warnings where Asciidoctor renders no image: listing, literal and
  verse content, `a|` cells and conditional branches. Any URI scheme counts
  as remote.
- PDF paths starting with `~`, and PDF paths in included files that use the
  including document's attributes, are no longer reported.
- `tags="a,b"` reports a missing tag, and an empty attribute value such as
  `tag=` no longer breaks the attributes after it.
- An `endif` for another target closes nothing, and `ifdef` in
  `.asciidoctorconfig` matches attribute names in any case, as in
  Asciidoctor.
- A leading byte order mark no longer hides the document title or a config
  entry.
- Highlighting: listings, source styles, every paragraph of an admonition
  block, and inline markup in quote blocks.

## [0.9.0] - 2026-11-19

### Added

- Error for a level 0 section in a document that is not a book.
- Hover on an anchor declaration shows the same as hover on an xref to it.

### Changed

- Xref paths in included files resolve from the top-level document, as in
  Asciidoctor.
- Go to definition and completion offer only targets the document actually
  finds.
- A missing include marked `opts=optional` is reported as information, not
  as an error.

### Fixed

- Xrefs to a section title or reftext (`<<Section title>>`), including block
  titles, resolve instead of being reported as broken.
- Bibliography anchors (`[[[id]]]`) are valid xref targets.
- Xrefs to a file that exists only elsewhere in the workspace are reported
  as broken.
- Rename refuses names that would break the document.
- Highlighting: prose words, literal blocks, author lines, autolinks in
  angle brackets and titles starting with an attribute reference.
- An attribute entry directly below a paragraph line is treated as text.
- Errors for section levels are reported where Asciidoctor reports them.

## [0.8.0] - 2026-11-17

### Added

- Syntax highlighting for AsciiDoc markup via semantic tokens.
- VS Code extension, attached to each release with the server bundled.
- Quick-fixes for broken xrefs and includes.
- Diagnostics for undefined attributes, section levels out of sequence,
  missing include tags and unclosed delimited blocks.
- Completion for block styles, source languages, attribute values and
  attribute names in `ifdef`/`ifndef`.
- `anchor:id[]` and `[#id]` anchors as xref targets.

### Changed

- Xrefs and duplicate IDs are checked per top-level document, as in
  Asciidoctor.

### Fixed

- Accepting a completion no longer leaves part of the typed word behind.
- Xrefs inside admonitions, quotes and description lists are found.
- Fewer false warnings, for example on URI includes and in `a`/`l` table
  columns.
- Document headers, lists and the end of a file are parsed as in
  Asciidoctor.

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
[Unreleased]: https://github.com/seflue/asciidoc-lsp/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.10.0...v0.10.0
[0.10.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/seflue/asciidoc-lsp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/seflue/asciidoc-lsp/commits/v0.1.0
