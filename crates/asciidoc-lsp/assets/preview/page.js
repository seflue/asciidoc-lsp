/*
 * The preview page: receives server-sent events, patches the document into
 * the page and keeps it scrolled to the editor's cursor line.
 *
 * Events (`events` route, all data is one line of JSON):
 *   html          {file, lineCount, html}  new converted document
 *   cursor        {file, line}             cursor line, 1-based source line
 *                                          (the server added 1 to the LSP line)
 *   convert-error {message}                the conversion failed
 *   debug         {enabled}                the debug log was switched; while
 *                                          it is on, `cursor` carries `seq` and
 *                                          the page reports on each event
 *                                          (`POST debug`, see scroll-debug.js)
 * In `html`, `file` is the converted document: the top-level document in
 * whole-document mode, else the active file. In `cursor`, `file` is the file
 * the editor is in, which can be one the shown document includes. The page
 * maps it through the file table (`<script id="adoc-source-files">` in the
 * HTML): the shown document's own anchors have no `data-src-K`, an included
 * file's have its index K. The include table (`<script
 * id="adoc-source-includes">`) lets a cursor on an `include::` line scroll to
 * the included content. A cursor in a file that the shown document does
 * not contain is kept and applied once HTML that contains it has arrived.
 *
 * Preview → editor: a double-click on a block sends `POST click {file,
 * line}`, scrolling by the reader sends `POST scroll {file, line, cursor}`
 * with the source line on the reading line, a quarter down the page (see
 * page-sync.js `readPoint`), and a banner names the file there when it is
 * not the editor's. What counts as the reader's scroll, and
 * when the page keeps still, is page-sync.js's scroll gate.
 *
 * Mapping logic lives in scroll-map.js and page-sync.js; this file only
 * touches the DOM.
 */
(function () {
  'use strict'

  var maps = window.AdocScrollMap
  var sync = window.AdocPageSync
  var debug = window.AdocScrollDebug
  var content = document.getElementById('content')
  var followBox = document.getElementById('follow')
  var jumpBox = document.getElementById('jump')
  var scrollEditorBox = document.getElementById('scroll-editor')
  var statusEl = document.getElementById('preview-status')
  var banner = document.getElementById('preview-banner')
  var sourceBanner = document.getElementById('preview-source')
  var sourceText = document.getElementById('preview-source-text')
  var sourceOpen = document.getElementById('preview-source-open')
  var debugBox = document.getElementById('preview-debug')

  var gate = sync.createScrollGate({ inputWindow: 600, echoWindow: 250, throttle: 100 })
  var trailingTimer = null
  var followTimer = null
  var bannerFrame = null
  var bannerTarget = null // {file, line} the banner's button opens

  var shownFile = null // file of the HTML in the page
  var lineCount = 0 // lines of that file, for the end sentinel
  // A line past any file: the editor clamps it to the file's last line.
  var END_OF_FILE = 2147483647
  var fileTable = [] // included files of the shown document, index = K
  var includeTable = [] // its include directives: {parent, line, file}
  var cursor = null // last cursor event: {file, line[, seq]}
  var baseBodyClass = document.body.className // what the page itself puts on its body

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

  function now() {
    return performance.now()
  }

  /** The file the editor is in: the last cursor's, else the shown one. */
  function activeFile() {
    return cursor === null ? shownFile : cursor.file
  }

  /** Scroll the page by itself; the scroll gate does not report it. */
  function scrollPage(top) {
    gate.ownScroll(now())
    window.scroll(0, Math.max(0, top))
  }

  /** Tell the server about a page action; the page needs no answer. */
  function post(route, body) {
    fetch(route, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    }).catch(function () {})
  }

  function readFileTable() {
    var script = document.getElementById('adoc-source-files')
    fileTable = maps.parseFileTable(script ? script.textContent : '[]')
    var includes = document.getElementById('adoc-source-includes')
    includeTable = maps.parseIncludeTable(includes ? includes.textContent : '[]')
  }

  /**
   * Body classes of the standalone page (`toc2 toc-left` for a sidebar toc):
   * the sidebar is laid out by the stylesheet through them.
   */
  function applyBodyClasses() {
    var script = document.getElementById('adoc-body-classes')
    document.body.className = maps.bodyClassName(baseBodyClass, script ? script.textContent : '[]')
  }

  /** The tab title: the document title, set as text. */
  function applyTitle() {
    var script = document.getElementById('adoc-page-title')
    document.title = maps.pageTitle(script ? script.textContent : '""')
  }

  /**
   * Anchors of one file (`src`: `null` for the shown file, else K) with their
   * current client-Y, ready for the mapping functions. Only the shown file
   * gets the end sentinel: the page knows no line count for an included one.
   */
  function anchors(src, entries) {
    var own = maps.fileAnchorsWithIncludes(entries || rawEntries(), includeTable, src)
    if (src !== null) return own
    return maps.withSentinel(
      maps.withPageTop(own, window.scrollY),
      lineCount,
      content.getBoundingClientRect().bottom
    )
  }

  /** Every anchor of the page with its current client-Y, in document order. */
  function rawEntries() {
    var entries = []
    var elements = content.querySelectorAll(maps.ANCHOR_SELECTOR)
    for (var i = 0; i < elements.length; i++) {
      var parsed = maps.parseAnchorClass(elements[i].className)
      if (parsed === null) continue
      var rect = elements[i].getBoundingClientRect()
      parsed.top = rect.top
      parsed.bottom = rect.bottom
      parsed.what = elements[i].className
      entries.push(parsed)
    }
    return entries
  }

  /** Scroll so that source line `line` is at the top of the page. */
  function pinToLine(line) {
    var top = maps.scrollTopForLine(anchors(null), window.scrollY, line)
    if (top !== undefined) scrollPage(top)
  }

  /** The `seq` of the last cursor event, 0 before the first. */
  function lastSeq() {
    return cursor !== null && typeof cursor.seq === 'number' ? cursor.seq : 0
  }

  var reporter = debug.createReporter({
    post: post,
    show: function (text) { debugBox.textContent = text },
  })
  var sendDebug = reporter.send

  /** Report on the cursor event: `info` adds what `revealCursor` saw. */
  function debugReveal(info) {
    if (!reporter.enabled() || cursor === null) return
    sendDebug(debug.revealReport({
      seq: lastSeq(),
      file: cursor.file,
      line: cursor.line,
      src: info.src,
      skipped: info.skipped,
      anchors: info.anchors,
      entries: info.entries,
      scrollBefore: info.scrollBefore === undefined ? window.scrollY : info.scrollBefore,
      scrollAfter: window.scrollY,
      viewport: window.innerHeight,
      target: info.target,
      action: info.action,
    }))
  }

  /** Bring the cursor line into view, unless it is already comfortably there. */
  function revealCursor() {
    if (!cursorApplies()) return debugReveal({ skipped: 'not-shown' })
    // While the page leads, the editor is where the page put it, not at the
    // last cursor it reported.
    if (gate.pageLeads()) return debugReveal({ src: cursorSrc(), skipped: 'page-leads' })
    var src = cursorSrc()
    var before = window.scrollY
    var entries = rawEntries()
    var list = anchors(src, entries)
    var top = maps.scrollTopForLine(list, before, cursor.line)
    if (top === undefined) return debugReveal({ src: src, skipped: 'no-anchors' })
    var action = sync.followAction({
      line: cursor.line,
      // The end of the page is the end of the shown file only.
      lastLine: src === null && lineCount > 0 ? lineCount : Infinity,
      include: sync.isIncludeLine(includeTable, src, cursor.line),
      targetY: top - window.scrollY,
      viewportHeight: window.innerHeight,
    })
    if (action === 'end') scrollPage(document.documentElement.scrollHeight - window.innerHeight)
    else if (action === 'align') scrollPage(top)
    debugReveal({
      src: src, anchors: list, entries: entries, scrollBefore: before, target: top, action: action,
    })
  }

  /**
   * The page's reading point (see `readPoint`), the anchor there and the
   * page's anchors, read once: the scroll report and the banner agree on it.
   */
  function reading() {
    var point = sync.readPoint(window.scrollY, window.innerHeight, document.documentElement.scrollHeight)
    var entries = rawEntries()
    return { point: point, entries: entries, anchor: sync.topAnchor(entries, point.y) }
  }

  function report(body) {
    post('scroll', body)
    gate.reported()
    sendDebug(debug.scrollReport({
      seq: lastSeq(), file: body.file, line: body.line, cursor: body.cursor, scrollY: window.scrollY,
    }))
  }

  /**
   * Report the source line on the page's reading point in the editor's
   * file: if the block there is that file's, or comes from a file it
   * includes, directly or through others; then the line is that of its
   * `include::` directive, and the editor's cursor goes there. Outside the
   * editor's chapter: the chapter's first or last line, with the cursor.
   */
  function sendScroll() {
    var file = activeFile()
    if (file === null) return
    var src = maps.srcForFile(fileTable, shownFile, file)
    if (src === undefined) return
    var at = reading()
    var topSrc = at.anchor.src === undefined ? null : at.anchor.src
    if (!sync.includedFiles(includeTable, src)[topSrc]) {
      var side = src === null ? null : sync.chapterEdge(at.entries, includeTable, src, at.point.y)
      if (side === 'above') report({ file: file, line: 1, cursor: true })
      else if (side === 'below') report({ file: file, line: END_OF_FILE, cursor: true })
      return
    }
    // Included content stands for its include directive; elsewhere the line
    // whose region holds the reading point (rounded down).
    var through = topSrc === src ? null : sync.includeLineFor(includeTable, src, topSrc)
    var line = through !== null ? through : maps.sourceLineAtViewportTop(anchors(src), at.point.y)
    if (line === null) return
    report({ file: file, line: Math.max(1, Math.floor(line)), cursor: at.point.cursor || topSrc !== src })
  }

  function onPageScroll() {
    scheduleBanner()
    if (!scrollEditorBox.checked) return
    var verdict = gate.onScroll(now(), document.hasFocus())
    if (verdict === 'send') sendScroll()
    else if (verdict === 'later' && trailingTimer === null) {
      trailingTimer = setTimeout(function () {
        trailingTimer = null
        if (gate.onTimer(now()) === 'send') sendScroll()
      }, gate.delay(now()))
    }
  }

  /** Show the banner when the top of the page comes from another file. */
  function updateBanner() {
    bannerFrame = null
    var top = reading().anchor
    var file = sync.bannerFile(top, fileTable, shownFile, activeFile())
    if (file === null) {
      bannerTarget = null
      sourceBanner.hidden = true
      return
    }
    bannerTarget = { file: file, line: top.line }
    sourceText.textContent = 'This section comes from ' + sync.baseName(file)
    sourceBanner.hidden = false
  }

  function scheduleBanner() {
    if (bannerFrame === null) bannerFrame = requestAnimationFrame(updateBanner)
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
    // While the page leads, the cursor it knows is stale: keep the line too.
    var following = followOn() && cursorApplies() && !gate.pageLeads()
    var keepLine =
      following || message.file !== shownFile ? null : maps.sourceLineAtViewportTop(anchors(null), 0)
    var scrollBefore = window.scrollY
    // A page of another height moves the scroll position by itself (the
    // browser clamps it): that scroll is not the reader's.
    gate.ownScroll(now())
    patch(message.html)
    shownFile = message.file
    lineCount = message.lineCount
    readFileTable()
    applyBodyClasses()
    applyTitle()
    showError(null)
    if (following) revealCursor()
    else if (keepLine !== null) pinToLine(keepLine)
    sendDebug(debug.htmlReport({
      seq: lastSeq(), file: message.file, following: following, keepLine: keepLine,
      scrollBefore: scrollBefore, scrollAfter: window.scrollY,
    }))
    scheduleBanner()
  }

  function onCursor(message) {
    var time = now()
    gate.cursorEvent(time)
    cursor = message
    clearTimeout(followTimer)
    if (followOn()) {
      // Right after the page reported a scroll, wait for the echo window to
      // end, then follow: the editor reports no move it was made to do, so
      // what arrives is the reader's own move and must not be lost.
      if (gate.followsCursor(time)) revealCursor()
      else {
        debugReveal({ src: cursorSrc(), skipped: 'deferred' })
        followTimer = setTimeout(function () {
          if (followOn()) revealCursor()
        }, gate.followDelay(time))
      }
    } else debugReveal({ src: cursorSrc(), skipped: 'follow-off' })
    scheduleBanner()
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

  function onInput() {
    gate.userInput(now())
  }
  var passive = { passive: true }
  window.addEventListener('wheel', onInput, passive)
  window.addEventListener('keydown', onInput)
  window.addEventListener('touchstart', onInput, passive)
  window.addEventListener('touchmove', onInput, passive)
  window.addEventListener('pointerdown', function () {
    gate.pointerDown(now())
  })
  window.addEventListener('pointerup', function () {
    gate.pointerUp(now())
  })
  window.addEventListener('pointercancel', function () {
    gate.pointerUp(now())
  })
  window.addEventListener('scroll', onPageScroll, passive)

  content.addEventListener('dblclick', function (event) {
    if (!jumpBox.checked || !(event.target instanceof Element)) return
    // A link keeps its own meaning.
    if (event.target.closest('a')) return
    var anchor = event.target.closest(maps.ANCHOR_SELECTOR)
    if (anchor === null) return
    var target = sync.clickTarget(maps.parseAnchorClass(anchor.className), fileTable, shownFile)
    if (target === null) return
    var selection = window.getSelection()
    if (selection) selection.removeAllRanges()
    post('click', target)
  })

  sourceOpen.addEventListener('click', function () {
    if (bannerTarget !== null) post('click', bannerTarget)
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
  listen(source, 'debug', function (message) {
    reporter.switchTo(message.enabled)
    debugBox.hidden = !reporter.enabled()
  })
})()
