package io.github.seflue.asciidoclsp

import java.io.IOException
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.TimeUnit

/** Runs a server binary with `--version`, as the settings' Check button does. */
object ServerCheck {
    sealed interface Result {
        data class Ok(val version: String) : Result
        data class Error(val message: String) : Result
    }

    private const val TIMEOUT_SECONDS = 5L

    fun run(binary: Path): Result {
        if (!Files.isRegularFile(binary)) return Result.Error("$binary does not exist")
        if (!Files.isExecutable(binary)) return Result.Error("$binary is not executable")
        val process = try {
            ProcessBuilder(binary.toString(), "--version").redirectErrorStream(true).start()
        } catch (e: IOException) {
            return Result.Error("$binary could not be started: ${e.message}")
        }
        if (!process.waitFor(TIMEOUT_SECONDS, TimeUnit.SECONDS)) {
            process.destroyForcibly()
            return Result.Error("$binary did not answer --version within $TIMEOUT_SECONDS s")
        }
        val output = process.inputStream.bufferedReader().readText().trim()
        return when {
            process.exitValue() != 0 -> Result.Error("$binary --version exited with ${process.exitValue()}: $output")
            !output.startsWith("asciidoc-lsp ") -> Result.Error("$binary is not asciidoc-lsp: $output")
            else -> Result.Ok(output)
        }
    }
}
