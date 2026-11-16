import org.gradle.api.tasks.testing.logging.TestExceptionFormat
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    // The versions apps/android uses (Gradle 9.6.1, Kotlin 2.2.10, JNA 5.14.0,
    // coroutines 1.10.2), so a machine that built the app has them cached.
    kotlin("jvm") version "2.2.10"
}

repositories {
    mavenCentral()
}

// main = the committed bindings plus the Android helper (it needs no android.jar).
sourceSets {
    main {
        kotlin.srcDir("../kotlin")
        kotlin.srcDir("../android")
    }
}

java {
    sourceCompatibility = JavaVersion.VERSION_11
    targetCompatibility = JavaVersion.VERSION_11
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_11)
    }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.14.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
    testImplementation(kotlin("test"))
}

tasks.jar {
    archiveFileName.set("classes.jar")
}

// The HOST library (mesh_client_ffi.dll / libmesh_client_ffi.so): -PnativeDir=<dir>, else
// $CARGO_TARGET_DIR/debug, else the workspace's target/debug.
val nativeDir: String =
    providers.gradleProperty("nativeDir").orNull
        ?: System.getenv("CARGO_TARGET_DIR")?.let { "$it/debug" }
        ?: rootDir.resolve("../../../target/debug").canonicalPath

tasks.test {
    useJUnitPlatform()
    systemProperty("jna.library.path", nativeDir)
    // Always run: the native library is not a Gradle input.
    outputs.upToDateWhen { false }
    testLogging {
        events("passed", "skipped", "failed")
        exceptionFormat = TestExceptionFormat.FULL
        showStandardStreams = true
    }
}
