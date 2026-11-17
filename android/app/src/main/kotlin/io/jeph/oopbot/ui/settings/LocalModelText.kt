package io.jeph.oopbot.ui.settings

import android.content.Context
import android.text.format.Formatter
import androidx.annotation.StringRes
import io.jeph.oopbot.R
import io.jeph.oopbot.localmodel.ModelArtifact
import io.jeph.oopbot.localmodel.RuntimeState
import io.jeph.oopbot.localmodel.catalog.DownloadFailure
import io.jeph.oopbot.localmodel.engine.Backend
import io.jeph.oopbot.localmodel.engine.EngineKind

/** "LiteRT-LM on the CPU, 614 MB". */
internal fun buildLabel(context: Context, artifact: ModelArtifact): String = context.getString(
  R.string.local_models_build,
  context.getString(artifact.engine.label),
  context.getString(artifact.backend.label),
  Formatter.formatShortFileSize(context, artifact.sizeBytes),
)

internal fun failureText(context: Context, failure: DownloadFailure): String = when (failure) {
  is DownloadFailure.NotEnoughSpace ->
    context.getString(R.string.local_models_error_space, Formatter.formatShortFileSize(context, failure.neededBytes))

  DownloadFailure.Network -> context.getString(R.string.local_models_error_network)

  is DownloadFailure.Http -> context.getString(R.string.local_models_error_http, failure.status)

  DownloadFailure.Corrupt -> context.getString(R.string.local_models_error_corrupt)
}

/** The model the runtime holds or is loading, if any. */
internal val RuntimeState.modelId: String?
  get() = when (this) {
    RuntimeState.Stopped -> null
    is RuntimeState.Loading -> modelId
    is RuntimeState.Ready -> modelId
    is RuntimeState.Failed -> modelId
  }

internal val EngineKind.label: Int
  @StringRes get() = when (this) {
    EngineKind.LLAMA -> R.string.local_models_engine_llama
    EngineKind.LITERT -> R.string.local_models_engine_litert
  }

internal val Backend.label: Int
  @StringRes get() = when (this) {
    Backend.CPU -> R.string.local_models_backend_cpu
    Backend.GPU -> R.string.local_models_backend_gpu
    Backend.NPU -> R.string.local_models_backend_npu
  }
