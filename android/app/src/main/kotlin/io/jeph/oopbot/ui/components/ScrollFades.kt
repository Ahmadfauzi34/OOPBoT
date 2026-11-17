package io.jeph.oopbot.ui.components

import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithCache
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/**
 * Fades the top of a list into [color] once it's scrolled and the bottom while rows lie below. Painted in the
 * background's colour over the rows rather than as a transparency mask, which would draw the whole list offscreen;
 * [state] is read while drawing and gradient brushes are cached across scroll frames, so scrolling redraws the fades
 * without recomposing or allocating shaders.
 */
internal fun Modifier.scrollFades(state: LazyListState, color: Color): Modifier = drawWithCache {
  val fade = SCROLL_FADE.toPx().coerceAtMost(size.height / 2)
  val fadeSize = Size(size.width, fade)
  val bottomTop = size.height - fade
  val topBrush = Brush.verticalGradient(listOf(color, Color.Transparent), endY = fade)
  val bottomBrush = Brush.verticalGradient(listOf(Color.Transparent, color), startY = bottomTop, endY = size.height)
  onDrawWithContent {
    drawContent()
    if (state.canScrollBackward) drawRect(topBrush, size = fadeSize)
    if (state.canScrollForward) drawRect(bottomBrush, topLeft = Offset(0f, bottomTop), size = fadeSize)
  }
}

private val SCROLL_FADE = 40.dp
