//! Content-free sync observability (§6, plan §9 "관측").
//!
//! [`SyncStats`] is a set of atomic counters the engine bumps; `snapshot()`
//! is what `sync_get_status` and the diagnostics export serialize. Nothing
//! here can hold an op payload, a key, a token, an entity id, or a vault id:
//! every counter is a number, every state is a fixed enum text, and
//! [`log_event`] only accepts [`Field`] values that are numbers, booleans,
//! fixed `&'static str` labels, or a [`VaultTag`].
//!
//! Vault ids: logs and exports may name a vault only by [`VaultTag`], the
//! first 8 hex chars of `SHA-256("sayknow-vault-tag-v1" ‖ vault_id)`. It is
//! enough to tell two vaults apart in a support log, and reveals nothing a
//! provider could use to find the vault (32 bits of a keyed-by-label hash).

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Why a provider call or a cycle step failed. `Local` is a store or
/// keychain fault on this device (never the remote's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureClass {
    Auth,
    Rate,
    Quota,
    Transient,
    Permanent,
    Binding,
    Unsupported,
    Local,
}

impl FailureClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Rate => "rate",
            Self::Quota => "quota",
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::Binding => "binding",
            Self::Unsupported => "unsupported",
            Self::Local => "local",
        }
    }

    fn code(self) -> u64 {
        match self {
            Self::Auth => 1,
            Self::Rate => 2,
            Self::Quota => 3,
            Self::Transient => 4,
            Self::Permanent => 5,
            Self::Binding => 6,
            Self::Unsupported => 7,
            Self::Local => 8,
        }
    }

    fn from_code(code: u64) -> Option<Self> {
        Some(match code {
            1 => Self::Auth,
            2 => Self::Rate,
            3 => Self::Quota,
            4 => Self::Transient,
            5 => Self::Permanent,
            6 => Self::Binding,
            7 => Self::Unsupported,
            8 => Self::Local,
            _ => return None,
        })
    }
}

/// Atomic counters shared by the engine and the command layer.
#[derive(Debug, Default)]
pub(crate) struct SyncStats {
    uploads: AtomicU64,
    upload_bytes: AtomicU64,
    downloads: AtomicU64,
    download_bytes: AtomicU64,
    fail_auth: AtomicU64,
    fail_rate: AtomicU64,
    fail_quota: AtomicU64,
    fail_transient: AtomicU64,
    fail_permanent: AtomicU64,
    fail_binding: AtomicU64,
    fail_unsupported: AtomicU64,
    fail_local: AtomicU64,
    quarantined: AtomicU64,
    rotations: AtomicU64,
    rejected: AtomicU64,
    forked: AtomicU64,
    reprojections: AtomicU64,
    reproject_ms_max: AtomicU64,
    ctx_size_max: AtomicU64,
    obs_size_max: AtomicU64,
    cycles: AtomicU64,
    last_cycle_ms: AtomicU64,
    /// 0 = none, else [`FailureClass::code`].
    last_error: AtomicU64,
}

/// Serializable view of [`SyncStats`]: numbers and one enum only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatsSnapshot {
    pub(crate) uploads: u64,
    pub(crate) upload_bytes: u64,
    pub(crate) downloads: u64,
    pub(crate) download_bytes: u64,
    pub(crate) failures: FailureCounts,
    pub(crate) quarantined: u64,
    pub(crate) rotations: u64,
    pub(crate) rejected: u64,
    pub(crate) forked: u64,
    pub(crate) reprojections: u64,
    pub(crate) reproject_ms_max: u64,
    pub(crate) ctx_size_max: u64,
    pub(crate) obs_size_max: u64,
    pub(crate) cycles: u64,
    pub(crate) last_cycle_ms: u64,
    pub(crate) last_error: Option<FailureClass>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FailureCounts {
    pub(crate) auth: u64,
    pub(crate) rate: u64,
    pub(crate) quota: u64,
    pub(crate) transient: u64,
    pub(crate) permanent: u64,
    pub(crate) binding: u64,
    pub(crate) unsupported: u64,
    pub(crate) local: u64,
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

impl SyncStats {
    pub(crate) fn uploaded(&self, bytes: usize) {
        add(&self.uploads, 1);
        add(&self.upload_bytes, bytes as u64);
    }

    pub(crate) fn downloaded(&self, bytes: usize) {
        add(&self.downloads, 1);
        add(&self.download_bytes, bytes as u64);
    }

    pub(crate) fn failed(&self, class: FailureClass) {
        let counter = match class {
            FailureClass::Auth => &self.fail_auth,
            FailureClass::Rate => &self.fail_rate,
            FailureClass::Quota => &self.fail_quota,
            FailureClass::Transient => &self.fail_transient,
            FailureClass::Permanent => &self.fail_permanent,
            FailureClass::Binding => &self.fail_binding,
            FailureClass::Unsupported => &self.fail_unsupported,
            FailureClass::Local => &self.fail_local,
        };
        add(counter, 1);
        self.last_error.store(class.code(), Ordering::Relaxed);
    }

    pub(crate) fn quarantined(&self) {
        add(&self.quarantined, 1);
    }

    pub(crate) fn rotated(&self) {
        add(&self.rotations, 1);
    }

    pub(crate) fn rejected(&self) {
        add(&self.rejected, 1);
    }

    pub(crate) fn forked(&self) {
        add(&self.forked, 1);
    }

    pub(crate) fn reprojected(&self, millis: u64) {
        add(&self.reprojections, 1);
        self.reproject_ms_max.fetch_max(millis, Ordering::Relaxed);
    }

    /// Largest `ctx` (devices) and `obs` (op ids) seen in an op.
    pub(crate) fn op_sizes(&self, ctx: usize, obs: usize) {
        self.ctx_size_max.fetch_max(ctx as u64, Ordering::Relaxed);
        self.obs_size_max.fetch_max(obs as u64, Ordering::Relaxed);
    }

    /// End of a cycle; `error` is the class it ended on, `None` when clean.
    pub(crate) fn cycle_done(&self, millis: u64, error: Option<FailureClass>) {
        add(&self.cycles, 1);
        self.last_cycle_ms.store(millis, Ordering::Relaxed);
        self.last_error.store(error.map_or(0, FailureClass::code), Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            uploads: get(&self.uploads),
            upload_bytes: get(&self.upload_bytes),
            downloads: get(&self.downloads),
            download_bytes: get(&self.download_bytes),
            failures: FailureCounts {
                auth: get(&self.fail_auth),
                rate: get(&self.fail_rate),
                quota: get(&self.fail_quota),
                transient: get(&self.fail_transient),
                permanent: get(&self.fail_permanent),
                binding: get(&self.fail_binding),
                unsupported: get(&self.fail_unsupported),
                local: get(&self.fail_local),
            },
            quarantined: get(&self.quarantined),
            rotations: get(&self.rotations),
            rejected: get(&self.rejected),
            forked: get(&self.forked),
            reprojections: get(&self.reprojections),
            reproject_ms_max: get(&self.reproject_ms_max),
            ctx_size_max: get(&self.ctx_size_max),
            obs_size_max: get(&self.obs_size_max),
            cycles: get(&self.cycles),
            last_cycle_ms: get(&self.last_cycle_ms),
            last_error: FailureClass::from_code(get(&self.last_error)),
        }
    }
}

/// The only way a vault appears in logs or exports (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct VaultTag(String);

impl VaultTag {
    pub(crate) fn of(vault_id: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"sayknow-vault-tag-v1");
        hasher.update(vault_id.as_bytes());
        VaultTag(super::crypto::hex(&hasher.finalize()[..4]))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A log field value. There is deliberately no variant for runtime text.
#[derive(Debug, Clone)]
pub(crate) enum Field {
    Num(u64),
    Bool(bool),
    Label(&'static str),
    Vault(VaultTag),
}

impl std::fmt::Display for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Field::Num(n) => write!(f, "{n}"),
            Field::Bool(b) => write!(f, "{b}"),
            Field::Label(text) => f.write_str(text),
            Field::Vault(tag) => f.write_str(tag.as_str()),
        }
    }
}

/// Emits one content-free `sync.<kind> k=v ...` line through `log`.
pub(crate) fn log_event(kind: &'static str, fields: &[(&'static str, Field)]) {
    let mut line = format!("sync.{kind}");
    for (name, value) in fields {
        line.push_str(&format!(" {name}={value}"));
    }
    log::info!("{line}");
    #[cfg(test)]
    capture::push(line);
}

/// The diagnostics export: counters plus state enums, nothing else.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Diagnostics<'a> {
    pub(crate) format: u32,
    pub(crate) vault: Option<VaultTag>,
    pub(crate) states: &'a [(&'static str, &'static str)],
    pub(crate) stats: StatsSnapshot,
}

pub(crate) fn export_diagnostics(
    stats: &SyncStats,
    vault_id: Option<&str>,
    states: &[(&'static str, &'static str)],
) -> String {
    let diagnostics = Diagnostics { format: 1, vault: vault_id.map(VaultTag::of), states, stats: stats.snapshot() };
    // Only numbers and fixed labels: serialization cannot fail.
    serde_json::to_string(&diagnostics).unwrap_or_else(|_| "{}".to_owned())
}

/// Test-only capture of every `log_event` line on this thread.
#[cfg(test)]
pub(crate) mod capture {
    use std::cell::RefCell;

    thread_local! {
        static LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn push(line: String) {
        LINES.with(|lines| lines.borrow_mut().push(line));
    }

    pub(crate) fn take() -> Vec<String> {
        LINES.with(|lines| std::mem::take(&mut *lines.borrow_mut()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_counters_and_the_last_error_class() {
        let stats = SyncStats::default();
        stats.uploaded(10);
        stats.uploaded(5);
        stats.downloaded(7);
        stats.failed(FailureClass::Rate);
        stats.failed(FailureClass::Binding);
        stats.reprojected(4);
        stats.reprojected(2);
        stats.op_sizes(3, 1);
        stats.op_sizes(2, 5);
        let s = stats.snapshot();
        assert_eq!((s.uploads, s.upload_bytes, s.downloads, s.download_bytes), (2, 15, 1, 7));
        assert_eq!((s.failures.rate, s.failures.binding, s.failures.auth), (1, 1, 0));
        assert_eq!(s.last_error, Some(FailureClass::Binding));
        assert_eq!((s.reprojections, s.reproject_ms_max), (2, 4));
        assert_eq!((s.ctx_size_max, s.obs_size_max), (3, 5));
        stats.cycle_done(9, None);
        assert_eq!(stats.snapshot().last_error, None);
        assert_eq!(stats.snapshot().last_cycle_ms, 9);
    }

    #[test]
    fn vault_tag_is_a_short_hash_not_the_id() {
        let tag = VaultTag::of("vault-1234");
        assert_eq!(tag.as_str().len(), 8);
        assert!(tag.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!"vault-1234".contains(tag.as_str()));
        assert_ne!(tag, VaultTag::of("vault-1235"));
        assert_eq!(tag, VaultTag::of("vault-1234"));
    }

    #[test]
    fn log_lines_carry_only_the_given_fields() {
        capture::take();
        log_event("test", &[("n", Field::Num(3)), ("ok", Field::Bool(true)), ("class", Field::Label("rate"))]);
        assert_eq!(capture::take(), vec!["sync.test n=3 ok=true class=rate".to_owned()]);
    }
}
