package io.github.seflue.asciidoclsp

import com.google.gson.JsonObject
import com.intellij.openapi.actionSystem.ActionGroup
import com.intellij.openapi.actionSystem.ActionManager
import com.intellij.openapi.actionSystem.IdeActions
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.testFramework.PlatformTestUtil
import com.redhat.devtools.lsp4ij.LSPIJUtils
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.time.Duration
import java.util.concurrent.TimeUnit

/** adoc-0307: the preview between the editor and the server's page. */
class PreviewTest : AsciidocLspTestCase() {
    fun testPreviewActionsAreInTheEditorContextMenuOfAsciidocFiles() {
        openPhysical("doc.adoc", "= Title\n")
        val popup = ActionManager.getInstance().getAction(IdeActions.GROUP_EDITOR_POPUP) as ActionGroup
        val ids = listOf("io.github.seflue.asciidoclsp.preview.open", "io.github.seflue.asciidoclsp.preview.openInBrowser")

        for (id in ids) {
            val action = ActionManager.getInstance().getAction(id) ?: throw AssertionError("$id is not registered")
            assertTrue("$id is not enabled in an AsciiDoc editor", myFixture.testAction(action).isEnabledAndVisible)
        }
        val group = ActionManager.getInstance().getAction("io.github.seflue.asciidoclsp.preview") as ActionGroup
        assertTrue("the preview group is not in the editor popup", popup.getChildren(null).contains(group))
    }

    fun testOpenAsksTheServerForThePageUrl() {
        if (!asciidoctorAvailable()) {
            return
        }
        useBinaryFromEnvironment()
        val file = openPhysical("doc.adoc", "= Title\n\nText.\n")
        waitForServer()

        val url = Preview.getInstance(project).pageUrl(file)
        waitUntil("no preview URL") { url.isDone }

        val page = url.get(0, TimeUnit.SECONDS)
        assertTrue("unexpected URL $page", page.startsWith("http://127.0.0.1:"))
    }

    fun testOpenInBrowserBrowsesThePageUrl() {
        if (!asciidoctorAvailable()) {
            return
        }
        val opened = openInBrowser(openPhysical("doc.adoc", "= Title\n"))

        assertTrue("unexpected URL $opened", opened.single().startsWith("http://127.0.0.1:"))
    }

    /** Scrolling the page past a chapter start: the server sends revealLine with the cursor. */
    fun testScrollingThePageWithCursorMovesTheCaret() {
        if (!asciidoctorAvailable()) {
            return
        }
        val file = openPhysical("doc.adoc", longDocument())
        val page = openInBrowser(file).single()

        postPageAction(page, "scroll", file, line = 41, cursor = true)

        waitUntil("caret did not move to line 40") { myFixture.editor.caretModel.logicalPosition.line == 40 }
    }

    fun testRevealLineDoesNothingWhileThePreviewIsClosed() {
        val file = openPhysical("doc.adoc", longDocument())

        AsciidocLanguageClient(project).revealLine(revealLine(file, 40, cursor = true))
        PlatformTestUtil.dispatchAllEventsInIdeEventQueue()

        assertEquals(0, myFixture.editor.caretModel.logicalPosition.line)
    }

    /** A double-click in the page: the server sends window/showDocument with the source line. */
    fun testDoubleClickInThePagePutsTheCaretOnTheBlock() {
        if (!asciidoctorAvailable()) {
            return
        }
        val file = openPhysical("doc.adoc", longDocument())
        val page = openInBrowser(file).single()

        postPageAction(page, "click", file, line = 31)

        waitUntil("caret did not move to line 30") { myFixture.editor.caretModel.logicalPosition.line == 30 }
    }

    fun testOpenWithoutJcefOpensTheBrowserInstead() {
        if (!asciidoctorAvailable()) {
            return
        }
        useBinaryFromEnvironment()
        val notifications = collectNotifications()
        openPhysical("doc.adoc", "= Title\n")
        waitForServer()
        val opened = mutableListOf<String>()
        val preview = Preview.getInstance(project)
        preview.browse = { opened += it }
        preview.jcefSupported = { false }

        preview.openToSide(myFixture.editor)

        waitUntil("the page was not opened in the browser") { opened.isNotEmpty() }
        assertTrue(notifications.any { it.content.contains("opens in the browser") })
    }

    /** Opens the page of [file] in a stand-in browser; returns the URLs it was asked to open. */
    private fun openInBrowser(file: VirtualFile): List<String> {
        useBinaryFromEnvironment()
        waitForServer()
        val opened = mutableListOf<String>()
        val preview = Preview.getInstance(project)
        preview.browse = { opened += it }
        preview.openInBrowser(myFixture.editor)
        waitUntil("${file.name} was not opened in the browser") { opened.isNotEmpty() }
        return opened
    }

    /** POSTs a page action as the page does; retries until the page knows the file's lines. */
    private fun postPageAction(page: String, route: String, file: VirtualFile, line: Int, cursor: Boolean = false) {
        val origin = URI(page).let { "${it.scheme}://${it.authority}" }
        val body = JsonObject().apply {
            addProperty("file", file.path)
            addProperty("line", line)
            addProperty("cursor", cursor)
        }
        val request = HttpRequest.newBuilder(URI(page).resolve(route))
            .timeout(Duration.ofSeconds(5))
            .header("Origin", origin)
            .header("Content-Type", "application/json")
            .POST(HttpRequest.BodyPublishers.ofString(body.toString()))
            .build()
        val client = HttpClient.newHttpClient()
        var status = 0
        waitUntil("POST $route was refused: $status") {
            status = client.send(request, HttpResponse.BodyHandlers.discarding()).statusCode()
            status in 200..299
        }
    }

    /** Paragraphs of one line each, so every line starts a block of the page. */
    private fun longDocument() = "= Title\n\n" + (1..40).joinToString("\n\n") { "Line $it." } + "\n"

    private fun revealLine(file: VirtualFile, line: Int, cursor: Boolean) = JsonObject().apply {
        addProperty("uri", LSPIJUtils.toUriAsString(file))
        addProperty("line", line)
        addProperty("cursor", cursor)
    }
}
