package net.meshnode.app

import android.Manifest
import android.app.AlarmManager
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.util.TypedValue
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.net.toUri
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

/**
 * First launch (spec §5.4), plain Android views. Each permission is a status line
 * (granted / missing) with a button; none of them blocks the service from starting.
 */
class OnboardingActivity : AppCompatActivity() {
  private lateinit var notificationsLine: TextView
  private lateinit var alarmsLine: TextView
  private lateinit var batteryLine: TextView
  private lateinit var notificationsButton: Button
  private lateinit var alarmsButton: Button
  private lateinit var batteryButton: Button

  private val askNotifications =
    registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
      if (!granted) Prefs(this).notificationDenials += 1
      refresh()
    }

  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    val pad = dp(16)
    val column = LinearLayout(this).apply {
      orientation = LinearLayout.VERTICAL
      setPadding(pad, pad, pad, pad)
    }
    column.addView(text("Set up Meshnode", 22f))
    column.addView(
      text(
        "Meshnode keeps running in the background to serve your other devices. " +
          "These settings help it stay up; you can skip any of them.",
      ),
    )

    notificationsLine = text("")
    notificationsButton = button("Allow notifications") {
      if (Prefs(this).notificationDenials >= 2) {
        // Android no longer shows the request after two denials: open the app's settings.
        open(
          Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
            .putExtra(Settings.EXTRA_APP_PACKAGE, packageName),
        )
      } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        askNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
      }
    }
    alarmsLine = text("")
    alarmsButton = button("Allow exact alarms") {
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        open(Intent(Settings.ACTION_REQUEST_SCHEDULE_EXACT_ALARM, packageUri()))
      }
    }
    batteryLine = text("")
    batteryButton = button("Allow running in the background") {
      // Meshnode is a self-hosted server; the exemption is what keeps it reachable.
      @Suppress("BatteryLife")
      open(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, packageUri()))
    }
    for (v in listOf(
      notificationsLine, notificationsButton,
      alarmsLine, alarmsButton,
      batteryLine, text(NotificationText.batteryExemptionWhy(), 14f), batteryButton,
    )) {
      column.addView(v)
    }

    column.addView(text("Lock Meshnode in recents so swiping it away doesn't stop it"))
    column.addView(
      text("Phone-specific advice: $DONT_KILL_MY_APP").apply {
        setOnClickListener { open(Intent(Intent.ACTION_VIEW, DONT_KILL_MY_APP.toUri())) }
      },
    )
    column.addView(
      button("Continue") {
        Prefs(this).onboarded = true
        finish() // back to MainActivity, which opened this over itself
      },
    )

    val root = ScrollView(this).apply { addView(column) }
    ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
      val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars())
      v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      insets
    }
    setContentView(root)
  }

  override fun onResume() {
    super.onResume()
    refresh()
  }

  private fun refresh() {
    val notifications = Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) ==
      PackageManager.PERMISSION_GRANTED
    val alarms = Build.VERSION.SDK_INT < Build.VERSION_CODES.S ||
      getSystemService(AlarmManager::class.java)?.canScheduleExactAlarms() == true
    val battery =
      getSystemService(PowerManager::class.java)?.isIgnoringBatteryOptimizations(packageName) == true
    show(notificationsLine, notificationsButton, "Notifications", notifications)
    show(alarmsLine, alarmsButton, "Exact alarms (restart after a swipe)", alarms)
    show(batteryLine, batteryButton, "Battery optimisation exemption", battery)
  }

  private fun show(line: TextView, button: Button, label: String, granted: Boolean) {
    line.text = getString(R.string.onboarding_status, label, if (granted) "granted" else "missing")
    button.visibility = if (granted) View.GONE else View.VISIBLE
  }

  private fun packageUri(): Uri = "package:$packageName".toUri()

  private fun open(intent: Intent) {
    try {
      startActivity(intent)
    } catch (e: ActivityNotFoundException) {
      // Some builds lack the screen; the status line stays "missing".
    } catch (e: SecurityException) {
      // As above.
    }
  }

  private fun text(value: String, sizeSp: Float = 16f): TextView =
    TextView(this).apply {
      text = value
      setTextSize(TypedValue.COMPLEX_UNIT_SP, sizeSp)
      setPadding(0, dp(8), 0, dp(8))
    }

  private fun button(label: String, onClick: () -> Unit): Button =
    Button(this).apply {
      text = label
      setOnClickListener { onClick() }
    }

  private fun dp(value: Int): Int = (value * resources.displayMetrics.density).toInt()

  private companion object {
    const val DONT_KILL_MY_APP = "https://dontkillmyapp.com"
  }
}
