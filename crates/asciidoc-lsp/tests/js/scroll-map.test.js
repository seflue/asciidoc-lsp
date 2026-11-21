// Tests for the pure functions of assets/preview/scroll-map.js.
// Run with `just test-js` (Node's built-in runner, no npm packages).
//
// The script is loaded the way a browser loads it: as a plain script in a
// context that has no `module`, so it publishes `AdocScrollMap` on the global.
// Not covered: page.js and everything that touches the DOM (reading anchor
// positions with getBoundingClientRect, scrolling, SSE handling, morphdom
// patching) and `ANCHOR_SELECTOR`, which only a real DOM can evaluate.

'use strict'

const test = require('node:test')
const strict = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')

const SCRIPT = path.join(__dirname, '..', '..', 'assets', 'preview', 'scroll-map.js')

const context = vm.createContext({})
vm.runInContext(fs.readFileSync(SCRIPT, 'utf8'), context, { filename: SCRIPT })
const map = context.AdocScrollMap

// Objects built inside the vm context have that realm's prototypes, which
// deepStrictEqual rejects; compare plain copies.
const assert = Object.assign(Object.create(strict), {
  deepEqual: (actual, expected, ...message) =>
    strict.deepEqual(JSON.parse(JSON.stringify(actual)), expected, ...message),
})

test('script publishes AdocScrollMap on the global like in the browser', () => {
  assert.equal(typeof map, 'object')
  for (const name of ['parseAnchorClass', 'fileAnchors', 'mainFileAnchors',
    'parseFileTable', 'srcForFile', 'withSentinel', 'anchorIndicesForLine',
    'scrollTopForLine', 'sourceLineAtViewportTop', 'shouldScroll']) {
    assert.equal(typeof map[name], 'function', name)
  }
})

// Anchors of a short document: heading at line 1, paragraphs at 3 and 7,
// a listing at 12. Tops are client-Y pixels at scrollY 0.
const anchors = [
  { line: 1, top: 0 },
  { line: 3, top: 100 },
  { line: 7, top: 300 },
  { line: 12, top: 800 },
]

test('parseAnchorClass reads main-file and included-file classes', () => {
  assert.deepEqual(map.parseAnchorClass('paragraph data-line-42'), { line: 42 })
  assert.deepEqual(map.parseAnchorClass('data-line-7 data-src-2 listingblock'),
    { line: 7, src: 2 })
  assert.equal(map.parseAnchorClass('paragraph'), null)
  assert.equal(map.parseAnchorClass('data-src-1'), null)
  assert.equal(map.parseAnchorClass('xdata-line-3'), null)
  assert.equal(map.parseAnchorClass(undefined), null)
})

test('anchorIndicesForLine: exact hit, between, above first, below last', () => {
  const lines = anchors.map((a) => a.line)
  assert.deepEqual(map.anchorIndicesForLine(lines, 7), { previous: 2, next: -1 })
  assert.deepEqual(map.anchorIndicesForLine(lines, 5), { previous: 1, next: 2 })
  assert.deepEqual(map.anchorIndicesForLine(lines, 0), { previous: 0, next: 0 })
  assert.deepEqual(map.anchorIndicesForLine(lines, 99), { previous: 3, next: -1 })
  assert.deepEqual(map.anchorIndicesForLine(lines, 5.9), { previous: 1, next: 2 })
})

test('scrollTopForLine interpolates between two anchors', () => {
  // Line 5 is halfway between lines 3 (top 100) and 7 (top 300).
  assert.equal(map.scrollTopForLine(anchors, 0, 5), 200)
  // Line 4 is a quarter of the way.
  assert.equal(map.scrollTopForLine(anchors, 0, 4), 150)
  // Anchor tops are client-Y: the current scroll offset is added.
  assert.equal(map.scrollTopForLine(anchors, 1000, 5), 1200)
})

test('scrollTopForLine at an anchor returns that anchor', () => {
  assert.equal(map.scrollTopForLine(anchors, 0, 7), 300)
  assert.equal(map.scrollTopForLine(anchors, 50, 1), 50)
})

test('scrollTopForLine before the first anchor clamps to it', () => {
  const late = [{ line: 5, top: 120 }, { line: 9, top: 400 }]
  assert.equal(map.scrollTopForLine(late, 0, 1), 120)
})

test('scrollTopForLine after the last anchor stays at the last anchor', () => {
  assert.equal(map.scrollTopForLine(anchors, 0, 500), 800)
})

test('scrollTopForLine with the end sentinel interpolates past the last block', () => {
  // A 20-line document whose last block starts at line 12 (top 800) and whose
  // content ends at 1100.
  const withEnd = map.withSentinel(anchors, 20, 1100)
  assert.equal(withEnd.length, anchors.length + 1)
  assert.deepEqual(withEnd[withEnd.length - 1], { line: 20, top: 1100 })
  assert.equal(map.scrollTopForLine(withEnd, 0, 16), 950)
  assert.equal(map.scrollTopForLine(withEnd, 0, 20), 1100)
})

test('scrollTopForLine with equal-line anchors does not divide by zero', () => {
  // Two blocks on one source line (a list and its first item) must not
  // produce NaN; the first one wins.
  const equal = [{ line: 4, top: 100 }, { line: 4, top: 100 }, { line: 8, top: 500 }]
  const top = map.scrollTopForLine(equal, 0, 4)
  assert.equal(top, 100)
  assert.equal(map.scrollTopForLine([{ line: 4, top: 10 }, { line: 4, top: 90 }], 0, 4), 10)
})

test('scrollTopForLine without anchors is undefined', () => {
  assert.equal(map.scrollTopForLine([], 0, 3), undefined)
})

test('withSentinel keeps the sequence strictly ascending', () => {
  // lineCount at or below the last anchor line: sentinel goes to last + 1.
  const out = map.withSentinel(anchors, 12, 900)
  assert.deepEqual(out[out.length - 1], { line: 13, top: 900 })
  // No anchors or no lines: unchanged.
  assert.deepEqual(map.withSentinel([], 20, 900), [])
  assert.equal(map.withSentinel(anchors, 0, 900), anchors)
})

test('sourceLineAtViewportTop interpolates and clamps', () => {
  assert.equal(map.sourceLineAtViewportTop(anchors, 200), 5)
  assert.equal(map.sourceLineAtViewportTop(anchors, 300), 7)
  // Above the first anchor: its line.
  assert.equal(map.sourceLineAtViewportTop(anchors, -50), 1)
  // Below the last anchor without sentinel: its line.
  assert.equal(map.sourceLineAtViewportTop(anchors, 5000), 12)
  // With a sentinel the line keeps growing to the end of the document.
  const withEnd = map.withSentinel(anchors, 20, 1100)
  assert.equal(map.sourceLineAtViewportTop(withEnd, 950), 16)
  assert.equal(map.sourceLineAtViewportTop([], 10), null)
})

test('sourceLineAtViewportTop with anchors at the same top does not produce NaN', () => {
  const stacked = [{ line: 2, top: 100 }, { line: 3, top: 100 }, { line: 9, top: 400 }]
  assert.equal(map.sourceLineAtViewportTop(stacked, 100), 3)
})

test('scrollTopForLine and sourceLineAtViewportTop round-trip', () => {
  for (const line of [1, 2, 3, 5, 7, 9.5, 12]) {
    const top = map.scrollTopForLine(anchors, 0, line)
    assert.ok(Math.abs(map.sourceLineAtViewportTop(anchors, top) - line) < 1e-9, String(line))
  }
})

test('shouldScroll: snap zone is the top quarter of the viewport', () => {
  const height = 800
  assert.equal(map.shouldScroll(100, height), true) // inside the top quarter: snap
  assert.equal(map.shouldScroll(200, height), true) // boundary counts as snap zone
  assert.equal(map.shouldScroll(201, height), false) // comfortably on screen
  assert.equal(map.shouldScroll(799, height), false)
  assert.equal(map.shouldScroll(800, height), true) // at the bottom edge or below
  assert.equal(map.shouldScroll(-30, height), true) // above the viewport
})

// A document that includes file 0 (Phase 2): blocks of the main file and of
// the included file interleave, and the included file's lines are its own.
const entries = [
  { line: 1, top: 0 },
  { line: 3, top: 100 },
  { line: 1, src: 0, top: 200 },
  { line: 5, src: 0, top: 260 },
  { line: 8, src: 0, top: 380 },
  { line: 6, top: 500 },
  { line: 10, top: 640 },
]

test('fileAnchors for the main file ignores included blocks', () => {
  assert.deepEqual(map.fileAnchors(entries), [
    { line: 1, top: 0 },
    { line: 3, top: 100 },
    { line: 6, top: 500 },
    { line: 10, top: 640 },
  ])
  assert.deepEqual(map.mainFileAnchors(entries), JSON.parse(JSON.stringify(map.fileAnchors(entries, null))))
})

test('fileAnchors for an included file returns only its own anchors', () => {
  assert.deepEqual(map.fileAnchors(entries, 0), [
    { line: 1, top: 200 },
    { line: 5, top: 260 },
    { line: 8, top: 380 },
  ])
  assert.deepEqual(map.fileAnchors(entries, 3), [])
})

test('fileAnchors keeps anchors that share a line with a list (fe97f3d)', () => {
  // A list and its first item are both anchored on line 12 of the included
  // file. Equal lines are not a fall back: the later anchors must stay.
  const list = [
    { line: 10, src: 1, top: 100 },
    { line: 12, src: 1, top: 160 }, // ulist
    { line: 12, src: 1, top: 164 }, // its first list item
    { line: 14, src: 1, top: 220 },
    { line: 20, src: 1, top: 400 },
  ]
  assert.deepEqual(map.fileAnchors(list, 1), [
    { line: 10, top: 100 },
    { line: 12, top: 160 },
    { line: 14, top: 220 },
    { line: 20, top: 400 },
  ])
})

test('fileAnchors stops where an included file starts over (second include)', () => {
  const twice = [
    { line: 1, src: 0, top: 0 },
    { line: 4, src: 0, top: 80 },
    { line: 9, src: 0, top: 200 },
    { line: 1, src: 0, top: 600 }, // second occurrence
    { line: 4, src: 0, top: 680 },
  ]
  assert.deepEqual(map.fileAnchors(twice, 0), [
    { line: 1, top: 0 },
    { line: 4, top: 80 },
    { line: 9, top: 200 },
  ])
})

test('fileAnchors keeps the result strictly ascending in the main file', () => {
  // The main file has one occurrence: a stray line that does not rise is
  // skipped, not a reason to stop.
  const stray = [
    { line: 2, top: 10 },
    { line: 6, top: 90 },
    { line: 4, top: 120 },
    { line: 9, top: 200 },
  ]
  assert.deepEqual(map.fileAnchors(stray, null), [
    { line: 2, top: 10 },
    { line: 6, top: 90 },
    { line: 9, top: 200 },
  ])
  assert.deepEqual(map.fileAnchors([], null), [])
})

test('parseFileTable reads a JSON array of paths', () => {
  const text = '["/work/docs/chapter1.adoc","/work/docs/sub/chapter 2.adoc"]'
  assert.deepEqual(map.parseFileTable(text), [
    '/work/docs/chapter1.adoc',
    '/work/docs/sub/chapter 2.adoc',
  ])
  assert.deepEqual(map.parseFileTable('[]'), [])
})

test('parseFileTable returns an empty table for malformed or missing input', () => {
  for (const bad of ['', '[', '{"a":1}', '"x"', 'null', '42', 'not json', undefined, null]) {
    assert.deepEqual(map.parseFileTable(bad), [], String(bad))
  }
})

test('srcForFile: main file, included file, file not shown', () => {
  const table = ['/work/a.adoc', '/work/b.adoc']
  assert.equal(map.srcForFile(table, '/work/main.adoc', '/work/main.adoc'), null)
  assert.equal(map.srcForFile(table, '/work/main.adoc', '/work/a.adoc'), 0)
  assert.equal(map.srcForFile(table, '/work/main.adoc', '/work/b.adoc'), 1)
  assert.equal(map.srcForFile(table, '/work/main.adoc', '/work/other.adoc'), undefined)
  assert.equal(map.srcForFile([], '/work/main.adoc', '/work/a.adoc'), undefined)
})

test('srcForFile uses the first index when the table lists a file twice', () => {
  assert.equal(map.srcForFile(['/w/x.adoc', '/w/y.adoc', '/w/x.adoc'], '/w/m.adoc', '/w/x.adoc'), 0)
})

test('srcForFile prefers the main file when it is also in the table', () => {
  assert.equal(map.srcForFile(['/w/m.adoc'], '/w/m.adoc', '/w/m.adoc'), null)
})
