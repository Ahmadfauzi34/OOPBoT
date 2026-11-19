use super::html::{is_ddg_challenge, parse_ddg_results, strip_html_tags, urlencoding};
use crate::config::extract_auth_credentials;
use crate::common::fs::read_json_file;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_GEMINI_SEARCH_MODEL: &str = "gemini-2.5-flash";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngine {
    Auto,
    GeminiSearch,
    Tavily,
    Brave,
    Serper,
    Searxng,
    Wikipedia,
    Github,
    HackerNews,
    DdgInstant,
    DuckDuckGo,
}

impl SearchEngine {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "google_search" | "google-search" | "gemini_search" | "gemini-search" | "gemini"
            | "google" => Ok(Self::GeminiSearch),
            "tavily" => Ok(Self::Tavily),
            "brave" => Ok(Self::Brave),
            "serper" => Ok(Self::Serper),
            "searxng" | "searx" => Ok(Self::Searxng),
            "wikipedia" | "wiki" => Ok(Self::Wikipedia),
            "github" | "gh" => Ok(Self::Github),
            "hn" | "hackernews" | "hacker_news" | "hacker-news" => Ok(Self::HackerNews),
            "ddg_instant" | "ddg-instant" | "instant" => Ok(Self::DdgInstant),
            "ddg" | "duckduckgo" => Ok(Self::DuckDuckGo),
            other => Err(format!(
                "Engine '{}' tidak dikenal. Pilihan valid: auto, google_search, tavily, brave, serper, searxng, wikipedia, github, hn, ddg_instant, ddg",
                other
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::GeminiSearch => "google_search",
            Self::Tavily => "tavily",
            Self::Brave => "brave",
            Self::Serper => "serper",
            Self::Searxng => "searxng",
            Self::Wikipedia => "wikipedia",
            Self::Github => "github",
            Self::HackerNews => "hn",
            Self::DdgInstant => "ddg_instant",
            Self::DuckDuckGo => "duckduckgo",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WebSearchConfig {
    pub gemini_api_key: Option<String>,
    pub gemini_base_url: Option<String>,
    pub gemini_model: Option<String>,
    pub tavily_api_key: Option<String>,
    pub brave_api_key: Option<String>,
    pub serper_api_key: Option<String>,
    pub searxng_url: Option<String>,
    pub default_engine: Option<SearchEngine>,
}

impl WebSearchConfig {
    pub fn load_runtime() -> Self {
        Self::load_from_dir(&resolve_config_dir())
    }

    pub fn load_from_dir(config_dir: &Path) -> Self {
        let settings = read_json_file(&config_dir.join("settings.json"));
        let ws = settings.get("webSearch").unwrap_or(&Value::Null);

        let env_or_setting = |env_names: &[&str], json_key: &str| -> Option<String> {
            for name in env_names {
                if let Ok(v) = std::env::var(name) {
                    let trimmed = v.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            ws.get(json_key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        let default_engine = env_or_setting(&["OOPBOT_WEB_SEARCH_ENGINE"], "defaultEngine")
            .and_then(|raw| SearchEngine::parse(&raw).ok());

        let gemini_grounding_enabled = std::env::var("OOPBOT_GEMINI_SEARCH")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
            .unwrap_or_else(|| {
                ws.get("googleSearchGrounding")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true)
            });

        let auth_json = read_json_file(&config_dir.join("auth.json"));
        let models_json = read_json_file(&config_dir.join("models.json"));

        let (gemini_api_key, gemini_base_url, gemini_model) = if gemini_grounding_enabled {
            resolve_gemini_grounding_config(&auth_json, &models_json, &settings, ws)
        } else {
            (None, None, None)
        };

        Self {
            gemini_api_key,
            gemini_base_url,
            gemini_model,
            tavily_api_key: env_or_setting(&["TAVILY_API_KEY"], "tavilyApiKey"),
            brave_api_key: env_or_setting(&["BRAVE_API_KEY"], "braveApiKey"),
            serper_api_key: env_or_setting(&["SERPER_API_KEY"], "serperApiKey"),
            searxng_url: env_or_setting(&["OOPBOT_SEARXNG_URL", "SEARXNG_URL"], "searxngUrl")
                .map(|u| u.trim_end_matches('/').to_string()),
            default_engine,
        }
    }
}

fn resolve_gemini_grounding_config(
    auth_json: &Value,
    models_json: &Value,
    settings: &Value,
    ws: &Value,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut api_key = ["google", "gemini"]
        .into_iter()
        .find_map(|id| extract_auth_credentials(auth_json, id).map(|(k, _)| k));

    if api_key.is_none() {
        api_key = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
            .into_iter()
            .find_map(|k| {
                std::env::var(k)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|s| !s.is_empty())
            });
    }

    let providers_obj = models_json.get("providers").and_then(|v| v.as_object());
    let google_provider = providers_obj.and_then(|p| {
        p.get("google").or_else(|| p.get("gemini")).or_else(|| {
            p.values().find(|pv| {
                pv.get("api").and_then(|a| a.as_str()) == Some("google-generative-ai")
            })
        })
    });

    if api_key.is_none() {
        api_key = google_provider
            .and_then(|pv| pv.get("apiKey").and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "no-api-key")
            .map(str::to_string);
    }

    let base_url = google_provider
        .and_then(|pv| pv.get("baseUrl").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .map(|s| s.trim_end_matches('/').to_string());

    let explicit_model = std::env::var("OOPBOT_GEMINI_SEARCH_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ws.get("geminiModel")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });

    let default_gemini_flash = settings
        .get("defaultProvider")
        .and_then(|v| v.as_str())
        .filter(|p| *p == "google" || *p == "gemini")
        .and_then(|_| settings.get("defaultModel").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|m| m.starts_with("gemini-") && m.contains("flash"))
        .map(str::to_string);

    let scanned_flash = google_provider
        .and_then(|pv| pv.get("models").and_then(|v| v.as_array()))
        .and_then(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                .find(|id| id.starts_with("gemini-") && id.contains("flash"))
                .map(str::to_string)
        });

    let model = explicit_model
        .or(default_gemini_flash)
        .or(scanned_flash)
        .unwrap_or_else(|| DEFAULT_GEMINI_SEARCH_MODEL.to_string());

    (api_key, base_url, Some(model))
}

fn resolve_config_dir() -> PathBuf {
    std::env::var("OOPBOT_CONFIG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            Path::new(&home).join(".oopbot")
        })
}

#[derive(Debug, Clone)]
pub struct WebSearchEndpoints {
    pub gemini_base_url: String,
    pub tavily_url: String,
    pub brave_url: String,
    pub serper_url: String,
    pub wikipedia_en_url: String,
    pub wikipedia_id_url: String,
    pub github_url: String,
    pub hn_url: String,
    pub ddg_instant_url: String,
    pub ddg_html_url: String,
}

impl Default for WebSearchEndpoints {
    fn default() -> Self {
        Self {
            gemini_base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
            tavily_url: "https://api.tavily.com/search".to_string(),
            brave_url: "https://api.search.brave.com/res/v1/web/search".to_string(),
            serper_url: "https://google.serper.dev/search".to_string(),
            wikipedia_en_url: "https://en.wikipedia.org/w/api.php".to_string(),
            wikipedia_id_url: "https://id.wikipedia.org/w/api.php".to_string(),
            github_url: "https://api.github.com/search/repositories".to_string(),
            hn_url: "https://hn.algolia.com/api/v1/search".to_string(),
            ddg_instant_url: "https://api.duckduckgo.com/".to_string(),
            ddg_html_url: "https://html.duckduckgo.com/html/".to_string(),
        }
    }
}

struct SearchOutcome {
    results: Vec<Value>,
    summary: Option<String>,
}

impl SearchOutcome {
    fn plain(results: Vec<Value>) -> Self {
        Self {
            results,
            summary: None,
        }
    }
}

enum EngineAttempt {
    Results(SearchOutcome),
    Blocked(String),
    Error(String),
}

pub(crate) async fn execute_search(
    client: &reqwest::Client,
    query: &str,
    requested_engine: Option<&str>,
    limit: usize,
    cfg: &WebSearchConfig,
    ep: &WebSearchEndpoints,
) -> Result<Value, String> {
    let engine = match requested_engine {
        Some(raw) if !raw.trim().is_empty() => SearchEngine::parse(raw)?,
        _ => cfg.default_engine.unwrap_or(SearchEngine::Auto),
    };
    let limit = limit.clamp(1, 15);

    if engine != SearchEngine::Auto {
        return run_explicit_engine(client, query, engine, limit, cfg, ep).await;
    }

    let mut warnings: Vec<String> = Vec::new();
    let mut blocked: Vec<String> = Vec::new();

    // 1. Prioritaskan API pencarian web terkonfigurasi:
    //    - Provider dedicated (Tavily/Brave/Serper/SearXNG) jika diatur eksplisit
    //    - Google Search Grounding (via API Gemini) otomatis bila akun Gemini terhubung
    let configured = [
        cfg.tavily_api_key.as_ref().map(|_| SearchEngine::Tavily),
        cfg.brave_api_key.as_ref().map(|_| SearchEngine::Brave),
        cfg.serper_api_key.as_ref().map(|_| SearchEngine::Serper),
        cfg.searxng_url.as_ref().map(|_| SearchEngine::Searxng),
        cfg.gemini_api_key.as_ref().map(|_| SearchEngine::GeminiSearch),
    ];
    for candidate in configured.into_iter().flatten() {
        match run_engine_attempt(client, query, candidate, limit, cfg, ep).await {
            EngineAttempt::Results(outcome) if !outcome.results.is_empty() => {
                let mut resp = json!({
                    "query": query,
                    "engine": candidate.as_str(),
                    "count": outcome.results.len(),
                    "results": outcome.results,
                });
                if let Some(summary) = outcome.summary.filter(|s| !s.is_empty()) {
                    resp["summary"] = json!(summary);
                }
                return Ok(resp);
            }
            EngineAttempt::Results(_) => {
                warnings.push(format!("{}: 0 hasil", candidate.as_str()));
            }
            EngineAttempt::Blocked(msg) => {
                blocked.push(candidate.as_str().to_string());
                warnings.push(format!("{}: diblokir ({})", candidate.as_str(), msg));
            }
            EngineAttempt::Error(msg) => {
                warnings.push(format!("{}: {}", candidate.as_str(), msg));
            }
        }
    }

    // 2. Zero-config mode: coba DuckDuckGo HTML sekali; jika diblokir/challenge/kosong,
    // langsung fallback otomatis ke API JSON publik gratis (DDG Instant, Wikipedia, GitHub, HN).
    match search_ddg_html(client, query, limit, ep).await {
        EngineAttempt::Results(outcome) if !outcome.results.is_empty() => {
            return Ok(json!({
                "query": query,
                "engine": SearchEngine::DuckDuckGo.as_str(),
                "count": outcome.results.len(),
                "results": outcome.results,
            }));
        }
        EngineAttempt::Results(_) => {
            warnings.push("duckduckgo: respons HTML tidak memuat hasil".to_string());
        }
        EngineAttempt::Blocked(msg) => {
            blocked.push("duckduckgo".to_string());
            warnings.push(format!("duckduckgo: diblokir ({})", msg));
        }
        EngineAttempt::Error(msg) => {
            warnings.push(format!("duckduckgo: {}", msg));
        }
    }

    // 3. Fallback paralel ke API JSON publik tanpa API key yang tidak memblokir bot/client.
    let (ddg_inst, wiki, gh, hn) = tokio::join!(
        search_ddg_instant(client, query, limit, ep),
        search_wikipedia_multi(client, query, limit, ep),
        search_github(client, query, limit, ep),
        search_hackernews(client, query, limit, ep),
    );

    let mut per_source: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for (name, attempt) in [
        ("ddg_instant", ddg_inst),
        ("wikipedia", wiki),
        ("github", gh),
        ("hn", hn),
    ] {
        match attempt {
            EngineAttempt::Results(outcome) if !outcome.results.is_empty() => {
                per_source.push((name, outcome.results));
            }
            EngineAttempt::Blocked(msg) => {
                blocked.push(name.to_string());
                warnings.push(format!("{}: diblokir ({})", name, msg));
            }
            EngineAttempt::Error(msg) => warnings.push(format!("{}: {}", name, msg)),
            EngineAttempt::Results(_) => {}
        }
    }

    let sources: Vec<&'static str> = per_source.iter().map(|(s, _)| *s).collect();
    let merged = merge_round_robin(per_source, limit);

    let note = if merged.is_empty() {
        format!(
            "Tidak ada hasil ditemukan ({}). Hubungkan provider Google Gemini (untuk Google Search otomatis) atau atur TAVILY_API_KEY, BRAVE_API_KEY, SERPER_API_KEY, maupun OOPBOT_SEARXNG_URL.",
            warnings.join("; ")
        )
    } else if !blocked.is_empty() {
        format!(
            "Mesin [{}] memblokir/membatasi akses; hasil diambil lewat fallback API publik [{}]. Hubungkan Google Gemini atau atur TAVILY_API_KEY / BRAVE_API_KEY / SERPER_API_KEY / OOPBOT_SEARXNG_URL untuk pencarian web penuh.",
            blocked.join(", "),
            sources.join(", ")
        )
    } else {
        format!("Hasil diambil dari API publik [{}].", sources.join(", "))
    };

    Ok(json!({
        "query": query,
        "engine": "fallback_public_apis",
        "fallback_used": true,
        "sources": sources,
        "blocked_engines": blocked,
        "warnings": warnings,
        "count": merged.len(),
        "results": merged,
        "note": note,
    }))
}

async fn run_explicit_engine(
    client: &reqwest::Client,
    query: &str,
    engine: SearchEngine,
    limit: usize,
    cfg: &WebSearchConfig,
    ep: &WebSearchEndpoints,
) -> Result<Value, String> {
    match run_engine_attempt(client, query, engine, limit, cfg, ep).await {
        EngineAttempt::Results(outcome) => {
            let mut resp = json!({
                "query": query,
                "engine": engine.as_str(),
                "count": outcome.results.len(),
                "results": outcome.results,
            });
            if let Some(summary) = outcome.summary.filter(|s| !s.is_empty()) {
                resp["summary"] = json!(summary);
            }
            if resp["count"].as_u64() == Some(0) {
                resp["note"] = json!(format!("Tidak ada hasil ditemukan dari {}.", engine.as_str()));
            }
            Ok(resp)
        }
        EngineAttempt::Blocked(msg) => Err(format!(
            "Engine '{}' memblokir permintaan ({}). Coba engine lain (auto, google_search, wikipedia, github, hn, tavily, brave, serper, searxng).",
            engine.as_str(),
            msg
        )),
        EngineAttempt::Error(msg) => Err(format!("Pencarian '{}' gagal: {}", engine.as_str(), msg)),
    }
}

async fn run_engine_attempt(
    client: &reqwest::Client,
    query: &str,
    engine: SearchEngine,
    limit: usize,
    cfg: &WebSearchConfig,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    match engine {
        SearchEngine::Auto => EngineAttempt::Error("internal: auto".to_string()),
        SearchEngine::GeminiSearch => match cfg.gemini_api_key.as_deref() {
            Some(key) => {
                let base = cfg
                    .gemini_base_url
                    .as_deref()
                    .unwrap_or(&ep.gemini_base_url);
                let model = cfg
                    .gemini_model
                    .as_deref()
                    .unwrap_or(DEFAULT_GEMINI_SEARCH_MODEL);
                search_gemini_grounding(client, query, limit, key, base, model).await
            }
            None => EngineAttempt::Error(
                "API key Google Gemini belum terhubung (hubungkan Gemini di Settings -> Providers atau atur GEMINI_API_KEY)"
                    .to_string(),
            ),
        },
        SearchEngine::Tavily => match cfg.tavily_api_key.as_deref() {
            Some(key) => search_tavily(client, query, limit, key, ep).await,
            None => EngineAttempt::Error("TAVILY_API_KEY / webSearch.tavilyApiKey belum diatur".to_string()),
        },
        SearchEngine::Brave => match cfg.brave_api_key.as_deref() {
            Some(key) => search_brave(client, query, limit, key, ep).await,
            None => EngineAttempt::Error("BRAVE_API_KEY / webSearch.braveApiKey belum diatur".to_string()),
        },
        SearchEngine::Serper => match cfg.serper_api_key.as_deref() {
            Some(key) => search_serper(client, query, limit, key, ep).await,
            None => EngineAttempt::Error("SERPER_API_KEY / webSearch.serperApiKey belum diatur".to_string()),
        },
        SearchEngine::Searxng => match cfg.searxng_url.as_deref() {
            Some(base) => search_searxng(client, query, limit, base).await,
            None => EngineAttempt::Error("OOPBOT_SEARXNG_URL / webSearch.searxngUrl belum diatur".to_string()),
        },
        SearchEngine::Wikipedia => search_wikipedia_multi(client, query, limit, ep).await,
        SearchEngine::Github => search_github(client, query, limit, ep).await,
        SearchEngine::HackerNews => search_hackernews(client, query, limit, ep).await,
        SearchEngine::DdgInstant => search_ddg_instant(client, query, limit, ep).await,
        SearchEngine::DuckDuckGo => search_ddg_html(client, query, limit, ep).await,
    }
}

fn merge_round_robin(sources: Vec<(&'static str, Vec<Value>)>, limit: usize) -> Vec<Value> {
    let mut queues: Vec<std::collections::VecDeque<Value>> =
        sources.into_iter().map(|(_, items)| items.into()).collect();
    let mut seen_urls = HashSet::new();
    let mut out = Vec::with_capacity(limit);

    while out.len() < limit {
        let mut progressed = false;
        for q in &mut queues {
            while let Some(item) = q.pop_front() {
                let url = item["url"].as_str().unwrap_or("").to_string();
                if url.is_empty() || seen_urls.insert(url) {
                    out.push(item);
                    progressed = true;
                    break;
                }
            }
            if out.len() >= limit {
                break;
            }
        }
        if !progressed {
            break;
        }
    }
    out
}

async fn search_gemini_grounding(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    api_key: &str,
    base_url: &str,
    model: &str,
) -> EngineAttempt {
    let clean_model = model.strip_prefix("models/").unwrap_or(model);
    let url = format!(
        "{}/models/{}:generateContent",
        base_url.trim_end_matches('/'),
        clean_model
    );
    let payload = json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "text": format!(
                    "Perform a Google Search for current, accurate information about: \"{}\"\n\
                     Provide a concise factual summary grounded strictly in the search results.",
                    query
                )
            }]
        }],
        "tools": [{ "google_search": {} }],
        "generationConfig": {
            "temperature": 0.2,
            "maxOutputTokens": 1024
        }
    });

    let res = match client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };

    let status = res.status();
    if matches!(status.as_u16(), 401 | 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }

    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => {
            if !status.is_success() {
                return EngineAttempt::Error(format!("HTTP {}", status));
            }
            return EngineAttempt::Error(e.to_string());
        }
    };

    if !status.is_success() {
        let msg = body["error"]["message"]
            .as_str()
            .unwrap_or("request rejected");
        return EngineAttempt::Error(format!("HTTP {}: {}", status, msg));
    }

    let candidate = &body["candidates"][0];
    let summary: String = candidate["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p["thought"].as_bool() != Some(true))
                .filter_map(|p| p["text"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let grounding = &candidate["groundingMetadata"];
    let mut chunk_snippets: HashMap<usize, Vec<String>> = HashMap::new();
    if let Some(supports) = grounding["groundingSupports"].as_array() {
        for sup in supports {
            let text = sup["segment"]["text"].as_str().unwrap_or("").trim();
            if text.is_empty() {
                continue;
            }
            if let Some(indices) = sup["groundingChunkIndices"].as_array() {
                for idx in indices.iter().filter_map(|v| v.as_u64()) {
                    chunk_snippets
                        .entry(idx as usize)
                        .or_default()
                        .push(text.to_string());
                }
            }
        }
    }

    let mut seen_urls = HashSet::new();
    let mut results = Vec::new();
    if let Some(chunks) = grounding["groundingChunks"].as_array() {
        for (idx, chunk) in chunks.iter().enumerate() {
            let web = &chunk["web"];
            let uri = web["uri"].as_str().unwrap_or("").trim();
            if uri.is_empty() || !seen_urls.insert(uri.to_string()) {
                continue;
            }
            let title = web["title"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(uri);

            let snippet = chunk_snippets
                .get(&idx)
                .map(|parts| parts.join(" · "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if summary.is_empty() {
                        format!("Google Search result ({})", title)
                    } else {
                        clip_chars(&summary, 280)
                    }
                });

            results.push(json!({
                "title": title,
                "url": uri,
                "snippet": clip_chars(&snippet, 400),
                "source": "google_search"
            }));
            if results.len() >= limit {
                break;
            }
        }
    }

    EngineAttempt::Results(SearchOutcome {
        results,
        summary: if summary.is_empty() {
            None
        } else {
            Some(summary)
        },
    })
}

fn clip_chars(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

async fn search_tavily(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    api_key: &str,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let payload = json!({
        "api_key": api_key,
        "query": query,
        "max_results": limit,
        "search_depth": "basic"
    });
    let res = match client.post(&ep.tavily_url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let status = res.status();
    if matches!(status.as_u16(), 401 | 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }
    if !status.is_success() {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let results = body["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(limit)
                .filter_map(|item| {
                    let url = item["url"].as_str()?.trim();
                    if url.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "title": item["title"].as_str().unwrap_or("").trim(),
                        "url": url,
                        "snippet": item["content"].as_str().unwrap_or("").trim(),
                        "source": "tavily"
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    EngineAttempt::Results(SearchOutcome::plain(results))
}

async fn search_brave(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    api_key: &str,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let url = format!("{}?q={}&count={}", ep.brave_url, urlencoding(query), limit);
    let res = match client
        .get(&url)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let status = res.status();
    if matches!(status.as_u16(), 401 | 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }
    if !status.is_success() {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let results = body["web"]["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(limit)
                .filter_map(|item| {
                    let url = item["url"].as_str()?.trim();
                    if url.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "title": item["title"].as_str().unwrap_or("").trim(),
                        "url": url,
                        "snippet": strip_html_tags(item["description"].as_str().unwrap_or("")),
                        "source": "brave"
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    EngineAttempt::Results(SearchOutcome::plain(results))
}

async fn search_serper(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    api_key: &str,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let payload = json!({
        "q": query,
        "num": limit
    });
    let res = match client
        .post(&ep.serper_url)
        .header("X-API-KEY", api_key)
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let status = res.status();
    if matches!(status.as_u16(), 401 | 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }
    if !status.is_success() {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let results = body["organic"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(limit)
                .filter_map(|item| {
                    let url = item["link"].as_str()?.trim();
                    if url.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "title": item["title"].as_str().unwrap_or("").trim(),
                        "url": url,
                        "snippet": item["snippet"].as_str().unwrap_or("").trim(),
                        "source": "serper"
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    EngineAttempt::Results(SearchOutcome::plain(results))
}

async fn search_searxng(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    base_url: &str,
) -> EngineAttempt {
    let base = base_url.trim_end_matches('/');
    let url = format!("{}/search?q={}&format=json", base, urlencoding(query));
    let res = match client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let status = res.status();
    if matches!(status.as_u16(), 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }
    if !status.is_success() {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let results = body["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(limit)
                .filter_map(|item| {
                    let url = item["url"].as_str()?.trim();
                    if url.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "title": item["title"].as_str().unwrap_or("").trim(),
                        "url": url,
                        "snippet": strip_html_tags(item["content"].as_str().unwrap_or("")),
                        "source": "searxng"
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    EngineAttempt::Results(SearchOutcome::plain(results))
}

async fn search_wikipedia_multi(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let per_lang = limit.min(5);
    let (en_res, id_res) = tokio::join!(
        search_wikipedia_lang(client, query, per_lang, &ep.wikipedia_en_url, "https://en.wikipedia.org"),
        search_wikipedia_lang(client, query, per_lang, &ep.wikipedia_id_url, "https://id.wikipedia.org"),
    );

    let mut combined = Vec::new();
    let mut errors = Vec::new();
    for res in [en_res, id_res] {
        match res {
            Ok(mut items) => combined.append(&mut items),
            Err(e) => errors.push(e),
        }
    }
    if combined.is_empty() && errors.len() == 2 {
        return EngineAttempt::Error(errors.join("; "));
    }
    combined.truncate(limit);
    EngineAttempt::Results(SearchOutcome::plain(combined))
}

async fn search_wikipedia_lang(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    api_url: &str,
    wiki_origin: &str,
) -> Result<Vec<Value>, String> {
    let url = format!(
        "{}?action=query&list=search&srsearch={}&srlimit={}&utf8=1&format=json",
        api_url,
        urlencoding(query),
        limit
    );
    let res = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let body: Value = res.json().await.map_err(|e| e.to_string())?;
    let items = body["query"]["search"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let title = item["title"].as_str()?.trim();
                    if title.is_empty() {
                        return None;
                    }
                    let slug = urlencoding(&title.replace(' ', "_"));
                    let page_url = format!("{}/wiki/{}", wiki_origin, slug);
                    let snippet = strip_html_tags(item["snippet"].as_str().unwrap_or(""));
                    Some(json!({
                        "title": title,
                        "url": page_url,
                        "snippet": snippet,
                        "source": "wikipedia"
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(items)
}

async fn search_github(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let url = format!(
        "{}?q={}&per_page={}",
        ep.github_url,
        urlencoding(query),
        limit.min(8)
    );
    let res = match client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let status = res.status();
    if matches!(status.as_u16(), 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }
    if !status.is_success() {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let results = body["items"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(limit)
                .filter_map(|item| {
                    let html_url = item["html_url"].as_str()?.trim();
                    let full_name = item["full_name"].as_str().unwrap_or(html_url);
                    let stars = item["stargazers_count"].as_u64().unwrap_or(0);
                    let lang = item["language"].as_str().unwrap_or("");
                    let desc = item["description"].as_str().unwrap_or("").trim();
                    let meta = if lang.is_empty() {
                        format!("★ {}", stars)
                    } else {
                        format!("★ {} · {}", stars, lang)
                    };
                    let snippet = if desc.is_empty() {
                        meta
                    } else {
                        format!("{} ({})", desc, meta)
                    };
                    Some(json!({
                        "title": full_name,
                        "url": html_url,
                        "snippet": snippet,
                        "source": "github"
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    EngineAttempt::Results(SearchOutcome::plain(results))
}

async fn search_hackernews(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let url = format!(
        "{}?query={}&tags=story&hitsPerPage={}",
        ep.hn_url,
        urlencoding(query),
        limit.min(8)
    );
    let res = match client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let status = res.status();
    if matches!(status.as_u16(), 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }
    if !status.is_success() {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let results = body["hits"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(limit)
                .filter_map(|hit| {
                    let title = hit["title"].as_str()?.trim();
                    let object_id = hit["objectID"].as_str().unwrap_or("");
                    let url = hit["url"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={}", object_id));
                    let points = hit["points"].as_u64().unwrap_or(0);
                    let comments = hit["num_comments"].as_u64().unwrap_or(0);
                    Some(json!({
                        "title": title,
                        "url": url,
                        "snippet": format!("Hacker News · {} points · {} comments", points, comments),
                        "source": "hn"
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    EngineAttempt::Results(SearchOutcome::plain(results))
}

async fn search_ddg_instant(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let url = format!(
        "{}?q={}&format=json&no_html=1&skip_disambig=1",
        ep.ddg_instant_url,
        urlencoding(query)
    );
    let res = match client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };
    let status = res.status();
    if matches!(status.as_u16(), 202 | 403 | 429) {
        return EngineAttempt::Blocked(format!("HTTP {}", status));
    }
    if !status.is_success() {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };

    let mut out = Vec::new();
    let abstract_text = body["AbstractText"].as_str().unwrap_or("").trim();
    let abstract_url = body["AbstractURL"].as_str().unwrap_or("").trim();
    let heading = body["Heading"].as_str().unwrap_or(query).trim();
    if !abstract_text.is_empty() && !abstract_url.is_empty() {
        out.push(json!({
            "title": heading,
            "url": abstract_url,
            "snippet": abstract_text,
            "source": "ddg_instant"
        }));
    }

    if let Some(topics) = body["RelatedTopics"].as_array() {
        collect_ddg_topics(topics, limit, &mut out);
    }
    out.truncate(limit);
    EngineAttempt::Results(SearchOutcome::plain(out))
}

fn collect_ddg_topics(topics: &[Value], limit: usize, out: &mut Vec<Value>) {
    for item in topics {
        if out.len() >= limit {
            break;
        }
        if let Some(nested) = item.get("Topics").and_then(|v| v.as_array()) {
            collect_ddg_topics(nested, limit, out);
            continue;
        }
        let url = item["FirstURL"].as_str().unwrap_or("").trim();
        let text = item["Text"].as_str().unwrap_or("").trim();
        if !url.is_empty() && !text.is_empty() {
            let title = text.split(" - ").next().unwrap_or(text).trim();
            out.push(json!({
                "title": title,
                "url": url,
                "snippet": text,
                "source": "ddg_instant"
            }));
        }
    }
}

async fn search_ddg_html(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    ep: &WebSearchEndpoints,
) -> EngineAttempt {
    let url = format!("{}?q={}", ep.ddg_html_url, urlencoding(query));
    let res = match client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        )
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "id-ID,id;q=0.9,en-US;q=0.8,en;q=0.7")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };

    let status = res.status().as_u16();
    let body = match res.text().await {
        Ok(t) => t,
        Err(e) => return EngineAttempt::Error(e.to_string()),
    };

    if is_ddg_challenge(status, &body) {
        return EngineAttempt::Blocked(format!("HTTP {} anti-bot challenge", status));
    }
    if !(200..300).contains(&status) {
        return EngineAttempt::Error(format!("HTTP {}", status));
    }

    EngineAttempt::Results(SearchOutcome::plain(parse_ddg_results(&body, limit)))
}
