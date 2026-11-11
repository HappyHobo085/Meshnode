package net.meshnode.app

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** The alarm set by `MeshService.onTaskRemoved`: start the service unless the user stopped it. */
class RestartReceiver : BroadcastReceiver() {
  override fun onReceive(context: Context, intent: Intent) {
    MeshService.startIfAllowed(context, RestartPolicy.Trigger.TASK_REMOVED_ALARM)
  }
}
