use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static WIRE_TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempWireEnv {
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    prefix: PathBuf,
    workspace: PathBuf,
}

impl TempWireEnv {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = WIRE_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "oopbot_wire_{}_{}_{}_{}",
            tag,
            std::process::id(),
            nanos,
            seq
        ));
        let home = root.join("home");
        let config = root.join("config");
        let prefix = root.join("prefix");
        let workspace = root.join("workspace");
        for dir in [&home, &config, &prefix.join("bin"), &workspace] {
            std::fs::create_dir_all(dir).expect("create isolated wire test dir");
        }
        Self {
            root,
            home,
            config,
            prefix,
            workspace,
        }
    }
}

impl Drop for TempWireEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct ChildGuard(Child);

impl ChildGuard {
    fn wait_clean(mut self) -> std::process::ExitStatus {
        let status = self.0.wait().expect("wait child");
        std::mem::forget(self);
        status
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_oopbot")
}

fn spawn_host(
    env_dir: &TempWireEnv,
    mode: &str,
    port: u16,
    token: &str,
) -> ChildGuard {
    let mut cmd = Command::new(bin_path());
    cmd.args(["host", mode, &port.to_string()])
        .current_dir(&env_dir.workspace)
        .env("HOME", &env_dir.home)
        .env("OOPBOT_CONFIG_DIR", &env_dir.config)
        .env("PI_CODING_AGENT_DIR", &env_dir.config)
        .env("PREFIX", &env_dir.prefix)
        .env("OOPBOT_TOKEN", token)
        .env_remove("PIPKIN_TOKEN")
        .env_remove("OOPBOT_API_KEY")
        .env_remove("PIPKIN_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn compiled oopbot host binary");
    ChildGuard(child)
}

fn accept_wire(
    listener: &TcpListener,
    expected_mode: &str,
    expected_token: &str,
) -> (Value, BufReader<TcpStream>, BufWriter<TcpStream>) {
    listener.set_nonblocking(false).ok();
    let (stream, _) = listener.accept().expect("accept TCP connection from oopbot host");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set write timeout");
    let write_stream = stream.try_clone().expect("clone TcpStream");
    let mut reader = BufReader::new(stream);
    let writer = BufWriter::new(write_stream);

    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("read pipkin_hello");
    assert!(n > 0, "expected pipkin_hello line");
    let hello: Value = serde_json::from_str(line.trim()).expect("parse pipkin_hello JSON");
    assert_eq!(hello["type"], "pipkin_hello");
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["mode"], expected_mode);
    assert_eq!(hello["token"], expected_token);
    assert_eq!(hello["oopbotVersion"], env!("CARGO_PKG_VERSION"));
    (hello, reader, writer)
}

fn write_json_line(writer: &mut BufWriter<TcpStream>, val: &Value) {
    writeln!(writer, "{}", serde_json::to_string(val).unwrap()).expect("write JSON line");
    writer.flush().expect("flush JSON line");
}

fn read_json_line(reader: &mut BufReader<TcpStream>) -> Value {
    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("read JSON line");
    assert!(n > 0, "unexpected EOF while reading JSONL wire line");
    serde_json::from_str(line.trim()).expect("parse JSONL wire line")
}

fn install_stub_oopbot(target: &Path, version: &str) {
    let script = format!("#!/bin/sh\necho \"oopbot {}\"\n", version);
    std::fs::write(target, script).expect("write stub oopbot");
    let mut perms = std::fs::metadata(target).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(target, perms).unwrap();
}

#[test]
fn reg012_binary_wire_ensure_and_unsupported_modes() {
    let env_dir = TempWireEnv::new("ensure");
    let stub = env_dir.prefix.join("bin").join("oopbot");
    install_stub_oopbot(&stub, env!("CARGO_PKG_VERSION"));

    // 1. ensure mode matching expectedVersion
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let child = spawn_host(&env_dir, "ensure", port, "tok-ensure");
    let (_hello, mut reader, mut writer) = accept_wire(&listener, "ensure", "tok-ensure");

    write_json_line(
        &mut writer,
        &json!({
            "type": "pipkin_request",
            "params": { "expectedVersion": env!("CARGO_PKG_VERSION") }
        }),
    );

    let mut result_msg = Value::Null;
    for _ in 0..8 {
        let msg = read_json_line(&mut reader);
        if msg["type"] == "pipkin_result" {
            result_msg = msg;
            break;
        }
    }
    assert_eq!(result_msg["ok"], true, "ensure result: {}", result_msg);
    assert_eq!(result_msg["data"]["oopbotVersion"], env!("CARGO_PKG_VERSION"));
    drop(writer);
    drop(reader);
    assert!(child.wait_clean().success());

    // 2. unsupported package_install mode must fail closed with code = "unsupported"
    let listener2 = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port2 = listener2.local_addr().unwrap().port();
    let child2 = spawn_host(&env_dir, "package_install", port2, "tok-pkg");
    let (_hello2, mut reader2, mut writer2) = accept_wire(&listener2, "package_install", "tok-pkg");
    write_json_line(&mut writer2, &json!({ "type": "pipkin_request", "params": {} }));
    let res2 = read_json_line(&mut reader2);
    assert_eq!(res2["type"], "pipkin_result");
    assert_eq!(res2["ok"], false);
    assert_eq!(res2["code"], "unsupported");
    drop(writer2);
    drop(reader2);
    assert!(child2.wait_clean().success());
}

#[test]
fn reg012_binary_wire_utility_auth_backups_and_unknown_op() {
    let env_dir = TempWireEnv::new("utility");
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let child = spawn_host(&env_dir, "utility", port, "tok-util");

    let (_hello, mut reader, mut writer) = accept_wire(&listener, "utility", "tok-util");
    write_json_line(&mut writer, &json!({ "type": "pipkin_request", "params": {} }));

    let ready = read_json_line(&mut reader);
    assert_eq!(ready["type"], "pipkin_utility_ready");

    // 1. auth_status on clean config
    write_json_line(
        &mut writer,
        &json!({ "type": "pipkin_utility_request", "id": 1, "op": "auth_status", "params": {} }),
    );
    let auth_res = read_json_line(&mut reader);
    assert_eq!(auth_res["id"], 1);
    assert_eq!(auth_res["ok"], true);
    assert!(auth_res["data"]["builtin"].is_array());

    // 2. backups on clean config -> empty list
    write_json_line(
        &mut writer,
        &json!({ "type": "pipkin_utility_request", "id": 2, "op": "backups", "params": {} }),
    );
    let backups_res = read_json_line(&mut reader);
    assert_eq!(backups_res["id"], 2);
    assert_eq!(backups_res["ok"], true);
    assert_eq!(backups_res["data"]["backups"], json!([]));

    // 3. unknown_op must fail closed
    write_json_line(
        &mut writer,
        &json!({ "type": "pipkin_utility_request", "id": 3, "op": "nonexistent_op", "params": {} }),
    );
    let unk_res = read_json_line(&mut reader);
    assert_eq!(unk_res["id"], 3);
    assert_eq!(unk_res["ok"], false);
    assert_eq!(unk_res["code"], "unknown_op");

    writer.get_ref().shutdown(Shutdown::Write).unwrap();
    drop(writer);
    drop(reader);
    assert!(child.wait_clean().success());
}

#[test]
fn reg012_binary_wire_files_and_git_handshakes() {
    let env_dir = TempWireEnv::new("files_git");
    std::fs::write(env_dir.workspace.join("hello.txt"), "wire-file-body").unwrap();
    let root_str = env_dir.workspace.to_string_lossy().to_string();

    // 1. files mode
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let child = spawn_host(&env_dir, "files", port, "tok-files");
    let (_hello, mut reader, mut writer) = accept_wire(&listener, "files", "tok-files");
    write_json_line(&mut writer, &json!({ "type": "pipkin_request", "params": {} }));

    let ready = read_json_line(&mut reader);
    assert_eq!(ready["type"], "pipkin_files_ready");

    write_json_line(
        &mut writer,
        &json!({ "type": "pipkin_files_list", "id": 10, "root": root_str, "path": "." }),
    );
    let listing = read_json_line(&mut reader);
    assert_eq!(listing["type"], "pipkin_files_listing");
    assert_eq!(listing["id"], 10);
    assert!(listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "hello.txt"));

    write_json_line(
        &mut writer,
        &json!({ "type": "pipkin_files_read", "id": 11, "root": root_str, "path": "hello.txt" }),
    );
    let read_res = read_json_line(&mut reader);
    assert_eq!(read_res["type"], "pipkin_files_file");
    assert_eq!(read_res["id"], 11);
    assert_eq!(read_res["kind"], "text");
    assert_eq!(read_res["content"], "wire-file-body");

    write_json_line(
        &mut writer,
        &json!({ "type": "pipkin_files_read", "id": 12, "root": root_str, "path": "../../etc/passwd" }),
    );
    let bad_res = read_json_line(&mut reader);
    assert_eq!(bad_res["type"], "pipkin_files_file");
    assert_eq!(bad_res["id"], 12);
    assert_eq!(bad_res["errorCode"], "outside_workspace");

    writer.get_ref().shutdown(Shutdown::Write).unwrap();
    drop(writer);
    drop(reader);
    assert!(child.wait_clean().success());

    // 2. git mode
    let listener_git = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port_git = listener_git.local_addr().unwrap().port();
    let child_git = spawn_host(&env_dir, "git", port_git, "tok-git");
    let (_hello_g, mut reader_g, mut writer_g) = accept_wire(&listener_git, "git", "tok-git");
    write_json_line(&mut writer_g, &json!({ "type": "pipkin_request", "params": {} }));

    let ready_g = read_json_line(&mut reader_g);
    assert_eq!(ready_g["type"], "pipkin_git_ready");

    write_json_line(
        &mut writer_g,
        &json!({ "type": "pipkin_git_detect", "id": 20, "root": root_str, "trusted": true }),
    );
    let det = read_json_line(&mut reader_g);
    assert_eq!(det["type"], "pipkin_git_detected");
    assert_eq!(det["id"], 20);
    assert!(det.get("git").is_some());

    writer_g.get_ref().shutdown(Shutdown::Write).unwrap();
    drop(writer_g);
    drop(reader_g);
    assert!(child_git.wait_clean().success());
}

#[test]
fn reg012_binary_wire_rpc_get_state_abort_and_clean_exit() {
    let env_dir = TempWireEnv::new("rpc");
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let child = spawn_host(&env_dir, "rpc", port, "tok-rpc");

    let (_hello, mut reader, mut writer) = accept_wire(&listener, "rpc", "tok-rpc");
    write_json_line(
        &mut writer,
        &json!({
            "type": "pipkin_request",
            "params": {
                "cwd": env_dir.workspace.to_string_lossy(),
                "pipkinExtensions": [{ "name": "pipkin-todo" }]
            }
        }),
    );

    // Wait for pipkin:hello channel banner emitted by RpcServer::run_loop
    let mut saw_hello = false;
    for _ in 0..8 {
        let msg = read_json_line(&mut reader);
        if msg["type"] == "pipkin_channel" && msg["topic"] == "pipkin:hello" {
            saw_hello = true;
            assert_eq!(msg["data"]["oopbotVersion"], env!("CARGO_PKG_VERSION"));
            break;
        }
    }
    assert!(saw_hello, "expected pipkin:hello banner in RPC mode");

    write_json_line(
        &mut writer,
        &json!({ "id": "wire_state_1", "type": "get_state" }),
    );
    let mut state_resp = Value::Null;
    for _ in 0..8 {
        let msg = read_json_line(&mut reader);
        if msg["type"] == "response" && msg["id"] == "wire_state_1" {
            state_resp = msg;
            break;
        }
    }
    assert_eq!(state_resp["success"], true);
    assert_eq!(state_resp["data"]["isStreaming"], false);
    assert_eq!(state_resp["data"]["isCompacting"], false);

    write_json_line(
        &mut writer,
        &json!({ "id": "wire_abort_1", "type": "abort" }),
    );
    let mut abort_resp = Value::Null;
    for _ in 0..8 {
        let msg = read_json_line(&mut reader);
        if msg["type"] == "response" && msg["id"] == "wire_abort_1" {
            abort_resp = msg;
            break;
        }
    }
    assert_eq!(abort_resp["success"], true);

    writer.get_ref().shutdown(Shutdown::Write).unwrap();
    drop(writer);

    let mut saw_exit = false;
    for _ in 0..8 {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            break;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
            if v["type"] == "pipkin_exit" {
                assert_eq!(v["code"], 0);
                saw_exit = true;
                break;
            }
        }
    }
    assert!(saw_exit, "expected pipkin_exit upon socket EOF");
    drop(reader);
    assert!(child.wait_clean().success());
}
