#[allow(unused_imports)]
use crate::models::{ModelCatalog, ModelInfo};
#[allow(unused_imports)]
use crate::util::{epoch_ms, iso_now, parse_iso_to_epoch_ms_opt as parse_iso_to_epoch_ms, read_json_file};

use super::*;
use std::io::Cursor;

fn spawn_provider_models_mock(
    responses: Vec<(u16, String, String)>,
) -> (
    String,
    Arc<Mutex<Vec<String>>>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{}", port);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    let handle = std::thread::spawn(move || {
        for (status, content_type, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            captured_clone.lock().unwrap().push(req);
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                404 => "Not Found",
                429 => "Too Many Requests",
                _ => "Error",
            };
            let reply = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                reason,
                content_type,
                body.len(),
                body
            );
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
        }
    });
    (base_url, captured, handle)
}

#[tokio::test]
async fn reg005_handle_provider_models_fails_closed_on_bad_url_and_http_errors() {
    // 1. Empty / non-HTTP URL -> bad_url (never returns builtin catalog)
    let empty = handle_provider_models(&json!({})).await;
    assert_eq!(empty["ok"], false);
    assert_eq!(empty["code"], "bad_url");

    let ftp = handle_provider_models(&json!({ "baseUrl": "ftp://example.com" })).await;
    assert_eq!(ftp["ok"], false);
    assert_eq!(ftp["code"], "bad_url");

    // 2. HTTP 401 -> unauthorized, 404 -> not_found, HTML/invalid JSON -> bad_response
    let (base_url, _req_log, server) = spawn_provider_models_mock(vec![
        (
            401,
            "application/json".to_string(),
            json!({ "error": { "message": "Invalid API key" } }).to_string(),
        ),
        (404, "text/plain".to_string(), "Not found".to_string()),
        (200, "text/html".to_string(), "<!doctype html><html>login</html>".to_string()),
    ]);

    let unauth = handle_provider_models(&json!({
        "baseUrl": base_url,
        "apiKey": "bad-key",
        "api": "openai-completions"
    }))
    .await;
    assert_eq!(unauth["ok"], false);
    assert_eq!(unauth["code"], "unauthorized");
    assert!(unauth["error"].as_str().unwrap().contains("Invalid API key"));

    let nf = handle_provider_models(&json!({
        "baseUrl": base_url,
        "apiKey": "k",
        "api": "openai-completions"
    }))
    .await;
    assert_eq!(nf["ok"], false);
    assert_eq!(nf["code"], "not_found");

    let html = handle_provider_models(&json!({
        "baseUrl": base_url,
        "apiKey": "k",
        "api": "openai-completions"
    }))
    .await;
    assert_eq!(html["ok"], false);
    assert_eq!(html["code"], "bad_response");
    assert_eq!(html["i18n"]["key"], "provider_web_page");

    server.join().unwrap();
}

#[tokio::test]
async fn reg005_handle_provider_models_sends_protocol_specific_headers() {
    let (base_url, captured, server) = spawn_provider_models_mock(vec![
        (
            200,
            "application/json".to_string(),
            json!({ "models": [{ "name": "models/gemini-2.5-flash", "displayName": "Gemini 2.5 Flash" }] })
                .to_string(),
        ),
        (
            200,
            "application/json".to_string(),
            json!({ "data": [{ "id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5" }] })
                .to_string(),
        ),
        (
            200,
            "application/json".to_string(),
            json!({ "data": [{ "id": "local-qwen" }] }).to_string(),
        ),
    ]);

    // Google uses x-goog-api-key and /models?pageSize=100 (NO Authorization: Bearer)
    let google_res = handle_provider_models(&json!({
        "baseUrl": base_url,
        "apiKey": "AIza-test",
        "api": "google-generative-ai"
    }))
    .await;
    assert_eq!(google_res["ok"], true);
    assert_eq!(google_res["data"]["models"][0]["id"], "gemini-2.5-flash");

    // Anthropic uses x-api-key + anthropic-version
    let ant_res = handle_provider_models(&json!({
        "baseUrl": base_url,
        "apiKey": "sk-ant-test",
        "api": "anthropic-messages"
    }))
    .await;
    assert_eq!(ant_res["ok"], true);
    assert_eq!(ant_res["data"]["models"][0]["id"], "claude-sonnet-4-5");

    // OpenAI-compatible uses Bearer token (falling back to no-api-key)
    let oai_res = handle_provider_models(&json!({
        "baseUrl": base_url,
        "apiKey": "",
        "api": "openai-completions"
    }))
    .await;
    assert_eq!(oai_res["ok"], true);
    assert_eq!(oai_res["data"]["models"][0]["id"], "local-qwen");

    server.join().unwrap();
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 3);

    let g_lower = reqs[0].to_ascii_lowercase();
    assert!(reqs[0].starts_with("GET /models?pageSize=100 "));
    assert!(g_lower.contains("x-goog-api-key: aiza-test"));
    assert!(!g_lower.contains("authorization: bearer"));

    let a_lower = reqs[1].to_ascii_lowercase();
    assert!(reqs[1].starts_with("GET /models "));
    assert!(a_lower.contains("x-api-key: sk-ant-test"));
    assert!(a_lower.contains("anthropic-version: 2023-06-01"));

    let o_lower = reqs[2].to_ascii_lowercase();
    assert!(reqs[2].starts_with("GET /models "));
    assert!(o_lower.contains("authorization: bearer no-api-key"));
}

#[test]
fn test_handle_model_defaults() {
    let params = json!({
        "ids": ["gemini-2.5-flash", "unknown-model"],
        "api": "openai-completions"
    });
    let res = handle_model_defaults(&params);
    assert_eq!(res["type"], "pipkin_result");
    assert_eq!(res["ok"], true);
    let models = res["data"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "gemini-2.5-flash");
    assert_eq!(models[1]["id"], "unknown-model");
}

#[test]
fn test_auth_status_schema_and_builtin() {
    let temp_dir = std::env::temp_dir().join(format!("oopbot_test_{}", epoch_ms()));
    let _ = std::fs::create_dir_all(&temp_dir);

    // Buat auth.json tiruan
    let auth_path = temp_dir.join("auth.json");
    let _ = std::fs::write(&auth_path, r#"{"google": {"apiKey": "test-key"}}"#);

    let res = handle_auth_status(&temp_dir);
    assert_eq!(res["type"], "pipkin_result");
    assert_eq!(res["ok"], true);

    let data = &res["data"];
    let builtin = data["builtin"].as_array().unwrap();
    assert_eq!(builtin.len(), 6);

    // Google harus configured = true
    let google_provider = builtin.iter().find(|p| p["id"] == "google").unwrap();
    assert_eq!(google_provider["configured"], true);
    assert_eq!(google_provider["type"], "api_key");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_base64_decode() {
    let text = "Halo OOPBoT dari native Termux!";
    let encoded = "SGFsbyBPT1BCb1QgZGFyaSBuYXRpdmUgVGVybXV4IQ==";
    let decoded = decode_base64(encoded).unwrap();
    assert_eq!(String::from_utf8_lossy(&decoded), text);
}

#[test]
fn test_save_attachments() {
    let temp_dir = std::env::temp_dir().join(format!("oopbot_attach_{}", epoch_ms()));
    let _ = std::fs::create_dir_all(&temp_dir);

    let params = json!({
        "cwd": "/some/cwd",
        "files": [
            {
                "name": "test.txt",
                "data": "SGFsbw=="
            }
        ]
    });

    let res = handle_save_attachments(&params, &temp_dir);
    assert_eq!(res["type"], "pipkin_result");
    assert_eq!(res["ok"], true);
    let paths = res["data"]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 1);
    let saved_path = paths[0].as_str().unwrap();
    assert!(Path::new(saved_path).exists());
    assert_eq!(std::fs::read_to_string(saved_path).unwrap(), "Halo");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_list_sessions_format() {
    let temp_dir = std::env::temp_dir().join(format!("oopbot_sess_{}", epoch_ms()));
    let sessions_dir = temp_dir.join("sessions").join("subfolder");
    let _ = std::fs::create_dir_all(&sessions_dir);

    let session_file = sessions_dir.join("test-session.jsonl");
    let header = json!({
        "type": "session",
        "id": "sess_123",
        "cwd": temp_dir.to_string_lossy(),
        "name": "Test Chat",
        "timestamp": 1700000000000u64,
    });
    let msg = json!({
        "type": "message",
        "message": {
            "role": "user",
            "content": "Pertanyaan pertama user"
        }
    });
    let content = format!("{}\n{}\n", header, msg);
    std::fs::write(&session_file, content).unwrap();

    let res = handle_list_sessions(&temp_dir, "/home");
    assert_eq!(res["type"], "pipkin_result");
    assert_eq!(res["ok"], true);
    let sessions = res["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "sess_123");
    assert_eq!(sessions[0]["name"], "Test Chat");
    assert_eq!(sessions[0]["firstMessage"], "Pertanyaan pertama user");
    assert_eq!(sessions[0]["messageCount"], 1);
    assert_eq!(sessions[0]["cwdExists"], true);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);
impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

#[tokio::test]
async fn test_rpc_stream_processing() {
    let _sink_guard = crate::rpc::output_sink_test_mutex().lock().await;
    let temp_dir = std::env::temp_dir().join(format!(
        "oopbot_host_rpc_test_{}_{}",
        std::process::id(),
        epoch_ms()
    ));
    let _ = std::fs::create_dir_all(&temp_dir);

    let raw = Arc::new(Mutex::new(Vec::new()));
    let output_buf = Arc::new(Mutex::new(Box::new(SharedBuf(raw.clone())) as Box<dyn Write + Send + Sync>));
    set_output_sink(Some(output_buf));

    let mut server = RpcServer::new_in_dir(temp_dir.clone(), None, None, vec![]).unwrap();
    let input = b"{\"type\":\"get_state\",\"id\":\"test_1\"}\n";
    let cursor = Cursor::new(input);

    let res = server.run_loop_with_input(cursor).await;
    assert!(res.is_ok());

    set_output_sink(None);
    let _ = std::fs::remove_dir_all(&temp_dir);

    let bytes = raw.lock().unwrap().clone();
    let written = String::from_utf8_lossy(&bytes);
    assert!(written.contains("test_1"));
    assert!(written.contains("\"success\":true"));
}

#[test]
fn test_encode_base64_and_roundtrip() {
    let original = "Hello World! OOPBoT Native FileHost 2026";
    let encoded = encode_base64(original.as_bytes());
    let decoded = decode_base64(&encoded).expect("decode failed");
    assert_eq!(String::from_utf8_lossy(&decoded), original);
}

#[test]
fn test_is_probably_text() {
    let text = b"This is plain text with \n and \t.";
    assert!(is_probably_text(text));

    let binary = [0u8, 1, 2, 255, 0, 15];
    assert!(!is_probably_text(&binary));
}

#[test]
fn test_resolve_browse_path() {
    let home = "/data/data/com.termux/files/home";
    assert_eq!(resolve_browse_path("~", home), PathBuf::from(home));
    assert_eq!(resolve_browse_path("~/projects", home), PathBuf::from(home).join("projects"));
    assert_eq!(resolve_browse_path("/sdcard/Download", home), PathBuf::from("/sdcard/Download"));
}

#[test]
fn test_search_folders_recursive() {
    let temp_dir = std::env::temp_dir().join(format!("oopbot_search_{}", epoch_ms()));
    let sub1 = temp_dir.join("my_awesome_project");
    let sub2 = temp_dir.join("other_folder").join("my_nested_project");
    let _ = std::fs::create_dir_all(&sub1);
    let _ = std::fs::create_dir_all(&sub2);

    let mut results = Vec::new();
    search_folders_recursive(&temp_dir, "project", 0, 4, 10, &mut results);
    assert_eq!(results.len(), 2);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_run_browse_loop() {
    let temp_dir = std::env::temp_dir().join(format!("oopbot_browse_{}", epoch_ms()));
    let sub = temp_dir.join("workspace_folder");
    let _ = std::fs::create_dir_all(&sub);

    let input = format!(
        "{{\"type\":\"pipkin_list\",\"id\":1,\"path\":\"{}\"}}\n",
        temp_dir.to_string_lossy()
    );
    let cursor = Cursor::new(input.into_bytes());
    let mut output = Vec::new();

    let res = run_browse_loop(&temp_dir.to_string_lossy(), cursor, &mut output);
    assert!(res.is_ok());

    let out_str = String::from_utf8_lossy(&output);
    assert!(out_str.contains("pipkin_places"));
    assert!(out_str.contains("pipkin_listing"));
    assert!(out_str.contains("workspace_folder"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_run_files_loop_list_and_read() {
    let temp_dir = std::env::temp_dir().join(format!("oopbot_files_{}", epoch_ms()));
    let file_path = temp_dir.join("hello.txt");
    let _ = std::fs::create_dir_all(&temp_dir);
    let _ = std::fs::write(&file_path, "Hello Native Files!");

    let input = format!(
        "{{\"type\":\"pipkin_files_list\",\"id\":10,\"root\":\"{}\",\"path\":\".\"}}\n\
         {{\"type\":\"pipkin_files_read\",\"id\":11,\"root\":\"{}\",\"path\":\"hello.txt\"}}\n\
         {{\"type\":\"pipkin_files_mkdir\",\"id\":12,\"root\":\"{}\",\"path\":\".\",\"name\":\"new_dir\"}}\n",
        temp_dir.to_string_lossy(),
        temp_dir.to_string_lossy(),
        temp_dir.to_string_lossy(),
    );
    let cursor = Cursor::new(input.into_bytes());
    let mut output = Vec::new();

    let res = run_files_loop(cursor, &mut output);
    assert!(res.is_ok());

    let out_str = String::from_utf8_lossy(&output);
    assert!(out_str.contains("pipkin_files_ready"));
    assert!(out_str.contains("pipkin_files_listing"));
    assert!(out_str.contains("hello.txt"));
    assert!(out_str.contains("pipkin_files_file"));
    assert!(out_str.contains("Hello Native Files!"));
    assert!(out_str.contains("pipkin_files_made"));
    assert!(out_str.contains("new_dir"));
    assert!(temp_dir.join("new_dir").is_dir());

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_sha256_hex() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"hello world"),
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn test_parse_git_diff_comprehensive() {
    let diff_sample = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 pub mod host;
-pub mod old;
+pub mod new_one;
+pub mod new_two;
diff --git a/new_file.txt b/new_file.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new_file.txt
@@ -0,0 +1,2 @@
+Line 1
+Line 2
diff --git a/deleted.txt b/deleted.txt
deleted file mode 100644
--- a/deleted.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-Bye
diff --git a/image.png b/image.png
Binary files a/image.png and b/image.png differ
";
    let files = parse_git_diff(diff_sample);
    assert_eq!(files.len(), 4);

    // 1. src/lib.rs (modified)
    assert_eq!(files[0]["path"], "src/lib.rs");
    assert_eq!(files[0]["change"], "modified");
    assert_eq!(files[0]["kind"], "text");
    assert_eq!(files[0]["additions"], 2);
    assert_eq!(files[0]["deletions"], 1);
    let hunks = files[0]["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0]["oldStart"], 1);
    assert_eq!(hunks[0]["newStart"], 1);

    // 2. new_file.txt (added)
    assert_eq!(files[1]["path"], "new_file.txt");
    assert_eq!(files[1]["change"], "added");
    assert_eq!(files[1]["kind"], "text");
    assert_eq!(files[1]["additions"], 2);
    assert_eq!(files[1]["deletions"], 0);

    // 3. deleted.txt (deleted)
    assert_eq!(files[2]["path"], "deleted.txt");
    assert_eq!(files[2]["change"], "deleted");
    assert_eq!(files[2]["additions"], 0);
    assert_eq!(files[2]["deletions"], 1);

    // 4. image.png (binary)
    assert_eq!(files[3]["path"], "image.png");
    assert_eq!(files[3]["kind"], "binary");
    assert_eq!(files[3]["hunks"].as_array().unwrap().len(), 0);
}

#[test]
fn test_run_git_loop_detect_and_changes() {
    let repo_root = env!("CARGO_MANIFEST_DIR");
    let input = format!(
        "{{\"type\":\"pipkin_git_detect\",\"id\":100,\"root\":\"{}\",\"trusted\":true}}\n\
         {{\"type\":\"oopbot_git_detect\",\"id\":101,\"root\":\"{}\",\"trusted\":true}}\n\
         {{\"type\":\"pipkin_git_changes\",\"id\":102,\"root\":\"{}\",\"mode\":\"uncommitted\",\"ignoreWhitespace\":false}}\n\
         {{\"type\":\"oopbot_git_changes\",\"id\":103,\"root\":\"{}\",\"mode\":\"uncommitted\",\"ignoreWhitespace\":false}}\n\
         {{\"type\":\"pipkin_git_commits\",\"id\":104,\"root\":\"{}\"}}\n\
         {{\"type\":\"pipkin_git_commit_diff\",\"id\":105,\"root\":\"{}\",\"sha\":\"0000000000000000000000000000000000000000\",\"ignoreWhitespace\":false}}\n",
        repo_root, repo_root, repo_root, repo_root, repo_root, repo_root
    );
    let cursor = Cursor::new(input.into_bytes());
    let mut output = Vec::new();

    let res = run_git_loop(cursor, &mut output);
    assert!(res.is_ok());

    let out_str = String::from_utf8_lossy(&output);
    assert!(out_str.contains("pipkin_git_ready"));
    assert!(out_str.contains("pipkin_git_detected"));
    assert!(out_str.contains("\"repo\":true"));
    assert!(out_str.contains("\"git\":true"));
    assert!(out_str.contains("pipkin_git_changes_result"));
    assert!(out_str.contains("fingerprint"));
    assert!(out_str.contains("pipkin_git_commits_result"));
    assert!(out_str.contains("pipkin_git_commit_diff_result"));
    assert!(out_str.contains("commit_not_found"));
}

#[test]
fn test_read_json_file_standard() {
    let temp_dir = std::env::temp_dir().join(format!("oopbot_test_{}", epoch_ms()));
    let oopbot_dir = temp_dir.join(".oopbot");
    let _ = std::fs::create_dir_all(&oopbot_dir);

    let oopbot_auth = oopbot_dir.join("auth.json");
    std::fs::write(&oopbot_auth, r#"{"test_provider":{"apiKey":"my-secret-456"}}"#).unwrap();
    let val = read_json_file(&oopbot_auth);
    assert_eq!(val["test_provider"]["apiKey"], "my-secret-456");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

fn spawn_mock_release_server(
    routes: Vec<(String, u16, Vec<u8>)>,
    max_requests: usize,
) -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits_clone = Arc::clone(&hits);

    let handle = std::thread::spawn(move || {
        use std::io::Read;
        for _ in 0..max_requests {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            let (status, body) = routes
                .iter()
                .find(|(p, _, _)| p == path)
                .map(|(_, s, b)| (*s, b.as_slice()))
                .unwrap_or((404, b"not found".as_slice()));
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let header = format!(
                "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status,
                reason,
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });
    (base_url, hits, handle)
}

fn write_fake_oopbot(path: &Path, version: &str) {
    let body = format!("#!/bin/sh\necho \"oopbot {}\"\n", version);
    atomic_write_binary(path, body.as_bytes()).unwrap();
}

#[tokio::test]
async fn test_ensure_oopbot_upgrades_stale_primary_and_secondary_from_release() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_upg_{}_{}", epoch_ms(), seq));
    let primary = temp_dir.join("usr/bin/oopbot");
    let secondary = temp_dir.join("root/bin/oopbot");
    let version_file = temp_dir.join("root/bin/oopbot.version");
    write_fake_oopbot(&primary, "0.5.2");
    write_fake_oopbot(&secondary, "0.5.2");
    std::fs::write(&version_file, "0.5.2\n").unwrap();

    let new_script = b"#!/bin/sh\necho \"oopbot 0.5.4\"\n".to_vec();
    let (base_url, hits, server) = spawn_mock_release_server(
        vec![("/v0.5.4/oopbot".to_string(), 200, new_script)],
        1,
    );
    let urls = vec![
        format!("{}/v0.5.4/oopbot", base_url),
        format!("{}/latest/oopbot", base_url),
    ];
    let mut logs = Vec::new();
    let res = ensure_oopbot_binary_with(
        "0.5.2",
        "0.5.4",
        false,
        &primary,
        std::slice::from_ref(&secondary),
        &urls,
        |line| logs.push(line.to_string()),
    )
    .await;

    server.join().unwrap();
    assert_eq!(res, Ok("0.5.4".to_string()));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));
    assert_eq!(read_oopbot_binary_version(&secondary).as_deref(), Some("0.5.4"));
    assert_eq!(std::fs::read_to_string(&version_file).unwrap().trim(), "0.5.4");
    assert!(!logs.is_empty());

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_ensure_oopbot_falls_back_on_404_and_rejects_version_mismatch() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_fb_{}_{}", epoch_ms(), seq));
    let primary = temp_dir.join("usr/bin/oopbot");
    write_fake_oopbot(&primary, "0.5.2");

    // 1. Primary tag 404 -> falls back to latest 200 with matching version
    let new_script = b"#!/bin/sh\necho \"oopbot 0.5.4\"\n".to_vec();
    let (base_url, hits, server) = spawn_mock_release_server(
        vec![("/latest/oopbot".to_string(), 200, new_script)],
        2,
    );
    let urls = vec![
        format!("{}/v0.5.4/oopbot", base_url),
        format!("{}/latest/oopbot", base_url),
    ];
    let res = ensure_oopbot_binary_with(
        "0.5.2",
        "0.5.4",
        false,
        &primary,
        &[],
        &urls,
        |_| {},
    )
    .await;
    server.join().unwrap();
    assert_eq!(res, Ok("0.5.4".to_string()));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));

    // 2. Mismatched version from server fails closed with upgrade_failed and keeps 0.5.4 intact
    let wrong_script = b"#!/bin/sh\necho \"oopbot 0.5.3\"\n".to_vec();
    let (bad_url, _bad_hits, bad_server) = spawn_mock_release_server(
        vec![("/v0.5.5/oopbot".to_string(), 200, wrong_script)],
        1,
    );
    let err = ensure_oopbot_binary_with(
        "0.5.4",
        "0.5.5",
        false,
        &primary,
        &[],
        &[format!("{}/v0.5.5/oopbot", bad_url)],
        |_| {},
    )
    .await
    .unwrap_err();
    bad_server.join().unwrap();
    assert_eq!(err.code, "upgrade_failed");
    assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));

    // 3. Unreachable endpoint reports npm_offline and keeps 0.5.4 intact
    let dead_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead_listener.local_addr().unwrap().port();
    drop(dead_listener);
    let offline_err = ensure_oopbot_binary_with(
        "0.5.4",
        "0.5.5",
        false,
        &primary,
        &[],
        &[format!("http://127.0.0.1:{}/v0.5.5/oopbot", dead_port)],
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(offline_err.code, "npm_offline");
    assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some("0.5.4"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_ensure_oopbot_skips_download_when_version_matches_and_syncs_secondary() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_skip_{}_{}", epoch_ms(), seq));
    let primary = temp_dir.join("usr/bin/oopbot");
    let secondary = temp_dir.join("root/bin/oopbot");
    write_fake_oopbot(&primary, "0.5.4");
    write_fake_oopbot(&secondary, "0.5.2");

    let (base_url, hits, server) = spawn_mock_release_server(vec![], 0);
    server.join().unwrap();
    let urls = vec![format!("{}/v0.5.4/oopbot", base_url)];
    let res = ensure_oopbot_binary_with(
        "0.5.4",
        "0.5.4",
        false,
        &primary,
        std::slice::from_ref(&secondary),
        &urls,
        |_| {},
    )
    .await;

    assert_eq!(res, Ok("0.5.4".to_string()));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(read_oopbot_binary_version(&secondary).as_deref(), Some("0.5.4"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_check_rpc_expected_version() {
    assert!(check_rpc_expected_version(&json!({}), "0.5.4").is_none());
    assert!(check_rpc_expected_version(&json!({ "expectedVersion": "0.5.4" }), "0.5.4").is_none());

    let refusal = check_rpc_expected_version(&json!({ "expectedVersion": "0.5.5" }), "0.5.4").unwrap();
    assert_eq!(refusal["type"], "pipkin_result");
    assert_eq!(refusal["ok"], false);
    assert_eq!(refusal["code"], "pi_version");
    assert_eq!(refusal["i18n"]["key"], "agent_version_mismatch");
}

#[test]
fn test_read_oopbot_binary_version_detail_diagnostics() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_ver_diag_{}_{}", epoch_ms(), seq));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let missing = temp_dir.join("missing");
    assert!(read_oopbot_binary_version_detail(&missing).unwrap_err().contains("metadata failed"));

    let empty = temp_dir.join("empty");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(
        read_oopbot_binary_version_detail(&empty).unwrap_err(),
        "binary is missing or empty"
    );

    let fail_script = temp_dir.join("fail");
    atomic_write_binary(&fail_script, b"#!/bin/sh\necho boom >&2\nexit 2\n").unwrap();
    let fail_err = read_oopbot_binary_version_detail(&fail_script).unwrap_err();
    assert!(fail_err.contains("exited with"));
    assert!(fail_err.contains("boom"));

    let bad_out = temp_dir.join("bad_out");
    atomic_write_binary(&bad_out, b"#!/bin/sh\necho not-oopbot\n").unwrap();
    let bad_err = read_oopbot_binary_version_detail(&bad_out).unwrap_err();
    assert!(bad_err.contains("unexpected --version output"));

    let busy_err = std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy);
    assert!(is_executable_file_busy(&busy_err));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_read_oopbot_binary_version_concurrent_fork_resilience() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_ver_fork_{}_{}", epoch_ms(), seq));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);
    let forker = std::thread::spawn(move || {
        while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = std::process::Command::new("sh").arg("-c").arg(":").status();
        }
    });

    for i in 0..25 {
        let bin = temp_dir.join(format!("oopbot_{}", i));
        write_fake_oopbot(&bin, "0.5.4");
        let ver = read_oopbot_binary_version_detail(&bin).unwrap();
        assert_eq!(ver, "0.5.4");
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    forker.join().unwrap();
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_pkg_version_055_rejects_054_rpc_and_upgrades_054_binary() {
    let pkg_ver = env!("CARGO_PKG_VERSION");
    assert_eq!(pkg_ver, "0.5.5");

    let refusal = check_rpc_expected_version(&json!({ "expectedVersion": pkg_ver }), "0.5.4").unwrap();
    assert_eq!(refusal["ok"], false);
    assert_eq!(refusal["code"], "pi_version");

    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_ensure_055_{}_{}", epoch_ms(), seq));
    let primary = temp_dir.join("usr/bin/oopbot");
    let secondary = temp_dir.join("root/bin/oopbot");
    let version_file = temp_dir.join("root/bin/oopbot.version");
    write_fake_oopbot(&primary, "0.5.4");
    write_fake_oopbot(&secondary, "0.5.4");
    std::fs::write(&version_file, "0.5.4\n").unwrap();

    let new_script = format!("#!/bin/sh\necho \"oopbot {}\"\n", pkg_ver).into_bytes();
    let (base_url, hits, server) = spawn_mock_release_server(
        vec![(format!("/v{}/oopbot", pkg_ver), 200, new_script)],
        1,
    );
    let urls = vec![format!("{}/v{}/oopbot", base_url, pkg_ver)];
    let res = ensure_oopbot_binary_with(
        "0.5.4",
        pkg_ver,
        false,
        &primary,
        std::slice::from_ref(&secondary),
        &urls,
        |_| {},
    )
    .await;
    server.join().unwrap();

    assert_eq!(res, Ok(pkg_ver.to_string()));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(read_oopbot_binary_version(&primary).as_deref(), Some(pkg_ver));
    assert_eq!(read_oopbot_binary_version(&secondary).as_deref(), Some(pkg_ver));
    assert_eq!(std::fs::read_to_string(&version_file).unwrap().trim(), pkg_ver);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn reg005_auth_status_oauth_unrestricted_scope_and_corrupt_json_errors() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_auth_reg005_{}_{}", epoch_ms(), seq));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // 1. OAuth credentials for anthropic, unrestricted empty enabledModels, no OOPBOT_API_KEY leak
    std::fs::write(
        temp_dir.join("auth.json"),
        json!({
            "anthropic": { "type": "oauth", "access": "oauth-access-token-123" },
            "openrouter": { "type": "api_key", "apiKey": "sk-or-1" }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        temp_dir.join("settings.json"),
        json!({ "enabledModels": [] }).to_string(),
    )
    .unwrap();

    let res = handle_auth_status(&temp_dir);
    assert_eq!(res["ok"], true);
    assert!(res["data"]["scope"].is_null(), "empty enabledModels must emit null scope");
    let builtin = res["data"]["builtin"].as_array().unwrap();
    let anthropic = builtin.iter().find(|p| p["id"] == "anthropic").unwrap();
    assert_eq!(anthropic["configured"], true);
    assert_eq!(anthropic["type"], "oauth");
    assert!(anthropic["enabled"].is_null(), "unrestricted scope must leave builtin[].enabled null");

    let zai = builtin.iter().find(|p| p["id"] == "zai").unwrap();
    assert_eq!(zai["configured"], false);
    assert!(zai["type"].is_null());

    // 2. Non-empty glob pattern in enabledModels activates scope + builtin[].enabled arrays
    std::fs::write(
        temp_dir.join("settings.json"),
        json!({ "enabledModels": ["anthropic/claude-*"] }).to_string(),
    )
    .unwrap();
    let scoped_res = handle_auth_status(&temp_dir);
    let scoped_builtin = scoped_res["data"]["builtin"].as_array().unwrap();
    let scoped_anthropic = scoped_builtin.iter().find(|p| p["id"] == "anthropic").unwrap();
    assert!(!scoped_anthropic["enabled"].as_array().unwrap().is_empty());
    let scoped_google = scoped_builtin.iter().find(|p| p["id"] == "google").unwrap();
    assert_eq!(scoped_google["enabled"].as_array().unwrap().len(), 0);

    // 3. Corrupt JSON surfaces modelsError & settingsError instead of silent empty state
    std::fs::write(temp_dir.join("models.json"), "{bad json").unwrap();
    std::fs::write(temp_dir.join("settings.json"), "not-an-object").unwrap();
    let err_res = handle_auth_status(&temp_dir);
    assert!(err_res["data"]["modelsError"].as_str().unwrap().contains("Invalid JSON"));
    assert!(err_res["data"]["settingsError"].as_str().unwrap().contains("Invalid JSON"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn reg008_local_models_sync_uses_pipkin_local_only_compaction_and_changed_flag() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_local_reg008_{}_{}", epoch_ms(), seq));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Seed legacy oopbot-local so we verify cleanup
    std::fs::write(
        temp_dir.join("models.json"),
        json!({ "providers": { "oopbot-local": { "baseUrl": "http://127.0.0.1:1/v1", "models": [] } } }).to_string(),
    )
    .unwrap();

    let params = json!({
        "baseUrl": "http://127.0.0.1:11434/v1",
        "apiKey": "no-api-key",
        "models": [
            { "id": "qwen2.5-7b", "name": "Qwen 2.5", "contextWindow": 16384, "maxTokens": 2048 }
        ]
    });

    let first = handle_local_models_sync(&temp_dir, &params);
    assert_eq!(first["ok"], true);
    assert_eq!(first["data"]["changed"], true);

    let models_doc = read_json_file(&temp_dir.join("models.json"));
    assert!(models_doc["providers"].get("pipkin-local").is_some());
    assert!(models_doc["providers"].get("oopbot-local").is_none());
    assert_eq!(
        models_doc["providers"]["pipkin-local"]["compat"]["sendSessionAffinityHeaders"],
        true
    );
    assert_eq!(
        models_doc["providers"]["pipkin-local"]["models"][0]["cost"]["input"],
        0
    );

    let settings_doc = read_json_file(&temp_dir.join("settings.json"));
    let ov = &settings_doc["compaction"]["modelOverrides"]["pipkin-local/qwen2.5-7b"];
    assert_eq!(ov["reserveTokens"], 2048 + 4096);
    assert_eq!(ov["keepRecentTokens"], 16384 / 16);

    // Repeat with identical params must report changed: false
    let second = handle_local_models_sync(&temp_dir, &params);
    assert_eq!(second["ok"], true);
    assert_eq!(second["data"]["changed"], false);

    // Empty list removes provider and compaction override
    let cleared = handle_local_models_sync(
        &temp_dir,
        &json!({ "baseUrl": "http://127.0.0.1:11434/v1", "apiKey": "", "models": [] }),
    );
    assert_eq!(cleared["data"]["changed"], true);
    let models_after = read_json_file(&temp_dir.join("models.json"));
    assert!(models_after["providers"].get("pipkin-local").is_none());
    let settings_after = read_json_file(&temp_dir.join("settings.json"));
    assert!(settings_after["compaction"].get("modelOverrides").is_none());

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn reg009_list_sessions_parses_iso_timestamp_tracks_renames_and_subagents() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_sess_reg009_{}_{}", epoch_ms(), seq));
    let sess_dir = temp_dir.join("sessions").join("proj");
    std::fs::create_dir_all(sess_dir.join("subagents").join("s-iso")).unwrap();

    let main_file = sess_dir.join("chat.jsonl");
    let lines = [
        json!({
            "type": "session",
            "id": "s-iso",
            "cwd": temp_dir.to_string_lossy(),
            "timestamp": "2026-10-15T14:30:00.500Z",
            "name": "Initial Name"
        })
        .to_string(),
        json!({
            "type": "session_info",
            "name": "Renamed Session Title"
        })
        .to_string(),
        json!({
            "type": "message",
            "message": { "role": "user", "content": "Hello" }
        })
        .to_string(),
    ]
    .join("\n");
    std::fs::write(&main_file, format!("{}\n", lines)).unwrap();

    // Create a subagent transcript under sessions/proj/subagents/s-iso/ (must NOT be listed as top-level session!)
    std::fs::write(
        sess_dir.join("subagents").join("s-iso").join("child1.jsonl"),
        json!({ "type": "session", "id": "sub-1", "cwd": temp_dir.to_string_lossy() }).to_string(),
    )
    .unwrap();

    let res = handle_list_sessions(&temp_dir, "/home");
    let sessions = res["data"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "s-iso");
    assert_eq!(sessions[0]["name"], "Renamed Session Title");
    assert_eq!(sessions[0]["subagents"], 1);
    // 2026-10-15T14:30:00.500Z in epoch ms (1792074600500)
    assert_eq!(
        sessions[0]["created"],
        parse_iso_to_epoch_ms("2026-10-15T14:30:00.500Z").unwrap()
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn reg010_save_attachments_and_files_containment_reject_traversal() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("oopbot_sec_reg010_{}_{}", epoch_ms(), seq));
    let workspace = temp_dir.join("ws");
    let outside = temp_dir.join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "top-secret").unwrap();

    // 1. Attachments validation & safe filename + deduplication
    let missing = handle_save_attachments(&json!({}), &temp_dir);
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["code"], "bad_params");

    let bad_b64 = handle_save_attachments(
        &json!({ "files": [{ "name": "a.txt", "data": "!!!" }] }),
        &temp_dir,
    );
    assert_eq!(bad_b64["ok"], false);
    assert_eq!(bad_b64["code"], "bad_params");

    let saved = handle_save_attachments(
        &json!({
            "files": [
                { "name": "../../.hidden.txt", "data": "SGVsbG8=" },
                { "name": "..\\..\\hidden.txt", "data": "V29ybGQ=" }
            ]
        }),
        &temp_dir,
    );
    assert_eq!(saved["ok"], true);
    let paths = saved["data"]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    let p0 = PathBuf::from(paths[0].as_str().unwrap());
    let p1 = PathBuf::from(paths[1].as_str().unwrap());
    assert_eq!(p0.file_name().unwrap(), "hidden.txt");
    assert_eq!(p1.file_name().unwrap(), "hidden-2.txt");
    assert!(p0.starts_with(std::fs::canonicalize(temp_dir.join("attachments")).unwrap()));

    // 2. Files loop blocks .. traversal and escaping symlinks
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("escape_link.txt"));

    let ws_str = workspace.to_string_lossy();
    let reqs = format!(
        "{{\"type\":\"pipkin_files_read\",\"id\":1,\"root\":\"{}\",\"path\":\"../outside/secret.txt\"}}\n\
         {{\"type\":\"pipkin_files_read\",\"id\":2,\"root\":\"{}\",\"path\":\"escape_link.txt\"}}\n\
         {{\"type\":\"pipkin_files_list\",\"id\":3,\"root\":\"{}\",\"path\":\".\"}}\n\
         {{\"type\":\"pipkin_files_mkdir\",\"id\":4,\"root\":\"{}\",\"path\":\"..\",\"name\":\"bad\"}}\n",
        ws_str, ws_str, ws_str, ws_str
    );
    let mut out = Vec::new();
    run_files_loop(Cursor::new(reqs.into_bytes()), &mut out).unwrap();
    let out_str = String::from_utf8_lossy(&out);
    let lines: Vec<Value> = out_str
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();

    let r1 = lines.iter().find(|v| v["id"] == 1).unwrap();
    assert_eq!(r1["errorCode"], "outside_workspace");

    #[cfg(unix)]
    {
        let r2 = lines.iter().find(|v| v["id"] == 2).unwrap();
        assert_eq!(r2["errorCode"], "outside_workspace");
        let r3 = lines.iter().find(|v| v["id"] == 3).unwrap();
        assert_eq!(r3["entries"].as_array().unwrap().len(), 0);
    }

    let r4 = lines.iter().find(|v| v["id"] == 4).unwrap();
    assert_eq!(r4["errorCode"], "outside_workspace");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn reg004_backups_create_list_restore_delete_and_fail_closed_on_corrupt_or_bad_name() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root_dir =
        std::env::temp_dir().join(format!("oopbot_bkp_reg004_{}_{}", epoch_ms(), seq));
    let config_dir = root_dir.join("agent");
    std::fs::create_dir_all(config_dir.join("sessions/proj")).unwrap();
    std::fs::write(config_dir.join("settings.json"), r#"{"defaultProvider":"google"}"#).unwrap();
    std::fs::write(config_dir.join("sessions/proj/s1.jsonl"), "live-session").unwrap();
    std::fs::write(config_dir.join("pi-debug.log"), "keep-me-log").unwrap();

    let mut progress_msgs = Vec::new();
    let created = handle_backup_create(&config_dir, &mut |line, _key, _count| {
        progress_msgs.push(line.to_string());
    });
    assert_eq!(created["ok"], true);
    let backup = &created["data"]["backup"];
    let bkp_name = backup["name"].as_str().unwrap().to_string();
    assert!(bkp_name.starts_with("pipkin-config-"));
    assert!(bkp_name.ends_with(".tar.gz"));
    assert!(backup["size"].as_u64().unwrap() > 0);
    assert!(backup["error"].is_null());

    let listed = handle_list_backups(&config_dir);
    assert_eq!(listed["ok"], true);
    let arr = listed["data"]["backups"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], bkp_name);

    // Mutate settings and verify restore restores settings while keeping sessions/logs
    std::fs::write(config_dir.join("settings.json"), r#"{"defaultProvider":"corrupted"}"#).unwrap();
    std::fs::write(config_dir.join("extra.txt"), "remove-on-restore").unwrap();
    let restored = handle_backup_restore(
        &config_dir,
        &json!({ "name": bkp_name }),
        &mut |_, _, _| {},
    );
    assert_eq!(restored["ok"], true);
    assert_eq!(
        std::fs::read_to_string(config_dir.join("settings.json")).unwrap(),
        r#"{"defaultProvider":"google"}"#
    );
    assert!(!config_dir.join("extra.txt").exists());
    assert_eq!(
        std::fs::read_to_string(config_dir.join("sessions/proj/s1.jsonl")).unwrap(),
        "live-session"
    );
    assert_eq!(
        std::fs::read_to_string(config_dir.join("pi-debug.log")).unwrap(),
        "keep-me-log"
    );

    // Corrupt archive is listed with error and rejected on restore
    let bkp_dir = backups_dir(&config_dir);
    std::fs::write(bkp_dir.join("corrupt.tar.gz"), b"not-a-gzip").unwrap();
    let listed_with_bad = handle_list_backups(&config_dir);
    let bad_entry = listed_with_bad["data"]["backups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "corrupt.tar.gz")
        .unwrap();
    assert!(bad_entry["error"].as_str().is_some());

    let restore_corrupt =
        handle_backup_restore(&config_dir, &json!({ "name": "corrupt.tar.gz" }), &mut |_, _, _| {});
    assert_eq!(restore_corrupt["ok"], false);
    assert_eq!(restore_corrupt["code"], "bad_backup");

    // Unsafe name rejected
    let bad_del = handle_backup_delete(&config_dir, &json!({ "name": "../evil.tar.gz" }));
    assert_eq!(bad_del["ok"], false);
    assert_eq!(bad_del["code"], "bad_name");

    let ok_del = handle_backup_delete(&config_dir, &json!({ "name": bkp_name }));
    assert_eq!(ok_del["ok"], true);
    assert!(!bkp_dir.join(&bkp_name).exists());

    let _ = std::fs::remove_dir_all(&root_dir);
}

#[test]
fn reg004_subagent_sessions_reads_pipkin_subagent_transcripts_and_ignores_unrelated() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let config_dir =
        std::env::temp_dir().join(format!("oopbot_sub_reg004_{}_{}", epoch_ms(), seq));
    let proj_dir = config_dir.join("sessions/proj");
    std::fs::create_dir_all(proj_dir.join("subagents/parent1")).unwrap();

    let parent_file = proj_dir.join("parent.jsonl");
    std::fs::write(
        &parent_file,
        json!({ "type": "session", "id": "parent1", "timestamp": "2026-10-15T10:00:00.000Z" })
            .to_string(),
    )
    .unwrap();

    let sub_file = proj_dir.join("subagents/parent1/run1.jsonl");
    let sub_lines = [
        json!({ "type": "session", "id": "sub_a", "timestamp": "2026-10-15T10:01:00.000Z" })
            .to_string(),
        json!({
            "type": "custom",
            "customType": "pipkin-subagent",
            "data": { "toolCallId": "call_1", "agent": "research", "description": "Audit files" }
        })
        .to_string(),
        json!({ "type": "message", "message": { "role": "user", "content": "hi" } }).to_string(),
    ]
    .join("\n");
    std::fs::write(&sub_file, format!("{}\n", sub_lines)).unwrap();
    // Unrelated file without pipkin-subagent link must be ignored
    std::fs::write(
        proj_dir.join("subagents/parent1/orphan.jsonl"),
        json!({ "type": "session", "id": "sub_b" }).to_string(),
    )
    .unwrap();

    let res = handle_subagent_sessions(
        &config_dir,
        &json!({ "path": parent_file.to_string_lossy() }),
    );
    assert_eq!(res["ok"], true);
    let list = res["data"]["sessions"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["toolCallId"], "call_1");
    assert_eq!(list[0]["agent"], "research");
    assert_eq!(list[0]["description"], "Audit files");
    assert_eq!(list[0]["messageCount"], 1);
    assert_eq!(
        list[0]["created"],
        parse_iso_to_epoch_ms("2026-10-15T10:01:00.000Z").unwrap()
    );

    let _ = std::fs::remove_dir_all(&config_dir);
}

#[test]
fn reg004_remote_hosts_save_workspace_remove_and_fail_closed() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root_dir =
        std::env::temp_dir().join(format!("oopbot_rem_reg004_{}_{}", epoch_ms(), seq));
    let config_dir = root_dir.join("agent");
    let home_dir = root_dir.join("home");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&home_dir).unwrap();
    let home_str = home_dir.to_string_lossy().to_string();

    // Validation error maps to remote_host_invalid + field
    let invalid = handle_remote_host_save(
        &config_dir,
        &home_str,
        &json!({ "host": { "name": "", "auth": "key", "address": "u@h", "port": 22 } }),
    );
    assert_eq!(invalid["ok"], false);
    assert_eq!(invalid["code"], "remote_host_invalid");
    assert_eq!(invalid["i18n"]["args"]["field"], "name");

    // Save valid host -> returns host.id & writes hosts.json + ssh/config
    let saved = handle_remote_host_save(
        &config_dir,
        &home_str,
        &json!({ "host": { "name": "Dev Box", "auth": "key", "address": "dev@example.com", "port": 2222 } }),
    );
    assert_eq!(saved["ok"], true);
    let host_id = saved["data"]["host"]["id"].as_str().unwrap().to_string();
    assert_eq!(host_id.len(), 12);

    // Create remote workspace stand-in
    let ws = handle_remote_workspace(
        &config_dir,
        &json!({ "hostId": host_id, "path": "/home/dev/proj/" }),
    );
    assert_eq!(ws["ok"], true);
    let cwd = ws["data"]["cwd"].as_str().unwrap().to_string();
    assert!(Path::new(&cwd).join("remote.json").is_file());

    // Remote stand-in rejects silent local attachment write
    let att_err = handle_save_attachments(
        &json!({
            "cwd": cwd,
            "files": [{ "name": "a.txt", "data": "SGVsbG8=" }]
        }),
        &config_dir,
    );
    assert_eq!(att_err["ok"], false);
    assert_eq!(att_err["code"], "remote_not_connected");

    let listed = handle_remote_hosts(&config_dir, &home_str);
    assert_eq!(listed["data"]["hosts"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"]["workspaces"].as_array().unwrap().len(), 1);

    let status = handle_remote_status(&config_dir, &json!({ "ids": [&host_id] }));
    assert_eq!(status["data"]["connected"][&host_id], false);

    // Removing host retains workspace stand-ins for archived chats
    let removed = handle_remote_host_remove(&config_dir, &home_str, &json!({ "id": host_id }));
    assert_eq!(removed["ok"], true);
    let listed_after = handle_remote_hosts(&config_dir, &home_str);
    assert_eq!(listed_after["data"]["hosts"].as_array().unwrap().len(), 0);
    assert_eq!(listed_after["data"]["workspaces"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&root_dir);
}

#[test]
fn reg007_terminal_pty_echo_and_resize_without_tmp_hardcode() {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_dir =
        std::env::temp_dir().join(format!("oopbot_term_reg007_{}_{}", epoch_ms(), seq));
    let prefix_bin = temp_dir.join("usr/bin");
    std::fs::create_dir_all(&prefix_bin).unwrap();
    std::fs::write(prefix_bin.join("bash"), b"#!/bin/sh\n").unwrap();

    let (resolved, args) = resolve_terminal_shell_with(
        None,
        Some(&temp_dir.join("usr").to_string_lossy()),
    );
    assert!(resolved.ends_with("/usr/bin/bash"));
    assert_eq!(args, vec!["-l"]);

    #[cfg(unix)]
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cwd_str = temp_dir.to_string_lossy().to_string();

        let client = std::net::TcpStream::connect(addr).unwrap();
        let (server_stream, _) = listener.accept().unwrap();

        let srv_read = std::io::BufReader::new(server_stream.try_clone().unwrap());
        let srv_write = std::io::BufWriter::new(server_stream);

        let server_handle = std::thread::spawn(move || {
            run_terminal_loop(
                json!({ "cwd": cwd_str, "rows": 24, "cols": 80 }),
                srv_read,
                srv_write,
            )
            .unwrap();
        });

        let mut cli_read = std::io::BufReader::new(client.try_clone().unwrap());
        let mut cli_write = client;

        let cmd_b64 = encode_base64(b"stty size; printf 'PTY_OK\\n'; exit\n");
        let resize_msg = json!({ "type": "resize", "rows": 42, "cols": 111 }).to_string();
        let input_msg = json!({ "type": "input", "data": cmd_b64 }).to_string();
        writeln!(cli_write, "{}", resize_msg).unwrap();
        writeln!(cli_write, "{}", input_msg).unwrap();
        cli_write.flush().unwrap();

        let mut captured = String::new();
        let mut saw_exit = false;
        let mut line = String::new();
        while let Ok(n) = std::io::BufRead::read_line(&mut cli_read, &mut line) {
            if n == 0 {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                match v["type"].as_str() {
                    Some("output") => {
                        if let Some(d) = v["data"].as_str().and_then(decode_base64) {
                            captured.push_str(&String::from_utf8_lossy(&d));
                        }
                    }
                    Some("pipkin_exit") => {
                        saw_exit = true;
                        break;
                    }
                    _ => {}
                }
            }
            line.clear();
        }

        drop(cli_write);
        server_handle.join().unwrap();
        assert!(saw_exit, "terminal must emit pipkin_exit");
        assert!(captured.contains("PTY_OK"), "PTY output missed marker: {}", captured);
        assert!(captured.contains("42 111"), "PTY TIOCSWINSZ resize did not propagate: {}", captured);
    }

    let _ = std::fs::remove_dir_all(&temp_dir);
}
