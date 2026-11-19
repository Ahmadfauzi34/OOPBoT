use super::*;
use crate::common::fs::read_json_file;
use serde_json::json;

#[test]
fn lookup_by_index_and_id() {
    let mut cat = ModelCatalog::builtin();
    assert_eq!(cat.find("1").unwrap().id, "gemini-3.5-flash-lite");
    assert_eq!(cat.find("gemini-2.5-pro").unwrap().label, "Gemini 2.5 Pro");
    cat.set_default_model("gemini-experimental-custom");
    assert_eq!(cat.default_model(), "gemini-experimental-custom");
    assert!(cat.find("gemini-experimental-custom").is_some());
}

#[test]
fn builtin_providers_cover_all_six_reference_providers() {
    let providers = builtin_providers();
    let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
    for expected in [
        "google",
        "openrouter",
        "zai",
        "kimi-coding",
        "anthropic",
        "openai-codex",
    ] {
        assert!(ids.contains(&expected), "missing provider {}", expected);
        let p = providers.iter().find(|p| p.id == expected).unwrap();
        assert!(!p.models.is_empty(), "provider {} has no models", expected);
    }
}

#[test]
fn model_overrides_and_thinking_levels_work() {
    let mut m = ModelCatalog::builtin()
        .find("gemini-2.5-flash")
        .unwrap()
        .clone();
    assert!(m.reasoning);
    assert!(m.available_thinking_levels().contains(&"high".to_string()));

    m.apply_override(&json!({
        "name": "Flash Kustom App",
        "contextWindow": 500_000,
        "maxTokens": 16_000,
        "thinkingLevelMap": {
            "minimal": "low",
            "low": "low",
            "medium": "medium",
            "high": null,
            "xhigh": null
        },
        "input": ["text"],
        "cost": { "input": 0.5, "output": 1.5 }
    }));
    assert_eq!(m.label, "Flash Kustom App");
    assert_eq!(m.input_token_limit, Some(500_000));
    assert_eq!(m.output_token_limit, Some(16_000));
    assert_eq!(m.input, vec!["text".to_string()]);
    assert_eq!(
        m.available_thinking_levels(),
        vec!["off", "minimal", "low", "medium"]
    );
    assert_eq!(m.clamp_thinking_level("high"), "medium");
}

#[test]
fn parses_gemini_and_openai_model_lists_with_capabilities() {
    let gemini_json = json!({
        "models": [
            {
                "name": "models/gemini-2.5-flash",
                "displayName": "Gemini 2.5 Flash",
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536,
                "supportedGenerationMethods": ["generateContent", "countTokens"]
            },
            {
                "name": "models/text-embedding-004",
                "supportedGenerationMethods": ["embedContent"]
            }
        ]
    });
    let parsed = parse_models_response(&gemini_json, "google");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].id, "gemini-2.5-flash");
    assert_eq!(parsed[0].input_token_limit, Some(1_048_576));
    assert!(parsed[0].reasoning);

    let openai_json = json!({
        "object": "list",
        "data": [
            {
                "id": "deepseek-chat",
                "name": "DeepSeek V3",
                "context_length": 64000,
                "reasoning_effort": ["low", "high"],
                "architecture": { "input_modalities": ["text"] },
                "pricing": { "prompt": "0.00000027", "completion": "0.0000011" }
            }
        ]
    });
    let parsed_oa = parse_models_response(&openai_json, "openrouter");
    assert_eq!(parsed_oa.len(), 1);
    assert_eq!(parsed_oa[0].id, "deepseek-chat");
    assert_eq!(parsed_oa[0].label, "DeepSeek V3");
    assert_eq!(parsed_oa[0].input_token_limit, Some(64_000));
    assert!(parsed_oa[0].reasoning);
    assert_eq!(parsed_oa[0].input, vec!["text".to_string()]);
    assert!(parsed_oa[0].thinking_level_map.is_some());
    assert!(parsed_oa[0].cost.is_some());
}

#[test]
fn resolves_model_scope_patterns() {
    let available = vec![
        ModelCatalog::builtin()
            .find("gemini-2.5-flash")
            .unwrap()
            .clone(),
        ModelCatalog::builtin()
            .find("gemini-2.5-pro")
            .unwrap()
            .clone(),
    ];
    let patterns = vec![
        "gemini/gemini-2.5-pro:high".to_string(),
        "*flash*".to_string(),
    ];
    let scoped = resolve_model_scope(&patterns, &available);
    assert_eq!(scoped.len(), 2);
    assert_eq!(scoped[0].0.id, "gemini-2.5-pro");
    assert_eq!(scoped[0].1.as_deref(), Some("high"));
    assert_eq!(scoped[1].0.id, "gemini-2.5-flash");
    assert_eq!(scoped[1].1, None);
}

#[test]
fn reg005_extract_auth_credentials_supports_oauth_and_api_key() {
    let doc = json!({
        "anthropic": { "type": "oauth", "access": "oa-tok-1", "refresh": "ref-1" },
        "openai-codex": { "access_token": "oa-tok-2" },
        "openrouter": { "type": "api_key", "key": "sk-or-123" },
        "google": "AIza-direct-str",
        "zai": { "apiKey": "no-api-key" }
    });
    assert_eq!(
        extract_auth_credentials(&doc, "anthropic"),
        Some(("oa-tok-1".to_string(), "oauth"))
    );
    assert_eq!(
        extract_auth_credentials(&doc, "openai-codex"),
        Some(("oa-tok-2".to_string(), "oauth"))
    );
    assert_eq!(
        extract_auth_credentials(&doc, "openrouter"),
        Some(("sk-or-123".to_string(), "api_key"))
    );
    assert_eq!(
        extract_auth_credentials(&doc, "google"),
        Some(("AIza-direct-str".to_string(), "api_key"))
    );
    assert_eq!(extract_auth_credentials(&doc, "zai"), None);
}

#[test]
fn config_store_roundtrips_auth_settings_and_models_with_secure_permissions() {
    let dir = std::env::temp_dir().join(format!("oopbot_config_store_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 1. save_auth_key dengan base_url menulis auth.json (0o600) + models.json
    store::save_auth_key(&dir, "openrouter", "sk-or-test", Some("https://openrouter.ai/api/v1"))
        .unwrap();
    let auth = read_json_file(&dir.join("auth.json"));
    assert_eq!(
        extract_auth_credentials(&auth, "openrouter"),
        Some(("sk-or-test".to_string(), "api_key"))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    let models = read_json_file(&dir.join("models.json"));
    assert_eq!(
        models["providers"]["openrouter"]["baseUrl"],
        "https://openrouter.ai/api/v1"
    );

    // 2. remove_auth_key menghapus provider dan menjaga permission 0o600
    store::remove_auth_key(&dir, "openrouter").unwrap();
    let auth_after = read_json_file(&dir.join("auth.json"));
    assert_eq!(extract_auth_credentials(&auth_after, "openrouter"), None);

    // 3. save_settings_defaults menyimpan defaultProvider, defaultModel, defaultThinkingLevel
    store::save_settings_defaults(&dir, Some("google"), Some("gemini-2.5-pro"), Some("high"))
        .unwrap();
    let settings = read_json_file(&dir.join("settings.json"));
    assert_eq!(settings["defaultProvider"], "google");
    assert_eq!(settings["defaultModel"], "gemini-2.5-pro");
    assert_eq!(settings["defaultThinkingLevel"], "high");

    // 4. save_scanned_models & save_model_override
    let flash = ModelCatalog::builtin()
        .find("gemini-2.5-flash")
        .unwrap()
        .clone();
    store::save_scanned_models(&dir, "google", &[flash], "AIza-test").unwrap();
    store::save_model_override(
        &dir,
        "google",
        "gemini-2.5-flash",
        &json!({ "contextWindow": 512_000 }),
    )
    .unwrap();
    let models_after = read_json_file(&dir.join("models.json"));
    assert_eq!(
        models_after["providers"]["google"]["models"][0]["id"],
        "gemini-2.5-flash"
    );
    assert_eq!(
        models_after["providers"]["google"]["modelOverrides"]["gemini-2.5-flash"]["contextWindow"],
        512_000
    );

    let _ = std::fs::remove_dir_all(&dir);
}
