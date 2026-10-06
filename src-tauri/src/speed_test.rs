//! Internet speed through macOS's own `networkQuality` (Apple's measurement
//! servers, the same test as `networkQuality` in Terminal). No third-party
//! service is contacted and nothing is installed.
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
/// A sequential run printed about 19 KB; anything near this is not a result.
#[cfg(target_os = "macos")]
const OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;

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

#[cfg(target_os = "macos")]
enum Ending {
    Finished(std::io::Result<(std::process::ExitStatus, Vec<u8>)>),
    Cancelled,
    TimedOut,
}

#[cfg(target_os = "macos")]
async fn collect(
    child: &mut tokio::process::Child,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout.take(OUTPUT_LIMIT).read_to_end(&mut output).await?;
    }
    let status = child.wait().await?;
    Ok((status, output))
}

#[cfg(target_os = "macos")]
async fn run() -> Result<SpeedTestResult, String> {
    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("speed_busy".into());
    }
    let _guard = RunGuard;
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    *CANCEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel_tx);

    let mut child = tokio::process::Command::new(TOOL)
        .args(["-c", "-s", "-M", MAX_RUNTIME_SECS])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "speed_unsupported")?;

    let ending = tokio::select! {
        finished = collect(&mut child) => Ending::Finished(finished),
        _ = cancel_rx => Ending::Cancelled,
        _ = tokio::time::sleep(HARD_DEADLINE) => Ending::TimedOut,
    };
    match ending {
        Ending::Finished(Ok((status, output))) => {
            if !status.success() {
                return Err("speed_failed".into());
            }
            parse(&String::from_utf8_lossy(&output))
        }
        Ending::Finished(Err(_)) => {
            let _ = child.kill().await;
            Err("speed_failed".into())
        }
        Ending::Cancelled => {
            let _ = child.kill().await;
            Err("speed_cancelled".into())
        }
        Ending::TimedOut => {
            let _ = child.kill().await;
            Err("speed_timeout".into())
        }
    }
}

#[tauri::command]
pub async fn run_speed_test() -> Result<SpeedTestResult, String> {
    #[cfg(target_os = "macos")]
    {
        run().await
    }
    #[cfg(not(target_os = "macos"))]
    {
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

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "real network test: uses about 200 MB of data against Apple's servers"]
    async fn live_speed_test_reports_both_directions() {
        let result = run().await.expect("live speed test");
        eprintln!("live speed test: {result:?}");
        assert!(result.download_bps.unwrap_or(0.0) > 0.0);
        assert!(result.upload_bps.unwrap_or(0.0) > 0.0);
        assert!(!RUNNING.load(Ordering::SeqCst));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "real network test: starts a run, then cancels it after two seconds"]
    async fn live_cancel_kills_the_tool_and_frees_the_slot() {
        let handle = tokio::spawn(run());
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(run().await, Err("speed_busy".into()));
        shutdown();
        assert_eq!(handle.await.unwrap(), Err("speed_cancelled".into()));
        assert!(!RUNNING.load(Ordering::SeqCst));
        let leftover = std::process::Command::new("/usr/bin/pgrep")
            .args(["-x", "networkQuality"])
            .output()
            .unwrap();
        assert!(leftover.stdout.is_empty(), "networkQuality left running");
    }
}
