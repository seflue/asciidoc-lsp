// Tests for the pure functions of assets/preview/scroll-debug.js.
// Run with `just test-js` (Node's built-in runner, no npm packages).
//
// Loaded like in the page: scroll-map.js first, then scroll-debug.js, both as
// plain scripts in one context without `module`, so each publishes its global.
// Not covered: the DOM wiring in page.js (overlay, fetch).

'use strict'

const test = require('node:test')
const strict = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')

const ASSETS = path.join(__dirname, '..', '..', 'assets', 'preview')

const context = vm.createContext({})
for (const name of ['scroll-map.js', 'scroll-debug.js']) {
  const file = path.join(ASSETS, name)
  vm.runInContext(fs.readFileSync(file, 'utf8'), context, { filename: file })
}
const debug = context.AdocScrollDebug

// Objects built inside the vm context have that realm's prototypes, which
// deepStrictEqual rejects; compare plain copies.
const assert = Object.assign(Object.create(strict), {
  deepEqual: (actual, expected, ...message) =>
    strict.deepEqual(JSON.parse(JSON.stringify(actual)), expected, ...message),
})

// A page scrolled to 100: the page-top anchor, and two paragraph anchors at
// client-Y 200 and 600 (absolute 300 and 700).
const entries = [
  { line: 10, top: 200, bottom: 250, what: 'paragraph data-line-10' },
  { line: 20, top: 600, bottom: 650, what: 'paragraph data-line-20' },
]
const anchors = [
  { line: 1, top: -100 },
  { line: 10, top: 200 },
  { line: 20, top: 600 },
]

test('a reveal report names the two anchors the target lies between', () => {
  const report = debug.revealReport({
    seq: 3, file: '/doc.adoc', line: 15, src: null, skipped: null,
    anchors: anchors, entries: entries,
    scrollBefore: 100, scrollAfter: 500, viewport: 800, target: 500, action: 'align',
  })
  assert.deepEqual(report, {
    seq: 3, kind: 'reveal', file: '/doc.adoc', line: 15, src: null, skipped: null,
    prev: { line: 10, y: 300, what: 'paragraph data-line-10' },
    next: { line: 20, y: 700, what: 'paragraph data-line-20' },
    target: 500, action: 'align', scrollBefore: 100, scrollAfter: 500, viewport: 800,
  })
})

test('an anchor of no block is called synthetic', () => {
  const report = debug.revealReport({
    seq: 1, file: '/doc.adoc', line: 4, src: null, skipped: null,
    anchors: anchors, entries: entries,
    scrollBefore: 100, scrollAfter: 100, viewport: 800, target: 100, action: null,
  })
  assert.deepEqual(report.prev, { line: 1, y: 0, what: 'synthetic' })
  assert.deepEqual(report.next, { line: 10, y: 300, what: 'paragraph data-line-10' })
  assert.equal(report.action, null)
})

test('an anchor matches the block of its own file only', () => {
  const ofChapter = [{ line: 10, src: 2, top: 200, bottom: 250, what: 'paragraph data-src-2 data-line-10' }]
  const report = debug.revealReport({
    seq: 1, file: '/doc.adoc', line: 10, src: null, skipped: null,
    anchors: [{ line: 10, top: 200 }], entries: ofChapter,
    scrollBefore: 0, scrollAfter: 200, viewport: 800, target: 200, action: 'align',
  })
  assert.deepEqual(report.prev, { line: 10, y: 200, what: 'synthetic' })
})

test('a skipped reveal says why and has no anchors', () => {
  const report = debug.revealReport({
    seq: 7, file: '/other.adoc', line: 2, src: undefined, skipped: 'not-shown',
    scrollBefore: 40, scrollAfter: 40, viewport: 800,
  })
  assert.deepEqual(report, {
    seq: 7, kind: 'reveal', file: '/other.adoc', line: 2, src: 'not-shown', skipped: 'not-shown',
    prev: null, next: null, target: null, action: null,
    scrollBefore: 40, scrollAfter: 40, viewport: 800,
  })
})

test('a long class attribute is cut to 120 characters', () => {
  const long = 'x'.repeat(300)
  const report = debug.revealReport({
    seq: 1, file: '/doc.adoc', line: 10, src: null, skipped: null,
    anchors: [{ line: 10, top: 200 }], entries: [{ line: 10, top: 200, bottom: 210, what: long }],
    scrollBefore: 0, scrollAfter: 0, viewport: 800, target: 200, action: null,
  })
  assert.equal(report.prev.what, 'x'.repeat(120))
})

test('scroll and html reports carry what the page sent and did', () => {
  assert.deepEqual(debug.scrollReport({ seq: 4, file: '/doc.adoc', line: 30, cursor: true, scrollY: 900 }), {
    seq: 4, kind: 'scroll', file: '/doc.adoc', line: 30, cursor: true, scrollY: 900,
  })
  assert.deepEqual(debug.htmlReport({
    seq: 4, file: '/doc.adoc', following: false, keepLine: 12.5, scrollBefore: 300, scrollAfter: 320,
  }), {
    seq: 4, kind: 'html', file: '/doc.adoc', following: false, keepLine: 12.5, scrollBefore: 300, scrollAfter: 320,
  })
})

test('the overlay text spells out the sum', () => {
  const text = debug.formatReport(debug.revealReport({
    seq: 3, file: '/book/doc.adoc', line: 15, src: null, skipped: null,
    anchors: anchors, entries: entries,
    scrollBefore: 100, scrollAfter: 500, viewport: 800, target: 500, action: 'align',
  }))
  assert.equal(text, [
    'seq 3  reveal  doc.adoc:15  src=null',
    'prev  line 10 @Y=300  paragraph data-line-10',
    'next  line 20 @Y=700  paragraph data-line-20',
    'target Y=500  action=align  scrollY 100 → 500  viewport 800',
  ].join('\n'))
  assert.equal(
    debug.formatReport(debug.revealReport({
      seq: 7, file: '/other.adoc', line: 2, src: undefined, skipped: 'not-shown',
      scrollBefore: 40, scrollAfter: 40, viewport: 800,
    })),
    'seq 7  reveal  other.adoc:2  src=not-shown\nskipped: not-shown  scrollY 40'
  )
})

test('the reporter sends and shows nothing until debug is switched on', () => {
  const sent = []
  const shown = []
  const reporter = debug.createReporter({
    post: (route, body) => sent.push([route, body]),
    show: (text) => shown.push(text),
  })
  const report = debug.scrollReport({ seq: 1, file: '/doc.adoc', line: 3, cursor: false, scrollY: 0 })
  assert.equal(reporter.enabled(), false)
  reporter.send(report)
  assert.deepEqual(sent, [])
  assert.deepEqual(shown, [])

  reporter.switchTo(true)
  reporter.send(report)
  assert.deepEqual(sent, [['debug', JSON.parse(JSON.stringify(report))]])
  assert.deepEqual(shown, [debug.formatReport(report)])

  reporter.switchTo(false)
  reporter.send(report)
  assert.equal(sent.length, 1)
})
