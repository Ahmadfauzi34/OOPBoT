use super::*;
use serde_json::Value;
use std::ffi::{CStr, CString};

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
        let cfg_v: Value = serde_json::from_str(CStr::from_ptr(cfg_ptr).to_str().unwrap()).unwrap();
        oopbot_string_free(cfg_ptr);
        assert_eq!(cfg_v["model"]["maxTokens"], 12345);
        assert_eq!(cfg_v["model"]["name"], "Flash FFI");

        let req_lvl = CString::new(r#"{"op":"set_thinking_level","level":"high"}"#).unwrap();
        let lvl_ptr = oopbot_engine_request(handle, req_lvl.as_ptr());
        let lvl_v: Value = serde_json::from_str(CStr::from_ptr(lvl_ptr).to_str().unwrap()).unwrap();
        oopbot_string_free(lvl_ptr);
        assert_eq!(lvl_v["thinking_level"], "high");

        let req_tab = CString::new(r#"{"op":"new_tab","model":"gemini-2.5-pro"}"#).unwrap();
        let tab_ptr = oopbot_engine_request(handle, req_tab.as_ptr());
        let tab_v: Value = serde_json::from_str(CStr::from_ptr(tab_ptr).to_str().unwrap()).unwrap();
        oopbot_string_free(tab_ptr);
        assert_eq!(tab_v["tab"]["model"], "gemini-2.5-pro");

        oopbot_engine_free(handle);
    }
}

#[test]
fn reg_err_ffi_null_and_invalid_inputs_return_safe_errors() {
    unsafe {
        assert!(oopbot_engine_new(std::ptr::null(), std::ptr::null(), false).is_null());
        oopbot_engine_free(std::ptr::null_mut());
        oopbot_string_free(std::ptr::null_mut());

        let req_ok = CString::new(r#"{"op":"models"}"#).unwrap();
        let null_handle_ptr = oopbot_engine_request(std::ptr::null_mut(), req_ok.as_ptr());
        let null_handle_v: Value =
            serde_json::from_str(CStr::from_ptr(null_handle_ptr).to_str().unwrap()).unwrap();
        oopbot_string_free(null_handle_ptr);
        assert_eq!(null_handle_v["ok"], false);
        assert_eq!(null_handle_v["error"], "null handle");

        let key = CString::new("dummy_key").unwrap();
        let model = CString::new("gemini-2.5-flash").unwrap();
        let handle = oopbot_engine_new(key.as_ptr(), model.as_ptr(), false);
        assert!(!handle.is_null());

        let null_req_ptr = oopbot_engine_request(handle, std::ptr::null());
        let null_req_v: Value =
            serde_json::from_str(CStr::from_ptr(null_req_ptr).to_str().unwrap()).unwrap();
        oopbot_string_free(null_req_ptr);
        assert_eq!(null_req_v["ok"], false);
        assert_eq!(null_req_v["error"], "null request_json");

        let bad_json = CString::new("{bad").unwrap();
        let bad_ptr = oopbot_engine_request(handle, bad_json.as_ptr());
        let bad_v: Value = serde_json::from_str(CStr::from_ptr(bad_ptr).to_str().unwrap()).unwrap();
        oopbot_string_free(bad_ptr);
        assert_eq!(bad_v["ok"], false);
        assert_eq!(bad_v["error"], "invalid JSON");

        let unknown_op = CString::new(r#"{"op":"does_not_exist"}"#).unwrap();
        let unk_ptr = oopbot_engine_request(handle, unknown_op.as_ptr());
        let unk_v: Value = serde_json::from_str(CStr::from_ptr(unk_ptr).to_str().unwrap()).unwrap();
        oopbot_string_free(unk_ptr);
        assert_eq!(unk_v["ok"], false);
        assert!(unk_v["error"]
            .as_str()
            .unwrap()
            .contains("unknown op 'does_not_exist'"));

        oopbot_engine_free(handle);

        // JNI exports must safely handle null env/args without panicking or unwinding.
        assert!(Java_io_jeph_oopbot_host_OopbotBridge_version(
            std::ptr::null_mut(),
            std::ptr::null_mut()
        )
        .is_null());
        assert_eq!(
            Java_io_jeph_oopbot_host_OopbotBridge_engineNew(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0
            ),
            0
        );
        assert!(Java_io_jeph_oopbot_host_OopbotBridge_engineRequest(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut()
        )
        .is_null());
        Java_io_jeph_oopbot_host_OopbotBridge_engineFree(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        );
    }
}
