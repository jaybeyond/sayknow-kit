//! Append-only record of everything the Cursor agent ran or changed on disk.
//!
//! The user chose unrestricted access with no approval prompts, so this log is
//! the only way to answer "what did it actually do". It is deliberately not a
//! permission gate: it never blocks an operation, it only records one.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// One audited action.
#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: &'static str,
    pub target: String,
    pub detail: String,
}

impl Entry {
    pub fn new(kind: &'static str, target: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            target: target.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    // Serializes writers so interleaved turns cannot tear a line.
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            lock: Mutex::new(()),
        }
    }

    /// Record one action. A failure to write is logged, never propagated: the
    /// agent's work is not aborted because the journal is unwritable.
    pub fn record(&self, entry: Entry) {
        let line = serde_json::json!({
            "ts": now_millis(),
            "kind": entry.kind,
            "target": entry.target,
            "detail": entry.detail,
        })
        .to_string();

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{line}") {
                    log::warn!("cursor audit log write failed: {e}");
                }
            }
            Err(e) => log::warn!("cursor audit log open failed: {e}"),
        }
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_appended_as_json_lines() {
        let dir = std::env::temp_dir().join(format!("cursor-audit-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.log");
        let log = AuditLog::new(&path);

        log.record(Entry::new("shell", "echo hi", "exit=0"));
        log.record(Entry::new("write", "/tmp/x", "12 bytes"));

        let contents = std::fs::read_to_string(&path).expect("log exists");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("json line");
        assert_eq!(first["kind"], "shell");
        assert_eq!(first["target"], "echo hi");
        assert!(first["ts"].as_u64().unwrap() > 0);

        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("json line");
        assert_eq!(second["kind"], "write");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_path_does_not_panic() {
        // A directory where a file must go: writing can only fail.
        let log = AuditLog::new("/");
        log.record(Entry::new("shell", "noop", ""));
    }
}
