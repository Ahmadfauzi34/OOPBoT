package io.jeph.oopbot.ui.settings

import android.icu.text.ListFormatter
import androidx.annotation.DrawableRes
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.jeph.oopbot.R
import io.jeph.oopbot.data.RemoteHosts
import io.jeph.oopbot.host.HostRejected
import io.jeph.oopbot.host.RemoteAuth
import io.jeph.oopbot.host.keyComment
import io.jeph.oopbot.host.keyFingerprint
import io.jeph.oopbot.ui.components.DialogConfirm
import io.jeph.oopbot.ui.components.LucideIcon
import io.jeph.oopbot.ui.components.OopbotDialog
import io.jeph.oopbot.ui.sidebar.pathTextStyle
import io.jeph.oopbot.ui.theme.OopbotFonts
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import java.io.IOException
import kotlinx.coroutines.launch

/**
 * OOPBoT's key: its comment, fingerprint and the hosts that sign in with it, then Copy public key and Make a new key.
 * Shown once the key exists, which the first key host's editor makes, so opening the list never makes one.
 */
@Composable
internal fun RemoteKeySection(remoteHosts: RemoteHosts) {
  val scope = rememberCoroutineScope()
  val remotes by remoteHosts.state.collectAsStateWithLifecycle()
  val key = remotes.publicKey ?: return
  val keyHosts = remotes.hosts.filter { it.auth == RemoteAuth.Key }.map { it.name }
  val names = ListFormatter.getInstance(LocalConfiguration.current.locales[0]).format(keyHosts)
  val failed = stringResource(R.string.remote_key_new_failed)
  var confirming by remember { mutableStateOf(false) }
  var making by remember { mutableStateOf(false) }
  var made by remember { mutableStateOf<String?>(null) }
  var error by remember { mutableStateOf<String?>(null) }
  KeyCard(
    key = key,
    usedBy = if (keyHosts.isEmpty()) {
      stringResource(R.string.remote_key_unused)
    } else {
      pluralStringResource(R.plurals.remote_key_used_by, keyHosts.size, keyHosts.size, names)
    },
    making = making,
    onNewKey = {
      error = null
      confirming = true
    },
  )
  error?.let { SettingsCaption(text = it, color = oopbotColors.destructive) }
  if (confirming) {
    NewKeyDialog(usedByHosts = keyHosts.isNotEmpty(), onDismiss = { confirming = false }) {
      confirming = false
      making = true
      scope.launch {
        try {
          made = remoteHosts.newKey()
        } catch (rejected: HostRejected) {
          error = rejected.error.ifEmpty { failed }
        } catch (failure: IOException) {
          error = failure.message?.ifEmpty { null } ?: failed
        } finally {
          making = false
        }
      }
    }
  }
  made?.let { newKey -> KeyMadeDialog(newKey, usedByHosts = keyHosts.isNotEmpty(), onDismiss = { made = null }) }
}

/** The card itself: who the key is, Copy public key, and Make a new key, off while one is being made. */
@Composable
private fun KeyCard(key: String, usedBy: String, making: Boolean, onNewKey: () -> Unit) {
  val context = LocalContext.current
  val label = stringResource(R.string.remote_key_label)
  SettingsSection(title = stringResource(R.string.remote_key_section)) {
    KeyIdentity(
      comment = keyComment(key).ifEmpty { stringResource(R.string.remote_auth_key) },
      fingerprint = keyFingerprint(key),
      usedBy = usedBy,
    )
    SettingsDivider()
    SettingsRow(
      title = stringResource(R.string.remote_copy_key),
      titleColor = oopbotColors.accent,
      onClick = { copyToClipboard(context, label, key) },
      leading = { AccentIcon(R.drawable.ic_lucide_copy) },
    )
    SettingsDivider()
    SettingsRow(
      title = stringResource(R.string.remote_key_new),
      subtitle = stringResource(R.string.remote_key_new_detail),
      titleColor = oopbotColors.accent,
      enabled = !making,
      onClick = onNewKey,
      leading = { AccentIcon(R.drawable.ic_lucide_refresh_cw) },
    )
  }
}

/** Asks before replacing the key, saying the hosts that use it, if [usedByHosts], refuse OOPBoT until updated. */
@Composable
private fun NewKeyDialog(usedByHosts: Boolean, onDismiss: () -> Unit, onConfirm: () -> Unit) {
  OopbotDialog(
    title = stringResource(R.string.remote_key_new_title),
    text = stringResource(if (usedByHosts) R.string.remote_key_new_text else R.string.remote_key_new_text_unused),
    icon = R.drawable.ic_lucide_key_round,
    confirm = DialogConfirm(
      label = stringResource(R.string.remote_key_new_confirm),
      onClick = onConfirm,
      destructive = usedByHosts,
    ),
    dismissLabel = stringResource(R.string.remote_cancel),
    onDismiss = onDismiss,
  )
}

/** The new key's line to copy, and, if [usedByHosts], that the hosts that used the old key need it. */
@Composable
private fun KeyMadeDialog(newKey: String, usedByHosts: Boolean, onDismiss: () -> Unit) {
  val context = LocalContext.current
  val label = stringResource(R.string.remote_key_label)
  OopbotDialog(
    title = stringResource(R.string.remote_key_made_title),
    text = stringResource(if (usedByHosts) R.string.remote_key_made_text else R.string.remote_key_made_text_unused),
    icon = R.drawable.ic_lucide_key_round,
    confirm = DialogConfirm(
      label = stringResource(R.string.remote_copy_key),
      onClick = {
        copyToClipboard(context, label, newKey)
        onDismiss()
      },
    ),
    dismissLabel = stringResource(R.string.remote_done),
    onDismiss = onDismiss,
  ) {
    KeyLine(newKey)
  }
}

/** The key's comment, its fingerprint in the code font, and which hosts sign in with it. */
@Composable
private fun KeyIdentity(comment: String, fingerprint: String?, usedBy: String) {
  val colors = oopbotColors
  Row(
    modifier = Modifier
      .fillMaxWidth()
      .heightIn(min = 54.dp)
      .padding(horizontal = OopbotTokens.Spacing.s16, vertical = OopbotTokens.Spacing.s8),
    horizontalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s12),
  ) {
    Box(modifier = Modifier.padding(top = 2.dp).size(20.dp), contentAlignment = Alignment.Center) {
      LucideIcon(icon = R.drawable.ic_lucide_key_round, contentDescription = null, tint = colors.foregroundMuted)
    }
    Column(modifier = Modifier.weight(1f)) {
      Text(
        text = comment,
        color = colors.foreground,
        fontSize = OopbotTokens.FontSize.base,
        fontWeight = FontWeight.Medium,
      )
      fingerprint?.let {
        Text(
          text = it,
          style = pathTextStyle,
          color = colors.foregroundMuted,
          fontFamily = OopbotFonts.mono,
          fontSize = OopbotTokens.FontSize.code,
        )
      }
      Text(text = usedBy, color = colors.foregroundMuted, fontSize = OopbotTokens.FontSize.sm)
    }
  }
}

/** A public key line in the code font, on the dialog's surface. */
@Composable
private fun KeyLine(line: String) {
  val colors = oopbotColors
  Text(
    text = line,
    modifier = Modifier
      .fillMaxWidth()
      .padding(top = OopbotTokens.Spacing.s8)
      .background(colors.surface0, RoundedCornerShape(OopbotTokens.Radius.md))
      .padding(horizontal = OopbotTokens.Spacing.s12, vertical = OopbotTokens.Spacing.s8),
    style = pathTextStyle,
    color = colors.foreground,
    fontFamily = OopbotFonts.mono,
    fontSize = OopbotTokens.FontSize.code,
    lineHeight = OopbotTokens.LineHeight.code,
  )
}

@Composable
private fun AccentIcon(@DrawableRes icon: Int) {
  LucideIcon(icon = icon, contentDescription = null, tint = oopbotColors.accent)
}
