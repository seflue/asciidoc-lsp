package io.github.seflue.asciidoclsp

// Decisions of the preview sync, kept free of the IntelliJ API so they can be tested
// without an IDE; a port of editors/vscode/src/previewSync.ts. The wiring lives in Preview.kt.

import com.google.gson.JsonElement
import com.google.gson.JsonObject
import com.google.gson.JsonPrimitive
import kotlin.math.abs

/** Lines the top of the editor may differ from a revealLine and still count as there: the page rounds its line. */
const val TOLERANCE = 1

/** Params of `asciidoc/revealLine`; [line] is 0-based. */
data class RevealLineParams(val uri: String, val line: Int, val cursor: Boolean = false)

/** The params of a revealLine as the server sent them, or `null` if they are malformed. */
fun revealLineParams(json: JsonElement?): RevealLineParams? {
    val obj = json as? JsonObject ?: return null
    val uri = obj.primitive("uri")?.takeIf { it.isString }?.asString ?: return null
    val number = obj.primitive("line")?.takeIf { it.isNumber } ?: return null
    // 1.5 has no exact int value
    val line = runCatching { number.asBigDecimal.intValueExact() }.getOrNull() ?: return null
    val cursor = when (val value = obj.get("cursor")) {
        null -> false
        is JsonPrimitive if value.isBoolean -> value.asBoolean
        else -> return null
    }
    return RevealLineParams(uri, line, cursor)
}

private fun JsonObject.primitive(name: String): JsonPrimitive? = get(name) as? JsonPrimitive

/** The parts of an editor a revealLine looks at; lines are 0-based, [top] and [bottom] the visible ones. */
data class EditorView(val uri: String, val lineCount: Int, val top: Int, val bottom: Int)

/** What a revealLine does to the editor. */
sealed interface RevealAction {
    data object None : RevealAction

    /** Scroll [line] to the top. */
    data class Scroll(val line: Int) : RevealAction

    /** Put the cursor on [line]; [scroll] it to the top as well. */
    data class Cursor(val line: Int, val scroll: Boolean) : RevealAction
}

/** What a revealLine does to [view]. Only an editor showing the document reacts. */
fun revealAction(params: RevealLineParams, view: EditorView): RevealAction {
    if (params.uri != view.uri || view.lineCount == 0) {
        return RevealAction.None
    }
    val line = params.line.coerceIn(0, view.lineCount - 1)
    if (params.cursor) {
        // The view moves only if the line is out of sight.
        return RevealAction.Cursor(line, scroll = line < view.top || line > view.bottom)
    }
    if (abs(view.top - line) <= TOLERANCE) {
        return RevealAction.None
    }
    return RevealAction.Scroll(line)
}

/** How long the cursor position a revealLine left counts as its echo. */
const val ECHO_MS = 250L

/**
 * Remembers the cursor a revealLine forced, so the caret change it causes is not sent
 * back. The record stays for further events at the same place and goes once the cursor
 * has moved on or the time is up.
 */
class EchoGuard(private val now: () -> Long = System::currentTimeMillis) {
    private data class Forced(val uri: String, val line: Int, val until: Long)

    private var forced: Forced? = null

    fun record(uri: String, line: Int) {
        forced = Forced(uri, line, now() + ECHO_MS)
    }

    fun isEcho(uri: String, line: Int): Boolean {
        val forced = forced
        if (forced != null && forced.uri == uri && forced.line == line && now() <= forced.until) {
            return true
        }
        this.forced = null
        return false
    }
}

/** The page URL from the answer to `asciidoc.preview.open`, or `null`. LSP4J hands the result over as a map. */
fun previewUrl(result: Any?): String? = (result as? Map<*, *>)?.get("url") as? String
