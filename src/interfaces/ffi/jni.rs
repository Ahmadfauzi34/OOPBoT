//! Android JNI Bridge (`io.jeph.oopbot.host.OopbotBridge`).

use super::{create_handle, dispatch_request, oopbot_engine_free, OopbotHandle};
use crate::config::builtin_catalog_json;
use serde_json::Value;
use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;

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
///
/// # Safety
/// `env` must be a valid JNI `JNIEnv*` pointer for the calling thread.
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_oopbot_host_OopbotBridge_version(
    env: JniEnv,
    _class: JObject,
) -> JString {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rust_to_jstring(env, env!("CARGO_PKG_VERSION"))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// `OopbotBridge.referenceCatalog(): String`
///
/// # Safety
/// `env` must be a valid JNI `JNIEnv*` pointer for the calling thread.
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_oopbot_host_OopbotBridge_referenceCatalog(
    env: JniEnv,
    _class: JObject,
) -> JString {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rust_to_jstring(env, &builtin_catalog_json().to_string())
    }))
    .unwrap_or_else(|_| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rust_to_jstring(env, r#"{"ok":false,"error":"internal panic"}"#)
        }))
        .unwrap_or(std::ptr::null_mut())
    })
}

/// `OopbotBridge.engineNew(apiKey: String, defaultModel: String, autoApprove: Boolean): Long`
///
/// # Safety
/// `env` must be a valid JNI `JNIEnv*` pointer, and `api_key` / `default_model` must be valid JNI string references (or null).
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_oopbot_host_OopbotBridge_engineNew(
    env: JniEnv,
    _class: JObject,
    api_key: JString,
    default_model: JString,
    auto_approve: JBoolean,
) -> JLong {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(key) = jstring_to_rust(env, api_key) else {
            return 0;
        };
        let model = jstring_to_rust(env, default_model).unwrap_or_default();
        create_handle(key, model, auto_approve != 0) as JLong
    }))
    .unwrap_or(0)
}

/// `OopbotBridge.engineRequest(handle: Long, requestJson: String): String`
///
/// # Safety
/// `env` must be a valid JNI `JNIEnv*`, `handle` must be a non-freed pointer from `engineNew` (or `0`), and `request_json` must be a valid JNI string reference (or null).
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_oopbot_host_OopbotBridge_engineRequest(
    env: JniEnv,
    _class: JObject,
    handle: JLong,
    request_json: JString,
) -> JString {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
    }))
    .unwrap_or_else(|_| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rust_to_jstring(env, r#"{"ok":false,"error":"internal panic"}"#)
        }))
        .unwrap_or(std::ptr::null_mut())
    })
}

/// `OopbotBridge.engineFree(handle: Long)`
///
/// # Safety
/// `handle` must be `0` or a live pointer previously returned by `engineNew` that has not yet been freed.
#[no_mangle]
pub unsafe extern "C" fn Java_io_jeph_oopbot_host_OopbotBridge_engineFree(
    _env: JniEnv,
    _class: JObject,
    handle: JLong,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        oopbot_engine_free(handle as *mut OopbotHandle);
    }));
}
