package net.meshnode.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class SafPathsTest {
  @Test
  fun joinsDisplayNamesWithSlashes() {
    assertEquals("site/css/a.css", SafPaths.relative(listOf("site", "css", "a.css")))
    assertEquals("index.html", SafPaths.relative(listOf("index.html")))
  }

  @Test
  fun refusesNamesThatAreNotOneSegment() {
    for (bad in listOf("bad:name", "a/b", "a\\b", "nul\u0000x", ".", "..", "")) {
      assertNull(bad, SafPaths.relative(listOf("site", bad)))
    }
    assertNull(SafPaths.relative(listOf("bad:name")))
    assertNull(SafPaths.relative(listOf("..")))
    assertNull(SafPaths.relative(emptyList()))
  }

  @Test
  fun keepsNonAsciiNames() {
    assertEquals("café/日本語.html", SafPaths.relative(listOf("café", "日本語.html")))
  }
}
