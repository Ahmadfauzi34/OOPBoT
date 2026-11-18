package io.jeph.oopbot.ui

import androidx.compose.animation.AnimatedVisibilityScope
import androidx.compose.animation.ContentTransform
import androidx.compose.animation.EnterExitState
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.IntOffset

/**
 * How a stack of screens moves from depth [from] to depth [to]: a deeper screen slides in from the right over the one
 * below, which shifts a quarter width for depth, and a popped screen slides back out over the one it uncovers. The same
 * depth cross-fades. Same timing as the sidebar panels.
 *
 * AnimatedContent keeps the z-index a screen entered with, so every transition sets it to the screen's stack depth: a
 * deeper screen always draws over a shallower one, however it got there.
 */
internal fun stackTransition(from: Int, to: Int): ContentTransform {
  val slide = tween<IntOffset>(durationMillis = SCREEN_SLIDE_MILLIS, easing = FastOutSlowInEasing)
  val fade = tween<Float>(durationMillis = SCREEN_SLIDE_MILLIS, easing = FastOutSlowInEasing)
  val transform = when {
    to > from -> slideInHorizontally(slide) { it } togetherWith slideOutHorizontally(slide) { -it / 4 }
    to < from -> slideInHorizontally(slide) { -it / 4 } togetherWith slideOutHorizontally(slide) { it }
    else -> fadeIn(fade) togetherWith fadeOut(fade)
  }
  return transform.apply { targetContentZIndex = to.toFloat() }
}

/**
 * Whether every enclosing [stackTransition] has settled on screen in [EnterExitState.Visible]. Expensive refreshes or
 * full-screen stream mounts wait for this so the 220ms slide animation stays frame-steady.
 */
internal val LocalTransitionSettled = compositionLocalOf { true }

/**
 * Gives an animated stack destination a persistent hardware display-list layer for horizontal slides and publishes
 * transition settlement via [LocalTransitionSettled].
 */
@Composable
internal fun AnimatedVisibilityScope.TransitionFrame(
  modifier: Modifier = Modifier,
  content: @Composable () -> Unit,
) {
  val settled = LocalTransitionSettled.current &&
    transition.currentState == transition.targetState &&
    transition.targetState == EnterExitState.Visible
  CompositionLocalProvider(LocalTransitionSettled provides settled) {
    Box(modifier = modifier.fillMaxSize().graphicsLayer()) {
      content()
    }
  }
}

private const val SCREEN_SLIDE_MILLIS = 220
