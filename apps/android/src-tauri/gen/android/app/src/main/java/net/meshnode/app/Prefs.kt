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

  /** Whether onboarding has been completed (Continue pressed). */
  var onboarded: Boolean
    get() = sp.getBoolean(KEY_ONBOARDED, false)
    set(value) = sp.edit { putBoolean(KEY_ONBOARDED, value) }

  private companion object {
    const val FILE = "meshnode"
    const val KEY_USER_STOPPED = "user_stopped"
    const val KEY_ONBOARDED = "onboarded"
  }
}
