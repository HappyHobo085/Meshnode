package net.meshnode.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import net.meshnode.app.RestartPolicy.Trigger

/** `BOOT_COMPLETED` and `MY_PACKAGE_REPLACED`: start the service unless the user stopped it. */
class BootReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    val trigger = when (intent.action) {
      Intent.ACTION_BOOT_COMPLETED -> Trigger.BOOT
      Intent.ACTION_MY_PACKAGE_REPLACED -> Trigger.PACKAGE_REPLACED
      else -> return
    }
    MeshService.startIfAllowed(context, trigger)
  }
}
