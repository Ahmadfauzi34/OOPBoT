package io.jeph.oopbot.ui.chat

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.ImageDecoder
import android.net.Uri
import android.util.Base64
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.core.graphics.createBitmap
import io.jeph.oopbot.agent.ImagePart
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.nio.ByteBuffer
import kotlin.math.max
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/**
 * Bounded thread-safe LRU cache for decoded thumbnails, capped by both entry count and estimated pixel
 * bytes so scrolling back through image history avoids re-decoding base64 payloads without unbounded heap growth.
 */
internal class ThumbnailCache<T>(
  private val maxEntries: Int = DEFAULT_MAX_ENTRIES,
  private val maxBytes: Long = DEFAULT_MAX_BYTES,
  private val bytesOf: (T) -> Long,
) {
  private val lock = Any()
  private val entries = LinkedHashMap<ImagePart, T>(32, 0.75f, true)
  private val aspects = LinkedHashMap<ImagePart, Float>(64, 0.75f, true)
  private var totalBytes = 0L

  fun get(part: ImagePart): T? = synchronized(lock) { entries[part] }

  fun aspect(part: ImagePart): Float? = synchronized(lock) { aspects[part] }

  fun put(part: ImagePart, value: T, aspectRatio: Float) {
    val size = bytesOf(value).coerceAtLeast(0L)
    synchronized(lock) {
      rememberAspect(part, aspectRatio)
      if (maxEntries <= 0 || size > maxBytes) return
      val previous = entries.put(part, value)
      if (previous != null) totalBytes -= bytesOf(previous).coerceAtLeast(0L)
      totalBytes += size
      val iterator = entries.entries.iterator()
      while ((entries.size > maxEntries || totalBytes > maxBytes) && iterator.hasNext()) {
        val eldest = iterator.next()
        totalBytes -= bytesOf(eldest.value).coerceAtLeast(0L)
        iterator.remove()
      }
    }
  }

  fun clear() = synchronized(lock) {
    entries.clear()
    aspects.clear()
    totalBytes = 0L
  }

  private fun rememberAspect(part: ImagePart, ratio: Float) {
    aspects[part] = ratio
    val iterator = aspects.entries.iterator()
    while (aspects.size > MAX_ASPECT_ENTRIES && iterator.hasNext()) {
      iterator.next()
      iterator.remove()
    }
  }

  internal companion object {
    const val DEFAULT_MAX_ENTRIES = 48
    const val DEFAULT_MAX_BYTES = 16L * 1024L * 1024L
    const val MAX_ASPECT_ENTRIES = 256
  }
}

/** Image decoding for the composer and the chat stream; all work runs off the main thread. */
internal object ChatImages {
  private const val PROMPT_EDGE = 1568
  private const val THUMBNAIL_EDGE = 512
  private const val JPEG_QUALITY = 85
  private const val BYTES_PER_PIXEL = 4L
  private const val JPEG = "image/jpeg"

  private val cache = ThumbnailCache<ImageBitmap> { it.width.toLong() * it.height.toLong() * BYTES_PER_PIXEL }

  /** Incremented on the main thread when a new thumbnail finishes decoding, so [BottomAnchor] keeps end alignment. */
  var decodeVersion by mutableIntStateOf(0)
    private set

  fun cachedThumbnail(part: ImagePart): ImageBitmap? = cache.get(part)

  fun cachedAspect(part: ImagePart): Float? = cache.aspect(part)

  /**
   * Reads [uri] for a prompt: the long edge is scaled to at most 1568px and the image is re-encoded
   * as JPEG 85 with transparency flattened on white. Null when the image cannot be read.
   */
  suspend fun attach(context: Context, uri: Uri): Attachment.Image? = withContext(Dispatchers.IO) {
    try {
      // Software pixels: the bitmap is drawn into a Canvas to flatten transparency.
      val bitmap = decode(ImageDecoder.createSource(context.contentResolver, uri), PROMPT_EDGE, software = true)
      val bytes = ByteArrayOutputStream().use { out ->
        flattenOnWhite(bitmap).compress(Bitmap.CompressFormat.JPEG, JPEG_QUALITY, out)
        out.toByteArray()
      }
      val thumbnail =
        decode(ImageDecoder.createSource(ByteBuffer.wrap(bytes)), THUMBNAIL_EDGE, software = false).asImageBitmap()
      val part = ImagePart(Base64.encodeToString(bytes, Base64.NO_WRAP), JPEG)
      cache.put(part, thumbnail, thumbnail.aspectRatio())
      Attachment.Image(part, thumbnail)
    } catch (_: IOException) {
      null
    } catch (_: SecurityException) {
      null
    }
  }

  /** A thumbnail of an image from the chat history, long edge at most 512px; null when it cannot be decoded. */
  suspend fun thumbnail(part: ImagePart): ImageBitmap? {
    cachedThumbnail(part)?.let { return it }
    val decoded = withContext(Dispatchers.Default) {
      try {
        val bytes = Base64.decode(part.data, Base64.DEFAULT)
        decode(ImageDecoder.createSource(ByteBuffer.wrap(bytes)), THUMBNAIL_EDGE, software = false).asImageBitmap()
      } catch (_: IOException) {
        null
      } catch (_: IllegalArgumentException) {
        null
      }
    }
    if (decoded != null) {
      cache.put(part, decoded, decoded.aspectRatio())
      withContext(Dispatchers.Main.immediate) { decodeVersion++ }
    }
    return decoded
  }

  private fun ImageBitmap.aspectRatio(): Float = width.toFloat() / height.coerceAtLeast(1)

  /** Decodes [source] downsampled so its long edge is at most [maxEdge]; smaller images keep their size. */
  private fun decode(source: ImageDecoder.Source, maxEdge: Int, software: Boolean): Bitmap =
    ImageDecoder.decodeBitmap(source) { decoder, info, _ ->
      val width = info.size.width
      val height = info.size.height
      val longEdge = max(width, height)
      if (longEdge > maxEdge) {
        val scale = maxEdge.toFloat() / longEdge
        decoder.setTargetSize(
          (width * scale).roundToInt().coerceAtLeast(1),
          (height * scale).roundToInt().coerceAtLeast(1),
        )
      }
      if (software) decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
    }

  private fun flattenOnWhite(bitmap: Bitmap): Bitmap {
    if (!bitmap.hasAlpha()) return bitmap
    val flat = createBitmap(bitmap.width, bitmap.height)
    Canvas(flat).apply {
      drawColor(Color.WHITE)
      drawBitmap(bitmap, 0f, 0f, null)
    }
    return flat
  }
}
