use crate::common::time::epoch_ms;
use crate::common::BoxError;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const GIT_CMD_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RAW_DIFF_BYTES: usize = 10 * 1024 * 1024;
const MAX_SMALL_GIT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILE_HUNK_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_PARSED_DIFF_BYTES: usize = 7 * 1024 * 1024;
const MAX_CONCURRENT_GIT_REQUESTS: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum GitCmdOutput {
    Ok(Vec<u8>),
    TooLarge,
    TimedOut,
    Failed,
}

fn kill_child_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid > 0 {
            let target = format!("-{}", pid);
            let ok = Command::new("kill")
                .args(["-s", "KILL", "--", &target])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                let _ = Command::new("sh")
                    .args(["-c", &format!("kill -KILL -{} 2>/dev/null || true", pid)])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn run_git_bounded(mut cmd: Command, timeout: Duration, max_bytes: usize) -> GitCmdOutput {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let Ok(mut child) = cmd.spawn() else {
        return GitCmdOutput::Failed;
    };
    let Some(mut stdout) = child.stdout.take() else {
        kill_child_tree(&mut child);
        return GitCmdOutput::Failed;
    };

    let exceeded = Arc::new(AtomicBool::new(false));
    let exceeded_reader = Arc::clone(&exceeded);
    let reader_handle = thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out.len().saturating_add(n) > max_bytes {
                        exceeded_reader.store(true, Ordering::SeqCst);
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        out
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if exceeded.load(Ordering::SeqCst) {
            kill_child_tree(&mut child);
            break None;
        }
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    kill_child_tree(&mut child);
                    break None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                kill_child_tree(&mut child);
                break None;
            }
        }
    };

    if exceeded.load(Ordering::SeqCst) {
        let _ = reader_handle.join();
        return GitCmdOutput::TooLarge;
    }
    if timed_out {
        let _ = reader_handle.join();
        return GitCmdOutput::TimedOut;
    }
    let bytes = reader_handle.join().unwrap_or_default();
    if exceeded.load(Ordering::SeqCst) {
        return GitCmdOutput::TooLarge;
    }
    match status {
        Some(st) if st.success() => GitCmdOutput::Ok(bytes),
        _ => GitCmdOutput::Failed,
    }
}

/// Hitung SHA-256 hash dan format sebagai lowercase hex string (FIPS 180-2 self-contained)
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut h_val = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h_val.wrapping_add(s1).wrapping_add(ch).wrapping_add(k[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h_val = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(h_val);
    }

    let mut out = String::with_capacity(64);
    for val in h {
        out.push_str(&format!("{:08x}", val));
    }
    out
}

/// Parser teks diff patch git terpadu (unified diff) menjadi format objek berkas Paseo / OOPBoT.
pub fn parse_git_diff(diff_text: &str) -> Vec<Value> {
    parse_git_diff_bounded(diff_text, usize::MAX).unwrap_or_default()
}

/// Parser diff dengan batas total byte terakumulasi (`max_total_bytes`).
/// Mengembalikan `None` segera bila total byte hunk yang dipertahankan melampaui batas
/// agar tidak mengalokasi ratusan MB RAM sebelum mengirim `tooLarge: true`.
pub(super) fn parse_git_diff_bounded(diff_text: &str, max_total_bytes: usize) -> Option<Vec<Value>> {
    let mut files = Vec::new();
    let mut current_path = String::new();
    let mut current_change = "modified".to_string();
    let mut current_kind = "text".to_string();
    let mut current_additions: i64 = 0;
    let mut current_deletions: i64 = 0;
    let mut current_hunks: Vec<Value> = Vec::new();
    let mut current_hunk_header = String::new();
    let mut current_hunk_old_start: i64 = 0;
    let mut current_hunk_new_start: i64 = 0;
    let mut current_hunk_lines: Vec<String> = Vec::new();
    let mut hunk_bytes_total: usize = 0;
    let mut total_retained_bytes: usize = 0;
    let mut file_too_large = false;
    let mut in_hunk = false;
    let mut in_file = false;

    let flush_hunk = |hunks: &mut Vec<Value>,
                      header: &str,
                      old_s: i64,
                      new_s: i64,
                      lines: &mut Vec<String>,
                      skip: bool| {
        if skip {
            lines.clear();
            return;
        }
        if !header.is_empty() || !lines.is_empty() {
            hunks.push(json!({
                "header": header,
                "oldStart": old_s,
                "newStart": new_s,
                "lines": std::mem::take(lines)
            }));
        }
    };

    let flush_file = |files: &mut Vec<Value>,
                      path: &str,
                      change: &str,
                      additions: i64,
                      deletions: i64,
                      kind: &str,
                      hunks: &mut Vec<Value>,
                      too_large: bool,
                      bytes: usize,
                      total_retained: &mut usize| -> bool {
        if !path.is_empty() {
            let final_kind = if too_large || bytes > MAX_FILE_HUNK_BYTES {
                "too_large"
            } else {
                kind
            };
            if final_kind == "too_large" {
                hunks.clear();
            } else {
                *total_retained = total_retained.saturating_add(bytes);
                if *total_retained > max_total_bytes {
                    return false;
                }
            }
            files.push(json!({
                "path": path,
                "change": change,
                "additions": additions,
                "deletions": deletions,
                "kind": final_kind,
                "hunks": if final_kind == "too_large" { vec![] } else { std::mem::take(hunks) }
            }));
        }
        true
    };

    for line in diff_text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if in_hunk {
                flush_hunk(
                    &mut current_hunks,
                    &current_hunk_header,
                    current_hunk_old_start,
                    current_hunk_new_start,
                    &mut current_hunk_lines,
                    file_too_large,
                );
                in_hunk = false;
            }
            if in_file
                && !flush_file(
                    &mut files,
                    &current_path,
                    &current_change,
                    current_additions,
                    current_deletions,
                    &current_kind,
                    &mut current_hunks,
                    file_too_large,
                    hunk_bytes_total,
                    &mut total_retained_bytes,
                )
            {
                return None;
            }
            in_file = true;
            current_change = "modified".to_string();
            current_kind = "text".to_string();
            current_additions = 0;
            current_deletions = 0;
            hunk_bytes_total = 0;
            file_too_large = false;
            current_hunk_header.clear();

            // Ekstrak b/ path: "diff --git a/foo.txt b/foo.txt"
            if let Some(pos) = rest.rfind(" b/") {
                current_path = rest[pos + 3..].trim().to_string();
            } else if let Some(pos) = rest.find(' ') {
                current_path = rest[pos + 1..].trim().trim_start_matches("b/").to_string();
            } else {
                current_path = rest.trim().to_string();
            }
            continue;
        }

        if !in_file {
            continue;
        }

        if line.starts_with("new file mode ") {
            current_change = "added".to_string();
            continue;
        } else if line.starts_with("deleted file mode ") {
            current_change = "deleted".to_string();
            continue;
        } else if line.starts_with("rename from ") {
            current_change = "renamed".to_string();
            continue;
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            current_path = rest.trim().to_string();
            continue;
        } else if line.starts_with("copy from ") {
            current_change = "copied".to_string();
            continue;
        } else if let Some(rest) = line.strip_prefix("copy to ") {
            current_path = rest.trim().to_string();
            continue;
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            current_kind = "binary".to_string();
            continue;
        } else if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("index ") {
            if let Some(rest) = line.strip_prefix("--- a/") {
                if current_change == "deleted" && current_path.is_empty() {
                    current_path = rest.trim().to_string();
                }
            }
            if let Some(rest) = line.strip_prefix("+++ b/") {
                if current_change != "deleted" {
                    current_path = rest.trim().to_string();
                }
            }
            continue;
        }

        if let Some(after_at) = line.strip_prefix("@@ ") {
            if in_hunk {
                flush_hunk(
                    &mut current_hunks,
                    &current_hunk_header,
                    current_hunk_old_start,
                    current_hunk_new_start,
                    &mut current_hunk_lines,
                    file_too_large,
                );
            }
            in_hunk = true;
            if file_too_large {
                continue;
            }
            let header = if let Some(end) = after_at.find("@@") {
                line[..3 + end + 2].to_string()
            } else {
                line.to_string()
            };
            current_hunk_header = header;

            let mut old_s = 1;
            let mut new_s = 1;
            for part in line.split_whitespace() {
                if let Some(rest) = part.strip_prefix('-') {
                    let num_part = rest.split(',').next().unwrap_or("1");
                    old_s = num_part.parse::<i64>().unwrap_or(1);
                } else if let Some(rest) = part.strip_prefix('+') {
                    let num_part = rest.split(',').next().unwrap_or("1");
                    new_s = num_part.parse::<i64>().unwrap_or(1);
                }
            }
            current_hunk_old_start = old_s;
            current_hunk_new_start = new_s;
            continue;
        }

        if in_hunk {
            hunk_bytes_total = hunk_bytes_total.saturating_add(line.len() + 1);
            if line.starts_with('+') {
                current_additions += 1;
            } else if line.starts_with('-') {
                current_deletions += 1;
            }
            if !file_too_large && hunk_bytes_total > MAX_FILE_HUNK_BYTES {
                file_too_large = true;
                current_hunk_lines.clear();
                current_hunks.clear();
                continue;
            }
            if !file_too_large
                && (line.starts_with('+')
                    || line.starts_with('-')
                    || line.starts_with(' ')
                    || line.starts_with('\\'))
            {
                current_hunk_lines.push(line.to_string());
            }
        }
    }

    if in_hunk {
        flush_hunk(
            &mut current_hunks,
            &current_hunk_header,
            current_hunk_old_start,
            current_hunk_new_start,
            &mut current_hunk_lines,
            file_too_large,
        );
    }
    if in_file
        && !flush_file(
            &mut files,
            &current_path,
            &current_change,
            current_additions,
            current_deletions,
            &current_kind,
            &mut current_hunks,
            file_too_large,
            hunk_bytes_total,
            &mut total_retained_bytes,
        )
    {
        return None;
    }

    Some(files)
}

/// Deteksi apakah direktori adalah repository git dan git tersedia di sistem
pub(super) fn git_detect(root: &str, trusted: bool) -> (bool, bool) {
    let has_git = std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let dot_git = Path::new(root).join(".git").exists();
    if !trusted || !has_git || !dot_git {
        return (dot_git, has_git);
    }
    let toplevel = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--show-toplevel"])
        .output();
    if let Ok(out) = toplevel {
        if out.status.success() {
            let top_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let real_root = std::fs::canonicalize(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| root.to_string());
            let top_canon = std::fs::canonicalize(&top_str)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(top_str);
            return (top_canon == real_root, true);
        }
    }
    (false, true)
}

/// Nama branch git saat ini
pub(super) fn git_current_branch(root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !b.is_empty() {
            return Some(b);
        }
    }
    None
}

/// Default branch (origin/HEAD atau main / master)
pub(super) fn git_default_branch(root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .output()
        .ok();
    if let Some(o) = out {
        if o.status.success() {
            let ref_name = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !ref_name.is_empty() {
                let remote_name = ref_name.trim_start_matches("refs/remotes/");
                let local_name = remote_name.trim_start_matches("origin/");
                return Some(local_name.to_string());
            }
        }
    }
    let has_main = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", "refs/heads/main"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_main {
        return Some("main".to_string());
    }
    let has_master = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", "refs/heads/master"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_master {
        return Some("master".to_string());
    }
    None
}

/// Ref pembanding (origin/<base> atau <base>)
pub(super) fn git_comparison_ref(root: &str, base: &str) -> Option<String> {
    let name = base.trim_start_matches("refs/heads/").trim_start_matches("origin/");
    let origin_ref = format!("refs/remotes/origin/{}", name);
    let has_origin = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", &origin_ref])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_origin {
        return Some(format!("origin/{}", name));
    }
    let local_ref = format!("refs/heads/{}", name);
    let has_local = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", &local_ref])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_local {
        return Some(name.to_string());
    }
    None
}

/// Merge-base ref HEAD
pub(super) fn git_merge_base(root: &str, comp_ref: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "merge-base", comp_ref, "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Commit HEAD
pub(super) fn git_head_commit(root: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Menangani permintaan perubahan berkas git (mode: uncommitted atau base)
pub(super) fn handle_git_changes(
    root: &str,
    mode: &str,
    ignore_whitespace: bool,
    known: Option<&str>,
) -> Value {
    use std::sync::atomic::AtomicU64;
    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

    let branch = git_current_branch(root);
    let base = git_default_branch(root);
    let base_ref = base.as_deref().and_then(|b| git_comparison_ref(root, b));

    let make_too_large = || {
        let too_large_body = json!({
            "branch": branch,
            "baseRef": base_ref,
            "files": [],
            "tooLarge": true
        });
        let fp = sha256_hex(too_large_body.to_string().as_bytes());
        if known == Some(&fp) {
            return json!({ "fingerprint": fp, "unchanged": true });
        }
        let mut res = too_large_body;
        res["fingerprint"] = json!(fp);
        res
    };

    let diff_result = if mode == "base" {
        if let Some(ref comp) = base_ref {
            let from = git_merge_base(root, comp).unwrap_or_else(|| comp.clone());
            let mut cmd = Command::new("git");
            cmd.args([
                "-C",
                root,
                "-c",
                "diff.renameLimit=200",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "-M",
            ]);
            if ignore_whitespace {
                cmd.arg("-w");
            }
            cmd.args([&from, "HEAD", "--"]);
            run_git_bounded(cmd, GIT_CMD_TIMEOUT, MAX_RAW_DIFF_BYTES)
        } else {
            GitCmdOutput::Ok(Vec::new())
        }
    } else {
        // mode uncommitted: gunakan scratch index unik agar berkas untracked terdeteksi tanpa mengubah repo
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp_dir = std::env::temp_dir().join(format!(
            "oopbot_git_scratch_{}_{}_{}",
            std::process::id(),
            epoch_ms(),
            seq
        ));
        let temp_objects = temp_dir.join("objects");
        let temp_index = temp_dir.join("index");
        let _ = std::fs::create_dir_all(&temp_objects);

        let real_index = Command::new("git")
            .args(["-C", root, "rev-parse", "--git-path", "index"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| ".git/index".into());
        let real_objects = Command::new("git")
            .args(["-C", root, "rev-parse", "--git-path", "objects"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| ".git/objects".into());

        let abs_index = Path::new(root).join(&real_index);
        let abs_objects = Path::new(root).join(&real_objects);

        if abs_index.exists() {
            let _ = std::fs::copy(&abs_index, &temp_index);
        }

        let mut add_cmd = Command::new("git");
        add_cmd
            .args(["-C", root, "add", "--intent-to-add", "--all"])
            .env("GIT_INDEX_FILE", &temp_index)
            .env("GIT_OBJECT_DIRECTORY", &temp_objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &abs_objects);
        let add_res = run_git_bounded(add_cmd, GIT_CMD_TIMEOUT, MAX_SMALL_GIT_OUTPUT_BYTES);
        if matches!(add_res, GitCmdOutput::TimedOut) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return json!({
                "code": "failed",
                "error": "git status/add timed out after 30s"
            });
        }

        let from = git_head_commit(root)
            .unwrap_or_else(|| "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string());
        let mut diff_cmd = Command::new("git");
        diff_cmd.args([
            "-C",
            root,
            "-c",
            "diff.renameLimit=200",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
        ]);
        if ignore_whitespace {
            diff_cmd.arg("-w");
        }
        diff_cmd
            .args([&from, "--"])
            .env("GIT_INDEX_FILE", &temp_index)
            .env("GIT_OBJECT_DIRECTORY", &temp_objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &abs_objects);

        let res = run_git_bounded(diff_cmd, GIT_CMD_TIMEOUT, MAX_RAW_DIFF_BYTES);
        let _ = std::fs::remove_dir_all(&temp_dir);
        res
    };

    let diff_bytes = match diff_result {
        GitCmdOutput::Ok(bytes) => bytes,
        GitCmdOutput::TooLarge => return make_too_large(),
        GitCmdOutput::TimedOut => {
            return json!({
                "code": "failed",
                "error": "git diff timed out after 30s"
            });
        }
        GitCmdOutput::Failed => Vec::new(),
    };

    let diff_output = String::from_utf8_lossy(&diff_bytes);
    let Some(files) = parse_git_diff_bounded(&diff_output, MAX_TOTAL_PARSED_DIFF_BYTES) else {
        return make_too_large();
    };
    let body = json!({
        "branch": branch,
        "baseRef": base_ref,
        "files": files,
        "tooLarge": false
    });
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    if body_str.len() > 8 * 1024 * 1024 {
        return make_too_large();
    }

    let fp = sha256_hex(body_str.as_bytes());
    if known == Some(&fp) {
        return json!({ "fingerprint": fp, "unchanged": true });
    }

    let mut res = body;
    res["fingerprint"] = json!(fp);
    res
}

/// Menangani daftar commit riwayat git ahead dari perbandingan
pub(super) fn handle_git_commits(root: &str, known: Option<&str>) -> Value {
    let branch = git_current_branch(root);
    let head = git_head_commit(root);
    let base = git_default_branch(root);
    let comparison = if branch.is_some() && head.is_some() && base.is_some() {
        base.as_deref().and_then(|b| git_comparison_ref(root, b))
    } else {
        None
    };

    let Some(comp_ref) = comparison else {
        let empty_body = json!({
            "baseRef": null,
            "total": 0,
            "commits": []
        });
        let fp = sha256_hex(empty_body.to_string().as_bytes());
        if known == Some(&fp) {
            return json!({ "fingerprint": fp, "unchanged": true });
        }
        let mut res = empty_body;
        res["fingerprint"] = json!(fp);
        return res;
    };
    let range = format!("{}..HEAD", comp_ref);

    let mut log_cmd = Command::new("git");
    log_cmd.args([
        "-C",
        root,
        "log",
        "--no-color",
        "--no-show-signature",
        "-z",
        "--max-count=500",
        "--format=%H%x00%at%x00%s",
        &range,
        "--",
    ]);
    let log_out = match run_git_bounded(log_cmd, GIT_CMD_TIMEOUT, MAX_SMALL_GIT_OUTPUT_BYTES) {
        GitCmdOutput::Ok(bytes) => bytes,
        GitCmdOutput::TimedOut => {
            return json!({
                "code": "failed",
                "error": "git log timed out after 30s"
            });
        }
        GitCmdOutput::TooLarge | GitCmdOutput::Failed => Vec::new(),
    };

    let mut count_cmd = Command::new("git");
    count_cmd.args(["-C", root, "rev-list", "--count", &range, "--"]);
    let count_out = match run_git_bounded(count_cmd, GIT_CMD_TIMEOUT, 64 * 1024) {
        GitCmdOutput::Ok(bytes) => String::from_utf8_lossy(&bytes)
            .trim()
            .parse::<i64>()
            .unwrap_or(0),
        _ => 0,
    };

    let fields: Vec<&[u8]> = log_out.split(|&b| b == 0).collect();
    let mut commits = Vec::new();
    let mut i = 0;
    while i + 2 < fields.len() {
        let sha = String::from_utf8_lossy(fields[i]).to_string();
        let at_str = String::from_utf8_lossy(fields[i + 1]);
        let author_time = at_str.parse::<i64>().unwrap_or(0);
        let subject = String::from_utf8_lossy(fields[i + 2]).to_string();
        if !sha.is_empty() {
            commits.push(json!({
                "sha": sha,
                "subject": subject,
                "authorTime": author_time
            }));
        }
        i += 3;
    }

    let total = count_out.max(commits.len() as i64);
    let body = json!({
        "baseRef": comp_ref,
        "total": total,
        "commits": commits
    });

    let body_str = serde_json::to_string(&body).unwrap_or_default();
    let fp = sha256_hex(body_str.as_bytes());
    if known == Some(&fp) {
        return json!({ "fingerprint": fp, "unchanged": true });
    }

    let mut res = body;
    res["fingerprint"] = json!(fp);
    res
}

/// Menangani diff satu commit terhadap commit parent
pub(super) fn handle_git_commit_diff(root: &str, sha: &str, ignore_whitespace: bool) -> Value {
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) || sha.len() < 4 || sha.len() > 64 {
        return json!({
            "code": "commit_not_found",
            "error": format!("Commit {} not found.", sha)
        });
    }

    let commit_verify = Command::new("git")
        .args([
            "-C",
            root,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}^{{commit}}", sha),
        ])
        .output();
    let full_sha = match commit_verify {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            return json!({
                "code": "commit_not_found",
                "error": format!("Commit {} not found.", sha)
            });
        }
    };

    let parent_verify = Command::new("git")
        .args([
            "-C",
            root,
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}^1", full_sha),
        ])
        .output();
    let from = match parent_verify {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string(),
    };

    let mut diff_cmd = Command::new("git");
    diff_cmd.args([
        "-C",
        root,
        "-c",
        "diff.renameLimit=200",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "-M",
    ]);
    if ignore_whitespace {
        diff_cmd.arg("-w");
    }
    diff_cmd.args([&from, &full_sha, "--"]);

    let diff_bytes = match run_git_bounded(diff_cmd, GIT_CMD_TIMEOUT, MAX_RAW_DIFF_BYTES) {
        GitCmdOutput::Ok(bytes) => bytes,
        GitCmdOutput::TooLarge => {
            return json!({
                "files": [],
                "tooLarge": true
            });
        }
        GitCmdOutput::TimedOut => {
            return json!({
                "code": "failed",
                "error": "git diff timed out after 30s"
            });
        }
        GitCmdOutput::Failed => Vec::new(),
    };
    let diff_out = String::from_utf8_lossy(&diff_bytes);
    let Some(files) = parse_git_diff_bounded(&diff_out, MAX_TOTAL_PARSED_DIFF_BYTES) else {
        return json!({
            "files": [],
            "tooLarge": true
        });
    };
    let body = json!({
        "files": files,
        "tooLarge": false
    });
    if serde_json::to_string(&body).unwrap_or_default().len() > 8 * 1024 * 1024 {
        return json!({
            "files": [],
            "tooLarge": true
        });
    }
    body
}

fn dispatch_git_request(req: &Value) -> Option<Value> {
    let id = req["id"].as_i64().unwrap_or(0);
    let root = req["root"].as_str().unwrap_or(".");

    match req["type"].as_str() {
        Some("oopbot_git_detect") => {
            let trusted = req["trusted"].as_bool().unwrap_or(false);
            let (repo, git) = git_detect(root, trusted);
            Some(json!({
                "type": "oopbot_git_detected",
                "id": id,
                "repo": repo,
                "git": git
            }))
        }
        Some("oopbot_git_changes") => {
            let mode = req["mode"].as_str().unwrap_or("uncommitted");
            let ignore_whitespace = req["ignoreWhitespace"].as_bool().unwrap_or(false);
            let known = req["known"].as_str();
            let mut data = handle_git_changes(root, mode, ignore_whitespace, known);
            if let Some(obj) = data.as_object_mut() {
                obj.insert("type".to_string(), json!("oopbot_git_changes_result"));
                obj.insert("id".to_string(), json!(id));
            }
            Some(data)
        }
        Some("oopbot_git_commits") => {
            let known = req["known"].as_str();
            let mut data = handle_git_commits(root, known);
            if let Some(obj) = data.as_object_mut() {
                obj.insert("type".to_string(), json!("oopbot_git_commits_result"));
                obj.insert("id".to_string(), json!(id));
            }
            Some(data)
        }
        Some("oopbot_git_commit_diff") => {
            let sha = req["sha"].as_str().unwrap_or("");
            let ignore_whitespace = req["ignoreWhitespace"].as_bool().unwrap_or(false);
            let mut data = handle_git_commit_diff(root, sha, ignore_whitespace);
            if let Some(obj) = data.as_object_mut() {
                obj.insert("type".to_string(), json!("oopbot_git_commit_diff_result"));
                obj.insert("id".to_string(), json!(id));
            }
            Some(data)
        }
        _ => None,
    }
}

/// Menangani mode `git`: backend tab Git Changes di UI Android dengan eksekusi konkuren terbatas
/// agar `oopbot_git_commits` / `oopbot_git_detect` tidak terblokir di belakang `oopbot_git_changes`.
pub(super) async fn handle_git_mode(
    mut reader: BufReader<TcpStream>,
    mut writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    let ready = json!({ "type": "oopbot_git_ready" });
    writeln!(writer, "{}", serde_json::to_string(&ready)?)?;
    writer.flush()?;

    let shared_writer = Arc::new(Mutex::new(writer));
    let active = Arc::new((Mutex::new(0usize), Condvar::new()));
    let mut line = String::new();

    while let Ok(bytes) = reader.read_line(&mut line) {
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            line.clear();
            continue;
        };
        line.clear();

        // Batasi jumlah worker paralel agar tidak membanjiri CPU/RAM perangkat.
        {
            let (lock, cvar) = &*active;
            let mut count = lock.lock().unwrap_or_else(|e| e.into_inner());
            while *count >= MAX_CONCURRENT_GIT_REQUESTS {
                count = cvar.wait(count).unwrap_or_else(|e| e.into_inner());
            }
            *count += 1;
        }

        struct ActiveSlotGuard(Arc<(Mutex<usize>, Condvar)>);
        impl Drop for ActiveSlotGuard {
            fn drop(&mut self) {
                let (lock, cvar) = &*self.0;
                let mut count = lock.lock().unwrap_or_else(|e| e.into_inner());
                *count = count.saturating_sub(1);
                cvar.notify_one();
            }
        }

        let writer_clone = Arc::clone(&shared_writer);
        let guard = ActiveSlotGuard(Arc::clone(&active));
        thread::spawn(move || {
            let _guard = guard;
            if let Some(reply) = dispatch_git_request(&req) {
                if let Ok(serialized) = serde_json::to_string(&reply) {
                    let mut w = writer_clone.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = writeln!(w, "{}", serialized);
                    let _ = w.flush();
                }
            }
        });
    }

    // Tunggu seluruh worker yang sedang berjalan sebelum menutup koneksi.
    let (lock, cvar) = &*active;
    let mut count = lock.lock().unwrap_or_else(|e| e.into_inner());
    while *count > 0 {
        count = cvar.wait(count).unwrap_or_else(|e| e.into_inner());
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn run_git_loop<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
) -> Result<(), BoxError> {
    let ready = json!({ "type": "oopbot_git_ready" });
    writeln!(writer, "{}", serde_json::to_string(&ready)?)?;
    writer.flush()?;

    let mut line = String::new();
    while let Ok(bytes) = reader.read_line(&mut line) {
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            line.clear();
            continue;
        };

        if let Some(reply) = dispatch_git_request(&req) {
            writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
            writer.flush()?;
        }

        line.clear();
    }

    Ok(())
}

