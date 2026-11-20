/*
 * The preview page: receives server-sent events, patches the document into
 * the page and keeps it scrolled to the editor's cursor line.
 *
 * Events (`events` route, all data is one line of JSON):
 *   html          {file, lineCount, html}  new converted document
 *   cursor        {file, line}             cursor line, 1-based source line
 *                                          (the server added 1 to the LSP line)
 *   convert-error {message}                the conversion failed
 * In `html`, `file` is the converted document: the top-level document in
 * whole-document mode, else the active file. In `cursor`, `file` is the file
 * the editor is in, which can be one the shown document includes. The page
 * maps it through the file table (`<script id="adoc-source-files">` in the
 * HTML): the shown document's own anchors have no `data-src-K`, an included
 * file's have its index K. A cursor in a file that the shown document does
 * not contain is kept and applied once HTML that contains it has arrived.
 *
 * Mapping logic lives in scroll-map.js; this file only touches the DOM.
 */
(function () {
  'use strict'

  var maps = window.AdocScrollMap
  var content = document.getElementById('content')
  var followBox = document.getElementById('follow')
  var statusEl = document.getElementById('preview-status')
  var banner = document.getElementById('preview-banner')

  var shownFile = null // file of the HTML in the page
  var lineCount = 0 // lines of that file, for the end sentinel
  var fileTable = [] // included files of the shown document, index = K
  var cursor = null // last cursor event: {file, line}

  function followOn() {
    return followBox.checked
  }

  /** `null`: the cursor is in the shown file; K: in included file K; `undefined`: not shown. */
  function cursorSrc() {
    if (cursor === null || shownFile === null) return undefined
    return maps.srcForFile(fileTable, shownFile, cursor.file)
  }

  function cursorApplies() {
    return cursorSrc() !== undefined
  }

  function readFileTable() {
    var script = document.getElementById('adoc-source-files')
    fileTable = maps.parseFileTable(script ? script.textContent : '[]')
  }

  /**
   * Anchors of one file (`src`: `null` for the shown file, else K) with their
   * current client-Y, ready for the mapping functions. Only the shown file
   * gets the end sentinel: the page knows no line count for an included one.
   */
  function anchors(src) {
    var entries = []
    var elements = content.querySelectorAll(maps.ANCHOR_SELECTOR)
    for (var i = 0; i < elements.length; i++) {
      var parsed = maps.parseAnchorClass(elements[i].className)
      if (parsed === null) continue
      parsed.top = elements[i].getBoundingClientRect().top
      entries.push(parsed)
    }
    var own = maps.fileAnchors(entries, src)
    if (src !== null) return own
    return maps.withSentinel(own, lineCount, content.getBoundingClientRect().bottom)
  }

  /** Scroll so that source line `line` is at the top of the page. */
  function pinToLine(line) {
    var top = maps.scrollTopForLine(anchors(null), window.scrollY, line)
    if (top !== undefined) window.scroll(0, Math.max(0, top))
  }

  /** Bring the cursor line into view, unless it is already comfortably there. */
  function revealCursor() {
    if (!cursorApplies()) return
    var top = maps.scrollTopForLine(anchors(cursorSrc()), window.scrollY, cursor.line)
    if (top === undefined) return
    if (!maps.shouldScroll(top - window.scrollY, window.innerHeight)) return
    window.scroll(0, Math.max(0, top))
  }

  function patch(html) {
    var next = content.cloneNode(false)
    next.innerHTML = html
    if (typeof window.morphdom === 'function') {
      window.morphdom(content, next, { childrenOnly: true })
    } else {
      content.innerHTML = html
    }
  }

  function showError(message) {
    banner.textContent = message
    banner.hidden = message === null
  }

  function onHtml(message) {
    // Without a cursor to follow, keep the line at the top of the page where
    // it is, even if the content above it changes height.
    // The line of another document says nothing about the new one.
    var keepLine =
      (followOn() && cursorApplies()) || message.file !== shownFile
        ? null
        : maps.sourceLineAtViewportTop(anchors(null), 0)
    patch(message.html)
    shownFile = message.file
    lineCount = message.lineCount
    readFileTable()
    showError(null)
    if (followOn() && cursorApplies()) revealCursor()
    else if (keepLine !== null) pinToLine(keepLine)
  }

  function onCursor(message) {
    cursor = message
    if (followOn()) revealCursor()
  }

  function listen(source, name, handler) {
    source.addEventListener(name, function (event) {
      var message
      try {
        message = JSON.parse(event.data)
      } catch (error) {
        return
      }
      handler(message)
    })
  }

  followBox.addEventListener('change', function () {
    if (followOn()) revealCursor()
  })

  var source = new EventSource('events')
  source.onopen = function () {
    statusEl.textContent = ''
  }
  source.onerror = function () {
    statusEl.textContent = 'Disconnected, retrying...'
  }
  listen(source, 'html', onHtml)
  listen(source, 'cursor', onCursor)
  listen(source, 'convert-error', function (message) {
    showError(message.message)
  })
})()
