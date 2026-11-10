package net.meshnode.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.PowerManager
import android.util.Log
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import net.meshnode.ffi.FfiWakeReason
import net.meshnode.ffi.wake

/** The 15-minute wall-clock tick: `wake(ALARM)` under a 10 s wake lock, then re-arm. */
class AlarmReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    val app = context.applicationContext
    if (Prefs(app).userStopped) return // Start re-arms it (MeshService.onCreate).
    Alarms.scheduleTick(app)
    val pending = goAsync()
    val lock = app.getSystemService(PowerManager::class.java)
      .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "meshnode:alarm")
      .apply {
        setReferenceCounted(false)
        acquire(WAKE_LOCK_MS)
      }
    MeshService.scope.launch {
      try {
        // wake returns within 10 s by itself (a no-op while stopped); this bounds it anyway.
        withTimeoutOrNull(WAKE_LOCK_MS) { wake(FfiWakeReason.ALARM) }
        MeshService.instance?.refreshNotification()
      } catch (t: Throwable) {
        Log.w("Meshnode", "alarm wake failed", t)
      } finally {
        try {
          if (lock.isHeld) lock.release()
        } finally {
          pending.finish()
        }
      }
    }
  }

  private companion object {
    const val WAKE_LOCK_MS = 10_000L
  }
}
