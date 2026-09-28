use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, ProcessRefreshKind, System, MINIMUM_CPU_UPDATE_INTERVAL};
use tokio::sync::oneshot;

const COLLECTION_TIMEOUT: Duration = Duration::from_secs(2);
const RESTART_REQUIRED_AFTER: Duration = Duration::from_secs(30);
const NO_PACKAGE_SENSOR: &str = "no_verified_package_sensor";
const SOC_DIE_PROVENANCE: &str = "apple_soc_die_max";
/// Version 2 added `gpu`.
const SCHEMA_VERSION: u32 = 2;
/// How far back the status graphs reach.
const HISTORY_SPAN_MS: u64 = 60 * 60 * 1000;
/// A hard stop on memory should something sample far faster than planned.
const HISTORY_MAX_POINTS: usize = 2_400;
/// Rows in each top-process list.
const TOP_PROCESS_COUNT: usize = 5;
static COALESCED_REQUESTS: AtomicU64 = AtomicU64::new(0);
static COLLECTION_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize)]
pub struct MetricsSnapshot {
    pub schema_version: u32,
    pub sampled_at_ms: u64,
    pub cpu: CpuStatus,
    pub gpu: GpuStatus,
    pub memory: ResourceStatus,
    pub storage: ResourceStatus,
    pub cpu_package_temperature: TemperatureStatus,
    pub battery: BatteryStatus,
    pub network: NetworkStatus,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GpuStatus {
    // Read from the accelerator's own statistics on macOS; other platforms
    // never construct this variant.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Available { percent: f32 },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CpuStatus {
    Available {
        percent: f32,
        system_percent: Option<f32>,
        user_percent: Option<f32>,
        idle_percent: Option<f32>,
        sample_start_ms: u64,
        sample_end_ms: u64,
    },
    WarmingUp {
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ResourceStatus {
    Available {
        total_bytes: u64,
        used_bytes: u64,
        available_bytes: u64,
        sampled_at_ms: u64,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TemperatureStatus {
    // Populated on macOS by the unprivileged AppleVendor die-sensor adapter.
    // Platforms without a trustworthy adapter never construct this variant.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Available {
        celsius: f32,
        sampled_at_ms: u64,
        provenance: String,
        adapter_id: String,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BatteryStatus {
    Available {
        percent: f32,
        is_charging: bool,
        adapter_name: Option<String>,
        max_capacity_percent: Option<f32>,
        cycle_count: Option<u32>,
        temperature_celsius: Option<f32>,
    },
    NotInstalled,
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NetworkStatus {
    Available {
        interface: String,
        ip_address: Option<String>,
        upload_bytes_per_sec: u64,
        download_bytes_per_sec: u64,
    },
    WarmingUp {
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}

type CollectionResult = Result<MetricsSnapshot, String>;

struct Sampler {
    system: System,
    cpu_baseline: Option<(Instant, u64, Option<CpuTicks>)>,
    network: crate::network_metrics::NetworkSampler,
    /// The last storage reading and when it was taken. A volume query is the
    /// second most expensive part of a snapshot and free space moves slowly,
    /// so it is reused for `STORAGE_REFRESH`; the reading keeps its own
    /// `sampled_at_ms`, so a reused one never claims to be newer than it is.
    storage: Option<(Instant, ResourceStatus)>,
}

const STORAGE_REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
struct CpuTicks {
    user: u64,
    system: u64,
    idle: u64,
    nice: u64,
}

impl Sampler {
    fn new() -> Self {
        Self {
            system: System::new(),
            cpu_baseline: None,
            storage: None,
            network: crate::network_metrics::NetworkSampler::new(),
        }
    }
}

struct ActiveAttempt {
    generation: u64,
    waiters: Vec<oneshot::Sender<CollectionResult>>,
}

struct Orphan {
    generation: u64,
    completed: Arc<AtomicBool>,
    timed_out_at: Instant,
    restart_logged: bool,
}

struct ServiceState {
    generation: u64,
    sampler: Arc<Mutex<Sampler>>,
    active: Option<ActiveAttempt>,
    orphan: Option<Orphan>,
}

enum Admission {
    Leader {
        generation: u64,
        sampler: Arc<Mutex<Sampler>>,
    },
    Follower(oneshot::Receiver<CollectionResult>),
}

impl ServiceState {
    fn new() -> Self {
        Self {
            generation: 0,
            sampler: Arc::new(Mutex::new(Sampler::new())),
            active: None,
            orphan: None,
        }
    }

    fn clear_finished_orphan(&mut self) {
        if self
            .orphan
            .as_ref()
            .is_some_and(|orphan| orphan.completed.load(Ordering::Acquire))
        {
            self.orphan = None;
        }
    }

    fn admit(&mut self, now: Instant) -> Result<Admission, &'static str> {
        self.clear_finished_orphan();
        if let Some(orphan) = &mut self.orphan {
            if now.duration_since(orphan.timed_out_at) >= RESTART_REQUIRED_AFTER {
                if !orphan.restart_logged {
                    orphan.restart_logged = true;
                    log::error!(
                        "system metrics worker generation {} is still quarantined; restart required",
                        orphan.generation
                    );
                }
                return Err("collector_restart_required");
            }
            return Err("collector_recovery_in_progress");
        }
        if let Some(active) = &mut self.active {
            let (sender, receiver) = oneshot::channel();
            active.waiters.push(sender);
            COALESCED_REQUESTS.fetch_add(1, Ordering::Relaxed);
            return Ok(Admission::Follower(receiver));
        }
        let generation = self.generation;
        self.active = Some(ActiveAttempt {
            generation,
            waiters: Vec::new(),
        });
        Ok(Admission::Leader {
            generation,
            sampler: self.sampler.clone(),
        })
    }

    fn finish(&mut self, generation: u64, result: &CollectionResult) {
        let Some(active) = self.active.take() else {
            return;
        };
        if active.generation != generation {
            self.active = Some(active);
            return;
        }
        for waiter in active.waiters {
            let _ = waiter.send(result.clone());
        }
    }

    fn timed_out(
        &mut self,
        generation: u64,
        completed: Arc<AtomicBool>,
        now: Instant,
        result: &CollectionResult,
    ) {
        self.finish(generation, result);
        if self.generation != generation {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.sampler = Arc::new(Mutex::new(Sampler::new()));
        self.orphan = Some(Orphan {
            generation,
            completed,
            timed_out_at: now,
            restart_logged: false,
        });
    }
}

/// One point on the status graphs: the figures worth drawing over time, taken
/// from a snapshot. `None` where the snapshot had no reading.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HistoryPoint {
    pub at_ms: u64,
    pub cpu: Option<f32>,
    pub gpu: Option<f32>,
    /// Percent of physical memory in use.
    pub memory: Option<f32>,
    pub temperature: Option<f32>,
    pub upload: Option<u64>,
    pub download: Option<u64>,
}

impl HistoryPoint {
    fn from_snapshot(snapshot: &MetricsSnapshot) -> Self {
        Self {
            at_ms: snapshot.sampled_at_ms,
            cpu: match snapshot.cpu {
                CpuStatus::Available { percent, .. } => Some(percent),
                _ => None,
            },
            gpu: match snapshot.gpu {
                GpuStatus::Available { percent } => Some(percent),
                GpuStatus::Unavailable { .. } => None,
            },
            memory: used_percent(&snapshot.memory),
            temperature: match snapshot.cpu_package_temperature {
                TemperatureStatus::Available { celsius, .. } => Some(celsius),
                TemperatureStatus::Unavailable { .. } => None,
            },
            upload: match snapshot.network {
                NetworkStatus::Available { upload_bytes_per_sec, .. } => Some(upload_bytes_per_sec),
                _ => None,
            },
            download: match snapshot.network {
                NetworkStatus::Available { download_bytes_per_sec, .. } => Some(download_bytes_per_sec),
                _ => None,
            },
        }
    }
}

/// Percent of a resource in use, when it was read.
pub(crate) fn used_percent(status: &ResourceStatus) -> Option<f32> {
    match status {
        ResourceStatus::Available {
            total_bytes,
            used_bytes,
            ..
        } if *total_bytes > 0 => Some((*used_bytes as f64 / *total_bytes as f64 * 100.0) as f32),
        _ => None,
    }
}

/// The last hour of samples, whoever asked for them: the open panel every
/// few seconds, or the background monitor while it is closed.
#[derive(Default)]
struct History {
    points: VecDeque<HistoryPoint>,
    latest: Option<MetricsSnapshot>,
}

impl History {
    fn record(&mut self, snapshot: &MetricsSnapshot) {
        // Coalesced callers hand back the same snapshot, and a collection
        // that finished late is older than one already kept.
        if self
            .points
            .back()
            .is_some_and(|last| last.at_ms >= snapshot.sampled_at_ms)
        {
            return;
        }
        self.points.push_back(HistoryPoint::from_snapshot(snapshot));
        let cutoff = snapshot.sampled_at_ms.saturating_sub(HISTORY_SPAN_MS);
        while self
            .points
            .front()
            .is_some_and(|first| first.at_ms < cutoff)
            || self.points.len() > HISTORY_MAX_POINTS
        {
            self.points.pop_front();
        }
        self.latest = Some(snapshot.clone());
    }

    fn since(&self, since_ms: u64) -> Vec<HistoryPoint> {
        self.points
            .iter()
            .filter(|point| point.at_ms > since_ms)
            .copied()
            .collect()
    }
}

/// A process worth naming in the status panel.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProcessUsage {
    pub pid: u32,
    pub name: String,
    /// Share of one core, as Activity Monitor counts it: a busy multithreaded
    /// process reads above 100.
    pub cpu_percent: f32,
    /// Resident memory.
    pub memory_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TopProcesses {
    Available {
        by_cpu: Vec<ProcessUsage>,
        by_memory: Vec<ProcessUsage>,
    },
    /// CPU use is a difference between two readings; the first has nothing
    /// to compare against.
    WarmingUp,
}

/// Its own `System`, apart from the snapshot sampler: walking every process
/// costs far more than the rest of a snapshot, so it only happens while the
/// list is on screen.
struct ProcessSampler {
    system: System,
    /// Process-table walks so far, and when the last one ended.
    looks: u32,
    refreshed_at: Option<Instant>,
}

impl ProcessSampler {
    fn new() -> Self {
        Self {
            system: System::new(),
            looks: 0,
            refreshed_at: None,
        }
    }

    fn refresh(&mut self) {
        self.system
            .refresh_processes_specifics(ProcessRefreshKind::new().with_cpu().with_memory());
        self.looks = self.looks.saturating_add(1);
        self.refreshed_at = Some(Instant::now());
    }

    fn sample(&mut self) -> TopProcesses {
        // sysinfo records a process's CPU time on its second sighting and only
        // measures against it on the third (checked on this Mac: every
        // process reads 0% for two walks). The first call takes the first two
        // walks back to back, so the list is ready on the next call.
        if self.looks == 0 {
            self.refresh();
        }
        let previous = self.refreshed_at;
        self.refresh();
        let too_soon = previous
            .zip(self.refreshed_at)
            .is_none_or(|(before, now)| now.duration_since(before) < MINIMUM_CPU_UPDATE_INTERVAL);
        if self.looks < 3 || too_soon {
            return TopProcesses::WarmingUp;
        }
        let all: Vec<ProcessUsage> = self
            .system
            .processes()
            .iter()
            .map(|(pid, process)| ProcessUsage {
                pid: pid.as_u32(),
                name: process.name().to_string(),
                cpu_percent: if process.cpu_usage().is_finite() {
                    process.cpu_usage().max(0.0)
                } else {
                    0.0
                },
                memory_bytes: process.memory(),
            })
            .collect();
        TopProcesses::Available {
            by_cpu: top_by(&all, |p| p.cpu_percent as f64),
            by_memory: top_by(&all, |p| p.memory_bytes as f64),
        }
    }
}

/// The heaviest `TOP_PROCESS_COUNT` processes by `key`, heaviest first, with
/// ties broken by pid so the list does not reshuffle between refreshes.
fn top_by(all: &[ProcessUsage], key: impl Fn(&ProcessUsage) -> f64) -> Vec<ProcessUsage> {
    let mut sorted: Vec<&ProcessUsage> = all.iter().filter(|p| key(p) > 0.0).collect();
    sorted.sort_by(|a, b| key(b).total_cmp(&key(a)).then(a.pid.cmp(&b.pid)));
    sorted.into_iter().take(TOP_PROCESS_COUNT).cloned().collect()
}

#[derive(Clone)]
pub struct SystemMetricsService {
    state: Arc<Mutex<ServiceState>>,
    history: Arc<Mutex<History>>,
    processes: Arc<Mutex<ProcessSampler>>,
}

impl SystemMetricsService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ServiceState::new())),
            history: Arc::new(Mutex::new(History::default())),
            processes: Arc::new(Mutex::new(ProcessSampler::new())),
        }
    }

    /// Take a snapshot and keep it in the history.
    pub async fn sample(&self) -> CollectionResult {
        let result = collect_metrics(self.state.clone()).await;
        if let (Ok(snapshot), Ok(mut history)) = (&result, self.history.lock()) {
            history.record(snapshot);
        }
        result
    }

    /// When the newest kept snapshot was taken.
    pub fn last_sampled_ms(&self) -> Option<u64> {
        self.history
            .lock()
            .ok()?
            .points
            .back()
            .map(|point| point.at_ms)
    }

    pub fn latest(&self) -> Option<MetricsSnapshot> {
        self.history.lock().ok()?.latest.clone()
    }

    pub fn history_since(&self, since_ms: u64) -> Vec<HistoryPoint> {
        self.history
            .lock()
            .map(|history| history.since(since_ms))
            .unwrap_or_default()
    }
}

struct CompletionFlag(Arc<AtomicBool>);

impl Drop for CompletionFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A snapshot at `at_ms` with nothing read yet and CPU still warming up, for
/// tests that fill in only the figures they are about.
#[cfg(test)]
pub(crate) fn test_snapshot(at_ms: u64) -> MetricsSnapshot {
    let mut snapshot = unavailable_snapshot("test");
    snapshot.sampled_at_ms = at_ms;
    snapshot.cpu = CpuStatus::WarmingUp {
        reason: "test".to_string(),
    };
    snapshot
}

#[cfg(test)]
fn unavailable_snapshot(reason: &str) -> MetricsSnapshot {
    let sampled_at_ms = now_ms();
    MetricsSnapshot {
        schema_version: SCHEMA_VERSION,
        sampled_at_ms,
        cpu: CpuStatus::Unavailable {
            reason: reason.to_string(),
        },
        gpu: GpuStatus::Unavailable {
            reason: reason.to_string(),
        },
        memory: ResourceStatus::Unavailable {
            reason: reason.to_string(),
        },
        storage: ResourceStatus::Unavailable {
            reason: reason.to_string(),
        },
        cpu_package_temperature: TemperatureStatus::Unavailable {
            reason: NO_PACKAGE_SENSOR.to_string(),
        },
        battery: BatteryStatus::Unavailable {
            reason: reason.to_string(),
        },
        network: NetworkStatus::Unavailable {
            reason: reason.to_string(),
        },
    }
}

fn resource_status(total: u64, available: u64, sampled_at_ms: u64) -> ResourceStatus {
    if total == 0 || available > total {
        return ResourceStatus::Unavailable {
            reason: "invalid_capacity".to_string(),
        };
    }
    ResourceStatus::Available {
        total_bytes: total,
        used_bytes: total - available,
        available_bytes: available,
        sampled_at_ms,
    }
}

#[cfg(target_os = "windows")]
fn system_root() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }

    let mut buffer = vec![0u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

#[cfg(not(target_os = "windows"))]
fn system_root() -> Option<PathBuf> {
    Some(PathBuf::from("/"))
}

#[cfg(target_os = "windows")]
fn normalize_mount(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

#[cfg(not(target_os = "windows"))]
fn normalize_mount(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('/').to_string()
}

#[cfg(target_os = "windows")]
fn mount_matches_root(mount: &Path, root: &Path) -> bool {
    let mount = normalize_mount(mount);
    let root = normalize_mount(root);
    !mount.is_empty()
        && (root == mount
            || root
                .strip_prefix(&mount)
                .is_some_and(|suffix| suffix.starts_with('\\')))
}

#[cfg(not(target_os = "windows"))]
fn mount_matches_root(mount: &Path, root: &Path) -> bool {
    mount == root
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DiskCandidate {
    mount: PathBuf,
    total: u64,
    available: u64,
    removable: bool,
}

fn select_system_volume(
    candidates: impl IntoIterator<Item = DiskCandidate>,
    root: &Path,
    sampled_at_ms: u64,
) -> ResourceStatus {
    let mut unique = HashSet::new();
    let mut matches = candidates
        .into_iter()
        .filter(|candidate| {
            !candidate.removable
                && mount_matches_root(&candidate.mount, root)
                && candidate.total > 0
                && candidate.available <= candidate.total
        })
        .filter(|candidate| {
            unique.insert((
                normalize_mount(&candidate.mount),
                candidate.total,
                candidate.available,
            ))
        });
    let Some(candidate) = matches.next() else {
        return ResourceStatus::Unavailable {
            reason: "system_volume_unavailable".to_string(),
        };
    };
    if matches.next().is_some() {
        return ResourceStatus::Unavailable {
            reason: "ambiguous_system_volume".to_string(),
        };
    }
    resource_status(candidate.total, candidate.available, sampled_at_ms)
}

fn collect(sampler: &Arc<Mutex<Sampler>>) -> CollectionResult {
    let sampled_at_ms = now_ms();
    let mut sampler = sampler
        .lock()
        .map_err(|_| "sampler_unavailable".to_string())?;

    let cpu = match sampler.cpu_baseline {
        None => {
            sampler.cpu_baseline = Some((Instant::now(), sampled_at_ms, read_cpu_ticks()));
            prime_global_cpu(&mut sampler.system);
            CpuStatus::WarmingUp {
                reason: "baseline_pending".to_string(),
            }
        }
        Some((baseline_at, _, _)) if baseline_at.elapsed() < MINIMUM_CPU_UPDATE_INTERVAL => {
            CpuStatus::WarmingUp {
                reason: "minimum_interval_pending".to_string(),
            }
        }
        Some((baseline_at, baseline_ms, previous_ticks)) => {
            let ticks = read_cpu_ticks();
            let end = Instant::now();
            let elapsed_ms = end.duration_since(baseline_at).as_millis();
            let sample_end_ms = u64::try_from(elapsed_ms)
                .ok()
                .filter(|elapsed| *elapsed > 0)
                .and_then(|elapsed| baseline_ms.checked_add(elapsed));
            let breakdown = cpu_breakdown(previous_ticks, ticks);
            sampler.cpu_baseline = sample_end_ms.map(|end_ms| (end, end_ms, ticks));
            match (sample_end_ms, breakdown) {
                (Some(sample_end_ms), Some((system, user, idle))) => {
                    let percent = (system + user).clamp(0.0, 100.0);
                    CpuStatus::Available {
                        percent,
                        system_percent: Some(system),
                        user_percent: Some(user),
                        idle_percent: Some(idle),
                        sample_start_ms: baseline_ms,
                        sample_end_ms,
                    }
                }
                // Without tick counters there is no system/user split to report,
                // but the total is still a real measurement.
                (Some(sample_end_ms), None) => match global_cpu_percent(&mut sampler.system) {
                    Some(percent) => CpuStatus::Available {
                        percent,
                        system_percent: None,
                        user_percent: None,
                        idle_percent: Some(100.0 - percent),
                        sample_start_ms: baseline_ms,
                        sample_end_ms,
                    },
                    None => CpuStatus::Unavailable {
                        reason: "invalid_cpu_sample".to_string(),
                    },
                },
                _ => CpuStatus::Unavailable {
                    reason: "invalid_cpu_sample".to_string(),
                },
            }
        }
    };

    let memory = macos_memory_status(sampled_at_ms).unwrap_or_else(|| {
        sampler.system.refresh_memory();
        resource_status(
            sampler.system.total_memory(),
            sampler.system.available_memory(),
            sampled_at_ms,
        )
    });

    let storage = match &sampler.storage {
        Some((read_at, status)) if read_at.elapsed() < STORAGE_REFRESH => status.clone(),
        _ => {
            let status = read_storage(sampled_at_ms);
            sampler.storage = Some((Instant::now(), status.clone()));
            status
        }
    };

    Ok(MetricsSnapshot {
        schema_version: SCHEMA_VERSION,
        sampled_at_ms,
        cpu,
        gpu: gpu_status(),
        memory,
        storage,
        cpu_package_temperature: temperature_status(sampled_at_ms),
        battery: battery_status(),
        network: match sampler.network.sample() {
            crate::network_metrics::NetworkStatus::Available {
                interface,
                ip_address,
                upload_bytes_per_sec,
                download_bytes_per_sec,
            } => NetworkStatus::Available {
                interface,
                ip_address,
                upload_bytes_per_sec,
                download_bytes_per_sec,
            },
            crate::network_metrics::NetworkStatus::WarmingUp { reason } => {
                NetworkStatus::WarmingUp { reason }
            }
            crate::network_metrics::NetworkStatus::Unavailable { reason } => {
                NetworkStatus::Unavailable { reason }
            }
        },
    })
}

/// The system volume's capacity: APFS container figures on macOS, the disk
/// holding the system root elsewhere.
fn read_storage(sampled_at_ms: u64) -> ResourceStatus {
    macos_storage_status(sampled_at_ms).unwrap_or_else(|| {
        system_root()
            .map(|root| {
                let disks = Disks::new_with_refreshed_list();
                select_system_volume(
                    disks.list().iter().map(|disk| DiskCandidate {
                        mount: disk.mount_point().to_path_buf(),
                        total: disk.total_space(),
                        available: disk.available_space(),
                        removable: disk.is_removable(),
                    }),
                    &root,
                    sampled_at_ms,
                )
            })
            .unwrap_or_else(|| ResourceStatus::Unavailable {
                reason: "system_volume_unavailable".to_string(),
            })
    })
}

fn battery_status() -> BatteryStatus {
    #[cfg(target_os = "macos")]
    {
        match crate::battery_macos::read_battery() {
            Some(crate::battery_macos::BatteryReading::NotInstalled) => BatteryStatus::NotInstalled,
            Some(crate::battery_macos::BatteryReading::Installed {
                percent,
                is_charging,
                adapter_name,
                max_capacity_percent,
                cycle_count,
                temperature_celsius,
            }) => BatteryStatus::Available {
                percent,
                is_charging,
                adapter_name,
                max_capacity_percent,
                cycle_count,
                temperature_celsius,
            },
            None => BatteryStatus::Unavailable {
                reason: "iokit_unavailable".to_string(),
            },
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        BatteryStatus::Unavailable {
            reason: "unsupported_platform".to_string(),
        }
    }
}

fn cpu_breakdown(previous: Option<CpuTicks>, current: Option<CpuTicks>) -> Option<(f32, f32, f32)> {
    let (previous, current) = (previous?, current?);
    let user = current.user.saturating_sub(previous.user);
    let system = current.system.saturating_sub(previous.system);
    let idle = current.idle.saturating_sub(previous.idle);
    let nice = current.nice.saturating_sub(previous.nice);
    let total = user + system + idle + nice;
    if total == 0 {
        return None;
    }
    let total = total as f32;
    Some((
        (system as f32 / total) * 100.0,
        ((user + nice) as f32 / total) * 100.0,
        (idle as f32 / total) * 100.0,
    ))
}

/// Per-state tick counters are a mach interface, so every other platform gets
/// the total from sysinfo instead of a permanently unavailable CPU card.
#[cfg(target_os = "macos")]
fn prime_global_cpu(_system: &mut System) {}

#[cfg(not(target_os = "macos"))]
fn prime_global_cpu(system: &mut System) {
    system.refresh_cpu_usage();
}

#[cfg(target_os = "macos")]
fn global_cpu_percent(_system: &mut System) -> Option<f32> {
    None
}

#[cfg(not(target_os = "macos"))]
fn global_cpu_percent(system: &mut System) -> Option<f32> {
    // sysinfo measures against its own previous refresh, which the baseline
    // primed at least MINIMUM_CPU_UPDATE_INTERVAL ago.
    system.refresh_cpu_usage();
    let percent = system.global_cpu_info().cpu_usage();
    percent.is_finite().then(|| percent.clamp(0.0, 100.0))
}

#[cfg(target_os = "macos")]
// libc deprecates mach_host_self in favour of the mach2 crate; the symbol is
// still exported and correct, and one host port is not worth a dependency.
#[allow(deprecated)]
fn read_cpu_ticks() -> Option<CpuTicks> {
    unsafe {
        let mut info = std::mem::MaybeUninit::<libc::host_cpu_load_info>::uninit();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let kr = libc::host_statistics(
            libc::mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            info.as_mut_ptr() as *mut libc::integer_t,
            &mut count,
        );
        if kr != libc::KERN_SUCCESS {
            return None;
        }
        let info = info.assume_init();
        Some(CpuTicks {
            user: info.cpu_ticks[libc::CPU_STATE_USER as usize] as u64,
            system: info.cpu_ticks[libc::CPU_STATE_SYSTEM as usize] as u64,
            idle: info.cpu_ticks[libc::CPU_STATE_IDLE as usize] as u64,
            nice: info.cpu_ticks[libc::CPU_STATE_NICE as usize] as u64,
        })
    }
}

#[cfg(not(target_os = "macos"))]
fn read_cpu_ticks() -> Option<CpuTicks> {
    None
}

/// RunCat / SystemInfoKit formula: app + wired + compressed, not sysinfo's
/// `used = total - available`. Activity Monitor matches this much more closely
/// than `sysinfo::System::used_memory()`.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // mach_host_self, as above.
fn macos_memory_status(sampled_at_ms: u64) -> Option<ResourceStatus> {
    unsafe {
        let mut stats = std::mem::MaybeUninit::<libc::vm_statistics64>::uninit();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let kr = libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            stats.as_mut_ptr() as *mut libc::integer_t,
            &mut count,
        );
        if kr != libc::KERN_SUCCESS {
            return None;
        }
        let stats = stats.assume_init();
        let page_size = libc::vm_page_size;
        if page_size == 0 {
            return None;
        }
        let mut total: u64 = 0;
        let mut total_len = std::mem::size_of::<u64>();
        let name = std::ffi::CString::new("hw.memsize").ok()?;
        if libc::sysctlbyname(
            name.as_ptr(),
            &mut total as *mut u64 as *mut libc::c_void,
            &mut total_len,
            std::ptr::null_mut(),
            0,
        ) != 0
            || total == 0
        {
            return None;
        }
        let page = page_size as u64;
        let wired = stats.wire_count as u64;
        let compressed = stats.compressor_page_count as u64;
        // Activity Monitor "Memory Used" ≈ App Memory + Wired + Compressed.
        // App Memory is anonymous/internal pages, not active+inactive-cached.
        let app = stats.internal_page_count as u64;
        let used_pages = app.saturating_add(wired).saturating_add(compressed);
        let used = used_pages.saturating_mul(page).min(total);
        Some(resource_status(total, total - used, sampled_at_ms))
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_memory_status(_sampled_at_ms: u64) -> Option<ResourceStatus> {
    None
}

#[cfg(test)]
fn memory_formula_pages(app: u64, wired: u64, compressed: u64) -> u64 {
    app.saturating_add(wired).saturating_add(compressed)
}


/// System Settings / RunCat: APFS container capacity, not the sealed system
/// snapshot `df` reports for `/`.
#[cfg(target_os = "macos")]
fn macos_storage_status(sampled_at_ms: u64) -> Option<ResourceStatus> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{
        NSNumber, NSString, NSURL, NSURLVolumeAvailableCapacityForImportantUsageKey,
        NSURLVolumeTotalCapacityKey,
    };

    let url = NSURL::fileURLWithPath(&NSString::from_str("/"));
    let mut total_obj: Option<Retained<AnyObject>> = None;
    let mut available_obj: Option<Retained<AnyObject>> = None;
    unsafe {
        url.getResourceValue_forKey_error(&mut total_obj, NSURLVolumeTotalCapacityKey)
            .ok()?;
        url.getResourceValue_forKey_error(
            &mut available_obj,
            NSURLVolumeAvailableCapacityForImportantUsageKey,
        )
        .ok()?;
    }
    let total = total_obj
        .as_ref()
        .and_then(|obj| obj.downcast_ref::<NSNumber>())
        .map(|n| n.as_u64())?;
    let available = available_obj
        .as_ref()
        .and_then(|obj| obj.downcast_ref::<NSNumber>())
        .map(|n| n.as_u64())?;
    Some(resource_status(total, available, sampled_at_ms))
}

#[cfg(not(target_os = "macos"))]
fn macos_storage_status(_sampled_at_ms: u64) -> Option<ResourceStatus> {
    None
}

/// Apple Silicon's accelerator driver publishes its own utilization. Nothing
/// else here has a GPU reading, and it is reported as unavailable rather than
/// as an idle 0%.
fn gpu_status() -> GpuStatus {
    #[cfg(target_os = "macos")]
    if let Some(percent) = crate::iokit_macos::gpu_utilization_percent() {
        return GpuStatus::Available { percent };
    }
    GpuStatus::Unavailable {
        reason: "no_gpu_statistics".to_string(),
    }
}

/// macOS reads the SoC die sensors through the unprivileged AppleVendor HID
/// temperature page. Every other platform, and every macOS machine where the
/// adapter cannot produce a trustworthy reading, stays explicitly unavailable.
fn temperature_status(sampled_at_ms: u64) -> TemperatureStatus {
    #[cfg(target_os = "macos")]
    {
        if let Some(celsius) = crate::thermal_macos::read_die_celsius() {
            return TemperatureStatus::Available {
                celsius,
                sampled_at_ms,
                provenance: SOC_DIE_PROVENANCE.to_string(),
                adapter_id: crate::thermal_macos::ADAPTER_ID.to_string(),
            };
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = sampled_at_ms;
    TemperatureStatus::Unavailable {
        reason: NO_PACKAGE_SENSOR.to_string(),
    }
}

fn complete_attempt(state: &Arc<Mutex<ServiceState>>, generation: u64, result: &CollectionResult) {
    if let Ok(mut state) = state.lock() {
        state.finish(generation, result);
    }
}

fn quarantine_attempt(
    state: &Arc<Mutex<ServiceState>>,
    generation: u64,
    completed: Arc<AtomicBool>,
    result: &CollectionResult,
) {
    if let Ok(mut service) = state.lock() {
        service.timed_out(generation, completed, Instant::now(), result);
    }
    let watchdog_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(RESTART_REQUIRED_AFTER).await;
        if let Ok(mut service) = watchdog_state.lock() {
            service.clear_finished_orphan();
            if let Some(orphan) = &mut service.orphan {
                if orphan.generation == generation && !orphan.restart_logged {
                    orphan.restart_logged = true;
                    log::error!(
                        "system metrics worker generation {generation} remains quarantined after {}s; restart required",
                        RESTART_REQUIRED_AFTER.as_secs()
                    );
                }
            }
        }
    });
}

async fn collect_metrics_with<Collector>(
    state: Arc<Mutex<ServiceState>>,
    queue_timeout: Duration,
    collection_timeout: Duration,
    collector: Collector,
) -> CollectionResult
where
    Collector: FnOnce(Arc<Mutex<Sampler>>) -> CollectionResult + Send + 'static,
{
    let admission = {
        let mut state = state
            .lock()
            .map_err(|_| "collector_state_unavailable".to_string())?;
        state.admit(Instant::now())
    };
    let (generation, sampler) = match admission {
        Ok(Admission::Follower(receiver)) => {
            return receiver
                .await
                .unwrap_or_else(|_| Err("collector_result_unavailable".to_string()));
        }
        Err(reason) => {
            log::debug!("system metrics collection skipped: {reason}");
            return Err(reason.to_string());
        }
        Ok(Admission::Leader {
            generation,
            sampler,
        }) => (generation, sampler),
    };

    let completed = Arc::new(AtomicBool::new(false));
    let completion_for_worker = CompletionFlag(completed.clone());
    let (started_sender, started_receiver) = oneshot::channel();
    let mut task = tokio::task::spawn_blocking(move || {
        let _completion = completion_for_worker;
        let _ = started_sender.send(());
        collector(sampler)
    });

    if tokio::time::timeout(queue_timeout, started_receiver)
        .await
        .is_err()
    {
        task.abort();
        let result = Err("collector_queue_timeout".to_string());
        quarantine_attempt(&state, generation, completed.clone(), &result);
        log::warn!("system metrics worker did not start within the queue deadline");
        return result;
    }

    let started = Instant::now();
    let result = match tokio::time::timeout(collection_timeout, &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("collection_failed".to_string()),
        Err(_) => {
            let result = Err("collection_timeout".to_string());
            let timeout_count = COLLECTION_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
            quarantine_attempt(&state, generation, completed.clone(), &result);
            log::warn!(
                "system metrics collection timed out after {}ms (timeouts={timeout_count}, coalesced={})",
                collection_timeout.as_millis(),
                COALESCED_REQUESTS.load(Ordering::Relaxed)
            );
            return result;
        }
    };
    complete_attempt(&state, generation, &result);
    if result.is_err() {
        log::warn!("system metrics worker failed");
    } else if started.elapsed() >= Duration::from_millis(500) {
        log::info!(
            "system metrics collection completed slowly: {}ms",
            started.elapsed().as_millis()
        );
    }
    result
}

async fn collect_metrics(state: Arc<Mutex<ServiceState>>) -> CollectionResult {
    collect_metrics_with(state, COLLECTION_TIMEOUT, COLLECTION_TIMEOUT, |sampler| {
        collect(&sampler)
    })
    .await
}

#[tauri::command]
pub async fn get_system_metrics(
    service: tauri::State<'_, SystemMetricsService>,
) -> CollectionResult {
    service.sample().await
}

/// Graph points newer than `since_ms` (all of the last hour when omitted), so
/// an open panel only fetches what it has not drawn yet.
#[tauri::command]
pub fn get_system_history(
    service: tauri::State<'_, SystemMetricsService>,
    since_ms: Option<u64>,
) -> Vec<HistoryPoint> {
    service.history_since(since_ms.unwrap_or(0))
}

/// The heaviest processes by CPU and by memory. Walking the process table is
/// the costly part of the status panel, so it runs off the UI thread, and a
/// call that arrives while one is still walking is answered as warming up
/// instead of queueing behind it.
#[tauri::command]
pub async fn get_top_processes(
    service: tauri::State<'_, SystemMetricsService>,
) -> Result<TopProcesses, String> {
    let processes = service.processes.clone();
    tauri::async_runtime::spawn_blocking(move || match processes.try_lock() {
        Ok(mut sampler) => sampler.sample(),
        Err(_) => TopProcesses::WarmingUp,
    })
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_cpu_sample_is_warming_up() {
        let sampler = Arc::new(Mutex::new(Sampler::new()));
        assert!(matches!(
            collect(&sampler).unwrap().cpu,
            CpuStatus::WarmingUp { .. }
        ));
    }

    #[test]
    fn activity_monitor_memory_used_is_app_plus_wired_plus_compressed() {
        assert_eq!(memory_formula_pages(10, 3, 2), 15);
    }

    #[test]
    fn invalid_capacities_are_unavailable() {
        assert!(matches!(
            resource_status(0, 0, 1),
            ResourceStatus::Unavailable { .. }
        ));
        assert!(matches!(
            resource_status(40, 100, 1),
            ResourceStatus::Unavailable { .. }
        ));
        assert!(matches!(
            resource_status(100, 40, 1),
            ResourceStatus::Available { used_bytes: 60, .. }
        ));
    }

    #[test]
    fn serialization_uses_discriminated_state() {
        let value = serde_json::to_value(unavailable_snapshot("busy")).unwrap();
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        assert_eq!(value["gpu"]["state"], "unavailable");
        assert_eq!(value["cpu"]["state"], "unavailable");
        assert_eq!(value["cpu"]["reason"], "busy");
        assert_eq!(
            value["cpu_package_temperature"]["reason"],
            NO_PACKAGE_SENSOR
        );
        assert!(value["cpu_package_temperature"].get("celsius").is_none());
    }

    #[test]
    fn live_second_sample_has_real_resource_data() {
        let sampler = Arc::new(Mutex::new(Sampler::new()));
        let first = collect(&sampler).unwrap();
        assert!(matches!(first.cpu, CpuStatus::WarmingUp { .. }));

        // A live sample can come back short once on a loaded machine. The
        // contract is that a running app reaches real data, not that the very
        // first retry does, so this retries the way the app itself would.
        let mut sample = None;
        for _ in 0..5 {
            std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
            let snapshot = collect(&sampler).unwrap();
            let complete = matches!(snapshot.cpu, CpuStatus::Available { .. })
                && matches!(snapshot.memory, ResourceStatus::Available { .. })
                && matches!(snapshot.storage, ResourceStatus::Available { .. });
            sample = Some(snapshot);
            if complete {
                break;
            }
        }
        let second = sample.expect("a live sample");
        match second.cpu {
            CpuStatus::Available {
                percent,
                system_percent,
                user_percent,
                idle_percent,
                ..
            } => {
                assert!((0.0..=100.0).contains(&percent), "cpu total out of range: {percent}");
                let idle = idle_percent.expect("idle share");
                if cfg!(target_os = "macos") {
                    let system = system_percent.expect("system ticks");
                    let user = user_percent.expect("user ticks");
                    assert!((percent - (system + user)).abs() < 0.01);
                    assert!((system + user + idle - 100.0).abs() < 0.2);
                    eprintln!(
                        "cpu: total={percent:.1} system={system:.1} user={user:.1} idle={idle:.1}"
                    );
                } else {
                    // Only mach exposes the per-state counters; elsewhere the
                    // total is real and the split is absent rather than faked.
                    assert_eq!(system_percent, None);
                    assert_eq!(user_percent, None);
                    assert!((percent + idle - 100.0).abs() < 0.01);
                    eprintln!("cpu: total={percent:.1} idle={idle:.1}");
                }
            }
            other => panic!("expected available cpu, got {other:?}"),
        }
        match second.memory {
            ResourceStatus::Available {
                total_bytes,
                used_bytes,
                available_bytes,
                ..
            } => {
                assert!(total_bytes > 0);
                assert_eq!(used_bytes + available_bytes, total_bytes);
                assert!(used_bytes < total_bytes, "used memory cannot be the whole machine");
                eprintln!(
                    "memory: used={} / total={} ({:.1}%)",
                    used_bytes,
                    total_bytes,
                    used_bytes as f64 / total_bytes as f64 * 100.0
                );
            }
            other => panic!("expected available memory, got {other:?}"),
        }
        match second.storage {
            ResourceStatus::Available {
                total_bytes,
                used_bytes,
                ..
            } => {
                eprintln!(
                    "storage: used={} / total={} ({:.2} GB / {:.2} GB)",
                    used_bytes,
                    total_bytes,
                    used_bytes as f64 / 1_000_000_000.0,
                    total_bytes as f64 / 1_000_000_000.0
                );
                assert!(used_bytes <= total_bytes, "used storage exceeds the volume");
                // A host-specific capacity would only assert the machine the
                // test runs on; a boot volume is never this small anywhere.
                assert!(
                    total_bytes > 10_000_000_000,
                    "expected a boot volume capacity, got {total_bytes}"
                );
            }
            other => panic!("expected available storage, got {other:?}"),
        }
        match second.cpu_package_temperature {
            TemperatureStatus::Available {
                celsius,
                ref provenance,
                ref adapter_id,
                ..
            } => {
                assert_eq!(provenance, SOC_DIE_PROVENANCE);
                assert!(!adapter_id.is_empty());
                assert!((5.0..=125.0).contains(&celsius));
                // Reaching an available die reading anywhere else means the
                // adapter answered on a platform that has no such sensor.
                if !cfg!(target_os = "macos") {
                    panic!("only macOS has a die adapter");
                }
            }
            TemperatureStatus::Unavailable { ref reason } => {
                assert_eq!(reason, NO_PACKAGE_SENSOR);
            }
        }
    }

    #[test]
    fn concurrent_admission_shares_the_leader_result() {
        let mut state = ServiceState::new();
        let leader_generation = match state.admit(Instant::now()).unwrap() {
            Admission::Leader { generation, .. } => generation,
            Admission::Follower(_) => panic!("first caller must lead"),
        };
        let mut follower = match state.admit(Instant::now()).unwrap() {
            Admission::Follower(receiver) => receiver,
            Admission::Leader { .. } => panic!("second caller must follow"),
        };
        let result = Err("shared_failure".to_string());
        state.finish(leader_generation, &result);
        assert!(matches!(
            follower.try_recv(),
            Ok(Err(reason)) if reason == "shared_failure"
        ));
    }

    #[test]
    fn orphan_gate_blocks_replacement_until_exit() {
        let mut state = ServiceState::new();
        let generation = match state.admit(Instant::now()).unwrap() {
            Admission::Leader { generation, .. } => generation,
            Admission::Follower(_) => panic!("first caller must lead"),
        };
        let mut follower = match state.admit(Instant::now()).unwrap() {
            Admission::Follower(receiver) => receiver,
            Admission::Leader { .. } => panic!("second caller must follow"),
        };
        let completed = Arc::new(AtomicBool::new(false));
        let timeout = Err("collection_timeout".to_string());
        state.timed_out(generation, completed.clone(), Instant::now(), &timeout);

        assert!(matches!(
            follower.try_recv(),
            Ok(Err(reason)) if reason == "collection_timeout"
        ));
        assert!(matches!(
            state.admit(Instant::now()),
            Err("collector_recovery_in_progress")
        ));
        assert!(state.active.is_none());

        completed.store(true, Ordering::Release);
        let (replacement_generation, replacement) = match state.admit(Instant::now()).unwrap() {
            Admission::Leader {
                generation,
                sampler,
            } => (generation, sampler),
            Admission::Follower(_) => panic!("replacement caller must lead"),
        };
        assert_eq!(replacement_generation, generation + 1);
        assert!(matches!(
            collect(&replacement).unwrap().cpu,
            CpuStatus::WarmingUp { .. }
        ));
    }

    #[test]
    fn physical_timeout_is_shared_and_blocks_replacement_until_exit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let state = Arc::new(Mutex::new(ServiceState::new()));
            let starts = Arc::new(AtomicU64::new(0));
            let (release_sender, release_receiver) = std::sync::mpsc::channel();

            let leader_state = state.clone();
            let leader_starts = starts.clone();
            let leader = tokio::spawn(collect_metrics_with(
                leader_state,
                Duration::from_secs(2),
                Duration::from_millis(20),
                move |_| {
                    leader_starts.fetch_add(1, Ordering::SeqCst);
                    release_receiver.recv().unwrap();
                    Ok(unavailable_snapshot("late_result"))
                },
            ));
            while starts.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }

            let follower_starts = starts.clone();
            let follower = tokio::spawn(collect_metrics_with(
                state.clone(),
                Duration::from_secs(2),
                Duration::from_millis(20),
                move |_| {
                    follower_starts.fetch_add(1, Ordering::SeqCst);
                    Ok(unavailable_snapshot("must_not_start"))
                },
            ));
            assert!(matches!(
                leader.await.unwrap(),
                Err(reason) if reason == "collection_timeout"
            ));
            assert!(matches!(
                follower.await.unwrap(),
                Err(reason) if reason == "collection_timeout"
            ));
            assert_eq!(starts.load(Ordering::SeqCst), 1);

            let blocked_starts = starts.clone();
            let blocked = collect_metrics_with(
                state.clone(),
                Duration::from_secs(2),
                Duration::from_millis(20),
                move |_| {
                    blocked_starts.fetch_add(1, Ordering::SeqCst);
                    Ok(unavailable_snapshot("must_not_start"))
                },
            )
            .await;
            assert!(matches!(
                blocked,
                Err(reason) if reason == "collector_recovery_in_progress"
            ));
            assert_eq!(starts.load(Ordering::SeqCst), 1);

            release_sender.send(()).unwrap();
            loop {
                let completed = state
                    .lock()
                    .unwrap()
                    .orphan
                    .as_ref()
                    .is_some_and(|orphan| orphan.completed.load(Ordering::Acquire));
                if completed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }

            let replacement_starts = starts.clone();
            let replacement = collect_metrics_with(
                state,
                Duration::from_secs(2),
                Duration::from_secs(2),
                move |sampler| {
                    replacement_starts.fetch_add(1, Ordering::SeqCst);
                    collect(&sampler)
                },
            )
            .await
            .unwrap();
            assert_eq!(starts.load(Ordering::SeqCst), 2);
            assert!(matches!(replacement.cpu, CpuStatus::WarmingUp { .. }));
        });
    }

    #[test]
    fn long_lived_orphan_requires_restart() {
        let mut state = ServiceState::new();
        state.orphan = Some(Orphan {
            generation: 7,
            completed: Arc::new(AtomicBool::new(false)),
            timed_out_at: Instant::now() - RESTART_REQUIRED_AFTER,
            restart_logged: false,
        });
        assert!(matches!(
            state.admit(Instant::now()),
            Err("collector_restart_required")
        ));
        assert!(state.orphan.as_ref().unwrap().restart_logged);
    }

    #[test]
    fn storage_deduplicates_identical_root_records() {
        let root = system_root().unwrap();
        let candidates = vec![
            DiskCandidate {
                mount: root.clone(),
                total: 100,
                available: 40,
                removable: false,
            },
            DiskCandidate {
                mount: root.clone(),
                total: 100,
                available: 40,
                removable: false,
            },
        ];
        assert!(matches!(
            select_system_volume(candidates, &root, 1),
            ResourceStatus::Available { used_bytes: 60, .. }
        ));
    }

    #[test]
    fn storage_rejects_ambiguous_or_removable_root_records() {
        let root = system_root().unwrap();
        assert!(matches!(
            select_system_volume(
                vec![
                    DiskCandidate {
                        mount: root.clone(),
                        total: 100,
                        available: 40,
                        removable: false,
                    },
                    DiskCandidate {
                        mount: root.clone(),
                        total: 200,
                        available: 80,
                        removable: false,
                    },
                ],
                &root,
                1,
            ),
            ResourceStatus::Unavailable { .. }
        ));
        assert!(matches!(
            select_system_volume(
                vec![DiskCandidate {
                    mount: root.clone(),
                    total: 100,
                    available: 40,
                    removable: true,
                }],
                &root,
                1,
            ),
            ResourceStatus::Unavailable { .. }
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_mount_matching_is_case_insensitive_and_bounded() {
        assert!(mount_matches_root(
            Path::new(r"C:\"),
            Path::new(r"c:\Windows\System32")
        ));
        assert!(!mount_matches_root(
            Path::new(r"C:\Win"),
            Path::new(r"C:\Windows\System32")
        ));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_root_match_is_exact() {
        assert!(mount_matches_root(Path::new("/"), Path::new("/")));
        assert!(!mount_matches_root(
            Path::new("/Volumes/Data"),
            Path::new("/")
        ));
    }

    fn snapshot_at(at_ms: u64, cpu: f32) -> MetricsSnapshot {
        let mut snapshot = unavailable_snapshot("test");
        snapshot.sampled_at_ms = at_ms;
        snapshot.cpu = CpuStatus::Available {
            percent: cpu,
            system_percent: None,
            user_percent: None,
            idle_percent: None,
            sample_start_ms: at_ms.saturating_sub(3_000),
            sample_end_ms: at_ms,
        };
        snapshot.memory = ResourceStatus::Available {
            total_bytes: 16,
            used_bytes: 12,
            available_bytes: 4,
            sampled_at_ms: at_ms,
        };
        snapshot
    }

    #[test]
    fn a_history_point_keeps_what_the_graphs_draw() {
        let point = HistoryPoint::from_snapshot(&snapshot_at(10_000, 42.5));
        assert_eq!(point.at_ms, 10_000);
        assert_eq!(point.cpu, Some(42.5));
        assert_eq!(point.memory, Some(75.0));
        // Unread figures stay unread rather than turning into zeros.
        assert_eq!((point.gpu, point.temperature, point.download), (None, None, None));
    }

    #[test]
    fn history_keeps_one_hour_in_order_and_ignores_repeats() {
        let mut history = History::default();
        history.record(&snapshot_at(1_000, 1.0));
        history.record(&snapshot_at(1_000, 99.0)); // a coalesced caller's copy
        history.record(&snapshot_at(500, 99.0)); // a late, older collection
        history.record(&snapshot_at(4_000, 2.0));
        assert_eq!(history.since(0).iter().map(|p| p.cpu).collect::<Vec<_>>(), vec![Some(1.0), Some(2.0)]);
        assert_eq!(history.since(1_000).len(), 1);
        assert_eq!(history.latest.as_ref().map(|s| s.sampled_at_ms), Some(4_000));

        history.record(&snapshot_at(4_000 + HISTORY_SPAN_MS, 3.0));
        let kept: Vec<u64> = history.since(0).iter().map(|p| p.at_ms).collect();
        assert_eq!(kept, vec![4_000, 4_000 + HISTORY_SPAN_MS]);
    }

    #[test]
    fn history_never_outgrows_its_cap() {
        let mut history = History::default();
        for i in 0..(HISTORY_MAX_POINTS as u64 + 50) {
            history.record(&snapshot_at(1_000 + i, 1.0));
        }
        assert_eq!(history.points.len(), HISTORY_MAX_POINTS);
        assert_eq!(history.points.back().map(|p| p.at_ms), Some(1_000 + HISTORY_MAX_POINTS as u64 + 49));
    }

    #[test]
    fn top_lists_are_heaviest_first_and_stable() {
        let process = |pid: u32, cpu: f32, memory: u64| ProcessUsage {
            pid,
            name: format!("p{pid}"),
            cpu_percent: cpu,
            memory_bytes: memory,
        };
        let mut all: Vec<ProcessUsage> = (1..=8).map(|pid| process(pid, 0.0, 10)).collect();
        all.push(process(20, 150.0, 5));
        all.push(process(21, 30.0, 900));
        all.push(process(22, 30.0, 1));
        let by_cpu: Vec<u32> = top_by(&all, |p| p.cpu_percent as f64).iter().map(|p| p.pid).collect();
        // Idle processes are not "top" of anything; equal loads keep pid order.
        assert_eq!(by_cpu, vec![20, 21, 22]);
        let by_memory: Vec<u32> = top_by(&all, |p| p.memory_bytes as f64).iter().map(|p| p.pid).collect();
        assert_eq!(by_memory, vec![21, 1, 2, 3, 4]);
    }

    #[test]
    fn live_top_processes_after_a_second_look() {
        let mut sampler = ProcessSampler::new();
        let started = Instant::now();
        assert!(matches!(sampler.sample(), TopProcesses::WarmingUp));
        let first = started.elapsed();
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        let TopProcesses::Available { by_cpu, by_memory } = sampler.sample() else {
            panic!("second look should have CPU figures");
        };
        eprintln!(
            "live processes: first walk {}ms, second {}ms; top cpu {:?}; top memory {:?}",
            first.as_millis(),
            started.elapsed().as_millis(),
            by_cpu.first().map(|p| (&p.name, p.cpu_percent)),
            by_memory.first().map(|p| (&p.name, p.memory_bytes)),
        );
        // This test run itself is resident and busy, so neither list can be
        // empty — an empty CPU list is the "every process reads 0%" bug.
        assert!(!by_memory.is_empty());
        assert!(!by_cpu.is_empty());
        assert!(by_memory.windows(2).all(|w| w[0].memory_bytes >= w[1].memory_bytes));
    }

    #[test]
    fn live_snapshot_cost() {
        let sampler = Arc::new(Mutex::new(Sampler::new()));
        let _ = collect(&sampler);
        std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
        let started = Instant::now();
        let snapshot = collect(&sampler).unwrap();
        eprintln!(
            "live snapshot: {}ms, gpu {:?}",
            started.elapsed().as_millis(),
            snapshot.gpu
        );
        assert_eq!(snapshot.schema_version, SCHEMA_VERSION);
    }
}
