//! Ekspor C-ABI (`oopbot_*`) untuk `liboopbot.so`.

use super::{create_handle, dispatch_request, from_c_str, to_c_string, OopbotHandle};
use crate::config::builtin_catalog_json;
use serde_json::{json, Value};
use std::ffi::CString;
use std::os::raw::c_char;

/// Versi statis library `liboopbot.so`.
#[no_mangle]
pub extern "C" fn oopbot_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Katalog referensi provider & model bawaan Rust dalam JSON (`*mut c_char`).
/// Bebaskan dengan `oopbot_string_free`.
#[no_mangle]
pub extern "C" fn oopbot_reference_catalog() -> *mut c_char {
    match std::panic::catch_unwind(builtin_catalog_json) {
        Ok(val) => to_c_string(val),
        Err(_) => to_c_string(json!({ "ok": false, "error": "internal panic" })),
    }
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
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(key) = from_c_str(api_key) else {
            return std::ptr::null_mut();
        };
        let model = from_c_str(default_model).unwrap_or_default();
        create_handle(key, model, auto_approve)
    }))
    .unwrap_or(std::ptr::null_mut())
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
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let h = &*handle;
        let Some(raw) = from_c_str(request_json) else {
            return to_c_string(json!({ "ok": false, "error": "null request_json" }));
        };
        let Ok(req) = serde_json::from_str::<Value>(&raw) else {
            return to_c_string(json!({ "ok": false, "error": "invalid JSON" }));
        };

        let resp = dispatch_request(h, &req);
        to_c_string(resp)
    })) {
        Ok(ptr) => ptr,
        Err(_) => to_c_string(json!({ "ok": false, "error": "internal panic" })),
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
