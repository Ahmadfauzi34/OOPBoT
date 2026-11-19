//! Renderer HTML sesi percakapan (`render_session_html` & `html_escape`) untuk command `export_html`.

use super::extract_user_message_text;
use serde_json::Value;
use std::path::Path;

pub(super) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub(super) fn render_session_html(
    session_id: &str,
    session_name: Option<&str>,
    cwd: &Path,
    ui_messages: &[Value],
) -> String {
    let title = session_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(session_id);
    let mut body = String::new();

    for msg in ui_messages {
        let role = msg["role"].as_str().unwrap_or("unknown");
        let (role_label, class_name, text_html) = match role {
            "user" => (
                "User",
                "user",
                format!(
                    "<pre>{}</pre>",
                    html_escape(&extract_user_message_text(msg))
                ),
            ),
            "assistant" => {
                let mut inner = String::new();
                if let Some(arr) = msg["content"].as_array() {
                    for b in arr {
                        match b["type"].as_str().unwrap_or_default() {
                            "thinking" => {
                                if let Some(th) = b["thinking"].as_str() {
                                    inner.push_str(&format!(
                                        "<details><summary>Thinking</summary><pre>{}</pre></details>",
                                        html_escape(th)
                                    ));
                                }
                            }
                            "text" => {
                                if let Some(t) = b["text"].as_str() {
                                    inner.push_str(&format!("<pre>{}</pre>", html_escape(t)));
                                }
                            }
                            "toolCall" => {
                                let name = b["name"].as_str().unwrap_or("tool");
                                let args = b
                                    .get("arguments")
                                    .or_else(|| b.get("args"))
                                    .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                    .unwrap_or_default();
                                inner.push_str(&format!(
                                    "<div class=\"tool\"><strong>Tool: {}</strong><pre>{}</pre></div>",
                                    html_escape(name),
                                    html_escape(&args)
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                if inner.is_empty() {
                    if let Some(err) = msg["errorMessage"].as_str() {
                        inner = format!("<pre class=\"err\">{}</pre>", html_escape(err));
                    }
                }
                ("Assistant", "assistant", inner)
            }
            "toolResult" => {
                let name = msg["toolName"].as_str().unwrap_or("tool");
                (
                    "Tool Result",
                    "tool-result",
                    format!(
                        "<div class=\"tool\"><strong>{}</strong><pre>{}</pre></div>",
                        html_escape(name),
                        html_escape(&extract_user_message_text(msg))
                    ),
                )
            }
            "compactionSummary" => (
                "Compaction Summary",
                "summary",
                format!(
                    "<pre>{}</pre>",
                    html_escape(msg["summary"].as_str().unwrap_or_default())
                ),
            ),
            "branchSummary" => (
                "Branch Summary",
                "summary",
                format!(
                    "<pre>{}</pre>",
                    html_escape(msg["summary"].as_str().unwrap_or_default())
                ),
            ),
            "bashExecution" => (
                "Bash",
                "bash",
                format!(
                    "<pre>$ {}\n{}</pre>",
                    html_escape(msg["command"].as_str().unwrap_or_default()),
                    html_escape(msg["output"].as_str().unwrap_or_default())
                ),
            ),
            _ => (
                role,
                "other",
                format!(
                    "<pre>{}</pre>",
                    html_escape(&extract_user_message_text(msg))
                ),
            ),
        };
        body.push_str(&format!(
            "<section class=\"msg {}\"><h3>{}</h3>{}</section>\n",
            class_name,
            html_escape(role_label),
            text_html
        ));
    }

    format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<title>{title}</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 860px; margin: 2rem auto; padding: 0 1rem; background: #0f1115; color: #e6edf3; }}
header {{ border-bottom: 1px solid #30363d; margin-bottom: 1.5rem; padding-bottom: 1rem; }}
.msg {{ border: 1px solid #30363d; border-radius: 8px; padding: 0.75rem 1rem; margin-bottom: 1rem; background: #161b22; }}
.msg.user {{ border-left: 4px solid #58a6ff; }}
.msg.assistant {{ border-left: 4px solid #3fb950; }}
.msg.summary {{ border-left: 4px solid #d29922; }}
pre {{ white-space: pre-wrap; word-break: break-word; font-family: ui-monospace, monospace; margin: 0.25rem 0; }}
.tool {{ background: #0d1117; padding: 0.5rem; border-radius: 6px; margin: 0.5rem 0; }}
</style>
</head>
<body>
<header>
<h1>{title}</h1>
<p>Session ID: <code>{sid}</code> · CWD: <code>{cwd}</code></p>
</header>
{body}
</body>
</html>",
        title = html_escape(title),
        sid = html_escape(session_id),
        cwd = html_escape(&cwd.to_string_lossy()),
        body = body,
    )
}
