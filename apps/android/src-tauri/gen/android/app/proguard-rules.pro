# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# --- Meshnode: UniFFI bindings over JNA (the JNA AAR ships no consumer rules) ---
# JNA resolves Library methods to native symbols by name, libjnidispatch looks up
# com/sun/jna classes and fields by name, and UniFFI's Structure subclasses list their
# fields by name: nothing in these packages may be renamed or removed.
-dontwarn java.awt.**
# JNA reads UniFFI's @Structure.FieldOrder annotations at runtime; keep them here rather
# than rely on AGP's default optimize file.
-keepattributes RuntimeVisibleAnnotations
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.Structure { <fields>; }
-keep class net.meshnode.ffi.** { *; }
# --- Reflectively created classes whose library rules keep the class but not its
# no-arg constructor, which R8 full mode (AGP 9's default) then removes: WorkManager's
# Room database (`WorkDatabase_Impl`, room-runtime 2.6.1 rules) and ML Kit's Firebase
# component registrars (barcode scanner). Without these the release app dies at startup
# in androidx.startup.InitializationProvider.
-keep class * extends androidx.room.RoomDatabase { <init>(); }
-keep class * implements com.google.firebase.components.ComponentRegistrar { <init>(); }
# mesh-ffi's raw JNI export (Java_net_meshnode_app_MeshNative_initAndroid).
-keepclasseswithmembernames class net.meshnode.app.MeshNative {
    native <methods>;
}
