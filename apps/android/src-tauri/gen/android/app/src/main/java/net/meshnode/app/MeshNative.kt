package net.meshnode.app

import android.content.Context

/**
 * mesh-ffi's raw JNI export (UniFFI cannot pass a `Context`). Call [initAndroid] on the main
 * thread, from `onCreate` of the service or the activity, before any `start`. It is
 * idempotent, and safe in either order relative to Tauri (the patched ndk-context keeps the
 * first context). Tauri also loads `meshnode_app`; loading it twice is harmless.
 */
object MeshNative {
  init {
    System.loadLibrary("meshnode_app")
  }

  @JvmStatic external fun initAndroid(context: Context)
}
