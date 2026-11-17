package net.meshnode.app

import android.content.Context
import androidx.core.content.edit

/** The app's own settings. The power policy is not here: its source of truth is meshd.toml. */
class Prefs(context: Context) {
  private val sp = context.applicationContext.getSharedPreferences(FILE, Context.MODE_PRIVATE)

  /** Set by Stop; while set, no automatic trigger starts the service ([RestartPolicy]). */
  var userStopped: Boolean
    get() = sp.getBoolean(KEY_USER_STOPPED, false)
    set(value) = sp.edit { putBoolean(KEY_USER_STOPPED, value) }

  /**
   * Set when a start was refused for data from an incompatible Meshnode version; while set,
   * no automatic trigger starts the service ([RestartPolicy]). Opening the app or Start
   * clears it.
   */
  var refusedIncompatible: Boolean
    get() = sp.getBoolean(KEY_REFUSED_INCOMPATIBLE, false)
    set(value) = sp.edit { putBoolean(KEY_REFUSED_INCOMPATIBLE, value) }

  /** Whether onboarding has been completed (Continue pressed). */
  var onboarded: Boolean
    get() = sp.getBoolean(KEY_ONBOARDED, false)
    set(value) = sp.edit { putBoolean(KEY_ONBOARDED, value) }

  /** How often the notification permission request was denied (onboarding). */
  var notificationDenials: Int
    get() = sp.getInt(KEY_NOTIFICATION_DENIALS, 0)
    set(value) = sp.edit { putInt(KEY_NOTIFICATION_DENIALS, value) }

  private companion object {
    const val FILE = "meshnode"
    const val KEY_USER_STOPPED = "user_stopped"
    const val KEY_REFUSED_INCOMPATIBLE = "refused_incompatible"
    const val KEY_ONBOARDED = "onboarded"
    const val KEY_NOTIFICATION_DENIALS = "notification_denials"
  }
}
