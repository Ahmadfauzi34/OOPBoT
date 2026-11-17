package io.jeph.oopbot.localmodel.engine;

import io.jeph.oopbot.localmodel.engine.IInferenceListener;

/** The :inference process: it loads one model, once, and serves it until the main process kills it. */
interface IInferenceProcess {
  int pid();

  /** Starts the engine described by an EngineConfig's JSON; the outcome arrives through the listener. */
  oneway void start(String configJson, IInferenceListener listener);
}
