use super::html::{
    decode_html_entities, extract_href, is_ddg_challenge, parse_ddg_results, strip_html_tags,
    urldecode, urlencoding,
};
use super::providers::{SearchEngine, WebSearchConfig, WebSearchEndpoints};
use super::{FetchPageTool, WebSearchTool};
use crate::tools::Tool;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn assert_no_additional_properties(val: &Value) {
    match val {
        Value::Object(map) => {
            assert!(
                !map.contains_key("additionalProperties"),
                "Schema must not contain additionalProperties: {}",
                val
            );
            for v in map.values() {
                assert_no_additional_properties(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                assert_no_additional_properties(v);
            }
        }
        _ => {}
    }
}

async fn spawn_mock_http<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: Fn(&str, &str, &str) -> (u16, &'static str, String) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut header_end = None;
                let mut content_len = 0usize;

                loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if header_end.is_none() {
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = Some(pos + 4);
                            let headers_str = String::from_utf8_lossy(&buf[..pos]);
                            for line in headers_str.lines() {
                                if let Some((k, v)) = line.split_once(':') {
                                    if k.trim().eq_ignore_ascii_case("content-length") {
                                        content_len = v.trim().parse().unwrap_or(0);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(hend) = header_end {
                        if buf.len() >= hend + content_len {
                            break;
                        }
                    }
                }

                let raw = String::from_utf8_lossy(&buf);
                let first_line = raw.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let target = parts.next().unwrap_or("/");
                let body = header_end
                    .map(|hend| String::from_utf8_lossy(&buf[hend..]).into_owned())
                    .unwrap_or_default();

                let (status, content_type, resp_body) = handler(method, target, &body);
                let reason = match status {
                    200 => "OK",
                    202 => "Accepted",
                    403 => "Forbidden",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    _ => "Status",
                };
                let resp = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    reason,
                    content_type,
                    resp_body.len(),
                    resp_body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    (format!("http://{}", addr), handle)
}

#[test]
fn html_strip_removes_scripts_styles_svg_comments_and_decodes_entities() {
    let html = r#"
    <html>
      <head>
        <style>.x { color: red; }</style>
        <script>console.log("secret_js");</script>
      </head>
      <body>
        <!-- hidden comment -->
        <h1>OOPBoT &amp; Termux</h1>
        <svg><path d="M0 0"/></svg>
        <noscript>Enable JS</noscript>
        <p>Harga &quot;Gratis&quot; &ndash; mantap &#39;ok&#39;!</p>
      </body>
    </html>"#;

    let clean = strip_html_tags(html);
    assert!(clean.contains("OOPBoT & Termux"));
    assert!(clean.contains("Harga \"Gratis\" – mantap 'ok'!"));
    assert!(!clean.contains("secret_js"));
    assert!(!clean.contains("color: red"));
    assert!(!clean.contains("hidden comment"));
    assert!(!clean.contains("Enable JS"));
    assert!(!clean.contains("M0 0"));
}

#[test]
fn html_strip_handles_adjacent_blocks_and_unclosed_script() {
    let adjacent = "<script>a()</script><style>b{}</style><p>Visible</p>";
    assert_eq!(strip_html_tags(adjacent), "Visible");

    let unclosed = "<p>Before</p><script>window.leak = 123;";
    assert_eq!(strip_html_tags(unclosed), "Before");
}

#[test]
fn urlencoding_urldecode_and_entities_work() {
    let encoded = urlencoding("rust tokio & android/termux");
    assert_eq!(encoded, "rust+tokio+%26+android%2Ftermux");
    assert_eq!(urldecode(&encoded), "rust tokio & android/termux");
    assert_eq!(decode_html_entities("A &lt; B &#x27;C&#x27;"), "A < B 'C'");
}

#[test]
fn is_ddg_challenge_detects_http_202_and_markers() {
    assert!(is_ddg_challenge(202, "<html>ok</html>"));
    assert!(is_ddg_challenge(403, "forbidden"));
    assert!(is_ddg_challenge(429, "rate limit"));
    assert!(is_ddg_challenge(200, "<div id=\"anomaly-modal\"></div>"));
    assert!(is_ddg_challenge(200, "<form id=\"challenge-form\"></form>"));
    assert!(!is_ddg_challenge(200, "<div class=\"result__body\"></div>"));
}

#[test]
fn parse_ddg_results_handles_multiclass_and_uddg_redirect() {
    let html = r##"
    <style>.result__body { display: block; }</style>
    <div class="result results_links">
      <div class="links_main links_deep result__body">
        <h2 class="result__title">
          <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=1">Rust <b>Lang</b></a>
        </h2>
        <a class="result__snippet" href="#">Fast &amp; reliable language</a>
        <div class="clear"></div>
      </div>
    </div>"##;

    let res = parse_ddg_results(html, 5);
    assert_eq!(res.len(), 1);
    assert_eq!(res[0]["title"], "Rust Lang");
    assert_eq!(res[0]["url"], "https://rust-lang.org/");
    assert_eq!(res[0]["snippet"], "Fast & reliable language");
    assert_eq!(res[0]["source"], "duckduckgo");

    assert_eq!(
        extract_href("href=\"//duckduckgo.com/assets/foo\"").as_deref(),
        Some("https://duckduckgo.com/assets/foo")
    );
}

#[test]
fn search_engine_parse_aliases_and_errors() {
    assert_eq!(SearchEngine::parse("").unwrap(), SearchEngine::Auto);
    assert_eq!(SearchEngine::parse("google_search").unwrap(), SearchEngine::GeminiSearch);
    assert_eq!(SearchEngine::parse("google").unwrap(), SearchEngine::GeminiSearch);
    assert_eq!(SearchEngine::parse("gemini").unwrap(), SearchEngine::GeminiSearch);
    assert_eq!(SearchEngine::parse("serper").unwrap(), SearchEngine::Serper);
    assert_eq!(SearchEngine::parse("searx").unwrap(), SearchEngine::Searxng);
    assert_eq!(SearchEngine::parse("wiki").unwrap(), SearchEngine::Wikipedia);
    assert_eq!(SearchEngine::parse("gh").unwrap(), SearchEngine::Github);
    assert_eq!(SearchEngine::parse("hackernews").unwrap(), SearchEngine::HackerNews);
    assert_eq!(SearchEngine::parse("instant").unwrap(), SearchEngine::DdgInstant);
    assert_eq!(SearchEngine::parse("ddg").unwrap(), SearchEngine::DuckDuckGo);
    assert!(SearchEngine::parse("invalid_engine").is_err());
}

#[test]
fn tool_schemas_have_no_additional_properties() {
    let client = reqwest::Client::new();
    let ws = WebSearchTool::new(client.clone());
    let fp = FetchPageTool::new(client);
    assert_no_additional_properties(&ws.parameters());
    assert_no_additional_properties(&fp.parameters());
}

#[tokio::test]
async fn auto_mode_falls_back_to_public_apis_when_ddg_blocks_with_202() {
    let (base, handle) = spawn_mock_http(|_method, target, _body| {
        if target.starts_with("/ddg-html") {
            return (
                202,
                "text/html",
                r#"<html><div class="anomaly-modal">Verify you are human</div></html>"#.to_string(),
            );
        }
        if target.starts_with("/ddg-instant") {
            return (
                200,
                "application/json",
                json!({
                    "Heading": "Tokio",
                    "AbstractText": "Asynchronous runtime for Rust.",
                    "AbstractURL": "https://tokio.rs/",
                    "RelatedTopics": []
                })
                .to_string(),
            );
        }
        if target.starts_with("/wiki-en") {
            return (
                200,
                "application/json",
                json!({
                    "query": {
                        "search": [
                            { "title": "Rust (programming language)", "snippet": "<b>Rust</b> is systems programming" }
                        ]
                    }
                })
                .to_string(),
            );
        }
        if target.starts_with("/wiki-id") {
            return (
                200,
                "application/json",
                json!({ "query": { "search": [] } }).to_string(),
            );
        }
        if target.starts_with("/github") {
            return (
                200,
                "application/json",
                json!({
                    "items": [
                        {
                            "full_name": "tokio-rs/tokio",
                            "html_url": "https://github.com/tokio-rs/tokio",
                            "stargazers_count": 25000,
                            "language": "Rust",
                            "description": "A runtime for reliable async apps"
                        }
                    ]
                })
                .to_string(),
            );
        }
        if target.starts_with("/hn") {
            return (
                200,
                "application/json",
                json!({
                    "hits": [
                        {
                            "title": "Tokio 1.0 Released",
                            "objectID": "12345",
                            "url": "https://tokio.rs/blog/2020-12-tokio-1-0",
                            "points": 800,
                            "num_comments": 120
                        }
                    ]
                })
                .to_string(),
            );
        }
        (404, "text/plain", "not found".to_string())
    })
    .await;

    let ep = WebSearchEndpoints {
        ddg_html_url: format!("{}/ddg-html", base),
        ddg_instant_url: format!("{}/ddg-instant", base),
        wikipedia_en_url: format!("{}/wiki-en", base),
        wikipedia_id_url: format!("{}/wiki-id", base),
        github_url: format!("{}/github", base),
        hn_url: format!("{}/hn", base),
        ..WebSearchEndpoints::default()
    };

    let tool = WebSearchTool::with_config_and_endpoints(
        reqwest::Client::new(),
        WebSearchConfig::default(),
        ep,
    );
    let out = tool.execute(&json!({ "query": "tokio rust" })).await.unwrap();

    assert_eq!(out["engine"], "fallback_public_apis");
    assert_eq!(out["fallback_used"], true);
    assert_eq!(out["blocked_engines"], json!(["duckduckgo"]));
    assert_eq!(out["count"], 4);

    let sources: Vec<&str> = out["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["source"].as_str())
        .collect();
    assert!(sources.contains(&"ddg_instant"));
    assert!(sources.contains(&"wikipedia"));
    assert!(sources.contains(&"github"));
    assert!(sources.contains(&"hn"));
    assert!(out["note"].as_str().unwrap().contains("duckduckgo"));

    handle.abort();
}

#[tokio::test]
async fn explicit_ddg_returns_actionable_error_on_challenge() {
    let (base, handle) = spawn_mock_http(|_method, _target, _body| {
        (
            202,
            "text/html",
            "<html><form id=\"challenge-form\"></form></html>".to_string(),
        )
    })
    .await;

    let ep = WebSearchEndpoints {
        ddg_html_url: format!("{}/ddg-html", base),
        ..WebSearchEndpoints::default()
    };
    let tool = WebSearchTool::with_config_and_endpoints(
        reqwest::Client::new(),
        WebSearchConfig::default(),
        ep,
    );

    let err = tool
        .execute(&json!({ "query": "rust", "engine": "ddg" }))
        .await
        .unwrap_err();
    assert!(err.contains("memblokir"));
    assert!(err.contains("tavily"));

    handle.abort();
}

#[tokio::test]
async fn auto_mode_prioritizes_configured_tavily_brave_serper_searxng() {
    let (base, handle) = spawn_mock_http(|method, target, body| {
        if target == "/tavily" && method == "POST" {
            let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            assert_eq!(parsed["api_key"], "tvly-secret");
            return (
                200,
                "application/json",
                json!({
                    "results": [
                        { "title": "Tavily Hit", "url": "https://tavily.com/doc", "content": "AI search" }
                    ]
                })
                .to_string(),
            );
        }
        if target.starts_with("/brave") {
            return (
                200,
                "application/json",
                json!({
                    "web": {
                        "results": [
                            { "title": "Brave Hit", "url": "https://search.brave.com", "description": "<b>Brave</b> result" }
                        ]
                    }
                })
                .to_string(),
            );
        }
        if target == "/serper" && method == "POST" {
            return (
                200,
                "application/json",
                json!({
                    "organic": [
                        { "title": "Serper Hit", "link": "https://serper.dev", "snippet": "Google SERP JSON" }
                    ]
                })
                .to_string(),
            );
        }
        if target.starts_with("/searxng/search") {
            return (
                200,
                "application/json",
                json!({
                    "results": [
                        { "title": "SearXNG Hit", "url": "https://searx.local", "content": "Meta search" }
                    ]
                })
                .to_string(),
            );
        }
        (404, "text/plain", "unexpected route".to_string())
    })
    .await;

    let ep = WebSearchEndpoints {
        tavily_url: format!("{}/tavily", base),
        brave_url: format!("{}/brave", base),
        serper_url: format!("{}/serper", base),
        ..WebSearchEndpoints::default()
    };

    // 1. Tavily configured
    let tavily_tool = WebSearchTool::with_config_and_endpoints(
        reqwest::Client::new(),
        WebSearchConfig {
            tavily_api_key: Some("tvly-secret".to_string()),
            ..Default::default()
        },
        ep.clone(),
    );
    let r = tavily_tool.execute(&json!({ "query": "oopbot" })).await.unwrap();
    assert_eq!(r["engine"], "tavily");
    assert_eq!(r["results"][0]["title"], "Tavily Hit");

    // 2. Brave configured
    let brave_tool = WebSearchTool::with_config_and_endpoints(
        reqwest::Client::new(),
        WebSearchConfig {
            brave_api_key: Some("BSA-secret".to_string()),
            ..Default::default()
        },
        ep.clone(),
    );
    let r = brave_tool.execute(&json!({ "query": "oopbot" })).await.unwrap();
    assert_eq!(r["engine"], "brave");
    assert_eq!(r["results"][0]["snippet"], "Brave result");

    // 3. Serper configured
    let serper_tool = WebSearchTool::with_config_and_endpoints(
        reqwest::Client::new(),
        WebSearchConfig {
            serper_api_key: Some("serper-secret".to_string()),
            ..Default::default()
        },
        ep.clone(),
    );
    let r = serper_tool.execute(&json!({ "query": "oopbot" })).await.unwrap();
    assert_eq!(r["engine"], "serper");
    assert_eq!(r["results"][0]["url"], "https://serper.dev");

    // 4. SearXNG configured
    let searx_tool = WebSearchTool::with_config_and_endpoints(
        reqwest::Client::new(),
        WebSearchConfig {
            searxng_url: Some(format!("{}/searxng", base)),
            ..Default::default()
        },
        ep,
    );
    let r = searx_tool.execute(&json!({ "query": "oopbot" })).await.unwrap();
    assert_eq!(r["engine"], "searxng");
    assert_eq!(r["results"][0]["title"], "SearXNG Hit");

    handle.abort();
}

#[tokio::test]
async fn fetch_page_cleans_html_preserves_json_and_handles_403() {
    let (base, handle) = spawn_mock_http(|_method, target, _body| {
        if target == "/article" {
            return (
                200,
                "text/html; charset=utf-8",
                "<html><script>bad()</script><p>Halo &amp; selamat datang</p></html>".to_string(),
            );
        }
        if target == "/data.json" {
            return (
                200,
                "application/json",
                r#"{"generic":"Vec<String>"}"#.to_string(),
            );
        }
        if target == "/blocked" {
            return (403, "text/plain", "Forbidden".to_string());
        }
        (404, "text/plain", "Not Found".to_string())
    })
    .await;

    let fp = FetchPageTool::new(reqwest::Client::new());

    let html_res = fp
        .execute(&json!({ "url": format!("{}/article", base) }))
        .await
        .unwrap();
    assert_eq!(html_res["status"], 200);
    assert_eq!(html_res["content"], "Halo & selamat datang");

    let json_res = fp
        .execute(&json!({ "url": format!("{}/data.json", base) }))
        .await
        .unwrap();
    assert_eq!(json_res["content"], r#"{"generic":"Vec<String>"}"#);

    let blocked_err = fp
        .execute(&json!({ "url": format!("{}/blocked", base) }))
        .await
        .unwrap_err();
    assert!(blocked_err.contains("403"));

    handle.abort();
}

#[test]
fn config_load_from_dir_detects_gemini_auth_and_flash_model() {
    let dir = std::env::temp_dir().join(format!("oopbot_ws_cfg_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_string_pretty(&json!({
            "google": { "type": "api_key", "key": "AIza-test-secret" }
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                "google": {
                    "baseUrl": "https://generativelanguage.googleapis.com/v1beta/",
                    "models": [
                        { "id": "gemini-3.5-pro" },
                        { "id": "gemini-2.5-flash-lite" }
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultProvider": "google",
            "defaultModel": "gemini-3.5-pro"
        }))
        .unwrap(),
    )
    .unwrap();

    let cfg = WebSearchConfig::load_from_dir(&dir);
    assert_eq!(cfg.gemini_api_key.as_deref(), Some("AIza-test-secret"));
    assert_eq!(
        cfg.gemini_base_url.as_deref(),
        Some("https://generativelanguage.googleapis.com/v1beta")
    );
    // Pro model on chat should prefer scanned flash model for fast grounding search
    assert_eq!(cfg.gemini_model.as_deref(), Some("gemini-2.5-flash-lite"));

    // Opt-out flag disables Gemini grounding cleanly
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "webSearch": { "googleSearchGrounding": false }
        }))
        .unwrap(),
    )
    .unwrap();
    let disabled_cfg = WebSearchConfig::load_from_dir(&dir);
    assert!(disabled_cfg.gemini_api_key.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn auto_mode_uses_gemini_google_search_when_connected() {
    let (base, handle) = spawn_mock_http(|method, target, body| {
        if method == "POST" && target == "/gemini/models/gemini-2.5-flash:generateContent" {
            let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            assert_eq!(parsed["tools"], json!([{ "google_search": {} }]));
            return (
                200,
                "application/json",
                json!({
                    "candidates": [{
                        "content": {
                            "parts": [{ "text": "Rust 1.85 memperkenalkan edisi 2024 secara stabil." }]
                        },
                        "groundingMetadata": {
                            "webSearchQueries": ["rust 1.85 release"],
                            "groundingChunks": [
                                {
                                    "web": {
                                        "uri": "https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html",
                                        "title": "blog.rust-lang.org"
                                    }
                                },
                                {
                                    "web": {
                                        "uri": "https://releases.rs/docs/1.85.0/",
                                        "title": "releases.rs"
                                    }
                                }
                            ],
                            "groundingSupports": [
                                {
                                    "segment": { "text": "Rust 1.85 menstabilkan Rust Edition 2024." },
                                    "groundingChunkIndices": [0]
                                }
                            ]
                        }
                    }]
                })
                .to_string(),
            );
        }
        (404, "text/plain", "unexpected route".to_string())
    })
    .await;

    let ep = WebSearchEndpoints {
        gemini_base_url: format!("{}/gemini", base),
        ..WebSearchEndpoints::default()
    };
    let cfg = WebSearchConfig {
        gemini_api_key: Some("AIza-connected".to_string()),
        ..Default::default()
    };

    let tool = WebSearchTool::with_config_and_endpoints(reqwest::Client::new(), cfg, ep);
    let res = tool.execute(&json!({ "query": "rust 1.85 edition 2024" })).await.unwrap();

    assert_eq!(res["engine"], "google_search");
    assert_eq!(res["count"], 2);
    assert_eq!(
        res["summary"],
        "Rust 1.85 memperkenalkan edisi 2024 secara stabil."
    );
    assert_eq!(
        res["results"][0]["url"],
        "https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html"
    );
    assert_eq!(
        res["results"][0]["snippet"],
        "Rust 1.85 menstabilkan Rust Edition 2024."
    );
    assert_eq!(res["results"][0]["source"], "google_search");

    handle.abort();
}

#[tokio::test]
async fn auto_mode_falls_back_when_gemini_google_search_rate_limited() {
    let (base, handle) = spawn_mock_http(|_method, target, _body| {
        if target.starts_with("/gemini/") {
            return (
                429,
                "application/json",
                json!({ "error": { "message": "Quota exceeded" } }).to_string(),
            );
        }
        if target.starts_with("/ddg-html") {
            return (
                202,
                "text/html",
                r#"<div class="anomaly-modal">Verify</div>"#.to_string(),
            );
        }
        if target.starts_with("/wiki-en") {
            return (
                200,
                "application/json",
                json!({
                    "query": {
                        "search": [{ "title": "Rust", "snippet": "Systems language" }]
                    }
                })
                .to_string(),
            );
        }
        if target.starts_with("/wiki-id") {
            return (200, "application/json", json!({ "query": { "search": [] } }).to_string());
        }
        if target.starts_with("/ddg-instant") {
            return (200, "application/json", json!({}).to_string());
        }
        if target.starts_with("/github") {
            return (200, "application/json", json!({ "items": [] }).to_string());
        }
        if target.starts_with("/hn") {
            return (200, "application/json", json!({ "hits": [] }).to_string());
        }
        (404, "text/plain", "not found".to_string())
    })
    .await;

    let ep = WebSearchEndpoints {
        gemini_base_url: format!("{}/gemini", base),
        ddg_html_url: format!("{}/ddg-html", base),
        ddg_instant_url: format!("{}/ddg-instant", base),
        wikipedia_en_url: format!("{}/wiki-en", base),
        wikipedia_id_url: format!("{}/wiki-id", base),
        github_url: format!("{}/github", base),
        hn_url: format!("{}/hn", base),
        ..WebSearchEndpoints::default()
    };
    let cfg = WebSearchConfig {
        gemini_api_key: Some("AIza-rate-limited".to_string()),
        ..Default::default()
    };

    let tool = WebSearchTool::with_config_and_endpoints(reqwest::Client::new(), cfg, ep);
    let res = tool.execute(&json!({ "query": "rust" })).await.unwrap();

    assert_eq!(res["engine"], "fallback_public_apis");
    assert_eq!(res["fallback_used"], true);
    assert_eq!(res["blocked_engines"], json!(["google_search", "duckduckgo"]));
    assert_eq!(res["count"], 1);
    assert_eq!(res["results"][0]["source"], "wikipedia");

    handle.abort();
}

