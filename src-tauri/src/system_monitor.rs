//! A background watch over the system snapshot, for the two things that have
//! to work while the popover is closed: the menu bar readout and the alerts
//! the user turned on.
//!
//! Nothing is sampled here unless one of them is on. A snapshot costs tens of
//! milliseconds (the SoC temperature sensors dominate), which is fine every
//! few seconds while the user asked for it and not fine as a silent default.
//! The status panel samples for itself while it is open, and every sample,
//! whoever took it, lands in the same history the alerts read.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tauri::{AppHandle, Manager};

use crate::system_metrics::{
    used_percent, BatteryStatus, CpuStatus, GpuStatus, HistoryPoint, MetricsSnapshot,
    ResourceStatus, SystemMetricsService, TemperatureStatus,
};

/// How often the watch wakes to see whether a sample is due.
const TICK: Duration = Duration::from_secs(1);
/// Sampling interval while the popover is closed.
const BACKGROUND_INTERVAL_MS: u64 = 5_000;
/// A load has to hold this long before it is worth a notification.
const SUSTAIN_MS: u64 = 2 * 60 * 1000;
/// Samples further apart than this mean the Mac slept or sampling paused; a
/// run across such a gap is not evidence of a sustained load.
const MAX_GAP_MS: u64 = 30_000;
/// The same alert is not repeated sooner than this, even if the condition
/// clears and returns in between.
const REFIRE_AFTER_MS: u64 = 30 * 60 * 1000;
/// Decimal gigabyte, as Finder counts free space.
const GB: f64 = 1_000_000_000.0;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Readout {
    #[default]
    Off,
    Cpu,
    Memory,
    Gpu,
    Temperature,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    Cpu,
    Memory,
    Temperature,
    Storage,
    Battery,
}

const ALERT_KINDS: [AlertKind; 5] = [
    AlertKind::Cpu,
    AlertKind::Memory,
    AlertKind::Temperature,
    AlertKind::Storage,
    AlertKind::Battery,
];

/// Notification text in the user's language; `{value}` in the body is
/// replaced with the reading that tripped the alert.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AlertText {
    pub title: String,
    pub body: String,
}

/// Pushed from the webview whenever the settings change, like the tray labels.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub readout: Readout,
    pub alerts: Vec<AlertKind>,
    pub text: HashMap<AlertKind, AlertText>,
}

impl MonitorConfig {
    /// The readout that can actually be shown. Only macOS draws a title next
    /// to a tray icon; elsewhere a readout would keep sampling for nothing.
    fn effective_readout(&self) -> Readout {
        if cfg!(target_os = "macos") {
            self.readout
        } else {
            Readout::Off
        }
    }

    fn is_idle(&self) -> bool {
        self.effective_readout() == Readout::Off && self.alerts.is_empty()
    }
}

#[derive(Default)]
pub struct MonitorState {
    config: Mutex<MonitorConfig>,
}

impl MonitorState {
    fn config(&self) -> MonitorConfig {
        self.config.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

#[tauri::command]
pub fn set_system_monitor_config(state: tauri::State<'_, MonitorState>, config: MonitorConfig) {
    log::info!(
        "system monitor: readout={:?} alerts={:?}",
        config.readout,
        config.alerts
    );
    if let Ok(mut current) = state.config.lock() {
        *current = config;
    }
}

/// What the menu bar should show next to the icon.
#[derive(Debug, PartialEq)]
enum Title {
    Clear,
    /// Nothing new to say (the reading is still warming up).
    Keep,
    Show(String),
}

fn readout_title(readout: Readout, snapshot: &MetricsSnapshot) -> Title {
    match readout {
        Readout::Off => Title::Clear,
        Readout::Cpu => match snapshot.cpu {
            CpuStatus::Available { percent, .. } => Title::Show(format!("CPU {percent:.0}%")),
            CpuStatus::WarmingUp { .. } => Title::Keep,
            CpuStatus::Unavailable { .. } => Title::Show("CPU —".to_string()),
        },
        Readout::Memory => match used_percent(&snapshot.memory) {
            Some(percent) => Title::Show(format!("MEM {percent:.0}%")),
            None => Title::Show("MEM —".to_string()),
        },
        Readout::Gpu => match snapshot.gpu {
            GpuStatus::Available { percent } => Title::Show(format!("GPU {percent:.0}%")),
            GpuStatus::Unavailable { .. } => Title::Show("GPU —".to_string()),
        },
        Readout::Temperature => match snapshot.cpu_package_temperature {
            TemperatureStatus::Available { celsius, .. } => Title::Show(format!("{celsius:.0}°C")),
            TemperatureStatus::Unavailable { .. } => Title::Show("—°C".to_string()),
        },
    }
}

/// Whether `holds` was true for every sample across the last `SUSTAIN_MS`
/// before `latest_at`. The history has to reach back that far with no gap
/// long enough to hide a dip; a few hot samples after a wake are not a
/// sustained load.
fn sustained(
    points: &[HistoryPoint],
    latest_at: u64,
    holds: impl Fn(&HistoryPoint) -> Option<bool>,
) -> bool {
    let start = latest_at.saturating_sub(SUSTAIN_MS);
    let window: Vec<&HistoryPoint> = points
        .iter()
        .filter(|p| p.at_ms <= latest_at && p.at_ms + MAX_GAP_MS >= start)
        .collect();
    let Some(from) = window.iter().rposition(|p| p.at_ms <= start) else {
        return false;
    };
    let window = &window[from..];
    window.last().is_some_and(|p| p.at_ms == latest_at)
        && window.windows(2).all(|w| w[1].at_ms - w[0].at_ms <= MAX_GAP_MS)
        && window.iter().all(|p| holds(p) == Some(true))
}

/// Where a condition stands: past its alert threshold, back below the level
/// that re-arms it, or in between (no change, so it does not flap).
#[derive(Debug, PartialEq)]
enum Level {
    Over(String),
    Under,
    Between,
}

fn level(kind: AlertKind, latest: &MetricsSnapshot, history: &[HistoryPoint]) -> Level {
    let at = latest.sampled_at_ms;
    let latest_point = history.iter().rev().find(|p| p.at_ms == at);
    let sustained_level = |value: Option<f32>, over: f32, rearm: f32, over_holds: &dyn Fn(&HistoryPoint) -> Option<f32>, unit: &str| {
        let Some(value) = value else {
            return Level::Between;
        };
        if sustained(history, at, |p| over_holds(p).map(|v| v >= over)) {
            Level::Over(format!("{value:.0}{unit}"))
        } else if value < rearm {
            Level::Under
        } else {
            Level::Between
        }
    };
    match kind {
        AlertKind::Cpu => sustained_level(latest_point.and_then(|p| p.cpu), 90.0, 80.0, &|p| p.cpu, "%"),
        AlertKind::Memory => {
            sustained_level(latest_point.and_then(|p| p.memory), 90.0, 85.0, &|p| p.memory, "%")
        }
        AlertKind::Temperature => sustained_level(
            latest_point.and_then(|p| p.temperature),
            100.0,
            90.0,
            &|p| p.temperature,
            "°C",
        ),
        AlertKind::Storage => match latest.storage {
            ResourceStatus::Available { available_bytes, .. } => {
                let free = available_bytes as f64 / GB;
                if free < 10.0 {
                    Level::Over(format!("{free:.1} GB"))
                } else if free > 15.0 {
                    Level::Under
                } else {
                    Level::Between
                }
            }
            ResourceStatus::Unavailable { .. } => Level::Between,
        },
        AlertKind::Battery => match latest.battery {
            BatteryStatus::Available {
                percent,
                is_charging,
                ..
            } => {
                if !is_charging && percent <= 15.0 {
                    Level::Over(format!("{percent:.0}%"))
                } else if is_charging || percent > 20.0 {
                    Level::Under
                } else {
                    Level::Between
                }
            }
            _ => Level::Between,
        },
    }
}

#[derive(Default)]
struct Armed {
    /// The condition is past its threshold and has been reported (or held
    /// back by the refire limit); it re-arms once back under.
    tripped: bool,
    last_fired_ms: Option<u64>,
}

/// Which alerts have fired, so each one notifies once per episode.
#[derive(Default)]
struct AlertBook {
    state: HashMap<AlertKind, Armed>,
}

impl AlertBook {
    /// Alerts to send now, with the reading that tripped each.
    fn evaluate(
        &mut self,
        enabled: &[AlertKind],
        latest: &MetricsSnapshot,
        history: &[HistoryPoint],
        now_ms: u64,
    ) -> Vec<(AlertKind, String)> {
        let mut fired = Vec::new();
        for kind in ALERT_KINDS {
            if !enabled.contains(&kind) {
                // Turned off: forget it, so turning it back on starts fresh.
                self.state.remove(&kind);
                continue;
            }
            let armed = self.state.entry(kind).or_default();
            match level(kind, latest, history) {
                Level::Over(value) => {
                    if !armed.tripped
                        && armed
                            .last_fired_ms
                            .is_none_or(|at| now_ms.saturating_sub(at) >= REFIRE_AFTER_MS)
                    {
                        armed.last_fired_ms = Some(now_ms);
                        fired.push((kind, value));
                    }
                    armed.tripped = true;
                }
                Level::Under => armed.tripped = false,
                Level::Between => {}
            }
        }
        fired
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn set_tray_title(app: &AppHandle, title: &str) {
    if let Some(tray) = app.tray_by_id("sayknow-tray") {
        // tray-icon ignores `None` on macOS and keeps the old title, so an
        // empty string is how the readout is taken down.
        if let Err(e) = tray.set_title(Some(title)) {
            log::warn!("system monitor: tray title not set: {e}");
        }
    }
}

fn notify(app: &AppHandle, config: &MonitorConfig, kind: AlertKind, value: &str) {
    use tauri_plugin_notification::NotificationExt;
    let (title, body) = match config.text.get(&kind) {
        Some(text) if !text.title.is_empty() => (text.title.clone(), text.body.replace("{value}", value)),
        _ => ("SayKnow Kit".to_string(), value.to_string()),
    };
    log::info!("system monitor: {kind:?} alert at {value}");
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        log::warn!("system monitor: notification failed: {e}");
    }
}

/// Start the watch. It idles (one config read a second) until the user turns
/// on a readout or an alert.
pub fn spawn(app: AppHandle) {
    let spawned = std::thread::Builder::new()
        .name("system-monitor".to_string())
        .spawn(move || {
            let service = app.state::<SystemMetricsService>().inner().clone();
            let mut book = AlertBook::default();
            let mut shown: Option<String> = None;
            let mut evaluated_ms = 0;
            loop {
                std::thread::sleep(TICK);
                let config = app.state::<MonitorState>().config();
                if config.is_idle() {
                    if shown.take().is_some() {
                        set_tray_title(&app, "");
                    }
                    book = AlertBook::default();
                    continue;
                }
                let due = service
                    .last_sampled_ms()
                    .is_none_or(|at| now_ms().saturating_sub(at) >= BACKGROUND_INTERVAL_MS);
                if due {
                    let _ = tauri::async_runtime::block_on(service.sample());
                }
                let Some(latest) = service.latest() else {
                    continue;
                };
                if latest.sampled_at_ms <= evaluated_ms {
                    continue;
                }
                evaluated_ms = latest.sampled_at_ms;

                match readout_title(config.effective_readout(), &latest) {
                    Title::Clear => {
                        if shown.take().is_some() {
                            set_tray_title(&app, "");
                        }
                    }
                    Title::Show(text) if shown.as_deref() != Some(text.as_str()) => {
                        set_tray_title(&app, &text);
                        shown = Some(text);
                    }
                    Title::Show(_) | Title::Keep => {}
                }

                let history = service
                    .history_since(latest.sampled_at_ms.saturating_sub(SUSTAIN_MS + MAX_GAP_MS));
                for (kind, value) in book.evaluate(&config.alerts, &latest, &history, now_ms()) {
                    notify(&app, &config, kind, &value);
                }
            }
        });
    if let Err(e) = spawned {
        log::error!("system monitor: thread not started: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_metrics::test_snapshot;

    const T0: u64 = 1_790_000_000_000;

    fn point(at_ms: u64, cpu: f32) -> HistoryPoint {
        HistoryPoint {
            at_ms,
            cpu: Some(cpu),
            gpu: None,
            memory: Some(50.0),
            temperature: Some(60.0),
            upload: None,
            download: None,
        }
    }

    fn cpu_snapshot(at_ms: u64, percent: f32) -> MetricsSnapshot {
        let mut snapshot = test_snapshot(at_ms);
        snapshot.cpu = CpuStatus::Available {
            percent,
            system_percent: None,
            user_percent: None,
            idle_percent: None,
            sample_start_ms: at_ms - 5_000,
            sample_end_ms: at_ms,
        };
        snapshot
    }

    /// Samples every five seconds from `from` to `to` inclusive.
    fn run(from: u64, to: u64, cpu: f32) -> Vec<HistoryPoint> {
        (from..=to).step_by(5_000).map(|at| point(at, cpu)).collect()
    }

    #[test]
    fn readouts_are_short_and_honest_about_missing_readings() {
        let snapshot = cpu_snapshot(T0, 23.4);
        assert_eq!(readout_title(Readout::Cpu, &snapshot), Title::Show("CPU 23%".into()));
        assert_eq!(readout_title(Readout::Off, &snapshot), Title::Clear);
        // test_snapshot has no GPU or temperature reading: a dash, not 0.
        assert_eq!(readout_title(Readout::Gpu, &snapshot), Title::Show("GPU —".into()));
        assert_eq!(readout_title(Readout::Temperature, &snapshot), Title::Show("—°C".into()));
        let warming = test_snapshot(T0);
        assert_eq!(readout_title(Readout::Cpu, &warming), Title::Keep);
    }

    #[test]
    fn a_load_counts_only_once_it_has_held_for_the_whole_window() {
        let latest = T0 + SUSTAIN_MS;
        let hot = |p: &HistoryPoint| p.cpu.map(|c| c >= 90.0);
        assert!(sustained(&run(T0, latest, 95.0), latest, hot));
        // Two minutes minus one sample: the history does not reach back.
        assert!(!sustained(&run(T0 + 5_000, latest, 95.0), latest, hot));
        // One cool sample in the middle breaks the run.
        let mut dipped = run(T0, latest, 95.0);
        dipped[10].cpu = Some(40.0);
        assert!(!sustained(&dipped, latest, hot));
        // A sleep-sized gap is not two minutes of load.
        let mut gapped = run(T0, T0 + 10_000, 95.0);
        gapped.extend(run(latest - 10_000, latest, 95.0));
        assert!(!sustained(&gapped, latest, hot));
    }

    #[test]
    fn an_alert_fires_once_per_episode_and_rearms_below_the_lower_line() {
        let mut book = AlertBook::default();
        let enabled = [AlertKind::Cpu];
        let mut history = run(T0, T0 + SUSTAIN_MS, 95.0);
        let mut at = T0 + SUSTAIN_MS;

        let fired = book.evaluate(&enabled, &cpu_snapshot(at, 95.0), &history, at);
        assert_eq!(fired, vec![(AlertKind::Cpu, "95%".to_string())]);
        // Still hot: no repeat.
        at += 5_000;
        history.push(point(at, 96.0));
        assert!(book.evaluate(&enabled, &cpu_snapshot(at, 96.0), &history, at).is_empty());
        // 85% is between the lines: not re-armed.
        at += 5_000;
        history.push(point(at, 85.0));
        assert!(book.evaluate(&enabled, &cpu_snapshot(at, 85.0), &history, at).is_empty());
        assert!(book.state[&AlertKind::Cpu].tripped);
        // Below 80% re-arms it...
        at += 5_000;
        history.push(point(at, 30.0));
        book.evaluate(&enabled, &cpu_snapshot(at, 30.0), &history, at);
        assert!(!book.state[&AlertKind::Cpu].tripped);
        // ...but a new episode inside the refire window stays quiet.
        let again = at + SUSTAIN_MS + 5_000;
        let recent = run(at + 5_000, again, 97.0);
        assert!(book.evaluate(&enabled, &cpu_snapshot(again, 97.0), &recent, again).is_empty());
        // After it, the next episode is reported.
        let later = at + REFIRE_AFTER_MS + SUSTAIN_MS;
        let recent = run(later - SUSTAIN_MS, later, 97.0);
        book.evaluate(&enabled, &cpu_snapshot(later - 5_000, 30.0), &run(later - 5_000, later - 5_000, 30.0), later - 5_000);
        let fired = book.evaluate(&enabled, &cpu_snapshot(later, 97.0), &recent, later);
        assert_eq!(fired, vec![(AlertKind::Cpu, "97%".to_string())]);
    }

    #[test]
    fn disabled_alerts_never_fire_and_forget_their_state() {
        let mut book = AlertBook::default();
        let at = T0 + SUSTAIN_MS;
        let history = run(T0, at, 99.0);
        assert!(book.evaluate(&[], &cpu_snapshot(at, 99.0), &history, at).is_empty());
        assert!(book.state.is_empty());
    }

    #[test]
    fn storage_and_battery_alert_on_the_latest_reading() {
        let mut snapshot = test_snapshot(T0);
        snapshot.storage = ResourceStatus::Available {
            total_bytes: 500_000_000_000,
            used_bytes: 491_500_000_000,
            available_bytes: 8_500_000_000,
            sampled_at_ms: T0,
        };
        assert_eq!(level(AlertKind::Storage, &snapshot, &[]), Level::Over("8.5 GB".into()));
        snapshot.battery = BatteryStatus::Available {
            percent: 12.0,
            is_charging: false,
            adapter_name: None,
            max_capacity_percent: None,
            cycle_count: None,
            temperature_celsius: None,
        };
        assert_eq!(level(AlertKind::Battery, &snapshot, &[]), Level::Over("12%".into()));
        // Plugged in, a low battery is on its way up: nothing to warn about.
        snapshot.battery = BatteryStatus::Available {
            percent: 12.0,
            is_charging: true,
            adapter_name: None,
            max_capacity_percent: None,
            cycle_count: None,
            temperature_celsius: None,
        };
        assert_eq!(level(AlertKind::Battery, &snapshot, &[]), Level::Under);
    }

    #[test]
    fn the_config_the_webview_sends_is_understood() {
        let raw = r#"{"readout":"temperature","alerts":["cpu","battery"],"text":{"cpu":{"title":"CPU","body":"CPU {value}"}}}"#;
        let config: MonitorConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(config.readout, Readout::Temperature);
        assert_eq!(config.alerts, vec![AlertKind::Cpu, AlertKind::Battery]);
        assert_eq!(config.text[&AlertKind::Cpu].body, "CPU {value}");
        assert!(!config.is_idle());
        assert!(serde_json::from_str::<MonitorConfig>("{}").unwrap().is_idle());
    }
}
