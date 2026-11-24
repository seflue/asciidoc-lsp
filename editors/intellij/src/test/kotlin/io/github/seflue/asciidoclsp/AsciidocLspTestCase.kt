package io.github.seflue.asciidoclsp

import com.intellij.notification.Notification
import com.intellij.notification.Notifications
import com.intellij.openapi.application.WriteAction
import com.intellij.openapi.module.EmptyModuleType
import com.intellij.openapi.module.Module
import com.intellij.openapi.module.ModuleManager
import com.intellij.openapi.roots.ModuleRootManager
import com.intellij.openapi.util.Disposer
import com.intellij.testFramework.PsiTestUtil
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.testFramework.PlatformTestUtil
import com.intellij.testFramework.common.ThreadLeakTracker
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import com.intellij.testFramework.fixtures.CodeInsightTestFixture
import com.intellij.testFramework.fixtures.IdeaTestFixtureFactory
import com.redhat.devtools.lsp4ij.LanguageServerItem
import com.redhat.devtools.lsp4ij.LanguageServerManager
import com.redhat.devtools.lsp4ij.LanguageServersRegistry
import com.redhat.devtools.lsp4ij.LanguageServiceAccessor
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.CompletableFuture

/**
 * Runs the real asciidoc-lsp binary through LSP4IJ. Files live on disk: LSP4IJ maps
 * them to NIO paths, which the light fixture's temp:// files don't have.
 */
abstract class AsciidocLspTestCase : BasePlatformTestCase() {
    override fun setUp() {
        registerThreadLeakWhitelist()
        super.setUp()
        AsciidocLspSettings.getInstance().loadState(AsciidocLspSettings.State())
    }

    override fun tearDown() {
        try {
            // Default StopOptions disable the server for the rest of the run.
            LanguageServerManager.getInstance(project)
                .stop(SERVER_ID, LanguageServerManager.StopOptions().setWillDisable(false))
            AsciidocLspSettings.getInstance().loadState(AsciidocLspSettings.State())
        } finally {
            super.tearDown()
        }
    }

    override fun createMyFixture(): CodeInsightTestFixture {
        val factory = IdeaTestFixtureFactory.getFixtureFactory()
        val projectFixture = factory.createFixtureBuilder(getTestName(false), true).fixture
        return factory.createCodeInsightFixture(projectFixture, factory.createTempDirTestFixture())
    }

    /**
     * Opens [text] as a file on disk inside the project directory, which LSP4IJ sends as the
     * workspace root: the server resolves xrefs only for files under it. `<caret>` marks the caret.
     */
    protected fun openPhysical(fileName: String, text: String): VirtualFile {
        val caret = text.indexOf(CARET)
        val root = Path.of(project.basePath ?: throw AssertionError("test project has no base path"))
        val path = root.resolve(fileName)
        Files.createDirectories(root)
        Files.writeString(path, text.replace(CARET, ""))
        val file = LocalFileSystem.getInstance().refreshAndFindFileByNioFile(path)
            ?: throw AssertionError("cannot find $path in the VFS")
        contentModule(file.parent)
        myFixture.configureFromExistingVirtualFile(file)
        if (caret >= 0) {
            myFixture.editor.caretModel.moveToOffset(caret)
        }
        return file
    }

    /** The heavy fixture's project has no module; LSP4IJ takes the workspace folders from module content roots. */
    private fun contentModule(root: VirtualFile) {
        val manager = ModuleManager.getInstance(project)
        val module = manager.findModuleByName(MODULE_NAME) ?: WriteAction.computeAndWait<Module, RuntimeException> {
            manager.newModule(root.toNioPath().resolve("$MODULE_NAME.iml"), EmptyModuleType.EMPTY_MODULE)
        }
        if (ModuleRootManager.getInstance(module).contentRoots.none { it == root }) {
            PsiTestUtil.addContentRoot(module, root)
        }
    }

    protected fun useBinaryFromEnvironment() {
        val settings = AsciidocLspSettings.getInstance().state
        settings.mode = AsciidocLspSettings.Mode.CUSTOM
        settings.customPath = testBinary()
    }

    /**
     * Starts the server for the open file and waits until it is initialized.
     * LanguageServerManager.getLanguageServer(id) only returns servers that already run,
     * so this asks for the servers of the file, which starts and connects them.
     */
    protected fun waitForServer(): LanguageServerItem {
        val future = startServers()
        val item = future.get().firstOrNull { it.serverDefinition.id == SERVER_ID }
            ?: throw AssertionError(
                "asciidoc-lsp was not started for ${myFixture.file.virtualFile.path} " +
                    "(file type ${myFixture.file.fileType.name}, server registered: " +
                    "${LanguageServersRegistry.getInstance().getServerDefinition(SERVER_ID) != null}, " +
                    "file supported: ${LanguageServersRegistry.getInstance().isFileSupported(myFixture.file)}, " +
                    "servers: ${future.get().map { it.serverDefinition.id }}, " +
                    "binary: ${ServerBinary.resolve(AsciidocLspSettings.getInstance().state, pluginDir())})",
            )
        val initialized = item.initializedServer
        PlatformTestUtil.waitWithEventsDispatching(
            "asciidoc-lsp did not initialize",
            { initialized.isDone },
            TIMEOUT_SECONDS,
        )
        initialized.get()
        return item
    }

    /** Asks LSP4IJ for the open file's servers, which starts them, and waits for the answer. */
    protected fun startServers(): CompletableFuture<List<LanguageServerItem>> {
        val future = LanguageServiceAccessor.getInstance(project)
            .getLanguageServers(myFixture.file, { true }, { true })
        PlatformTestUtil.waitWithEventsDispatching("asciidoc-lsp did not start", { future.isDone }, TIMEOUT_SECONDS)
        return future
    }

    /** Polls [condition] while the IDE processes events, failing with [message] after the timeout. */
    protected fun waitUntil(message: String, condition: () -> Boolean) {
        PlatformTestUtil.waitWithEventsDispatching(message, condition, TIMEOUT_SECONDS)
    }

    /**
     * Whether a real Asciidoctor is installed, for tests that need one; as
     * `asciidoctor_available` in the server's tests. A test that finds none skips itself,
     * unless `ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR` is set: then the missing Asciidoctor fails
     * the test, so CI cannot pass without having run it.
     */
    protected fun asciidoctorAvailable(): Boolean {
        val available = runCatching {
            ProcessBuilder("asciidoctor", "--version").redirectErrorStream(true).start().run {
                inputStream.readAllBytes()
                waitFor() == 0
            }
        }.getOrDefault(false)
        if (!available) {
            assertNull(
                "asciidoctor is not on the PATH, but ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR is set",
                System.getenv("ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR"),
            )
            System.err.println("skipped ${getTestName(false)}: asciidoctor is not on the PATH")
        }
        return available
    }

    /** Notifications the project shows from now on. */
    protected fun collectNotifications(): List<Notification> {
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

    companion object {
        const val CARET = "<caret>"
        private const val MODULE_NAME = "docs"
        private const val TIMEOUT_SECONDS = 30

        fun testBinary(): String = System.getenv("ASCIIDOC_LSP_TEST_BINARY")
            ?.takeIf(String::isNotBlank)
            ?: throw AssertionError("Set ASCIIDOC_LSP_TEST_BINARY (just test-jetbrains does this).")

        private val whitelist = Disposer.newDisposable("asciidoc-lsp test thread whitelist")
        private var whitelistRegistered = false

        private fun registerThreadLeakWhitelist() {
            synchronized(whitelist) {
                if (!whitelistRegistered) {
                    ThreadLeakTracker.longRunningThreadCreated(whitelist, "SystemPropertyWatcher")
                    whitelistRegistered = true
                }
            }
        }
    }
}
