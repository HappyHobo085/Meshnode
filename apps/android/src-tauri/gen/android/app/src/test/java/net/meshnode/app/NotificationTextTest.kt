package net.meshnode.app

import org.junit.Assert.assertEquals
import org.junit.Test

class NotificationTextTest {
  @Test
  fun running() {
    assertEquals("Meshnode: hosting 3 services · 2 peers online", NotificationText.running(3, 2))
    assertEquals("Meshnode: hosting 0 services · 0 peers online", NotificationText.running(0, 0))
  }

  @Test
  fun failed() {
    assertEquals(
      "Meshnode couldn't start: the data directory is locked — tap to open",
      NotificationText.failed("the data directory is locked"),
    )
  }

  @Test
  fun uninitialised() {
    assertEquals("Meshnode: not set up — tap to open", NotificationText.uninitialised())
  }

  @Test
  fun restartNeeded() {
    assertEquals(
      "Meshnode stopped in the background — tap to restart",
      NotificationText.restartNeeded(),
    )
  }

  @Test
  fun batteryExemptionWhy() {
    assertEquals(
      "Needed for automatic restart after the system stops Meshnode",
      NotificationText.batteryExemptionWhy(),
    )
  }

  @Test
  fun starting() {
    assertEquals("Meshnode: starting…", NotificationText.starting())
  }

  @Test
  fun forPhasePicksTheText() {
    assertEquals(NotificationText.starting(), NotificationText.forPhase(NodePhase.STARTING, 1, 1, null))
    assertEquals(NotificationText.running(4, 5), NotificationText.forPhase(NodePhase.RUNNING, 4, 5, null))
    assertEquals(NotificationText.uninitialised(), NotificationText.forPhase(NodePhase.NOT_SET_UP, 0, 0, null))
    assertEquals(NotificationText.failed("boom"), NotificationText.forPhase(NodePhase.FAILED, 0, 0, "boom"))
    assertEquals(
      NotificationText.failed("unknown error"),
      NotificationText.forPhase(NodePhase.FAILED, 0, 0, null),
    )
    assertEquals(
      NotificationText.failed("unknown error"),
      NotificationText.forPhase(NodePhase.FAILED, 0, 0, "  "),
    )
  }
}
