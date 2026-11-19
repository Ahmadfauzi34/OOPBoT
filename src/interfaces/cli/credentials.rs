//! Resolusi kredensial API provider dari variabel lingkungan untuk mode CLI.

use std::env;

pub struct CliCredentials {
    pub provider_name: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
}

pub fn resolve_cli_credentials(cli_model: Option<&str>) -> Option<CliCredentials> {
    // 1. Cek OOPBOT_API_KEY (universal / kustom / lokal)
    if let Ok(key) = env::var("OOPBOT_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("OOPBOT_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("OOPBOT_MODEL")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| {
                    if key.starts_with("sk-or-") {
                        "deepseek/deepseek-chat".to_string()
                    } else {
                        "default".to_string()
                    }
                });
            return Some(CliCredentials {
                provider_name: "OOPBoT / Kustom".to_string(),
                api_key: key,
                base_url,
                model,
            });
        }
    }

    // 2. Cek OPENROUTER_API_KEY
    if let Ok(key) = env::var("OPENROUTER_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("OPENROUTER_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("OPENROUTER_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "deepseek/deepseek-chat".to_string());
            return Some(CliCredentials {
                provider_name: "OpenRouter".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 3. Cek DEEPSEEK_API_KEY
    if let Ok(key) = env::var("DEEPSEEK_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("DEEPSEEK_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("DEEPSEEK_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "deepseek-chat".to_string());
            return Some(CliCredentials {
                provider_name: "DeepSeek".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 4. Cek OPENAI_API_KEY
    if let Ok(key) = env::var("OPENAI_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("OPENAI_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "gpt-4o-mini".to_string());
            return Some(CliCredentials {
                provider_name: "OpenAI".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 5. Cek ANTHROPIC_API_KEY
    if let Ok(key) = env::var("ANTHROPIC_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("ANTHROPIC_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://api.anthropic.com".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("ANTHROPIC_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "claude-3-5-haiku-20241022".to_string());
            return Some(CliCredentials {
                provider_name: "Anthropic".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    // 6. Cek GEMINI_API_KEY
    if let Ok(key) = env::var("GEMINI_API_KEY").map(|s| s.trim().to_string()) {
        if !key.is_empty() {
            let base_url = env::var("GEMINI_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
            let model = cli_model
                .map(|s| s.to_string())
                .or_else(|| {
                    env::var("GEMINI_MODEL")
                        .or_else(|_| env::var("OOPBOT_MODEL"))
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or_else(|| "gemini-2.5-flash".to_string());
            return Some(CliCredentials {
                provider_name: "Google Gemini".to_string(),
                api_key: key,
                base_url: Some(base_url),
                model,
            });
        }
    }

    None
}

pub fn print_missing_credentials_help() {
    eprintln!(
        "❌ Kunci API (API Key) belum ditemukan.\n\n\
         OOPBoT mendukung berbagai provider AI secara mandiri:\n\
         1. OpenRouter (Ratusan model: DeepSeek V3/R1, Claude 3.5, Llama):\n\
            export OPENROUTER_API_KEY=\"sk-or-v1-...\"\n\n\
         2. DeepSeek (Hemat & bertenaga untuk koding):\n\
            export DEEPSEEK_API_KEY=\"sk-...\"\n\n\
         3. Google Gemini:\n\
            export GEMINI_API_KEY=\"...\"\n\n\
         4. OpenAI:\n\
            export OPENAI_API_KEY=\"sk-...\"\n\n\
         5. Anthropic Claude:\n\
            export ANTHROPIC_API_KEY=\"sk-ant-...\"\n\n\
         6. Server Kustom / Lokal (Ollama, LMStudio, Groq, Together):\n\
            export OOPBOT_API_KEY=\"...\"\n\
            export OOPBOT_BASE_URL=\"http://localhost:11434/v1\"\n\
            export OOPBOT_MODEL=\"llama3.2\"\n\n\
         Atau simpan salah satu konfigurasi di atas ke file:\n\
            ~/.config/oopbot/.env\n"
    );
}
