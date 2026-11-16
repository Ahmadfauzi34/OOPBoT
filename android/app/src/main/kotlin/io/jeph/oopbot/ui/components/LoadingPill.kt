package io.jeph.oopbot.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors

/** A pot and [text] in a pill, pinned over other content while something loads. */
@Composable
internal fun LoadingPill(text: String, modifier: Modifier = Modifier) {
  Row(
    modifier = modifier
      .clip(CircleShape)
      .background(oopbotColors.surface2)
      .padding(horizontal = OopbotTokens.Spacing.s12, vertical = OopbotTokens.Spacing.s8),
    verticalAlignment = Alignment.CenterVertically,
    horizontalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s8),
  ) {
    SimmeringPot(size = OopbotTokens.IconSize.sm)
    Text(text = text, color = oopbotColors.foregroundMuted, fontSize = OopbotTokens.FontSize.meta)
  }
}
