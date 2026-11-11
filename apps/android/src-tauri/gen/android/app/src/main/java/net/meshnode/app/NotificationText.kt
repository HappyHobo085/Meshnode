package net.meshnode.app

/** What the foreground notification reports. */
enum class NodePhase { STARTING, RUNNING, NOT_SET_UP, FAILED }

/** The foreground notification's text (spec §5.1). Pure, so the JVM tests cover it. */
object NotificationText {
  fun running(services: Int, peers: Int): String =
    "Meshnode: hosting $services services · $peers peers online"

  fun failed(reason: String): String = "Meshnode couldn't start: $reason — tap to open"

  fun uninitialised(): String = "Meshnode: not set up — tap to open"

  /**
   * The alert (an ordinary notification) when Android refused to restart the service from
   * the background; tapping it opens the app, which may start it.
   */
  fun restartNeeded(): String = "Meshnode stopped in the background — tap to restart"

  /** The onboarding note under the battery-exemption line. */
  fun batteryExemptionWhy(): String = "Needed for automatic restart after the system stops Meshnode"

  /** Shown from `startForeground` until the first `start()` returns. */
  fun starting(): String = "Meshnode: starting…"

  /** The text for [phase]; [error] is the failure reason (a blank one reads "unknown error"). */
  fun forPhase(phase: NodePhase, services: Int, peers: Int, error: String?): String =
    when (phase) {
      NodePhase.STARTING -> starting()
      NodePhase.RUNNING -> running(services, peers)
      NodePhase.NOT_SET_UP -> uninitialised()
      NodePhase.FAILED -> failed(error?.takeIf { it.isNotBlank() } ?: "unknown error")
    }
}
