/*
 * The preview page: receives server-sent events, patches the document into
 * the page and keeps it scrolled to the editor's cursor line.
 *
 * Events (`events` route, all data is one line of JSON):
 *   html          {file, lineCount, html}  new converted document
 *   cursor        {file, line}             cursor line, 1-based source line
 *                                          (the server added 1 to the LSP line)
 *   convert-error {message}                the conversion failed
 * `file` is the document the event belongs to. A cursor event for another file
 * than the shown one is kept and applied once that file's HTML has arrived.
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
  var cursor = null // last cursor event: {file, line}

  function followOn() {
    return followBox.checked
  }

  function cursorApplies() {
    return cursor !== null && cursor.file === shownFile
  }

  /** Anchors with their current client-Y, ready for the mapping functions. */
  function anchors() {
    var entries = []
    var elements = content.querySelectorAll(maps.ANCHOR_SELECTOR)
    for (var i = 0; i < elements.length; i++) {
      var parsed = maps.parseAnchorClass(elements[i].className)
      if (parsed === null) continue
      parsed.top = elements[i].getBoundingClientRect().top
      entries.push(parsed)
    }
    return maps.withSentinel(
      maps.mainFileAnchors(entries),
      lineCount,
      content.getBoundingClientRect().bottom
    )
  }

  /** Scroll so that source line `line` is at the top of the page. */
  function pinToLine(line) {
    var top = maps.scrollTopForLine(anchors(), window.scrollY, line)
    if (top !== undefined) window.scroll(0, Math.max(0, top))
  }

  /** Bring the cursor line into view, unless it is already comfortably there. */
  function revealCursor() {
    if (!cursorApplies()) return
    var top = maps.scrollTopForLine(anchors(), window.scrollY, cursor.line)
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
        : maps.sourceLineAtViewportTop(anchors(), 0)
    patch(message.html)
    shownFile = message.file
    lineCount = message.lineCount
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
