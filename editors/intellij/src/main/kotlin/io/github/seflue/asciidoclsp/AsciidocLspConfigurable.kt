package io.github.seflue.asciidoclsp

import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ModalityState
import com.intellij.openapi.fileChooser.FileChooserDescriptorFactory
import com.intellij.openapi.options.Configurable
import com.intellij.openapi.ui.TextFieldWithBrowseButton
import com.intellij.ui.components.JBLabel
import com.intellij.ui.components.JBRadioButton
import com.intellij.ui.dsl.builder.AlignX
import com.intellij.ui.dsl.builder.panel
import javax.swing.JComponent

/** Settings | Tools | asciidoc-lsp: bundled or custom server binary, with a --version check. */
class AsciidocLspConfigurable : Configurable {
    /** The values the dialog shows; reading and writing them goes through the UI components. */
    inner class Form {
        var mode: AsciidocLspSettings.Mode
            get() = if (customButton.isSelected) AsciidocLspSettings.Mode.CUSTOM else AsciidocLspSettings.Mode.AUTO
            set(value) {
                customButton.isSelected = value == AsciidocLspSettings.Mode.CUSTOM
                autoButton.isSelected = value == AsciidocLspSettings.Mode.AUTO
                pathField.isEnabled = value == AsciidocLspSettings.Mode.CUSTOM
            }

        var customPath: String
            get() = pathField.text.trim()
            set(value) {
                pathField.text = value
            }
    }

    private val autoButton = JBRadioButton("Bundled server")
    private val customButton = JBRadioButton("Custom server binary:")
    private val pathField = TextFieldWithBrowseButton()
    private val checkResult = JBLabel()
    val form = Form()

    override fun getDisplayName(): String = "asciidoc-lsp"

    override fun createComponent(): JComponent {
        autoButton.addActionListener { form.mode = AsciidocLspSettings.Mode.AUTO }
        customButton.addActionListener { form.mode = AsciidocLspSettings.Mode.CUSTOM }
        pathField.addBrowseFolderListener(null, FileChooserDescriptorFactory.createSingleFileNoJarsDescriptor())
        reset()
        return panel {
            buttonsGroup {
                row { cell(autoButton) }
                row {
                    cell(customButton)
                    cell(pathField).align(AlignX.FILL)
                }
            }
            row {
                button("Check") { check() }
                cell(checkResult)
            }
        }
    }

    override fun isModified(): Boolean {
        val state = AsciidocLspSettings.getInstance().state
        return form.mode != state.mode || form.customPath != state.customPath
    }

    override fun apply() {
        val state = AsciidocLspSettings.getInstance().state
        state.mode = form.mode
        state.customPath = form.customPath
        restartServers()
    }

    override fun reset() {
        val state = AsciidocLspSettings.getInstance().state
        form.mode = state.mode
        form.customPath = state.customPath
        checkResult.text = ""
    }

    /** Runs the binary off the UI thread: a hanging one would freeze the dialog until the timeout. */
    private fun check() {
        val pending = AsciidocLspSettings.State(form.mode, form.customPath)
        checkResult.text = "Checking…"
        ApplicationManager.getApplication().executeOnPooledThread {
            val text = when (val resolution = ServerBinary.resolve(pending, pluginDir())) {
                is ServerResolution.Failed -> resolution.message
                is ServerResolution.Found -> when (val result = ServerCheck.run(resolution.path)) {
                    is ServerCheck.Result.Ok -> result.version
                    is ServerCheck.Result.Error -> result.message
                }
            }
            ApplicationManager.getApplication().invokeLater({ checkResult.text = text }, ModalityState.any())
        }
    }
}
