package io.jeph.oopbot.host

import io.jeph.oopbot.termux.assertRunsInAnyShell
import java.security.MessageDigest
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PlaySetupTest {
  private val servedKeys = mutableListOf<ByteArray>()

  @Test
  fun pairingMatchesTheSpecVector() {
    assertEquals(PAIRING_KEY, SetupSession.pairingKey(TOKEN).toHexString())
    val body = SetupSession.pairingBody(TOKEN, SID, KEY, ByteArray(12) { 0x22 })
    assertEquals(PAIRING_BODY, String(body, Charsets.US_ASCII))
  }

  @Test
  fun installerIsRenderedWithTheFilesHashesPackageAndThisBuildsTermuxNames() {
    val session = open()
    val installer = String(PlaySetup.respond("/oopbot/${session.sid}/setup.sh", NOW).body)
    // OOPBoT Dev shares Termux with the release, so the two builds' helpers need ports of their own.
    val ports = when (TermuxBuild.NAME) {
      "oopbot-dev", "pipkin-dev" -> "47320,47321,47322,47323,47324"
      else -> "47310,47311,47312,47313,47314"
    }
    assertEquals(
      "h=${sha256("helper")} r=${sha256("run")} s=${sha256("start")} p=io.jeph.oopbot.debug " +
        "n=${TermuxBuild.NAME} ports=$ports\n",
      installer,
    )
    assertEquals(sha256(installer), session.installerSha256)
  }

  @Test
  fun servesEachFileOfAnOpenSession() {
    val session = open()
    for ((name, body) in listOf("helper.mjs" to "helper", "run.sh" to "run", "start.sh" to "start")) {
      val response = PlaySetup.respond("/oopbot/${session.sid}/$name", NOW)
      assertEquals(200, response.status)
      assertEquals(body, String(response.body))
    }
  }

  @Test
  fun keyDecryptsWithTheTokenAndBecomesPending() {
    val session = open()
    val response = PlaySetup.respond("/oopbot/${session.sid}/key", NOW)

    assertEquals(200, response.status)
    val sealed = String(response.body, Charsets.US_ASCII).hexToByteArray()
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(
      Cipher.DECRYPT_MODE,
      SecretKeySpec(SetupSession.pairingKey(session.token), "AES"),
      GCMParameterSpec(128, sealed.copyOfRange(0, 12)),
    )
    cipher.updateAAD("oopbot-pair|${session.sid}".toByteArray())
    assertArrayEquals(session.key, cipher.doFinal(sealed.copyOfRange(12, sealed.size)))
    assertEquals(1, servedKeys.size)
    assertArrayEquals(session.key, servedKeys.single())
  }

  @Test
  fun otherPathsAreNotFound() {
    val session = open()
    assertEquals(404, PlaySetup.respond("/", NOW).status)
    assertEquals(404, PlaySetup.respond("/pipkin/${session.sid}/setup.sh", NOW).status)
    assertEquals(404, PlaySetup.respond("/oopbot/${session.sid}/host.mjs", NOW).status)
    assertEquals(404, PlaySetup.respond("/oopbot/${session.sid}/key?again", NOW).status)
    assertEquals(404, PlaySetup.respond("/oopbot/${session.sid}/../key", NOW).status)
  }

  @Test
  fun unknownSessionsAreGone() {
    open()
    assertEquals(410, PlaySetup.respond("/oopbot/$SID/setup.sh", NOW).status)
  }

  @Test
  fun aNewerCommandSupersedesTheSession() {
    val first = open()
    val second = open()
    assertEquals(410, PlaySetup.respond("/oopbot/${first.sid}/key", NOW).status)
    assertEquals(200, PlaySetup.respond("/oopbot/${second.sid}/key", NOW).status)
  }

  @Test
  fun sessionsExpire() {
    val session = open()
    assertEquals(200, PlaySetup.respond("/oopbot/${session.sid}/run.sh", EXPIRES - 1).status)
    assertEquals(410, PlaySetup.respond("/oopbot/${session.sid}/run.sh", EXPIRES).status)
  }

  @Test
  fun aHelperUsingTheKeyEndsOnlyTheKey() {
    val session = open()
    PlaySetup.complete(KEY)
    assertEquals(200, PlaySetup.respond("/oopbot/${session.sid}/key", NOW).status)
    PlaySetup.complete(session.key)
    assertEquals(410, PlaySetup.respond("/oopbot/${session.sid}/key", NOW).status)
    assertEquals(200, PlaySetup.respond("/oopbot/${session.sid}/setup.sh", NOW).status)
  }

  @Test
  fun commandRunsTheInstallerOnlyAfterItsHashChecksInAnyShell() {
    val session = open()
    val command = session.command(4242)
    val check = command.indexOf("echo \"${session.installerSha256}  \$f\" | sha256sum -c --status && ")
    val run = command.indexOf("bash \"\$f\"")
    assertTrue(check in 0 until run)
    assertTrue(command.contains("pkg install -y curl"))
    assertTrue(command.contains("OOPBOT_VERSION=\"${session.expectedVersion}\""))
    assertRunsInAnyShell(command)
  }

  @Test
  fun installerReplacesOopbotVersionPlaceholder() {
    val files = mapOf(
      HelperAssets.HELPER to "helper",
      HelperAssets.RUN to "run",
      HelperAssets.START to "start",
      HelperAssets.INSTALLER to "ver=@OOPBOT_VERSION@\n",
    )
    val session = SetupSession.create(
      read = { files.getValue(it).toByteArray() },
      packageName = "io.jeph.oopbot.debug",
      expiresAt = EXPIRES,
      onKeyServed = { servedKeys += it },
      expectedVersion = "0.5.9",
    )
    PlaySetup.open(session)
    val installer = String(PlaySetup.respond("/oopbot/${session.sid}/setup.sh", NOW).body)
    assertEquals("ver=0.5.9\n", installer)
  }

  private fun open(): SetupSession {
    val files = mapOf(
      HelperAssets.HELPER to "helper",
      HelperAssets.RUN to "run",
      HelperAssets.START to "start",
      HelperAssets.INSTALLER to
        "h=@HELPER_SHA256@ r=@RUN_SHA256@ s=@START_SHA256@ p=@OOPBOT_PACKAGE@ n=@OOPBOT_NAME@ ports=@HELPER_PORTS@\n",
    )
    val session = SetupSession.create(
      read = { files.getValue(it).toByteArray() },
      packageName = "io.jeph.oopbot.debug",
      expiresAt = EXPIRES,
      onKeyServed = { servedKeys += it },
    )
    PlaySetup.open(session)
    return session
  }

  private fun sha256(text: String) = MessageDigest.getInstance("SHA-256").digest(text.toByteArray()).toHexString()

  private companion object {
    const val NOW = 1_000_000L
    const val EXPIRES = NOW + 30 * 60_000L
    const val SID = "00112233445566778899aabbccddeeff"
    val TOKEN = ByteArray(32) { 0x11 }
    val KEY = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".hexToByteArray()
    const val PAIRING_KEY = "820c287eadee847ce6a8273d681e378e10982b67cb5cc40651a9400797005b0a"
    const val PAIRING_BODY = "222222222222222222222222aa7ecbd8e22b36928599e2b343d73b0ede989399" +
      "a42852de7d1a85ae54a33ead63d625268421e196cc26929d9a4d623c"
  }
}
