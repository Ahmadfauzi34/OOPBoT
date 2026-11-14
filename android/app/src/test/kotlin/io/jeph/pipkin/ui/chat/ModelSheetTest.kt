package io.jeph.pipkin.ui.chat

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ModelSheetTest {
  private fun model(provider: String, id: String, name: String? = null): JSONObject = JSONObject().apply {
    put("provider", provider)
    put("id", id)
    if (name != null) put("name", name)
  }

  @Test
  fun `models sharing a display name under one provider show their ids as subtitles`() {
    val models = listOf(
      model("google", "antigravity-agent-preview-04", "Antigravity Agent Preview"),
      model("google", "antigravity-agent-preview-05", "Antigravity Agent Preview"),
      model("google", "gemini-2.5-flash", "Gemini 2.5 Flash"),
    )

    val groups = buildModelGroups(models, query = "")

    assertEquals(1, groups.size)
    val entries = groups.single().entries
    assertEquals(3, entries.size)
    assertEquals("Antigravity Agent Preview", entries[0].label)
    assertEquals("antigravity-agent-preview-04", entries[0].subtitle)
    assertEquals("Antigravity Agent Preview", entries[1].label)
    assertEquals("antigravity-agent-preview-05", entries[1].subtitle)
    assertEquals("Gemini 2.5 Flash", entries[2].label)
    assertNull(entries[2].subtitle)
  }

  @Test
  fun `same display name under different providers does not add subtitles`() {
    val models = listOf(
      model("google", "gemini-2.5-flash", "Gemini 2.5 Flash"),
      model("openrouter", "google/gemini-2.5-flash", "Gemini 2.5 Flash"),
    )

    val groups = buildModelGroups(models, query = "")

    assertEquals(listOf("google", "openrouter"), groups.map { it.provider })
    assertNull(groups[0].entries.single().subtitle)
    assertNull(groups[1].entries.single().subtitle)
  }

  @Test
  fun `searching by id keeps the disambiguating subtitle when one duplicate remains`() {
    val models = listOf(
      model("google", "antigravity-agent-preview-04", "Antigravity Agent Preview"),
      model("google", "antigravity-agent-preview-05", "Antigravity Agent Preview"),
    )

    val groups = buildModelGroups(models, query = "  preview-05 ")

    val entry = groups.single().entries.single()
    assertEquals("antigravity-agent-preview-05", entry.id)
    assertEquals("Antigravity Agent Preview", entry.label)
    assertEquals("antigravity-agent-preview-05", entry.subtitle)
  }

  @Test
  fun `duplicate provider and id entries are collapsed cleanly`() {
    val models = listOf(
      model("google", "gemini-2.5-flash", "Gemini 2.5 Flash"),
      model("google", "gemini-2.5-flash", "Gemini 2.5 Flash"),
    )

    val entries = buildModelGroups(models, query = "").single().entries

    assertEquals(1, entries.size)
    assertNull(entries.single().subtitle)
  }

  @Test
  fun `isSameModel requires matching id and modelLabel falls back to id`() {
    val first = model("google", "antigravity-agent-preview-04", "Antigravity Agent Preview")
    val second = model("google", "antigravity-agent-preview-05", "Antigravity Agent Preview")
    val unnamed = model("google", "gemini-custom", "   ")

    assertTrue(isSameModel(first, model("google", "antigravity-agent-preview-04", "Antigravity Agent Preview")))
    assertFalse(isSameModel(first, second))
    assertEquals("gemini-custom", modelLabel(unnamed))
    assertNull(modelLabel(null))
  }
}
