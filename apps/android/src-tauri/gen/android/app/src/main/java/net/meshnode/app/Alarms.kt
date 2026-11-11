package net.meshnode.app

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.SystemClock
import android.util.Log

/** The restart alarm (after a swipe from recents) and the 15-minute wall-clock tick. */
object Alarms {
  const val TICK_MS = 15 * 60 * 1000L
  private const val REQ_RESTART = 1
  private const val REQ_TICK = 2
  private const val TAG = "Meshnode"

  /** Fires [RestartReceiver] after [delayMs]: exact when allowed, otherwise inexact. */
  fun scheduleRestart(context: Context, delayMs: Long) {
    val am = context.getSystemService(AlarmManager::class.java) ?: return
    val pi = broadcast(context, RestartReceiver::class.java, REQ_RESTART)
    val at = SystemClock.elapsedRealtime() + delayMs
    try {
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S || am.canScheduleExactAlarms()) {
        am.setExactAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, at, pi)
      } else {
        am.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, at, pi)
      }
    } catch (e: SecurityException) {
      // The exact-alarm grant was revoked in between.
      Log.w(TAG, "exact restart alarm refused; using an inexact one", e)
      am.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, at, pi)
    }
  }

  /** Arms (or re-arms) the inexact 15-minute tick that fires [AlarmReceiver]. */
  fun scheduleTick(context: Context) {
    val am = context.getSystemService(AlarmManager::class.java) ?: return
    val pi = broadcast(context, AlarmReceiver::class.java, REQ_TICK)
    am.setAndAllowWhileIdle(
      AlarmManager.ELAPSED_REALTIME_WAKEUP,
      SystemClock.elapsedRealtime() + TICK_MS,
      pi,
    )
  }

  private fun broadcast(context: Context, receiver: Class<*>, request: Int): PendingIntent =
    PendingIntent.getBroadcast(
      context,
      request,
      Intent(context, receiver),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
}
