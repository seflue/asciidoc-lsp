package io.github.seflue.asciidoclsp

import java.nio.file.Files
import java.nio.file.Path

/** Where the server binary comes from, or why there is none. */
sealed interface ServerResolution {
    data class Found(val path: Path) : ServerResolution
    data class Failed(val message: String) : ServerResolution
}

object ServerBinary {
    /** Rust target of the bundled binary for this JVM's `os.name` / `os.arch`, or null if none is bundled. */
    fun rustTarget(osName: String, osArch: String): String? {
        val os = osName.lowercase()
        val arch = when (osArch.lowercase()) {
            "amd64", "x86_64" -> "x86_64"
            "aarch64", "arm64" -> "aarch64"
            else -> return null
        }
        return when {
            os.startsWith("linux") -> "$arch-unknown-linux-musl"
            os.startsWith("mac") -> "$arch-apple-darwin"
            os.startsWith("windows") && arch == "x86_64" -> "x86_64-pc-windows-msvc"
            else -> null
        }
    }

    fun binaryName(osName: String): String =
        if (osName.lowercase().startsWith("windows")) "asciidoc-lsp.exe" else "asciidoc-lsp"

    fun resolve(
        settings: AsciidocLspSettings.State,
        pluginDir: Path?,
        osName: String = System.getProperty("os.name"),
        osArch: String = System.getProperty("os.arch"),
    ): ServerResolution = when (settings.mode) {
        AsciidocLspSettings.Mode.CUSTOM -> resolveCustom(settings.customPath)
        AsciidocLspSettings.Mode.AUTO -> resolveBundled(pluginDir, osName, osArch)
    }

    private fun resolveCustom(customPath: String): ServerResolution {
        val trimmed = customPath.trim()
        if (trimmed.isEmpty()) {
            return ServerResolution.Failed("Custom mode is set, but no server path is configured.")
        }
        val path = Path.of(trimmed)
        return when {
            !Files.isRegularFile(path) -> ServerResolution.Failed("Custom server path does not exist: $trimmed")
            !Files.isExecutable(path) -> ServerResolution.Failed("Custom server path is not executable: $trimmed")
            else -> ServerResolution.Found(path)
        }
    }

    private fun resolveBundled(pluginDir: Path?, osName: String, osArch: String): ServerResolution {
        val target = rustTarget(osName, osArch)
            ?: return ServerResolution.Failed(
                "No bundled asciidoc-lsp binary for $osName/$osArch. " +
                    "Set a custom server path in Settings | Tools | asciidoc-lsp.",
            )
        if (pluginDir == null) {
            return ServerResolution.Failed("The asciidoc-lsp plugin directory could not be found.")
        }
        val path = pluginDir.resolve("server").resolve(target).resolve(binaryName(osName))
        if (!Files.isRegularFile(path)) {
            return ServerResolution.Failed("Bundled server binary is missing: $path")
        }
        if (!Files.isExecutable(path) && !path.toFile().setExecutable(true)) {
            return ServerResolution.Failed("Bundled server binary could not be made executable: $path")
        }
        return ServerResolution.Found(path)
    }
}
