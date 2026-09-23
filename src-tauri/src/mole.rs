//! Mole runs directly in a private output PTY, never through Terminal or a login shell.
//! Stdin is closed: this is an in-app operation, not an interactive terminal session.
//! Mole 1.38.1's MOLE_TEST_NO_AUTH disables authentication without disabling real
//! user-level work. MOLE_TEST_MODE would fake results and must never be inherited.
//! The internal no-auth contract is version-gated; re-audit it before adding versions.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const SUPPORTED_VERSION: &str = "1.38.1";
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const TERMINATION_GRACE: Duration = Duration::from_millis(500);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize)]
pub struct MoleInfo {
    pub path: String,
    pub version: String,
    pub supported: bool,
    pub required_version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MoleRun {
    pub command: String,
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub json: Option<serde_json::Value>,
}

#[derive(Default)]
struct RunState {
    busy: bool,
    pid: Option<u32>,
    shutting_down: bool,
}

static RUN_STATE: Mutex<RunState> = Mutex::new(RunState {
    busy: false,
    pid: None,
    shutting_down: false,
});

struct RunPermit<'a>(&'a Mutex<RunState>);

impl<'a> RunPermit<'a> {
    fn acquire(state: &'a Mutex<RunState>) -> Result<Self, String> {
        let mut current = state.lock().map_err(|_| "mole_state_poisoned")?;
        if current.shutting_down {
            return Err("mole_shutting_down".into());
        }
        if current.busy {
            return Err("mole_busy".into());
        }
        current.busy = true;
        Ok(Self(state))
    }
}

impl Drop for RunPermit<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).busy = false;
    }
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path).filter(|p| p.is_absolute()) {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

fn mole_bin() -> Option<PathBuf> {
    search_dirs()
        .into_iter()
        .map(|dir| dir.join("mo"))
        .find(|path| {
            let Ok(metadata) = path.metadata() else {
                return false;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                metadata.is_file()
            }
        })
}

fn mole_command(path: &Path, args: &[&str]) -> Result<Command, String> {
    let mut command = Command::new(path);
    command
        .args(args)
        .env(
            "PATH",
            std::env::join_paths(search_dirs()).map_err(|e| e.to_string())?,
        )
        .env("NO_COLOR", "1")
        .env("MO_NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("MOLE_TEST_NO_AUTH", "1")
        .env_remove("MOLE_TEST_MODE")
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .env_remove("TERM_PROGRAM")
        .env_remove("TERM_SESSION_ID")
        .stdin(Stdio::null());
    Ok(command)
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

#[derive(Default)]
struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in self.buf.iter().enumerate() {
            if *byte == b'\r' || *byte == b'\n' {
                if start < index {
                    lines.push(String::from_utf8_lossy(&self.buf[start..index]).into_owned());
                }
                start = index + 1;
            }
        }
        self.buf.drain(..start);
        lines
    }

    fn rest(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    // Only PIDs registered from our own setsid() child are ever passed here.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

fn shutdown_state(state: &Mutex<RunState>) {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    state.shutting_down = true;
    #[cfg(unix)]
    if let Some(pid) = state.pid {
        // The app is exiting; no worker can be relied on to escalate later.
        signal_group(pid, libc::SIGKILL);
    }
}

pub(crate) fn shutdown() {
    shutdown_state(&RUN_STATE);
}

#[cfg(unix)]
struct ProcessGroup<'a> {
    child: std::process::Child,
    state: &'a Mutex<RunState>,
    reaped: bool,
}

#[cfg(unix)]
impl<'a> ProcessGroup<'a> {
    fn spawn(command: &mut Command, state: &'a Mutex<RunState>) -> Result<Self, String> {
        // Serialize spawn/registration with shutdown so a late child cannot escape.
        let mut current = state.lock().map_err(|_| "mole_state_poisoned")?;
        if current.shutting_down {
            return Err("mole_shutting_down".into());
        }
        let child = command
            .spawn()
            .map_err(|e| format!("mole spawn failed: {e}"))?;
        current.pid = Some(child.id());
        Ok(Self {
            child,
            state,
            reaped: false,
        })
    }

    fn signal(&self, signal: i32) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pid == Some(self.child.id()) {
            signal_group(self.child.id(), signal);
        }
    }

    /// Kills the group while the leader is still a zombie, then reaps it.
    /// Reaping first would release the pid, and the group id derived from it,
    /// back to the kernel: a later group signal could then hit an unrelated
    /// process that reused the number.
    fn kill_group_then_reap(&mut self) -> Result<std::process::ExitStatus, String> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.pid == Some(self.child.id()) {
                signal_group(self.child.id(), libc::SIGKILL);
                // Unregister under the same lock, so shutdown cannot signal the
                // pid once the wait below makes it reusable.
                state.pid = None;
            }
        }
        let status = self
            .child
            .wait()
            .map_err(|e| format!("mole wait failed: {e}"))?;
        self.reaped = true;
        Ok(status)
    }

    /// Reports the leader's exit without consuming it, so the group survives
    /// until every leftover child of that group has been signalled.
    fn poll_exit(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == -1
        {
            return Err(format!(
                "mole wait failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if waited_pid(&info) == 0 {
            return Ok(None);
        }
        self.kill_group_then_reap().map(Some)
    }
}

#[cfg(unix)]
fn waited_pid(info: &libc::siginfo_t) -> libc::pid_t {
    #[cfg(target_os = "macos")]
    {
        info.si_pid
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        info.si_pid()
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup<'_> {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        self.signal(libc::SIGKILL);
        let deadline = std::time::Instant::now() + REAP_TIMEOUT;
        loop {
            match self.poll_exit() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if std::time::Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pid == Some(self.child.id()) {
            state.pid = None;
        }
    }
}

#[cfg(unix)]
fn open_pty() -> Result<(std::fs::File, std::fs::File), String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    #[link(name = "util")]
    extern "C" {
        fn openpty(
            master: *mut i32,
            slave: *mut i32,
            name: *mut i8,
            term: *mut libc::termios,
            size: *mut libc::winsize,
        ) -> i32;
    }
    let mut master = 0;
    let mut slave = 0;
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok((master, slave))
}

#[cfg(unix)]
fn run_program(
    app: Option<&AppHandle>,
    path: &Path,
    args: &[&str],
    timeout: Duration,
    state: &Mutex<RunState>,
) -> Result<MoleRun, String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::time::Instant;

    let (mut master, slave) = open_pty()?;
    let mut command = mole_command(path, args)?;
    command.stderr(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?));
    command.stdout(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = ProcessGroup::spawn(&mut command, state)?;
    // Command retains Stdio handles after spawn. Closing them is necessary for EOF.
    drop(command);
    let started = Instant::now();
    let mut stopping: Option<Instant> = None;
    let mut failure: Option<&str> = None;
    let mut status = None;
    let mut output = Vec::new();
    let mut splitter = LineSplitter::default();
    let mut closed = false;
    let mut buf = [0u8; 8192];
    loop {
        // Always check deadlines, including when a child continuously writes output.
        if failure.is_none() && started.elapsed() >= timeout {
            failure = Some("mole_timeout");
            stopping = Some(Instant::now());
            process.signal(libc::SIGTERM);
        }
        if let Some(at) = stopping {
            if at.elapsed() >= TERMINATION_GRACE {
                process.signal(libc::SIGKILL);
            }
            if at.elapsed() >= TERMINATION_GRACE + REAP_TIMEOUT {
                return Err(format!(
                    "{}: process did not exit",
                    failure.unwrap_or("mole_failed")
                ));
            }
        }
        if status.is_none() {
            // A finished leader may still have background spinners/children, so
            // poll_exit kills the group before it reaps and frees the group id.
            status = process.poll_exit()?;
        }
        let mut drained = closed;
        if !closed {
            match master.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    drained = true;
                }
                Ok(n) => {
                    let remaining = OUTPUT_LIMIT.saturating_sub(output.len());
                    output.extend_from_slice(&buf[..n.min(remaining)]);
                    for line in splitter.push(&buf[..n.min(remaining)]) {
                        emit_line(app, &line);
                    }
                    if n > remaining && failure.is_none() {
                        failure = Some("mole_output_limit");
                        stopping = Some(Instant::now());
                        process.signal(libc::SIGTERM);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => drained = true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // BSD PTYs may report EIO instead of EOF once the slave closes.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                    closed = true;
                    drained = true;
                }
                Err(e) => return Err(format!("mole read failed: {e}")),
            }
        }
        if status.is_some() && drained {
            break;
        }
        if drained {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    emit_line(app, &splitter.rest());
    let stdout = String::from_utf8_lossy(&output).into_owned();
    let status = status.ok_or("mole_missing_exit_status")?;
    let ok = failure.is_none() && status.success();
    let stderr = failure.map(str::to_owned).unwrap_or_else(|| {
        if ok {
            String::new()
        } else {
            format!("mole exited with {status}")
        }
    });
    let json = if ok {
        serde_json::from_str(&stdout).ok()
    } else {
        None
    };
    Ok(MoleRun {
        command: format!("mo {}", args.join(" ")),
        ok,
        stdout,
        stderr,
        json,
    })
}

#[cfg(not(unix))]
fn run_program(
    _app: Option<&AppHandle>,
    _path: &Path,
    _args: &[&str],
    _timeout: Duration,
    _state: &Mutex<RunState>,
) -> Result<MoleRun, String> {
    Err("mole is macOS-only".into())
}

fn version_from_output(output: &str) -> Option<&str> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Mole version "))
        .and_then(|line| line.split_whitespace().next())
}

fn require_supported_version(output: &str) -> Result<(), String> {
    let version = version_from_output(output).unwrap_or("unknown");
    if version == SUPPORTED_VERSION {
        return Ok(());
    }
    Err(format!("mole_unsupported_version: in-app cleanup requires Mole {SUPPORTED_VERSION}; found {version}"))
}

fn read_version(path: &Path) -> Result<MoleRun, String> {
    let result = run_program(
        None,
        path,
        &["--version"],
        Duration::from_secs(10),
        &RUN_STATE,
    )?;
    if !result.ok {
        return Err(result.stderr);
    }
    Ok(result)
}

#[tauri::command(async)]
pub fn detect_mole() -> Result<Option<MoleInfo>, String> {
    let _permit = RunPermit::acquire(&RUN_STATE)?;
    let Some(path) = mole_bin() else {
        return Ok(None);
    };
    let result = read_version(&path)?;
    let version = version_from_output(&result.stdout)
        .unwrap_or("unknown")
        .to_owned();
    Ok(Some(MoleInfo {
        path: path.to_string_lossy().into_owned(),
        supported: version == SUPPORTED_VERSION,
        version,
        required_version: SUPPORTED_VERSION.into(),
    }))
}

fn action_args(action: &str) -> Result<(&'static [&'static str], Duration), String> {
    match action {
        "analyze" => Ok((&["analyze", "--json"], Duration::from_secs(180))),
        "history" => Ok((&["history", "--json"], Duration::from_secs(20))),
        "clean-preview" => Ok((&["clean", "--dry-run"], Duration::from_secs(90))),
        "optimize-preview" => Ok((&["optimize", "--dry-run"], Duration::from_secs(90))),
        "clean" => Ok((&["clean"], Duration::from_secs(180))),
        "optimize" => Ok((&["optimize"], Duration::from_secs(180))),
        _ => Err(format!("unknown mole action: {action}")),
    }
}

#[tauri::command(async)]
pub fn run_mole_action(app: AppHandle, action: String) -> Result<MoleRun, String> {
    let (args, timeout) = action_args(&action)?;
    let _permit = RunPermit::acquire(&RUN_STATE)?;
    let path = mole_bin().ok_or("mole_not_installed")?;
    require_supported_version(&read_version(&path)?.stdout)?;
    run_program(Some(&app), &path, args, timeout, &RUN_STATE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_cr_lf_and_preserves_split_utf8() {
        let mut splitter = LineSplitter::default();
        assert_eq!(splitter.push(b"a\rscan\r\n"), vec!["a", "scan"]);
        let bytes = "정리\n".as_bytes();
        assert!(splitter.push(&bytes[..2]).is_empty());
        assert_eq!(splitter.push(&bytes[2..]), vec!["정리"]);
    }

    #[test]
    fn version_gate_rejects_unaudited_no_auth_contracts() {
        assert!(require_supported_version("Mole version 1.38.1\r\nmacOS: 27.0").is_ok());
        assert!(require_supported_version("Mole version 1.38.10").is_err());
        assert!(require_supported_version("Mole version 1.39.0").is_err());
        assert!(require_supported_version("unknown").is_err());
    }

    #[test]
    fn reported_version_is_the_number_alone_so_the_panel_can_gate_on_it() {
        assert_eq!(
            version_from_output("Mole version 1.38.1\r\nmacOS: 27.0"),
            Some("1.38.1")
        );
        assert_eq!(version_from_output("mo: no version banner"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Requires installed Mole; reads version only, never runs cleanup"]
    fn installed_mole_metadata_uses_hidden_runner() {
        let info = detect_mole().expect("metadata command").expect("Mole installed");
        assert_eq!(info.version, SUPPORTED_VERSION);
        assert_eq!(info.required_version, SUPPORTED_VERSION);
        assert!(info.supported);
        println!("Hidden PTY metadata: {} ({})", info.path, info.version);
        assert!(RUN_STATE.lock().unwrap().pid.is_none());
        assert!(!RUN_STATE.lock().unwrap().busy);
    }

    #[test]
    fn command_is_direct_and_does_not_inherit_shell_startup_or_fake_mode() {
        use std::ffi::OsStr;
        let cmd = mole_command(Path::new("/path with spaces/mo"), &["clean", "--dry-run"]).unwrap();
        assert_eq!(cmd.get_program(), OsStr::new("/path with spaces/mo"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("clean"), OsStr::new("--dry-run")]
        );
        let env = cmd.get_envs().collect::<std::collections::HashMap<_, _>>();
        assert_eq!(env[OsStr::new("MOLE_TEST_NO_AUTH")], Some(OsStr::new("1")));
        for key in [
            "MOLE_TEST_MODE",
            "BASH_ENV",
            "ENV",
            "TERM_PROGRAM",
            "TERM_SESSION_ID",
        ] {
            assert_eq!(env[OsStr::new(key)], None);
        }
        assert!(action_args("clean; open -a Terminal").is_err());
    }

    #[test]
    fn repeated_actions_are_rejected_and_permit_releases_on_failure() {
        let state = Mutex::new(RunState::default());
        let permit = RunPermit::acquire(&state).unwrap();
        assert_eq!(RunPermit::acquire(&state).err().unwrap(), "mole_busy");
        drop(permit);
        assert!(RunPermit::acquire(&state).is_ok());
        shutdown_state(&state);
        assert_eq!(
            RunPermit::acquire(&state).err().unwrap(),
            "mole_shutting_down"
        );
    }

    // Controlled fixtures only. Never run installed mo clean/optimize in a test suite.
    #[cfg(unix)]
    fn fixture(script: &str, timeout: Duration) -> MoleRun {
        run_program(
            None,
            Path::new("/bin/sh"),
            &["-c", script],
            timeout,
            &Mutex::new(RunState::default()),
        )
        .expect("fixture process")
    }

    #[cfg(unix)]
    #[test]
    fn hidden_output_pty_has_noninteractive_stdin_and_no_auth() {
        let result = fixture("test ! -t 0 && test -t 1 && test -t 2 && test \"$MOLE_TEST_NO_AUTH\" = 1 && test -z \"${MOLE_TEST_MODE-}\" && printf 'in-app-only\\n'", Duration::from_secs(2));
        assert!(result.ok, "{}", result.stderr);
        assert!(result.stdout.contains("in-app-only"));
    }

    #[cfg(unix)]
    #[test]
    fn output_and_valid_json_do_not_disguise_failure() {
        let result = fixture(
            "printf '{\"done\":true}\\n'; exit 7",
            Duration::from_secs(2),
        );
        assert!(!result.ok);
        assert!(result.json.is_none());
        assert!(result.stderr.contains('7'));
    }

    #[cfg(unix)]
    #[test]
    fn collects_json_and_large_output_before_reaping() {
        let json = fixture("printf '{\"entries\":[]}\\n'", Duration::from_secs(2));
        assert!(json.ok);
        assert_eq!(json.json.unwrap()["entries"], serde_json::json!([]));
        let result = fixture(
            "i=0; while [ $i -lt 4000 ]; do printf 'row-%s\\n' \"$i\"; i=$((i+1)); done",
            Duration::from_secs(3),
        );
        assert!(result.ok);
        assert!(result.stdout.contains("row-3999"));
    }

    #[cfg(unix)]
    #[test]
    fn deadline_applies_to_continuous_output_and_ignored_signals() {
        let started = std::time::Instant::now();
        let result = fixture(
            "trap '' INT TERM; while :; do printf 'Scanning\\r'; sleep 0.01; done",
            Duration::from_millis(100),
        );
        assert!(!result.ok);
        assert_eq!(result.stderr, "mole_timeout");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[cfg(unix)]
    #[test]
    fn deadline_applies_after_stdout_closes() {
        let started = std::time::Instant::now();
        let result = fixture(
            "exec 1>&- 2>&-; trap '' INT TERM; while :; do sleep 1; done",
            Duration::from_millis(100),
        );
        assert!(!result.ok);
        assert_eq!(result.stderr, "mole_timeout");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[cfg(unix)]
    #[test]
    fn exiting_leader_does_not_leave_background_children() {
        let result = fixture(
            "(trap '' HUP INT TERM; while :; do sleep 1; done) & echo child=$!; sleep 0.05; exit 0",
            Duration::from_secs(2),
        );
        assert!(result.ok);
        let pid: u32 = result
            .stdout
            .trim()
            .strip_prefix("child=")
            .unwrap()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid as i32, 0) } == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(
            unsafe { libc::kill(pid as i32, 0) },
            0,
            "child {pid} was orphaned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn app_shutdown_terminates_registered_child() {
        let state = std::sync::Arc::new(Mutex::new(RunState::default()));
        let worker_state = state.clone();
        let worker = std::thread::spawn(move || {
            run_program(
                None,
                Path::new("/bin/sh"),
                &["-c", "while :; do sleep 1; done"],
                Duration::from_secs(10),
                &worker_state,
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while state.lock().unwrap().pid.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(state.lock().unwrap().pid.is_some());
        shutdown_state(&state);
        assert!(!worker.join().unwrap().unwrap().ok);
        assert!(state.lock().unwrap().pid.is_none());
    }
}
