use super::{decode_base64, encode_base64};
use crate::gemini::BoxError;
use serde_json::{json, Value};
use std::env;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(super) fn resolve_terminal_shell_with(
    env_shell: Option<&str>,
    env_prefix: Option<&str>,
) -> (String, Vec<String>) {
    if let Some(sh) = env_shell.map(str::trim).filter(|s| !s.is_empty()) {
        if !sh.contains('/') || Path::new(sh).is_file() {
            let arg = if sh.ends_with("/sh") || sh == "sh" {
                "-i"
            } else {
                "-l"
            };
            return (sh.to_string(), vec![arg.to_string()]);
        }
    }

    let prefix = env_prefix
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("/data/data/com.termux/files/usr");
    let candidates = [
        (format!("{}/bin/bash", prefix), "-l"),
        (format!("{}/bin/zsh", prefix), "-l"),
        (format!("{}/bin/sh", prefix), "-i"),
        ("/bin/bash".to_string(), "-l"),
        ("/bin/sh".to_string(), "-i"),
    ];
    for (cand, flag) in candidates {
        if Path::new(&cand).is_file() {
            return (cand, vec![flag.to_string()]);
        }
    }
    ("sh".to_string(), vec!["-i".to_string()])
}

pub(super) fn resolve_terminal_shell() -> (String, Vec<String>) {
    let env_shell = env::var("SHELL").ok();
    let env_prefix = env::var("PREFIX").ok();
    resolve_terminal_shell_with(env_shell.as_deref(), env_prefix.as_deref())
}

#[cfg(unix)]
pub(super) mod pty_unix {
    use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_ushort};
    use std::fs::File;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    const O_RDWR: c_int = 2;
    const O_NOCTTY: c_int = 0x100;
    const TIOCSCTTY: c_ulong = 0x540e;
    const TIOCSWINSZ: c_ulong = 0x5414;

    #[repr(C)]
    struct Winsize {
        ws_row: c_ushort,
        ws_col: c_ushort,
        ws_xpixel: c_ushort,
        ws_ypixel: c_ushort,
    }

    extern "C" {
        fn posix_openpt(flags: c_int) -> c_int;
        fn grantpt(fd: c_int) -> c_int;
        fn unlockpt(fd: c_int) -> c_int;
        fn ptsname_r(fd: c_int, buf: *mut c_char, buflen: usize) -> c_int;
        fn setsid() -> c_int;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn open(pathname: *const c_char, flags: c_int, ...) -> c_int;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    pub struct PtySession {
        pub master: File,
        pub child: std::process::Child,
    }

    pub fn set_winsize(master: &File, rows: u16, cols: u16) {
        let ws = Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            let _ = ioctl(master.as_raw_fd(), TIOCSWINSZ, &ws);
        }
    }

    pub fn spawn_shell(
        shell: &str,
        args: &[String],
        cwd: &str,
        rows: u16,
        cols: u16,
    ) -> std::io::Result<PtySession> {
        let master_raw = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
        if master_raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master_fd = unsafe { OwnedFd::from_raw_fd(master_raw) };

        if unsafe { grantpt(master_fd.as_raw_fd()) } != 0
            || unsafe { unlockpt(master_fd.as_raw_fd()) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let mut name_buf = [0 as c_char; 256];
        let rc = unsafe { ptsname_r(master_fd.as_raw_fd(), name_buf.as_mut_ptr(), name_buf.len()) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc));
        }
        let slave_name = unsafe { CStr::from_ptr(name_buf.as_ptr()) };
        let slave_cstr = CString::new(slave_name.to_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in ptsname"))?;

        let master_file = File::from(master_fd);
        set_winsize(&master_file, rows, cols);
        let master_fd_to_close = master_file.as_raw_fd();

        let mut cmd = std::process::Command::new(shell);
        cmd.args(args)
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor");

        unsafe {
            cmd.pre_exec(move || {
                let _ = close(master_fd_to_close);
                if setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let slave = open(slave_cstr.as_ptr(), O_RDWR);
                if slave < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = ioctl(slave, TIOCSCTTY, 0 as c_int);
                if dup2(slave, 0) < 0 || dup2(slave, 1) < 0 || dup2(slave, 2) < 0 {
                    let err = std::io::Error::last_os_error();
                    let _ = close(slave);
                    return Err(err);
                }
                if slave > 2 {
                    let _ = close(slave);
                }
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        Ok(PtySession {
            master: master_file,
            child,
        })
    }
}

/// Menangani mode `terminal`: terminal interaktif shell native dengan PTY asli dan streaming Base64
pub(super) async fn handle_terminal_mode(
    params: Value,
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
) -> Result<(), BoxError> {
    run_terminal_loop(params, reader, writer)
}

pub(super) fn run_terminal_loop<R: BufRead + Send + 'static, W: Write + Send + 'static>(
    params: Value,
    mut reader: R,
    writer: W,
) -> Result<(), BoxError> {
    let cwd = params["cwd"].as_str().unwrap_or(".").to_string();
    let rows = params["rows"].as_u64().unwrap_or(24).clamp(1, u16::MAX as u64) as u16;
    let cols = params["cols"].as_u64().unwrap_or(80).clamp(1, u16::MAX as u64) as u16;

    if !Path::new(&cwd).is_dir() {
        let err = json!({
            "type": "pipkin_result",
            "ok": false,
            "error": "Folder not found",
            "code": "cwd_missing"
        });
        let mut w = writer;
        writeln!(w, "{}", serde_json::to_string(&err)?)?;
        w.flush()?;
        return Ok(());
    }

    let (shell, shell_args) = resolve_terminal_shell();
    let shared_writer = Arc::new(Mutex::new(writer));

    #[cfg(unix)]
    {
        let mut pty = pty_unix::spawn_shell(&shell, &shell_args, &cwd, rows, cols)?;
        let mut pty_read = pty.master.try_clone()?;

        enum TermEvent {
            Command(Value),
            InputEof,
            PtyClosed,
        }
        let (tx, rx) = std::sync::mpsc::channel::<TermEvent>();

        let tx_pty = tx.clone();
        let writer_out = shared_writer.clone();
        let stdout_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut pty_read, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let b64 = encode_base64(&buf[..n]);
                        let msg = json!({
                            "type": "output",
                            "data": b64
                        });
                        if let Ok(mut w) = writer_out.lock() {
                            let _ = writeln!(w, "{}", msg);
                            let _ = w.flush();
                        }
                    }
                }
            }
            let _ = tx_pty.send(TermEvent::PtyClosed);
        });

        let tx_in = tx;
        let _input_handle = std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx_in.send(TermEvent::InputEof);
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(req) = serde_json::from_str::<Value>(trimmed) {
                            if tx_in.send(TermEvent::Command(req)).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        let mut pty_write = pty.master;
        while let Ok(ev) = rx.recv() {
            match ev {
                TermEvent::Command(req) => match req["type"].as_str() {
                    Some("input") => {
                        if let Some(data_b64) = req["data"].as_str() {
                            if let Some(bytes) = decode_base64(data_b64) {
                                let _ = std::io::Write::write_all(&mut pty_write, &bytes);
                                let _ = std::io::Write::flush(&mut pty_write);
                            }
                        }
                    }
                    Some("resize") => {
                        let r = req["rows"].as_u64().unwrap_or(rows as u64).clamp(1, u16::MAX as u64) as u16;
                        let c = req["cols"].as_u64().unwrap_or(cols as u64).clamp(1, u16::MAX as u64) as u16;
                        pty_unix::set_winsize(&pty_write, r, c);
                    }
                    _ => {}
                },
                TermEvent::InputEof => {
                    let _ = pty.child.kill();
                    break;
                }
                TermEvent::PtyClosed => {
                    break;
                }
            }
        }

        drop(pty_write);
        let status = pty.child.wait().ok();
        let _ = stdout_handle.join();

        let exit_code = status.and_then(|s| s.code());
        let exit_msg = json!({
            "type": "pipkin_exit",
            "code": exit_code,
            "signal": null,
            "stderr": ""
        });
        if let Ok(mut w) = shared_writer.lock() {
            let _ = writeln!(w, "{}", serde_json::to_string(&exit_msg)?);
            let _ = w.flush();
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = (shell, shell_args, reader, shared_writer, rows, cols);
        Err("Terminal PTY is only supported on Unix targets".into())
    }
}


