// Tests for the pure functions of assets/preview/page-sync.js.
// Run with `just test-js` (Node's built-in runner, no npm packages).
//
// Loaded like scroll-map.test.js: as a plain script in a context without
// `module`, so it publishes `AdocPageSync` on the global. Not covered: the
// DOM wiring in page.js (event listeners, timers, fetch).

'use strict'

const test = require('node:test')
const strict = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')

const SCRIPT = path.join(__dirname, '..', '..', 'assets', 'preview', 'page-sync.js')

const context = vm.createContext({})
vm.runInContext(fs.readFileSync(SCRIPT, 'utf8'), context, { filename: SCRIPT })
const sync = context.AdocPageSync

const assert = Object.assign(Object.create(strict), {
  deepEqual: (actual, expected, ...message) =>
    strict.deepEqual(JSON.parse(JSON.stringify(actual)), expected, ...message),
})

const MAIN = '/book/main.adoc'
const CHAPTER = '/book/chapter.adoc'
const TABLE = [CHAPTER]

// --- scroll gate -----------------------------------------------------------

function gate() {
  return sync.createScrollGate({ inputWindow: 600, echoWindow: 250, throttle: 100 })
}

test('a scroll without a user input before it is not sent', () => {
  const g = gate()
  assert.equal(g.onScroll(1000), 'drop')
})

test('a scroll right after a user input is sent', () => {
  const g = gate()
  g.userInput(1000)
  assert.equal(g.onScroll(1010), 'send')
})

test('a scroll long after the last user input is not sent', () => {
  const g = gate()
  g.userInput(1000)
  assert.equal(g.onScroll(1700), 'drop')
})

test('a held pointer (scrollbar drag) counts as input until it is released', () => {
  const g = gate()
  g.pointerDown(1000)
  assert.equal(g.onScroll(3000), 'send')
  g.pointerUp(3100)
  assert.equal(g.onScroll(3900), 'drop')
})

test('the page scrolling itself is never sent, even during user input', () => {
  const g = gate()
  g.userInput(1000)
  g.ownScroll(1010)
  assert.equal(g.onScroll(1020), 'drop')
  assert.equal(g.onScroll(1200), 'drop')
  assert.equal(g.onScroll(1270), 'send')
})

test('a reader scroll shortly after a cursor event waits for the echo window, then goes out', () => {
  const g = gate()
  g.cursorEvent(1000)
  g.userInput(1010)
  assert.equal(g.onScroll(1100), 'later')
  assert.equal(g.delay(1100), 150)
  assert.equal(g.onTimer(1250), 'send')
})

test('a reader input after the page followed the cursor is caught up after the window', () => {
  // G in the editor: the page follows at once, then the reader wheels.
  const g = gate()
  g.cursorEvent(1000)
  g.ownScroll(1000)
  assert.equal(g.onScroll(1005), 'drop', 'the follow scroll itself')
  g.userInput(1100)
  assert.equal(g.onScroll(1110), 'later')
  assert.equal(g.delay(1110), 140)
  assert.equal(g.onTimer(1250), 'send')
})

test('a focused-page scroll without input inside the echo window is dropped, not caught up', () => {
  // The page's own follow scroll while it has the focus must never go out.
  const g = gate()
  g.ownScroll(1000)
  assert.equal(g.onScroll(1010, true), 'drop')
  assert.equal(g.onTimer(1300), 'drop')
})

test('sends are throttled, and the last position follows after the window', () => {
  const g = gate()
  g.userInput(1000)
  assert.equal(g.onScroll(1000), 'send')
  assert.equal(g.onScroll(1030), 'later')
  assert.equal(g.onScroll(1060), 'later')
  assert.equal(g.delay(1060), 40)
  assert.equal(g.onTimer(1100), 'send')
  assert.equal(g.onTimer(1100), 'drop', 'one trailing send per window')
})

test('a trailing send is dropped if the editor moved in the meantime', () => {
  const g = gate()
  g.userInput(1000)
  g.onScroll(1000)
  g.onScroll(1050)
  g.cursorEvent(1080)
  assert.equal(g.onTimer(1100), 'drop')
})

test('the page follows no cursor shortly after it sent a scroll, and says when it will again', () => {
  const g = gate()
  assert.equal(g.followsCursor(1000), true)
  assert.equal(g.followDelay(1000), 0)
  g.userInput(1000)
  g.onScroll(1000)
  assert.equal(g.followsCursor(1100), false)
  assert.equal(g.followDelay(1100), 150)
  assert.equal(g.followsCursor(1260), true)
})

// --- files and anchors -------------------------------------------------------

test('a source index names the shown file or a file table entry', () => {
  assert.equal(sync.fileOfSrc(TABLE, MAIN, null), MAIN)
  assert.equal(sync.fileOfSrc(TABLE, MAIN, 0), CHAPTER)
  assert.equal(sync.fileOfSrc(TABLE, MAIN, 5), null)
})

test('the top anchor is the last one that starts at or above the viewport top', () => {
  const entries = [
    { line: 1, top: -900, bottom: 400 }, // section of the main file
    { line: 3, src: 0, top: -300, bottom: -10 },
    { line: 7, src: 0, top: -5, bottom: 120 },
    { line: 9, top: 130, bottom: 200 },
  ]
  assert.deepEqual(sync.topAnchor(entries, 0), { line: 7, src: 0, top: -5, bottom: 120 })
})

test('without an anchor above the viewport top, the header of the shown file is on top', () => {
  // The document title and header carry no anchor.
  const entries = [{ line: 4, src: 0, top: 50, bottom: 80 }]
  assert.deepEqual(sync.topAnchor(entries, 0), { line: 1 })
  assert.deepEqual(sync.topAnchor([], 0), { line: 1 })
})

test('included content stands for the include directive it comes through', () => {
  // main includes 0 at line 7 and 1 at line 9; 0 includes 2 at line 4.
  const includes = [
    { parent: null, line: 7, file: 0 },
    { parent: null, line: 9, file: 1 },
    { parent: 0, line: 4, file: 2 },
  ]
  assert.equal(sync.includeLineFor(includes, null, 0), 7)
  assert.equal(sync.includeLineFor(includes, null, 2), 7, 'through a nested include')
  assert.equal(sync.includeLineFor(includes, 0, 2), 4)
  assert.equal(sync.includeLineFor(includes, null, 1), 9)
  assert.equal(sync.includeLineFor(includes, 1, 2), null, 'not included there')
  const twice = includes.concat([{ parent: null, line: 11, file: 0 }])
  assert.equal(sync.includeLineFor(twice, null, 0), null, 'ambiguous: included twice')
})

test('a double-click target names its file and line', () => {
  assert.deepEqual(sync.clickTarget({ line: 12 }, TABLE, MAIN), { file: MAIN, line: 12 })
  assert.deepEqual(sync.clickTarget({ line: 3, src: 0 }, TABLE, MAIN), {
    file: CHAPTER,
    line: 3,
  })
  assert.equal(sync.clickTarget(null, TABLE, MAIN), null)
  assert.equal(sync.clickTarget({ line: 3, src: 4 }, TABLE, MAIN), null)
})

test('the banner names the top file when it is not the active one', () => {
  const top = { line: 3, src: 0 }
  assert.equal(sync.bannerFile(top, TABLE, MAIN, MAIN), CHAPTER)
  assert.equal(sync.bannerFile(top, TABLE, MAIN, CHAPTER), null)
  assert.equal(sync.bannerFile({ line: 2 }, TABLE, MAIN, MAIN), null)
  assert.equal(sync.bannerFile({ line: 2 }, TABLE, MAIN, CHAPTER), MAIN)
  assert.equal(sync.bannerFile(null, TABLE, MAIN, MAIN), null)
})

test('the banner shows the file name, not the path', () => {
  assert.equal(sync.baseName('/book/chapters/chapter-03.adoc'), 'chapter-03.adoc')
  assert.equal(sync.baseName('C:\\book\\ch.adoc'), 'ch.adoc')
})

// --- end of the page -----------------------------------------------------------

test('the page is at its end when the viewport reaches the bottom', () => {
  assert.equal(sync.atEnd(1200, 800, 2000), true)
  assert.equal(sync.atEnd(1199, 800, 2000), true, 'a pixel of rounding')
  assert.equal(sync.atEnd(1100, 800, 2000), false)
  assert.equal(sync.atEnd(0, 800, 600), false, 'a page that does not scroll has no end to report')
})

// --- reading line --------------------------------------------------------------

test('the page reads its position on the reading line, a quarter down', () => {
  assert.deepEqual(sync.readPoint(500, 800, 4000), { y: 200, cursor: false })
})

test('at its top the page reads the document start and moves the cursor there', () => {
  assert.deepEqual(sync.readPoint(0, 800, 4000), { y: 0, cursor: true })
})

test('at its end the page reads the document end and moves the cursor there', () => {
  assert.deepEqual(sync.readPoint(3200, 800, 4000), { y: 800, cursor: true })
})

test('an include directive line is one of the file\'s own include table entries', () => {
  const includes = [
    { parent: null, line: 10, file: 0 },
    { parent: 0, line: 4, file: 1 },
  ]
  assert.equal(sync.isIncludeLine(includes, null, 10), true)
  assert.equal(sync.isIncludeLine(includes, null, 4), false)
  assert.equal(sync.isIncludeLine(includes, 0, 4), true)
  assert.equal(sync.isIncludeLine(includes, 0, 10), false)
})

// --- following the cursor --------------------------------------------------------

test('a cursor on the last line takes the page to its end', () => {
  assert.equal(sync.followAction({ line: 20, lastLine: 20, include: false, targetY: 300, viewportHeight: 800 }), 'end')
})

test('a cursor on an include directive aligns its chapter unless it starts above the reading line', () => {
  const at = (targetY) =>
    sync.followAction({ line: 12, lastLine: 20, include: true, targetY: targetY, viewportHeight: 800 })
  assert.equal(at(0), 'none')
  assert.equal(at(200), 'none')
  assert.equal(at(201), 'align')
  assert.equal(at(-40), 'align')
  assert.equal(at(900), 'align')
})

test('a cursor on another line keeps the dead band of the cursor follow', () => {
  const at = (targetY) =>
    sync.followAction({ line: 7, lastLine: 20, include: false, targetY: targetY, viewportHeight: 800 })
  assert.equal(at(400), 'none')
  assert.equal(at(200), 'none', 'on the reading line, where the page reads it')
  assert.equal(at(100), 'align')
  assert.equal(at(900), 'align')
})

// --- round trip over chapter boundaries -----------------------------------------

{
  const MAP = path.join(__dirname, '..', '..', 'assets', 'preview', 'scroll-map.js')
  vm.runInContext(fs.readFileSync(MAP, 'utf8'), context, { filename: MAP })
  const map = context.AdocScrollMap

  // An overview file of 13 lines: preamble on line 5, chapters included on
  // lines 7, 9 and 11 (file 0, 1, 2), each rendered 1000 px high, one block
  // per 100 px, chapter 0 starting at 200. Page coordinates (scrollY 0).
  const includes = [7, 9, 11].map((line, file) => ({ parent: null, line: line, file: file }))
  const pageEntries = [{ line: 5, top: 100, bottom: 200 }]
  for (let file = 0; file < 3; file++) {
    for (let block = 0; block < 10; block++) {
      const top = 200 + file * 1000 + block * 100
      pageEntries.push({ line: 1 + block * 3, src: file, top: top, bottom: top + 100 })
    }
  }
  const LINES = 13
  const VIEWPORT = 800
  const PAGE_HEIGHT = 3300
  const anchorsAt = (scrollY) =>
    map.withSentinel(
      map.withPageTop(
        map.fileAnchorsWithIncludes(
          pageEntries.map((e) => Object.assign({}, e, { top: e.top - scrollY, bottom: e.bottom - scrollY })),
          includes,
          null
        ),
        scrollY
      ),
      LINES,
      PAGE_HEIGHT - scrollY
    )
  const reportAt = (scrollY) => {
    const point = sync.readPoint(scrollY, VIEWPORT, PAGE_HEIGHT)
    return Math.max(1, Math.floor(map.sourceLineAtViewportTop(anchorsAt(scrollY), point.y)))
  }

  test('a cursor on an include directive shows its chapter, and the page reports it back', () => {
    // The last chapter cannot reach the top: the page ends first. Its end is
    // covered by the last line, see below.
    for (const line of [7, 9]) {
      const scrollY = map.scrollTopForLine(anchorsAt(0), 0, line)
      assert.equal(reportAt(scrollY), line, `chapter at line ${line}`)
    }
  })

  test('a chapter becomes current when its start crosses the reading line, both ways', () => {
    // Chapter 1 (line 9) starts at 1200; the reading line is 200 below scrollY.
    assert.equal(reportAt(999), 7)
    assert.equal(reportAt(1000), 9)
    assert.equal(reportAt(1001), 9)
    assert.equal(reportAt(999), 7, 'scrolling back up')
  })

  test('the top and the end of the page report the first and last line', () => {
    assert.equal(reportAt(0), 1)
    assert.equal(reportAt(PAGE_HEIGHT - VIEWPORT), LINES)
  })
}

// --- focus ---------------------------------------------------------------------

test('a scroll while the page has the focus counts as the reader\'s, without an input event', () => {
  // A key handler of a browser extension (Surfingkeys j/k) swallows the key
  // and scrolls by script: the page sees the scroll only.
  const g = gate()
  assert.equal(g.onScroll(1000, true), 'send')
  assert.equal(g.onScroll(2000, false), 'drop', 'without the focus it needs an input event')
})

test('the focus does not lift the echo window or the page\'s own scroll', () => {
  const g = gate()
  g.cursorEvent(1000)
  assert.equal(g.onScroll(1100, true), 'drop')
  g.ownScroll(2000)
  assert.equal(g.onScroll(2100, true), 'drop')
})

// --- edges of a chapter --------------------------------------------------------

test('a reading point outside the editor\'s chapter lies above or below it', () => {
  // Main preamble, chapter 0 (which includes file 2), chapter 1.
  const entries = [
    { line: 5, top: 0 },
    { line: 1, src: 0, top: 100 },
    { line: 1, src: 2, top: 200 },
    { line: 9, src: 0, top: 300 },
    { line: 1, src: 1, top: 400 },
  ]
  const includes = [
    { parent: null, line: 7, file: 0 },
    { parent: 0, line: 4, file: 2 },
    { parent: null, line: 9, file: 1 },
  ]
  assert.equal(sync.chapterEdge(entries, includes, 0, 50), 'above')
  assert.equal(sync.chapterEdge(entries, includes, 0, 150), null)
  assert.equal(sync.chapterEdge(entries, includes, 0, 250), null, 'its own include')
  assert.equal(sync.chapterEdge(entries, includes, 0, 450), 'below')
  assert.equal(sync.chapterEdge(entries, includes, 1, 350), 'above')
  assert.equal(sync.chapterEdge(entries, includes, 5, 350), null, 'a file the page does not show')
})

// --- who leads ---------------------------------------------------------------------

test('after the page posted a report, it leads until the editor reports a cursor of its own', () => {
  // The editor reports no move it was made to do, so the last cursor the
  // page knows is stale while it leads: following it would jump back.
  const g = gate()
  assert.equal(g.pageLeads(), false)
  g.userInput(1000)
  assert.equal(g.onScroll(1000), 'send')
  assert.equal(g.pageLeads(), false, 'a send the page did not post does not lead')
  g.reported()
  assert.equal(g.pageLeads(), true)
  g.cursorEvent(2000)
  assert.equal(g.pageLeads(), false)
})
