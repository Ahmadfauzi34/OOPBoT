//! FFI (C-ABI + Android JNI) untuk `liboopbot.so` (`cdylib`).
//!
//! Menyediakan:
//! 1. **C-ABI (`oopbot_*`)**: untuk pemanggilan dari C/C++, JNA, Python, atau Node.
//! 2. **Android JNI (`Java_io_jeph_pipkin_host_OopbotBridge_*`)**: untuk pemanggilan
//!    langsung dari aplikasi Android Kotlin (`OopbotBridge.kt`) lewat `System.loadLibrary("oopbot")`
//!    tanpa memerlukan dependensi crate tambahan.

use crate::agent::{Agent, Approver, GeminiAgent};
use crate::events::{AgentEvent, EventSink};
use crate::gemini::GeminiClient;
use crate::models::builtin_catalog_json;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::ffi::{c_void, CStr, CString};
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

fn create_handle(api_key: String, default_model: String, auto_approve: bool) -> *mut OopbotHandle {
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
    let Ok(client) = GeminiClient::new(api_key, model) else {
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
    agent.set_active_model(&agent_model);

    Box::into_raw(Box::new(OopbotHandle { runtime, agent }))
}

/// Versi statis library `liboopbot.so`.
#[no_mangle]
pub extern "C" fn oopbot_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Katalog referensi provider & model bawaan Rust dalam JSON (`*mut c_char`).
/// Bebaskan dengan `oopbot_string_free`.
#[no_mangle]
pub extern "C" fn oopbot_reference_catalog() -> *mut c_char {
    to_c_string(builtin_catalog_json())
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
    let Some(key) = from_c_str(api_key) else {
        return std::ptr::null_mut();
    };
    let model = from_c_str(default_model).unwrap_or_default();
    create_handle(key, model, auto_approve)
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

pub(crate) fn dispatch_request(h: &OopbotHandle, req: &Value) -> Value {
    match req["op"].as_str().unwrap_or("") {
        "catalog" => {
            let mut cat = builtin_catalog_json();
            cat["ok"] = json!(true);
            cat
        }
        "get_state" => {
            let info = h.agent.active_model_info();
            json!({
                "ok": true,
                "active_model": h.agent.model(),
                "model": info.to_pipkin_json(None),
                "thinking_level": h.agent.thinking_level(),
                "available_thinking_levels": info.available_thinking_levels(),
            })
        }
        "models" => json!({
            "ok": true,
            "active_model": h.agent.model(),
            "thinking_level": h.agent.thinking_level(),
            "models": h.agent.list_models(),
        }),
        "scan_models" => match h.runtime.block_on(h.agent.scan_models()) {
            Ok(models) => json!({
                "ok": true,
                "count": models.len(),
                "active_model": h.agent.model(),
                "models": models,
            }),
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        },
        "set_provider" => {
            let Some(api_key) = req["api_key"].as_str() else {
                return json!({ "ok": false, "error": "missing 'api_key'" });
            };
            let base_url = req["base_url"].as_str();
            match h.runtime.block_on(h.agent.update_provider_token(api_key, base_url)) {
                Ok(models) => json!({
                    "ok": true,
                    "count": models.len(),
                    "active_model": h.agent.model(),
                    "models": models,
                }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        "set_model" => {
            let Some(m) = req["model"].as_str() else {
                return json!({ "ok": false, "error": "missing 'model'" });
            };
            let active = h.agent.set_active_model(m);
            let clamped_level = h.agent.set_thinking_level(&h.agent.thinking_level());
            let info = h.agent.active_model_info();
            json!({
                "ok": true,
                "active_model": active,
                "thinking_level": clamped_level,
                "model": info.to_pipkin_json(None),
            })
        }
        "set_thinking_level" => {
            let Some(level) = req["level"].as_str() else {
                return json!({ "ok": false, "error": "missing 'level'" });
            };
            let clamped = h.agent.set_thinking_level(level);
            json!({
                "ok": true,
                "thinking_level": clamped,
                "available_thinking_levels": h.agent.active_model_info().available_thinking_levels(),
            })
        }
        "configure_model" => {
            let target_model = req["model"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| h.agent.model());
            let patch = req.get("override").unwrap_or(req);
            let updated = h.agent.configure_model(&target_model, patch);
            let clamped = h.agent.set_thinking_level(&h.agent.thinking_level());
            json!({
                "ok": true,
                "model": updated.to_pipkin_json(None),
                "thinking_level": clamped,
            })
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

/// Bebaskan string hasil `oopbot_engine_request` atau `oopbot_reference_catalog`.
///
/// # Safety
/// `ptr` harus berasal dari `oopbot_engine_request` atau `oopbot_reference_catalog`.
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

// ===========================================================================
// Android JNI Bridge (`io.jeph.pipkin.host.OopbotBridge`)
// ===========================================================================

pub type JniEnv = *mut *const JniNativeInterface;
pub type JObject = *mut c_void;
pub type JString = *mut c_void;
pub type JLong = i64;
pub type JBoolean = u8;

/// Tabel fungsi minimal `JNINativeInterface_` sesuai spesifikasi JNI C ABI:
/// - slot 167: `NewStringUTF`
/// - slot 169: `GetStringUTFChars`
/// - slot 170: `ReleaseStringUTFChars`
#[repr(C)]
pub struct JniNativeInterface {
    _reserved: [*const c_void; 167],
    new_string_utf: unsafe extern "C" fn(JniEnv, *const c_char) -> JString,
    _get_string_utf_len: *const c_void,
    get_string_utf_chars: unsafe extern "C" fn(JniEnv, JString, *mut JBoolean) -> *const c_char,
    release_string_utf_chars: unsafe extern "C" fn(JniEnv, JString, *const c_char),
}

unsafe fn jstring_to_rust(env: JniEnv, s: JString) -> Option<String> {
    if env.is_null() || s.is_null() {
        return None;
    }
    let funcs = &**env;
    let chars = (funcs.get_string_utf_chars)(env, s, std::ptr::null_mut());
    if chars.is_null() {
        return None;
    }
    let owned = CStr::from_ptr(chars).to_string_lossy().into_owned();
    (funcs.release_string_utf_chars)(env, s, chars);
    Some(owned)
}

unsafe fn rust_to_jstring(env: JniEnv, text: &str) -> JString {
    if env.is_null() {
        return std::ptr::null_mut();
    }
    let funcs = &**env;
    let cstr = CString::new(text)
        .unwrap_or_else(|_| CString::new("{\"ok\":false,\"error\":\"invalid utf8\"}").unwrap());
    (funcs.new_string_utf)(env, cstr.as_ptr())
}

/// `OopbotBridge.version(): String`
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_pipkin_host_OopbotBridge_version(
    env: JniEnv,
    _class: JObject,
) -> JString {
    rust_to_jstring(env, env!("CARGO_PKG_VERSION"))
}

/// `OopbotBridge.referenceCatalog(): String`
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_pipkin_host_OopbotBridge_referenceCatalog(
    env: JniEnv,
    _class: JObject,
) -> JString {
    rust_to_jstring(env, &builtin_catalog_json().to_string())
}

/// `OopbotBridge.engineNew(apiKey: String, defaultModel: String, autoApprove: Boolean): Long`
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_pipkin_host_OopbotBridge_engineNew(
    env: JniEnv,
    _class: JObject,
    api_key: JString,
    default_model: JString,
    auto_approve: JBoolean,
) -> JLong {
    let Some(key) = jstring_to_rust(env, api_key) else {
        return 0;
    };
    let model = jstring_to_rust(env, default_model).unwrap_or_default();
    create_handle(key, model, auto_approve != 0) as JLong
}

/// `OopbotBridge.engineRequest(handle: Long, requestJson: String): String`
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_pipkin_host_OopbotBridge_engineRequest(
    env: JniEnv,
    _class: JObject,
    handle: JLong,
    request_json: JString,
) -> JString {
    let ptr = handle as *mut OopbotHandle;
    if ptr.is_null() {
        return rust_to_jstring(env, r#"{"ok":false,"error":"null handle"}"#);
    }
    let Some(raw) = jstring_to_rust(env, request_json) else {
        return rust_to_jstring(env, r#"{"ok":false,"error":"null requestJson"}"#);
    };
    let Ok(req) = serde_json::from_str::<Value>(&raw) else {
        return rust_to_jstring(env, r#"{"ok":false,"error":"invalid JSON"}"#);
    };
    let resp = dispatch_request(&*ptr, &req);
    rust_to_jstring(env, &resp.to_string())
}

/// `OopbotBridge.engineFree(handle: Long)`
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_pipkin_host_OopbotBridge_engineFree(
    _env: JniEnv,
    _class: JObject,
    handle: JLong,
) {
    oopbot_engine_free(handle as *mut OopbotHandle);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_models_thinking_and_tabs_roundtrip() {
        unsafe {
            let cat_ptr = oopbot_reference_catalog();
            let cat_str = CStr::from_ptr(cat_ptr).to_str().unwrap().to_owned();
            oopbot_string_free(cat_ptr);
            let cat_v: Value = serde_json::from_str(&cat_str).unwrap();
            assert_eq!(cat_v["defaultProvider"], "google");
            assert!(cat_v["providers"].as_array().unwrap().len() >= 6);

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

            let req_cfg = CString::new(
                r#"{"op":"configure_model","model":"gemini-2.5-flash","override":{"maxTokens":12345,"name":"Flash FFI"}}"#,
            )
            .unwrap();
            let cfg_ptr = oopbot_engine_request(handle, req_cfg.as_ptr());
            let cfg_v: Value =
                serde_json::from_str(CStr::from_ptr(cfg_ptr).to_str().unwrap()).unwrap();
            oopbot_string_free(cfg_ptr);
            assert_eq!(cfg_v["model"]["maxTokens"], 12345);
            assert_eq!(cfg_v["model"]["name"], "Flash FFI");

            let req_lvl = CString::new(r#"{"op":"set_thinking_level","level":"high"}"#).unwrap();
            let lvl_ptr = oopbot_engine_request(handle, req_lvl.as_ptr());
            let lvl_v: Value =
                serde_json::from_str(CStr::from_ptr(lvl_ptr).to_str().unwrap()).unwrap();
            oopbot_string_free(lvl_ptr);
            assert_eq!(lvl_v["thinking_level"], "high");

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

