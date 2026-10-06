//! Backend-owned, one-shot removal snapshots. The only write adapter is Foundation Trash.
use super::{RunPermit, RUN_STATE};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize)]
pub struct AppSummary {
    pub id: String,
    pub name: String,
    pub bundle_id: String,
    pub path: String,
    pub size_label: String,
    pub source: String,
    pub blocked_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct AppInventory {
    pub generation: String,
    pub apps: Vec<AppSummary>,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemovalKind {
    App,
    Cache,
    Preferences,
    SavedState,
    Webkit,
    HttpStorage,
    Support,
    Container,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemovalCandidate {
    pub id: String,
    pub kind: RemovalKind,
    pub path: String,
    pub size_bytes: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemovalExclusion {
    pub kind: RemovalKind,
    pub path: String,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct AppRemovalPreview {
    pub token: String,
    pub generation: String,
    pub expires_at_ms: u64,
    pub app: RemovalCandidate,
    pub related: Vec<RemovalCandidate>,
    pub excluded: Vec<RemovalExclusion>,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemovalStatus {
    Moved,
    Failed,
    Unknown,
    NotAttempted,
}
#[derive(Debug, Serialize)]
pub struct RemovalItemResult {
    pub candidate_id: String,
    pub kind: RemovalKind,
    pub path: String,
    pub status: RemovalStatus,
    pub error: Option<String>,
    pub trash_path: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct AppRemovalOutcome {
    pub items: Vec<RemovalItemResult>,
    pub stopped_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MoleApp {
    name: String,
    bundle_id: String,
    path: String,
    size: String,
    source: String,
}
fn parse_inventory(json: &str) -> Result<Vec<MoleApp>, String> {
    let rows: Vec<MoleApp> = serde_json::from_str(json).map_err(|_| "mole_inventory_invalid")?;
    if rows.len() > 10_000
        || rows
            .iter()
            .any(|r| r.path.len() > 4096 || r.path.contains('\0') || r.name.len() > 4096)
    {
        return Err("mole_inventory_invalid".into());
    }
    Ok(rows)
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn valid_bundle_id(value: &str) -> bool {
    value.len() <= 200
        && value.contains('.')
        && value
            .split('.')
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
}
#[derive(Clone, Debug)]
struct Guarded {
    row: RemovalCandidate,
    chain: Vec<(PathBuf, Identity)>,
    modified: SystemTime,
    len: u64,
}
#[derive(Clone)]
struct InventoryEntry {
    summary: AppSummary,
    guard: Option<Guarded>,
}
#[derive(Clone)]
struct Inventory {
    generation: String,
    entries: Vec<InventoryEntry>,
}
#[derive(Clone)]
struct Preview {
    wire: AppRemovalPreview,
    deadline: Instant,
    bundle_id: String,
    known_paths: Vec<PathBuf>,
    app: Guarded,
    related: Vec<Guarded>,
}
#[derive(Default)]
struct State {
    inventory: Option<Inventory>,
    preview: Option<Preview>,
}
static STATE: Mutex<State> = Mutex::new(State {
    inventory: None,
    preview: None,
});

impl State {
    fn list_admission(&mut self) {
        self.inventory = None;
        self.preview = None;
    }
    fn preview_admission(
        &mut self,
        generation: &str,
        app_id: &str,
    ) -> Result<(InventoryEntry, Vec<PathBuf>), String> {
        self.preview = None;
        let inventory = self
            .inventory
            .as_ref()
            .filter(|i| i.generation == generation)
            .ok_or("mole_inventory_stale")?;
        let app = inventory
            .entries
            .iter()
            .find(|e| e.summary.id == app_id)
            .ok_or("mole_inventory_stale")?;
        let paths = inventory
            .entries
            .iter()
            .filter(|e| {
                e.summary
                    .bundle_id
                    .eq_ignore_ascii_case(&app.summary.bundle_id)
            })
            .map(|e| PathBuf::from(&e.summary.path))
            .collect();
        Ok((app.clone(), paths))
    }
    fn consume(&mut self, token: &str, now: Instant) -> Result<Preview, String> {
        let preview = self.preview.take().ok_or("mole_preview_stale")?;
        if preview.wire.token != token || now >= preview.deadline {
            return Err("mole_preview_stale".into());
        }
        self.inventory = None;
        Ok(preview)
    }
    fn cancel(&mut self, token: &str) -> Result<(), String> {
        let preview = self.preview.take().ok_or("mole_preview_stale")?;
        if preview.wire.token != token {
            return Err("mole_preview_stale".into());
        }
        Ok(())
    }
}

pub(super) fn invalidate_write_rights() -> Result<(), String> {
    STATE
        .lock()
        .map_err(|_| "mole_inventory_invalid")?
        .list_admission();
    Ok(())
}
fn select<'a>(preview: &'a Preview, selected: &[String]) -> Result<Vec<&'a Guarded>, String> {
    let mut seen = HashSet::new();
    if selected
        .iter()
        .any(|s| !seen.insert(s) || !preview.related.iter().any(|r| &r.row.id == s))
    {
        return Err("mole_invalid_selection".into());
    }
    let mut rows = vec![&preview.app];
    rows.extend(preview.related.iter().filter(|r| seen.contains(&r.row.id)));
    for (index, row) in rows.iter().enumerate() {
        let path = Path::new(&row.row.path);
        if rows[..index].iter().any(|other| {
            let other = Path::new(&other.row.path);
            path.starts_with(other) || other.starts_with(path)
        }) {
            return Err("mole_invalid_selection".into());
        }
    }
    Ok(rows)
}

struct NativeResult {
    error: Option<String>,
    destination: Option<PathBuf>,
}
trait Adapter {
    fn guard(&self, preview: &Preview, item: &Guarded, app_present: bool) -> Result<(), String>;
    fn trash(&self, item: &Guarded) -> NativeResult;
    fn reconcile(&self, item: &Guarded, result: &NativeResult) -> RemovalStatus;
}
fn execute(
    preview: &Preview,
    selected: &[String],
    adapter: &impl Adapter,
) -> Result<AppRemovalOutcome, String> {
    let rows = select(preview, selected)?;
    let mut outcome = AppRemovalOutcome {
        items: rows
            .iter()
            .map(|item| RemovalItemResult {
                candidate_id: item.row.id.clone(),
                kind: item.row.kind,
                path: item.row.path.clone(),
                status: RemovalStatus::NotAttempted,
                error: None,
                trash_path: None,
            })
            .collect(),
        stopped_reason: None,
    };
    // Once selection is valid, even a zero-write rejection has per-row results.
    for (index, row) in rows.iter().enumerate() {
        if let Err(error) = adapter.guard(preview, row, true) {
            outcome.stopped_reason = Some(error.clone());
            outcome.items[index].error = Some(error);
            return Ok(outcome);
        }
    }
    for (index, item) in rows.iter().enumerate() {
        if let Err(error) = adapter.guard(preview, item, index == 0) {
            outcome.stopped_reason = Some(error.clone());
            outcome.items[index].error = Some(error);
            break;
        }
        let native = adapter.trash(item);
        let status = adapter.reconcile(item, &native);
        let error = native.error.or_else(|| match status {
            RemovalStatus::Moved => None,
            RemovalStatus::Failed => Some("mole_trash_failed".into()),
            _ => Some("mole_result_unknown".into()),
        });
        outcome.items[index].status = status;
        outcome.items[index].trash_path =
            native.destination.map(|p| p.to_string_lossy().into_owned());
        outcome.items[index].error = error.clone();
        if status != RemovalStatus::Moved || error.is_some() {
            outcome.stopped_reason = error;
            break;
        }
    }
    Ok(outcome)
}

#[tauri::command(async)]
pub fn list_mole_apps() -> Result<AppInventory, String> {
    let _permit = RunPermit::acquire(&RUN_STATE)?;
    STATE
        .lock()
        .map_err(|_| "mole_inventory_invalid")?
        .list_admission();
    #[cfg(target_os = "macos")]
    {
        let binary = super::mole_bin().ok_or("mole_not_installed")?;
        super::require_supported_version(&super::read_version(&binary)?.stdout)?;
        let run = super::run_pipe_program(
            &binary,
            &["uninstall", "--list"],
            Duration::from_secs(90),
            &RUN_STATE,
        )?;
        if !run.ok {
            return Err(format!("mole_inventory_invalid: {}", run.stderr));
        }
        let inventory = native::inventory(parse_inventory(&run.stdout)?)?;
        let wire = AppInventory {
            generation: inventory.generation.clone(),
            apps: inventory
                .entries
                .iter()
                .map(|e| e.summary.clone())
                .collect(),
        };
        STATE
            .lock()
            .map_err(|_| "mole_inventory_invalid")?
            .inventory = Some(inventory);
        Ok(wire)
    }
    #[cfg(not(target_os = "macos"))]
    Err("mole_path_unsupported".into())
}

#[tauri::command(async)]
pub fn preview_mole_app_removal(
    app_id: String,
    generation: String,
) -> Result<AppRemovalPreview, String> {
    let _permit = RunPermit::acquire(&RUN_STATE)?;
    let (entry, known_paths) = STATE
        .lock()
        .map_err(|_| "mole_inventory_invalid")?
        .preview_admission(&generation, &app_id)?;
    if let Some(reason) = entry.summary.blocked_reason {
        return Err(reason);
    }
    #[cfg(target_os = "macos")]
    {
        let preview = native::preview(entry, known_paths, generation)?;
        let wire = preview.wire.clone();
        STATE.lock().map_err(|_| "mole_inventory_invalid")?.preview = Some(preview);
        Ok(wire)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = known_paths;
        Err("mole_path_unsupported".into())
    }
}

#[tauri::command(async)]
pub fn cancel_mole_app_removal(preview_token: String) -> Result<(), String> {
    let _permit = RunPermit::acquire(&RUN_STATE)?;
    STATE
        .lock()
        .map_err(|_| "mole_preview_stale")?
        .cancel(&preview_token)
}

#[tauri::command(async)]
pub fn trash_mole_app_selection(
    preview_token: String,
    selected_candidate_ids: Vec<String>,
) -> Result<AppRemovalOutcome, String> {
    let _permit = RunPermit::acquire(&RUN_STATE)?;
    let preview = STATE
        .lock()
        .map_err(|_| "mole_preview_stale")?
        .consume(&preview_token, Instant::now())?;
    #[cfg(target_os = "macos")]
    {
        execute(&preview, &selected_candidate_ids, &native::Foundation)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (preview, selected_candidate_ids);
        Err("mole_path_unsupported".into())
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSBundle, NSFileManager, NSHomeDirectory, NSString, NSURL};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    fn home() -> Result<PathBuf, String> {
        let path = PathBuf::from(NSHomeDirectory().to_string());
        if !path.is_absolute() || path == Path::new("/") {
            return Err("mole_identity_unavailable".into());
        }
        Ok(path)
    }
    fn identity(path: &Path) -> Result<Identity, String> {
        let m = std::fs::symlink_metadata(path).map_err(|_| "mole_identity_unavailable")?;
        if m.file_type().is_symlink() {
            return Err("mole_path_unsupported".into());
        }
        Ok(Identity {
            dev: m.dev(),
            ino: m.ino(),
            uid: m.uid(),
            mode: m.mode(),
        })
    }
    fn capture(path: &Path, kind: RemovalKind) -> Result<Guarded, String> {
        if !path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("mole_path_unsupported".into());
        }
        let chain = path
            .ancestors()
            .map(|p| Ok((p.to_path_buf(), identity(p)?)))
            .collect::<Result<Vec<_>, String>>()?;
        let m = std::fs::symlink_metadata(path).map_err(|_| "mole_identity_unavailable")?;
        if !m.is_dir() && !m.is_file() {
            return Err("mole_path_unsupported".into());
        }
        if m.is_file() && m.nlink() != 1 {
            return Err("mole_shared_data".into());
        }
        if chain.get(1).is_none_or(|(_, parent)| parent.dev != m.dev()) {
            return Err("mole_path_unsupported".into());
        }
        Ok(Guarded {
            row: RemovalCandidate {
                id: id(),
                kind,
                path: path.to_str().ok_or("mole_path_unsupported")?.into(),
                size_bytes: if m.is_file() { Some(m.len()) } else { None },
            },
            chain,
            modified: m.modified().map_err(|_| "mole_identity_unavailable")?,
            len: m.len(),
        })
    }
    fn unchanged(item: &Guarded) -> Result<(), String> {
        for (path, expected) in &item.chain {
            if identity(path)? != *expected {
                return Err("mole_app_changed".into());
            }
        }
        let m = std::fs::symlink_metadata(&item.row.path).map_err(|_| "mole_app_changed")?;
        if m.modified().ok() != Some(item.modified) || m.len() != item.len {
            return Err("mole_app_changed".into());
        }
        permissions(Path::new(&item.row.path))
    }
    fn permissions(path: &Path) -> Result<(), String> {
        let uid = unsafe { libc::geteuid() };
        // Never use root privileges, even if the application was launched as root.
        if uid == 0 {
            return Err("mole_permission_required".into());
        }
        let m = std::fs::symlink_metadata(path).map_err(|_| "mole_identity_unavailable")?;
        if m.file_type().is_symlink() {
            return Err("mole_path_unsupported".into());
        }
        if m.is_file() && m.nlink() != 1 {
            return Err("mole_shared_data".into());
        }
        if m.uid() != uid || m.mode() & 0o022 != 0 {
            return Err("mole_permission_required".into());
        }
        let parent = path.parent().ok_or("mole_path_unsupported")?;
        let parent_metadata =
            std::fs::symlink_metadata(parent).map_err(|_| "mole_permission_required")?;
        if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
            return Err("mole_permission_required".into());
        }
        if protected_flags(std::os::macos::fs::MetadataExt::st_flags(&m))
            || protected_parent_flags(std::os::macos::fs::MetadataExt::st_flags(&parent_metadata))
        {
            return Err("mole_permission_required".into());
        }
        // Keep sticky-directory policy explicit even though the kernel also checks it.
        if parent_metadata.mode() & libc::S_ISVTX as u32 != 0
            && parent_metadata.uid() != uid
            && m.uid() != uid
        {
            return Err("mole_permission_required".into());
        }
        // Darwin sys/unistd.h _DELETE_OK is NOT W_OK. XNU
        // bsd/vfs/vfs_syscalls.c faccessat_internal requests WANTPARENT for it;
        // access1 maps it to KAUTH_VNODE_DELETE and vnode_authorize evaluates
        // the target delete / parent delete_child ACL together, including deny.
        // Do not separately require delete_child: DELETE may legitimately grant
        // removal without it. AT_EACCESS uses the actual Foundation credentials.
        // Source: https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_syscalls.c
        const DELETE_OK: libc::c_int = 1 << 12;
        for (p, access) in [
            (path, DELETE_OK),
            (path, libc::W_OK),
            (parent, libc::W_OK | libc::X_OK),
        ] {
            let name = std::ffi::CString::new(p.as_os_str().as_bytes())
                .map_err(|_| "mole_permission_required")?;
            if unsafe {
                libc::faccessat(
                    libc::AT_FDCWD,
                    name.as_ptr(),
                    access,
                    libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err("mole_permission_required".into());
            }
            let mut volume = std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe { libc::statfs(name.as_ptr(), volume.as_mut_ptr()) } != 0 {
                return Err("mole_permission_required".into());
            }
            if !writable_local_volume(unsafe { volume.assume_init() }.f_flags) {
                return Err("mole_permission_required".into());
            }
        }
        Ok(())
    }
    fn protected_flags(flags: u32) -> bool {
        // SDK sys/stat.h: SF_NOUNLINK protects this entry from removal/rename,
        // not its children. Keep it blocked on the actual Trash target.
        protected_parent_flags(flags) || flags & 0x00100000 != 0
    }
    fn protected_parent_flags(flags: u32) -> bool {
        // XNU bsd/vfs/vfs_subr.c vnode_attr_authorize_internal checks the
        // parent's KAUTH_VNODE_DELETE_CHILD separately from the target DELETE.
        // Immutable/append-only forbid child removal; retain our SIP restriction.
        // SF_NOUNLINK alone does not forbid modifying children. Passing this
        // filter never grants access: permissions still requires kernel _DELETE_OK.
        // Source: https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_subr.c
        flags
            & (libc::UF_IMMUTABLE
                | libc::SF_IMMUTABLE
                | libc::UF_APPEND
                | libc::SF_APPEND
                | 0x00080000)
            != 0
    }
    fn writable_local_volume(flags: u32) -> bool {
        // sys/mount.h: reject remote, read-only, snapshot and ignored ownership.
        const IGNORE_OWNERSHIP: u32 = 0x00200000;
        flags & libc::MNT_LOCAL as u32 != 0
            && flags & (libc::MNT_RDONLY as u32 | libc::MNT_SNAPSHOT as u32 | IGNORE_OWNERSHIP) == 0
    }
    fn bundle_id(path: &Path) -> Result<String, String> {
        // NSBundle caches metadata; read a fresh Info.plist dictionary instead.
        let plist = path.join("Contents/Info.plist");
        let before = capture(&plist, RemovalKind::App)?;
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&plist)
            .map_err(|_| "mole_identity_unavailable")?;
        let opened = file.metadata().map_err(|_| "mole_identity_unavailable")?;
        if opened.dev() != before.chain[0].1.dev || opened.ino() != before.chain[0].1.ino {
            return Err("mole_app_changed".into());
        }
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "mole_identity_unavailable")?;
        if bytes.len() > 1024 * 1024 {
            return Err("mole_identity_unavailable".into());
        }
        // Check identity without imposing writable permissions on the Info.plist.
        let after = capture(&plist, RemovalKind::App)?;
        if before.chain != after.chain
            || before.modified != after.modified
            || before.len != after.len
        {
            return Err("mole_app_changed".into());
        }
        let data = objc2_foundation::NSData::with_bytes(&bytes);
        let object = unsafe {
            objc2_foundation::NSPropertyListSerialization::propertyListWithData_options_format_error(
                &data, objc2_foundation::NSPropertyListMutabilityOptions::Immutable, std::ptr::null_mut(),
            )
        }.map_err(|_| "mole_identity_unavailable")?;
        let dictionary = object
            .downcast_ref::<objc2_foundation::NSDictionary>()
            .ok_or("mole_identity_unavailable")?;
        let value = dictionary
            .objectForKey(&NSString::from_str("CFBundleIdentifier"))
            .ok_or("mole_identity_unavailable")?;
        let value = value
            .downcast_ref::<NSString>()
            .ok_or("mole_identity_unavailable")?
            .to_string();
        if !valid_bundle_id(&value) {
            return Err("mole_identity_unavailable".into());
        }
        Ok(value)
    }
    fn runtime_guard(path: &Path, bid: &str) -> Result<(), String> {
        let main = NSBundle::mainBundle();
        let runtime = PathBuf::from(main.bundlePath().to_string());
        let executable = std::env::current_exe().map_err(|_| "mole_identity_unavailable")?;
        if bid.eq_ignore_ascii_case("com.sayknow.measure")
            || main
                .bundleIdentifier()
                .is_some_and(|s| s.to_string().eq_ignore_ascii_case(bid))
            || path == runtime
            || executable.starts_with(path)
        {
            return Err("mole_self_app".into());
        }
        if bid.to_ascii_lowercase().starts_with("com.apple.")
            || path.starts_with("/System")
            || path.starts_with("/Library")
        {
            return Err("mole_protected_app".into());
        }
        let workspace = NSWorkspace::sharedWorkspace();
        for running in workspace.runningApplications().iter() {
            let bundle = running
                .bundleURL()
                .and_then(|u| u.path())
                .map(|s| PathBuf::from(s.to_string()));
            let executable = running
                .executableURL()
                .and_then(|u| u.path())
                .map(|s| PathBuf::from(s.to_string()));
            let running_id = running.bundleIdentifier().map(|s| s.to_string());
            if running_match(
                path,
                bid,
                running_id.as_deref(),
                bundle.as_deref(),
                executable.as_deref(),
            ) {
                return Err("mole_app_running".into());
            }
        }
        process_guard(path)
    }
    fn running_match(
        target: &Path,
        bid: &str,
        running_id: Option<&str>,
        bundle: Option<&Path>,
        executable: Option<&Path>,
    ) -> bool {
        running_id.is_some_and(|id| id.eq_ignore_ascii_case(bid))
            || bundle.is_some_and(|p| p.starts_with(target))
            || executable.is_some_and(|p| p.starts_with(target))
    }
    fn inspect_processes(
        target: &Path,
        pids: &[libc::pid_t],
        started: Instant,
        mut executable: impl FnMut(libc::pid_t) -> Result<Option<PathBuf>, String>,
    ) -> Result<(), String> {
        for &pid in pids {
            // PID 0 is the kernel, not an executable app/helper.
            if pid == 0 {
                continue;
            }
            if pid < 0 || started.elapsed() >= Duration::from_secs(2) {
                return Err("mole_identity_unavailable: process inventory incomplete".into());
            }
            if let Some(path) = executable(pid)? {
                if !path.is_absolute() {
                    return Err("mole_identity_unavailable: process executable unavailable".into());
                }
                if path.starts_with(target) {
                    return Err("mole_app_running".into());
                }
            }
        }
        if started.elapsed() >= Duration::from_secs(2) {
            return Err("mole_identity_unavailable: process inventory timeout".into());
        }
        Ok(())
    }
    fn process_guard(target: &Path) -> Result<(), String> {
        // libproc.h / sys/proc_info.h. A fixed cap, one enumeration, no shell,
        // no retry; full buffers and inaccessible live processes fail closed.
        const MAX_PIDS: usize = 16_384;
        let started = Instant::now();
        let mut pids = vec![0 as libc::pid_t; MAX_PIDS];
        let capacity = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let bytes = unsafe {
            libc::proc_listpids(
                1, /* PROC_ALL_PIDS */
                0,
                pids.as_mut_ptr().cast(),
                capacity,
            )
        };
        if bytes <= 0
            || bytes >= capacity
            || !(bytes as usize).is_multiple_of(std::mem::size_of::<libc::pid_t>())
        {
            return Err("mole_identity_unavailable: process inventory incomplete".into());
        }
        pids.truncate(bytes as usize / std::mem::size_of::<libc::pid_t>());
        inspect_processes(target, &pids, started, |pid| {
            let mut buffer = [0u8; 4096]; // PROC_PIDPATHINFO_MAXSIZE
            let count =
                unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
            if count <= 0 {
                let error = std::io::Error::last_os_error();
                // Only a vanished process is demonstrably irrelevant. EPERM,
                // EACCES, empty/truncated paths, etc. are not evidence of safety.
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(None);
                }
                return Err(format!(
                    "mole_identity_unavailable: executable for pid {pid}: {error}"
                ));
            }
            let end = buffer
                .iter()
                .position(|&b| b == 0)
                .filter(|&n| n > 0 && n < buffer.len() - 1)
                .ok_or("mole_identity_unavailable: truncated process executable")?;
            Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(
                &buffer[..end],
            ))))
        })
    }
    fn app_guard(path: &Path, bid: &str) -> Result<(), String> {
        runtime_guard(path, bid)?;
        let home = home()?;
        if path.extension().and_then(|s| s.to_str()) != Some("app")
            || (path.parent() != Some(Path::new("/Applications"))
                && path.parent() != Some(home.join("Applications").as_path()))
        {
            return Err("mole_path_unsupported".into());
        }
        if bundle_id(path)? != bid {
            return Err("mole_app_changed".into());
        }
        permissions(path)
    }
    fn shared(
        preview_path: &Path,
        bid: &str,
        known: &[PathBuf],
        app_present: bool,
    ) -> Result<(), String> {
        let mut paths: HashSet<PathBuf> = known.iter().cloned().collect();
        for url in NSWorkspace::sharedWorkspace()
            .URLsForApplicationsWithBundleIdentifier(&NSString::from_str(bid))
            .iter()
        {
            let path = url.path().ok_or("mole_shared_data")?;
            paths.insert(PathBuf::from(path.to_string()));
        }
        for path in paths {
            if path == preview_path {
                continue;
            }
            // Do not dismiss an inaccessible or symlinked registered installation.
            match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err("mole_shared_data".into()),
            }
        }
        if app_present && bundle_id(preview_path)? != bid {
            return Err("mole_app_changed".into());
        }
        if !app_present
            && !std::fs::symlink_metadata(preview_path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Err("mole_app_changed".into());
        }
        Ok(())
    }
    pub(super) fn inventory(rows: Vec<MoleApp>) -> Result<Inventory, String> {
        let mut entries = Vec::with_capacity(rows.len());
        for mut row in rows {
            let path = Path::new(&row.path);
            let guard = capture(path, RemovalKind::App);
            // Supplement Mole's claimed identity with the actual bundle metadata,
            // including blocked installations, for duplicate ownership checks.
            if let Ok(actual) = bundle_id(path) {
                row.bundle_id = actual;
            }
            let blocked = guard
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|_| app_guard(path, &row.bundle_id))
                .err();
            entries.push(InventoryEntry {
                summary: AppSummary {
                    id: id(),
                    name: row.name,
                    bundle_id: row.bundle_id,
                    path: row.path,
                    size_label: row.size,
                    source: row.source,
                    blocked_reason: blocked,
                },
                guard: guard.ok(),
            });
        }
        Ok(Inventory {
            generation: id(),
            entries,
        })
    }
    fn candidate_specs(home: &Path, bid: &str) -> Vec<(RemovalKind, PathBuf)> {
        let library = home.join("Library");
        vec![
            (RemovalKind::Cache, library.join("Caches").join(bid)),
            (
                RemovalKind::Preferences,
                library.join("Preferences").join(format!("{bid}.plist")),
            ),
            (
                RemovalKind::SavedState,
                library
                    .join("Saved Application State")
                    .join(format!("{bid}.savedState")),
            ),
            (RemovalKind::Webkit, library.join("WebKit").join(bid)),
            (
                RemovalKind::HttpStorage,
                library.join("HTTPStorages").join(bid),
            ),
            (
                RemovalKind::Support,
                library.join("Application Support").join(bid),
            ),
            (RemovalKind::Container, library.join("Containers").join(bid)),
        ]
    }
    pub(super) fn preview(
        entry: InventoryEntry,
        known_paths: Vec<PathBuf>,
        generation: String,
    ) -> Result<Preview, String> {
        let app = entry.guard.ok_or("mole_identity_unavailable")?;
        unchanged(&app)?;
        app_guard(Path::new(&app.row.path), &entry.summary.bundle_id)?;
        let duplicate = shared(
            Path::new(&app.row.path),
            &entry.summary.bundle_id,
            &known_paths,
            true,
        )
        .err();
        let mut related = Vec::new();
        let mut excluded = Vec::new();
        for (kind, path) in candidate_specs(&home()?, &entry.summary.bundle_id) {
            match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                _ => {}
            }
            let candidate = capture(&path, kind).and_then(|item| {
                if let Some(error) = &duplicate {
                    return Err(error.clone());
                }
                // Container ownership needs independently validated container-manager
                // metadata. Exact directory spelling alone is not ownership evidence.
                if kind == RemovalKind::Container {
                    return Err(
                        "mole_identity_unavailable: container ownership metadata is not verified"
                            .into(),
                    );
                }
                permissions(&path)?;
                Ok(item)
            });
            match candidate {
                Ok(item) => related.push(item),
                Err(reason) => excluded.push(RemovalExclusion {
                    kind,
                    path: path.to_string_lossy().into_owned(),
                    reason,
                }),
            }
        }
        let now = Instant::now();
        let wire = AppRemovalPreview {
            token: id(),
            generation,
            expires_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "mole_identity_unavailable")?
                .as_millis() as u64
                + TTL.as_millis() as u64,
            app: app.row.clone(),
            related: related.iter().map(|r| r.row.clone()).collect(),
            excluded,
        };
        Ok(Preview {
            wire,
            deadline: now + TTL,
            bundle_id: entry.summary.bundle_id,
            known_paths,
            app,
            related,
        })
    }
    pub(super) struct Foundation;
    impl Adapter for Foundation {
        fn guard(
            &self,
            preview: &Preview,
            item: &Guarded,
            app_present: bool,
        ) -> Result<(), String> {
            if RUN_STATE
                .lock()
                .map_err(|_| "mole_shutting_down")?
                .shutting_down
            {
                return Err("mole_shutting_down".into());
            }
            if Instant::now() >= preview.deadline {
                return Err("mole_preview_stale".into());
            }
            runtime_guard(Path::new(&preview.app.row.path), &preview.bundle_id)?;
            if app_present {
                unchanged(&preview.app)?;
                app_guard(Path::new(&preview.app.row.path), &preview.bundle_id)?;
                if !preview.related.is_empty() {
                    shared(
                        Path::new(&preview.app.row.path),
                        &preview.bundle_id,
                        &preview.known_paths,
                        true,
                    )?;
                }
            }
            unchanged(item)?;
            if item.row.kind != RemovalKind::App {
                shared(
                    Path::new(&preview.app.row.path),
                    &preview.bundle_id,
                    &preview.known_paths,
                    app_present,
                )?;
                if !candidate_specs(&home()?, &preview.bundle_id)
                    .iter()
                    .any(|(kind, path)| *kind == item.row.kind && path == Path::new(&item.row.path))
                {
                    return Err("mole_path_unsupported".into());
                }
            }
            Ok(())
        }
        fn trash(&self, item: &Guarded) -> NativeResult {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&item.row.path));
            let mut destination = None;
            let result = NSFileManager::defaultManager()
                .trashItemAtURL_resultingItemURL_error(&url, Some(&mut destination));
            NativeResult {
                error: result
                    .err()
                    .map(|e| format!("mole_trash_failed: {}", e.localizedDescription())),
                destination: destination
                    .and_then(|u| u.path())
                    .map(|s| PathBuf::from(s.to_string())),
            }
        }
        fn reconcile(&self, item: &Guarded, result: &NativeResult) -> RemovalStatus {
            let expected = &item.chain[0].1;
            let source = std::fs::symlink_metadata(&item.row.path);
            let missing = source
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            let destination_matches = result.destination.as_ref().is_some_and(|p| {
                identity(p).is_ok_and(|i| i.dev == expected.dev && i.ino == expected.ino)
            });
            if missing && destination_matches {
                return RemovalStatus::Moved;
            }
            if identity(Path::new(&item.row.path)).is_ok_and(|i| i == *expected)
                && source
                    .as_ref()
                    .is_ok_and(|m| m.modified().ok() == Some(item.modified) && m.len() == item.len)
                && !destination_matches
                && result.error.is_some()
            {
                return RemovalStatus::Failed;
            }
            RemovalStatus::Unknown
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};

        #[test]
        fn running_helpers_match_components_not_string_prefixes() {
            let target = Path::new("/Applications/A.app");
            for (bundle, executable) in [
                (
                    Some(Path::new(
                        "/Applications/A.app/Contents/Library/LoginItems/Helper.app",
                    )),
                    None,
                ),
                (
                    None,
                    Some(Path::new("/Applications/A.app/Contents/MacOS/helper")),
                ),
            ] {
                assert!(running_match(
                    target,
                    "org.fixture.a",
                    Some("org.distinct.helper"),
                    bundle,
                    executable
                ));
            }
            for other in [
                "/Applications/A.app.backup/Contents/MacOS/helper",
                "/Applications/AA.app",
                "/Applications/A.application",
            ] {
                assert!(!running_match(
                    target,
                    "org.fixture.a",
                    Some("org.other"),
                    Some(Path::new(other)),
                    Some(Path::new(other))
                ));
            }
            assert!(running_match(
                target,
                "org.fixture.a",
                Some("ORG.FIXTURE.A"),
                None,
                None
            ));
        }
        #[test]
        fn native_process_inventory_helpers_uncertainty_and_deadline() {
            let target = Path::new("/Applications/A.app");
            assert_eq!(
                inspect_processes(target, &[42], Instant::now(), |_| {
                    Ok(Some(target.join("Contents/Helpers/no-bundle")))
                })
                .unwrap_err(),
                "mole_app_running"
            );
            assert!(
                inspect_processes(target, &[0, 42, 43], Instant::now(), |pid| {
                    assert_ne!(pid, 0);
                    Ok(if pid == 42 {
                        None
                    } else {
                        Some(PathBuf::from("/Applications/A.app.backup/helper"))
                    })
                })
                .is_ok()
            );
            assert!(inspect_processes(target, &[42], Instant::now(), |_| Err(
                "mole_identity_unavailable: permission denied".into()
            ))
            .is_err());
            assert!(
                inspect_processes(target, &[42], Instant::now(), |_| Ok(Some(PathBuf::from(
                    "relative"
                ))))
                .is_err()
            );
            assert!(inspect_processes(
                target,
                &[42],
                Instant::now() - Duration::from_secs(3),
                |_| panic!("expired inventory must not query")
            )
            .is_err());
        }
        #[test]
        fn protected_entry_parent_flags_and_volume_states_fail_closed() {
            for flag in [
                libc::UF_IMMUTABLE,
                libc::SF_IMMUTABLE,
                libc::UF_APPEND,
                libc::SF_APPEND,
                0x00080000,
                0x00100000,
            ] {
                assert!(protected_flags(flag));
            }
            assert!(!protected_flags(0));
            let local = libc::MNT_LOCAL as u32;
            assert!(writable_local_volume(local));
            assert!(!writable_local_volume(0));
            for flag in [
                libc::MNT_RDONLY as u32,
                libc::MNT_SNAPSHOT as u32,
                0x00200000,
            ] {
                assert!(!writable_local_volume(local | flag));
            }
        }

        #[test]
        fn parent_nounlink_does_not_block_child_authorization_but_target_does() {
            const SF_NOUNLINK: u32 = 0x00100000;
            assert!(!protected_parent_flags(SF_NOUNLINK));
            assert!(protected_flags(SF_NOUNLINK));
            // A nounlink parent must not mask other child-modification vetoes.
            for flag in [
                libc::UF_IMMUTABLE,
                libc::SF_IMMUTABLE,
                libc::UF_APPEND,
                libc::SF_APPEND,
                0x00080000,
            ] {
                assert!(protected_parent_flags(flag));
                assert!(protected_parent_flags(SF_NOUNLINK | flag));
                assert!(protected_flags(SF_NOUNLINK | flag));
            }
        }

        // SDK sys/acl.h and membership.h. Test-only native ACL manipulation is
        // restricted to newly created disposable entries, with exact restoration.
        type Acl = *mut libc::c_void;
        unsafe extern "C" {
            fn acl_get_file(path: *const libc::c_char, kind: libc::c_int) -> Acl;
            fn acl_init(count: libc::c_int) -> Acl;
            fn acl_dup(acl: Acl) -> Acl;
            fn acl_free(acl: Acl) -> libc::c_int;
            fn acl_create_entry_np(
                acl: *mut Acl,
                entry: *mut Acl,
                index: libc::c_int,
            ) -> libc::c_int;
            fn acl_set_tag_type(entry: Acl, tag: libc::c_int) -> libc::c_int;
            fn acl_set_qualifier(entry: Acl, qualifier: *const libc::c_void) -> libc::c_int;
            fn acl_set_permset_mask_np(entry: Acl, mask: u64) -> libc::c_int;
            fn acl_set_file(path: *const libc::c_char, kind: libc::c_int, acl: Acl) -> libc::c_int;
            fn mbr_uid_to_uuid(uid: libc::uid_t, uuid: *mut u8) -> libc::c_int;
        }
        struct FixtureAcl {
            path: std::ffi::CString,
            original: Acl,
        }
        impl FixtureAcl {
            fn deny(path: &Path, fixture: &Fixture, mask: u64) -> Self {
                assert!(path == fixture.0 || path.parent() == Some(fixture.0.as_path()));
                let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                unsafe {
                    let mut original = acl_get_file(path.as_ptr(), 0x100);
                    if original.is_null() {
                        // Darwin reports ENOENT for an existing file with no ACL.
                        assert_eq!(
                            std::io::Error::last_os_error().raw_os_error(),
                            Some(libc::ENOENT)
                        );
                        original = acl_init(0);
                    }
                    assert!(!original.is_null(), "fixture ACL allocation must succeed");
                    let restore = Self { path, original };
                    struct WorkingAcl(Acl);
                    impl Drop for WorkingAcl {
                        fn drop(&mut self) {
                            unsafe {
                                acl_free(self.0);
                            }
                        }
                    }
                    let mut working = WorkingAcl(acl_dup(original));
                    assert!(!working.0.is_null());
                    let mut entry = std::ptr::null_mut();
                    assert_eq!(acl_create_entry_np(&mut working.0, &mut entry, 0), 0);
                    let mut uuid = [0u8; 16];
                    assert_eq!(mbr_uid_to_uuid(libc::geteuid(), uuid.as_mut_ptr()), 0);
                    assert_eq!(acl_set_tag_type(entry, 2 /* ACL_EXTENDED_DENY */), 0);
                    assert_eq!(acl_set_qualifier(entry, uuid.as_ptr().cast()), 0);
                    assert_eq!(acl_set_permset_mask_np(entry, mask), 0);
                    assert_eq!(acl_set_file(restore.path.as_ptr(), 0x100, working.0), 0);
                    restore
                }
            }
        }
        impl Drop for FixtureAcl {
            fn drop(&mut self) {
                unsafe {
                    let result = acl_set_file(self.path.as_ptr(), 0x100, self.original);
                    acl_free(self.original);
                    assert_eq!(result, 0, "restore only the disposable fixture ACL");
                }
            }
        }
        #[test]
        fn native_acl_delete_denial_preflights_entire_batch_without_trash() {
            assert_ne!(
                unsafe { libc::geteuid() },
                0,
                "run fixture tests without root"
            );
            let f = Fixture::new();
            let app_path = f.file("app");
            let related_path = f.file("related");
            assert!(permissions(&app_path).is_ok());
            assert!(permissions(&related_path).is_ok());
            // A deny unrelated to deletion is not grounds for blocking an app.
            {
                let _unrelated = FixtureAcl::deny(&related_path, &f, 1 << 3 /* execute */);
                assert!(permissions(&related_path).is_ok());
            }
            // Both sides denied: no parent delete_child grant can override the
            // target DELETE deny, or vice versa. Mode W_OK still succeeds.
            let _target_acl = FixtureAcl::deny(&related_path, &f, 1 << 4);
            let _parent_acl = FixtureAcl::deny(&f.0, &f, 1 << 6);
            let name = std::ffi::CString::new(related_path.as_os_str().as_bytes()).unwrap();
            assert_eq!(
                unsafe {
                    libc::faccessat(libc::AT_FDCWD, name.as_ptr(), libc::W_OK, libc::AT_EACCESS)
                },
                0
            );
            assert_eq!(
                permissions(&related_path).unwrap_err(),
                "mole_permission_required"
            );
            let app = capture(&app_path, RemovalKind::App).unwrap();
            let related = capture(&related_path, RemovalKind::Cache).unwrap();
            let p = Preview {
                wire: AppRemovalPreview {
                    token: id(),
                    generation: id(),
                    expires_at_ms: 0,
                    app: app.row.clone(),
                    related: vec![related.row.clone()],
                    excluded: vec![],
                },
                deadline: Instant::now() + TTL,
                bundle_id: "org.fixture.acl".into(),
                known_paths: vec![app_path],
                app,
                related: vec![related],
            };
            struct PermissionOnly;
            impl Adapter for PermissionOnly {
                fn guard(&self, _: &Preview, item: &Guarded, _: bool) -> Result<(), String> {
                    permissions(Path::new(&item.row.path))
                }
                fn trash(&self, _: &Guarded) -> NativeResult {
                    panic!("ACL preflight must make zero native writes")
                }
                fn reconcile(&self, _: &Guarded, _: &NativeResult) -> RemovalStatus {
                    panic!("no native calls")
                }
            }
            let out = execute(&p, &[p.related[0].row.id.clone()], &PermissionOnly).unwrap();
            assert_eq!(
                out.stopped_reason.as_deref(),
                Some("mole_permission_required")
            );
            assert_eq!(out.items.len(), 2);
            assert!(out
                .items
                .iter()
                .all(|r| r.status == RemovalStatus::NotAttempted));
            assert_eq!(std::fs::read(&related_path).unwrap(), b"fixture");
        }

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let root = std::env::temp_dir()
                    .canonicalize()
                    .unwrap()
                    .join(format!("sayknow-removal-{}", id()));
                std::fs::create_dir(&root).unwrap();
                Self(root)
            }
            fn file(&self, name: &str) -> PathBuf {
                let path = self.0.join(name);
                std::fs::write(&path, b"fixture").unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                path
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                // Only agent-created immediate fixture children; never recurse.
                for entry in std::fs::read_dir(&self.0).unwrap() {
                    let path = entry.unwrap().path();
                    if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                        std::fs::remove_dir(path).unwrap();
                    } else {
                        std::fs::remove_file(path).unwrap();
                    }
                }
                std::fs::remove_dir(&self.0).unwrap();
            }
        }
        #[test]
        fn symlink_leaf_and_ancestor_are_never_candidates() {
            let f = Fixture::new();
            let path = f.file("data");
            symlink(&path, f.0.join("link")).unwrap();
            assert!(capture(&f.0.join("link"), RemovalKind::Cache).is_err());
            symlink(&f.0, f.0.join("parent")).unwrap();
            assert!(capture(&f.0.join("parent/data"), RemovalKind::Cache).is_err());
        }
        #[test]
        fn identity_swap_and_effective_readonly_permissions_fail_closed() {
            let f = Fixture::new();
            let path = f.file("data");
            let item = capture(&path, RemovalKind::Cache).unwrap();
            std::fs::rename(&path, f.0.join("original")).unwrap();
            f.file("data");
            assert!(unchanged(&item).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
            assert!(permissions(&path).is_err());
        }
        #[test]
        fn exact_id_candidates_do_not_include_names_shared_roots_or_documents() {
            let specs = candidate_specs(Path::new("/Users/fixture"), "org.fixture.App");
            assert_eq!(specs.len(), 7);
            assert!(specs
                .iter()
                .all(|(_, p)| p.starts_with("/Users/fixture/Library")));
            assert_eq!(
                specs[1].1,
                Path::new("/Users/fixture/Library/Preferences/org.fixture.App.plist")
            );
            for (_, path) in &specs {
                assert!(!path.to_string_lossy().contains("Group Containers"));
                assert!(!path.to_string_lossy().contains("Documents"));
            }
            for (i, (_, path)) in specs.iter().enumerate() {
                assert!(!specs[..i]
                    .iter()
                    .any(|(_, other)| path.starts_with(other) || other.starts_with(path)));
            }
        }
        #[test]
        fn system_and_measure_apps_are_protected_before_filesystem_lookup() {
            assert_eq!(
                runtime_guard(
                    Path::new("/Applications/Measure.app"),
                    "com.sayknow.measure"
                )
                .unwrap_err(),
                "mole_self_app"
            );
            assert_eq!(
                runtime_guard(
                    Path::new("/System/Applications/Test.app"),
                    "org.fixture.app"
                )
                .unwrap_err(),
                "mole_protected_app"
            );
            assert_eq!(
                runtime_guard(Path::new("/Applications/Test.app"), "com.apple.Test").unwrap_err(),
                "mole_protected_app"
            );
        }
        #[test]
        fn reconciliation_requires_observed_identity_not_native_success_alone() {
            let f = Fixture::new();
            let path = f.file("data");
            let item = capture(&path, RemovalKind::Cache).unwrap();
            let adapter = Foundation;
            let success_without_destination = NativeResult {
                error: None,
                destination: None,
            };
            assert_eq!(
                adapter.reconcile(&item, &success_without_destination),
                RemovalStatus::Unknown
            );
            let failure = NativeResult {
                error: Some("native failure".into()),
                destination: None,
            };
            assert_eq!(adapter.reconcile(&item, &failure), RemovalStatus::Failed);
            let destination = f.0.join("moved");
            std::fs::rename(&path, &destination).unwrap();
            let moved_error = NativeResult {
                error: Some("native failure".into()),
                destination: Some(destination),
            };
            assert_eq!(adapter.reconcile(&item, &moved_error), RemovalStatus::Moved);
            f.file("data");
            assert_eq!(
                adapter.reconcile(&item, &moved_error),
                RemovalStatus::Unknown
            );
        }

        #[test]
        #[ignore = "opt-in: moves only UUID disposable fixtures through Foundation and leaves them in Trash"]
        fn foundation_trash_disposable_app_and_selected_related_file() {
            // This exercises native move/reconciliation and selection, NOT real
            // inventory discovery or production application-path authorization.
            // No home-directory lookup, application inventory, rollback, or
            // permanent-delete fallback is used.
            struct PreparedFixtureAdapter<'a> {
                root: &'a Path,
                allowed: &'a [Guarded],
            }
            impl Adapter for PreparedFixtureAdapter<'_> {
                fn guard(&self, _: &Preview, item: &Guarded, _: bool) -> Result<(), String> {
                    if Path::new(&item.row.path).parent() != Some(self.root)
                        || !self.allowed.iter().any(|expected| {
                            expected.row.id == item.row.id
                                && expected.row.path == item.row.path
                                && expected.chain == item.chain
                        })
                    {
                        return Err("mole_path_unsupported".into());
                    }
                    unchanged(item)
                }
                fn trash(&self, item: &Guarded) -> NativeResult {
                    Foundation.trash(item)
                }
                fn reconcile(&self, item: &Guarded, result: &NativeResult) -> RemovalStatus {
                    Foundation.reconcile(item, result)
                }
            }

            // Clean only these exact, self-created source-side nested paths.
            // Never follow the native destination or remove anything from Trash.
            // Fixture's existing nonrecursive Drop handles the remaining root.
            struct RemainingApp(PathBuf);
            impl Drop for RemainingApp {
                fn drop(&mut self) {
                    for (path, directory) in [
                        (self.0.join("Contents/Info.plist"), false),
                        (self.0.join("Contents"), true),
                        (self.0.clone(), true),
                    ] {
                        let result = if directory {
                            std::fs::remove_dir(&path)
                        } else {
                            std::fs::remove_file(&path)
                        };
                        if let Err(error) = result {
                            if error.kind() != std::io::ErrorKind::NotFound {
                                eprintln!(
                                    "disposable fixture cleanup failed for {}: {error}",
                                    path.display()
                                );
                            }
                        }
                    }
                }
            }

            let f = Fixture::new();
            let app_path = f.0.join(format!("{}.app", id()));
            std::fs::create_dir(&app_path).unwrap();
            let _remaining_app = RemainingApp(app_path.clone());
            std::fs::set_permissions(&app_path, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::create_dir(app_path.join("Contents")).unwrap();
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
                 <key>CFBundleIdentifier</key><string>org.sayknow.fixture.{}</string>\
                 <key>CFBundlePackageType</key><string>APPL</string></dict></plist>",
                id()
            );
            std::fs::write(app_path.join("Contents/Info.plist"), plist.as_bytes()).unwrap();
            let selected_path = f.file(&format!("{}.plist", id()));
            let selected_data = format!("selected disposable fixture {}", id());
            std::fs::write(&selected_path, selected_data.as_bytes()).unwrap();
            let unselected_path = f.file(&format!("{}.unselected", id()));
            let unselected_data = format!("unselected disposable fixture {}", id());
            std::fs::write(&unselected_path, unselected_data.as_bytes()).unwrap();

            let app = capture(&app_path, RemovalKind::App).unwrap();
            let selected = capture(&selected_path, RemovalKind::Preferences).unwrap();
            let unselected = capture(&unselected_path, RemovalKind::Support).unwrap();
            let plist_identity = identity(&app_path.join("Contents/Info.plist")).unwrap();
            let allowed = [app.clone(), selected.clone()];
            let related = vec![selected.clone(), unselected.clone()];
            let preview = Preview {
                wire: AppRemovalPreview {
                    token: id(),
                    generation: id(),
                    expires_at_ms: 0,
                    app: app.row.clone(),
                    related: related.iter().map(|item| item.row.clone()).collect(),
                    excluded: vec![],
                },
                deadline: Instant::now() + TTL,
                bundle_id: format!("org.sayknow.fixture.{}", id()),
                known_paths: vec![app_path.clone()],
                app,
                related,
            };
            let adapter = PreparedFixtureAdapter {
                root: &f.0,
                allowed: &allowed,
            };
            let outcome =
                execute(&preview, std::slice::from_ref(&selected.row.id), &adapter).unwrap();
            assert_eq!(outcome.items.len(), 2, "{outcome:?}");
            assert!(outcome.stopped_reason.is_none(), "{outcome:?}");
            for (row, original) in outcome.items.iter().zip(&allowed) {
                assert_eq!(row.candidate_id, original.row.id);
                assert_eq!(row.kind, original.row.kind);
                assert_eq!(row.path, original.row.path);
                assert_eq!(row.status, RemovalStatus::Moved, "{outcome:?}");
                assert!(row.error.is_none(), "{outcome:?}");
                assert_eq!(
                    std::fs::symlink_metadata(&row.path).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
                let destination = Path::new(
                    row.trash_path
                        .as_deref()
                        .expect("observed Trash destination"),
                );
                assert!(destination.is_absolute());
                assert!(!destination.starts_with(&f.0));
                let observed = identity(destination).unwrap();
                let expected = &original.chain[0].1;
                assert_eq!((observed.dev, observed.ino), (expected.dev, expected.ino));
                if row.kind == RemovalKind::App {
                    assert!(std::fs::symlink_metadata(destination).unwrap().is_dir());
                    let moved_plist = destination.join("Contents/Info.plist");
                    let observed_plist = identity(&moved_plist).unwrap();
                    assert_eq!(
                        (observed_plist.dev, observed_plist.ino),
                        (plist_identity.dev, plist_identity.ino)
                    );
                    assert_eq!(std::fs::read(moved_plist).unwrap(), plist.as_bytes());
                } else {
                    assert_eq!(
                        std::fs::read(destination).unwrap(),
                        selected_data.as_bytes()
                    );
                }
                eprintln!(
                    "disposable fixture left in Trash: {}",
                    destination.display()
                );
            }
            assert!(!outcome
                .items
                .iter()
                .any(|row| row.candidate_id == unselected.row.id));
            assert_eq!(identity(&unselected_path).unwrap(), unselected.chain[0].1);
            assert_eq!(
                std::fs::read(&unselected_path).unwrap(),
                unselected_data.as_bytes()
            );
            unchanged(&unselected).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn item(name: &str, kind: RemovalKind) -> Guarded {
        Guarded {
            row: RemovalCandidate {
                id: name.into(),
                kind,
                path: format!("/fixture/{name}"),
                size_bytes: None,
            },
            chain: vec![],
            modified: UNIX_EPOCH,
            len: 0,
        }
    }
    fn preview(now: Instant) -> Preview {
        let app = item("app", RemovalKind::App);
        let related = vec![
            item("cache", RemovalKind::Cache),
            item("support", RemovalKind::Support),
        ];
        Preview {
            wire: AppRemovalPreview {
                token: "token".into(),
                generation: "generation".into(),
                expires_at_ms: 60_000,
                app: app.row.clone(),
                related: related.iter().map(|r| r.row.clone()).collect(),
                excluded: vec![],
            },
            deadline: now + TTL,
            bundle_id: "org.fixture.app".into(),
            known_paths: vec![],
            app,
            related,
        }
    }
    struct Fake {
        calls: RefCell<Vec<String>>,
        guards: Cell<usize>,
        fail_guard: Option<(usize, &'static str)>,
        statuses: Vec<RemovalStatus>,
        native_error: Option<usize>,
    }
    impl Fake {
        fn new(statuses: Vec<RemovalStatus>) -> Self {
            Self {
                calls: RefCell::new(vec![]),
                guards: Cell::new(0),
                fail_guard: None,
                statuses,
                native_error: None,
            }
        }
    }
    impl Adapter for Fake {
        fn guard(&self, _: &Preview, _: &Guarded, _: bool) -> Result<(), String> {
            let n = self.guards.get();
            self.guards.set(n + 1);
            if let Some((at, reason)) = self.fail_guard {
                if at == n {
                    return Err(reason.into());
                }
            }
            Ok(())
        }
        fn trash(&self, item: &Guarded) -> NativeResult {
            let n = self.calls.borrow().len();
            self.calls.borrow_mut().push(item.row.id.clone());
            NativeResult {
                error: (self.native_error == Some(n)).then(|| "native original error".into()),
                destination: None,
            }
        }
        fn reconcile(&self, _: &Guarded, _: &NativeResult) -> RemovalStatus {
            self.statuses[self.calls.borrow().len() - 1]
        }
    }
    #[test]
    fn wire_inventory_rejects_malformed_truncated_and_wrong_field_types() {
        for raw in [
            "{}",
            "[",
            "null",
            r#"[{"name":"a","bundle_id":"org.a","path":"/a","size":1,"source":"App"}]"#,
        ] {
            assert!(parse_inventory(raw).is_err());
        }
        let rows = parse_inventory(r#"[{"name":"‎A","bundle_id":"org.a","path":"/Applications/A.app","size":"1.2G","source":"App","uninstall_name":"a"}]"#).unwrap();
        assert_eq!(rows[0].size, "1.2G");
        assert_eq!(rows[0].path, "/Applications/A.app");
    }
    #[test]
    fn metadata_identifier_cannot_inject_paths() {
        for value in [
            "",
            ".",
            "..",
            "../Documents",
            "org.a/../../x",
            "/org.a",
            "org..a",
            "org.a\\x",
            "org.a\0x",
            "org.a*",
        ] {
            assert!(!valid_bundle_id(value), "{value:?}");
        }
        assert!(valid_bundle_id("org.fixture.Test-1"));
    }
    #[test]
    fn wire_names_and_nulls_are_exact() {
        let p = preview(Instant::now());
        let wire = serde_json::to_value(&p.wire).unwrap();
        assert!(wire.get("expires_at_ms").is_some());
        assert!(wire.get("expiresAtMs").is_none());
        assert_eq!(wire["related"][0]["kind"], "cache");
        assert!(wire["app"]["size_bytes"].is_null());
        for (kind, expected) in [
            (RemovalKind::SavedState, "saved_state"),
            (RemovalKind::HttpStorage, "http_storage"),
            (RemovalKind::Webkit, "webkit"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), expected);
        }
    }
    #[test]
    fn admission_failures_invalidate_old_write_rights() {
        let mut state = State {
            inventory: None,
            preview: Some(preview(Instant::now())),
        };
        assert!(state.preview_admission("missing", "missing").is_err());
        assert!(state.preview.is_none());
        state.preview = Some(preview(Instant::now()));
        state.list_admission();
        assert!(state.preview.is_none());
        assert!(state.inventory.is_none());
    }
    #[test]
    fn monotonic_expiry_wrong_token_and_replay_consume_once() {
        let now = Instant::now();
        for (token, time) in [("wrong", now), ("token", now + TTL)] {
            let mut state = State {
                inventory: None,
                preview: Some(preview(now)),
            };
            assert!(state.consume(token, time).is_err());
            assert!(state.consume("token", now).is_err());
        }
        let mut state = State {
            inventory: None,
            preview: Some(preview(now)),
        };
        assert!(state
            .consume("token", now + TTL - Duration::from_nanos(1))
            .is_ok());
        assert!(state.consume("token", now).is_err());
        state.preview = Some(preview(now));
        state.cancel("token").unwrap();
        assert!(state.consume("token", now).is_err());
        state.preview = Some(preview(now));
        assert!(state.cancel("wrong").is_err());
        assert!(state.consume("token", now).is_err());
    }
    #[test]
    fn unknown_duplicate_and_app_ids_reject_before_any_invocation() {
        let p = preview(Instant::now());
        for selected in [
            vec!["unknown".into()],
            vec!["cache".into(), "cache".into()],
            vec!["app".into()],
        ] {
            let fake = Fake::new(vec![]);
            assert!(execute(&p, &selected, &fake).is_err());
            assert!(fake.calls.borrow().is_empty());
        }
    }
    #[test]
    fn whole_selected_set_is_preflighted_before_first_write() {
        let p = preview(Instant::now());
        for reason in [
            "mole_app_changed",
            "mole_shared_data",
            "mole_app_running",
            "mole_self_app",
            "mole_protected_app",
            "mole_permission_required",
            "mole_path_unsupported",
            "mole_identity_unavailable",
            "mole_preview_stale",
            "mole_shutting_down",
        ] {
            // Every preflight position and the guard immediately before call 1.
            for position in 0..=3 {
                let mut fake = Fake::new(vec![]);
                fake.fail_guard = Some((position, reason));
                let out = execute(&p, &["cache".into(), "support".into()], &fake).unwrap();
                assert_eq!(out.stopped_reason.as_deref(), Some(reason));
                assert_eq!(
                    out.items
                        .iter()
                        .map(|r| r.candidate_id.as_str())
                        .collect::<Vec<_>>(),
                    ["app", "cache", "support"]
                );
                assert!(out
                    .items
                    .iter()
                    .all(|r| r.status == RemovalStatus::NotAttempted && r.trash_path.is_none()));
                assert!(fake.calls.borrow().is_empty());
            }
        }
    }
    #[test]
    fn selection_preserves_display_order_and_unselected_data() {
        let p = preview(Instant::now());
        let fake = Fake::new(vec![RemovalStatus::Moved; 2]);
        execute(&p, &["support".into()], &fake).unwrap();
        assert_eq!(*fake.calls.borrow(), ["app", "support"]);
        let fake = Fake::new(vec![RemovalStatus::Moved; 3]);
        execute(&p, &["support".into(), "cache".into()], &fake).unwrap();
        assert_eq!(*fake.calls.borrow(), ["app", "cache", "support"]);
        let fake = Fake::new(vec![RemovalStatus::Moved]);
        execute(&p, &[], &fake).unwrap();
        assert_eq!(*fake.calls.borrow(), ["app"]);
    }
    #[test]
    fn overlap_is_rejected_without_writes() {
        let mut p = preview(Instant::now());
        p.related[0].row.path = "/fixture/app/nested".into();
        let fake = Fake::new(vec![]);
        assert!(execute(&p, &["cache".into()], &fake).is_err());
        assert!(fake.calls.borrow().is_empty());
    }
    #[test]
    fn app_first_failure_and_unknown_stop_remaining_rows() {
        let p = preview(Instant::now());
        for status in [RemovalStatus::Failed, RemovalStatus::Unknown] {
            let fake = Fake::new(vec![status]);
            let out = execute(&p, &["cache".into(), "support".into()], &fake).unwrap();
            assert_eq!(*fake.calls.borrow(), ["app"]);
            assert_eq!(out.items[0].status, status);
            assert_eq!(out.items[1].status, RemovalStatus::NotAttempted);
            assert!(out.stopped_reason.is_some());
        }
    }
    #[test]
    fn moved_with_native_error_preserves_error_and_stops() {
        let p = preview(Instant::now());
        let mut fake = Fake::new(vec![RemovalStatus::Moved; 2]);
        fake.native_error = Some(1);
        let out = execute(&p, &["cache".into(), "support".into()], &fake).unwrap();
        assert_eq!(out.items[1].status, RemovalStatus::Moved);
        assert_eq!(out.items[1].error.as_deref(), Some("native original error"));
        assert_eq!(out.items[2].status, RemovalStatus::NotAttempted);
        assert_eq!(*fake.calls.borrow(), ["app", "cache"]);
    }
    #[test]
    fn midbatch_identity_race_returns_structured_partial_without_retry() {
        let p = preview(Instant::now());
        let mut fake = Fake::new(vec![RemovalStatus::Moved]);
        fake.fail_guard = Some((4, "mole_app_changed"));
        let out = execute(&p, &["cache".into(), "support".into()], &fake).unwrap();
        assert_eq!(out.items[0].status, RemovalStatus::Moved);
        assert_eq!(out.items[1].status, RemovalStatus::NotAttempted);
        assert_eq!(out.stopped_reason.as_deref(), Some("mole_app_changed"));
        assert_eq!(*fake.calls.borrow(), ["app"]);
        let fake = Fake::new(vec![RemovalStatus::Moved, RemovalStatus::Unknown]);
        let out = execute(&p, &["cache".into(), "support".into()], &fake).unwrap();
        assert_eq!(out.items[1].status, RemovalStatus::Unknown);
        assert_eq!(out.items[2].status, RemovalStatus::NotAttempted);
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "read-only installed Mole inventory, one app preview, and cancellation; never removes data"]
    fn live_inventory_preview_cancel_is_read_only() {
        let inventory = list_mole_apps().expect("installed supported Mole inventory");
        assert!(!inventory.generation.is_empty());
        let unique: HashSet<_> = inventory.apps.iter().map(|app| &app.id).collect();
        assert_eq!(unique.len(), inventory.apps.len());
        let eligible = inventory
            .apps
            .iter()
            .filter(|app| app.blocked_reason.is_none())
            .count();
        eprintln!(
            "read-only inventory: {} apps, {eligible} eligible",
            inventory.apps.len()
        );
        let mut blocked = std::collections::BTreeMap::new();
        for app in &inventory.apps {
            if let Some(reason) = &app.blocked_reason {
                *blocked.entry(reason).or_insert(0) += 1;
            }
        }
        eprintln!("read-only blocked reason counts: {blocked:?}");
        let app = inventory
            .apps
            .iter()
            .find(|app| app.blocked_reason.is_none())
            .expect("live smoke requires at least one eligible installed app");
        let preview = preview_mole_app_removal(app.id.clone(), inventory.generation.clone())
            .expect("read-only preview for eligible app");
        assert_eq!(preview.generation, inventory.generation);
        assert_eq!(preview.app.path, app.path);
        assert!(preview.expires_at_ms > 0);
        cancel_mole_app_removal(preview.token.clone()).unwrap();
        assert!(STATE.lock().unwrap().preview.is_none());
        eprintln!(
            "read-only preview cancelled: {} candidates, {} exclusions",
            preview.related.len(),
            preview.excluded.len()
        );
    }
    #[test]
    fn backend_generation_and_app_ids_select_exact_installation_and_duplicate_paths() {
        let entries = ["/Applications/One.app", "/Applications/Two.app"]
            .into_iter()
            .enumerate()
            .map(|(index, path)| InventoryEntry {
                summary: AppSummary {
                    id: format!("backend-{index}"),
                    name: "Same name".into(),
                    bundle_id: "org.fixture.app".into(),
                    path: path.into(),
                    size_label: "unknown".into(),
                    source: "App".into(),
                    blocked_reason: None,
                },
                guard: None,
            })
            .collect();
        let mut state = State {
            inventory: Some(Inventory {
                generation: "generation".into(),
                entries,
            }),
            preview: None,
        };
        assert!(state.preview_admission("old", "backend-0").is_err());
        assert!(state
            .preview_admission("generation", "/Applications/One.app")
            .is_err());
        let (entry, known) = state.preview_admission("generation", "backend-1").unwrap();
        assert_eq!(entry.summary.path, "/Applications/Two.app");
        assert_eq!(
            known,
            [
                PathBuf::from("/Applications/One.app"),
                PathBuf::from("/Applications/Two.app")
            ]
        );
        state.list_admission();
        assert!(state.preview_admission("generation", "backend-1").is_err());
    }
}
