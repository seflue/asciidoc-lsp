/*
 * Preview → editor: when the page reports a double-click or a scroll, and
 * which file the content on its reading line comes from. Pure functions and a small
 * state machine with the time passed in, so they can be checked without a
 * browser; page.js does the DOM side.
 *
 * Scroll sync runs both ways, so without care the two sides push each other
 * back and forth (rubberbanding). The scroll gate holds the page's share of
 * the guards:
 *   1. Only the reader's input is sent: a scroll counts if a wheel, key or
 *      touch input came shortly before, while a pointer is held (dragging
 *      the scrollbar), or while the page has the keyboard focus (a browser
 *      extension that scrolls by script swallows the key event). A scroll
 *      the page makes itself is never sent.
 *   2. For `echoWindow` ms after a cursor event from the editor nothing is
 *      sent, and for `echoWindow` ms after a send the page follows no
 *      cursor event. A reader's input in that window is not lost: it goes
 *      out, or is followed, when the window ends.
 *   3. Sends are throttled to one per `throttle` ms; the last position of a
 *      window follows when it ends.
 * The editor's share (no echo of a move it was told to make, nothing to do
 * when the line is already visible) lives in the editor plugin.
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) {
    module.exports = factory()
  } else {
    root.AdocPageSync = factory()
  }
})(typeof self !== 'undefined' ? self : this, function () {
  'use strict'

  /**
   * A gate for the page's scroll reports. Times are milliseconds from any
   * monotonic clock. `onScroll` answers `send`, `later` (a trailing send is
   * due after `delay` ms, then call `onTimer`) or `drop`.
   */
  function createScrollGate(options) {
    var inputWindow = options.inputWindow
    var echoWindow = options.echoWindow
    var throttle = options.throttle
    var NEVER = -Infinity
    var lastInput = NEVER
    var pointerHeld = false
    var lastOwn = NEVER // the page's own scroll
    var lastCursor = NEVER // a cursor event from the editor
    var lastSent = NEVER
    var pending = false
    var leads = false // the page sent last; the editor has not moved since

    function quiet(now) {
      return now - lastOwn >= echoWindow && now - lastCursor >= echoWindow
    }

    function fromInput(now) {
      return pointerHeld || now - lastInput <= inputWindow
    }

    function fromReader(now, focused) {
      return focused === true || fromInput(now)
    }

    return {
      userInput: function (now) {
        lastInput = now
      },
      pointerDown: function (now) {
        pointerHeld = true
        lastInput = now
      },
      pointerUp: function (now) {
        pointerHeld = false
        lastInput = now
      },
      /** The page is about to scroll by itself. */
      ownScroll: function (now) {
        lastOwn = now
        pending = false
      },
      /** A cursor event from the editor arrived. */
      cursorEvent: function (now) {
        lastCursor = now
        leads = false
        pending = false
      },
      /** `focused`: whether the page has the keyboard focus. */
      onScroll: function (now, focused) {
        if (!quiet(now)) {
          // The page's own scroll never goes out. A reader's input after a
          // cursor event, or after the page's own scroll, is caught up when
          // the window ends; only an input event counts here, as a focused
          // page's own scroll has none.
          var ownScrolling = now - lastOwn < echoWindow && lastInput <= lastOwn
          if (ownScrolling || !fromInput(now)) return 'drop'
          pending = true
          return 'later'
        }
        if (!fromReader(now, focused)) return 'drop'
        if (now - lastSent >= throttle) {
          lastSent = now
          pending = false
          return 'send'
        }
        pending = true
        return 'later'
      },
      /** Milliseconds until the trailing send is due. */
      delay: function (now) {
        return Math.max(
          0,
          throttle - (now - lastSent),
          echoWindow - (now - lastCursor),
          echoWindow - (now - lastOwn)
        )
      },
      onTimer: function (now) {
        if (!pending || !quiet(now)) {
          pending = false
          return 'drop'
        }
        pending = false
        lastSent = now
        return 'send'
      },
      /** Whether a cursor event may scroll the page now. */
      followsCursor: function (now) {
        return now - lastSent >= echoWindow
      },
      /** The page posted a scroll report: from now on it leads. */
      reported: function () {
        leads = true
      },
      /**
       * Whether the page leads: it posted a report last and the editor
       * reported no cursor since. The editor reports no move it was made to do, so the
       * last cursor the page knows is stale then.
       */
      pageLeads: function () {
        return leads
      },
      /** Milliseconds until the page follows cursor events again. */
      followDelay: function (now) {
        return Math.max(0, echoWindow - (now - lastSent))
      },
    }
  }

  /**
   * The file of source index `src`: the shown file for `null`, else entry
   * `src` of the file table; `null` if the table has no such entry.
   */
  function fileOfSrc(table, shownFile, src) {
    if (src === null || src === undefined) return shownFile
    return typeof table[src] === 'string' ? table[src] : null
  }

  /**
   * The anchor at `viewportTop`, from `{line, src?, top}` entries in
   * document order: the last one that starts at or above it, so of nested
   * blocks the innermost. Above every anchor lies the document header
   * (title, attributes), which has none: `{line: 1}` of the shown file.
   */
  function topAnchor(entries, viewportTop) {
    var found = { line: 1 }
    for (var i = 0; i < entries.length; i++) {
      if (entries[i].top <= viewportTop) found = entries[i]
    }
    return found
  }

  /**
   * The files `src` includes, directly or through others, and `src` itself,
   * as a set (object keys) of source indices.
   */
  function includedFiles(includes, src) {
    var members = {}
    members[src] = true
    for (var grew = true, depth = 0; grew && depth <= 16; depth++) {
      grew = false
      ;(includes || []).forEach(function (inc) {
        if (members[inc.parent] && !members[inc.file]) {
          members[inc.file] = true
          grew = true
        }
      })
    }
    return members
  }

  /**
   * The line of the include directive in file `src` that content of file
   * `from` comes through, directly or nested; `null` if none does, or if
   * several do (a file included twice: the position decides, not the file).
   */
  function includeLineFor(includes, src, from) {
    var lines = {}
    ;(includes || []).forEach(function (inc) {
      if (inc.parent === src && includedFiles(includes, inc.file)[from]) lines[inc.line] = true
    })
    var found = Object.keys(lines)
    return found.length === 1 ? Number(found[0]) : null
  }

  /**
   * `{file, line}` for a double-click on an element whose nearest anchor is
   * `anchor` (`{line, src?}`, as scroll-map.js's `parseAnchorClass` reads
   * it), or `null` without an anchor or for a file the page does not show.
   */
  function clickTarget(anchor, table, shownFile) {
    if (anchor === null) return null
    var file = fileOfSrc(table, shownFile, anchor.src)
    return file === null ? null : { file: file, line: anchor.line }
  }

  /**
   * The file to name in the banner: the file of the `top` anchor when it is
   * not `activeFile`, else `null`.
   */
  function bannerFile(top, table, shownFile, activeFile) {
    if (top === null) return null
    var file = fileOfSrc(table, shownFile, top.src)
    return file !== null && file !== activeFile ? file : null
  }

  /**
   * Whether a page scrolled to `scrollY` with a viewport of `viewportHeight`
   * shows its end (`scrollHeight` high), allowing two pixels of rounding. A
   * page that does not scroll has none: its top is all there is to report.
   */
  function atEnd(scrollY, viewportHeight, scrollHeight) {
    return scrollHeight > viewportHeight && scrollY + viewportHeight >= scrollHeight - 2
  }

  /** The reading line, as a share of the viewport height from its top. */
  var READING_LINE = 0.25

  /**
   * Where a page scrolled to `scrollY` reads its position, as client-Y `y`:
   * on the reading line; at the top of the page its top, the document start;
   * at its end its bottom, the document end. `cursor` says the editor's
   * cursor should go to that line, which it does at start and end.
   */
  function readPoint(scrollY, viewportHeight, scrollHeight) {
    if (atEnd(scrollY, viewportHeight, scrollHeight)) return { y: viewportHeight, cursor: true }
    if (scrollY <= 0) return { y: 0, cursor: true }
    return { y: viewportHeight * READING_LINE, cursor: false }
  }

  /** Whether `line` of the file with source index `src` is an include directive. */
  function isIncludeLine(includes, src, line) {
    return (includes || []).some(function (inc) {
      return inc.parent === src && inc.line === line
    })
  }

  /**
   * How the page follows a cursor on `line` (of `lastLine`), whose target
   * lies at client-Y `targetY`: `end` (scroll to the end of the page) for
   * the last line; for an include directive `align` (its chapter to the
   * top) unless the chapter starts between the top and the reading line;
   * else the dead band of the cursor follow (comfortably visible stays,
   * the top quarter snaps).
   */
  function followAction(o) {
    if (o.line >= o.lastLine) return 'end'
    var reading = o.viewportHeight * READING_LINE
    if (o.include) return o.targetY >= 0 && o.targetY <= reading ? 'none' : 'align'
    var visible = o.targetY >= reading && o.targetY < o.viewportHeight
    return visible ? 'none' : 'align'
  }

  /**
   * Where client-Y `y` lies against the content of the file with source index
   * `src` (an included chapter) and the files it includes: `above` its first
   * element, `below` its last one, or `null` inside it, or for a file without
   * content on the page. `entries` are `{line, src?, top}` in document order.
   */
  function chapterEdge(entries, includes, src, y) {
    var members = includedFiles(includes, src)
    var first = Infinity
    var last = -Infinity
    entries.forEach(function (entry) {
      if (entry.src === undefined || !members[entry.src]) return
      first = Math.min(first, entry.top)
      last = Math.max(last, entry.top)
    })
    if (first === Infinity) return null
    if (y < first) return 'above'
    var at = topAnchor(entries, y)
    return y > last && (at.src === undefined || !members[at.src]) ? 'below' : null
  }

  /** The last segment of a path, `/` or `\` separated. */
  function baseName(file) {
    var parts = String(file).split(/[\\/]/)
    return parts[parts.length - 1]
  }

  return {
    createScrollGate: createScrollGate,
    fileOfSrc: fileOfSrc,
    topAnchor: topAnchor,
    clickTarget: clickTarget,
    bannerFile: bannerFile,
    atEnd: atEnd,
    readPoint: readPoint,
    isIncludeLine: isIncludeLine,
    followAction: followAction,
    chapterEdge: chapterEdge,
    includedFiles: includedFiles,
    includeLineFor: includeLineFor,
    baseName: baseName,
  }
})
