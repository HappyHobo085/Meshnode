package net.meshnode.client

/**
 * Android set-up for [MeshClient]: call [init] once (e.g. in `Application.onCreate`) before
 * the first `MeshClient.join` or `MeshClient.open`, which fail with
 * `MeshClientException.NotInitialised` otherwise. The networking code needs the
 * application context. Not needed on the desktop JVM.
 */
object MeshClientAndroid {
    init {
        System.loadLibrary("mesh_client_ffi")
    }

    /** Gives the native library the application context of [context] (an `android.content.Context`). Idempotent. */
    fun init(context: Any) {
        initAndroid(context)
    }

    @JvmStatic
    private external fun initAndroid(context: Any)
}
