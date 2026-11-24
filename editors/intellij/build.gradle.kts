import org.jetbrains.intellij.platform.gradle.IntelliJPlatformType
import org.jetbrains.intellij.platform.gradle.TestFrameworkType

plugins {
    id("java")
    id("org.jetbrains.kotlin.jvm") version "2.4.21"
    id("org.jetbrains.intellij.platform") version "2.20.1"
}

group = providers.gradleProperty("pluginGroup").get()

// The plugin releases in lockstep with the server: just and release.yml pass the
// workspace version from Cargo.toml.
val pluginVersion = providers.gradleProperty("pluginVersion")
    .orElse(providers.provider { throw GradleException("Pass -PpluginVersion=<server version>") })
version = pluginVersion.get()

// Directory holding <rust-target>/asciidoc-lsp[.exe]; packed into the plugin as server/.
val serverDir = providers.gradleProperty("serverDir")

kotlin {
    jvmToolchain(21)
}

repositories {
    mavenCentral()
    intellijPlatform {
        defaultRepositories()
    }
}

dependencies {
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.opentest4j:opentest4j:1.3.0")

    intellijPlatform {
        intellijIdeaCommunity(providers.gradleProperty("platformVersion"))
        plugins(providers.gradleProperty("lsp4ijVersion").map { listOf("com.redhat.devtools.lsp4ij:$it") })
        testFramework(TestFrameworkType.Platform)
    }
}

intellijPlatform {
    pluginConfiguration {
        name = providers.gradleProperty("pluginName")
        version = pluginVersion
        ideaVersion {
            sinceBuild = providers.gradleProperty("pluginSinceBuild")
            untilBuild = provider { null }
        }
    }
    // Installed IDEs given as -PverifyIdes=<path>:<path> (no download); without them the build platform.
    pluginVerification {
        ides {
            val local = providers.gradleProperty("verifyIdes").orNull?.split(':')?.filter(String::isNotBlank).orEmpty()
            if (local.isEmpty()) {
                create(IntelliJPlatformType.IntellijIdeaCommunity, providers.gradleProperty("platformVersion"))
            } else {
                local.forEach { local(it) }
            }
        }
    }
}

tasks {
    // prepareSandbox and prepareTestSandbox: the plugin ZIP and the test IDE get the same server/
    withType<org.jetbrains.intellij.platform.gradle.tasks.PrepareSandboxTask>().configureEach {
        if (serverDir.isPresent) {
            from(serverDir) {
                // the task's own pluginName: the plugin's directory in the sandbox
                into(pluginName.map { "$it/server" })
            }
        }
    }
    test {
        testLogging {
            events("started", "passed", "failed", "skipped")
        }
        providers.gradleProperty("testBinary").orNull?.let {
            environment("ASCIIDOC_LSP_TEST_BINARY", it)
        }
        // Read through providers, so a change reconfigures and reruns the tests: the configuration
        // cache otherwise keeps the environment of the run that stored it.
        val requireAsciidoctor = providers.environmentVariable("ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR").orNull
        inputs.property("requireAsciidoctor", requireAsciidoctor ?: "unset")
        if (requireAsciidoctor == null) {
            environment.remove("ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR")
        } else {
            environment("ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR", requireAsciidoctor)
        }
    }
}
