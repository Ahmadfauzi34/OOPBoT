use super::*;
use serde_json::json;

#[test]
fn parses_text_thoughts_usage_and_function_calls() {
    let res = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    { "thought": true, "text": "Menganalisis direktori kerja..." },
                    { "text": "Saya cek dulu." },
                    { "functionCall": { "name": "list_dir", "args": { "path": "." } },
                      "thoughtSignature": "abc" }
                ]
            }
        }],
        "usageMetadata": {
            "promptTokenCount": 120,
            "candidatesTokenCount": 45,
            "thoughtsTokenCount": 15
        }
    });
    let turn = parse_model_turn(&res).unwrap();
    assert_eq!(turn.text, "Saya cek dulu.");
    assert_eq!(turn.thoughts, vec!["Menganalisis direktori kerja...".to_string()]);
    assert_eq!(turn.function_calls.len(), 1);
    assert_eq!(turn.function_calls[0].name, "list_dir");
    // thoughtSignature harus tetap ada di raw content
    assert_eq!(turn.raw_content["parts"][2]["thoughtSignature"], "abc");
    let usage = turn.usage.unwrap();
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.output_tokens, 60);
}

#[test]
fn reports_blocked_response() {
    let res = json!({ "promptFeedback": { "blockReason": "SAFETY" } });
    let err = parse_model_turn(&res).unwrap_err().to_string();
    assert!(err.contains("SAFETY"));
}

#[test]
fn function_response_keeps_id() {
    let call = FunctionCall {
        id: Some("x1".into()),
        name: "read_file".into(),
        args: json!({}),
        thought_signature: None,
    };
    let part = function_response_part(&call, json!({ "ok": true }));
    assert_eq!(part["functionResponse"]["id"], "x1");
    assert_eq!(part["functionResponse"]["name"], "read_file");
}

#[tokio::test]
async fn reg006_openai_responses_routes_to_responses_endpoint_and_parses_output() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]);
                let content_len = head
                    .lines()
                    .find_map(|l| {
                        let lower = l.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + content_len {
                    let req_body = &buf[pos + 4..pos + 4 + content_len];
                    let captured = (
                        head.to_string(),
                        serde_json::from_slice::<Value>(req_body).unwrap(),
                    );
                    let resp_body = json!({
                        "id": "resp_1",
                        "output": [
                            {
                                "type": "reasoning",
                                "summary": [{ "type": "summary_text", "text": "Checking files first" }]
                            },
                            {
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": "Running tool now." }]
                            },
                            {
                                "type": "function_call",
                                "call_id": "fc_99",
                                "name": "read_file",
                                "arguments": "{\"path\":\"Cargo.toml\"}"
                            }
                        ],
                        "usage": { "input_tokens": 88, "output_tokens": 24 }
                    })
                    .to_string();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    );
                    stream.write_all(http_resp.as_bytes()).await.unwrap();
                    return captured;
                }
            }
        }
        panic!("Incomplete HTTP request received");
    });

    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-5.3-codex".into(),
        Some(format!("http://{}/v1", addr)),
    )
    .unwrap();

    let contents = vec![
        user_message_with_images("Check image", &[("image/png".into(), "Zm9v".into())]),
        json!({
            "role": "model",
            "parts": [
                { "thought": true, "text": "old thought" },
                { "text": "Calling list" },
                { "functionCall": { "id": "call_prev", "name": "list_dir", "args": { "path": "." } } }
            ]
        }),
        json!({
            "role": "user",
            "parts": [
                function_response_part(
                    &FunctionCall {
                        id: Some("call_prev".into()),
                        name: "list_dir".into(),
                        args: json!({}),
                        thought_signature: None,
                    },
                    json!({ "files": ["Cargo.toml"] })
                )
            ]
        }),
    ];
    let decls = vec![json!({
        "name": "read_file",
        "description": "Read a file",
        "parameters": { "type": "object" }
    })];
    let opts = GenerationOptions {
        api: Some("openai-responses".into()),
        max_tokens: Some(2048),
        thinking_level: Some("high".into()),
        reasoning: true,
        thinking_level_map: None,
        session_id: Some("sess-resp-1".into()),
    };

    let turn = client
        .generate_with_options("gpt-5.3-codex", "System guide", &contents, &decls, &opts)
        .await
        .unwrap();

    let (head, req_json) = server.await.unwrap();
    assert!(
        head.starts_with("POST /v1/responses HTTP/1.1"),
        "unexpected request head: {}",
        head
    );
    assert!(head
        .to_ascii_lowercase()
        .contains("authorization: bearer sk-test"));
    assert!(head
        .to_ascii_lowercase()
        .contains("x-session-affinity: sess-resp-1"));
    assert_eq!(req_json["model"], "gpt-5.3-codex");
    assert_eq!(req_json["instructions"], "System guide");
    assert_eq!(req_json["max_output_tokens"], 2048);
    assert_eq!(req_json["tools"][0]["type"], "function");
    assert_eq!(req_json["tools"][0]["name"], "read_file");
    assert_eq!(req_json["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(req_json["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(req_json["input"][1]["type"], "message");
    assert_eq!(req_json["input"][1]["content"][0]["type"], "output_text");
    assert_eq!(req_json["input"][2]["type"], "function_call");
    assert_eq!(req_json["input"][2]["call_id"], "call_prev");
    assert_eq!(req_json["input"][3]["type"], "function_call_output");
    assert_eq!(req_json["input"][3]["call_id"], "call_prev");

    assert_eq!(turn.thoughts, vec!["Checking files first"]);
    assert_eq!(turn.text, "Running tool now.");
    assert_eq!(turn.function_calls.len(), 1);
    assert_eq!(turn.function_calls[0].id.as_deref(), Some("fc_99"));
    assert_eq!(turn.function_calls[0].name, "read_file");
    assert_eq!(turn.function_calls[0].args["path"], "Cargo.toml");
    assert_eq!(turn.usage.unwrap().input_tokens, 88);
    assert_eq!(turn.usage.unwrap().output_tokens, 24);
}

async fn spawn_single_http_response(status_line: &str, content_type: &str, body: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_line,
        content_type,
        body.len(),
        body
    );
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let _ = stream.write_all(resp.as_bytes()).await;
    });
    format!("http://{}/v1", addr)
}

#[tokio::test]
async fn reg_err_gemini_http_non_json_and_embedded_errors_preserve_status_and_reason() {
    // 502 Bad Gateway HTML: preserves HTTP 502 instead of serde parse failure masking it
    let base_502 = spawn_single_http_response(
        "502 Bad Gateway",
        "text/html",
        "<html><body><h1>502 Bad Gateway</h1></body></html>",
    )
    .await;
    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-4o-mini".into(),
        Some(base_502),
    )
    .unwrap();
    let err = client
        .generate_with_options(
            "gpt-4o-mini",
            "sys",
            &[user_message_with_images("hi", &[])],
            &[],
            &GenerationOptions {
                api: Some("openai-completions".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("502"), "expected 502 in error, got: {}", err);
    assert!(err.contains("HTML"), "expected HTML summary, got: {}", err);

    // 429 plain text rate limit: preserves HTTP 429 and text message
    let base_429 = spawn_single_http_response(
        "429 Too Many Requests",
        "text/plain",
        "Too many requests, slow down",
    )
    .await;
    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-4o-mini".into(),
        Some(base_429),
    )
    .unwrap();
    let err = client
        .generate_with_options(
            "gpt-4o-mini",
            "sys",
            &[user_message_with_images("hi", &[])],
            &[],
            &GenerationOptions {
                api: Some("openai-completions".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("429"), "expected 429 in error, got: {}", err);
    assert!(
        err.contains("Too many requests"),
        "expected plain-text body, got: {}",
        err
    );

    // 200 OK with HTML proxy page returns explicit non-JSON error
    let base_200_html = spawn_single_http_response(
        "200 OK",
        "text/html",
        "<!DOCTYPE html><html><body>Captive Portal</body></html>",
    )
    .await;
    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-4o-mini".into(),
        Some(base_200_html),
    )
    .unwrap();
    let err = client.scan_models().await.unwrap_err().to_string();
    assert!(
        err.contains("bukan JSON") && err.contains("HTML"),
        "unexpected 200 HTML error: {}",
        err
    );

    // 200 OK with embedded JSON error object surfaces the error instead of succeeding empty
    let base_200_err = spawn_single_http_response(
        "200 OK",
        "application/json",
        r#"{"error":{"message":"Quota exceeded on upstream proxy"}}"#,
    )
    .await;
    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-4o-mini".into(),
        Some(base_200_err),
    )
    .unwrap();
    let err = client
        .generate_with_options(
            "gpt-4o-mini",
            "sys",
            &[user_message_with_images("hi", &[])],
            &[],
            &GenerationOptions {
                api: Some("openai-completions".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("Quota exceeded on upstream proxy"),
        "expected embedded error message, got: {}",
        err
    );

    // 200 OK with empty choices fails loudly instead of silently returning empty text
    let base_empty_choices = spawn_single_http_response(
        "200 OK",
        "application/json",
        r#"{"id":"cmpl_1","choices":[]}"#,
    )
    .await;
    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-4o-mini".into(),
        Some(base_empty_choices),
    )
    .unwrap();
    let err = client
        .generate_with_options(
            "gpt-4o-mini",
            "sys",
            &[user_message_with_images("hi", &[])],
            &[],
            &GenerationOptions {
                api: Some("openai-completions".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("choices"),
        "expected empty choices error, got: {}",
        err
    );

    // OpenAI Responses 200 OK with "error": null and "status": "failed" returns failure;
    // whereas "error": null with valid output succeeds.
    let base_resp_failed = spawn_single_http_response(
        "200 OK",
        "application/json",
        r#"{"id":"r1","status":"failed","error":null,"incomplete_details":{"reason":"content_filter"},"output":[]}"#,
    )
    .await;
    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-5.3-codex".into(),
        Some(base_resp_failed),
    )
    .unwrap();
    let err = client
        .generate_with_options(
            "gpt-5.3-codex",
            "sys",
            &[user_message_with_images("hi", &[])],
            &[],
            &GenerationOptions {
                api: Some("openai-responses".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("content_filter"),
        "expected failed reason, got: {}",
        err
    );

    let base_resp_ok_null_error = spawn_single_http_response(
        "200 OK",
        "application/json",
        r#"{"id":"r2","status":"completed","error":null,"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"All good"}]}]}"#,
    )
    .await;
    let client = LlmClient::new_with_base_url(
        "sk-test".into(),
        "gpt-5.3-codex".into(),
        Some(base_resp_ok_null_error),
    )
    .unwrap();
    let turn = client
        .generate_with_options(
            "gpt-5.3-codex",
            "sys",
            &[user_message_with_images("hi", &[])],
            &[],
            &GenerationOptions {
                api: Some("openai-responses".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(turn.text, "All good");
}

#[test]
fn sanitize_gemini_schema_strips_unsupported_keywords_and_preserves_param_names() {
    let raw_decls = vec![json!({
        "name": "complex_tool",
        "description": "Tool dengan schema JSON lengkap",
        "parameters": {
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "env": {
                    "type": "object",
                    "description": "Env vars",
                    "additionalProperties": { "type": "string" }
                },
                "default": {
                    "type": ["string", "null"],
                    "default": "fallback",
                    "minLength": 1,
                    "description": "Parameter bernama default harus tetap dipertahankan"
                },
                "mode": {
                    "anyOf": [
                        { "type": "string", "enum": ["fast", "deep"], "const": "fast" },
                        { "type": "null" }
                    ],
                    "description": "Nullable enum via anyOf"
                },
                "items_list": {
                    "type": "array",
                    "uniqueItems": true,
                    "items": [
                        { "type": "integer", "minimum": 0, "maximum": 100 }
                    ]
                }
            },
            "required": ["env"]
        }
    })];

    let sanitized = sanitize_gemini_function_declarations(&raw_decls);
    assert_eq!(sanitized.len(), 1);
    let params = &sanitized[0]["parameters"];
    assert!(params.get("$schema").is_none());
    assert!(params.get("additionalProperties").is_none());
    assert_eq!(params["type"], "object");
    assert_eq!(params["required"], json!(["env"]));

    // `env` bebas dari `additionalProperties`
    let env = &params["properties"]["env"];
    assert_eq!(env["type"], "object");
    assert!(env.get("additionalProperties").is_none());

    // Parameter yang kebetulan bernama `default` tetap ada, namun keyword `default` & `minLength` di dalamnya dihapus
    let default_param = &params["properties"]["default"];
    assert_eq!(default_param["type"], "string");
    assert_eq!(default_param["nullable"], true);
    assert!(default_param.get("default").is_none());
    assert!(default_param.get("minLength").is_none());

    // `anyOf` nullable diubah ke schema datar
    let mode = &params["properties"]["mode"];
    assert_eq!(mode["type"], "string");
    assert_eq!(mode["enum"], json!(["fast", "deep"]));
    assert_eq!(mode["nullable"], true);
    assert!(mode.get("anyOf").is_none());
    assert!(mode.get("const").is_none());

    // Tuple `items` menjadi schema tunggal dan `minimum`/`maximum`/`uniqueItems` dibuang
    let list = &params["properties"]["items_list"];
    assert_eq!(list["type"], "array");
    assert!(list.get("uniqueItems").is_none());
    assert_eq!(list["items"]["type"], "integer");
    assert!(list["items"].get("minimum").is_none());
    assert!(list["items"].get("maximum").is_none());
}

#[tokio::test]
async fn gemini_generate_sanitizes_tool_schemas_in_request_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]);
                let content_len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + content_len {
                    let req_body = &buf[pos + 4..pos + 4 + content_len];
                    let captured = serde_json::from_slice::<Value>(req_body).unwrap();
                    let resp_body = json!({
                        "candidates": [{
                            "content": {
                                "role": "model",
                                "parts": [{ "text": "OK" }]
                            }
                        }]
                    })
                    .to_string();
                    let http_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    );
                    stream.write_all(http_resp.as_bytes()).await.unwrap();
                    return captured;
                }
            }
        }
        panic!("Incomplete HTTP request received");
    });

    let client = LlmClient::new_with_base_url(
        "AIza-test".into(),
        "gemini-2.5-flash".into(),
        Some(format!("http://{}/v1beta", addr)),
    )
    .unwrap();

    let decls = vec![json!({
        "name": "background",
        "description": "Manage background jobs",
        "parameters": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Env vars"
                }
            }
        }
    })];

    let turn = client
        .generate_with_options(
            "gemini-2.5-flash",
            "sys",
            &[user_text("hi")],
            &decls,
            &GenerationOptions {
                api: Some("google-generative-ai".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(turn.text, "OK");

    let req_json = server.await.unwrap();
    let sent_params = &req_json["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(sent_params.get("$schema").is_none());
    assert!(sent_params.get("additionalProperties").is_none());
    assert!(sent_params["properties"]["env"]
        .get("additionalProperties")
        .is_none());
    assert_eq!(sent_params["properties"]["env"]["type"], "object");
}
