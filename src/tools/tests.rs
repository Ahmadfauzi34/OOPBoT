use super::shell::TERMUX_DEFAULT_BASH;
use super::*;
use serde_json::json;

#[test]
fn truncate_respects_char_boundary() {
    let s = "ééééé"; // 2 byte per char
    let t = truncate(s, 3);
    assert!(t.starts_with("é"));
    assert!(t.contains("terpotong"));
}

#[tokio::test]
async fn shell_write_read_list_roundtrip() {
    let dir = std::env::temp_dir().join(format!("oopbot-test-{}", std::process::id()));
    let file = dir.join("sub/hello.txt");
    let file_s = file.to_string_lossy().to_string();

    let w = WriteFile
        .execute(&json!({ "path": file_s, "content": "halo" }))
        .await
        .unwrap();
    assert_eq!(w["bytes_written"], 4);

    let r = ReadFile.execute(&json!({ "path": file_s })).await.unwrap();
    assert_eq!(r["content"], "halo");

    let l = ListDir
        .execute(&json!({ "path": dir.to_string_lossy() }))
        .await
        .unwrap();
    assert_eq!(l["entries"][0]["name"], "sub");
    assert_eq!(l["entries"][0]["type"], "dir");

    let s = RunShell
        .execute(&json!({ "command": format!("cat '{}'; echo err >&2; exit 3", file_s) }))
        .await
        .unwrap();
    assert_eq!(s["exit_code"], 3);
    assert_eq!(s["stdout"], "halo");
    assert_eq!(s["stderr"], "err\n");

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn shell_timeout() {
    let err = RunShell
        .execute(&json!({ "command": "sleep 5", "timeout_secs": 1 }))
        .await
        .unwrap_err();
    assert!(err.contains("timeout"));
}

#[test]
fn test_is_dangerous_shell_command_safe_cases() {
    let safe_cmds = [
        "ls -la",
        "cat src/main.rs",
        "cargo test --locked",
        "cargo check",
        "git status",
        "git diff",
        "git log -n 5",
        "git branch",
        "git checkout main",
        "git add .",
        "git commit -m 'feat: something'",
        "git push origin main",
        "mkdir -p src/new_dir",
        "echo 'hello world' > file.txt",
        "echo 'log' > /dev/null 2>&1",
        "python3 script.py",
        "npm run build",
        "chmod 755 run.sh",
        "dd if=/dev/zero of=test.img bs=1M count=10",
    ];

    let shell_tool = RunShell;
    for cmd in safe_cmds {
        assert!(
            !is_dangerous_shell_command(cmd),
            "Perintah seharusnya aman (otonom): {}",
            cmd
        );
        assert!(
            !shell_tool.requires_confirmation_for_args(&json!({ "command": cmd })),
            "Tool RunShell seharusnya tidak meminta konfirmasi: {}",
            cmd
        );
    }
}

#[test]
fn test_is_dangerous_shell_command_dangerous_cases() {
    let dangerous_cmds = [
        "rm -rf /tmp/test",
        "rm -r target",
        "rm -rf /",
        "rm -f /",
        "rm *",
        "rmdir old_dir",
        "git reset --hard HEAD~1",
        "git clean -fd",
        "git clean -f",
        "git push --force origin main",
        "git push -f",
        "git push origin +feature",
        "git branch -D old_branch",
        "reboot",
        "shutdown -h now",
        "poweroff",
        "halt",
        "init 0",
        "init 6",
        "mkfs.ext4 /dev/sdb1",
        "fdisk /dev/sda",
        "parted /dev/sda",
        "wipefs -a /dev/sda",
        "shred -u secret.txt",
        "dd if=/dev/zero of=/dev/sda",
        ":(){ :|:& };:",
        "chmod -R 777 /",
        "chmod -R 777 /system",
        "chown -R root /",
        "fastboot flash boot boot.img",
        "echo test > /dev/sda",
        "cargo build && rm -rf /tmp/build",
        "time rm -rf /tmp/build",
    ];

    let shell_tool = RunShell;
    for cmd in dangerous_cmds {
        assert!(
            is_dangerous_shell_command(cmd),
            "Perintah seharusnya berbahaya (minta izin): {}",
            cmd
        );
        assert!(
            shell_tool.requires_confirmation_for_args(&json!({ "command": cmd })),
            "Tool RunShell seharusnya meminta konfirmasi: {}",
            cmd
        );
    }
}

#[test]
fn resolve_command_shell_prefers_termux_bash_and_falls_back_cleanly() {
    // 1. Termux normal: ignores non-bash $SHELL (e.g. zsh) and selects $PREFIX/bin/bash
    let picked = resolve_command_shell_with(
        None,
        Some("/data/data/com.termux/files/usr/bin/zsh"),
        Some("/data/data/com.termux/files/usr"),
        Some("/data/data/com.termux/files/usr/bin:/system/bin"),
        |p| {
            matches!(
                p,
                "/data/data/com.termux/files/usr/bin/zsh"
                    | "/data/data/com.termux/files/usr/bin/bash"
                    | "/data/data/com.termux/files/usr/bin/sh"
            )
        },
    );
    assert_eq!(picked, "/data/data/com.termux/files/usr/bin/bash");

    // 2. Termux stripped env without PREFIX still finds /data/data/com.termux/files/usr/bin/bash
    let picked_no_prefix = resolve_command_shell_with(
        None,
        None,
        None,
        None,
        |p| p == TERMUX_DEFAULT_BASH || p == "/bin/sh",
    );
    assert_eq!(picked_no_prefix, TERMUX_DEFAULT_BASH);

    // 3. Explicit OOPBOT_SHELL override wins when valid
    let override_sh = resolve_command_shell_with(
        Some("/custom/bin/bash"),
        None,
        Some("/data/data/com.termux/files/usr"),
        None,
        |p| p == "/custom/bin/bash" || p == TERMUX_DEFAULT_BASH,
    );
    assert_eq!(override_sh, "/custom/bin/bash");

    // 4. Standard Linux /bin/bash
    let linux_bash = resolve_command_shell_with(
        None,
        Some("/bin/sh"),
        None,
        Some("/usr/local/bin:/usr/bin:/bin"),
        |p| p == "/bin/bash" || p == "/bin/sh",
    );
    assert_eq!(linux_bash, "/bin/bash");

    // 5. Fallback to sh when bash does not exist anywhere
    let fallback_sh = resolve_command_shell_with(
        None,
        None,
        None,
        Some("/bin"),
        |p| p == "/bin/sh",
    );
    assert_eq!(fallback_sh, "/bin/sh");
}

#[tokio::test]
async fn run_shell_supports_bash_builtins_time_history_and_arrays() {
    let res = RunShell
        .execute(&json!({
            "command": "set -o pipefail; arr=(alpha beta); [[ ${arr[1]} == beta ]] && history >/dev/null && time echo OK:{1..2} \"${arr[*]}\""
        }))
        .await
        .unwrap();
    assert_eq!(res["exit_code"], 0, "stderr: {}", res["stderr"]);
    assert_eq!(res["stdout"], "OK:1 OK:2 alpha beta\n");
    let stderr = res["stderr"].as_str().unwrap_or("");
    assert!(
        stderr.contains("real"),
        "expected `time` builtin timing in stderr, got: {}",
        stderr
    );

    let desc = RunShell.description();
    for tool in ["aria2c", "curl", "wget", "jq", "dig", "nc", "ssh", "git"] {
        assert!(
            desc.contains(tool),
            "RunShell description missing {tool}: {desc}"
        );
    }
}
