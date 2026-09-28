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
const CACHE_VERSION: u32 = 4;
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
/// available offline — Claude Code and SayKnow CLI log no equivalent, so
/// Claude's windows come from a status-line cache or the Claude app's own
/// history, and SayKnow CLI's have to be derived from timestamps instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RateWindow {
    pub used_percent: f64,
    pub window_minutes: u64,
    /// Unix seconds; 0 when the source does not say when the window renews.
    pub resets_at: i64,
    /// `resets_at` was worked out from a history of readings rather than
    /// reported, so the UI must not present it as exact.
    #[serde(default)]
    pub resets_estimated: bool,
    /// The model family a window is limited to, when it is not the whole plan
    /// (the Claude app tracks separate weekly allowances for Opus and Sonnet).
    #[serde(default)]
    pub scope: Option<String>,
}

/// Where a set of limits was read from. It decides what the UI can promise:
/// how fresh the numbers are, and how the user gets newer ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// Written by the agent into its own session log (Codex).
    #[default]
    SessionLog,
    /// A Claude Code status line's cached stdin.
    StatusLine,
    /// The Claude desktop app's plan-usage history.
    ClaudeApp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// Further windows limited to one model family.
    #[serde(default)]
    pub scoped: Vec<RateWindow>,
    #[serde(default)]
    pub source: LimitSource,
}

/// Sign-in state, when it can be established without touching a secret.
/// Codex keeps its OAuth tokens in a plaintext `auth.json`, so the `exp`
/// claim is readable by decoding the JWT body — no signature check, no
/// credential use, and the token itself never leaves the parser. Claude Code
/// stores the equivalent in the macOS Keychain, and a menubar utility has no
/// business prompting for that, so its state stays unreported.
#[derive(Clone, Serialize)]
pub struct AuthState {
    pub expires_at: String,
    pub expired: bool,
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
    /// Sign-in state where it is knowable from plaintext on disk.
    pub auth: Option<AuthState>,
    /// True when session data exists outside the scan window, so "no records"
    /// can be told apart from "older than we look".
    pub has_older_data: bool,
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
    // { "type": "message", "timestamp": "...", "message": { "model": "...",
    //   "usage": { input, output, cacheRead, cacheWrite, totalTokens,
    //   cost: { total } } } }
    //
    // Only `message.usage` counts. Tool results embed the *tool provider's*
    // own accounting further down (generate_image and web_search put an
    // {inputTokens, outputTokens, totalTokens} object under
    // message.details[.response]), which is a different meter entirely —
    // SayKnow CLI doesn't even price it. A recursive search for "the first
    // object with a usage map" swept those in as if they were turn usage.
    let holder = v.get("message")?.as_object()?;
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
        resets_estimated: false,
        scope: None,
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
        scoped: Vec::new(),
        source: LimitSource::SessionLog,
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
        resets_estimated: false,
        scope: None,
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
                scoped: Vec::new(),
                source: LimitSource::StatusLine,
            },
        ));
    }
    best.map(|(_, rl)| rl)
}

/// Where the Claude desktop app keeps its plan limits, relative to $HOME.
const CLAUDE_APP_HISTORY: &str = "Library/Application Support/Claude/plan-usage-history.json";
/// A month of readings is well under 1 MB; anything far larger is not the
/// file this reader knows.
const CLAUDE_APP_MAX_BYTES: u64 = 8 * 1024 * 1024;
const FIVE_HOURS_SECS: i64 = 5 * 3600;
const WEEK_SECS: i64 = 7 * 86_400;
/// Two renewals this close to a whole number of weeks apart are the same
/// weekly schedule.
const WEEKLY_PHASE_SLACK_SECS: i64 = 2 * 3600;

/// The windows the app records, by the key it stores each one under.
struct AppWindow {
    key: &'static str,
    minutes: u64,
    scope: Option<&'static str>,
}

const APP_WINDOWS: [AppWindow; 4] = [
    AppWindow { key: "fh", minutes: 300, scope: None },
    AppWindow { key: "sd", minutes: 10_080, scope: None },
    AppWindow { key: "so", minutes: 10_080, scope: Some("Opus") },
    AppWindow { key: "sn", minutes: 10_080, scope: Some("Sonnet") },
];

/// One reading from the app's history: when, which account, and the percent
/// used of each window in `APP_WINDOWS` order.
struct AppSample {
    at: i64,
    org: Option<String>,
    used: [Option<f64>; APP_WINDOWS.len()],
}

/// Readings oldest first, or None for a file whose layout is not the one this
/// reader was written against (version 2: `{version, samples: [{t, org, u}]}`,
/// `t` in milliseconds). An unknown layout is left out rather than guessed at.
fn claude_app_samples(v: &Value) -> Option<Vec<AppSample>> {
    if v.get("version")?.as_u64()? != 2 {
        return None;
    }
    let mut samples: Vec<AppSample> = v
        .get("samples")?
        .as_array()?
        .iter()
        .filter_map(|s| {
            let ms = s.get("t")?.as_f64().filter(|ms| ms.is_finite() && *ms > 0.0)?;
            let u = s.get("u")?.as_object()?;
            let mut used = [None; APP_WINDOWS.len()];
            for (slot, window) in used.iter_mut().zip(APP_WINDOWS.iter()) {
                *slot = u
                    .get(window.key)
                    .and_then(Value::as_f64)
                    .filter(|p| p.is_finite())
                    .map(|p| p.clamp(0.0, 100.0));
            }
            Some(AppSample {
                at: (ms / 1000.0) as i64,
                org: s.get("org").and_then(Value::as_str).map(str::to_string),
                used,
            })
        })
        .collect();
    samples.sort_by_key(|s| s.at);
    Some(samples)
}

/// The latest moment the current five-hour window can renew.
///
/// The app records percentages only. A window opens with the first request
/// after the previous one ran out, and within a window the percentage never
/// falls, so the run of non-zero, non-falling readings that ends with the
/// latest one is the current window, and it opened no later than that run's
/// first reading. Five hours after that reading is when it renews at the
/// latest. The latest rather than a midpoint on purpose: a countdown that ends
/// while the limit still blocks is worse than one that ends a little late.
fn session_renewal(samples: &[AppSample], slot: usize) -> Option<i64> {
    let last = samples.len().checked_sub(1)?;
    if samples[last].used[slot]? <= 0.0 {
        // Nothing used: no window is open, so there is nothing to count down.
        return None;
    }
    let mut first = last;
    while first > 0 {
        let (prev, cur) = (&samples[first - 1], &samples[first]);
        let (Some(p), Some(c)) = (prev.used[slot], cur.used[slot]) else {
            break;
        };
        if p <= 0.0 || p > c || cur.at - prev.at >= FIVE_HOURS_SECS {
            break;
        }
        first -= 1;
    }
    // A window cannot be older than five hours at its latest reading; a run
    // longer than that renewed without the reading ever falling.
    while samples[last].at - samples[first].at >= FIVE_HOURS_SECS {
        first += 1;
    }
    Some(samples[first].at + FIVE_HOURS_SECS)
}

/// When a renewal seen between two readings most likely happened. Weekly
/// renewals on real histories land on the hour, so when exactly one hour
/// boundary falls in the gap it is that hour; otherwise the later reading.
fn renewal_moment(before: i64, after: i64) -> i64 {
    let hour = (before.div_euclid(3600) + 1) * 3600;
    if hour <= after && hour + 3600 > after {
        hour
    } else {
        after
    }
}

/// The next weekly renewal after `reading`, from the renewals the history saw.
///
/// A weekly allowance renews at a fixed moment each week and shows up as a
/// drop between two readings. Anthropic also resets allowances off schedule
/// now and then, so a renewal that another one a whole number of weeks earlier
/// agrees with is trusted over a newer lone one.
fn weekly_renewal(samples: &[AppSample], slot: usize, reading: i64) -> Option<i64> {
    let renewals: Vec<i64> = samples
        .windows(2)
        .filter_map(|pair| {
            let (p, c) = (pair[0].used[slot]?, pair[1].used[slot]?);
            (c < p).then(|| renewal_moment(pair[0].at, pair[1].at))
        })
        .collect();
    let on_schedule = |moment: i64| {
        renewals.iter().any(|&other| {
            let gap = moment - other;
            let phase = gap.rem_euclid(WEEK_SECS);
            gap >= WEEK_SECS - WEEKLY_PHASE_SLACK_SECS
                && (phase <= WEEKLY_PHASE_SLACK_SECS || phase >= WEEK_SECS - WEEKLY_PHASE_SLACK_SECS)
        })
    };
    let anchor = renewals
        .iter()
        .rev()
        .copied()
        .find(|&moment| on_schedule(moment))
        .or_else(|| renewals.last().copied())?;
    let weeks_ahead = (reading - anchor).div_euclid(WEEK_SECS) + 1;
    Some(anchor + weeks_ahead.max(0) * WEEK_SECS)
}

/// Plan limits from the Claude app's history, for `account` (Claude Code's
/// organization) when it is known.
///
/// The app can be signed in to a different account than Claude Code, and its
/// limits then say nothing about the CLI's, so only the matching account's
/// readings are used. Without a known account the latest reading's account is
/// followed, so two accounts' histories are never spliced together.
fn claude_app_limits_from(v: &Value, account: Option<&str>, now: i64) -> Option<RateLimits> {
    let mut samples = claude_app_samples(v)?;
    let wanted = account
        .map(str::to_string)
        .or_else(|| samples.last().and_then(|s| s.org.clone()));
    samples.retain(|s| s.org.is_none() || s.org == wanted);
    let latest = samples.last()?;
    // A future reading is a clock problem; a week-old one describes no window
    // that is still open.
    if latest.at > now + 300 || now - latest.at >= WEEK_SECS {
        return None;
    }
    let windows = APP_WINDOWS.iter().enumerate().filter_map(|(slot, window)| {
        let used = latest.used[slot]?;
        let renews = if window.minutes == 300 {
            session_renewal(&samples, slot)
        } else {
            weekly_renewal(&samples, slot, latest.at)
        };
        Some(RateWindow {
            used_percent: used,
            window_minutes: window.minutes,
            resets_at: renews.unwrap_or(0),
            resets_estimated: renews.is_some(),
            scope: window.scope.map(str::to_string),
        })
    });
    let (mut primary, mut secondary, mut scoped) = (None, None, Vec::new());
    for window in windows {
        match (window.scope.is_some(), window.window_minutes) {
            (false, 300) => primary = Some(window),
            (false, _) => secondary = Some(window),
            (true, _) => scoped.push(window),
        }
    }
    if primary.is_none() && secondary.is_none() && scoped.is_empty() {
        return None;
    }
    Some(RateLimits {
        captured_at: epoch_to_iso(latest.at.max(0) as u64),
        plan_type: None,
        primary,
        secondary,
        scoped,
        source: LimitSource::ClaudeApp,
    })
}

/// Claude Code's signed-in organization, from the account block it keeps in
/// `~/.claude.json`. Only the id is read; no credential lives in that block.
fn claude_code_account(home: &Path) -> Option<String> {
    let path = home.join(".claude.json");
    if fs::metadata(&path).ok()?.len() > 32 * 1024 * 1024 {
        return None;
    }
    let v: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    v.get("oauthAccount")?
        .get("organizationUuid")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// The Claude desktop app checks the plan's limits itself every few minutes
/// while it runs and keeps a month of them on disk. That file is read as is:
/// no sign-in, no request, nothing sent.
fn claude_app_limits(home: &Path, now: i64) -> Option<RateLimits> {
    let path = home.join(CLAUDE_APP_HISTORY);
    if fs::metadata(&path).ok()?.len() > CLAUDE_APP_MAX_BYTES {
        return None;
    }
    let v: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    claude_app_limits_from(&v, claude_code_account(home).as_deref(), now)
}

/// The fresher of two readings of the same limits.
fn newest_limits(a: Option<RateLimits>, b: Option<RateLimits>) -> Option<RateLimits> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.captured_at > a.captured_at { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// Decode a JWT's `exp` claim. Body only — this never validates or uses the
/// token, it just reads the expiry that is already sitting in plaintext.
fn jwt_exp(token: &str) -> Option<i64> {
    let body = token.split('.').nth(1)?;
    let mut b = body.replace('-', "+").replace('_', "/");
    while b.len() % 4 != 0 {
        b.push('=');
    }
    let bytes = base64_decode(&b)?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp")?.as_i64()
}

/// Minimal standard-alphabet base64 decoder — avoids pulling a crate in for
/// one field.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for ch in s.bytes() {
        if ch == b'=' {
            break;
        }
        let idx = TBL.iter().position(|&c| c == ch)? as u32;
        buf = (buf << 6) | idx;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn codex_auth(home: &Path) -> Option<AuthState> {
    let text = fs::read_to_string(home.join(".codex/auth.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let token = v.get("tokens")?.get("access_token")?.as_str()?;
    let exp = jwt_exp(token)?;
    Some(AuthState {
        expires_at: epoch_to_iso(exp.max(0) as u64),
        expired: exp < now_secs() as i64,
    })
}

/// Newest `*.jsonl` mtime under `root`, ignoring the scan cutoff. Used only to
/// tell "never used" apart from "last used before the window".
fn newest_file_mtime(dir: &Path, depth: usize) -> Option<u64> {
    if depth > 8 {
        return None;
    }
    let mut newest = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let found = if meta.is_dir() {
            newest_file_mtime(&path, depth + 1)
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            Some(mtime_secs(&meta))
        } else {
            None
        };
        if let Some(t) = found {
            if newest.map(|n| t > n).unwrap_or(true) {
                newest = Some(t);
            }
        }
    }
    newest
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

        // Activity older than the scan window still answers "when did I last
        // use this?", which is the one thing a zeroed card must not get wrong.
        let mut has_older_data = false;
        if detected && last_ts.is_none() {
            if let Some(mt) = newest_file_mtime(&root, 0) {
                has_older_data = true;
                last_ts = Some(epoch_to_iso(mt));
            }
        }

        let auth = if spec.kind == Kind::Codex {
            home.as_ref().and_then(|h| codex_auth(h))
        } else {
            None
        };

        // Claude Code logs no limits of its own. A status line's cache has
        // exact reset times; the Claude app's history is there without any
        // setup. Whichever was written last describes the plan as it is now.
        if spec.kind == Kind::ClaudeCode && rate_limits.is_none() {
            if let Some(h) = &home {
                rate_limits = newest_limits(
                    claude_rate_limits(h),
                    claude_app_limits(h, now_secs() as i64),
                );
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
            auth,
            has_older_data,
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

    /// A real tool result: the image/search provider reports its own tokens
    /// under message.details, in a different schema and with no price. That is
    /// not the agent's turn usage and must not be counted as such.
    #[test]
    fn skc_ignores_tool_provider_accounting() {
        let tool_result = r#"{"type":"message","id":"746e893c","timestamp":"2026-08-17T14:10:07.036Z","message":{"role":"toolResult","toolName":"web_search","content":[{"type":"text","text":"..."}],"details":{"response":{"usage":{"inputTokens":36736,"outputTokens":2585,"totalTokens":41881}}}}}"#;
        assert!(parse_skc(&serde_json::from_str(tool_result).unwrap()).is_none());

        // The assistant turn right next to it still parses.
        let turn = parse_skc(&serde_json::from_str(SKC_MESSAGE).unwrap()).unwrap();
        assert_eq!(turn.b.total, 12_315);
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
    fn jwt_exp_is_read_without_touching_the_signature() {
        // {"exp":1785933904,"iss":"https://auth.openai.com"} — same shape as
        // the Codex access token, with a garbage signature to prove we never
        // look at it.
        let token = "eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjE3ODU5MzM5MDQsImlzcyI6Imh0dHBzOi8vYXV0aC5vcGVuYWkuY29tIn0.not-a-real-signature";
        assert_eq!(jwt_exp(token), Some(1_785_933_904));
        assert_eq!(jwt_exp("garbage"), None);
        assert_eq!(jwt_exp("a.b"), None);
    }

    #[test]
    fn codex_auth_expiry_comes_from_the_plaintext_file() {
        let base = std::env::temp_dir().join(format!(
            "sayknow-auth-test-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(base.join(".codex")).unwrap();
        // exp 1785933904 = 2026-08-05, i.e. already past.
        let auth = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjE3ODU5MzM5MDQsImlzcyI6Imh0dHBzOi8vYXV0aC5vcGVuYWkuY29tIn0.sig"}}"#;
        fs::write(base.join(".codex/auth.json"), auth).unwrap();

        let state = codex_auth(&base).expect("auth.json should be readable");
        assert_eq!(state.expires_at, "2026-08-05T12:45:04Z");
        assert!(state.expired);

        // No auth.json at all must not be reported as "expired".
        fs::remove_file(base.join(".codex/auth.json")).unwrap();
        assert!(codex_auth(&base).is_none());

        fs::remove_dir_all(&base).ok();
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

    const ACCOUNT: &str = "0f0f0f0f-1111-2222-3333-444444444444";
    const OTHER_ACCOUNT: &str = "9e9e9e9e-5555-6666-7777-888888888888";
    /// 2026-09-24T14:00:00Z, a Thursday: the weekly renewal hour seen in the
    /// history this reader was built against.
    const THU_14: i64 = 1_790_258_400;
    const MIN: i64 = 60;

    fn reading(at: i64, org: &str, fh: f64, sd: f64) -> Value {
        serde_json::json!({ "t": at * 1000, "org": org, "u": { "fh": fh, "sd": sd } })
    }

    fn history(samples: Vec<Value>) -> Value {
        serde_json::json!({ "version": 2, "samples": samples })
    }

    /// Same layout as a real `plan-usage-history.json` entry (the account id is
    /// replaced): `t` in milliseconds, the account, and whole percentages.
    #[test]
    fn a_real_shaped_history_entry_is_read() {
        let raw = r#"{"version":2,"samples":[{"t":1790589669793,"org":"0f0f0f0f-1111-2222-3333-444444444444","u":{"fh":7,"sd":54}}]}"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        let limits = claude_app_limits_from(&v, Some(ACCOUNT), 1_790_589_700).unwrap();
        assert_eq!(limits.source, LimitSource::ClaudeApp);
        assert_eq!(limits.captured_at, "2026-09-28T10:01:09Z");
        let five = limits.primary.unwrap();
        assert_eq!((five.used_percent, five.window_minutes), (7.0, 300));
        let week = limits.secondary.unwrap();
        assert_eq!((week.used_percent, week.window_minutes), (54.0, 10_080));
        // One reading shows no renewal, so the week's reset is unknown and
        // must not be presented as a time.
        assert_eq!((week.resets_at, week.resets_estimated), (0, false));
    }

    #[test]
    fn the_session_renews_five_hours_after_its_first_reading() {
        let start = THU_14 + 3 * 3600;
        let v = history(vec![
            reading(start - 15 * MIN, ACCOUNT, 0.0, 10.0),
            reading(start, ACCOUNT, 4.0, 11.0),
            reading(start + 15 * MIN, ACCOUNT, 9.0, 12.0),
            reading(start + 30 * MIN, ACCOUNT, 9.0, 12.0),
        ]);
        let five = claude_app_limits_from(&v, Some(ACCOUNT), start + 31 * MIN)
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(five.used_percent, 9.0);
        assert_eq!(five.resets_at, start + FIVE_HOURS_SECS);
        assert!(five.resets_estimated);
    }

    /// Work that carries on across a renewal never reads zero; the fall from
    /// 80 to 1 is the renewal, and the window opened after it.
    #[test]
    fn a_fall_without_zero_still_starts_a_new_session() {
        let t = THU_14 + 3 * 3600;
        let v = history(vec![
            reading(t, ACCOUNT, 60.0, 10.0),
            reading(t + 15 * MIN, ACCOUNT, 80.0, 10.0),
            reading(t + 30 * MIN, ACCOUNT, 1.0, 11.0),
            reading(t + 45 * MIN, ACCOUNT, 6.0, 11.0),
        ]);
        let five = claude_app_limits_from(&v, Some(ACCOUNT), t + 46 * MIN)
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(five.resets_at, t + 30 * MIN + FIVE_HOURS_SECS);
    }

    #[test]
    fn an_unused_session_has_no_countdown() {
        let v = history(vec![reading(THU_14, ACCOUNT, 0.0, 20.0)]);
        let five = claude_app_limits_from(&v, Some(ACCOUNT), THU_14 + MIN)
            .unwrap()
            .primary
            .unwrap();
        assert_eq!((five.used_percent, five.resets_at, five.resets_estimated), (0.0, 0, false));
    }

    /// A run that climbs for longer than five hours renewed somewhere inside
    /// it; the window at the latest reading can be no older than five hours.
    #[test]
    fn a_session_is_never_older_than_five_hours() {
        let t = THU_14 + 3600;
        let v = history(
            (0..=24)
                .map(|i| reading(t + i * 15 * MIN, ACCOUNT, 1.0 + i as f64, 10.0))
                .collect(),
        );
        let latest = t + 24 * 15 * MIN;
        let five = claude_app_limits_from(&v, Some(ACCOUNT), latest)
            .unwrap()
            .primary
            .unwrap();
        assert!(five.resets_at > latest);
        assert!(five.resets_at <= latest + FIVE_HOURS_SECS);
    }

    /// Renewals seen at 13:50→14:05 on consecutive Thursdays, then an
    /// off-schedule reset the next Wednesday morning: the schedule wins, and
    /// the next renewal is the following Thursday at 14:00.
    #[test]
    fn the_weekly_renewal_follows_the_schedule_not_a_lone_reset() {
        let bracket = |renewal: i64, before: f64| {
            vec![
                reading(renewal - 10 * MIN, ACCOUNT, 0.0, before),
                reading(renewal + 5 * MIN, ACCOUNT, 0.0, 0.0),
            ]
        };
        let mut samples = bracket(THU_14 - WEEK_SECS, 80.0);
        samples.extend(bracket(THU_14, 90.0));
        let off_schedule = THU_14 + 6 * 86_400 - 8 * 3600;
        samples.push(reading(off_schedule - 3 * MIN, ACCOUNT, 0.0, 40.0));
        samples.push(reading(off_schedule + 12 * MIN, ACCOUNT, 0.0, 0.0));
        let latest = off_schedule + 3600;
        samples.push(reading(latest, ACCOUNT, 0.0, 3.0));

        let week = claude_app_limits_from(&history(samples), Some(ACCOUNT), latest + MIN)
            .unwrap()
            .secondary
            .unwrap();
        assert_eq!(week.resets_at, THU_14 + WEEK_SECS);
        assert!(week.resets_estimated);
    }

    #[test]
    fn a_renewal_lands_on_the_hour_inside_its_gap() {
        assert_eq!(renewal_moment(THU_14 - 10 * MIN, THU_14 + 5 * MIN), THU_14);
        // Several hours in the gap: nothing to pick between, the later reading.
        assert_eq!(renewal_moment(THU_14 - 5 * 3600, THU_14 + 5 * MIN), THU_14 + 5 * MIN);
    }

    #[test]
    fn another_accounts_readings_are_never_used() {
        let v = history(vec![
            reading(THU_14, ACCOUNT, 30.0, 40.0),
            reading(THU_14 + 15 * MIN, OTHER_ACCOUNT, 70.0, 90.0),
        ]);
        // Claude Code is signed in to ACCOUNT: the other account's newer
        // reading is not this CLI's plan.
        let mine = claude_app_limits_from(&v, Some(ACCOUNT), THU_14 + 16 * MIN).unwrap();
        assert_eq!(mine.primary.unwrap().used_percent, 30.0);
        // Unknown CLI account: follow the latest reading's account only.
        let latest = claude_app_limits_from(&v, None, THU_14 + 16 * MIN).unwrap();
        assert_eq!(latest.primary.unwrap().used_percent, 70.0);
        // An account the app never recorded gets nothing rather than a guess.
        assert!(claude_app_limits_from(&v, Some("elsewhere"), THU_14 + 16 * MIN).is_none());
    }

    #[test]
    fn model_scoped_weekly_windows_are_kept_apart() {
        let v = history(vec![serde_json::json!({
            "t": THU_14 * 1000, "org": ACCOUNT, "u": { "fh": 5, "sd": 20, "so": 35, "sn": 12 }
        })]);
        let limits = claude_app_limits_from(&v, Some(ACCOUNT), THU_14 + MIN).unwrap();
        let scopes: Vec<(Option<&str>, f64)> = limits
            .scoped
            .iter()
            .map(|w| (w.scope.as_deref(), w.used_percent))
            .collect();
        assert_eq!(scopes, vec![(Some("Opus"), 35.0), (Some("Sonnet"), 12.0)]);
        assert_eq!(limits.secondary.unwrap().scope, None);
    }

    #[test]
    fn stale_or_unknown_histories_are_left_out() {
        let v = history(vec![reading(THU_14, ACCOUNT, 5.0, 5.0)]);
        assert!(claude_app_limits_from(&v, Some(ACCOUNT), THU_14 + WEEK_SECS).is_none());
        let unknown = serde_json::json!({ "version": 3, "samples": [reading(THU_14, ACCOUNT, 5.0, 5.0)] });
        assert!(claude_app_limits_from(&unknown, Some(ACCOUNT), THU_14 + MIN).is_none());
        let flags = serde_json::json!({ "version": 2, "samples": [{ "t": THU_14 * 1000, "org": ACCOUNT, "u": { "fh": true } }] });
        assert!(claude_app_limits_from(&flags, Some(ACCOUNT), THU_14 + MIN).is_none());
    }

    #[test]
    fn the_fresher_source_wins() {
        let at = |captured: &str, source| RateLimits {
            captured_at: captured.to_string(),
            plan_type: None,
            primary: None,
            secondary: None,
            scoped: Vec::new(),
            source,
        };
        let status = at("2026-09-28T09:00:00Z", LimitSource::StatusLine);
        let app = at("2026-09-28T10:01:09Z", LimitSource::ClaudeApp);
        assert_eq!(newest_limits(Some(status.clone()), Some(app)).unwrap().source, LimitSource::ClaudeApp);
        assert_eq!(newest_limits(Some(status), None).unwrap().source, LimitSource::StatusLine);
        assert!(newest_limits(None, None).is_none());
    }

    /// The whole path the command takes: the app's file under Library, Claude
    /// Code's account in ~/.claude.json, and nothing else configured.
    #[test]
    fn claude_app_limits_are_read_off_the_disk() {
        let base = std::env::temp_dir().join(format!(
            "sayknow-claude-app-test-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = base.join("Library/Application Support/Claude");
        fs::create_dir_all(&dir).unwrap();
        let file = history(vec![
            reading(THU_14, ACCOUNT, 12.0, 30.0),
            reading(THU_14 + 15 * MIN, OTHER_ACCOUNT, 99.0, 99.0),
        ]);
        fs::write(dir.join("plan-usage-history.json"), file.to_string()).unwrap();
        let account = serde_json::json!({ "oauthAccount": { "organizationUuid": ACCOUNT } });
        fs::write(base.join(".claude.json"), account.to_string()).unwrap();

        let limits = claude_app_limits(&base, THU_14 + 20 * MIN).expect("history should be read");
        assert_eq!(limits.primary.unwrap().used_percent, 12.0);

        fs::remove_dir_all(&base).ok();
    }

    /// Reads this Mac's own Claude app history and prints what the panel would
    /// show. Read-only; nothing is written or sent.
    #[test]
    #[ignore = "Reads the real Claude app history; run with -- --ignored --nocapture"]
    fn live_claude_app_limits() {
        let home = home_dir().expect("HOME");
        let limits = claude_app_limits(&home, now_secs() as i64).expect("Claude app history present");
        println!("{}", serde_json::to_string_pretty(&limits).unwrap());
    }
}
