//! Tipe error bersama (`BoxError`) dan helper ekstraksi pesan error HTTP/HTML.

use serde_json::Value;

/// Tipe error boxed thread-safe standar lintas modul OOPBoT.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Mendeteksi apakah response body HTTP berupa halaman HTML (mis. halaman error proxy/gateway).
pub(crate) fn is_html_error_body(body: &str) -> bool {
    let trimmed = body.trim_start().to_ascii_lowercase();
    trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<html")
        || trimmed.starts_with("<head")
        || trimmed.starts_with("<body")
}

/// Mengekstrak pesan error yang bermakna dari payload JSON API (OpenAI/Gemini/Anthropic/generic),
/// dibatasi hingga `max_chars` karakter.
pub(crate) fn extract_error_from_value(val: &Value, max_chars: usize) -> Option<String> {
    let candidates = [
        val["error"]["message"].as_str(),
        val["error"].as_str(),
        val.as_array()
            .and_then(|arr| arr.first())
            .and_then(|item| item["error"]["message"].as_str()),
        val["message"].as_str(),
        val["detail"].as_str(),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(|s| s.chars().take(max_chars).collect())
}

/// Mengekstrak ringkasan pesan error ramah-manusia dari body respons API HTTP.
pub(crate) fn extract_api_error_message(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty response body".to_string();
    }
    if is_html_error_body(trimmed) {
        return "HTML error page returned by server/proxy".to_string();
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
        if let Some(msg) = extract_error_from_value(&parsed, 500) {
            return msg;
        }
        return parsed.to_string().chars().take(400).collect();
    }
    trimmed.chars().take(400).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_html_error_bodies_case_insensitively() {
        assert!(is_html_error_body("  <!DOCTYPE html><html></html>"));
        assert!(is_html_error_body("<html><body>502 Bad Gateway</body></html>"));
        assert!(is_html_error_body("<HEAD><title>Err</title></HEAD>"));
        assert!(!is_html_error_body("{\"error\":\"invalid_key\"}"));
    }

    #[test]
    fn extracts_error_from_common_json_shapes() {
        assert_eq!(
            extract_error_from_value(&json!({ "error": { "message": "quota exceeded" } }), 500)
                .as_deref(),
            Some("quota exceeded")
        );
        assert_eq!(
            extract_error_from_value(&json!([{ "error": { "message": "bad req" } }]), 500)
                .as_deref(),
            Some("bad req")
        );
        assert_eq!(
            extract_error_from_value(&json!({ "detail": "not found" }), 5).as_deref(),
            Some("not f")
        );
    }

    #[test]
    fn formats_api_error_messages_cleanly() {
        assert_eq!(extract_api_error_message("   "), "empty response body");
        assert_eq!(
            extract_api_error_message("<!doctype html><h1>502</h1>"),
            "HTML error page returned by server/proxy"
        );
        assert_eq!(
            extract_api_error_message(r#"{"error":{"message":"invalid api key"}}"#),
            "invalid api key"
        );
    }
}
