#!/usr/bin/env bash
# Builds dist/mesh-client.aar (local use; not committed): libmesh_client_ffi.so for
# arm64-v8a and x86_64 (release, symbols stripped), classes.jar (the committed Kotlin
# bindings plus MeshClientAndroid, compiled by crates/mesh-client-ffi/jvm-test), a
# minimal AndroidManifest.xml and proguard.txt (consumer keep rules for minified apps).
#
# Usage: scripts/build-client-aar.sh
#
# Needs cargo-ndk with ANDROID_NDK_HOME (or NDK_HOME) set to an NDK (27.2 is the one the
# app uses), the two Rust Android targets, and JAVA_HOME (JDK 17+). The app using the
# AAR adds its runtime dependencies itself: net.java.dev.jna:jna:5.14.0@aar and
# org.jetbrains.kotlinx:kotlinx-coroutines-android (see crates/mesh-client/README.md).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ffi="$root/crates/mesh-client-ffi"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-}"
if [ -z "$target_dir" ]; then
    target_dir="$(cargo metadata --format-version 1 --no-deps |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | tr '\\' '/' | tr -s '/')"
fi

# JNA finds the exports in the dynamic symbol table, which stripping keeps.
CARGO_PROFILE_RELEASE_STRIP=symbols \
    cargo ndk -t arm64-v8a -t x86_64 build --release -p mesh-client-ffi --lib

(cd "$ffi/jvm-test" && ./gradlew --no-daemon jar)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
for pair in arm64-v8a:aarch64-linux-android x86_64:x86_64-linux-android; do
    abi="${pair%%:*}"
    triple="${pair#*:}"
    mkdir -p "$stage/jni/$abi"
    cp "$target_dir/$triple/release/libmesh_client_ffi.so" "$stage/jni/$abi/"
done
cp "$ffi/jvm-test/build/libs/classes.jar" "$stage/classes.jar"
cat >"$stage/AndroidManifest.xml" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="net.meshnode.client">
    <uses-sdk android:minSdkVersion="29" />
    <uses-permission android:name="android.permission.INTERNET" />
    <uses-permission android:name="android.permission.ACCESS_NETWORK_STATE" />
</manifest>
XML
: >"$stage/R.txt"
# Consumer R8/ProGuard rules, applied to an app that minifies (release builds): JNA binds
# by name and the JNA AAR ships no rules (the same block as apps/android's
# proguard-rules.pro, for this package).
cat >"$stage/proguard.txt" <<'PRO'
# --- mesh-client: UniFFI bindings over JNA ---
# JNA resolves Library methods to native symbols by name, libjnidispatch looks up
# com/sun/jna classes and fields by name, and UniFFI's Structure subclasses list their
# fields by name: nothing in these packages may be renamed or removed.
-dontwarn java.awt.**
-keepattributes RuntimeVisibleAnnotations
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.Structure { <fields>; }
-keep class net.meshnode.client.** { *; }
# The raw JNI export Java_net_meshnode_client_MeshClientAndroid_initAndroid.
-keepclasseswithmembernames class net.meshnode.client.MeshClientAndroid {
    native <methods>;
}
PRO

mkdir -p "$root/dist"
rm -f "$root/dist/mesh-client.aar"
# An AAR is a zip; the JDK's jar tool writes one (M: no jar manifest of its own).
"${JAVA_HOME:+$JAVA_HOME/bin/}jar" --create --no-manifest --file "$root/dist/mesh-client.aar" -C "$stage" .
echo "wrote $root/dist/mesh-client.aar" >&2
