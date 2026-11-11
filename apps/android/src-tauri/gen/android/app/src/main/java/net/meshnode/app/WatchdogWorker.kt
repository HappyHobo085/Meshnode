package net.meshnode.app

import android.content.Context
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.Worker
import androidx.work.WorkerParameters
import java.util.concurrent.TimeUnit

/** Every 15 min: start the service if the node is not running (and the user did not stop it). */
class WatchdogWorker(context: Context, params: WorkerParameters) : Worker(context, params) {
  override fun doWork(): Result {
    MeshService.startIfAllowed(applicationContext, RestartPolicy.Trigger.WATCHDOG)
    return Result.success()
  }

  companion object {
    const val NAME = "meshnode-watchdog"

    fun schedule(context: Context) {
      val request = PeriodicWorkRequestBuilder<WatchdogWorker>(15, TimeUnit.MINUTES).build()
      WorkManager.getInstance(context.applicationContext)
        .enqueueUniquePeriodicWork(NAME, ExistingPeriodicWorkPolicy.KEEP, request)
    }
  }
}
