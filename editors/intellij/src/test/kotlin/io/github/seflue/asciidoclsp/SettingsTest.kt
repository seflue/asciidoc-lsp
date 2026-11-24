package io.github.seflue.asciidoclsp

import com.intellij.notification.Notification
import com.intellij.notification.Notifications
import java.nio.file.Files
import java.nio.file.Path

/** Stories 5, 6, 9-14: choosing, checking and switching the server binary. */
class SettingsTest : AsciidocLspTestCase() {
    fun testAutoIsTheDefault() {
        assertEquals(AsciidocLspSettings.Mode.AUTO, AsciidocLspSettings().state.mode)
    }

    fun testMissingCustomPathIsReportedAtServerStart() {
        val notifications = collectNotifications()
        val settings = AsciidocLspSettings.getInstance().state
        settings.mode = AsciidocLspSettings.Mode.CUSTOM
        settings.customPath = "/does/not/exist/asciidoc-lsp"
        openPhysical("doc.adoc", "= Title\n")

        val started = runCatching { startServers().get() }.getOrDefault(emptyList())
        assertTrue(started.none { it.serverDefinition.id == SERVER_ID })
        waitUntil("no notification about the missing path") {
            notifications.any { it.content.contains("/does/not/exist/asciidoc-lsp") }
        }
    }

    fun testCheckShowsTheServerVersion() {
        val result = ServerCheck.run(Path.of(testBinary()))

        assertTrue("expected a version in $result", result is ServerCheck.Result.Ok)
        assertTrue((result as ServerCheck.Result.Ok).version.startsWith("asciidoc-lsp "))
    }

    fun testCheckRejectsAMissingPath() {
        val result = ServerCheck.run(Path.of("/does/not/exist/asciidoc-lsp"))

        assertTrue(result is ServerCheck.Result.Error)
        assertTrue((result as ServerCheck.Result.Error).message.contains("does not exist"))
    }

    fun testCheckRejectsANonExecutableFile() {
        val file = Files.createTempFile("asciidoc-lsp", "")
        try {
            val result = ServerCheck.run(file)

            assertTrue(result is ServerCheck.Result.Error)
            assertTrue((result as ServerCheck.Result.Error).message.contains("not executable"))
        } finally {
            Files.delete(file)
        }
    }

    fun testCheckRejectsABinaryThatIsNotTheServer() {
        val result = ServerCheck.run(Path.of("/bin/false"))

        assertTrue(result is ServerCheck.Result.Error)
    }

    fun testApplyRestartsTheRunningServer() {
        openPhysical("doc.adoc", "= Title\n")
        val wrapper = waitForServer().serverWrapper
        val bundled = wrapper.currentProcessCommandLine

        val configurable = AsciidocLspConfigurable()
        configurable.createComponent()
        configurable.form.mode = AsciidocLspSettings.Mode.CUSTOM
        configurable.form.customPath = testBinary()
        assertTrue(configurable.isModified)
        configurable.apply()

        assertEquals(AsciidocLspSettings.Mode.CUSTOM, AsciidocLspSettings.getInstance().state.mode)
        assertEquals(testBinary(), AsciidocLspSettings.getInstance().state.customPath)
        waitUntil("server was not restarted with the custom binary (was $bundled)") {
            wrapper.currentProcessCommandLine == listOf(testBinary())
        }
        configurable.disposeUIResources()
    }

    fun testBundledBinaryIsMadeExecutable() {
        val target = ServerBinary.rustTarget("Linux", "amd64")!!
        val pluginDir = Files.createTempDirectory("asciidoc-lsp-plugin")
        try {
            val binary = pluginDir.resolve("server").resolve(target).resolve("asciidoc-lsp")
            Files.createDirectories(binary.parent)
            Files.writeString(binary, "#!/bin/sh\n")

            val result = ServerBinary.resolve(AsciidocLspSettings.State(), pluginDir, "Linux", "amd64")

            assertEquals(ServerResolution.Found(binary), result)
            assertTrue(Files.isExecutable(binary))
        } finally {
            pluginDir.toFile().deleteRecursively()
        }
    }

    fun testUnbundledPlatformPointsToTheSetting() {
        val result = ServerBinary.resolve(AsciidocLspSettings.State(), Path.of("/plugin"), "Windows 11", "aarch64")

        assertTrue(result is ServerResolution.Failed)
        val message = (result as ServerResolution.Failed).message
        assertTrue(message, message.contains("Windows 11/aarch64") && message.contains("custom server path"))
    }

    private fun collectNotifications(): List<Notification> {
        val seen = java.util.concurrent.CopyOnWriteArrayList<Notification>()
        project.messageBus.connect(testRootDisposable).subscribe(
            Notifications.TOPIC,
            object : Notifications {
                override fun notify(notification: Notification) {
                    seen += notification
                }
            },
        )
        return seen
    }
}
