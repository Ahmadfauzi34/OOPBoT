use super::*;
#[allow(unused_imports)]
use crate::agent::{AgentEvent, Approver, EventSink, StepsOutcome, ToolStatus};
#[allow(unused_imports)]
use crate::common::fs::read_json_file;
#[allow(unused_imports)]
use crate::common::time::{epoch_ms, iso_now, parse_iso_to_epoch_ms};
#[allow(unused_imports)]
use crate::config::{ModelCatalog, ModelInfo};
#[allow(unused_imports)]
use crate::tools::ToolRegistry;
#[allow(unused_imports)]
use serde_json::{json, Value};
#[allow(unused_imports)]
use std::io::{self, Cursor};
#[allow(unused_imports)]
use std::path::Path;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};

#[test]
fn load_providers_applies_model_overrides_and_local_providers() {
    let tmp = std::env::temp_dir().join(format!("oopbot_rpc_test_{}", epoch_ms()));
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(
        tmp.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                "google": {
                    "modelOverrides": {
                        "gemini-2.5-flash": {
                            "name": "Flash Override Dari Aplikasi",
                            "maxTokens": 8192,
                            "reasoning": false
                        }
                    }
                },
                "oopbot-local": {
                    "baseUrl": "http://127.0.0.1:8080/v1",
                    "api": "openai-completions",
                    "apiKey": "no-api-key",
                    "models": [
                        {
                            "id": "qwen3-4b",
                            "name": "Qwen3 4B Local",
                            "reasoning": true,
                            "contextWindow": 32768,
                            "maxTokens": 4096
                        }
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let cat = ModelCatalog::builtin();
    let providers = load_providers(&tmp, &cat);
    let google = providers.get("google").unwrap();
    let flash = google
        .models
        .iter()
        .find(|m| m.id == "gemini-2.5-flash")
        .unwrap();
    assert_eq!(flash.label, "Flash Override Dari Aplikasi");
    assert_eq!(flash.output_token_limit, Some(8192));
    assert!(!flash.reasoning);

    let local = providers.get("oopbot-local").unwrap();
    assert!(local.has_auth);
    assert_eq!(local.models.len(), 1);
    assert_eq!(local.models[0].id, "qwen3-4b");
    assert_eq!(local.models[0].input_token_limit, Some(32768));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn load_providers_uses_scanned_models_without_mixing_hardcoded_models() {
    let tmp = std::env::temp_dir().join(format!("oopbot_scan_test_{}", epoch_ms()));
    let _ = std::fs::create_dir_all(&tmp);

    // Simulasikan hasil scan live dari token pengguna yang hanya mendukung gemini-2.0-flash
    let scanned = vec![ModelInfo {
        id: "gemini-2.0-flash".to_string(),
        label: "Gemini 2.0 Flash (Scanned)".to_string(),
        provider: "google".to_string(),
        supports_tools: true,
        input_token_limit: Some(1_048_576),
        output_token_limit: Some(8192),
        description: "Scanned from token".to_string(),
        api: "google-generative-ai".to_string(),
        reasoning: true,
        thinking_level_map: None,
        input: vec!["text".to_string(), "image".to_string()],
        cost: None,
    }];

    save_scanned_models_to_config(&tmp, "google", &scanned, "test-api-key").unwrap();

    let cat = ModelCatalog::builtin();
    let providers = load_providers(&tmp, &cat);
    let google = providers.get("google").unwrap();

    // Harus HANYA berisi model hasil scan dari token, TIDAK mencampur model hardcode bawaan!
    assert_eq!(google.models.len(), 1);
    assert_eq!(google.models[0].id, "gemini-2.0-flash");
    assert!(!google
        .models
        .iter()
        .any(|m| m.id == "gemini-3.5-flash-lite"));

    // Verifikasi scannedApiKey tersimpan
    let read_back = read_json_file(&tmp.join("models.json"));
    assert_eq!(
        read_back["providers"]["google"]["scannedApiKey"].as_str(),
        Some("test-api-key")
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn reg005_available_models_empty_scope_and_no_local_duplicate() {
    let tmp = TempConfigGuard::new("reg005_models_scope");
    std::fs::write(
        tmp.path().join("settings.json"),
        json!({ "enabledModels": [] }).to_string(),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("models.json"),
        json!({
            "providers": {
                "oopbot-local": {
                    "baseUrl": "http://127.0.0.1:11434/v1",
                    "api": "openai-completions",
                    "models": [{ "id": "llama3.2", "name": "Llama 3.2" }]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let models = server.available_models_json();
    let local_matches: Vec<&Value> = models
        .iter()
        .filter(|m| m["id"] == "llama3.2" && m["provider"] == "oopbot-local")
        .collect();
    assert_eq!(
        local_matches.len(),
        1,
        "empty enabledModels must leave scope unrestricted and local model must appear once"
    );
}


#[tokio::test]
async fn reg_err_transient_llm_error_covers_network_and_529_without_matching_4xx() {
    for transient in [
        "HTTP 529 overloaded",
        "429 Too Many Requests",
        "Service Temporarily Unavailable",
        "connection reset by peer",
        "connection closed before message completed",
        "broken pipe",
        "request timeout after 30s",
        "error sending request for url",
        "empty response from provider",
    ] {
        assert!(
            is_transient_llm_error(transient),
            "expected transient match for: {}",
            transient
        );
    }

    for fatal in [
        "HTTP 400 Bad Request: invalid field",
        "HTTP 401 Unauthorized: invalid api key",
        "HTTP 403 Forbidden: billing disabled",
        "HTTP 404 Not Found: model does not exist",
    ] {
        assert!(
            !is_transient_llm_error(fatal),
            "did not expect transient match for: {}",
            fatal
        );
    }
}

#[tokio::test]
async fn reg_err_rpc_config_write_failure_is_returned_and_reported_by_slash_command() {
    let blocked = TempConfigGuard::new("rpc_write_blocked");
    std::fs::create_dir_all(blocked.path().join("settings.json")).unwrap();
    std::fs::create_dir_all(blocked.path().join("models.json")).unwrap();
    std::fs::create_dir_all(blocked.path().join("auth.json")).unwrap();

    assert!(save_settings_fields(blocked.path(), Some("google"), Some("gemini-2.5-flash"), None).is_err());
    assert!(save_model_override_to_config(
        blocked.path(),
        "google",
        "gemini-2.5-flash",
        &json!({ "maxTokens": 4096 })
    )
    .is_err());
    assert!(save_auth_key(blocked.path(), "google", "k", None).is_err());

    let tmp = TempConfigGuard::new("rpc_slash_save_err");
    let server = RpcServer::new_in_dir_with_cwd(
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
        None,
        None,
        vec![],
    )
    .unwrap();
    let models_path = tmp.path().join("models.json");
    if models_path.exists() {
        let _ = std::fs::remove_file(&models_path);
    }
    std::fs::create_dir_all(&models_path).unwrap();

    let reply = server
        .runtime
        .handle_slash_command("/max-tokens 2048")
        .await
        .unwrap();
    assert!(
        reply.contains("Gagal menyimpan"),
        "expected failure reply when models.json write fails, got: {}",
        reply
    );
    assert!(
        !reply.contains("berhasil disimpan"),
        "must not falsely claim success: {}",
        reply
    );
}

