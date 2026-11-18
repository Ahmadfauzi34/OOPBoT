package io.jeph.oopbot.data

import io.jeph.oopbot.agent.typedText
import java.util.Locale
import org.json.JSONArray
import org.json.JSONObject

/**
 * Extracts compact searchable plaintext from Pi `messages` JSON arrays (skipping base64 images and `<skill>` wrappers)
 * and turns user queries into safe SQLite FTS4 `MATCH` prefix queries.
 */
internal object TranscriptIndexer {
  private const val MAX_BODY_CHARS = 250_000
  private const val MAX_BASH_OUTPUT_CHARS = 16_000
  private const val MAX_QUERY_TOKENS = 16

  fun extractBody(messages: JSONArray): String {
    val sb = StringBuilder()
    for (index in 0 until messages.length()) {
      if (sb.length >= MAX_BODY_CHARS) break
      val message = messages.optJSONObject(index) ?: continue
      appendMessage(sb, message)
    }
    return sb.toString().trim()
  }

  /**
   * Builds a safe FTS4 Standard Query Syntax prefix-AND expression (`foo* bar*`) from [rawQuery], or null when no
   * letter or digit token is present. Tokens are lowercased to match `unicode61` folding and keep `AND`/`OR`/`NOT`/
   * `NEAR` words from being misparsed as query operators.
   */
  fun buildFtsQuery(rawQuery: String): String? {
    val tokens = tokenize(rawQuery)
    if (tokens.isEmpty()) return null
    return tokens.joinToString(" ") { "$it*" }
  }

  private fun appendMessage(sb: StringBuilder, message: JSONObject) {
    when (message.optString("role")) {
      "user" -> {
        val blocks = message.optJSONArray("content")
        val raw = blocks?.let(::blocksText) ?: (message.opt("content") as? String).orEmpty()
        appendChunk(sb, typedText(raw))
      }

      "assistant" -> appendAssistant(sb, message)

      "bashExecution" -> {
        appendChunk(sb, message.optString("command"))
        appendChunk(sb, message.optString("output").take(MAX_BASH_OUTPUT_CHARS))
      }

      "custom" -> {
        if (message.optBoolean("display")) {
          val blocks = message.optJSONArray("content")
          val text = blocks?.let(::blocksText) ?: (message.opt("content") as? String).orEmpty()
          appendChunk(sb, text)
        }
      }
    }
  }

  private fun appendAssistant(sb: StringBuilder, message: JSONObject) {
    val blocks = message.optJSONArray("content")
    if (blocks != null) {
      for (index in 0 until blocks.length()) {
        val block = blocks.optJSONObject(index) ?: continue
        when (block.optString("type")) {
          "text" -> appendChunk(sb, block.optString("text"))

          "thinking" -> appendChunk(sb, block.optString("thinking"))

          "toolCall" -> {
            appendChunk(sb, block.optString("name"))
            val args = block.optJSONObject("arguments")
            if (args != null) {
              if (!args.isNull("command")) appendChunk(sb, args.optString("command"))
              if (!args.isNull("path")) appendChunk(sb, args.optString("path"))
            }
          }
        }
      }
    }
    if (message.optString("stopReason") == "error" && !message.isNull("errorMessage")) {
      appendChunk(sb, message.optString("errorMessage"))
    }
  }

  private fun blocksText(blocks: JSONArray): String = buildString {
    for (index in 0 until blocks.length()) {
      val block = blocks.optJSONObject(index) ?: continue
      if (block.optString("type") == "text") {
        val text = block.optString("text")
        if (text.isNotBlank()) {
          if (isNotEmpty()) append('\n')
          append(text)
        }
      }
    }
  }

  private fun appendChunk(sb: StringBuilder, chunk: String) {
    if (sb.length >= MAX_BODY_CHARS || chunk.isBlank()) return
    if (sb.isNotEmpty()) sb.append('\n')
    val remaining = MAX_BODY_CHARS - sb.length
    sb.append(if (chunk.length <= remaining) chunk else chunk.take(remaining))
  }

  private fun tokenize(rawQuery: String): List<String> {
    val tokens = LinkedHashSet<String>()
    val current = StringBuilder()
    var index = 0
    while (index < rawQuery.length && tokens.size < MAX_QUERY_TOKENS) {
      val cp = rawQuery.codePointAt(index)
      if (Character.isLetterOrDigit(cp)) {
        current.appendCodePoint(cp)
      } else if (current.isNotEmpty()) {
        tokens += current.toString().lowercase(Locale.ROOT)
        current.setLength(0)
      }
      index += Character.charCount(cp)
    }
    if (current.isNotEmpty() && tokens.size < MAX_QUERY_TOKENS) {
      tokens += current.toString().lowercase(Locale.ROOT)
    }
    return tokens.toList()
  }
}
