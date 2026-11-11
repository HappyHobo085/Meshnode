package net.meshnode.app

import android.annotation.SuppressLint
import android.content.Context
import android.os.PowerManager
import android.util.Log
import java.util.concurrent.ConcurrentHashMap
import net.meshnode.ffi.FfiWakeLockSink

/**
 * mesh-ffi's wake-lock sink: one non-reference-counted `PARTIAL_WAKE_LOCK` per tag.
 *
 * Rust calls these under its own mutex, so they never block, never throw and never call
 * back into mesh-ffi.
 */
class WakeLocks private constructor(context: Context) : FfiWakeLockSink {
  private val power = context.applicationContext.getSystemService(PowerManager::class.java)
  private val locks = ConcurrentHashMap<String, PowerManager.WakeLock>()

  /** Takes or refreshes the lock for [tag]; `ULong.MAX_VALUE` means no timeout. */
  @SuppressLint("WakelockTimeout") // Rust asks for no timeout only under the always-on policy.
  override fun acquire(tag: String, maxMs: ULong) {
    try {
      val lock = locks.getOrPut(tag) {
        power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "meshnode:$tag").apply {
          setReferenceCounted(false)
        }
      }
      val timeout = WakeLockTimeout.toMillis(maxMs)
      if (timeout == null) lock.acquire() else lock.acquire(timeout)
    } catch (t: Throwable) {
      Log.w(TAG, "wake lock acquire($tag) failed", t)
    }
  }

  override fun release(tag: String) {
    try {
      val lock = locks[tag] ?: return
      if (lock.isHeld) lock.release()
    } catch (t: Throwable) {
      Log.w(TAG, "wake lock release($tag) failed", t)
    }
  }

  /** Releases every lock held (the service is going away). */
  fun releaseAll() {
    for (tag in locks.keys) release(tag)
  }

  companion object {
    private const val TAG = "Meshnode"

    @Volatile private var instance: WakeLocks? = null

    /** The process-wide sink. */
    fun get(context: Context): WakeLocks =
      instance ?: synchronized(this) { instance ?: WakeLocks(context).also { instance = it } }
  }
}
