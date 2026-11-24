package io.github.seflue.asciidoclsp

import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.PersistentStateComponent
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.State
import com.intellij.openapi.components.Storage

/** Application-wide choice of the server binary. */
@Service(Service.Level.APP)
@State(name = "AsciidocLspSettings", storages = [Storage("asciidoc-lsp.xml")])
class AsciidocLspSettings : PersistentStateComponent<AsciidocLspSettings.State> {
    enum class Mode { AUTO, CUSTOM }

    data class State(
        var mode: Mode = Mode.AUTO,
        var customPath: String = "",
    )

    private var state = State()

    override fun getState(): State = state

    override fun loadState(state: State) {
        this.state = state
    }

    companion object {
        fun getInstance(): AsciidocLspSettings =
            ApplicationManager.getApplication().getService(AsciidocLspSettings::class.java)
    }
}
