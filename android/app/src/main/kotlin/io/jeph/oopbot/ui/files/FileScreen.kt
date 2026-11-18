package io.jeph.oopbot.ui.files

import android.graphics.BitmapFactory
import android.text.format.Formatter
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.text.selection.DisableSelection
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDirection
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.jeph.oopbot.R
import io.jeph.oopbot.files.FileState
import io.jeph.oopbot.files.FileTab
import io.jeph.oopbot.host.FileContent
import io.jeph.oopbot.ui.components.OopbotSheet
import io.jeph.oopbot.ui.components.SheetRow
import io.jeph.oopbot.ui.components.SourceLinesCache
import io.jeph.oopbot.ui.components.fileLanguage
import io.jeph.oopbot.ui.components.rememberHighlightedCode
import io.jeph.oopbot.ui.components.splitLines
import io.jeph.oopbot.ui.remote.RemoteConnectAction
import io.jeph.oopbot.ui.settings.SegmentedControl
import io.jeph.oopbot.ui.settings.copyToClipboard
import io.jeph.oopbot.ui.sidebar.rememberFolderNames
import io.jeph.oopbot.ui.theme.OopbotFonts
import io.jeph.oopbot.ui.theme.OopbotTokens
import io.jeph.oopbot.ui.theme.oopbotColors
import io.jeph.oopbot.ui.workspace.PaneControls
import io.jeph.oopbot.ui.workspace.PaneHeader
import io.jeph.oopbot.ui.workspace.PublishTabActions
import io.jeph.oopbot.ui.workspace.TabAction
import io.jeph.oopbot.workspace.WorkspaceTab
import kotlin.math.max
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * One open file in a pane: the pane's header with the file's name over its folder, which acts through [controls]; a
 * caption with the file's size and line count; and the file itself, read again each time its tab shows, since Pi may
 * have changed it. Text shows with line numbers, coloured when a grammar knows the extension; images show fitted to
 * the width. Select text, Copy path and Close file go to the session sheet the title opens.
 */
@Composable
internal fun FileScreen(file: FileTab, controls: PaneControls, modifier: Modifier = Modifier) {
  val context = LocalContext.current
  val state by file.state.collectAsStateWithLifecycle()
  // On a remote workspace, the path on the remote host, where the user would look for the file.
  val fullPath = rememberFolderNames().absolutePath(file.cwd, file.path)
  val scope = rememberCoroutineScope()
  LaunchedEffect(file) { file.reload() }
  val loadedText = (state as? FileState.Loaded)?.content as? FileContent.Text
  val previewing = loadedText != null && isMarkdownFile(file.name) && loadedText.size <= PREVIEW_LIMIT &&
    file.showsPreview
  val sourceShown = loadedText != null && !previewing
  var selectionMode by remember(file) { mutableStateOf(false) }
  LaunchedEffect(sourceShown) { if (!sourceShown) selectionMode = false }
  BackHandler(enabled = selectionMode) { selectionMode = false }
  val tabActions = buildList {
    if (sourceShown) {
      add(
        TabAction(
          label = if (selectionMode) R.string.file_done_selecting else R.string.file_select_text,
          icon = if (selectionMode) R.drawable.ic_lucide_check else R.drawable.ic_lucide_file_text,
          onClick = { selectionMode = !selectionMode },
        ),
      )
    }
    add(
      TabAction(R.string.file_copy_path, R.drawable.ic_lucide_copy, {
        copyToClipboard(context, file.name, fullPath)
      }),
    )
    add(
      TabAction(R.string.file_close, R.drawable.ic_lucide_x, {
        controls.onCloseTab(WorkspaceTab.File(file))
      }, destructive = true),
    )
  }
  PublishTabActions(controls.shownTab, tabActions)

  Column(modifier = modifier.fillMaxSize().background(oopbotColors.surface0)) {
    PaneHeader(title = file.name, cwd = file.cwd, controls = controls, subfolder = file.folder)
    when (val shown = state) {
      FileState.Loading -> FileNote(Modifier.weight(1f)) {
        CircularProgressIndicator(
          modifier = Modifier.size(OopbotTokens.IconSize.md),
          color = oopbotColors.foregroundMuted,
          strokeWidth = 1.5.dp,
        )
        NoteText(stringResource(R.string.file_loading))
      }

      is FileState.Failed -> FileNote(Modifier.weight(1f)) {
        NoteText(stringResource(R.string.file_failed), oopbotColors.destructive)
        if (shown.message.isNotEmpty()) NoteText(shown.message)
        RemoteConnectAction(file.cwd, shown.code, onConnected = { scope.launch { file.reload() } })
      }

      is FileState.Loaded -> {
        // A fresh list per file, so switching files never carries the old one's scroll position over.
        key(file) { LoadedFile(file, shown.content, selectionMode, Modifier.weight(1f)) }
      }
    }
  }
}

@Composable
private fun LoadedFile(file: FileTab, content: FileContent, selectionMode: Boolean, modifier: Modifier) {
  val lines = (content as? FileContent.Text)?.let { rememberSourceLines(it.text, file.name) }
  val previewable = content is FileContent.Text && isMarkdownFile(file.name) && content.size <= PREVIEW_LIMIT
  FileBar(
    size = content.size,
    lineCount = lines?.size,
    showsPreview = file.showsPreview.takeIf { previewable },
    onShowPreview = { file.showsPreview = it },
  )
  when (content) {
    is FileContent.Text -> if (previewable && file.showsPreview) {
      MarkdownPreview(content.text, modifier)
    } else {
      SourceView(lines.orEmpty(), selectionMode, modifier)
    }

    is FileContent.Image -> ImageView(content.bytes, modifier)

    is FileContent.Binary -> FileNote(modifier) { NoteText(stringResource(R.string.file_binary)) }

    is FileContent.TooLarge -> FileNote(modifier) { NoteText(stringResource(R.string.file_too_large)) }
  }
}

/**
 * The file's caption: its size and, for text, its line count, small and muted; for markdown, the Preview and Source
 * switch at its end, [showsPreview] saying which is selected.
 */
@Composable
private fun FileBar(size: Long, lineCount: Int?, showsPreview: Boolean?, onShowPreview: (Boolean) -> Unit) {
  val colors = oopbotColors
  val context = LocalContext.current
  Row(
    modifier = Modifier
      .fillMaxWidth()
      .heightIn(min = 44.dp)
      .padding(horizontal = OopbotTokens.Spacing.s16, vertical = OopbotTokens.Spacing.s4),
    verticalAlignment = Alignment.CenterVertically,
    horizontalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s12),
  ) {
    Text(
      Formatter.formatShortFileSize(context, size),
      color = colors.foregroundMuted,
      fontSize = OopbotTokens.FontSize.sm,
      style = LocalTextStyle.current.copy(fontFeatureSettings = OopbotFonts.TABULAR),
    )
    if (lineCount != null) {
      Text(
        text = pluralStringResource(R.plurals.file_lines, lineCount, lineCount),
        color = colors.foregroundMuted,
        fontSize = OopbotTokens.FontSize.sm,
        style = LocalTextStyle.current.copy(fontFeatureSettings = OopbotFonts.TABULAR),
      )
    }
    if (showsPreview != null) {
      Spacer(Modifier.weight(1f))
      SegmentedControl(options = listOf(true, false), selected = showsPreview, onSelect = onShowPreview) {
          preview,
          selected,
        ->
        Text(
          text = stringResource(if (preview) R.string.file_preview else R.string.file_source),
          color = if (selected) colors.foreground else colors.foregroundMuted,
          fontSize = OopbotTokens.FontSize.sm,
          fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Medium,
        )
      }
    }
  }
}

/**
 * [text]'s lines, coloured for [name]'s extension up to a 1 MiB highlighting budget for phones and plain past
 * it. The lines are split off the main thread; a file ending in a line break has no empty line after it.
 */
@Composable
private fun rememberSourceLines(text: String, name: String): List<AnnotatedString> {
  val highlighted = rememberHighlightedCode(text, fileLanguage(name).takeIf { text.length <= HIGHLIGHT_LIMIT })
  val cached = remember(highlighted) { SourceLinesCache.shared.get(highlighted) }
  if (cached != null) return cached
  val lines by produceState<List<AnnotatedString>?>(null, highlighted) {
    value = SourceLinesCache.shared.get(highlighted) ?: withContext(Dispatchers.Default) {
      highlighted.splitLines().let { if (it.size > 1 && it.last().isEmpty()) it.dropLast(1) else it }
    }.also { SourceLinesCache.shared.put(highlighted, it) }
  }
  return lines.orEmpty()
}

/**
 * The source view: a 56dp gutter of right-aligned, faint line numbers and the lines, wrapped, in the code font.
 * Selection is opt-in via [selectionMode] so fast scrolling stays free of global SelectionRegistrar overhead.
 */
@Composable
private fun SourceView(lines: List<AnnotatedString>, selectionMode: Boolean, modifier: Modifier) {
  val colors = oopbotColors
  val baseStyle = LocalTextStyle.current
  val gutterStyle = remember(colors) {
    TextStyle(
      color = colors.foregroundExtraMuted,
      fontFamily = OopbotFonts.mono,
      fontSize = OopbotTokens.FontSize.code,
      lineHeight = OopbotTokens.LineHeight.code,
      textAlign = TextAlign.End,
    )
  }
  val lineStyle = remember(colors, baseStyle) {
    baseStyle.copy(
      color = colors.foreground,
      fontFamily = OopbotFonts.mono,
      fontSize = OopbotTokens.FontSize.code,
      lineHeight = OopbotTokens.LineHeight.code,
      // Code reads left to right in any language, so its punctuation stays where it was written.
      textDirection = TextDirection.Ltr,
    )
  }
  val navigationBar = WindowInsets.navigationBars.asPaddingValues()
  val contentPadding = PaddingValues(
    top = OopbotTokens.Spacing.s8,
    bottom = OopbotTokens.Spacing.s16 + navigationBar.calculateBottomPadding(),
  )
  if (selectionMode) {
    SelectionContainer(modifier = modifier) {
      SourceList(lines, selectionMode = true, gutterStyle, lineStyle, contentPadding, Modifier.fillMaxSize())
    }
  } else {
    SourceList(lines, selectionMode = false, gutterStyle, lineStyle, contentPadding, modifier.fillMaxSize())
  }
}

@Composable
private fun SourceList(
  lines: List<AnnotatedString>,
  selectionMode: Boolean,
  gutterStyle: TextStyle,
  lineStyle: TextStyle,
  contentPadding: PaddingValues,
  modifier: Modifier,
) {
  LazyColumn(modifier = modifier, contentPadding = contentPadding) {
    itemsIndexed(lines, key = { index, _ -> index }, contentType = { _, _ -> "line" }) { index, line ->
      Row(modifier = Modifier.fillMaxWidth()) {
        val gutterModifier = Modifier.width(GUTTER_WIDTH).padding(end = OopbotTokens.Spacing.s12)
        if (selectionMode) {
          DisableSelection { Text(text = (index + 1).toString(), modifier = gutterModifier, style = gutterStyle) }
        } else {
          Text(text = (index + 1).toString(), modifier = gutterModifier, style = gutterStyle)
        }
        Text(
          text = line,
          modifier = Modifier.weight(1f).padding(end = OopbotTokens.Spacing.s12),
          style = lineStyle,
        )
      }
    }
  }
}

/** The image fitted to the width; a format Android can't decode says so, as a binary file does. */
@Composable
private fun ImageView(bytes: ByteArray, modifier: Modifier) {
  val image by produceState<ImageResult>(ImageResult.Decoding, bytes) {
    value =
      withContext(Dispatchers.Default) { decodeImage(bytes)?.let(ImageResult::Decoded) ?: ImageResult.Undecodable }
  }
  when (val shown = image) {
    ImageResult.Decoding -> Box(modifier = modifier.fillMaxSize())

    ImageResult.Undecodable -> FileNote(modifier) { NoteText(stringResource(R.string.file_binary)) }

    is ImageResult.Decoded -> Box(
      modifier = modifier.fillMaxSize().padding(OopbotTokens.Spacing.s12),
      contentAlignment = Alignment.Center,
    ) {
      Image(
        bitmap = shown.bitmap,
        contentDescription = null,
        modifier = Modifier.fillMaxWidth(),
        contentScale = ContentScale.Fit,
      )
    }
  }
}

private sealed interface ImageResult {
  data object Decoding : ImageResult

  data object Undecodable : ImageResult

  class Decoded(val bitmap: ImageBitmap) : ImageResult
}

/** [bytes] decoded at no more than [MAX_IMAGE_SIDE] pixels a side, so a camera photo can't exhaust memory. */
private fun decodeImage(bytes: ByteArray): ImageBitmap? {
  val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
  BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
  if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
  var sample = 1
  while (max(bounds.outWidth, bounds.outHeight) / sample > MAX_IMAGE_SIDE) sample *= 2
  val options = BitmapFactory.Options().apply { inSampleSize = sample }
  return BitmapFactory.decodeByteArray(bytes, 0, bytes.size, options)?.asImageBitmap()
}

/** A file tab's actions: copy the file's full path, or close the tab. */
@Composable
internal fun FileActionsSheet(file: FileTab, onClose: () -> Unit, onDismiss: () -> Unit) {
  val context = LocalContext.current
  val fullPath = rememberFolderNames().absolutePath(file.cwd, file.path)
  OopbotSheet(onDismissRequest = onDismiss) { dismiss ->
    SheetRow(
      label = stringResource(R.string.file_copy_path),
      icon = R.drawable.ic_lucide_copy,
      onClick = { dismiss { copyToClipboard(context, file.name, fullPath) } },
    )
    SheetRow(
      label = stringResource(R.string.file_close),
      icon = R.drawable.ic_lucide_x,
      labelColor = oopbotColors.destructive,
      onClick = { dismiss(onClose) },
    )
  }
}

@Composable
private fun FileNote(modifier: Modifier, content: @Composable () -> Unit) {
  Column(
    modifier = modifier.fillMaxSize().padding(OopbotTokens.Spacing.s16),
    verticalArrangement = Arrangement.spacedBy(OopbotTokens.Spacing.s8, Alignment.CenterVertically),
    horizontalAlignment = Alignment.CenterHorizontally,
  ) {
    content()
  }
}

@Composable
private fun NoteText(text: String, color: Color = oopbotColors.foregroundMuted) {
  Text(text = text, color = color, fontSize = OopbotTokens.FontSize.base, textAlign = TextAlign.Center)
}

/** Files up to 1 MiB are highlighted on phones, and bigger ones show plain. */
private const val HIGHLIGHT_LIMIT = 1024 * 1024

/**
 * Markdown up to this size can be previewed; bigger files show only their source. The renderer composes a whole
 * document at once, and a table of 64 KiB already takes half the app's heap, so a bigger one could crash it.
 */
private const val PREVIEW_LIMIT = 64 * 1024

private const val MAX_IMAGE_SIDE = 2048

private val GUTTER_WIDTH = 56.dp
