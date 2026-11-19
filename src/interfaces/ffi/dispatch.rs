//! Router JSON `dispatch_request` untuk C-ABI & JNI `OopbotHandle`.

use super::OopbotHandle;
use crate::agent::Agent;
use crate::config::builtin_catalog_json;
use serde_json::{json, Value};

pub(crate) fn dispatch_request(h: &OopbotHandle, req: &Value) -> Value {
    match req["op"].as_str().unwrap_or("") {
        "catalog" => {
            let mut cat = builtin_catalog_json();
            cat["ok"] = json!(true);
            cat
        }
        "get_state" => {
            let info = h.agent.active_model_info();
            json!({
                "ok": true,
                "active_model": h.agent.model(),
                "model": info.to_oopbot_json(None),
                "thinking_level": h.agent.thinking_level(),
                "available_thinking_levels": info.available_thinking_levels(),
            })
        }
        "models" => json!({
            "ok": true,
            "active_model": h.agent.model(),
            "thinking_level": h.agent.thinking_level(),
            "models": h.agent.list_models(),
        }),
        "scan_models" => match h.runtime.block_on(h.agent.scan_models()) {
            Ok(models) => json!({
                "ok": true,
                "count": models.len(),
                "active_model": h.agent.model(),
                "models": models,
            }),
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        },
        "set_provider" => {
            let Some(api_key) = req["api_key"].as_str() else {
                return json!({ "ok": false, "error": "missing 'api_key'" });
            };
            let base_url = req["base_url"].as_str();
            match h
                .runtime
                .block_on(h.agent.update_provider_token(api_key, base_url))
            {
                Ok(models) => json!({
                    "ok": true,
                    "count": models.len(),
                    "active_model": h.agent.model(),
                    "models": models,
                }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        "set_model" => {
            let Some(m) = req["model"].as_str() else {
                return json!({ "ok": false, "error": "missing 'model'" });
            };
            let active = h.agent.set_active_model(m);
            let clamped_level = h.agent.set_thinking_level(&h.agent.thinking_level());
            let info = h.agent.active_model_info();
            json!({
                "ok": true,
                "active_model": active,
                "thinking_level": clamped_level,
                "model": info.to_oopbot_json(None),
            })
        }
        "set_thinking_level" => {
            let Some(level) = req["level"].as_str() else {
                return json!({ "ok": false, "error": "missing 'level'" });
            };
            let clamped = h.agent.set_thinking_level(level);
            json!({
                "ok": true,
                "thinking_level": clamped,
                "available_thinking_levels": h.agent.active_model_info().available_thinking_levels(),
            })
        }
        "configure_model" => {
            let target_model = req["model"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| h.agent.model());
            let patch = req.get("override").unwrap_or(req);
            let updated = h.agent.configure_model(&target_model, patch);
            let clamped = h.agent.set_thinking_level(&h.agent.thinking_level());
            json!({
                "ok": true,
                "model": updated.to_oopbot_json(None),
                "thinking_level": clamped,
            })
        }
        "tabs" => {
            let (active_index, tabs) = h.agent.list_tabs();
            json!({ "ok": true, "active_index": active_index, "tabs": tabs })
        }
        "new_tab" => {
            let tab = h.agent.new_tab(req["model"].as_str());
            json!({ "ok": true, "tab": tab })
        }
        "switch_tab" => {
            let Some(sel) = req["tab"].as_str() else {
                return json!({ "ok": false, "error": "missing 'tab'" });
            };
            match h.agent.switch_tab(sel) {
                Some(tab) => json!({ "ok": true, "tab": tab }),
                None => json!({ "ok": false, "error": "tab not found" }),
            }
        }
        "close_tab" => {
            let Some(sel) = req["tab"].as_str() else {
                return json!({ "ok": false, "error": "missing 'tab'" });
            };
            json!({ "ok": h.agent.close_tab(sel) })
        }
        "timeline" => json!({
            "ok": true,
            "timeline": h.agent.active_timeline(),
        }),
        "clear" => {
            h.agent.clear_memory();
            json!({ "ok": true })
        }
        "chat" => {
            let Some(prompt) = req["prompt"].as_str() else {
                return json!({ "ok": false, "error": "missing 'prompt'" });
            };
            match h.runtime.block_on(h.agent.chat(prompt)) {
                Ok(reply) => json!({
                    "ok": true,
                    "reply": reply,
                    "timeline": h.agent.active_timeline(),
                }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        other => json!({ "ok": false, "error": format!("unknown op '{}'", other) }),
    }
}
