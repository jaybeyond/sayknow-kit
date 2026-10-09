//! Backend-owned, one-shot removal snapshots. The only write adapter is Foundation Trash.
use super::{RunPermit, RUN_STATE};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

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
    /// The app is open now and will be quit before it is moved, as Mole does.
    pub running: bool,
    /// The bundle is owned by another user or root, so the move goes through
    /// the macOS administrator prompt, like Mole's `sudo mv` to the user Trash.
    pub needs_admin: bool,
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
    /// Runs once after confirmation and before any guard: Mole quits the app
    /// right before deleting it rather than refusing a running one.
    fn prepare(&self, _preview: &Preview) -> Result<(), String> {
        Ok(())
    }
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
    if let Err(error) = adapter.prepare(preview) {
        outcome.stopped_reason = Some(error.clone());
        outcome.items[0].error = Some(error);
        return Ok(outcome);
    }
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
        let outcome = execute(&preview, &selected_candidate_ids, &native::Foundation)?;
        // The webview is the only other place this outcome lands, and a
        // removal that "does nothing" is undiagnosable without the reason.
        for item in &outcome.items {
            log::info!(
                "app removal: {:?} {:?} {} error={:?}",
                item.kind,
                item.status,
                item.path,
                item.error
            );
        }
        if let Some(reason) = &outcome.stopped_reason {
            log::warn!("app removal stopped: {reason}");
        }
        Ok(outcome)
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
        unchanged_identity(item)?;
        permissions(Path::new(&item.row.path))
    }
    /// The identity half of `unchanged`, without the user-permission check, so
    /// a root-owned bundle (admin move) is still tied to what the preview saw.
    fn unchanged_identity(item: &Guarded) -> Result<(), String> {
        for (path, expected) in &item.chain {
            if identity(path)? != *expected {
                return Err("mole_app_changed".into());
            }
        }
        let m = std::fs::symlink_metadata(&item.row.path).map_err(|_| "mole_app_changed")?;
        if m.modified().ok() != Some(item.modified) || m.len() != item.len {
            return Err("mole_app_changed".into());
        }
        Ok(())
    }
    /// Mole's `needs_sudo`: an app bundle the user cannot move themselves —
    /// owned by root or another user, or in a folder they cannot write — is
    /// moved through the macOS administrator prompt instead of being refused.
    /// Everything else that blocks a user move (immutable flags, read-only or
    /// remote volumes, symlinks, hard links) stays a hard refusal.
    fn needs_admin(path: &Path) -> Result<bool, String> {
        match permissions(path) {
            Ok(()) => Ok(false),
            Err(error) if error == "mole_permission_required" => {
                let m = std::fs::symlink_metadata(path).map_err(|_| "mole_identity_unavailable")?;
                let parent = path.parent().ok_or("mole_path_unsupported")?;
                let parent_metadata =
                    std::fs::symlink_metadata(parent).map_err(|_| "mole_identity_unavailable")?;
                let flags = std::os::macos::fs::MetadataExt::st_flags(&m);
                let parent_flags = std::os::macos::fs::MetadataExt::st_flags(&parent_metadata);
                let name = std::ffi::CString::new(path.as_os_str().as_bytes())
                    .map_err(|_| "mole_path_unsupported")?;
                let mut volume = std::mem::MaybeUninit::<libc::statfs>::uninit();
                if unsafe { libc::statfs(name.as_ptr(), volume.as_mut_ptr()) } != 0
                    || !writable_local_volume(unsafe { volume.assume_init() }.f_flags)
                    || protected_flags(flags)
                    || protected_parent_flags(parent_flags)
                    || !m.is_dir()
                    || !parent_metadata.is_dir()
                {
                    return Err(error);
                }
                Ok(true)
            }
            Err(error) => Err(error),
        }
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
    /// Refusals that no confirmation can override: this app itself and macOS.
    fn identity_guard(path: &Path, bid: &str) -> Result<(), String> {
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
        Ok(())
    }
    fn running_apps(
        path: &Path,
        bid: &str,
    ) -> Vec<objc2::rc::Retained<objc2_app_kit::NSRunningApplication>> {
        NSWorkspace::sharedWorkspace()
            .runningApplications()
            .iter()
            .filter(|running| {
                let bundle = running
                    .bundleURL()
                    .and_then(|u| u.path())
                    .map(|s| PathBuf::from(s.to_string()));
                let executable = running
                    .executableURL()
                    .and_then(|u| u.path())
                    .map(|s| PathBuf::from(s.to_string()));
                let running_id = running.bundleIdentifier().map(|s| s.to_string());
                running_match(
                    path,
                    bid,
                    running_id.as_deref(),
                    bundle.as_deref(),
                    executable.as_deref(),
                )
            })
            .collect()
    }
    fn running_now(path: &Path, bid: &str) -> Result<bool, String> {
        Ok(!running_apps(path, bid).is_empty() || !bundle_pids(path)?.is_empty())
    }
    fn runtime_guard(path: &Path, bid: &str) -> Result<(), String> {
        identity_guard(path, bid)?;
        if running_now(path, bid)? {
            return Err("mole_app_running".into());
        }
        Ok(())
    }
    fn wait_until(limit: Duration, done: impl Fn() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < limit {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        done()
    }
    /// Mole's `force_kill_app`, without a shell: ask the app to quit (the same
    /// Quit Apple Event its menu sends, so it can save), force it if it is
    /// still up, then stop helpers running from inside the bundle. Whatever
    /// survives is caught by the guard that runs right after, which refuses.
    fn quit_app(path: &Path, bid: &str) {
        let apps = running_apps(path, bid);
        let all_gone = || apps.iter().all(|app| app.isTerminated());
        if !apps.is_empty() {
            log::info!(
                "app removal: quitting {} running instance(s) of {bid}",
                apps.len()
            );
            for app in &apps {
                app.terminate();
            }
            if !wait_until(Duration::from_secs(5), all_gone) {
                log::warn!("app removal: {bid} did not quit, forcing it");
                for app in &apps {
                    app.forceTerminate();
                }
                wait_until(Duration::from_secs(3), all_gone);
            }
        }
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let helpers = bundle_pids(path).unwrap_or_default();
            if helpers.is_empty() {
                return;
            }
            log::info!(
                "app removal: signalling {} helper process(es) in {}",
                helpers.len(),
                path.display()
            );
            for pid in helpers {
                unsafe {
                    libc::kill(pid, signal);
                }
            }
            wait_until(Duration::from_secs(2), || {
                bundle_pids(path).is_ok_and(|p| p.is_empty())
            });
        }
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
    /// Pids whose executable lives inside `target`. Uncertainty fails closed:
    /// an unreadable live process is an error, not evidence that it is unrelated.
    fn matching_processes(
        target: &Path,
        pids: &[libc::pid_t],
        started: Instant,
        mut executable: impl FnMut(libc::pid_t) -> Result<Option<PathBuf>, String>,
    ) -> Result<Vec<libc::pid_t>, String> {
        let mut matches = Vec::new();
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
                    matches.push(pid);
                }
            }
        }
        if started.elapsed() >= Duration::from_secs(2) {
            return Err("mole_identity_unavailable: process inventory timeout".into());
        }
        Ok(matches)
    }
    fn bundle_pids(target: &Path) -> Result<Vec<libc::pid_t>, String> {
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
        matching_processes(target, &pids, started, |pid| {
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
    /// Where the app is and who may move it, without the running check: a
    /// running app is quit at removal time, not refused (Mole's behaviour).
    /// Returns whether the move needs the administrator prompt.
    fn app_location(path: &Path, bid: &str) -> Result<bool, String> {
        identity_guard(path, bid)?;
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
        needs_admin(path)
    }
    /// Right before a write: everything in `app_location`, and nothing from the
    /// bundle may still be running.
    fn app_guard(path: &Path, bid: &str) -> Result<(), String> {
        app_location(path, bid)?;
        runtime_guard(path, bid)
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
            // Running and admin-owned apps are removable (quit / admin prompt at
            // removal time); only location, identity and protection block here.
            let blocked = guard
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|_| app_location(path, &row.bundle_id))
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
        unchanged_identity(&app)?;
        let app_path = Path::new(&app.row.path);
        let needs_admin = app_location(app_path, &entry.summary.bundle_id)?;
        let running = running_now(app_path, &entry.summary.bundle_id)?;
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
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "mole_identity_unavailable")?
                .as_millis() as u64
                + TTL.as_millis() as u64,
            app: app.row.clone(),
            related: related.iter().map(|r| r.row.clone()).collect(),
            excluded,
            running,
            needs_admin,
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
    impl Foundation {
        fn alive(preview: &Preview) -> Result<(), String> {
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
            Ok(())
        }
    }
    impl Adapter for Foundation {
        fn prepare(&self, preview: &Preview) -> Result<(), String> {
            Self::alive(preview)?;
            let path = Path::new(&preview.app.row.path);
            // Never quit anything the preview did not resolve to this exact bundle.
            identity_guard(path, &preview.bundle_id)?;
            unchanged_identity(&preview.app)?;
            quit_app(path, &preview.bundle_id);
            Ok(())
        }
        fn guard(
            &self,
            preview: &Preview,
            item: &Guarded,
            app_present: bool,
        ) -> Result<(), String> {
            Self::alive(preview)?;
            let app_path = Path::new(&preview.app.row.path);
            if app_present {
                unchanged_identity(&preview.app)?;
                app_guard(app_path, &preview.bundle_id)?;
                if !preview.related.is_empty() {
                    shared(app_path, &preview.bundle_id, &preview.known_paths, true)?;
                }
            } else {
                runtime_guard(app_path, &preview.bundle_id)?;
            }
            if item.row.kind == RemovalKind::App {
                // Ownership was settled by app_guard: user-movable or admin move.
                unchanged_identity(item)?;
            } else {
                unchanged(item)?;
                shared(
                    app_path,
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
        /// Mole's routing: a bundle the user cannot move goes through the
        /// administrator prompt (`sudo mv -n` to the user Trash); everything
        /// else is a direct Trash move, and an app that the direct move cannot
        /// take (App Management, Finder-only rights) is retried through Finder.
        /// There is no permanent-delete fallback.
        fn trash(&self, item: &Guarded) -> NativeResult {
            let path = Path::new(&item.row.path);
            if item.row.kind == RemovalKind::App {
                match needs_admin(path) {
                    Ok(true) => {
                        log::info!("app removal: {} needs administrator rights", path.display());
                        return admin_trash(path);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return NativeResult {
                            error: Some(error),
                            destination: None,
                        }
                    }
                }
            }
            let direct = foundation_trash(path);
            if direct.error.is_none() || item.row.kind != RemovalKind::App {
                return direct;
            }
            // Retry only an untouched source; a half-known first attempt is
            // reconciled as is, never repeated.
            if unchanged_identity(item).is_err() {
                return direct;
            }
            let first = direct.error.unwrap_or_default();
            log::warn!("app removal: direct Trash move failed ({first}); retrying through Finder");
            let finder = finder_trash(path);
            NativeResult {
                error: finder.error.map(|e| format!("{first}; {e}")),
                destination: finder.destination,
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

    /// Keep the domain/code (and the underlying POSIX cause) next to the
    /// localized text: "you don't have permission" alone cannot tell a TCC
    /// App Management denial from an ownership or volume problem.
    fn trash_error(error: &objc2_foundation::NSError) -> String {
        let mut text = format!(
            "mole_trash_failed: {} ({} {})",
            error.localizedDescription(),
            error.domain(),
            error.code()
        );
        let underlying = error
            .userInfo()
            .objectForKey(unsafe { objc2_foundation::NSUnderlyingErrorKey })
            .and_then(|value| value.downcast::<objc2_foundation::NSError>().ok());
        if let Some(cause) = underlying {
            text.push_str(&format!("; underlying {} {}", cause.domain(), cause.code()));
        }
        text
    }

    fn foundation_trash(path: &Path) -> NativeResult {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let mut destination = None;
        let result = NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut destination));
        NativeResult {
            error: result.err().map(|e| trash_error(&e)),
            destination: destination
                .and_then(|u| u.path())
                .map(|s| PathBuf::from(s.to_string())),
        }
    }

    /// AppleScript error numbers that are not a generic failure.
    fn osascript_error(context: &str, stderr: &str) -> String {
        let stderr = stderr.trim();
        if stderr.contains("(-128)") {
            format!("mole_cancelled: {context} was cancelled")
        } else if stderr.contains("(-1743)") {
            format!("mole_permission_required: {context} is not allowed in Privacy & Security > Automation ({stderr})")
        } else {
            format!("mole_trash_failed: {context}: {stderr}")
        }
    }

    fn osascript(lines: &[&str], args: &[&str], label: &str) -> Result<String, String> {
        let mut command = std::process::Command::new("/usr/bin/osascript");
        for line in lines {
            command.arg("-e").arg(line);
        }
        command.args(args).stdin(std::process::Stdio::null());
        // Long enough for the user to answer an Automation or password prompt.
        let run = super::super::run_pipe_command(
            command,
            label.into(),
            Duration::from_secs(180),
            &RUN_STATE,
        )?;
        if run.ok {
            Ok(run.stdout.trim().to_string())
        } else {
            Err(osascript_error(label, &run.stderr))
        }
    }

    /// Mole's AppleScript fallback (`tell application "Finder" to delete`).
    /// Finder may move app bundles that a direct call from another app cannot.
    fn finder_trash(path: &Path) -> NativeResult {
        let result = osascript(
            &[
                "on run argv",
                "tell application \"Finder\" to set moved to delete (POSIX file (item 1 of argv))",
                "return POSIX path of (moved as alias)",
                "end run",
            ],
            &[&path.to_string_lossy()],
            "Finder Trash move",
        );
        match result {
            Ok(moved) => NativeResult {
                error: None,
                destination: Some(PathBuf::from(moved.trim_end_matches('/'))),
            },
            Err(error) => NativeResult {
                error: Some(error),
                destination: None,
            },
        }
    }

    /// First free "Name.app", "Name 2.app", … in the Trash folder.
    fn trash_destination(trash: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
        let source = Path::new(name);
        let stem = source.file_stem()?.to_string_lossy().into_owned();
        let extension = source.extension().map(|e| e.to_string_lossy().into_owned());
        (1..100)
            .map(|n| {
                let base = if n == 1 {
                    stem.clone()
                } else {
                    format!("{stem} {n}")
                };
                trash.join(match &extension {
                    Some(e) => format!("{base}.{e}"),
                    None => base,
                })
            })
            .find(|candidate| {
                std::fs::symlink_metadata(candidate)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            })
    }

    /// Mole's `_mole_move_sudo_path_to_user_trash`: the macOS administrator
    /// prompt authorizes one `mv -n` of the bundle into the user's own Trash.
    /// No shell string is built from the path; both paths are argv items.
    fn admin_trash(path: &Path) -> NativeResult {
        let fail = |error: String| NativeResult {
            error: Some(error),
            destination: None,
        };
        let trash = match home() {
            Ok(home) => home.join(".Trash"),
            Err(error) => return fail(error),
        };
        let trash_metadata = match std::fs::symlink_metadata(&trash) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => m,
            _ => return fail("mole_trash_failed: the user Trash folder is unavailable".into()),
        };
        // A rename keeps the bundle's identity; a cross-volume copy would not,
        // and must not be done with administrator rights.
        let same_volume = path
            .parent()
            .and_then(|parent| std::fs::symlink_metadata(parent).ok())
            .is_some_and(|parent| parent.dev() == trash_metadata.dev());
        if !same_volume {
            return fail(
                "mole_path_unsupported: the app and the Trash are on different volumes".into(),
            );
        }
        let Some(destination) = path
            .file_name()
            .and_then(|name| trash_destination(&trash, name))
        else {
            return fail("mole_trash_failed: no free name in the Trash".into());
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let result = osascript(
            &[
                "on run argv",
                "do shell script \"/bin/mv -n \" & quoted form of (item 1 of argv) & \" \" & quoted form of (item 2 of argv) with prompt (item 3 of argv) with administrator privileges",
                "end run",
            ],
            &[
                &path.to_string_lossy(),
                &destination.to_string_lossy(),
                &format!("SayKnow Kit wants to move “{name}” to the Trash."),
            ],
            "administrator Trash move",
        );
        match result {
            Ok(_) => NativeResult {
                error: None,
                destination: Some(destination),
            },
            Err(error) => fail(error),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};

        #[test]
        fn trash_errors_keep_domain_code_and_underlying_cause() {
            use objc2_foundation::{
                NSCocoaErrorDomain, NSDictionary, NSError, NSPOSIXErrorDomain, NSUnderlyingErrorKey,
            };
            let cause =
                unsafe { NSError::errorWithDomain_code_userInfo(NSPOSIXErrorDomain, 1, None) };
            let info = NSDictionary::from_slices(
                &[unsafe { NSUnderlyingErrorKey }],
                &[&*cause as &objc2::runtime::AnyObject],
            );
            let error = unsafe {
                NSError::errorWithDomain_code_userInfo(NSCocoaErrorDomain, 513, Some(&info))
            };
            let text = trash_error(&error);
            assert!(text.starts_with("mole_trash_failed: "), "{text}");
            assert!(text.contains("(NSCocoaErrorDomain 513)"), "{text}");
            assert!(
                text.ends_with("; underlying NSPOSIXErrorDomain 1"),
                "{text}"
            );
            let bare =
                unsafe { NSError::errorWithDomain_code_userInfo(NSCocoaErrorDomain, 4, None) };
            assert!(trash_error(&bare).ends_with("(NSCocoaErrorDomain 4)"));
        }

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
                matching_processes(target, &[42, 43], Instant::now(), |pid| {
                    Ok(Some(if pid == 42 {
                        target.join("Contents/Helpers/no-bundle")
                    } else {
                        PathBuf::from("/usr/bin/true")
                    }))
                })
                .unwrap(),
                [42]
            );
            assert!(
                matching_processes(target, &[0, 42, 43], Instant::now(), |pid| {
                    assert_ne!(pid, 0);
                    Ok(if pid == 42 {
                        None
                    } else {
                        Some(PathBuf::from("/Applications/A.app.backup/helper"))
                    })
                })
                .unwrap()
                .is_empty()
            );
            assert!(matching_processes(target, &[42], Instant::now(), |_| Err(
                "mole_identity_unavailable: permission denied".into()
            ))
            .is_err());
            assert!(
                matching_processes(target, &[42], Instant::now(), |_| Ok(Some(PathBuf::from(
                    "relative"
                ))))
                .is_err()
            );
            assert!(matching_processes(
                target,
                &[42],
                Instant::now() - Duration::from_secs(3),
                |_| panic!("expired inventory must not query")
            )
            .is_err());
        }
        #[test]
        fn trash_destination_never_reuses_an_existing_name() {
            let f = Fixture::new();
            assert_eq!(
                trash_destination(&f.0, std::ffi::OsStr::new("Tool.app")).unwrap(),
                f.0.join("Tool.app")
            );
            std::fs::create_dir(f.0.join("Tool.app")).unwrap();
            std::fs::create_dir(f.0.join("Tool 2.app")).unwrap();
            assert_eq!(
                trash_destination(&f.0, std::ffi::OsStr::new("Tool.app")).unwrap(),
                f.0.join("Tool 3.app")
            );
            symlink(f.0.join("missing"), f.0.join("Tool 3.app")).unwrap();
            assert_eq!(
                trash_destination(&f.0, std::ffi::OsStr::new("Tool.app")).unwrap(),
                f.0.join("Tool 4.app")
            );
        }
        #[test]
        fn osascript_failures_keep_cancel_and_automation_distinct() {
            assert!(
                osascript_error("move", "execution error: User canceled. (-128)")
                    .starts_with("mole_cancelled")
            );
            assert!(osascript_error(
                "move",
                "Not authorized to send Apple events to Finder. (-1743)"
            )
            .starts_with("mole_permission_required"));
            let other = osascript_error("move", "  Finder got an error (-10000)\n");
            assert_eq!(
                other,
                "mole_trash_failed: move: Finder got an error (-10000)"
            );
        }
        #[test]
        fn user_owned_bundles_do_not_need_the_admin_prompt_but_immutable_ones_are_refused() {
            let f = Fixture::new();
            let app = f.0.join("Fixture.app");
            std::fs::create_dir(&app).unwrap();
            assert!(!needs_admin(&app).unwrap());
            // A user cannot unlock a user-immutable bundle with an admin move.
            let name = std::ffi::CString::new(app.as_os_str().as_bytes()).unwrap();
            assert_eq!(
                unsafe { libc::chflags(name.as_ptr(), libc::UF_IMMUTABLE) },
                0
            );
            let refused = needs_admin(&app);
            assert_eq!(unsafe { libc::chflags(name.as_ptr(), 0) }, 0);
            assert_eq!(refused.unwrap_err(), "mole_permission_required");
            // A plain file is never an app bundle the admin path would move.
            let file = f.file("plain");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
            assert!(needs_admin(&file).is_err());
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
                    running: false,
                    needs_admin: false,
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
                    running: false,
                    needs_admin: false,
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
            modified: std::time::UNIX_EPOCH,
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
                running: false,
                needs_admin: false,
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
        fail_prepare: Option<&'static str>,
        prepared: Cell<usize>,
        statuses: Vec<RemovalStatus>,
        native_error: Option<usize>,
    }
    impl Fake {
        fn new(statuses: Vec<RemovalStatus>) -> Self {
            Self {
                calls: RefCell::new(vec![]),
                guards: Cell::new(0),
                fail_guard: None,
                fail_prepare: None,
                prepared: Cell::new(0),
                statuses,
                native_error: None,
            }
        }
    }
    impl Adapter for Fake {
        fn prepare(&self, _: &Preview) -> Result<(), String> {
            self.prepared.set(self.prepared.get() + 1);
            // Quitting must happen before any guard reads the running state.
            assert_eq!(self.guards.get(), 0, "prepare runs before every guard");
            match self.fail_prepare {
                Some(reason) => Err(reason.into()),
                None => Ok(()),
            }
        }
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
    fn the_app_is_quit_once_before_guards_and_a_failed_quit_writes_nothing() {
        let p = preview(Instant::now());
        let fake = Fake::new(vec![RemovalStatus::Moved; 2]);
        execute(&p, &["cache".into()], &fake).unwrap();
        assert_eq!(fake.prepared.get(), 1);
        assert_eq!(*fake.calls.borrow(), ["app", "cache"]);
        let mut fake = Fake::new(vec![]);
        fake.fail_prepare = Some("mole_app_changed");
        let out = execute(&p, &["cache".into()], &fake).unwrap();
        assert_eq!(out.stopped_reason.as_deref(), Some("mole_app_changed"));
        assert_eq!(out.items[0].error.as_deref(), Some("mole_app_changed"));
        assert!(out
            .items
            .iter()
            .all(|r| r.status == RemovalStatus::NotAttempted));
        assert_eq!(fake.guards.get(), 0);
        assert!(fake.calls.borrow().is_empty());
        // Invalid selections are rejected before anything is quit.
        let fake = Fake::new(vec![]);
        assert!(execute(&p, &["unknown".into()], &fake).is_err());
        assert_eq!(fake.prepared.get(), 0);
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
