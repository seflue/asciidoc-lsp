package io.github.seflue.asciidoclsp

import com.intellij.execution.configurations.GeneralCommandLine
import com.intellij.ide.plugins.PluginManagerCore
import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.extensions.PluginId
import com.intellij.openapi.project.Project
import com.intellij.openapi.project.ProjectManager
import com.redhat.devtools.lsp4ij.LanguageServerFactory
import com.redhat.devtools.lsp4ij.LanguageServerManager
import com.redhat.devtools.lsp4ij.ServerStatus
import com.redhat.devtools.lsp4ij.server.CannotStartProcessException
import com.redhat.devtools.lsp4ij.server.OSProcessStreamConnectionProvider
import com.redhat.devtools.lsp4ij.server.StreamConnectionProvider
import java.nio.file.Path

const val PLUGIN_ID = "io.github.seflue.asciidoclsp"
const val SERVER_ID = "asciidocLsp"
private const val NOTIFICATION_GROUP = "asciidoc-lsp"

class AsciidocLanguageServerFactory : LanguageServerFactory {
    override fun createConnectionProvider(project: Project): StreamConnectionProvider =
        AsciidocConnectionProvider(project)
}

/** Starts the binary the settings select; reports a missing binary as a notification. */
class AsciidocConnectionProvider(private val project: Project) : OSProcessStreamConnectionProvider() {
    private val failure: String?

    init {
        when (val resolution = ServerBinary.resolve(AsciidocLspSettings.getInstance().state, pluginDir())) {
            is ServerResolution.Found -> {
                commandLine = GeneralCommandLine(resolution.path.toString())
                failure = null
            }
            is ServerResolution.Failed -> failure = resolution.message
        }
    }

    override fun start() {
        failure?.let {
            notifyError(project, it)
            throw CannotStartProcessException(it)
        }
        super.start()
    }
}

fun pluginDir(): Path? = PluginManagerCore.getPlugin(PluginId.getId(PLUGIN_ID))?.pluginPath

fun notifyError(project: Project?, message: String) {
    NotificationGroupManager.getInstance()
        .getNotificationGroup(NOTIFICATION_GROUP)
        .createNotification("asciidoc-lsp", message, NotificationType.ERROR)
        .notify(project)
}

/**
 * Restarts the server where it was started, so a changed binary takes effect; this includes one that
 * stopped because its binary was missing. Projects that never started it stay untouched.
 */
fun restartServers() {
    val options = LanguageServerManager.StartOptions().setForceRestart(true)
    ProjectManager.getInstance().openProjects
        .filterNot { it.isDisposed }
        .map(LanguageServerManager::getInstance)
        .filter { it.getServerStatus(SERVER_ID) != ServerStatus.none }
        .forEach { it.start(SERVER_ID, options) }
}
