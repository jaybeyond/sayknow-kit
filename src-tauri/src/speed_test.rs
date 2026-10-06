//! Internet speed through macOS's own `networkQuality` (Apple's measurement
//! servers, the same test as `networkQuality` in Terminal). No third-party
//! service is contacted and nothing is installed.
//!
//! The tool prints a live "Downlink: … Mbps … - Uplink: … Mbps" line about
//! four times a second, but only when its output is a terminal — through a
//! pipe it stays silent until the end. So it runs on a pseudo-terminal, the
//! live lines are forwarded to the window as they arrive, and the final result
//! goes to a JSON file (`-c<path>`), which is what the summary is read from.
//!
//! The tool honours `-M` on a healthy network, but on a broken path (a bound
//! interface that does not exist was observed) it can wait forever, so the run
//! also has a hard deadline and is killed when it passes.

use serde::Serialize;
use serde_json::Value;

#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
const TOOL: &str = "/usr/bin/networkQuality";
/// Seconds handed to `-M`. Sequential mode splits it between download and upload.
#[cfg(target_os = "macos")]
const MAX_RUNTIME_SECS: &str = "20";
#[cfg(target_os = "macos")]
const HARD_DEADLINE: Duration = Duration::from_secs(45);
/// A sequential run wrote about 10 KB of JSON; anything near this is not a result.
#[cfg(target_os = "macos")]
const OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;
/// The live lines of a 20 s run add up to about 9 KB; past this the terminal
/// output is drained without being kept.
#[cfg(target_os = "macos")]
const LIVE_BUFFER_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SpeedTestResult {
    /// Bits per second.
    pub download_bps: Option<f64>,
    /// Bits per second.
    pub upload_bps: Option<f64>,
    /// Idle round trip in milliseconds.
    pub idle_latency_ms: Option<f64>,
    /// Bytes the test itself moved in both directions.
    pub bytes_used: Option<u64>,
    pub interface: Option<String>,
    pub server: Option<String>,
}

/// One live reading while the test runs. Sequential mode measures download
/// first, then upload; the direction not yet measured reads 0.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SpeedTestProgress {
    pub elapsed_ms: u64,
    /// Bits per second.
    pub download_bps: f64,
    /// Bits per second.
    pub upload_bps: f64,
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `networkQuality -c` exits 0 even when the network is unreachable; the
/// failure is an `error_code` in the JSON instead.
pub fn parse(stdout: &str) -> Result<SpeedTestResult, String> {
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|_| "speed_failed")?;
    if !value.is_object() {
        return Err("speed_failed".into());
    }
    if value.get("error_code").is_some() {
        return Err("speed_network_error".into());
    }
    let download_bps = number(&value, "dl_throughput");
    let upload_bps = number(&value, "ul_throughput");
    if download_bps.is_none() && upload_bps.is_none() {
        return Err("speed_failed".into());
    }
    let transferred = ["dl_bytes_transferred", "ul_bytes_transferred"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_u64))
        .collect::<Vec<_>>();
    Ok(SpeedTestResult {
        download_bps,
        upload_bps,
        idle_latency_ms: number(&value, "base_rtt"),
        bytes_used: if transferred.is_empty() {
            None
        } else {
            Some(transferred.iter().sum())
        },
        interface: text(&value, "interface_name"),
        server: text(&value, "test_endpoint"),
    })
}

/// The number of Mbps after `label`, e.g. "Downlink: 167.391 Mbps".
fn mbps_after(line: &str, label: &str) -> Option<f64> {
    let rest = line[line.find(label)? + label.len()..].trim_start();
    let value = rest.split_whitespace().next()?;
    if !rest[value.len()..].trim_start().starts_with("Mbps") {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
}

/// Reads one live status line. The tool redraws it with `\r` and an erase
/// sequence, so callers split on `\r`/`\n` and hand each piece here; anything
/// that is not a complete reading (the summary, a half-written line) is `None`.
pub fn parse_progress_line(line: &str) -> Option<(f64, f64)> {
    let download = mbps_after(line, "Downlink:")?;
    let upload = mbps_after(line, "Uplink:")?;
    Some((download * 1_000_000.0, upload * 1_000_000.0))
}

#[cfg(target_os = "macos")]
static RUNNING: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static CANCEL: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);

/// Clears the run slot however the command ends, including a dropped future.
#[cfg(target_os = "macos")]
struct RunGuard;

#[cfg(target_os = "macos")]
impl Drop for RunGuard {
    fn drop(&mut self) {
        CANCEL.lock().unwrap_or_else(|e| e.into_inner()).take();
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// The result file, removed however the run ends.
#[cfg(target_os = "macos")]
struct ResultFile(std::path::PathBuf);

#[cfg(target_os = "macos")]
impl Drop for ResultFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(target_os = "macos")]
enum Ending {
    Finished(std::io::Result<std::process::ExitStatus>),
    Cancelled,
    TimedOut,
}

/// A pseudo-terminal pair: the child writes to `secondary`, we read `primary`.
#[cfg(target_os = "macos")]
fn open_terminal() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;
    let mut primary = -1;
    let mut secondary = -1;
    // SAFETY: openpty writes two descriptors it opened into the out-params; a
    // null name, termios and window size select the defaults.
    let rc = unsafe {
        libc::openpty(
            &mut primary,
            &mut secondary,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors are fresh and owned by nothing else.
    Ok(unsafe {
        (
            std::fs::File::from_raw_fd(primary),
            std::fs::File::from_raw_fd(secondary),
        )
    })
}

/// Forwards live readings until the terminal closes. Runs on a blocking
/// thread: a terminal primary is not something tokio can poll.
#[cfg(target_os = "macos")]
fn read_progress(
    mut primary: std::fs::File,
    started: std::time::Instant,
    mut report: impl FnMut(SpeedTestProgress),
) {
    use std::io::Read;
    let mut chunk = [0u8; 4096];
    let mut pending = String::new();
    // Read errors (EIO once the child has exited) end the loop like EOF.
    while let Ok(n) = primary.read(&mut chunk) {
        if n == 0 {
            break;
        }
        if pending.len() > LIVE_BUFFER_LIMIT {
            pending.clear();
        }
        pending.push_str(&String::from_utf8_lossy(&chunk[..n]));
        // Keep the piece after the last separator: it may still be growing.
        let Some(cut) = pending.rfind(['\r', '\n']) else {
            continue;
        };
        let complete: String = pending.drain(..=cut).collect();
        if let Some((download_bps, upload_bps)) = complete
            .split(['\r', '\n'])
            .rev()
            .find_map(parse_progress_line)
        {
            report(SpeedTestProgress {
                elapsed_ms: started.elapsed().as_millis() as u64,
                download_bps,
                upload_bps,
            });
        }
    }
}

#[cfg(target_os = "macos")]
fn read_result(path: &std::path::Path) -> Result<SpeedTestResult, String> {
    use std::io::Read;
    let mut output = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(OUTPUT_LIMIT).read_to_string(&mut output))
        .map_err(|_| "speed_failed")?;
    parse(&output)
}

#[cfg(target_os = "macos")]
async fn run(
    report: impl FnMut(SpeedTestProgress) + Send + 'static,
) -> Result<SpeedTestResult, String> {
    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("speed_busy".into());
    }
    let _guard = RunGuard;
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    *CANCEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel_tx);

    let result_file = ResultFile(std::env::temp_dir().join(format!(
        "sayknow-speed-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )));
    let (primary, secondary) = open_terminal().map_err(|_| "speed_failed")?;
    let output = secondary.try_clone().map_err(|_| "speed_failed")?;
    let mut child = tokio::process::Command::new(TOOL)
        .arg("-s")
        .args(["-M", MAX_RUNTIME_SECS])
        // Attached to the flag: `-c <path>` would take the path as a stray argument.
        .arg(format!("-c{}", result_file.0.display()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(secondary))
        .stderr(std::process::Stdio::from(output))
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "speed_unsupported")?;
    // `Command` keeps the secondary end until it is dropped; the child holds
    // its own copies, so the reader sees the end once the child exits.
    let started = std::time::Instant::now();
    let reader = tauri::async_runtime::spawn_blocking(move || {
        read_progress(primary, started, report);
    });

    let ending = tokio::select! {
        finished = child.wait() => Ending::Finished(finished),
        _ = cancel_rx => Ending::Cancelled,
        _ = tokio::time::sleep(HARD_DEADLINE) => Ending::TimedOut,
    };
    let outcome = match ending {
        Ending::Finished(Ok(status)) if status.success() => read_result(&result_file.0),
        Ending::Finished(_) => Err("speed_failed".to_owned()),
        Ending::Cancelled => {
            let _ = child.kill().await;
            Err("speed_cancelled".to_owned())
        }
        Ending::TimedOut => {
            let _ = child.kill().await;
            Err("speed_timeout".to_owned())
        }
    };
    // The child has exited or been killed, so the terminal closes and the
    // reader returns; wait for it so no reading arrives after the result.
    let _ = reader.await;
    outcome
}

#[tauri::command]
pub async fn run_speed_test(
    progress: tauri::ipc::Channel<SpeedTestProgress>,
) -> Result<SpeedTestResult, String> {
    #[cfg(target_os = "macos")]
    {
        run(move |reading| {
            // A closed window only means nobody is watching; the run goes on.
            let _ = progress.send(reading);
        })
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = progress;
        Err("speed_unsupported".into())
    }
}

#[tauri::command]
pub fn cancel_speed_test() {
    shutdown();
}

/// Stops a running test. Also called on app exit; `-M` bounds an orphan if
/// the runtime is gone before the kill lands.
pub(crate) fn shutdown() {
    #[cfg(target_os = "macos")]
    if let Some(cancel) = CANCEL.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = cancel.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_throughput_latency_and_data_from_a_sequential_run() {
        let result = parse(
            r#"{"base_rtt":114.86,"dl_throughput":155087616,"ul_throughput":162815872,
            "dl_bytes_transferred":384228046,"ul_bytes_transferred":292225021,
            "interface_name":"en0","test_endpoint":"jptyo5-edge-fx-005.aaplimg.com",
            "dl_responsiveness":70.9}"#,
        )
        .unwrap();
        assert_eq!(result.download_bps, Some(155087616.0));
        assert_eq!(result.upload_bps, Some(162815872.0));
        assert_eq!(result.idle_latency_ms, Some(114.86));
        assert_eq!(result.bytes_used, Some(676453067));
        assert_eq!(result.interface.as_deref(), Some("en0"));
        assert_eq!(
            result.server.as_deref(),
            Some("jptyo5-edge-fx-005.aaplimg.com")
        );
    }

    #[test]
    fn an_unreachable_network_is_an_error_even_though_the_tool_exited_zero() {
        let output = r#"{"cli_options":["-c","-s"],"draft_version":8,"error_code":-1004,
            "error_domain":"NSURLErrorDomain","os_version":"Version 27.0.1"}"#;
        assert_eq!(parse(output), Err("speed_network_error".into()));
    }

    #[test]
    fn missing_fields_stay_unknown_instead_of_zero() {
        let result = parse(r#"{"dl_throughput":1000}"#).unwrap();
        assert_eq!(result.upload_bps, None);
        assert_eq!(result.idle_latency_ms, None);
        assert_eq!(result.bytes_used, None);
        assert_eq!(result.server, None);
    }

    #[test]
    fn output_without_any_throughput_is_not_a_result() {
        assert_eq!(parse(r#"{"base_rtt":20}"#), Err("speed_failed".into()));
        assert_eq!(parse(""), Err("speed_failed".into()));
        assert_eq!(parse("[1,2]"), Err("speed_failed".into()));
        assert_eq!(parse("not json"), Err("speed_failed".into()));
    }

    #[test]
    fn negative_or_non_numeric_values_are_dropped() {
        let result = parse(r#"{"dl_throughput":-5,"ul_throughput":"fast","base_rtt":12}"#);
        assert_eq!(result, Err("speed_failed".into()));
    }

    #[test]
    fn live_lines_from_the_terminal_become_bits_per_second() {
        // Exactly as macOS 27 prints it, erase sequence included.
        let line = "\u{1b}[2KDownlink: 167.391 Mbps, 162 RPM - Uplink: 25.835 Mbps, 204 RPM";
        assert_eq!(
            parse_progress_line(line),
            Some((167_391_000.0, 25_835_000.0))
        );
        // The verbose form names the figure "capacity".
        let verbose = "Downlink: capacity 0.084 Mbps, responsiveness 0 RPM (3.736 KB, 1 flow) - Uplink: capacity 0.000 Mbps, responsiveness 0 RPM (0 B, 0 flows)";
        assert_eq!(parse_progress_line(verbose), None);
    }

    #[test]
    fn summary_and_partial_lines_are_not_readings() {
        assert_eq!(parse_progress_line("==== SUMMARY ===="), None);
        assert_eq!(parse_progress_line("Uplink capacity: 108.865 Mbps"), None);
        assert_eq!(
            parse_progress_line("Downlink: 12.5 Mbps, 0 RPM - Upl"),
            None
        );
        assert_eq!(
            parse_progress_line("Downlink: -1 Mbps - Uplink: 2 Mbps"),
            None
        );
        assert_eq!(
            parse_progress_line("Downlink: NaN Mbps - Uplink: 2 Mbps"),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn readings_split_across_reads_and_redraws_are_forwarded_once_complete() {
        use std::io::Write;
        let (primary, secondary) = open_terminal().unwrap();
        let mut writer = secondary;
        let readings = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = readings.clone();
        let reader = std::thread::spawn(move || {
            read_progress(primary, std::time::Instant::now(), move |p| {
                seen.lock().unwrap().push((p.download_bps, p.upload_bps));
            });
        });
        writer
            .write_all(b"\r\x1b[2K\rDownlink: 1.5 Mbps, 0 RPM - Uplink: 0.000 Mbps, 0 RPM\r\x1b[2K\rDownlink: 2.0 Mb")
            .unwrap();
        writer.flush().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        writer
            .write_all(b"ps, 0 RPM - Uplink: 0.500 Mbps, 3 RPM\r==== SUMMARY ====\r\n")
            .unwrap();
        drop(writer);
        reader.join().unwrap();
        assert_eq!(
            *readings.lock().unwrap(),
            vec![(1_500_000.0, 0.0), (2_000_000.0, 500_000.0)]
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "real network test: uses a few hundred MB of data against Apple's servers"]
    async fn live_speed_test_reports_both_directions_while_it_runs() {
        let readings = std::sync::Arc::new(Mutex::new(Vec::<SpeedTestProgress>::new()));
        let seen = readings.clone();
        let result = run(move |p| seen.lock().unwrap().push(p))
            .await
            .expect("live speed test");
        let readings = readings.lock().unwrap();
        eprintln!(
            "live speed test: {result:?}; {} readings, first {:?}, last {:?}",
            readings.len(),
            readings.first(),
            readings.last()
        );
        assert!(result.download_bps.unwrap_or(0.0) > 0.0);
        assert!(result.upload_bps.unwrap_or(0.0) > 0.0);
        assert!(readings.len() > 20, "expected several readings a second");
        assert!(readings.iter().any(|p| p.download_bps > 0.0));
        assert!(readings.iter().any(|p| p.upload_bps > 0.0));
        assert!(!RUNNING.load(Ordering::SeqCst));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "real network test: starts a run, then cancels it after two seconds"]
    async fn live_cancel_kills_the_tool_and_frees_the_slot() {
        let handle = tokio::spawn(run(|_| {}));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(run(|_| {}).await, Err("speed_busy".into()));
        shutdown();
        assert_eq!(handle.await.unwrap(), Err("speed_cancelled".into()));
        assert!(!RUNNING.load(Ordering::SeqCst));
        let leftover = std::process::Command::new("/usr/bin/pgrep")
            .args(["-x", "networkQuality"])
            .output()
            .unwrap();
        assert!(leftover.stdout.is_empty(), "networkQuality left running");
        let files = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("sayknow-speed-")
            })
            .count();
        assert_eq!(files, 0, "result file left behind");
    }
}
