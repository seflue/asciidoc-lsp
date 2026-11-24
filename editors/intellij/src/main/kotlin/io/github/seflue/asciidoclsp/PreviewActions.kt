package io.github.seflue.asciidoclsp

import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.actionSystem.CommonDataKeys
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.DumbAwareAction
import com.intellij.openapi.project.Project
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory

/** Opens the preview of the AsciiDoc editor the action runs in. */
abstract class PreviewAction : DumbAwareAction() {
    override fun getActionUpdateThread() = ActionUpdateThread.BGT

    override fun update(event: AnActionEvent) {
        event.presentation.isEnabledAndVisible = editor(event) != null
    }

    override fun actionPerformed(event: AnActionEvent) {
        val project = event.project ?: return
        val editor = editor(event) ?: return
        open(Preview.getInstance(project), editor)
    }

    protected abstract fun open(preview: Preview, editor: Editor)

    private fun editor(event: AnActionEvent): Editor? = event.getData(CommonDataKeys.EDITOR)?.takeIf(::isAsciidoc)
}

/** *Open AsciiDoc Preview*: the page in the tool window. */
class OpenPreviewAction : PreviewAction() {
    override fun open(preview: Preview, editor: Editor) = preview.openToSide(editor)
}

/** *Open AsciiDoc Preview in Browser*. */
class OpenPreviewInBrowserAction : PreviewAction() {
    override fun open(preview: Preview, editor: Editor) = preview.openInBrowser(editor)
}

/**
 * The preview's tool window. It stays out of the stripe until the first *Open AsciiDoc
 * Preview*; [Preview] puts the page into it.
 */
class PreviewToolWindowFactory : ToolWindowFactory, DumbAware {
    override fun shouldBeAvailable(project: Project) = false

    override fun createToolWindowContent(project: Project, toolWindow: ToolWindow) = Unit
}
