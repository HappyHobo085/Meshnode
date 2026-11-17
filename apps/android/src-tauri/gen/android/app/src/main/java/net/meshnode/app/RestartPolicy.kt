package net.meshnode.app

/**
 * Whether a trigger starts [MeshService] (spec §5.2). Pure, so the JVM tests cover it.
 *
 * `user_stopped` (set by the Stop action) blocks every automatic trigger; only the user
 * pressing Start overrides it, and clears it (the caller does that, through [Prefs]).
 *
 * `refused_incompatible` (set when the node refused data from an incompatible Meshnode
 * version, sub-spec 8b §5.2) blocks the automatic triggers too, so that boot, the restart
 * alarm, the watchdog and an app update do not retry a start that cannot succeed. Opening
 * the app or pressing Start tries again, and clears it.
 */
object RestartPolicy {
  enum class Trigger { BOOT, PACKAGE_REPLACED, TASK_REMOVED_ALARM, WATCHDOG, APP_OPEN, USER_START }

  fun shouldStart(
    trigger: Trigger,
    userStopped: Boolean,
    refusedIncompatible: Boolean,
    running: Boolean,
  ): Boolean {
    if (running) return false
    if (trigger == Trigger.USER_START) return true
    if (userStopped) return false
    return !refusedIncompatible || trigger == Trigger.APP_OPEN
  }

  /** Whether the caller must clear `user_stopped` for this trigger. */
  fun clearsUserStopped(trigger: Trigger): Boolean = trigger == Trigger.USER_START

  /** Whether the caller must clear `refused_incompatible` for this trigger. */
  fun clearsRefusedIncompatible(trigger: Trigger): Boolean =
    trigger == Trigger.APP_OPEN || trigger == Trigger.USER_START
}
