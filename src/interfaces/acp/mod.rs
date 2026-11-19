//! Adapter Layer 3: Agent Client Protocol (ACP) Client & OOPBoT RPC Bridge.
//!
//! Memungkinkan OOPBoT terhubung ke external CLI agent yang mendukung standar ACP
//! (seperti `agy acp`) melalui JSON-RPC 2.0 di atas `stdin`/`stdout`, sambil menerjemahkan
//! event streaming, tool call, plan, dan permission approval ke protokol JSONL OOPBoT.

pub mod bridge;
pub mod client;
pub mod protocol;

#[cfg(test)]
mod tests;

pub use bridge::{AcpBridgeConfig, AcpRpcBridge};
pub use client::{AcpAgentEvent, AcpClient, AcpLaunchConfig};
pub use protocol::{
    AcpPermissionOption, AcpPermissionRequest, AcpPlanEntry, AcpSessionUpdate, IncomingJsonRpc,
    JsonRpcError, ACP_PROTOCOL_VERSION,
};
