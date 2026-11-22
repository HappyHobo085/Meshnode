import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Release signing (sub-spec 8b §4). The keystore and its passwords stay outside the repo, named by
// a local properties file: $MESHNODE_KEYSTORE_PROPERTIES (an absolute path) if set, else
// apps/android/keystore.properties (gitignored); keys as in apps/android/keystore.properties.example.
// Without it a release build fails, unless -PmeshnodeUnsignedRelease=true asks for an unsigned APK
// (CI, which has no key). The checks fail the release tasks only, so debug builds need no key.
val keystorePropertiesFile: File = providers.environmentVariable("MESHNODE_KEYSTORE_PROPERTIES")
    .map { file(it) }
    .getOrElse(rootProject.file("../../../keystore.properties"))
    .canonicalFile
val unsignedRelease = providers.gradleProperty("meshnodeUnsignedRelease").orNull == "true"
val signingKeys = listOf("storeFile", "storePassword", "keyAlias", "keyPassword")
val keystoreProperties: Properties? =
    if (!unsignedRelease && keystorePropertiesFile.isFile) {
        Properties().apply { keystorePropertiesFile.inputStream().use { load(it) } }
    } else {
        null
    }
// storeFile: absolute, `~/…`, or relative to the properties file's directory.
fun keystoreFile(path: String): File = when {
    path.startsWith("~/") -> File(System.getProperty("user.home"), path.removePrefix("~/"))
    File(path).isAbsolute -> File(path)
    else -> File(keystorePropertiesFile.parentFile, path)
}

// What makes a release build fail; reported by the task below, never at configuration time.
val releaseProblems = buildList {
    if (!unsignedRelease) {
        if (keystoreProperties == null) {
            add(
                "Release signing: $keystorePropertiesFile not found. Copy " +
                    "apps/android/keystore.properties.example, or pass " +
                    "-PmeshnodeUnsignedRelease=true for an unsigned build (CI)."
            )
        } else {
            signingKeys.filter { keystoreProperties.getProperty(it).isNullOrBlank() }.forEach {
                add("Release signing: $keystorePropertiesFile has no `$it`.")
            }
        }
    }
    // The release must carry the product version (written by the Tauri CLI from tauri.conf.json),
    // never the defaults below.
    for (key in listOf("tauri.android.versionCode", "tauri.android.versionName")) {
        if (tauriProperties.getProperty(key).isNullOrBlank()) {
            add(
                "Release version: ${file("tauri.properties")} has no `$key`; build with " +
                    "apps/android/build-apk.sh (the Tauri CLI writes it from tauri.conf.json)."
            )
        }
    }
}

android {
    // 36, not 35: the tauri-android library (tauri 2.11+) is compiled against 36 and its AAR
    // metadata requires apps to compile against 36 or later. targetSdk stays 35.
    compileSdk = 36
    // AGP 9.3 ignores build tools below its minimum (36.0.0), so pin what it uses.
    buildToolsVersion = "36.0.0"
    ndkVersion = "27.2.12479018"
    namespace = "net.meshnode.app"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "net.meshnode.app"
        minSdk = 29
        targetSdk = 35
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        if (keystoreProperties != null && signingKeys.all { !keystoreProperties.getProperty(it).isNullOrBlank() }) {
            create("release") {
                storeFile = keystoreFile(keystoreProperties.getProperty("storeFile"))
                storePassword = keystoreProperties.getProperty("storePassword")
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            // Absent (unsigned APK) only with -PmeshnodeUnsignedRelease=true; otherwise the
            // release check below fails the build first.
            signingConfigs.findByName("release")?.let { signingConfig = it }
            optimization {
               enable = true
            }
            proguardFiles(
                *fileTree(".") {
                  include("**/*.pro")
                  exclude("build/**")
                }.files.toTypedArray()
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        buildConfig = true
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

rust {
    rootDirRel = "../../../"
}

// Runs before every release variant (pre<Variant>ReleaseBuild) and fails it on releaseProblems.
val checkRelease = tasks.register("checkMeshnodeRelease") {
    description = "Fails a release build without a signing key (or opt-out) or a product version"
    val problems = releaseProblems
    doLast {
        if (problems.isNotEmpty()) {
            throw GradleException(problems.joinToString("\n"))
        }
    }
}
tasks.configureEach {
    if (name.startsWith("pre") && name.endsWith("ReleaseBuild")) {
        dependsOn(checkRelease)
    }
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    // The UniFFI Kotlin bindings (net.meshnode.ffi, generated by ../../../gen-bindings.sh).
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    implementation("androidx.work:work-runtime-ktx:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = file("tauri.build.gradle.kts"))
