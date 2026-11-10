package net.meshnode.app

/**
 * Whether a trigger starts [MeshService] (spec §5.2). Pure, so the JVM tests cover it.
 *
 * `user_stopped` (set by the Stop action) blocks every automatic trigger; only the user
 * pressing Start overrides it, and clears it (the caller does that, through [Prefs]).
 */
object RestartPolicy {
  enum class Trigger { BOOT, PACKAGE_REPLACED, TASK_REMOVED_ALARM, WATCHDOG, APP_OPEN, USER_START }

  fun shouldStart(trigger: Trigger, userStopped: Boolean, running: Boolean): Boolean =
    !running && (trigger == Trigger.USER_START || !userStopped)

  /** Whether the caller must clear `user_stopped` for this trigger. */
  fun clearsUserStopped(trigger: Trigger): Boolean = trigger == Trigger.USER_START
}
