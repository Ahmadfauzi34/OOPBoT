package io.jeph.oopbot.localmodel.engine

/**
 * Asks the vendor's OpenCL driver, if OOPBoT can load one, for the phone's GPU. Both LiteRT-LM's GPU backend and
 * llama.cpp's run on OpenCL, and llama.cpp's only on Adreno GPUs, so the GPU's name decides which can run here. The
 * library loads the driver only when asked, so a phone without one still runs everything else.
 */
internal object OpenClProbe {
  init {
    System.loadLibrary("oopbot-opencl-probe")
  }

  /** `CL_DEVICE_NAME` of the first GPU OpenCL reports, such as `QUALCOMM Adreno(TM) 840`, or null when there's none. */
  @JvmStatic
  external fun gpuName(): String?
}
