/*
 * Line <-> pixel mapping for the preview's scroll sync. Pure functions: numbers
 * in, numbers out, no DOM access, so they can be checked without a browser.
 *
 * Ported from asciidoctor-vscode (MIT License, Copyright (c) the
 * asciidoctor-vscode contributors):
 *   src/features/preview/scrollMapping.ts   anchorIndicesForLine,
 *                                            scrollTopForLine,
 *                                            sourceLineAtViewportTop
 *   preview-src/scroll-sync.ts              anchor selector, class parsing,
 *                                            end-of-document sentinel
 * https://github.com/asciidoctor/asciidoctor-vscode
 *
 * Convention (the one that causes off-by-one scroll jumps): every line
 * handled here is a 1-based SOURCE line, the number in a `data-line-N` class.
 * The server converts the editor's 0-based line once, before it reaches the
 * page. Do not add or subtract 1 in this file.
 *
 * Differences from the vscode port: asciidoctor-vscode previews one file at a
 * time. Here a page can show a whole document, and the cursor can be in the
 * main file or in an included one. Blocks from an included file carry
 * `data-src-K` besides `data-line-N`, and their N counts lines of that other
 * file, so lines are only comparable within one file: `fileAnchors` picks the
 * anchors of the file the cursor is in (the main file, or file K of the
 * page's file table), and `srcForFile` finds K.
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) {
    module.exports = factory()
  } else {
    root.AdocScrollMap = factory()
  }
})(typeof self !== 'undefined' ? self : this, function () {
  'use strict'

  /** CSS selector of the elements that carry a `data-line-N` class. */
  var ANCHOR_SELECTOR = '[class^="data-line-"], [class*=" data-line-"]'

  /**
   * Parse a class attribute: `{line}` for a main-file anchor, `{line, src}`
   * for a block of an included file, `null` without a `data-line-N` class.
   */
  function parseAnchorClass(className) {
    var line = null
    var src = null
    var classes = String(className || '').split(/\s+/)
    for (var i = 0; i < classes.length; i++) {
      var lineMatch = /^data-line-(\d+)$/.exec(classes[i])
      if (lineMatch) line = parseInt(lineMatch[1], 10)
      var srcMatch = /^data-src-(\d+)$/.exec(classes[i])
      if (srcMatch) src = parseInt(srcMatch[1], 10)
    }
    if (line === null) return null
    return src === null ? { line: line } : { line: line, src: src }
  }

  /**
   * Anchors usable for the mapping, from `{line, src?, top}` entries in
   * document order, for one file: `src` is `null` for the main file or the
   * index K of an included file (`data-src-K`). Lines of another file are
   * dropped, as are entries whose line does not rise, so the sequence stays
   * strictly ascending and interpolation and binary search hold.
   *
   * A file included more than once counts with its first occurrence: the
   * anchors stop where an included file's lines fall back, which is where
   * its next occurrence starts. (A main file has one occurrence.) Pixel
   * space of other files' blocks is not lost: it lies between the file's own
   * neighbours, and the interpolation spans it.
   */
  function fileAnchors(entries, src) {
    var wanted = src === undefined ? null : src
    var result = []
    var last = 0
    for (var i = 0; i < entries.length; i++) {
      var entry = entries[i]
      var from = entry.src === undefined ? null : entry.src
      if (from !== wanted) continue
      if (entry.line <= last) {
        if (wanted !== null && result.length) break
        continue
      }
      result.push({ line: entry.line, top: entry.top })
      last = entry.line
    }
    return result
  }

  /** Anchors of the main file; see `fileAnchors`. */
  function mainFileAnchors(entries) {
    return fileAnchors(entries, null)
  }

  /**
   * The file table of a page: the JSON text of `<script id="adoc-source-files">`,
   * an array of absolute paths where the index is K of `data-src-K`. Empty
   * for anything that is not such an array.
   */
  function parseFileTable(text) {
    var table
    try {
      table = JSON.parse(text)
    } catch (error) {
      return []
    }
    return Array.isArray(table) ? table : []
  }

  /**
   * Which anchors `file` owns on a page whose converted document is
   * `shownFile` with the file table `table`: `null` for the main file, the
   * index K for an included one (the first, if the table lists it twice),
   * `undefined` if the page does not show `file` at all.
   */
  function srcForFile(table, shownFile, file) {
    if (file === shownFile) return null
    var index = table.indexOf(file)
    return index < 0 ? undefined : index
  }

  /**
   * Add the end-of-document sentinel: an anchor at the bottom of the content
   * for the last line of the source, so there is something to interpolate
   * towards after the final block (asciidoctor-vscode appends an empty
   * `data-line-<lineCount>` element for this). Its line stays above the last
   * anchor's line so the sequence remains strictly ascending.
   */
  function withSentinel(anchors, lineCount, bottom) {
    if (!anchors.length || !(lineCount > 0)) return anchors
    var lastLine = anchors[anchors.length - 1].line
    var line = Math.max(lineCount, lastLine + 1)
    return anchors.concat([{ line: line, top: bottom }])
  }

  /**
   * Indices into the line-ascending `lines` array of the anchors around
   * `targetLine` (a 1-based source line). `previous` is the last anchor at or
   * before the target (0 if the target lies above the first); `next` is the
   * following anchor when the target falls strictly between two, else -1.
   */
  function anchorIndicesForLine(lines, targetLine) {
    var lineNumber = Math.floor(targetLine)
    var previous = 0
    for (var i = 0; i < lines.length; i++) {
      if (lines[i] === lineNumber) return { previous: i, next: -1 }
      if (lines[i] > lineNumber) return { previous: previous, next: i }
      previous = i
    }
    return { previous: previous, next: -1 }
  }

  /**
   * Page scroll offset that brings `line` (1-based source line) to the top of
   * the page, or `undefined` without anchors. `anchors` hold client-Y tops;
   * `scrollY` is the current page scroll.
   */
  function scrollTopForLine(anchors, scrollY, line) {
    if (!anchors.length) return undefined
    var idx = anchorIndicesForLine(
      anchors.map(function (a) { return a.line }),
      line
    )
    var previousAnchor = anchors[idx.previous]
    if (!previousAnchor) return undefined
    var nextAnchor = idx.next >= 0 ? anchors[idx.next] : undefined
    if (nextAnchor && nextAnchor.line !== previousAnchor.line) {
      // Between two anchors: interpolate by the progress through the source
      // lines that separate them.
      var progress =
        (line - previousAnchor.line) / (nextAnchor.line - previousAnchor.line)
      return scrollY + previousAnchor.top + progress * (nextAnchor.top - previousAnchor.top)
    }
    return scrollY + previousAnchor.top
  }

  /**
   * Fractional 1-based source line at client-Y `viewportTop`, or `null`
   * without anchors; interpolated between the two anchors around it.
   */
  function sourceLineAtViewportTop(anchors, viewportTop) {
    if (!anchors.length) return null
    if (anchors[0].top > viewportTop) return anchors[0].line
    var lo = 0
    var hi = anchors.length - 1
    while (lo < hi) {
      var mid = Math.ceil((lo + hi) / 2)
      if (anchors[mid].top <= viewportTop) lo = mid
      else hi = mid - 1
    }
    var previous = anchors[lo]
    var next = anchors[lo + 1]
    if (next) {
      var span = next.top - previous.top
      var progress = span > 0 ? (viewportTop - previous.top) / span : 0
      return previous.line + progress * (next.line - previous.line)
    }
    return previous.line
  }

  /**
   * Whether to move for a target at client-Y `clientY` in a viewport of
   * `viewportHeight`: not when the line is already comfortably on screen.
   * Lines in the top quarter still snap to the top, so lockstep scrolling
   * does not stall (asciidoctor-vscode #638).
   */
  function shouldScroll(clientY, viewportHeight) {
    var snapZone = viewportHeight / 4
    return !(clientY > snapZone && clientY < viewportHeight)
  }

  return {
    ANCHOR_SELECTOR: ANCHOR_SELECTOR,
    parseAnchorClass: parseAnchorClass,
    fileAnchors: fileAnchors,
    mainFileAnchors: mainFileAnchors,
    parseFileTable: parseFileTable,
    srcForFile: srcForFile,
    withSentinel: withSentinel,
    anchorIndicesForLine: anchorIndicesForLine,
    scrollTopForLine: scrollTopForLine,
    sourceLineAtViewportTop: sourceLineAtViewportTop,
    shouldScroll: shouldScroll,
  }
})
