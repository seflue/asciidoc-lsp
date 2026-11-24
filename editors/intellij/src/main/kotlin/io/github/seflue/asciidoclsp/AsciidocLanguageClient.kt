package io.github.seflue.asciidoclsp

import com.google.gson.JsonObject
import com.intellij.openapi.project.Project
import com.redhat.devtools.lsp4ij.ServerStatus
import com.redhat.devtools.lsp4ij.client.LanguageClientImpl
import org.eclipse.lsp4j.jsonrpc.services.JsonNotification
import org.eclipse.lsp4j.services.LanguageServer

/** The server side of the preview's own notifications. */
interface AsciidocLanguageServerApi : LanguageServer {
    /** Where the cursor of the editor the page follows is; [CursorMovedParams.line] is 0-based. */
    @JsonNotification("asciidoc/cursorMoved")
    fun cursorMoved(params: CursorMovedParams)
}

/** Params of `asciidoc/cursorMoved`. */
data class CursorMovedParams(val uri: String, val line: Int)

/** The client side of the preview's own notifications. */
class AsciidocLanguageClient(project: Project) : LanguageClientImpl(project) {
    /** The reader scrolled the page; malformed params are dropped. */
    @JsonNotification("asciidoc/revealLine")
    fun revealLine(params: JsonObject) {
        revealLineParams(params)?.let { Preview.getInstance(project).revealLine(it) }
    }

    override fun handleServerStatusChanged(serverStatus: ServerStatus) {
        super.handleServerStatusChanged(serverStatus)
        if (serverStatus == ServerStatus.started) {
            Preview.getInstance(project).serverStarted()
        }
    }
}
