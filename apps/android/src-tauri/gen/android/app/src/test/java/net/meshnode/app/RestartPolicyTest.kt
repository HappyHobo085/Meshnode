package net.meshnode.app

import net.meshnode.app.RestartPolicy.Trigger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class RestartPolicyTest {
  /** The expected decision, written out per row rather than re-deriving the formula. */
  private val table: List<Triple<Trigger, Pair<Boolean, Boolean>, Boolean>> = listOf(
    // trigger, (userStopped, running), start
    Triple(Trigger.BOOT, false to false, true),
    Triple(Trigger.BOOT, false to true, false),
    Triple(Trigger.BOOT, true to false, false),
    Triple(Trigger.BOOT, true to true, false),
    Triple(Trigger.PACKAGE_REPLACED, false to false, true),
    Triple(Trigger.PACKAGE_REPLACED, false to true, false),
    Triple(Trigger.PACKAGE_REPLACED, true to false, false),
    Triple(Trigger.PACKAGE_REPLACED, true to true, false),
    Triple(Trigger.TASK_REMOVED_ALARM, false to false, true),
    Triple(Trigger.TASK_REMOVED_ALARM, false to true, false),
    Triple(Trigger.TASK_REMOVED_ALARM, true to false, false),
    Triple(Trigger.TASK_REMOVED_ALARM, true to true, false),
    Triple(Trigger.WATCHDOG, false to false, true),
    Triple(Trigger.WATCHDOG, false to true, false),
    Triple(Trigger.WATCHDOG, true to false, false),
    Triple(Trigger.WATCHDOG, true to true, false),
    Triple(Trigger.APP_OPEN, false to false, true),
    Triple(Trigger.APP_OPEN, false to true, false),
    Triple(Trigger.APP_OPEN, true to false, false),
    Triple(Trigger.APP_OPEN, true to true, false),
    Triple(Trigger.USER_START, false to false, true),
    Triple(Trigger.USER_START, false to true, false),
    Triple(Trigger.USER_START, true to false, true),
    Triple(Trigger.USER_START, true to true, false),
  )

  @Test
  fun everyTriggerUserStoppedAndRunningCombination() {
    for ((trigger, flags, expected) in table) {
      val (userStopped, running) = flags
      assertEquals(
        "trigger=$trigger userStopped=$userStopped running=$running",
        expected,
        RestartPolicy.shouldStart(trigger, userStopped, running),
      )
    }
  }

  @Test
  fun theTableCoversEveryCombination() {
    assertEquals(Trigger.entries.size * 4, table.size)
    for (t in Trigger.entries) {
      for (userStopped in listOf(false, true)) {
        for (running in listOf(false, true)) {
          assertTrue(
            "missing $t $userStopped $running",
            table.any { it.first == t && it.second == (userStopped to running) },
          )
        }
      }
    }
  }

  @Test
  fun onlyUserStartClearsUserStopped() {
    for (t in Trigger.entries) {
      assertEquals(t == Trigger.USER_START, RestartPolicy.clearsUserStopped(t))
    }
  }
}
