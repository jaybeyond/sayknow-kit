// Subscription-agent usage reader.
//
// Claude Code, Codex, and SayKnow CLI all write per-turn token accounting into
// local JSONL session logs. None of them expose a query API, and the
// subscription rate-limit windows (Claude's 5h/weekly) only exist in live API
// response headers — so anything we show has to be derived from those logs.
// We read them directly instead of shelling out to a helper like `ccusage`.
//
// Cost: the logs are large (hundreds of MB after a few weeks) and a full
// re-parse on every tab open would be wasteful. Session files are
// append-only and go immutable once the session ends, so we keep a per-file
// aggregate keyed by (mtime, size) and only re-parse what actually changed.
//
// Bucketing: records carry RFC3339 Zulu timestamps. We bucket by UTC hour
// (the "YYYY-MM-DDTHH" prefix) and let the frontend fold those into local-time
// windows — that keeps the cache timezone-independent and dodges the
// `time` crate's multithreaded local-offset problem.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

const CACHE_FILE: &str = "agent-usage.json";
/// Bump on every parser or aggregate-shape change. Entries are keyed by
/// (mtime, size), so without this a parser fix silently keeps serving the
/// aggregates computed by the old parser for files that never changed —
/// which is exactly how the Codex quota snapshots came back empty.
const CACHE_VERSION: u32 = 3;
/// Session files older than this are ignored outright — the panel only ever
/// shows 30-day windows, and the extra margin keeps month boundaries honest.
const MAX_SCAN_DAYS: u64 = 45;
/// Guard against a runaway log file wedging the scan.
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Bucket {
    pub calls: u64,
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    pub total: u64,
    #[serde(default)]
    pub cost_usd: f64,
}

impl Bucket {
    fn merge(&mut self, o: &Bucket) {
        self.calls += o.calls;
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.total += o.total;
        self.cost_usd += o.cost_usd;
    }
}

/// One quota window as the provider reports it. Codex writes these into every
/// `token_count` event, which makes them the only authoritative limit numbers
/// available offline — Claude Code and SayKnow CLI log no equivalent, so their
/// windows have to be derived from timestamps instead.
#[derive(Clone, Serialize, Deserialize)]
pub struct RateWindow {
    pub used_percent: f64,
    pub window_minutes: u64,
    /// Unix seconds.
    pub resets_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RateLimits {
    /// Timestamp of the event this snapshot came from. A snapshot is only
    /// meaningful next to its capture time: percentages from a month ago say
    /// nothing about the window you're in now.
    pub captured_at: String,
    pub plan_type: Option<String>,
    /// Short window — 300 minutes (5h) in Codex's case.
    pub primary: Option<RateWindow>,
    /// Long window — 10080 minutes (7 days).
    pub secondary: Option<RateWindow>,
}

#[derive(Clone, Serialize, Deserialize)]
struct FileAgg {
    mtime: u64,
    size: u64,
    hours: HashMap<String, Bucket>,
    /// model -> "YYYY-MM-DD" -> tokens. Day-bucketed so the UI can scope the
    /// model breakdown to the same window as the totals it sits under;
    /// a flat lifetime total made retired models look current.
    model_days: HashMap<String, HashMap<String, u64>>,
    last_ts: Option<String>,
    #[serde(default)]
    rate_limits: Option<RateLimits>,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    files: HashMap<String, FileAgg>,
}

#[derive(Serialize)]
pub struct AgentReport {
    pub id: String,
    pub label: String,
    /// Whether the agent's data directory exists at all. `false` means "not
    /// installed / never used on this machine", which the UI must not confuse
    /// with "installed but idle".
    pub detected: bool,
    /// Whether this agent's logs carry a real cost figure. Claude Code and
    /// Codex only record token counts, so the UI hides money for them rather
    /// than inventing a price.
    pub has_cost: bool,
    pub hours: HashMap<String, Bucket>,
    /// model -> "YYYY-MM-DD" -> tokens, for window-scoped breakdowns.
    pub model_days: HashMap<String, HashMap<String, u64>>,
    pub last_ts: Option<String>,
    pub files: usize,
    /// Provider-reported quota windows, when the agent logs them.
    pub rate_limits: Option<RateLimits>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Skc,
    ClaudeCode,
    Codex,
}

struct AgentSpec {
    id: &'static str,
    label: &'static str,
    kind: Kind,
    /// Path relative to $HOME.
    root: &'static str,
    has_cost: bool,
}

const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "skc",
        label: "SayKnow CLI",
        kind: Kind::Skc,
        root: ".skc/agent/sessions",
        has_cost: true,
    },
    AgentSpec {
        id: "claude-code",
        label: "Claude Code",
        kind: Kind::ClaudeCode,
        root: ".claude/projects",
        has_cost: false,
    },
    AgentSpec {
        id: "codex",
        label: "Codex",
        kind: Kind::Codex,
        root: ".codex/sessions",
        has_cost: false,
    },
];

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Depth-limited recursive walk for `*.jsonl` newer than `cutoff`.
fn collect_jsonl(dir: &Path, cutoff: u64, out: &mut Vec<(PathBuf, u64, u64)>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            collect_jsonl(&path, cutoff, out, depth + 1);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let mtime = mtime_secs(&meta);
            if mtime >= cutoff && meta.len() <= MAX_FILE_BYTES {
                out.push((path, mtime, meta.len()));
            }
        }
    }
}

/// `2026-08-19T16:05:01.938Z` -> `2026-08-19T16`. Non-Zulu timestamps are
/// dropped rather than silently bucketed into the wrong hour.
fn hour_key(ts: &str) -> Option<String> {
    if !ts.ends_with('Z') || ts.len() < 13 {
        return None;
    }
    let b = ts.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    Some(ts[..13].to_string())
}

fn as_u64(v: Option<&Value>) -> u64 {
    v.and_then(|x| x.as_u64()).unwrap_or(0)
}

/// One accounted turn pulled out of a log line.
struct Turn {
    ts: String,
    model: Option<String>,
    b: Bucket,
}

fn parse_skc(v: &Value) -> Option<Turn> {
    // { "model": "...", "usage": { input, output, cacheRead, cacheWrite,
    //   totalTokens, cost: { total } }, "ts": "..." }
    let holder = find_usage_holder(v, 0)?;
    let u = holder.get("usage")?.as_object()?;
    let ts = holder
        .get("ts")
        .or_else(|| holder.get("timestamp"))
        .and_then(|x| x.as_str())
        .or_else(|| v.get("ts").and_then(|x| x.as_str()))
        .or_else(|| v.get("timestamp").and_then(|x| x.as_str()))?
        .to_string();
    let input = as_u64(u.get("input"));
    let output = as_u64(u.get("output"));
    let cache_read = as_u64(u.get("cacheRead"));
    let cache_write = as_u64(u.get("cacheWrite"));
    let total = {
        let t = as_u64(u.get("totalTokens"));
        if t > 0 {
            t
        } else {
            input + output + cache_read + cache_write
        }
    };
    if total == 0 {
        return None;
    }
    let cost = u
        .get("cost")
        .and_then(|c| c.get("total"))
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0);
    Some(Turn {
        ts,
        model: holder
            .get("model")
            .and_then(|m| m.as_str())
            .map(str::to_string),
        b: Bucket {
            calls: 1,
            input,
            output,
            cache_read,
            cache_write,
            total,
            cost_usd: cost,
        },
    })
}

/// SKC nests the accounted message at varying depths depending on record type,
/// so walk until we hit the object that owns a `usage` map.
fn find_usage_holder(v: &Value, depth: usize) -> Option<&serde_json::Map<String, Value>> {
    if depth > 6 {
        return None;
    }
    match v {
        Value::Object(map) => {
            if map.get("usage").map(|u| u.is_object()).unwrap_or(false) {
                return Some(map);
            }
            for val in map.values() {
                if let Some(found) = find_usage_holder(val, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_usage_holder(item, depth + 1)),
        _ => None,
    }
}

fn parse_claude(v: &Value) -> Option<Turn> {
    // { "timestamp": "...", "message": { "model": "...", "usage": {
    //   input_tokens, output_tokens, cache_creation_input_tokens,
    //   cache_read_input_tokens } } }
    let msg = v.get("message")?.as_object()?;
    let u = msg.get("usage")?.as_object()?;
    let ts = v.get("timestamp")?.as_str()?.to_string();
    let input = as_u64(u.get("input_tokens"));
    let output = as_u64(u.get("output_tokens"));
    let cache_write = as_u64(u.get("cache_creation_input_tokens"));
    let cache_read = as_u64(u.get("cache_read_input_tokens"));
    let total = input + output + cache_write + cache_read;
    if total == 0 {
        return None;
    }
    let model = msg.get("model").and_then(|m| m.as_str()).map(str::to_string);
    // Claude logs bookkeeping turns under a `<synthetic>` model; they carry no
    // real spend and would pollute the model breakdown.
    if model.as_deref() == Some("<synthetic>") {
        return None;
    }
    Some(Turn {
        ts,
        model,
        b: Bucket {
            calls: 1,
            input,
            output,
            cache_read,
            cache_write,
            total,
            cost_usd: 0.0,
        },
    })
}

fn window_from(v: Option<&Value>) -> Option<RateWindow> {
    let o = v?.as_object()?;
    Some(RateWindow {
        used_percent: o.get("used_percent")?.as_f64()?,
        window_minutes: o.get("window_minutes").and_then(|x| x.as_u64()).unwrap_or(0),
        resets_at: o.get("resets_at").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// Codex attaches the live quota snapshot to every `token_count` event.
fn parse_codex_limits(v: &Value) -> Option<RateLimits> {
    let payload = v.get("payload")?.as_object()?;
    if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
        return None;
    }
    let rl = payload.get("rate_limits")?.as_object()?;
    let primary = window_from(rl.get("primary"));
    let secondary = window_from(rl.get("secondary"));
    if primary.is_none() && secondary.is_none() {
        return None;
    }
    Some(RateLimits {
        captured_at: v.get("timestamp")?.as_str()?.to_string(),
        plan_type: rl
            .get("plan_type")
            .and_then(|p| p.as_str())
            .map(str::to_string),
        primary,
        secondary,
    })
}

fn parse_codex(v: &Value) -> Option<Turn> {
    // { "timestamp": "...", "type": "event_msg",
    //   "payload": { "type": "token_count", "info": { "last_token_usage": {...} } } }
    let payload = v.get("payload")?.as_object()?;
    if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
        return None;
    }
    // `total_token_usage` is cumulative for the session; summing it would
    // multiply the real number by the turn count. `last_token_usage` is the
    // per-event delta, which is what we want.
    let u = payload
        .get("info")?
        .get("last_token_usage")?
        .as_object()?;
    let ts = v.get("timestamp")?.as_str()?.to_string();
    let input = as_u64(u.get("input_tokens"));
    let output = as_u64(u.get("output_tokens"));
    let cache_read = as_u64(u.get("cached_input_tokens"));
    let cache_write = as_u64(u.get("cache_write_input_tokens"));
    let total = {
        let t = as_u64(u.get("total_tokens"));
        if t > 0 {
            t
        } else {
            input + output + cache_read + cache_write
        }
    };
    if total == 0 {
        return None;
    }
    Some(Turn {
        ts,
        model: None,
        b: Bucket {
            calls: 1,
            input,
            output,
            cache_read,
            cache_write,
            total,
            cost_usd: 0.0,
        },
    })
}

/// Cheap prefilter so we only pay for `serde_json` on lines that can possibly
/// carry accounting.
fn line_is_candidate(kind: Kind, line: &str) -> bool {
    match kind {
        Kind::Skc | Kind::ClaudeCode => line.contains("\"usage\""),
        // token_count carries both the usage delta and the quota snapshot.
        Kind::Codex => line.contains("token_count"),
    }
}

fn parse_file(kind: Kind, path: &Path, mtime: u64, size: u64) -> FileAgg {
    let mut agg = FileAgg {
        mtime,
        size,
        hours: HashMap::new(),
        model_days: HashMap::new(),
        last_ts: None,
        rate_limits: None,
    };
    let Ok(file) = fs::File::open(path) else {
        return agg;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line_is_candidate(kind, &line) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if kind == Kind::Codex {
            // A zero-token event still carries a fresh quota snapshot, so this
            // has to happen before the turn is filtered out.
            if let Some(rl) = parse_codex_limits(&v) {
                let newer = agg
                    .rate_limits
                    .as_ref()
                    .map(|prev| rl.captured_at > prev.captured_at)
                    .unwrap_or(true);
                if newer {
                    agg.rate_limits = Some(rl);
                }
            }
        }
        let turn = match kind {
            Kind::Skc => parse_skc(&v),
            Kind::ClaudeCode => parse_claude(&v),
            Kind::Codex => parse_codex(&v),
        };
        let Some(turn) = turn else { continue };
        let Some(key) = hour_key(&turn.ts) else {
            continue;
        };
        agg.hours.entry(key).or_default().merge(&turn.b);
        if let Some(m) = turn.model {
            let day = turn.ts[..10].to_string();
            *agg.model_days.entry(m).or_default().entry(day).or_insert(0) += turn.b.total;
        }
        if agg.last_ts.as_deref().map(|p| turn.ts.as_str() > p).unwrap_or(true) {
            agg.last_ts = Some(turn.ts);
        }
    }
    agg
}

/// Unix seconds -> `YYYY-MM-DDTHH:MM:SSZ`, so snapshots taken from a file's
/// mtime compare and display the same way as Codex's inline timestamps.
fn epoch_to_iso(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant), epoch-shifted to 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn claude_window(v: Option<&Value>, window_minutes: u64) -> Option<RateWindow> {
    let o = v?.as_object()?;
    Some(RateWindow {
        used_percent: o.get("used_percentage")?.as_f64()?,
        window_minutes,
        resets_at: o.get("resets_at").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// Claude Code never writes quota data into its session transcripts, but it
/// does hand the live figures to the status line on stdin
/// (`rate_limits.five_hour` / `.seven_day`). Any status line that caches that
/// payload therefore leaves the real numbers on disk, which is the only way to
/// read them without touching the user's credentials or config.
fn claude_rate_limits(home: &Path) -> Option<RateLimits> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // OMC HUD and similar caching status lines.
    if let Ok(entries) = fs::read_dir(home.join(".claude/hud/cache")) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("stdin.") && name.ends_with(".json") {
                candidates.push(e.path());
            }
        }
    }
    // Documented drop point for users who wire up their own capture.
    candidates.push(home.join(".claude/statusline-input.json"));

    let mut best: Option<(u64, RateLimits)> = None;
    for path in candidates {
        let Ok(meta) = fs::metadata(&path) else { continue };
        let mtime = mtime_secs(&meta);
        if best.as_ref().map(|(t, _)| mtime <= *t).unwrap_or(false) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(rl) = v.get("rate_limits").and_then(|r| r.as_object()) else {
            continue;
        };
        let primary = claude_window(rl.get("five_hour"), 300);
        let secondary = claude_window(rl.get("seven_day"), 10_080);
        if primary.is_none() && secondary.is_none() {
            continue;
        }
        best = Some((
            mtime,
            RateLimits {
                // The payload carries no timestamp of its own, so the file's
                // mtime is when these percentages were true.
                captured_at: epoch_to_iso(mtime),
                plan_type: None,
                primary,
                secondary,
            },
        ));
    }
    best.map(|(_, rl)| rl)
}

fn cache_path(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?;
    let _ = fs::create_dir_all(&dir);
    Some(dir.join(CACHE_FILE))
}

fn load_cache(app: &AppHandle) -> Cache {
    let cache: Cache = cache_path(app)
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if cache.version != CACHE_VERSION {
        return Cache::default();
    }
    cache
}

fn save_cache(app: &AppHandle, cache: &Cache) {
    if let Some(p) = cache_path(app) {
        if let Ok(json) = serde_json::to_string(cache) {
            let _ = fs::write(p, json);
        }
    }
}

pub fn scan(app: &AppHandle) -> Vec<AgentReport> {
    let mut cache = load_cache(app);
    let mut next: HashMap<String, FileAgg> = HashMap::new();
    let cutoff = now_secs().saturating_sub(MAX_SCAN_DAYS * 86_400);
    let home = home_dir();
    let mut reports = Vec::new();

    for spec in AGENTS {
        let root = match &home {
            Some(h) => h.join(spec.root),
            None => PathBuf::new(),
        };
        let detected = root.is_dir();
        let mut files = Vec::new();
        if detected {
            collect_jsonl(&root, cutoff, &mut files, 0);
        }

        let mut hours: HashMap<String, Bucket> = HashMap::new();
        let mut model_days: HashMap<String, HashMap<String, u64>> = HashMap::new();
        let mut last_ts: Option<String> = None;
        let mut rate_limits: Option<RateLimits> = None;

        for (path, mtime, size) in &files {
            let key = path.to_string_lossy().to_string();
            // Session logs are append-only, so an unchanged (mtime, size) pair
            // means the aggregate we already computed is still exact.
            let agg = match cache.files.remove(&key) {
                Some(cached) if cached.mtime == *mtime && cached.size == *size => cached,
                _ => parse_file(spec.kind, path, *mtime, *size),
            };
            for (hk, b) in &agg.hours {
                hours.entry(hk.clone()).or_default().merge(b);
            }
            for (m, days) in &agg.model_days {
                let target = model_days.entry(m.clone()).or_default();
                for (day, t) in days {
                    *target.entry(day.clone()).or_insert(0) += t;
                }
            }
            if let Some(ts) = &agg.last_ts {
                if last_ts.as_deref().map(|p| ts.as_str() > p).unwrap_or(true) {
                    last_ts = Some(ts.clone());
                }
            }
            if let Some(rl) = &agg.rate_limits {
                #[allow(clippy::redundant_clone)]
                let newer = rate_limits
                    .as_ref()
                    .map(|prev| rl.captured_at > prev.captured_at)
                    .unwrap_or(true);
                if newer {
                    rate_limits = Some(rl.clone());
                }
            }
            next.insert(key, agg);
        }

        if spec.kind == Kind::ClaudeCode && rate_limits.is_none() {
            if let Some(h) = &home {
                rate_limits = claude_rate_limits(h);
            }
        }

        reports.push(AgentReport {
            id: spec.id.to_string(),
            label: spec.label.to_string(),
            detected,
            has_cost: spec.has_cost,
            hours,
            model_days,
            last_ts,
            files: files.len(),
            rate_limits,
        });
    }

    // Dropping whatever stayed in `cache.files` evicts entries for files that
    // aged past the cutoff or were deleted, so the cache can't grow forever.
    save_cache(
        app,
        &Cache {
            version: CACHE_VERSION,
            files: next,
        },
    );
    reports
}

/// Scanning is hundreds of MB of disk reads on a cold cache, so keep it off
/// the UI thread.
#[tauri::command]
pub async fn agent_usage(app: AppHandle) -> Result<Vec<AgentReport>, String> {
    tauri::async_runtime::spawn_blocking(move || scan(&app))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte copy of a real Claude Code status-line payload, taken
    /// from ~/.claude/hud/cache/stdin.*.json.
    const CLAUDE_STATUSLINE: &str = r#"{"session_id":"49bfb868","version":"2.1.126","rate_limits":{"five_hour":{"used_percentage":17,"resets_at":1783927200},"seven_day":{"used_percentage":21,"resets_at":1784210400}}}"#;

    /// Byte-for-byte copy of a real Codex token_count event.
    const CODEX_EVENT: &str = r#"{"timestamp":"2026-05-24T07:49:08.867Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":10,"cache_write_input_tokens":5,"output_tokens":20,"reasoning_output_tokens":0,"total_tokens":5174},"last_token_usage":{"input_tokens":40,"cached_input_tokens":4,"cache_write_input_tokens":2,"output_tokens":8,"reasoning_output_tokens":0,"total_tokens":54}},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1779626935},"secondary":{"used_percent":40.0,"window_minutes":10080,"resets_at":1780213735},"credits":null,"plan_type":"prolite"}}}"#;

    /// Byte-for-byte copy of a real SayKnow CLI assistant message.
    const SKC_MESSAGE: &str = r#"{"type":"message","id":"0f12a332","timestamp":"2026-08-17T10:06:01.830Z","message":{"role":"assistant","model":"gpt-5.6-sol","usage":{"input":2287,"output":44,"cacheRead":9984,"cacheWrite":0,"totalTokens":12315,"cost":{"input":0.011435,"output":0.00132,"cacheRead":0.004992,"cacheWrite":0,"total":0.017747}},"timestamp":1755425161830}}"#;

    #[test]
    fn claude_statusline_percentages_survive_parsing() {
        let v: Value = serde_json::from_str(CLAUDE_STATUSLINE).unwrap();
        let rl = v.get("rate_limits").unwrap().as_object().unwrap();
        let five = claude_window(rl.get("five_hour"), 300).unwrap();
        let seven = claude_window(rl.get("seven_day"), 10_080).unwrap();
        assert_eq!(five.used_percent, 17.0);
        assert_eq!(five.window_minutes, 300);
        assert_eq!(five.resets_at, 1_783_927_200);
        assert_eq!(seven.used_percent, 21.0);
        assert_eq!(seven.window_minutes, 10_080);
    }

    #[test]
    fn codex_limits_are_read_from_the_event() {
        let v: Value = serde_json::from_str(CODEX_EVENT).unwrap();
        let rl = parse_codex_limits(&v).unwrap();
        assert_eq!(rl.plan_type.as_deref(), Some("prolite"));
        let p = rl.primary.unwrap();
        assert_eq!(p.used_percent, 12.5);
        assert_eq!(p.window_minutes, 300);
        let s = rl.secondary.unwrap();
        assert_eq!(s.used_percent, 40.0);
        assert_eq!(s.window_minutes, 10_080);
    }

    #[test]
    fn codex_sums_the_delta_not_the_running_total() {
        let v: Value = serde_json::from_str(CODEX_EVENT).unwrap();
        let turn = parse_codex(&v).unwrap();
        // total_token_usage says 5174; last_token_usage says 54. Summing the
        // former across a session multiplies the real figure by the turn count.
        assert_eq!(turn.b.total, 54);
        assert_eq!(turn.b.input, 40);
        assert_eq!(turn.b.cache_read, 4);
    }

    #[test]
    fn skc_reads_tokens_and_cost_from_the_message() {
        let v: Value = serde_json::from_str(SKC_MESSAGE).unwrap();
        let turn = parse_skc(&v).unwrap();
        assert_eq!(turn.b.total, 12_315);
        assert_eq!(turn.b.cache_read, 9_984);
        assert!((turn.b.cost_usd - 0.017747).abs() < 1e-9);
        assert_eq!(turn.model.as_deref(), Some("gpt-5.6-sol"));
        // message.timestamp is a number in real logs, so the record-level
        // RFC3339 string is what has to win.
        assert_eq!(turn.ts, "2026-08-17T10:06:01.830Z");
    }

    #[test]
    fn claude_skips_synthetic_bookkeeping_turns() {
        let real = r#"{"timestamp":"2026-07-13T10:00:00.000Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":30,"cache_read_input_tokens":40}}}"#;
        let synthetic = r#"{"timestamp":"2026-07-13T10:00:00.000Z","message":{"model":"<synthetic>","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let turn = parse_claude(&serde_json::from_str(real).unwrap()).unwrap();
        assert_eq!(turn.b.total, 100);
        assert!(parse_claude(&serde_json::from_str(synthetic).unwrap()).is_none());
    }

    #[test]
    fn hour_keys_reject_non_zulu_timestamps() {
        assert_eq!(hour_key("2026-08-19T16:05:01.938Z").unwrap(), "2026-08-19T16");
        assert!(hour_key("2026-08-19T16:05:01+09:00").is_none());
        assert!(hour_key("nonsense").is_none());
    }

    /// Exercises the exact function the command calls, against a real
    /// directory layout on disk — this is the whole path from "a status line
    /// cached its stdin" to "the panel has a percentage to draw".
    #[test]
    fn claude_limits_are_read_off_the_disk() {
        let base = std::env::temp_dir().join(format!(
            "sayknow-usage-test-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = base.join(".claude/hud/cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("stdin.old.json"), CLAUDE_STATUSLINE).unwrap();

        let read = claude_rate_limits(&base).expect("payload should be found");
        assert_eq!(read.primary.as_ref().unwrap().used_percent, 17.0);
        assert_eq!(read.secondary.as_ref().unwrap().used_percent, 21.0);

        // A newer payload must win over an older one.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            base.join(".claude/statusline-input.json"),
            r#"{"rate_limits":{"five_hour":{"used_percentage":63.5,"resets_at":9999999999},"seven_day":{"used_percentage":88.25,"resets_at":9999999999}}}"#,
        )
        .unwrap();
        let read = claude_rate_limits(&base).expect("payload should be found");
        assert_eq!(read.primary.as_ref().unwrap().used_percent, 63.5);
        assert_eq!(read.secondary.as_ref().unwrap().used_percent, 88.25);

        // Payloads without the field are ignored rather than zeroing the card.
        fs::write(cache.join("stdin.nolimits.json"), r#"{"session_id":"x"}"#).unwrap();
        assert!(claude_rate_limits(&base).is_some());

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn epoch_formats_back_to_the_same_instant() {
        // 1783927200 is the five_hour reset in the captured payload.
        assert_eq!(epoch_to_iso(1_783_927_200), "2026-07-13T07:20:00Z");
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00Z");
    }
}
