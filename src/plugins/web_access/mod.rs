pub mod html;
pub mod providers;

#[cfg(test)]
mod tests;

use self::html::strip_html_tags;
use self::providers::{execute_search, WebSearchConfig, WebSearchEndpoints};
use crate::plugins::Plugin;
use crate::tools::{truncate, Tool, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

// ===========================================================================
// WebAccessPlugin (Multi-Engine Search & HTML/Text Page Fetcher)
// ===========================================================================

pub struct WebAccessPlugin {
    client: reqwest::Client,
}

impl Default for WebAccessPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl WebAccessPlugin {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("OOPBoT-WebAccess/0.5 (Android Termux; Linux)")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

#[async_trait]
impl Plugin for WebAccessPlugin {
    fn id(&self) -> &str {
        "oopbot-web-access"
    }

    fn aliases(&self) -> Vec<&'static str> {
        vec!["web-access", "web_access", "web_search", "browser"]
    }

    fn name(&self) -> &str {
        "Web Access & Search"
    }

    fn description(&self) -> &str {
        "Kemampuan pencarian internet multi-provider (otomatis Google Search saat Gemini terhubung, fallback API publik / Tavily / Brave / Serper / SearXNG / DDG) dan pembacaan halaman web."
    }

    fn register_tools(&self, registry: &mut ToolRegistry) {
        registry.register(Box::new(WebSearchTool::new(self.client.clone())));
        registry.register(Box::new(FetchPageTool::new(self.client.clone())));
    }

    fn system_prompt_append(&self) -> Option<String> {
        Some(
            "## Web Access Guidelines\n\
            - Gunakan `web_search` untuk mencari informasi terbaru, dokumentasi library, isu teknis, atau referensi di internet.\n\
            - `web_search` otomatis memakai Google Search (grounding API Gemini) saat akun Gemini terhubung, atau provider terkonfigurasi lain (Tavily/Brave/Serper/SearXNG), dan fallback otomatis ke API publik (Wikipedia, GitHub, Hacker News, DDG Instant) bila DuckDuckGo memblokir. Parameter `engine` opsional: `auto`, `google_search`, `tavily`, `brave`, `serper`, `searxng`, `wikipedia`, `github`, `hn`, `ddg_instant`, `ddg`.\n\
            - Gunakan `fetch_page` untuk membaca isi teks lengkap dari sebuah URL hasil pencarian."
                .to_string(),
        )
    }
}

pub struct WebSearchTool {
    client: reqwest::Client,
    config_override: Option<WebSearchConfig>,
    endpoints_override: Option<WebSearchEndpoints>,
}

impl WebSearchTool {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            config_override: None,
            endpoints_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_config_and_endpoints(
        client: reqwest::Client,
        config: WebSearchConfig,
        endpoints: WebSearchEndpoints,
    ) -> Self {
        Self {
            client,
            config_override: Some(config),
            endpoints_override: Some(endpoints),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Cari informasi di internet dengan fallback otomatis multi-engine (Google Search via Gemini saat terhubung, Tavily, Brave, Serper, SearXNG, DuckDuckGo + API publik Wikipedia/GitHub/HN/DDG Instant)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Kata kunci pencarian yang ingin dicari di internet"
                },
                "engine": {
                    "type": "string",
                    "description": "Mesin pencari opsional: auto (default), google_search, tavily, brave, serper, searxng, wikipedia, github, hn, ddg_instant, ddg"
                },
                "limit": {
                    "type": "integer",
                    "description": "Jumlah maksimum hasil (1-15, default 8)"
                }
            },
            "required": ["query"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let q = args["query"].as_str().unwrap_or("");
        match args["engine"].as_str().filter(|s| !s.trim().is_empty()) {
            Some(eng) => format!("web_search [{}]: \"{}\"", eng, q),
            None => format!("web_search: \"{}\"", q),
        }
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let query = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "argumen 'query' (string) wajib diisi".to_string())?;

        let requested_engine = args["engine"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(8) as usize;

        let runtime_cfg;
        let cfg = match &self.config_override {
            Some(c) => c,
            None => {
                runtime_cfg = WebSearchConfig::load_runtime();
                &runtime_cfg
            }
        };
        let default_ep;
        let ep = match &self.endpoints_override {
            Some(e) => e,
            None => {
                default_ep = WebSearchEndpoints::default();
                &default_ep
            }
        };

        execute_search(&self.client, query, requested_engine, limit, cfg, ep).await
    }
}

pub struct FetchPageTool {
    client: reqwest::Client,
}

impl FetchPageTool {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for FetchPageTool {
    fn name(&self) -> &'static str {
        "fetch_page"
    }

    fn description(&self) -> &'static str {
        "Ambil dan baca isi teks dari sebuah halaman web (URL). Tag HTML, script, dan style otomatis dibersihkan menjadi teks polos."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL website lengkap yang ingin dibaca (misal: https://example.com)"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Batas maksimum panjang konten yang dikembalikan (default 25000, maks 100000)"
                }
            },
            "required": ["url"]
        })
    }

    fn preview(&self, args: &Value) -> String {
        let url = args["url"].as_str().unwrap_or("");
        format!("fetch_page: {}", url)
    }

    async fn execute(&self, args: &Value) -> Result<Value, String> {
        let url = args["url"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "argumen 'url' wajib diisi".to_string())?;

        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("URL harus diawali dengan http:// atau https://".to_string());
        }

        let max_bytes = args["max_chars"]
            .as_u64()
            .unwrap_or(25_000)
            .clamp(500, 100_000) as usize;

        let res = self
            .client
            .get(url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (compatible; OOPBoT/0.5; +https://github.com/Ahmadfauzi34/OOPBoT)",
            )
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/json,text/plain;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
            .map_err(|e| format!("Gagal memuat URL '{}': {}", url, e))?;

        let status = res.status();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            return Err(format!(
                "Server memblokir atau membatasi permintaan (HTTP {}). Situs ini mungkin memerlukan browser atau API resmi.",
                status
            ));
        }
        if !status.is_success() {
            return Err(format!("Server mengembalikan status HTTP {}", status));
        }

        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let raw_body = res.text().await.map_err(|e| e.to_string())?;
        let text = if content_type.is_empty()
            || content_type.contains("html")
            || content_type.contains("xml")
        {
            strip_html_tags(&raw_body)
        } else {
            raw_body
        };

        let clean = truncate(&text, max_bytes);

        Ok(json!({
            "url": url,
            "status": status.as_u16(),
            "content": clean
        }))
    }
}
