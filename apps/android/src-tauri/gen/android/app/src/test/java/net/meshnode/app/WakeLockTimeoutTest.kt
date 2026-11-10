package net.meshnode.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class WakeLockTimeoutTest {
  @Test
  fun maxMeansNoTimeout() {
    assertNull(WakeLockTimeout.toMillis(ULong.MAX_VALUE))
  }

  @Test
  fun ordinaryValuesPassThrough() {
    assertEquals(10_000L, WakeLockTimeout.toMillis(10_000UL))
    assertEquals(330_000L, WakeLockTimeout.toMillis(330_000UL))
  }

  @Test
  fun zeroPassesThrough() {
    assertEquals(0L, WakeLockTimeout.toMillis(0UL))
  }

  @Test
  fun valuesAboveLongMaxAreClamped() {
    assertEquals(Long.MAX_VALUE, WakeLockTimeout.toMillis(Long.MAX_VALUE.toULong()))
    assertEquals(Long.MAX_VALUE, WakeLockTimeout.toMillis(ULong.MAX_VALUE - 1UL))
  }
}
