package io.jeph.oopbot.ui.chat

import io.jeph.oopbot.agent.ImagePart
import io.jeph.oopbot.agent.TimelineItem
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ChatViewTest {
  private data class FakePixels(val bytes: Long)

  @Test
  fun thumbnailCacheEvictsEldestByCountAndByteBudgetWhileKeepingAspect() {
    val cache = ThumbnailCache<FakePixels>(maxEntries = 3, maxBytes = 100L) { it.bytes }
    val first = ImagePart("a", "image/jpeg")
    val second = ImagePart("b", "image/jpeg")
    val third = ImagePart("c", "image/jpeg")

    cache.put(first, FakePixels(60L), aspectRatio = 1.5f)
    cache.put(second, FakePixels(40L), aspectRatio = 0.75f)
    // Refresh `first` so `second` is evicted when `third` exceeds the 100-byte budget.
    assertEquals(FakePixels(60L), cache.get(first))

    cache.put(third, FakePixels(40L), aspectRatio = 2.0f)

    assertEquals(FakePixels(60L), cache.get(first))
    assertNull(cache.get(second))
    assertEquals(0.75f, cache.aspect(second))
    assertEquals(FakePixels(40L), cache.get(third))
  }

  @Test
  fun thumbnailCacheSkipsOversizedEntryButRecordsAspect() {
    val cache = ThumbnailCache<FakePixels>(maxEntries = 2, maxBytes = 50L) { it.bytes }
    val small = ImagePart("small", "image/jpeg")
    val huge = ImagePart("huge", "image/jpeg")

    cache.put(small, FakePixels(30L), aspectRatio = 1.0f)
    cache.put(huge, FakePixels(100L), aspectRatio = 1.25f)

    assertEquals(FakePixels(30L), cache.get(small))
    assertNull(cache.get(huge))
    assertEquals(1.25f, cache.aspect(huge))

    cache.clear()
    assertNull(cache.get(small))
    assertNull(cache.aspect(huge))
  }

  @Test
  fun endRepliesCollectsFinishedRepliesNewestFirstAndSkipsStreaming() {
    val items = listOf(
      TimelineItem.AssistantText(key = "1", markdown = "oldest", streaming = false, timestamp = 1L),
      TimelineItem.User(key = "u", text = "hi", images = emptyList(), timestamp = 2L),
      TimelineItem.AssistantText(key = "2", markdown = "middle", streaming = false, timestamp = 3L),
      TimelineItem.AssistantText(key = "3", markdown = "live delta", streaming = true, timestamp = 4L),
    )

    val replies = items.endReplies().toList()

    assertEquals(listOf("middle", "oldest"), replies)
  }
}
