//! FFI (C-ABI) untuk `liboopbot.so` (`cdylib`).
//!
//! Memungkinkan aplikasi Android (Kotlin/Java via JNA/JNI, C/C++, atau Python/Node)
//! memanggil seluruh engine `OOPBoT` secara in-process lewat protokol JSON satu pintu:
//!
//! - `oopbot_version() -> *const c_char`
//! - `oopbot_engine_new(api_key, default_model, auto_approve) -> *mut OopbotHandle`
//! - `oopbot_engine_request(handle, request_json) -> *mut c_char`
//! - `oopbot_string_free(ptr)`
//! - `oopbot_engine_free(handle)`
//!
//! Operasi JSON yang didukung `oopbot_engine_request` (mirip `utility` & `rpc` di Pipkin):
//! - `{"op": "models"}` → daftar model & model aktif
//! - `{"op": "set_model", "model": "..."}` → ganti model tab aktif
//! - `{"op": "tabs"}` → daftar semua tab & tab aktif
//! - `{"op": "new_tab", "model": "..."}` → buat tab baru
//! - `{"op": "switch_tab", "tab": "1"}` → pindah tab
//! - `{"op": "close_tab", "tab": "2"}` → tutup tab
//! - `{"op": "timeline"}` → ambil `TimelineEntry` terlipat (`StepsFold`) tab aktif
//! - `{"op": "chat", "prompt": "..."}` → jalankan 1 giliran agen & kembalikan balasan + timeline

use crate::agent::{Agent, Approver, GeminiAgent};
use crate::events::{AgentEvent, EventSink};
use crate::gemini::GeminiClient;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

struct AutoOrDenyApprover {
    allow: bool,
}

impl Approver for AutoOrDenyApprover {
    fn approve(&self, _tool_name: &str, _preview: &str) -> bool {
        self.allow
    }
}

struct SilentEventSink;

impl EventSink for SilentEventSink {
    fn on_event(&self, _event: &AgentEvent) {}
}

pub struct OopbotHandle {
    runtime: tokio::runtime::Runtime,
    agent: GeminiAgent,
}

fn to_c_string(val: Value) -> *mut c_char {
    let s = val.to_string();
    CString::new(s)
        .unwrap_or_else(|_| CString::new("{\"ok\":false,\"error\":\"null byte\"}").unwrap())
        .into_raw()
}

unsafe fn from_c_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
}

/// Versi statis library `liboopbot.so`.
#[no_mangle]
pub extern "C" fn oopbot_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Buat instance engine `OopbotHandle` baru di dalam memori `.so`.
///
/// # Safety
/// `api_key` dan `default_model` harus pointer C-string valid (UTF-8, null-terminated).
#[no_mangle]
pub unsafe extern "C" fn oopbot_engine_new(
    api_key: *const c_char,
    default_model: *const c_char,
    auto_approve: bool,
) -> *mut OopbotHandle {
    let Some(key) = from_c_str(api_key).filter(|s| !s.trim().is_empty()) else {
        return std::ptr::null_mut();
    };
    let model = from_c_str(default_model)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gemini-3.5-flash-lite".to_string());

    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        return std::ptr::null_mut();
    };

    let Ok(client) = GeminiClient::new(key, model) else {
        return std::ptr::null_mut();
    };

    let agent = GeminiAgent::with_event_sink(
        "OOPBoT",
        client,
        ToolRegistry::with_defaults(),
        Box::new(AutoOrDenyApprover {
            allow: auto_approve,
        }),
        Box::new(SilentEventSink),
        25,
    );

    Box::into_raw(Box::new(OopbotHandle { runtime, agent }))
}

/// Kirim perintah JSON ke engine dan terima respons JSON (`*mut c_char`).
/// Bebaskan string hasil dengan `oopbot_string_free`.
///
/// # Safety
/// `handle` harus pointer valid dari `oopbot_engine_new`, `request_json` C-string valid.
#[no_mangle]
pub unsafe extern "C" fn oopbot_engine_request(
    handle: *mut OopbotHandle,
    request_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return to_c_string(json!({ "ok": false, "error": "null handle" }));
    }
    let h = &*handle;
    let Some(raw) = from_c_str(request_json) else {
        return to_c_string(json!({ "ok": false, "error": "null request_json" }));
    };
    let Ok(req) = serde_json::from_str::<Value>(&raw) else {
        return to_c_string(json!({ "ok": false, "error": "invalid JSON" }));
    };

    let resp = dispatch_request(h, &req);
    to_c_string(resp)
}

fn dispatch_request(h: &OopbotHandle, req: &Value) -> Value {
    match req["op"].as_str().unwrap_or("") {
        "models" => json!({
            "ok": true,
            "active_model": h.agent.model(),
            "models": h.agent.list_models(),
        }),
        "set_model" => {
            let Some(m) = req["model"].as_str() else {
                return json!({ "ok": false, "error": "missing 'model'" });
            };
            let active = h.agent.set_active_model(m);
            json!({ "ok": true, "active_model": active })
        }
        "tabs" => {
            let (active_index, tabs) = h.agent.list_tabs();
            json!({ "ok": true, "active_index": active_index, "tabs": tabs })
        }
        "new_tab" => {
            let tab = h.agent.new_tab(req["model"].as_str());
            json!({ "ok": true, "tab": tab })
        }
        "switch_tab" => {
            let Some(sel) = req["tab"].as_str() else {
                return json!({ "ok": false, "error": "missing 'tab'" });
            };
            match h.agent.switch_tab(sel) {
                Some(tab) => json!({ "ok": true, "tab": tab }),
                None => json!({ "ok": false, "error": "tab not found" }),
            }
        }
        "close_tab" => {
            let Some(sel) = req["tab"].as_str() else {
                return json!({ "ok": false, "error": "missing 'tab'" });
            };
            json!({ "ok": h.agent.close_tab(sel) })
        }
        "timeline" => json!({
            "ok": true,
            "timeline": h.agent.active_timeline(),
        }),
        "clear" => {
            h.agent.clear_memory();
            json!({ "ok": true })
        }
        "chat" => {
            let Some(prompt) = req["prompt"].as_str() else {
                return json!({ "ok": false, "error": "missing 'prompt'" });
            };
            match h.runtime.block_on(h.agent.chat(prompt)) {
                Ok(reply) => json!({
                    "ok": true,
                    "reply": reply,
                    "timeline": h.agent.active_timeline(),
                }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        other => json!({ "ok": false, "error": format!("unknown op '{}'", other) }),
    }
}

/// Bebaskan string hasil `oopbot_engine_request`.
///
/// # Safety
/// `ptr` harus berasal dari `oopbot_engine_request`.
#[no_mangle]
pub unsafe extern "C" fn oopbot_string_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        let _ = CString::from_raw(ptr);
    }
}

/// Hancurkan instance `OopbotHandle`.
///
/// # Safety
/// `handle` harus berasal dari `oopbot_engine_new` dan belum pernah dibebaskan.
#[no_mangle]
pub unsafe extern "C" fn oopbot_engine_free(handle: *mut OopbotHandle) {
    if !handle.is_null() {
        let _ = Box::from_raw(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_models_and_tabs_roundtrip() {
        unsafe {
            let key = CString::new("dummy_key").unwrap();
            let model = CString::new("gemini-2.5-flash").unwrap();
            let handle = oopbot_engine_new(key.as_ptr(), model.as_ptr(), false);
            assert!(!handle.is_null());

            let req = CString::new(r#"{"op":"models"}"#).unwrap();
            let res_ptr = oopbot_engine_request(handle, req.as_ptr());
            let res_str = CStr::from_ptr(res_ptr).to_str().unwrap().to_owned();
            oopbot_string_free(res_ptr);

            let v: Value = serde_json::from_str(&res_str).unwrap();
            assert_eq!(v["ok"], true);
            assert_eq!(v["active_model"], "gemini-2.5-flash");

            let req_tab = CString::new(r#"{"op":"new_tab","model":"gemini-2.5-pro"}"#).unwrap();
            let tab_ptr = oopbot_engine_request(handle, req_tab.as_ptr());
            let tab_v: Value =
                serde_json::from_str(CStr::from_ptr(tab_ptr).to_str().unwrap()).unwrap();
            oopbot_string_free(tab_ptr);
            assert_eq!(tab_v["tab"]["model"], "gemini-2.5-pro");

            oopbot_engine_free(handle);
        }
    }
}
