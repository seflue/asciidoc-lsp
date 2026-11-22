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
  var PRODUCT = 'AsciiDoc Preview'
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
   * anchors stop where an included file's lines fall back (below the last
   * line, not equal to it: a list and its first item share a line), which is
   * where its next occurrence starts. (A main file has one occurrence.) Pixel
   * space of other files' blocks is not lost: it lies between the file's own
   * neighbours, and the interpolation spans it.
   */
  function fileAnchors(entries, src) {
    return ownEntries(entries, src).map(function (own) {
      return { line: own.line, top: own.top }
    })
  }

  /** The entries `fileAnchors` keeps, with their index in `entries`. */
  function ownEntries(entries, src) {
    var wanted = src === undefined ? null : src
    var result = []
    var last = 0
    for (var i = 0; i < entries.length; i++) {
      var entry = entries[i]
      var from = entry.src === undefined ? null : entry.src
      if (from !== wanted) continue
      if (entry.line < last && wanted !== null && result.length) break
      if (entry.line <= last) continue
      result.push({ line: entry.line, top: entry.top, index: i })
      last = entry.line
    }
    return result
  }

  /**
   * Anchors of one file like `fileAnchors`, plus two per include directive of
   * that file, so a cursor on the directive lands on the included content
   * instead of somewhere interpolated between the file's own blocks. The
   * included blocks carry the other file's lines and are no anchors of this
   * one. `includes` is the page's include table, `{parent, line, file}`
   * entries (`parent` null for the main file, as `src`); entries need
   * `bottom` besides `top`.
   *
   * For the directive on line L: an anchor at L at the top of the first
   * element of the included content, and an anchor at L + 1 at the bottom of
   * its last element, unless the file has an anchor there. The content
   * includes what the included file includes itself. An include that rendered
   * nothing adds nothing.
   */
  function fileAnchorsWithIncludes(entries, includes, src) {
    var wanted = src === undefined ? null : src
    var own = ownEntries(entries, wanted)
    var anchors = own.map(function (entry) {
      return { line: entry.line, top: entry.top }
    })
    var mine = (includes || [])
      .filter(function (inc) { return inc.parent === wanted })
      .sort(function (a, b) { return a.line - b.line })
    if (!mine.length) return anchors

    var starts = []
    var ends = []
    var resume = 0
    for (var i = 0; i < mine.length; i++) {
      var inc = mine[i]
      var from = resume
      for (var j = 0; j < own.length; j++) {
        if (own[j].line < inc.line) from = Math.max(from, own[j].index + 1)
      }
      var extent = includeExtent(entries, includes, inc, from)
      if (!extent) continue
      starts.push({ line: inc.line, top: extent.top })
      ends.push({ line: inc.line + 1, top: extent.bottom })
      resume = extent.end
    }

    var taken = {}
    var merged = []
    function add(anchor) {
      if (taken[anchor.line]) return
      taken[anchor.line] = true
      merged.push({ line: anchor.line, top: anchor.top, include: anchor.include })
    }
    starts.forEach(function (a) { add({ line: a.line, top: a.top, include: true }) })
    anchors.forEach(add)
    ends.forEach(function (a) { add({ line: a.line, top: a.top, include: true }) })
    merged.sort(function (a, b) { return a.line - b.line })

    // Guard: an include anchor that would break the order in `top` against
    // its neighbours (the file's own anchors, or the include anchors kept so
    // far) is dropped, so the result ascends in line and top alike.
    var nextOwnTop = new Array(merged.length)
    var ahead = Infinity
    for (var k = merged.length - 1; k >= 0; k--) {
      nextOwnTop[k] = ahead
      if (!merged[k].include) ahead = merged[k].top
    }
    var result = []
    var floor = -Infinity
    merged.forEach(function (anchor, index) {
      if (anchor.include && (anchor.top < floor || anchor.top > nextOwnTop[index])) return
      floor = Math.max(floor, anchor.top)
      result.push({ line: anchor.line, top: anchor.top })
    })
    return result
  }

  /**
   * The files whose content belongs to one include of `file`, as the number
   * of times each may occur in it: `file` once, plus one occurrence per
   * distinct include directive found in it, and so on down. Global
   * membership would let one include swallow a later direct include of a
   * file that it only includes itself at another place.
   */
  function nestedBudget(includes, file) {
    var budget = {}
    budget[file] = 1
    function add(parent, depth) {
      var seen = {}
      includes.forEach(function (child) {
        var key = child.line + ':' + child.file
        if (child.parent !== parent || seen[key] || depth > 16) return
        seen[key] = true
        budget[child.file] = (budget[child.file] || 0) + 1
        add(child.file, depth + 1)
      })
    }
    add(file, 0)
    return budget
  }

  /** Whether `file` is included by `ancestor`, directly or through others. */
  function includesFile(includes, ancestor, file, depth) {
    return depth <= 16 && includes.some(function (inc) {
      return inc.parent === ancestor &&
        (inc.file === file || includesFile(includes, inc.file, file, depth + 1))
    })
  }

  /**
   * The rendered content of include `inc`, looking at `entries` from index
   * `from`: top of its first element, bottom of its last, and the index after
   * it. `null` if nothing of it rendered. Content is every element of
   * `inc.file` or of a file it includes (see `nestedBudget`), up to the first
   * element of another file or a line of a file that falls back (its next
   * occurrence). The search stops at the first own block of the parent after
   * the directive: content comes before it, so a file that rendered nowhere
   * at `inc` (inside a listing) does not take a later occurrence. The bottom
   * is capped at the top of the next entry, because a first element that is
   * a section container encloses what follows it.
   */
  function includeExtent(entries, includes, inc, from) {
    var parent = inc.parent
    var limit = entries.length
    for (var p = from; p < entries.length; p++) {
      var own = entries[p].src === undefined ? null : entries[p].src
      if (own === parent && entries[p].line > inc.line) {
        limit = p
        break
      }
    }
    var budget = nestedBudget(includes, inc.file)
    var occurrences = {}
    var lastLine = {}
    var previous = -1
    var first = -1
    var last = -1
    for (var i = from; i < limit; i++) {
      var entry = entries[i]
      var inside = entry.src !== undefined && budget[entry.src] > 0
      if (first < 0) {
        if (inside) first = i
        else continue
      } else if (!inside) {
        break
      }
      var seen = occurrences[entry.src] || 0
      var resumed = entry.src === previous || includesFile(includes, entry.src, previous, 0)
      if (seen === 0 || entry.line < lastLine[entry.src] || !resumed) {
        if (seen >= budget[entry.src]) break
        occurrences[entry.src] = seen + 1
      }
      lastLine[entry.src] = entry.line
      previous = entry.src
      last = i
    }
    if (first < 0) return null
    var top = entries[first].top
    var bottom = top
    for (var n = first; n <= last; n++) {
      if (entries[n].bottom > bottom) bottom = entries[n].bottom
    }
    if (last + 1 < entries.length) bottom = Math.max(top, Math.min(bottom, entries[last + 1].top))
    return { top: top, bottom: bottom, end: last + 1 }
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
   * The body class attribute for a page: `base` plus the classes in `text`,
   * the JSON text of `<script id="adoc-body-classes">`. Entries that are not
   * a single class token are dropped; anything else than an array adds none.
   */
  function bodyClassName(base, text) {
    var list
    try {
      list = JSON.parse(text)
    } catch (error) {
      return base
    }
    if (!Array.isArray(list)) return base
    var classes = [base]
    for (var i = 0; i < list.length; i++) {
      if (typeof list[i] === 'string' && /^[^\s]+$/.test(list[i])) classes.push(list[i])
    }
    return classes.join(' ')
  }

  /**
   * The tab title for a page: `text` is the JSON text of
   * `<script id="adoc-page-title">`, a string, the plain-text document title.
   */
  function pageTitle(text) {
    var title
    try {
      title = JSON.parse(text)
    } catch (error) {
      return PRODUCT
    }
    return typeof title === 'string' && title !== '' ? title + ' \u2013 ' + PRODUCT : PRODUCT
  }

  /**
   * The include table of a page: the JSON text of
   * `<script id="adoc-source-includes">`, an array of `{parent, line, file}`
   * (`parent` null for the main file, else a file table index). Empty for
   * anything that is not such an array.
   */
  function parseIncludeTable(text) {
    var table
    try {
      table = JSON.parse(text)
    } catch (error) {
      return []
    }
    if (!Array.isArray(table)) return []
    return table.filter(function (inc) {
      return inc !== null && typeof inc === 'object' &&
        (inc.parent === null || Number.isInteger(inc.parent)) &&
        Number.isInteger(inc.line) && Number.isInteger(inc.file)
    })
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
   * Anchor for the top of the page: the header (title, inline toc) has no
   * anchor, so lines above the first block would map to that block and
   * scroll the title away. Line 1 sits at the page top, `scrollY` above the
   * current viewport; the lines in between interpolate. Anchors that start
   * at line 1 already cover the top.
   */
  function withPageTop(anchors, scrollY) {
    if (!anchors.length || anchors[0].line <= 1) return anchors
    return [{ line: 1, top: -scrollY }].concat(anchors)
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
    fileAnchorsWithIncludes: fileAnchorsWithIncludes,
    parseIncludeTable: parseIncludeTable,
    parseFileTable: parseFileTable,
    bodyClassName: bodyClassName,
    pageTitle: pageTitle,
    srcForFile: srcForFile,
    withSentinel: withSentinel,
    withPageTop: withPageTop,
    anchorIndicesForLine: anchorIndicesForLine,
    scrollTopForLine: scrollTopForLine,
    sourceLineAtViewportTop: sourceLineAtViewportTop,
    shouldScroll: shouldScroll,
  }
})
