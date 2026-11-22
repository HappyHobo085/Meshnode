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
    val prefs = Prefs(app)
    // Start re-arms it (MeshService.onCreate); after an incompatible-data refusal no node
    // runs to wake until the app is opened.
    if (prefs.userStopped || prefs.refusedIncompatible) return
    Alarms.scheduleTick(app)
    val pending = goAsync()
    val lock = app.getSystemService(PowerManager::class.java)
      .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "meshnode:alarm-receiver")
      .apply {
        setReferenceCounted(false)
        acquire(WAKE_LOCK_MS)
      }
    MeshService.scope.launch {
      try {
        // wake returns within 10 s by itself (a no-op while stopped); 8 s here leaves room
        // for the refresh and finish() inside the 10 s lock.
        withTimeoutOrNull(WAKE_BOUND_MS) { wake(FfiWakeReason.ALARM) }
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
    const val WAKE_BOUND_MS = 8_000L
  }
}
