package io.github.seflue.asciidoclsp

import com.google.gson.JsonObject
import com.intellij.ide.BrowserUtil
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.editor.EditorFactory
import com.intellij.openapi.editor.LogicalPosition
import com.intellij.openapi.editor.event.CaretEvent
import com.intellij.openapi.editor.event.CaretListener
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.fileEditor.FileEditorManagerEvent
import com.intellij.openapi.fileEditor.FileEditorManagerListener
import com.intellij.openapi.fileEditor.TextEditor
import com.intellij.openapi.fileTypes.LanguageFileType
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowManager
import com.intellij.ui.content.ContentFactory
import com.intellij.ui.jcef.JBCefApp
import com.intellij.ui.jcef.JBCefBrowser
import com.intellij.util.Alarm
import com.redhat.devtools.lsp4ij.LSPIJUtils
import com.redhat.devtools.lsp4ij.LanguageServerManager
import org.eclipse.lsp4j.ExecuteCommandParams
import org.eclipse.lsp4j.services.LanguageServer
import java.awt.Point
import java.util.concurrent.CompletableFuture

private const val OPEN_COMMAND = "asciidoc.preview.open"

/** Interval of the trailing throttle on cursorMoved. */
private const val THROTTLE_MS = 100

/** The live preview of asciidoc-lsp: the server's page in a JCEF tool window or the browser. */
@Service(Service.Level.PROJECT)
class Preview(private val project: Project) : Disposable {
    /** The page was opened in the browser; it follows the editor until the server restarts. */
    private var inBrowser = false

    /** The page in the tool window, once it was opened there. */
    private var browser: JBCefBrowser? = null
    private val echo = EchoGuard()

    /** Opens a URL in the browser; tests replace it. */
    internal var browse: (String) -> Unit = BrowserUtil::browse

    /** Whether this IDE runs with JCEF; tests replace it. */
    internal var jcefSupported: () -> Boolean = JBCefApp::isSupported

    /**
     * The asciidoc editor the reader last worked in. While the tool window has the focus,
     * the page still follows this one.
     */
    private var editor: Editor? = null

    /**
     * Trailing throttle: caret changes fire per keystroke. A pending request is left alone,
     * so continuous movement still sends once per interval; [sendCursor] reads the
     * position when it runs, so the last one wins.
     */
    private val throttle = Alarm(Alarm.ThreadToUse.SWING_THREAD, this)

    init {
        EditorFactory.getInstance().eventMulticaster.addCaretListener(
            object : CaretListener {
                override fun caretPositionChanged(event: CaretEvent) = caretMoved(event.editor, event.newPosition.line)
            },
            this,
        )
        project.messageBus.connect(this).subscribe(
            FileEditorManagerListener.FILE_EDITOR_MANAGER,
            object : FileEditorManagerListener {
                override fun selectionChanged(event: FileEditorManagerEvent) {
                    val selected = (event.newEditor as? TextEditor)?.editor ?: return
                    if (!isAsciidoc(selected)) {
                        return
                    }
                    editor = selected
                    if (following()) {
                        schedule()
                    }
                }
            },
        )
    }

    private fun caretMoved(moved: Editor, line: Int) {
        if (moved.project != project || !following() || !isAsciidoc(moved)) {
            return
        }
        val uri = uriOf(moved) ?: return
        if (echo.isEcho(uri, line)) {
            return
        }
        editor = moved
        schedule()
    }

    private fun schedule() {
        if (throttle.isEmpty) {
            throttle.addRequest(::sendCursor, THROTTLE_MS)
        }
    }

    /** Tell the server the cursor line of the editor the page follows. */
    private fun sendCursor() {
        val editor = editor?.takeUnless { it.isDisposed } ?: return
        if (!following()) {
            return
        }
        val params = CursorMovedParams(uriOf(editor) ?: return, editor.caretModel.logicalPosition.line)
        // Without a running server there is nobody to tell.
        server().thenAccept { (it as? AsciidocLanguageServerApi)?.cursorMoved(params) }
    }

    override fun dispose() = Unit

    /** The page of [editor] in the tool window; without JCEF in the browser instead. EDT only. */
    fun openToSide(editor: Editor) {
        if (!jcefSupported()) {
            notifyInfo(project, "AsciiDoc Preview: this IDE runs without JCEF, so the preview opens in the browser.")
            openInBrowser(editor)
            return
        }
        open(editor) { url ->
            load(url)
            toolWindow()?.show()
        }
    }

    /** The page of [editor] in the browser. EDT only. */
    fun openInBrowser(editor: Editor) {
        open(editor) { url ->
            browse(url)
            inBrowser = true
        }
    }

    /**
     * After a server start: the tool window's page gets the new server's URL and stays
     * shown or hidden as it was. Any thread.
     */
    fun serverStarted() {
        ApplicationManager.getApplication().invokeLater({
            // A page in the browser still points at the old server; it is opened anew.
            inBrowser = false
            val editor = editor?.takeUnless { it.isDisposed }
            if (browser != null && editor != null) {
                open(editor, ::load)
            }
        }, project.disposed)
    }

    /**
     * Whether the page follows the editor: it was opened in the tool window, which keeps
     * it while hidden, or in the browser.
     */
    private fun following(): Boolean = inBrowser || browser != null

    /** Ask the server for the page of [editor]'s file, [display] it, and follow [editor]. */
    private fun open(editor: Editor, display: (String) -> Unit) {
        val file = editor.virtualFile ?: return
        this.editor = editor
        pageUrl(file).whenComplete { url, error ->
            ApplicationManager.getApplication().invokeLater({
                if (error != null) {
                    val cause = error.cause ?: error
                    notifyError(project, "AsciiDoc Preview: ${cause.message ?: cause.javaClass.simpleName}")
                } else {
                    display(url)
                    sendCursor()
                }
            }, project.disposed)
        }
    }

    /** Loads [url] into the tool window's page; makes the window available on first use. */
    private fun load(url: String) {
        val window = toolWindow() ?: return
        window.isAvailable = true
        val page = browser ?: JBCefBrowser().also {
            Disposer.register(this, it)
            val content = ContentFactory.getInstance().createContent(it.component, null, false)
            content.setDisposer(it)
            window.contentManager.addContent(content)
            browser = it
        }
        // Loading the same URL again would reload the page; the URL changes only with a new server.
        if (page.cefBrowser.url != url) {
            page.loadURL(url)
        }
    }

    private fun toolWindow(): ToolWindow? = ToolWindowManager.getInstance(project).getToolWindow(TOOL_WINDOW_ID)

    /** Handler of `asciidoc/revealLine`: the reader scrolled the page. Any thread. */
    fun revealLine(params: RevealLineParams) {
        ApplicationManager.getApplication().invokeLater({ applyRevealLine(params) }, project.disposed)
    }

    private fun applyRevealLine(params: RevealLineParams) {
        if (!following()) {
            return
        }
        val editor = editorFor(params.uri) ?: return
        val area = editor.scrollingModel.visibleArea
        val view = EditorView(
            uri = params.uri,
            lineCount = editor.document.lineCount,
            top = editor.xyToLogicalPosition(Point(0, area.y)).line,
            bottom = editor.xyToLogicalPosition(Point(0, area.y + area.height)).line,
        )
        when (val action = revealAction(params, view)) {
            RevealAction.None -> Unit
            is RevealAction.Scroll -> scrollToTop(editor, action.line)
            is RevealAction.Cursor -> {
                val caret = editor.caretModel.primaryCaret
                if (caret.logicalPosition.line != action.line || caret.logicalPosition.column != 0 || caret.hasSelection()) {
                    echo.record(params.uri, action.line)
                    caret.removeSelection()
                    caret.moveToLogicalPosition(LogicalPosition(action.line, 0))
                }
                if (action.scroll) {
                    scrollToTop(editor, action.line)
                }
            }
        }
    }

    /** The visible text editor of the document [uri]. */
    private fun editorFor(uri: String): Editor? =
        FileEditorManager.getInstance(project).selectedEditors
            .filterIsInstance<TextEditor>()
            .map { it.editor }
            .firstOrNull { uriOf(it) == uri }

    private fun uriOf(editor: Editor): String? = editor.virtualFile?.let(LSPIJUtils::toUriAsString)

    private fun scrollToTop(editor: Editor, line: Int) {
        editor.scrollingModel.scrollVertically(editor.logicalPositionToXY(LogicalPosition(line, 0)).y)
    }

    /** Asks the server for the page of [file]; [file] becomes the document the page shows. */
    fun pageUrl(file: VirtualFile): CompletableFuture<String> =
        server().thenCompose { server ->
            val mode = JsonObject().apply { addProperty("mode", "document") }
            server.workspaceService.executeCommand(
                ExecuteCommandParams(OPEN_COMMAND, listOf(LSPIJUtils.toUriAsString(file), mode)),
            )
        }.thenApply { result ->
            previewUrl(result) ?: throw PreviewException("asciidoc-lsp returned no preview URL.")
        }

    /** The running server; fails if none runs for this project. */
    private fun server(): CompletableFuture<LanguageServer> =
        LanguageServerManager.getInstance(project).getLanguageServer(SERVER_ID).thenApply { item ->
            item?.server ?: throw PreviewException("the language server is not running.")
        }

    companion object {
        fun getInstance(project: Project): Preview = project.service()
    }
}

/** Whether [editor] shows a file the server handles: the patterns and language plugin.xml maps. */
internal fun isAsciidoc(editor: Editor): Boolean {
    val file = editor.virtualFile ?: return false
    return file.extension in ASCIIDOC_EXTENSIONS ||
        (file.fileType as? LanguageFileType)?.language?.id == ASCIIDOC_LANGUAGE
}

/** The id of the tool window in plugin.xml. */
private const val TOOL_WINDOW_ID = "asciidoc-lsp Preview"
private val ASCIIDOC_EXTENSIONS = setOf("adoc", "asciidoc")
private const val ASCIIDOC_LANGUAGE = "AsciiDoc"

/** A preview request failed; the message is for the reader. */
class PreviewException(message: String) : RuntimeException(message)
