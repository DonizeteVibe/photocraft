import java.io.ByteArrayOutputStream
import java.io.File
import java.util.Locale
import java.util.Properties

plugins {
    id("com.android.application")
}

val rustJniLibsDir = layout.buildDirectory.dir("generated/rustJniLibs").get().asFile
val workspaceRoot = rootProject.projectDir.toPath().resolve("../..").normalize().toFile()
val localProperties = Properties().apply {
    rootProject.file("local.properties").takeIf { it.isFile }?.inputStream()?.use { load(it) }
}
val androidSdkHome = providers.environmentVariable("ANDROID_HOME")
    .orElse(providers.environmentVariable("ANDROID_SDK_ROOT"))
    .orElse(providers.provider {
        localProperties.getProperty("sdk.dir")
            ?: throw GradleException("Set ANDROID_HOME/ANDROID_SDK_ROOT or sdk.dir in local.properties")
    })
val androidNdkHome = providers.environmentVariable("ANDROID_NDK_HOME")
    .orElse(providers.environmentVariable("ANDROID_NDK_ROOT"))
    .orElse(providers.provider { File(androidSdkHome.get(), "ndk/28.2.13676358").absolutePath })

android {
    namespace = "ai.storyteller.photocraft"
    compileSdk = 36
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "ai.storyteller.photocraft"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.3.0"
        ndk {
            abiFilters.add("arm64-v8a")
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.directories.add(rustJniLibsDir.absolutePath)
        }
    }

    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }
}

tasks.register<Exec>("buildRustArm64") {
    workingDir = workspaceRoot
    environment("ANDROID_NDK_HOME", androidNdkHome.get())
    commandLine(
        "cargo", "ndk", "--platform", "26", "-t", "arm64-v8a", "-o",
        rustJniLibsDir.absolutePath,
        "build", "--release", "-p", "photocraft-android",
    )
}

tasks.register<Exec>("verifyRustNativeActivityEntry") {
    dependsOn("buildRustArm64")
    val symbolOutput = ByteArrayOutputStream()
    standardOutput = symbolOutput

    doFirst {
        val osName = System.getProperty("os.name").lowercase(Locale.ROOT)
        val hostTags = when {
            osName.contains("mac") -> listOf("darwin-arm64", "darwin-x86_64")
            osName.contains("linux") -> listOf("linux-x86_64")
            osName.contains("windows") -> listOf("windows-x86_64")
            else -> emptyList()
        }
        val prebuiltRoot = File(androidNdkHome.get(), "toolchains/llvm/prebuilt")
        val hostTag = hostTags.firstOrNull { File(prebuiltRoot, it).isDirectory }
            ?: throw GradleException("No supported NDK LLVM toolchain found for $osName")
        val llvmNm = File(prebuiltRoot, "$hostTag/bin/llvm-nm${if (osName.contains("windows")) ".exe" else ""}")
        val nativeLibrary = File(rustJniLibsDir, "arm64-v8a/libphotocraft_android.so")

        executable = llvmNm.absolutePath
        args("-D", nativeLibrary.absolutePath)
    }

    doLast {
        val hasEntryPoint = symbolOutput.toString("UTF-8").lineSequence().any { line ->
            val fields = line.trim().split(Regex("\\s+"))
            fields.size >= 3 && fields[fields.lastIndex - 1] == "T" && fields.last() == "android_main"
        }
        if (!hasEntryPoint) {
            throw GradleException("The Rust library does not export the required android_main entry point")
        }
    }
}

tasks.named("preBuild") {
    dependsOn("verifyRustNativeActivityEntry")
}
