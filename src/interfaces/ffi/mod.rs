//! FFI (C-ABI + Android JNI) untuk `liboopbot.so` (`cdylib`).
//!
//! Menyediakan:
//! 1. **C-ABI (`oopbot_*`)**: untuk pemanggilan dari C/C++, JNA, Python, atau Node.
//! 2. **Android JNI (`Java_io_jeph_oopbot_host_OopbotBridge_*`)**: untuk pemanggilan
//!    langsung dari aplikasi Android Kotlin (`OopbotBridge.kt`) lewat `System.loadLibrary("oopbot")`
//!    tanpa memerlukan dependensi crate tambahan.

mod c_abi;
mod dispatch;
mod jni;

#[cfg(test)]
mod tests;

pub use c_abi::{
    oopbot_engine_free, oopbot_engine_new, oopbot_engine_request, oopbot_reference_catalog,
    oopbot_string_free, oopbot_version,
};
pub(crate) use dispatch::dispatch_request;
pub use jni::{
    Java_io_jeph_oopbot_host_OopbotBridge_engineFree,
    Java_io_jeph_oopbot_host_OopbotBridge_engineNew,
    Java_io_jeph_oopbot_host_OopbotBridge_engineRequest,
    Java_io_jeph_oopbot_host_OopbotBridge_referenceCatalog,
    Java_io_jeph_oopbot_host_OopbotBridge_version, JBoolean, JLong, JObject, JString, JniEnv,
    JniNativeInterface,
};

use crate::agent::{AgentHarness, Approver, NoopEventSink};
use crate::llm::LlmClient;
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use serde_json::Value;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

struct AutoOrDenyApprover {
    allow: bool,
}

#[async_trait]
impl Approver for AutoOrDenyApprover {
    async fn approve(&self, _tool_name: &str, _preview: &str) -> bool {
        self.allow
    }
}

pub struct OopbotHandle {
    pub(super) runtime: tokio::runtime::Runtime,
    pub(super) agent: AgentHarness,
}

pub(super) fn to_c_string(val: Value) -> *mut c_char {
    let s = val.to_string();
    CString::new(s)
        .unwrap_or_else(|_| CString::new("{\"ok\":false,\"error\":\"null byte\"}").unwrap())
        .into_raw()
}

pub(super) unsafe fn from_c_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
}

pub(super) fn create_handle(
    api_key: String,
    default_model: String,
    auto_approve: bool,
) -> *mut OopbotHandle {
    if api_key.trim().is_empty() {
        return std::ptr::null_mut();
    }
    let model = if default_model.trim().is_empty() {
        "gemini-3.5-flash-lite".to_string()
    } else {
        default_model
    };

    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        return std::ptr::null_mut();
    };

    let agent_model = model.clone();
    let Ok(client) = LlmClient::new(api_key, model) else {
        return std::ptr::null_mut();
    };

    let agent = AgentHarness::with_event_sink(
        "OOPBoT",
        client,
        ToolRegistry::with_defaults(),
        Box::new(AutoOrDenyApprover {
            allow: auto_approve,
        }),
        Box::new(NoopEventSink),
        25,
    );
    agent.set_active_model(&agent_model);

    Box::into_raw(Box::new(OopbotHandle { runtime, agent }))
}
