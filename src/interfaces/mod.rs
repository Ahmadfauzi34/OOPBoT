//! Layer 3: Interface Adapters — titik masuk untuk semua antarmuka eksternal OOPBoT.
//!
//! Setiap subfolder di sini adalah adapter tipis yang menerjemahkan protokol I/O
//! (terminal CLI, JSONL RPC, TCP host bridge, atau C-ABI/JNI FFI) ke layer inti
//! (`agent`, `plugins`, `tools`, `config`, `llm`).

pub mod acp;
pub mod cli;
pub mod ffi;
pub mod host;
pub mod rpc;
