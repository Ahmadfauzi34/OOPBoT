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

// ---------------------------------------------------------------------------
// edit_file tests
// ---------------------------------------------------------------------------

struct TempEditDir(std::path::PathBuf);

impl TempEditDir {
    fn new(tag: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "oopbot-edit-test-{}-{}-{}",
            tag,
            std::process::id(),
            seq
        ));
        std::fs::create_dir_all(&dir).expect("create temp edit test dir");
        Self(dir)
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TempEditDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn edit_file_registry_confirmation_and_preview() {
    let reg = ToolRegistry::with_defaults();
    let tool = reg.get("edit_file").expect("edit_file registered in defaults");
    assert!(tool.requires_confirmation());
    assert!(tool.requires_confirmation_for_args(&json!({})));

    let preview_ok = tool.preview(&json!({
        "path": "src/main.rs",
        "edits": [
            { "oldText": "a\nb", "newText": "a\nb\nc" },
            { "oldText": "x\ny", "newText": "z\nw\nv" }
        ]
    }));
    assert_eq!(preview_ok, "edit src/main.rs (2 blok, -4/+6 baris)");

    // Malformed / empty args must never panic
    assert_eq!(tool.preview(&json!({})), "edit ?");
    assert_eq!(tool.preview(&json!({ "path": "foo.rs" })), "edit foo.rs");
    assert_eq!(
        tool.preview(&json!({ "path": "foo.rs", "edits": "not-json" })),
        "edit foo.rs"
    );
}

#[tokio::test]
async fn edit_file_single_edit_happy_path_and_diff_contract() {
    let tmp = TempEditDir::new("single");
    let file = tmp.path("sample.rs");
    std::fs::write(&file, "line 1\nline 2\nline 3\nline 4\nline 5\n").unwrap();

    let res = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [{
                "oldText": "line 2\nline 3\nline 4",
                "newText": "line 2\nline 3 modified\nline 3 extra\nline 4"
            }]
        }))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "line 1\nline 2\nline 3 modified\nline 3 extra\nline 4\nline 5\n"
    );
    assert_eq!(res["edits_applied"], 1);
    assert_eq!(res["lines_added"], 2);
    assert_eq!(res["lines_removed"], 1);
    assert_eq!(res["first_changed_line"], 3);
    assert_eq!(res["details"]["firstChangedLine"], 3);

    let diff = res["details"]["diff"].as_str().unwrap();
    // StepFigure.kt contract: no --- / +++ headers, exact +/- counts, common prefix/suffix are context
    let mut plus_count = 0usize;
    let mut minus_count = 0usize;
    for line in diff.lines() {
        assert!(!line.starts_with("---"), "unexpected header: {line}");
        assert!(!line.starts_with("+++"), "unexpected header: {line}");
        if line.starts_with('+') {
            plus_count += 1;
        } else if line.starts_with('-') {
            minus_count += 1;
        }
    }
    assert_eq!(plus_count, 2, "diff:\n{diff}");
    assert_eq!(minus_count, 1, "diff:\n{diff}");
    assert!(diff.contains("  2 line 2"), "common prefix must be context:\n{diff}");
    assert!(diff.contains("  5 line 4"), "common suffix must be context:\n{diff}");
}

#[tokio::test]
async fn edit_file_multi_edit_unsorted_and_hunk_elision() {
    let tmp = TempEditDir::new("multi");
    let file = tmp.path("long.txt");
    let original: String = (1..=60).map(|i| format!("row_{i:02}\n")).collect();
    std::fs::write(&file, &original).unwrap();

    // Unsorted order: line 50 first, then line 5, then line 25
    let res = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [
                { "oldText": "row_50", "newText": "ROW_50_NEW" },
                { "oldText": "row_05", "newText": "ROW_05_A\nROW_05_B" },
                { "oldText": "row_25\n", "newText": "" }
            ]
        }))
        .await
        .unwrap();

    let updated = std::fs::read_to_string(&file).unwrap();
    assert!(updated.contains("row_04\nROW_05_A\nROW_05_B\nrow_06\n"));
    assert!(!updated.contains("row_25\n"));
    assert!(updated.contains("row_24\nrow_26\n"));
    assert!(updated.contains("row_49\nROW_50_NEW\nrow_51\n"));
    assert_eq!(res["edits_applied"], 3);
    assert_eq!(res["lines_added"], 3);
    assert_eq!(res["lines_removed"], 3);
    assert_eq!(res["first_changed_line"], 5);

    let diff = res["details"]["diff"].as_str().unwrap();
    assert!(diff.contains("     ..."), "expected elision between distant hunks:\n{diff}");
    assert_eq!(diff.lines().filter(|l| l.starts_with('+')).count(), 3);
    assert_eq!(diff.lines().filter(|l| l.starts_with('-')).count(), 3);
}

#[tokio::test]
async fn edit_file_input_variations_and_deletion() {
    let tmp = TempEditDir::new("variations");
    let file = tmp.path("var.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").unwrap();

    // 1. Legacy top-level oldText / newText
    EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "oldText": "alpha",
            "newText": "ALPHA"
        }))
        .await
        .unwrap();
    assert!(std::fs::read_to_string(&file).unwrap().starts_with("ALPHA\n"));

    // 2. Single object in `edits`
    EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": { "oldText": "beta", "newText": "BETA" }
        }))
        .await
        .unwrap();
    assert!(std::fs::read_to_string(&file).unwrap().contains("BETA\n"));

    // 3. JSON-stringified array in `edits` + deletion (newText: "")
    let stringified = json!([
        { "oldText": "gamma\n", "newText": "" },
        { "oldText": "delta", "newText": "DELTA" }
    ])
    .to_string();
    let res = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": stringified
        }))
        .await
        .unwrap();
    assert_eq!(res["edits_applied"], 2);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "ALPHA\nBETA\nDELTA\n");

    // 4. Ambiguous combination of `edits` + legacy top-level rejected
    let err = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [{ "oldText": "ALPHA", "newText": "a" }],
            "oldText": "BETA",
            "newText": "b"
        }))
        .await
        .unwrap_err();
    assert!(err.contains("ambigu"));
}

#[tokio::test]
async fn edit_file_errors_are_atomic_and_informative() {
    let tmp = TempEditDir::new("errors");

    // 1. Missing file guides to write_file
    let missing = tmp.path("nope.txt");
    let err = EditFile
        .execute(&json!({
            "path": missing.to_string_lossy(),
            "edits": [{ "oldText": "a", "newText": "b" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("write_file"), "err: {err}");

    // 2. Directory target rejected
    let err = EditFile
        .execute(&json!({
            "path": tmp.0.to_string_lossy(),
            "edits": [{ "oldText": "a", "newText": "b" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("direktori"), "err: {err}");

    // Prepare file for failure-atomicity checks
    let file = tmp.path("atomic.txt");
    let initial = "one\ndup\ntwo\ndup\nthree\ndup\n";
    std::fs::write(&file, initial).unwrap();
    let initial_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

    // 3. oldText not found
    let err = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [{ "oldText": "missing_needle", "newText": "x" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("tidak ditemukan"), "err: {err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), initial);

    // 4. Ambiguous match (>1) lists line numbers
    let err = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [{ "oldText": "dup", "newText": "unique" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("ambigu"), "err: {err}");
    assert!(err.contains("3 kecocokan"), "err: {err}");
    assert!(err.contains("2, 4, 6"), "err: {err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), initial);

    // 5. All-or-nothing: block #1 valid, block #2 missing -> block #1 must NOT be written
    let err = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [
                { "oldText": "one", "newText": "ONE" },
                { "oldText": "nonexistent", "newText": "X" }
            ]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("#2"), "err: {err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), initial);

    // 6. Overlapping edits rejected, file untouched
    let err = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [
                { "oldText": "one\ndup\ntwo", "newText": "A" },
                { "oldText": "two\ndup\nthree", "newText": "B" }
            ]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("tumpang tindih"), "err: {err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), initial);

    // 7. Empty oldText, empty edits, and no-op rejected without changing mtime
    for bad_args in [
        json!({ "path": file.to_string_lossy(), "edits": [] }),
        json!({ "path": file.to_string_lossy(), "edits": [{ "oldText": "", "newText": "x" }] }),
        json!({ "path": file.to_string_lossy(), "edits": [{ "oldText": "one", "newText": "one" }] }),
    ] {
        assert!(EditFile.execute(&bad_args).await.is_err());
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), initial);
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        initial_mtime
    );

    // 8. Binary file (NUL byte) rejected
    let bin_file = tmp.path("bin.dat");
    let bin_bytes = b"hello\0world";
    std::fs::write(&bin_file, bin_bytes).unwrap();
    let err = EditFile
        .execute(&json!({
            "path": bin_file.to_string_lossy(),
            "edits": [{ "oldText": "hello", "newText": "hi" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("biner"), "err: {err}");
    assert_eq!(std::fs::read(&bin_file).unwrap(), bin_bytes);

    // 9. Invalid UTF-8 rejected without corrupting file
    let bad_utf8 = tmp.path("bad_utf8.txt");
    let raw_bad = vec![0x61, 0x62, 0xFF, 0xFE, 0x0A];
    std::fs::write(&bad_utf8, &raw_bad).unwrap();
    let err = EditFile
        .execute(&json!({
            "path": bad_utf8.to_string_lossy(),
            "edits": [{ "oldText": "ab", "newText": "cd" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("UTF-8"), "err: {err}");
    assert_eq!(std::fs::read(&bad_utf8).unwrap(), raw_bad);

    // 10. File exceeding MAX_EDIT_BYTES rejected
    let huge = tmp.path("huge.txt");
    std::fs::write(&huge, vec![b'a'; super::fs::MAX_EDIT_BYTES + 1]).unwrap();
    let err = EditFile
        .execute(&json!({
            "path": huge.to_string_lossy(),
            "edits": [{ "oldText": "aaa", "newText": "bbb" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("terlalu besar"), "err: {err}");
}

#[tokio::test]
async fn edit_file_preserves_crlf_bom_and_trailing_ws_fallback() {
    let tmp = TempEditDir::new("crlf_bom_ws");

    // 1. CRLF + UTF-8 BOM preserved when edited with LF args
    let crlf_bom = tmp.path("win.txt");
    let orig = "\u{FEFF}first\r\nsecond\r\nthird\r\n";
    std::fs::write(&crlf_bom, orig.as_bytes()).unwrap();

    EditFile
        .execute(&json!({
            "path": crlf_bom.to_string_lossy(),
            "edits": [{ "oldText": "second\nthird", "newText": "SECOND\nTHIRD" }]
        }))
        .await
        .unwrap();

    let after = std::fs::read_to_string(&crlf_bom).unwrap();
    assert_eq!(after, "\u{FEFF}first\r\nSECOND\r\nTHIRD\r\n");

    // 2. Stage-2 trailing-whitespace fallback succeeds when unique
    let ws_file = tmp.path("ws.txt");
    std::fs::write(&ws_file, "fn main() {   \n    let x = 1;\t \n}\n").unwrap();

    EditFile
        .execute(&json!({
            "path": ws_file.to_string_lossy(),
            "edits": [{
                "oldText": "fn main() {\n    let x = 1;",
                "newText": "fn main() {\n    let x = 42;"
            }]
        }))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&ws_file).unwrap(),
        "fn main() {\n    let x = 42;\n}\n"
    );

    // 3. Stage-2 fallback still fails when ambiguous
    let ws_ambig = tmp.path("ws_ambig.txt");
    std::fs::write(&ws_ambig, "dup  \nmid\ndup\t\n").unwrap();
    let err = EditFile
        .execute(&json!({
            "path": ws_ambig.to_string_lossy(),
            "edits": [{ "oldText": "dup", "newText": "one" }]
        }))
        .await
        .unwrap_err();
    assert!(err.contains("ambigu"), "err: {err}");
    assert!(err.contains("1, 3"), "err: {err}");
}

#[cfg(unix)]
#[tokio::test]
async fn edit_file_preserves_permissions_and_follows_symlink() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let tmp = TempEditDir::new("meta_sym");
    let target = tmp.path("script.sh");
    std::fs::write(&target, "#!/bin/sh\necho old\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

    let link = tmp.path("link.sh");
    symlink(&target, &link).unwrap();

    EditFile
        .execute(&json!({
            "path": link.to_string_lossy(),
            "edits": [{ "oldText": "echo old", "newText": "echo new" }]
        }))
        .await
        .unwrap();

    // Symlink remains a symlink, target content updated, executable mode preserved
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "#!/bin/sh\necho new\n");
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

#[tokio::test]
async fn edit_file_diff_truncation_keeps_step_figure_safe_footer() {
    let tmp = TempEditDir::new("diff_trunc");
    let file = tmp.path("big.txt");
    let old_block: String = (1..=600)
        .map(|i| format!("old_line_{i:04}_padding_text_for_diff_size_check\n"))
        .collect();
    let new_block: String = (1..=600)
        .map(|i| format!("new_line_{i:04}_padding_text_for_diff_size_check\n"))
        .collect();
    std::fs::write(&file, &old_block).unwrap();

    let res = EditFile
        .execute(&json!({
            "path": file.to_string_lossy(),
            "edits": [{ "oldText": old_block, "newText": new_block }]
        }))
        .await
        .unwrap();

    assert_eq!(res["lines_added"], 600);
    assert_eq!(res["lines_removed"], 600);
    let diff = res["details"]["diff"].as_str().unwrap();
    assert!(diff.len() <= 32_768);
    assert!(diff.ends_with("     ... (diff dipotong)"));
    assert!(diff.lines().last().unwrap().starts_with(' '));
}
