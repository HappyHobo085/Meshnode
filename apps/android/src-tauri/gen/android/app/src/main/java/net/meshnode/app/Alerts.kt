package net.meshnode.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * Ordinary (not foreground) notifications on their own channel. Used when Android refused
 * to start the service from the background: tapping opens the app, whose start (an
 * APP_OPEN, from the foreground) is allowed.
 */
object Alerts {
  private const val CHANNEL_ID = "meshnode-alerts"
  private const val RESTART_ID = 2

  fun restartNeeded(context: Context) {
    val app = context.applicationContext
    try {
      val nm = app.getSystemService(NotificationManager::class.java) ?: return
      nm.createNotificationChannel(
        NotificationChannel(CHANNEL_ID, "Meshnode alerts", NotificationManager.IMPORTANCE_DEFAULT),
      )
      val open = PendingIntent.getActivity(
        app,
        0,
        Intent(app, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
        PendingIntent.FLAG_IMMUTABLE,
      )
      val text = NotificationText.restartNeeded()
      val n = Notification.Builder(app, CHANNEL_ID)
        .setSmallIcon(R.drawable.ic_stat_meshnode)
        .setContentText(text)
        .setStyle(Notification.BigTextStyle().bigText(text))
        .setContentIntent(open)
        .setAutoCancel(true)
        .build()
      nm.notify(RESTART_ID, n)
    } catch (e: RuntimeException) {
      Log.w("Meshnode", "cannot post the restart alert", e)
    }
  }

  /** Removes the restart alert (the service is in the foreground again). */
  fun clearRestartNeeded(context: Context) {
    try {
      context.getSystemService(NotificationManager::class.java)?.cancel(RESTART_ID)
    } catch (e: RuntimeException) {
      Log.w("Meshnode", "cannot cancel the restart alert", e)
    }
  }
}
