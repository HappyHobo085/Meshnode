package net.meshnode.app

/** Maps mesh-ffi's wake-lock `max_ms` to a `PowerManager.WakeLock.acquire` timeout. Pure. */
object WakeLockTimeout {
  /** `null` means no timeout (`max_ms == u64::MAX`); larger values are clamped to `Long`. */
  fun toMillis(maxMs: ULong): Long? = when {
    maxMs == ULong.MAX_VALUE -> null
    maxMs > Long.MAX_VALUE.toULong() -> Long.MAX_VALUE
    else -> maxMs.toLong()
  }
}
