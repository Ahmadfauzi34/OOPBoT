package io.jeph.pipkin.host

import io.jeph.pipkin.agent.helloVersionMismatch
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class HostClientVersionGuardTest {
  @Test
  fun verifiedEnsureVersionAcceptsMatchingOopbotOrPiVersion() {
    assertEquals(
      "0.5.5",
      verifiedEnsureVersion(JSONObject().put("oopbotVersion", "0.5.5").put("piVersion", "0.5.5"), "0.5.5"),
    )
    assertEquals("0.5.5", verifiedEnsureVersion(JSONObject().put("piVersion", "0.5.5"), "0.5.5"))
  }

  @Test
  fun verifiedEnsureVersionRejectsStaleNullBlankOrMissingWithNodeTooOld() {
    val invalidPayloads = listOf(
      JSONObject(),
      JSONObject().put("oopbotVersion", ""),
      JSONObject().put("oopbotVersion", JSONObject.NULL),
      JSONObject().put("piVersion", JSONObject.NULL),
      JSONObject().put("oopbotVersion", "0.5.4").put("piVersion", "0.5.4"),
      JSONObject().put("piVersion", "0.5.2"),
      JSONObject().put("oopbotVersion", "0.5.6"),
    )
    for (payload in invalidPayloads) {
      val failure = assertThrows(HostFailure.NodeTooOld::class.java) {
        verifiedEnsureVersion(payload, "0.5.5")
      }
      assertEquals(HostFailure.Kind.NodeTooOld, failure.kind)
      assertTrue(failure.command?.contains("/releases/download/v0.5.5/") == true)
    }
  }

  @Test
  fun extractEnsureVersionDoesNotTurnJsonNullIntoLiteralNullString() {
    val payload = JSONObject()
      .put("oopbotVersion", JSONObject.NULL)
      .put("piVersion", JSONObject.NULL)
    assertNull(extractEnsureVersion(payload))
  }

  @Test
  fun helloVersionMismatchReturnsPiVersionRejectionOnlyOnMismatch() {
    assertNull(helloVersionMismatch(JSONObject().put("oopbotVersion", "0.5.5"), "0.5.5"))
    assertNull(helloVersionMismatch(JSONObject().put("piVersion", "0.5.5"), "0.5.5"))
    assertNull(helloVersionMismatch(JSONObject(), "0.5.5"))
    assertNull(helloVersionMismatch(JSONObject().put("oopbotVersion", JSONObject.NULL), "0.5.5"))

    val stale = helloVersionMismatch(JSONObject().put("oopbotVersion", "0.5.4"), "0.5.5")
    assertEquals("pi_version", stale?.code)

    val staleLegacy = helloVersionMismatch(JSONObject().put("piVersion", "0.5.2"), "0.5.5")
    assertEquals("pi_version", staleLegacy?.code)
  }
}
