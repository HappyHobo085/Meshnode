package net.meshnode.app

import android.content.Intent
import android.os.Bundle
import android.util.Log
import androidx.activity.enableEdgeToEdge
import net.meshnode.ffi.setServiceControl

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    // Main thread, before any start; idempotent, and either order relative to Tauri is safe.
    MeshNative.initAndroid(applicationContext)
    try {
      // Registered here so that Start works while the service is not running.
      setServiceControl(ServiceControl(this))
    } catch (t: Throwable) {
      Log.e("Meshnode", "setServiceControl failed", t)
    }
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    MeshService.startIfAllowed(this, RestartPolicy.Trigger.APP_OPEN)
    if (savedInstanceState == null && !Prefs(this).onboarded) {
      // Shown over the Tauri page; Continue returns to it.
      startActivity(Intent(this, OnboardingActivity::class.java))
    }
  }
}
