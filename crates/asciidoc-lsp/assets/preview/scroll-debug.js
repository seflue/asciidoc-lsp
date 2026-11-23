/*
 * Scroll-sync debug reports: while the editor has switched the debug log on
 * (`:AsciidocPreview debug on`), the page tells the server, per cursor event,
 * how it turned the line into a scroll position, and shows the last report in
 * an overlay. The server pairs each report with the event it answers by its
 * `seq`. Pure functions and the reporter that sends only while debug is
 * on; page.js gives it the POST and the overlay.
 *
 * Reports (`kind`):
 *   reveal  a cursor event: the file's two anchors the line lies between
 *           (`prev`, `next`: source line, absolute page Y and the class of
 *           the block, or `synthetic` for an include edge, the page top or
 *           the end sentinel), the target, the action and the scroll
 *           position before and after; or `skipped` and why
 *   scroll  the page reported a scroll to the editor
 *   html    a new conversion arrived: whether the page followed the cursor
 *           or kept its line at the top, and the scroll position around it
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) {
    module.exports = factory(require('./scroll-map.js'))
  } else {
    root.AdocScrollDebug = factory(root.AdocScrollMap)
  }
})(typeof self !== 'undefined' ? self : this, function (maps) {
  'use strict'

  /** Longest block class kept in a report. */
  var MAX_WHAT = 120

  function round(value) {
    return typeof value === 'number' && isFinite(value) ? Math.round(value) : null
  }

  /**
   * `anchor` ({line, top}, client-Y) as `{line, y, what}`: `y` absolute at
   * scroll position `scrollY`, `what` the class of the block of file `src`
   * at that line and top among `entries`, or `synthetic`.
   */
  function describe(anchor, entries, src, scrollY) {
    if (!anchor) return null
    var block = null
    for (var i = 0; i < entries.length; i++) {
      var entry = entries[i]
      var from = entry.src === undefined ? null : entry.src
      if (from === src && entry.line === anchor.line && entry.top === anchor.top) {
        block = entry
        break
      }
    }
    return {
      line: anchor.line,
      y: round(anchor.top + scrollY),
      what: block ? String(block.what).slice(0, MAX_WHAT) : 'synthetic',
    }
  }

  /**
   * The report on one cursor event. `src` is the cursor's file as the page
   * resolved it (`undefined`: not on the page); `anchors` are that file's
   * anchors as the mapping saw them, `entries` the page's blocks read at the
   * same moment, each with its class as `what`.
   */
  function revealReport(input) {
    var anchors = input.anchors || []
    var entries = input.entries || []
    var src = input.src === undefined ? 'not-shown' : input.src
    var prev = null
    var next = null
    if (anchors.length) {
      var idx = maps.anchorIndicesForLine(anchors.map(function (a) { return a.line }), input.line)
      prev = describe(anchors[idx.previous], entries, input.src, input.scrollBefore)
      next = idx.next >= 0 ? describe(anchors[idx.next], entries, input.src, input.scrollBefore) : null
    }
    return {
      seq: input.seq,
      kind: 'reveal',
      file: input.file,
      line: input.line,
      src: src,
      skipped: input.skipped || null,
      prev: prev,
      next: next,
      target: input.target === undefined ? null : round(input.target),
      action: input.action || null,
      scrollBefore: round(input.scrollBefore),
      scrollAfter: round(input.scrollAfter),
      viewport: round(input.viewport),
    }
  }

  /** The report on a scroll the page sent to the editor. */
  function scrollReport(input) {
    return {
      seq: input.seq,
      kind: 'scroll',
      file: input.file,
      line: input.line,
      cursor: input.cursor,
      scrollY: round(input.scrollY),
    }
  }

  /** The report on a new conversion. `keepLine` is null while following. */
  function htmlReport(input) {
    return {
      seq: input.seq,
      kind: 'html',
      file: input.file,
      following: input.following,
      keepLine: input.keepLine,
      scrollBefore: round(input.scrollBefore),
      scrollAfter: round(input.scrollAfter),
    }
  }

  function baseName(file) {
    var parts = String(file).split(/[\\/]/)
    return parts[parts.length - 1]
  }

  function anchorText(label, anchor) {
    if (!anchor) return label + '  -'
    return label + '  line ' + anchor.line + ' @Y=' + anchor.y + '  ' + anchor.what
  }

  /** The overlay text of a report. */
  function formatReport(report) {
    var head = 'seq ' + report.seq + '  ' + report.kind + '  '
    if (report.kind === 'scroll') {
      return head + baseName(report.file) + ':' + report.line + (report.cursor ? '  +cursor' : '') +
        '\nscrollY ' + report.scrollY
    }
    if (report.kind === 'html') {
      return head + baseName(report.file) +
        (report.following ? '  following' : '  keep line ' + report.keepLine) +
        '\nscrollY ' + report.scrollBefore + ' → ' + report.scrollAfter
    }
    head += baseName(report.file) + ':' + report.line + '  src=' + report.src
    if (report.skipped) return head + '\nskipped: ' + report.skipped + '  scrollY ' + report.scrollBefore
    return [
      head,
      anchorText('prev', report.prev),
      anchorText('next', report.next),
      'target Y=' + report.target + '  action=' + report.action +
        '  scrollY ' + report.scrollBefore + ' → ' + report.scrollAfter + '  viewport ' + report.viewport,
    ].join('\n')
  }

  /**
   * The page's debug channel: `send(report)` posts it (`post('debug',
   * report)`) and shows it (`show(text)`), but only after `switchTo(true)`.
   */
  function createReporter(io) {
    var on = false
    return {
      enabled: function () { return on },
      switchTo: function (enabled) { on = enabled === true },
      send: function (report) {
        if (!on) return
        io.show(formatReport(report))
        io.post('debug', report)
      },
    }
  }

  return {
    createReporter: createReporter,
    revealReport: revealReport,
    scrollReport: scrollReport,
    htmlReport: htmlReport,
    formatReport: formatReport,
  }
})
