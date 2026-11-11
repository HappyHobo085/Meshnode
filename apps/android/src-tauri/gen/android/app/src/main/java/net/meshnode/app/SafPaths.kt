package net.meshnode.app

/** The relative path of a document in a picked tree, from its display names (pure, JVM-tested). */
object SafPaths {
  /**
   * The display names from the tree root down to the file, joined with `/`; `null` if there
   * are none, or one is empty, `.` or `..`, or contains `/`, `\`, `:` or NUL (such a name would
   * not be one segment of an upload path).
   */
  fun relative(segments: List<String>): String? {
    if (segments.isEmpty() || segments.any { !isSegment(it) }) return null
    return segments.joinToString("/")
  }

  private fun isSegment(name: String): Boolean =
    name.isNotEmpty() && name != "." && name != ".." && name.none { it in "/\\:\u0000" }
}
