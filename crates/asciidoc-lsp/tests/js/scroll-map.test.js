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
    'parseFileTable', 'bodyClassName', 'pageTitle', 'srcForFile', 'withSentinel', 'withPageTop', 'anchorIndicesForLine',
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

// A parent with one own block (line 7) and three consecutive includes on
// lines 10, 12 and 14. Their content has the lines of the included files.
const PARENT_ENTRIES = [
  { line: 7, top: 0, bottom: 50 },
  { line: 1, src: 0, top: 50, bottom: 100 },
  { line: 3, src: 0, top: 100, bottom: 150 },
  { line: 1, src: 1, top: 150, bottom: 250 },
  { line: 1, src: 2, top: 250, bottom: 300 },
]
const PARENT_INCLUDES = [
  { parent: null, line: 10, file: 0 },
  { parent: null, line: 12, file: 1 },
  { parent: null, line: 14, file: 2 },
]

function scrollAt(anchors, line) {
  return map.scrollTopForLine(anchors, 0, line)
}

test('cursor on an include line maps to the start of its content', () => {
  const anchors = map.fileAnchorsWithIncludes(PARENT_ENTRIES, PARENT_INCLUDES, null)
  assert.equal(scrollAt(anchors, 10), 50)
  assert.equal(scrollAt(anchors, 12), 150)
  assert.equal(scrollAt(anchors, 14), 250)
})

test('the line after an include maps to the end of its content', () => {
  const anchors = map.fileAnchorsWithIncludes(PARENT_ENTRIES, PARENT_INCLUDES, null)
  assert.equal(scrollAt(anchors, 11), 150)
  assert.equal(scrollAt(anchors, 13), 250)
  assert.equal(scrollAt(anchors, 15), 300)
})

test('a line between two includes interpolates inside the gap only', () => {
  const entries = [
    { line: 7, top: 0, bottom: 50 },
    { line: 1, src: 0, top: 50, bottom: 100 },
    { line: 1, src: 1, top: 140, bottom: 200 },
  ]
  const includes = [
    { parent: null, line: 10, file: 0 },
    { parent: null, line: 14, file: 1 },
  ]
  const anchors = map.fileAnchorsWithIncludes(entries, includes, null)
  assert.equal(scrollAt(anchors, 10), 50)
  assert.equal(scrollAt(anchors, 11), 100)
  assert.equal(scrollAt(anchors, 12.5), 120)
  assert.equal(scrollAt(anchors, 14), 140)
})

test('an own anchor right after an include is kept, not replaced by its end', () => {
  const entries = [
    { line: 7, top: 0, bottom: 50 },
    { line: 1, src: 0, top: 50, bottom: 100 },
    { line: 11, top: 110, bottom: 150 },
  ]
  const anchors = map.fileAnchorsWithIncludes(entries, [{ parent: null, line: 10, file: 0 }], null)
  assert.equal(scrollAt(anchors, 11), 110)
})

test('a nested include counts into the content of its parent include', () => {
  const entries = [
    { line: 1, top: 0, bottom: 40 },
    { line: 1, src: 0, top: 40, bottom: 80 },
    { line: 1, src: 1, top: 80, bottom: 140 },
    { line: 5, src: 0, top: 140, bottom: 200 },
    { line: 9, top: 200, bottom: 240 },
  ]
  const includes = [
    { parent: null, line: 3, file: 0 },
    { parent: 0, line: 3, file: 1 },
  ]
  const main = map.fileAnchorsWithIncludes(entries, includes, null)
  assert.equal(scrollAt(main, 3), 40)
  assert.equal(scrollAt(main, 4), 200)
  // In the child file the nested include has its own start and end.
  const child = map.fileAnchorsWithIncludes(entries, includes, 0)
  assert.equal(scrollAt(child, 1), 40)
  assert.equal(scrollAt(child, 3), 80)
  assert.equal(scrollAt(child, 4), 140)
  assert.equal(scrollAt(child, 5), 140)
})

test('an include that rendered nothing adds no anchor', () => {
  const entries = [
    { line: 7, top: 0, bottom: 50 },
    { line: 1, src: 1, top: 50, bottom: 100 },
  ]
  const includes = [
    { parent: null, line: 10, file: 0 },
    { parent: null, line: 12, file: 1 },
  ]
  const anchors = map.fileAnchorsWithIncludes(entries, includes, null)
  assert.deepEqual(anchors.map((a) => a.line), [7, 12, 13])
})

test('the same file included twice in a row gets one extent per include', () => {
  const entries = [
    { line: 1, src: 0, top: 0, bottom: 50 },
    { line: 2, src: 0, top: 50, bottom: 100 },
    { line: 1, src: 0, top: 100, bottom: 150 },
    { line: 2, src: 0, top: 150, bottom: 200 },
  ]
  const includes = [
    { parent: null, line: 4, file: 0 },
    { parent: null, line: 6, file: 0 },
  ]
  const anchors = map.fileAnchorsWithIncludes(entries, includes, null)
  assert.equal(scrollAt(anchors, 4), 0)
  assert.equal(scrollAt(anchors, 5), 100)
  assert.equal(scrollAt(anchors, 6), 100)
  assert.equal(scrollAt(anchors, 7), 200)
})

test('without includes the anchors are those of fileAnchors', () => {
  const entries = [
    { line: 1, top: 0, bottom: 20 },
    { line: 3, top: 30, bottom: 60 },
    { line: 2, src: 0, top: 60, bottom: 90 },
  ]
  for (const src of [null, 0]) {
    assert.deepEqual(
      map.fileAnchorsWithIncludes(entries, [], src),
      JSON.parse(JSON.stringify(map.fileAnchors(entries, src)))
    )
  }
})

test('parseIncludeTable reads the include list and drops malformed input', () => {
  assert.deepEqual(
    map.parseIncludeTable('[{"parent":null,"line":3,"file":0},{"parent":0,"line":3,"file":1}]'),
    [{ parent: null, line: 3, file: 0 }, { parent: 0, line: 3, file: 1 }]
  )
  assert.deepEqual(map.parseIncludeTable('[1,{"parent":"x","line":1,"file":0}]'), [])
  for (const bad of ['', '[', '{}', 'null', undefined]) {
    assert.deepEqual(map.parseIncludeTable(bad), [], String(bad))
  }
})

function assertAscending(anchors) {
  for (let i = 1; i < anchors.length; i++) {
    assert.ok(anchors[i].line > anchors[i - 1].line, `line at ${i}`)
    assert.ok(anchors[i].top >= anchors[i - 1].top, `top at ${i}`)
  }
}

// docs/index.adoc: chapter 1 (file 0) on line 10, chapter 4 (file 1) with
// leveloffset=+1 on line 12, chapter 5 (file 2) on line 14. Chapter 4 becomes
// a sect2 and sits inside chapter 1's sect1, whose bottom lies below it.
const NESTED_BOXES = [
  { line: 7, top: 0, bottom: 50 },
  { line: 1, src: 0, top: 50, bottom: 600 },
  { line: 3, src: 0, top: 100, bottom: 200 },
  { line: 9, src: 0, top: 200, bottom: 300 },
  { line: 1, src: 1, top: 400, bottom: 580 },
  { line: 3, src: 1, top: 450, bottom: 500 },
  { line: 1, src: 2, top: 600, bottom: 900 },
  { line: 3, src: 2, top: 650, bottom: 700 },
]

test('a section container enclosing the next include keeps the anchors ascending', () => {
  const anchors = map.fileAnchorsWithIncludes(NESTED_BOXES, PARENT_INCLUDES, null)
  assertAscending(anchors)
  assert.equal(scrollAt(anchors, 10), 50)
  assert.equal(scrollAt(anchors, 11), 400)
  assert.equal(scrollAt(anchors, 12), 400)
  assert.equal(scrollAt(anchors, 13), 580)
  assert.equal(scrollAt(anchors, 14), 600)
})

test('line to scroll to line round trip on nested includes', () => {
  const anchors = map.fileAnchorsWithIncludes(NESTED_BOXES, PARENT_INCLUDES, null)
  for (const line of [7, 10, 12, 14]) {
    const top = scrollAt(anchors, line)
    assert.equal(map.sourceLineAtViewportTop(anchors, top), line, String(line))
  }
})

test('an include anchor out of order against its neighbours is dropped', () => {
  const entries = [
    { line: 7, top: 100, bottom: 150 },
    { line: 1, src: 0, top: 50, bottom: 120 },
  ]
  const includes = [{ parent: null, line: 10, file: 0 }]
  const anchors = map.fileAnchorsWithIncludes(entries, includes, null)
  assertAscending(anchors)
  assert.deepEqual(anchors.map((a) => a.line), [7, 11])
})

test('a file shown verbatim first and rendered later is not taken by the first include', () => {
  const entries = [
    { line: 5, top: 0, bottom: 50 },
    { line: 7, top: 50, bottom: 100 },
    { line: 10, top: 100, bottom: 150 },
    { line: 1, src: 0, top: 150, bottom: 200 },
    { line: 2, src: 0, top: 200, bottom: 250 },
  ]
  const includes = [
    { parent: null, line: 8, file: 0 },
    { parent: null, line: 12, file: 0 },
  ]
  const anchors = map.fileAnchorsWithIncludes(entries, includes, null)
  assertAscending(anchors)
  assert.deepEqual(anchors.map((a) => a.line), [5, 7, 10, 12, 13])
  assert.equal(scrollAt(anchors, 12), 150)
  assert.equal(scrollAt(anchors, 13), 250)
})

test('nesting is per include: a direct include of a nested file is not swallowed', () => {
  // main includes A (line 10), A includes B on its line 3, main includes B
  // directly (line 12).
  const entries = [
    { line: 1, src: 0, top: 0, bottom: 50 },
    { line: 1, src: 1, top: 50, bottom: 100 },
    { line: 5, src: 0, top: 100, bottom: 150 },
    { line: 1, src: 1, top: 150, bottom: 200 },
  ]
  const includes = [
    { parent: null, line: 10, file: 0 },
    { parent: 0, line: 3, file: 1 },
    { parent: null, line: 12, file: 1 },
  ]
  const anchors = map.fileAnchorsWithIncludes(entries, includes, null)
  assertAscending(anchors)
  assert.equal(scrollAt(anchors, 10), 0)
  assert.equal(scrollAt(anchors, 11), 150)
  assert.equal(scrollAt(anchors, 12), 150)
  assert.equal(scrollAt(anchors, 13), 200)
})

test('two direct includes where the first includes nothing keep separate extents', () => {
  const entries = [
    { line: 1, src: 0, top: 0, bottom: 50 },
    { line: 1, src: 1, top: 50, bottom: 100 },
  ]
  const includes = [
    { parent: null, line: 10, file: 0 },
    { parent: null, line: 12, file: 1 },
  ]
  const anchors = map.fileAnchorsWithIncludes(entries, includes, null)
  assert.equal(scrollAt(anchors, 10), 0)
  assert.equal(scrollAt(anchors, 12), 50)
  assert.equal(scrollAt(anchors, 13), 100)
})

test('bodyClassName adds the listed classes to the base class', () => {
  assert.equal(map.bodyClassName('article', '["toc2","toc-left"]'), 'article toc2 toc-left')
  assert.equal(map.bodyClassName('article', '[]'), 'article')
})

test('bodyClassName drops what is not a class token', () => {
  assert.equal(map.bodyClassName('article', '["a b",1,"","ok"]'), 'article ok')
  assert.equal(map.bodyClassName('article', '{"a":1}'), 'article')
  assert.equal(map.bodyClassName('article', 'not json'), 'article')
})

// A titled document: header (title, inline toc) on lines 1-2 carries no
// anchor, the first block is at line 3 and lies 80px below the page top.
const titled = [
  { line: 3, top: 80 },
  { line: 7, top: 280 },
]

test('withPageTop puts line 1 at the top of the page above the first block', () => {
  assert.deepEqual(map.withPageTop(titled, 0), [{ line: 1, top: 0 }, ...titled])
  assert.deepEqual(map.withPageTop(titled, 150), [{ line: 1, top: -150 }, ...titled])
})

test('withPageTop leaves anchors that start at line 1, and none, alone', () => {
  assert.deepEqual(map.withPageTop(anchors, 40), anchors)
  assert.deepEqual(map.withPageTop([], 40), [])
})

test('a cursor in the header scrolls to the page top, not to the first block', () => {
  const page = map.withPageTop(titled, 0)
  assert.equal(map.scrollTopForLine(page, 0, 1), 0)
  assert.equal(map.scrollTopForLine(page, 0, 2), 40)
  // Scrolled down by 300: the same page, client tops shifted.
  const scrolled = map.withPageTop(titled.map((a) => ({ line: a.line, top: a.top - 300 })), 300)
  assert.equal(map.scrollTopForLine(scrolled, 300, 1), 0)
})

test('a page scrolled to the top stays at the top when it is pinned', () => {
  const page = map.withPageTop(titled, 0)
  const line = map.sourceLineAtViewportTop(page, 0)
  assert.equal(line, 1)
  assert.equal(map.scrollTopForLine(page, 0, line), 0)
})

test('pageTitle puts the document title before the product name', () => {
  assert.equal(map.pageTitle('"My Doc"'), 'My Doc – AsciiDoc Preview')
})

test('pageTitle falls back to the product name', () => {
  assert.equal(map.pageTitle('""'), 'AsciiDoc Preview')
  assert.equal(map.pageTitle('[]'), 'AsciiDoc Preview')
  assert.equal(map.pageTitle('not json'), 'AsciiDoc Preview')
})
