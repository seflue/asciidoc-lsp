package io.github.seflue.asciidoclsp

import com.intellij.ide.plugins.PluginManagerCore
import com.intellij.lang.annotation.HighlightSeverity
import com.intellij.openapi.extensions.PluginId

/** Stories 1, 4, 7, 13: the server starts for AsciiDoc files and its features reach the IDE. */
class ServerFeaturesTest : AsciidocLspTestCase() {
    fun testStartsBundledBinaryByDefault() {
        openPhysical("doc.adoc", "= Title\n\nText.\n")

        val item = waitForServer()

        assertEquals(AsciidocLspSettings.Mode.AUTO, AsciidocLspSettings.getInstance().state.mode)
        assertNotNull(item.server)
    }

    /** Stories 1 and 3: the .asciidoc pattern alone starts the server, without the asciidoctor plugin. */
    fun testStartsForAsciidocExtensionWithoutAsciidoctorPlugin() {
        assertNull(PluginManagerCore.getPlugin(PluginId.getId("org.asciidoctor.intellij.asciidoc")))
        useBinaryFromEnvironment()
        openPhysical("doc.asciidoc", "= Title\n\n== Section\n")

        assertNotNull(waitForServer().server)
    }

    fun testGoToDefinitionJumpsFromXrefToAnchor() {
        useBinaryFromEnvironment()
        val text = "= Title\n\n[[target]]\n== Target\n\nSee <<tar${CARET}get>>.\n"
        openPhysical("doc.adoc", text)
        waitForServer()
        myFixture.performEditorAction("GotoDeclaration")

        val anchorLine = 2
        assertEquals(anchorLine, myFixture.editor.caretModel.logicalPosition.line)
    }

    fun testCompletionOffersAnchorsInXref() {
        useBinaryFromEnvironment()
        // The generated section id appears nowhere in the text, so word completion can't supply it.
        openPhysical("doc.adoc", "= Title\n\n== Getting Started\n\nSee <<$CARET\n")
        waitForServer()

        val lookups = myFixture.completeBasic()?.map { it.lookupString }.orEmpty()

        assertTrue("expected '_getting_started' in $lookups", lookups.any { it.contains("_getting_started") })
    }

    fun testDocumentSymbolsListSections() {
        useBinaryFromEnvironment()
        val file = openPhysical("doc.adoc", "= Title\n\n== First\n\n== Second\n")
        val item = waitForServer()

        val params = org.eclipse.lsp4j.DocumentSymbolParams(
            org.eclipse.lsp4j.TextDocumentIdentifier(file.toNioPath().toUri().toString()),
        )
        val symbols = item.server!!.textDocumentService.documentSymbol(params).get()
        val names = symbols.flatMap { flatten(it.right) }

        assertTrue("expected sections in $names", names.containsAll(listOf("First", "Second")))
    }

    fun testReportsBrokenXref() {
        useBinaryFromEnvironment()
        openPhysical("doc.adoc", "= Title\n\nSee <<nonexistent>>.\n")
        waitForServer()

        var seen = emptyList<String?>()
        try {
            waitUntil("no diagnostic for the broken xref") {
                seen = myFixture.doHighlighting(HighlightSeverity.WARNING).map { it.description }
                seen.any { it?.contains("nonexistent") == true }
            }
        } catch (e: AssertionError) {
            throw AssertionError("${e.message}; highlights: $seen", e)
        }
    }

    private fun flatten(symbol: org.eclipse.lsp4j.DocumentSymbol?): List<String> =
        if (symbol == null) emptyList() else listOf(symbol.name) + symbol.children.orEmpty().flatMap(::flatten)
}
