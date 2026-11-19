//! Handler slash command RPC (`/scan`, `/token`, `/models`, `/model`, `/thinking`,
//! `/provider`, `/max-tokens`, `/clear`, `/todos`).

use super::{
    save_auth_key, save_model_override_to_config, save_scanned_models_to_config,
    save_settings_fields, RpcRuntime,
};
use crate::plugins::{TodoPlugin, NO_TODOS_MESSAGE, TODO_STATE_ENTRY};
use serde_json::json;

impl RpcRuntime {
    pub(super) async fn handle_slash_command(&self, cmd_line: &str) -> Option<String> {
        let parts: Vec<&str> = cmd_line.split_whitespace().collect();
        let cmd = parts.first().copied()?;
        match cmd {
            "/scan" => {
                self.refresh_credentials();
                match self.agent.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        let preview: Vec<String> = scanned
                            .iter()
                            .take(15)
                            .map(|m| {
                                let think_badge = if m.reasoning { " 🧠" } else { "" };
                                format!("- `{}` ({}){}", m.id, m.label, think_badge)
                            })
                            .collect();
                        let (active_provider, cur_key) = {
                            let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                            m.catalog.replace_with_scanned(scanned.clone());
                            let key = m
                                .providers
                                .get(&m.active_provider)
                                .map(|p| p.api_key.clone())
                                .unwrap_or_default();
                            (m.active_provider.clone(), key)
                        };
                        self.sync_agent_catalog();
                        let save_note = match save_scanned_models_to_config(
                            &self.config_dir,
                            &active_provider,
                            &scanned,
                            &cur_key,
                        ) {
                            Ok(()) => "dan menyimpannya ke `models.json`".to_string(),
                            Err(e) => {
                                format!("namun **gagal menyimpan** ke `models.json` ({})", e)
                            }
                        };
                        self.sync_agent_active_model();
                        self.refresh_credentials();
                        Some(format!(
                            "Berhasil men-scan **{} model** dari `{}` {}:\n{}\n\nGunakan pemilih model di atas atau ketik `/model <ID>` untuk mengganti model.",
                            count,
                            self.agent.base_url(),
                            save_note,
                            preview.join("\n")
                        ))
                    }
                    Err(e) => Some(format!("Gagal scan model: {}", e)),
                }
            }
            "/token" => {
                if parts.len() < 2 {
                    return Some(
                        "Format: `/token <API_KEY> [BASE_URL]`\nContoh:\n- `/token AIzaSy...`\n- `/token sk-or-v1-... https://openrouter.ai/api/v1`"
                            .to_string(),
                    );
                }
                let api_key = parts[1].trim();
                let base_url = parts.get(2).map(|s| s.trim().to_string());
                let active_provider = self
                    .mutable
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .active_provider
                    .clone();
                self.agent
                    .update_credentials(api_key.to_string(), base_url.clone());
                let auth_save_err = save_auth_key(
                    &self.config_dir,
                    &active_provider,
                    api_key,
                    base_url.as_deref(),
                )
                .err();
                match self.agent.scan_models().await {
                    Ok(scanned) => {
                        let count = scanned.len();
                        let active_model = {
                            let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                            if !scanned.iter().any(|item| item.id == m.active_model) {
                                if let Some(first) = scanned.first() {
                                    m.active_model = first.id.clone();
                                }
                            }
                            m.catalog.replace_with_scanned(scanned.clone());
                            m.active_model.clone()
                        };
                        self.sync_agent_catalog();
                        self.sync_agent_active_model();
                        let mut save_warnings = Vec::new();
                        if let Some(e) = &auth_save_err {
                            save_warnings.push(format!("`auth.json`: {}", e));
                        }
                        if let Err(e) = save_scanned_models_to_config(
                            &self.config_dir,
                            &active_provider,
                            &scanned,
                            api_key,
                        ) {
                            save_warnings.push(format!("`models.json`: {}", e));
                        }
                        self.refresh_credentials();
                        if let Err(e) = save_settings_fields(
                            &self.config_dir,
                            Some(&active_provider),
                            Some(&active_model),
                            None,
                        ) {
                            save_warnings.push(format!("`settings.json`: {}", e));
                        }
                        if save_warnings.is_empty() {
                            Some(format!(
                                "Token berhasil disimpan & **{} model** terdeteksi otomatis! Model aktif sekarang: `{}` (`{}`).",
                                count, active_model, active_provider
                            ))
                        } else {
                            Some(format!(
                                "Token aktif di memori & **{} model** terdeteksi (`{}`), namun penyimpanan gagal ({}).",
                                count,
                                active_model,
                                save_warnings.join("; ")
                            ))
                        }
                    }
                    Err(e) => {
                        self.refresh_credentials();
                        if let Some(save_err) = auth_save_err {
                            Some(format!(
                                "Gagal menyimpan token ke `auth.json` ({}) dan auto-scan `/models` gagal: {}",
                                save_err, e
                            ))
                        } else {
                            Some(format!(
                                "Token disimpan, namun auto-scan `/models` gagal: {}",
                                e
                            ))
                        }
                    }
                }
            }
            "/models" => {
                self.refresh_credentials();
                let (active_provider, active_model) = {
                    let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                    (m.active_provider.clone(), m.active_model.clone())
                };
                let avail = self.available_models_json();
                let lines: Vec<String> = avail
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let pid = m["provider"].as_str().unwrap_or("");
                        let mid = m["id"].as_str().unwrap_or("");
                        let name = m["name"].as_str().unwrap_or(mid);
                        let cw = m["contextWindow"].as_u64().unwrap_or(0) / 1000;
                        let think = if m["reasoning"].as_bool() == Some(true) {
                            " · 🧠 thinking"
                        } else {
                            ""
                        };
                        let mark = if pid == active_provider && mid == active_model {
                            " **(aktif)**"
                        } else {
                            ""
                        };
                        format!(
                            "{}. `{}/{}` — {} ({}k ctx{}){}",
                            i + 1,
                            pid,
                            mid,
                            name,
                            cw,
                            think,
                            mark
                        )
                    })
                    .collect();
                Some(format!(
                    "**Daftar Model Tersedia ({}):**\n{}",
                    avail.len(),
                    lines.join("\n")
                ))
            }
            "/model" => {
                if parts.len() < 2 {
                    let (active_provider, active_model, thinking_level, info) = {
                        let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        (
                            m.active_provider.clone(),
                            m.active_model.clone(),
                            m.thinking_level.clone(),
                            Self::model_info_from_state(&m),
                        )
                    };
                    return Some(format!(
                        "Model aktif saat ini: `{}/{}` (**{}**)\n- API: `{}`\n- Context window: `{}` token\n- Max output: `{}` token\n- Thinking: `{}` (level aktif: `{}`)\n\nGunakan `/model <ID>` atau `/model <provider>/<ID>` untuk mengganti.",
                        active_provider,
                        active_model,
                        info.label,
                        info.api,
                        info.input_token_limit.unwrap_or(1_048_576),
                        info.output_token_limit.unwrap_or(65_536),
                        info.reasoning,
                        thinking_level
                    ));
                }
                let target = parts[1].trim();
                {
                    let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some((prov, mid)) = target.split_once('/') {
                        if m.providers.contains_key(prov) {
                            m.active_provider = prov.to_string();
                            m.active_model = mid.to_string();
                        } else {
                            m.active_model = target.to_string();
                        }
                    } else if let Some(info) = m.catalog.find(target) {
                        m.active_model = info.id.clone();
                    } else {
                        let mut found = None;
                        for (pid, ep) in &m.providers {
                            if let Some(model) = ep
                                .models
                                .iter()
                                .find(|item| item.id.eq_ignore_ascii_case(target))
                            {
                                found = Some((pid.clone(), model.id.clone()));
                                break;
                            }
                        }
                        if let Some((pid, mid)) = found {
                            m.active_provider = pid;
                            m.active_model = mid;
                        } else {
                            m.catalog.register_custom(target, target);
                            m.active_model = target.to_string();
                        }
                    }
                }
                self.sync_agent_active_model();
                self.refresh_credentials();
                self.sync_thinking_level_for_active_model();
                let (active_provider, active_model, thinking_level) = {
                    let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        m.active_provider.clone(),
                        m.active_model.clone(),
                        m.thinking_level.clone(),
                    )
                };
                if let Err(e) = save_settings_fields(
                    &self.config_dir,
                    Some(&active_provider),
                    Some(&active_model),
                    Some(&thinking_level),
                ) {
                    Some(format!(
                        "Model aktif diganti ke `{}/{}` (thinking: `{}`), namun gagal menyimpan ke `settings.json` ({}).",
                        active_provider, active_model, thinking_level, e
                    ))
                } else {
                    Some(format!(
                        "Model aktif diganti ke `{}/{}` (thinking: `{}`).",
                        active_provider, active_model, thinking_level
                    ))
                }
            }
            "/thinking" => {
                let (info, active_model, thinking_level) = {
                    let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        Self::model_info_from_state(&m),
                        m.active_model.clone(),
                        m.thinking_level.clone(),
                    )
                };
                let supported = info.available_thinking_levels();
                if parts.len() < 2 {
                    return Some(format!(
                        "Level thinking saat ini: `{}` (didukung oleh `{}`: `{}`).\nGunakan `/thinking <{}>` untuk mengubah.",
                        thinking_level,
                        active_model,
                        supported.join(", "),
                        supported.join("|")
                    ));
                }
                let requested = parts[1].trim().to_ascii_lowercase();
                let clamped = info.clamp_thinking_level(&requested);
                {
                    self.mutable
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .thinking_level = clamped.clone();
                }
                self.agent.set_thinking_level(&clamped);
                let save_res = save_settings_fields(&self.config_dir, None, None, Some(&clamped));
                Self::send_line(&json!({
                    "type": "thinking_level_changed",
                    "level": clamped,
                }));
                match save_res {
                    Ok(()) => Some(format!(
                        "Level thinking diatur ke `{}` untuk model `{}`.",
                        clamped, active_model
                    )),
                    Err(e) => Some(format!(
                        "Level thinking diatur ke `{}` untuk model `{}`, namun gagal menyimpan ke `settings.json` ({}).",
                        clamped, active_model, e
                    )),
                }
            }
            "/provider" => {
                if parts.len() < 2 {
                    let (providers, active_provider) = {
                        let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        (m.providers.clone(), m.active_provider.clone())
                    };
                    let mut lines = Vec::new();
                    for (pid, ep) in &providers {
                        let mark = if *pid == active_provider {
                            " **(aktif)**"
                        } else {
                            ""
                        };
                        let auth_badge = if ep.has_auth {
                            "✅ siap"
                        } else {
                            "⚪ belum ada key"
                        };
                        lines.push(format!(
                            "- `{}` ({}) — {} · {} model{}",
                            pid,
                            ep.base_url,
                            auth_badge,
                            ep.models.len(),
                            mark
                        ));
                    }
                    lines.sort();
                    return Some(format!(
                        "**Daftar Provider:**\n{}\n\nGunakan `/provider <ID>` untuk berpindah provider.",
                        lines.join("\n")
                    ));
                }
                let target_pid = parts[1].trim();
                let ep_opt = self
                    .mutable
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .providers
                    .get(target_pid)
                    .cloned();
                if let Some(ep) = ep_opt {
                    {
                        let mut m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        m.active_provider = target_pid.to_string();
                        if let Some(first) = ep.models.first() {
                            m.active_model = first.id.clone();
                        }
                    }
                    self.refresh_credentials();
                    self.sync_agent_active_model();
                    self.sync_thinking_level_for_active_model();
                    let (active_provider, active_model, thinking_level) = {
                        let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                        (
                            m.active_provider.clone(),
                            m.active_model.clone(),
                            m.thinking_level.clone(),
                        )
                    };
                    if let Err(e) = save_settings_fields(
                        &self.config_dir,
                        Some(&active_provider),
                        Some(&active_model),
                        Some(&thinking_level),
                    ) {
                        Some(format!(
                            "Provider aktif diganti ke `{}` dengan model `{}`, namun gagal menyimpan ke `settings.json` ({}).",
                            active_provider, active_model, e
                        ))
                    } else {
                        Some(format!(
                            "Provider aktif diganti ke `{}` dengan model `{}`.",
                            active_provider, active_model
                        ))
                    }
                } else {
                    Some(format!("Provider `{}` tidak ditemukan.", target_pid))
                }
            }
            "/max-tokens" => {
                if parts.len() < 2 {
                    let cur = self
                        .active_model_info()
                        .output_token_limit
                        .unwrap_or(65_536);
                    let active_model = self
                        .mutable
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .active_model
                        .clone();
                    return Some(format!(
                        "Batas `maxTokens` model `{}` saat ini: `{}`.\nGunakan `/max-tokens <angka>` untuk mengubah dan menyimpan ke `models.json`.",
                        active_model, cur
                    ));
                }
                let Ok(max_tok) = parts[1].trim().parse::<u64>() else {
                    return Some(
                        "Nilai `/max-tokens` harus berupa angka bulat positif.".to_string(),
                    );
                };
                if max_tok == 0 {
                    return Some("Nilai `/max-tokens` harus lebih besar dari 0.".to_string());
                }
                let (active_provider, active_model) = {
                    let m = self.mutable.lock().unwrap_or_else(|e| e.into_inner());
                    (m.active_provider.clone(), m.active_model.clone())
                };
                if let Err(e) = save_model_override_to_config(
                    &self.config_dir,
                    &active_provider,
                    &active_model,
                    &json!({ "maxTokens": max_tok }),
                ) {
                    return Some(format!(
                        "Gagal menyimpan `maxTokens` untuk `{}/{}` ke `models.json`: {}",
                        active_provider, active_model, e
                    ));
                }
                self.refresh_credentials();
                self.sync_agent_active_model();
                Some(format!(
                    "`maxTokens` untuk `{}/{}` berhasil disimpan ke `{}`.",
                    active_provider, active_model, max_tok
                ))
            }
            "/clear" => {
                self.ensure_session_header();
                {
                    let mut state = self.ui_state.lock().unwrap_or_else(|e| e.into_inner());
                    state.ui_messages.clear();
                    state.last_context_tokens = None;
                    state.context_unknown = false;
                    state.append_session_entry("context_reset", json!({ "reason": "clear" }));
                }
                self.agent.clear_memory_silent();
                Some("Memori percakapan sesi ini telah dibersihkan.".to_string())
            }
            "/todos" => {
                let Some(plugin) = self.plugins.get_plugin("oopbot-todo") else {
                    return Some(NO_TODOS_MESSAGE.to_string());
                };
                let Some(todo_plugin) = plugin
                    .as_any()
                    .and_then(|a| a.downcast_ref::<TodoPlugin>())
                else {
                    return Some(NO_TODOS_MESSAGE.to_string());
                };
                if parts.get(1).copied() == Some("clear") {
                    if let Some(details) = todo_plugin.clear_state() {
                        self.ensure_session_header();
                        self.append_session_entry(
                            "custom",
                            json!({ "customType": TODO_STATE_ENTRY, "data": details }),
                        );
                    }
                    self.emit_active_widgets();
                    return Some("Daftar tugas pada sesi ini telah dikosongkan.".to_string());
                }
                Some(
                    todo_plugin
                        .command_text()
                        .unwrap_or_else(|| NO_TODOS_MESSAGE.to_string()),
                )
            }
            _ => None,
        }
    }
}
