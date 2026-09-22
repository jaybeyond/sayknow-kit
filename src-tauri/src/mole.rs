//! Mole CLI (`mo`) from https://github.com/tw93/Mole.
//!
//! Piped stdout never finishes `mo clean --dry-run` (spinner + block-buffered
//! TTY UI). A PTY makes Mole think it has a terminal so scans complete and
//! `read_key` can skip the sudo prompt with Space.

use serde::Serialize;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

#[derive(Clone, Debug, Serialize)]
pub struct MoleInfo {
    pub path: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MoleRun {
    pub command: String,
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub json: Option<serde_json::Value>,
}

fn mole_bin() -> Option<PathBuf> {
    crate::which_via_shell("mo")
}

fn with_login_path(cmd: &mut Command) {
    if let Some(path_env) = crate::login_shell_path() {
        let current = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{path_env}:{current}"));
    }
    cmd.env("NO_COLOR", "1");
    cmd.env("MO_NO_COLOR", "1");
}

fn emit_line(app: Option<&AppHandle>, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() || is_spinner_noise(trimmed) {
        return;
    }
    if let Some(app) = app {
        let _ = app.emit("mole:line", trimmed);
    }
}

fn is_spinner_noise(line: &str) -> bool {
    let t = line.trim();
    t.contains("Scanning")
        || t.contains("Cleaning...")
        || t.contains("Cleaning old")
        || t.starts_with('{')
        || t.starts_with('}')
        || t.starts_with('[')
        || t.starts_with(']')
        || t.starts_with('"')
        || t.contains(": {")
        || t.contains(": [")
}

fn is_progress_line(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && !is_spinner_noise(t)
        && (t.contains('✓') || t.contains('→') || t.contains('◎') || t.contains("complete") || t.contains("dry"))
}

#[cfg(unix)]
fn interrupt_mole(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGINT);
    }
}

struct LineSplitter {
    buf: String,
}

impl LineSplitter {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let n = self
                .buf
                .find('\n')
                .or_else(|| self.buf.find('\r'));
            let Some(n) = n else { break };
            let mut line = self.buf[..n].to_string();
            self.buf.replace_range(..=n, "");
            if line.ends_with('\r') {
                line.pop();
            }
            out.push(line);
        }
        out
    }

    fn rest(&mut self) -> Option<String> {
        if self.buf.trim().is_empty() {
            self.buf.clear();
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }
}

#[cfg(unix)]
fn open_pty() -> Result<(std::fs::File, std::fs::File), String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    #[link(name = "util")]
    extern "C" {
        fn openpty(
            amaster: *mut i32,
            aslave: *mut i32,
            name: *mut i8,
            termp: *mut libc::termios,
            winp: *mut libc::winsize,
        ) -> i32;
    }
    let mut master = 0;
    let mut slave = 0;
    let rc = unsafe {
        openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err("openpty failed".into());
    }
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        ws.ws_row = 40;
        ws.ws_col = 120;
        let _ = libc::ioctl(master, libc::TIOCSWINSZ, &ws);
        Ok((
            std::fs::File::from(OwnedFd::from_raw_fd(master)),
            std::fs::File::from(OwnedFd::from_raw_fd(slave)),
        ))
    }
}

#[cfg(unix)]
fn run_mole(app: Option<&AppHandle>, args: &[&str], timeout: Duration, skip_sudo: bool) -> Result<MoleRun, String> {
    let path = mole_bin().ok_or_else(|| "mole_not_installed".to_string())?;
    let (master, slave) = open_pty()?;
    let writer = master.try_clone().map_err(|e| e.to_string())?;
    let slave_in = slave.try_clone().map_err(|e| e.to_string())?;
    let slave_err = slave.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&path);
    cmd.args(args);
    with_login_path(&mut cmd);
    cmd.env("TERM", "xterm-256color");
    cmd.stdin(Stdio::from(slave_in));
    cmd.stdout(Stdio::from(slave));
    cmd.stderr(Stdio::from(slave_err));
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("mole spawn failed: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut master = master;
        let mut buf = [0u8; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = writer;
    if skip_sudo {
        let mut kick = writer.try_clone().ok();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(mut w) = kick.take() {
                let _ = w.write_all(b" ");
                let _ = w.flush();
            }
        });
    }
    let started = Instant::now();
    let mut last_progress = Instant::now();
    let mut stdout_buf = String::new();
    let mut splitter = LineSplitter::new();
    let mut skipped_sudo = false;
    let mut interrupted = false;

    loop {
        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(bytes) => {
                let chunk = String::from_utf8_lossy(&bytes);
                stdout_buf.push_str(&chunk);
                for line in splitter.push(&chunk) {
                    if is_progress_line(&line) {
                        last_progress = Instant::now();
                    }
                    emit_line(app, &line);
                }
                if skip_sudo && !skipped_sudo {
                    let lower = stdout_buf.to_ascii_lowercase();
                    if lower.contains("sudo")
                        && (lower.contains("skip")
                            || lower.contains("space")
                            || stdout_buf.contains("跳过")
                            || stdout_buf.contains("繼續")
                            || stdout_buf.contains("继续"))
                    {
                        let _ = writer.write_all(b" ");
                        let _ = writer.flush();
                        skipped_sudo = true;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if started.elapsed() >= timeout {
                    interrupt_mole(child.id());
                    let _ = child.wait();
                    return Err("mole_timeout".into());
                }
                if !interrupted
                    && !stdout_buf.is_empty()
                    && last_progress.elapsed() >= Duration::from_secs(18)
                {
                    interrupt_mole(child.id());
                    interrupted = true;
                    last_progress = Instant::now();
                } else if interrupted && last_progress.elapsed() >= Duration::from_secs(3) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(rest) = splitter.rest() {
        emit_line(app, &rest);
    }
    let status = child.wait().map_err(|e| format!("mole wait failed: {e}"))?;
    let json = serde_json::from_str::<serde_json::Value>(&stdout_buf).ok();
    Ok(MoleRun {
        command: format!("mo {}", args.join(" ")),
        ok: status.success() || !stdout_buf.trim().is_empty(),
        stdout: stdout_buf,
        stderr: String::new(),
        json,
    })
}

#[cfg(not(unix))]
fn run_mole(app: Option<&AppHandle>, args: &[&str], timeout: Duration, _skip_sudo: bool) -> Result<MoleRun, String> {
    let _ = (app, timeout);
    Err(format!("mole is macOS-only; cannot run {}", args.join(" ")))
}

#[tauri::command]
pub fn detect_mole() -> Result<Option<MoleInfo>, String> {
    let Some(path) = mole_bin() else {
        return Ok(None);
    };
    let mut cmd = Command::new(&path);
    cmd.arg("--version");
    with_login_path(&mut cmd);
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Ok(None);
    }
    let mut version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if version.is_empty() {
        version = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Ok(Some(MoleInfo {
        path: path.to_string_lossy().into_owned(),
        version,
    }))
}

#[tauri::command(async)]
pub fn run_mole_action(app: AppHandle, action: String) -> Result<MoleRun, String> {
    let (args, timeout, skip_sudo): (&[&str], Duration, bool) = match action.as_str() {
        "analyze" => (&["analyze", "--json"], Duration::from_secs(180), false),
        "history" => (&["history", "--json"], Duration::from_secs(20), false),
        "clean-preview" => (&["clean", "--dry-run"], Duration::from_secs(90), false),
        "optimize-preview" => (&["optimize", "--dry-run"], Duration::from_secs(90), false),
        "clean" => (&["clean"], Duration::from_secs(180), true),
        "optimize" => (&["optimize"], Duration::from_secs(180), true),
        _ => return Err(format!("unknown mole action: {action}")),
    };
    run_mole(Some(&app), args, timeout, skip_sudo)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_splitter_handles_cr_and_lf() {
        let mut s = LineSplitter::new();
        let lines = s.push("a\rScanning...\r  → cache 1GB dry\n");
        assert!(lines.iter().any(|l| l.contains("cache")));
    }

    #[test]
    fn openpty_creates_a_terminal() {
        let (master, slave) = open_pty().expect("pty");
        drop(master);
        drop(slave);
    }
    #[test]
    fn pty_dry_run_returns_user_cache_rows() {
        if mole_bin().is_none() {
            return;
        }
        let run = run_mole(None, &["clean", "--dry-run"], Duration::from_secs(50), false)
            .expect("mo clean --dry-run via pty");
        let text = run.stdout;
        assert!(
            text.contains("User app cache") || text.contains("Clean Your Mac"),
            "unexpected dry-run output: {}",
            &text[..text.len().min(400)]
        );
    }

    #[test]
    fn pty_clean_skips_sudo_and_cleans_user_cache() {
        if mole_bin().is_none() {
            return;
        }
        let run = run_mole(None, &["clean"], Duration::from_secs(70), true)
            .expect("mo clean via pty");
        let text = run.stdout;
        assert!(
            text.contains("User app cache") || text.contains("Clean Your Mac"),
            "unexpected clean output: {}",
            &text[..text.len().min(500)]
        );
        assert!(
            !text.contains("Enter continue") || text.contains("User app cache"),
            "sudo prompt blocked cleanup"
        );
    }
}
