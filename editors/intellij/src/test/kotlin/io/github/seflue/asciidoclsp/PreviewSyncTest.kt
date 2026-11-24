package io.github.seflue.asciidoclsp

import com.google.gson.JsonParser
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The decisions of the preview sync; the same cases as editors/vscode/test/previewSync.test.ts. */
class PreviewSyncTest {
    private val view = EditorView(uri = "file:///doc.adoc", lineCount = 100, top = 10, bottom = 40)

    @Test
    fun revealLineForAnotherDocumentDoesNothing() {
        assertEquals(RevealAction.None, revealAction(RevealLineParams("file:///other.adoc", 20), view))
    }

    @Test
    fun revealLineScrollsTheLineToTheTop() {
        assertEquals(RevealAction.Scroll(20), revealAction(RevealLineParams(view.uri, 20), view))
    }

    @Test
    fun revealLineWithinTheToleranceOfTheTopDoesNothing() {
        assertEquals(RevealAction.None, revealAction(RevealLineParams(view.uri, 9), view))
        assertEquals(RevealAction.None, revealAction(RevealLineParams(view.uri, 10), view))
        assertEquals(RevealAction.None, revealAction(RevealLineParams(view.uri, 11), view))
        assertEquals(RevealAction.Scroll(12), revealAction(RevealLineParams(view.uri, 12), view))
    }

    @Test
    fun revealLineWithoutCursorNeverMovesTheCursor() {
        assertEquals(RevealAction.Scroll(60), revealAction(RevealLineParams(view.uri, 60, cursor = false), view))
    }

    @Test
    fun revealLineWithCursorSetsTheCursorAndScrollsOnlyWhenTheLineIsOutOfSight() {
        assertEquals(RevealAction.Cursor(30, scroll = false), revealAction(RevealLineParams(view.uri, 30, cursor = true), view))
        assertEquals(RevealAction.Cursor(60, scroll = true), revealAction(RevealLineParams(view.uri, 60, cursor = true), view))
    }

    @Test
    fun revealLineWithCursorOnTheTopLineStillSetsTheCursor() {
        assertEquals(RevealAction.Cursor(10, scroll = false), revealAction(RevealLineParams(view.uri, 10, cursor = true), view))
    }

    @Test
    fun revealLinePastTheEndLandsOnTheLastLine() {
        assertEquals(RevealAction.Cursor(99, scroll = true), revealAction(RevealLineParams(view.uri, 500, cursor = true), view))
    }

    @Test
    fun revealLineInAnEmptyDocumentDoesNothing() {
        assertEquals(RevealAction.None, revealAction(RevealLineParams(view.uri, 0, cursor = true), view.copy(lineCount = 0)))
    }

    @Test
    fun revealLineParamsAreChecked() {
        fun parse(json: String) = revealLineParams(JsonParser.parseString(json))
        assertEquals(RevealLineParams("file:///a.adoc", 3), parse("""{"uri": "file:///a.adoc", "line": 3}"""))
        assertEquals(RevealLineParams("file:///a.adoc", 3, cursor = true), parse("""{"uri": "file:///a.adoc", "line": 3, "cursor": true}"""))
        assertNull(parse("""{"uri": "file:///a.adoc", "line": 1.5}"""))
        assertNull(parse("""{"uri": "file:///a.adoc", "line": "3"}"""))
        assertNull(parse("""{"uri": "file:///a.adoc", "line": 3, "cursor": "yes"}"""))
        assertNull(parse("""{"uri": "file:///a.adoc", "line": 3, "cursor": null}"""))
        assertNull(parse("""{"line": 3}"""))
        assertNull(parse("null"))
    }

    @Test
    fun theCursorARevealLineForcedIsAnEchoUntilItMovesOrTheTimeIsUp() {
        var now = 1000L
        val guard = EchoGuard { now }
        guard.record(view.uri, 60)
        assertTrue(guard.isEcho(view.uri, 60))
        assertTrue("further events at the same place", guard.isEcho(view.uri, 60))
        now += ECHO_MS + 1
        assertFalse("time is up", guard.isEcho(view.uri, 60))
    }

    @Test
    fun aCursorThatMovedOnIsNoEchoNorLaterBackAtTheForcedLine() {
        val guard = EchoGuard { 0 }
        guard.record(view.uri, 60)
        assertFalse(guard.isEcho(view.uri, 61))
        assertFalse(guard.isEcho(view.uri, 60))
    }

    @Test
    fun anEchoIsBoundToItsDocument() {
        val guard = EchoGuard { 0 }
        guard.record(view.uri, 60)
        assertFalse(guard.isEcho("file:///book/ch2.adoc", 60))
    }

    /** LSP4J hands an executeCommand result over as a map. */
    @Test
    fun thePreviewUrlIsReadFromTheCommandResult() {
        assertEquals("http://127.0.0.1:4711/abc/", previewUrl(mapOf("url" to "http://127.0.0.1:4711/abc/")))
        assertNull(previewUrl(mapOf("url" to 3)))
        assertNull(previewUrl(null))
        assertNull(previewUrl("http://127.0.0.1:4711/abc/"))
    }
}
