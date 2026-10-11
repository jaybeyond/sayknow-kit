//! Device identity per vault and clone/restore detection (§4.2.1).
//!
//! The identity `{vault_id, device_id, install_fp, generation}` is kept twice:
//! in the keychain (`sync:device:<vault_id>`) and in SQLite (`vault_identity`).
//! The keychain is always written first, so after any crash the keychain is
//! equal to or one generation ahead of the database. Anything else means one
//! side was copied or rolled back, and [`detect`] answers with the triggers
//! that require a rotation to a new device id before the next upload.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::crypto::hex;
use super::envelope::is_id;
use super::secrets::{read_secret, write_secret, SecretError, SecretStore, SyncSecret};

/// `SHA-256("sayknow-install-v1" ‖ machine_id ‖ os_user_id)`. The raw machine
/// and user ids are never stored or uploaded; only this hash is.
pub(crate) fn install_fingerprint(machine_id: &str, os_user_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"sayknow-install-v1");
    hasher.update(machine_id.as_bytes());
    hasher.update(os_user_id.as_bytes());
    hex(&hasher.finalize())
}

/// This install's fingerprint, or `None` if the platform ids cannot be read
/// right now. Only a successful read is cached, so a transient failure is
/// retried next cycle. Tests never read this machine's ids; they cover the
/// hashing and parsing.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn current_install_fingerprint() -> Option<String> {
    static FINGERPRINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(cached) = FINGERPRINT.get() {
        return Some(cached.clone());
    }
    let (machine, user) = platform::machine_and_user()?;
    Some(FINGERPRINT.get_or_init(|| install_fingerprint(&machine, &user)).clone())
}

#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
mod platform {
    /// `IOPlatformUUID` and the numeric uid; both readable without privileges.
    pub(super) fn machine_and_user() -> Option<(String, String)> {
        let properties = crate::iokit_macos::service_properties("IOPlatformExpertDevice")?;
        let machine = crate::iokit_macos::string_value(&properties, "IOPlatformUUID")?;
        let machine = machine.trim();
        if machine.is_empty() {
            return None;
        }
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        Some((machine.to_owned(), uid.to_string()))
    }
}

#[cfg(target_os = "windows")]
#[cfg_attr(test, allow(dead_code))]
mod platform {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// Runs a system tool by its absolute path, so nothing earlier on the
    /// search path (such as the app directory) can stand in for it.
    fn run(tool: &str, args: &[&str]) -> Option<String> {
        let root = std::env::var_os("SystemRoot")?;
        let program = std::path::Path::new(&root).join("System32").join(tool);
        let output = Command::new(program)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// `MachineGuid` and the user's SID.
    pub(super) fn machine_and_user() -> Option<(String, String)> {
        let registry = run(
            "reg.exe",
            &["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"],
        )?;
        let whoami = run("whoami.exe", &["/user", "/fo", "csv", "/nh"])?;
        Some((super::parse_machine_guid(&registry)?, super::parse_user_sid(&whoami)?))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[cfg_attr(test, allow(dead_code))]
mod platform {
    pub(super) fn machine_and_user() -> Option<(String, String)> {
        None
    }
}

/// The `MachineGuid` value from `reg query … /v MachineGuid` output.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_machine_guid(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "MachineGuid" && fields.next()? == "REG_SZ")
            .then(|| fields.next())
            .flatten()
            .filter(|guid| guid.len() == 36 && guid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
            .map(str::to_owned)
    })
}

/// The SID from `whoami /user /fo csv /nh` output: `"domain\user","S-1-…"`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_user_sid(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|line| !line.is_empty())?;
    let sid = line.rsplit(',').next()?.trim().trim_matches('"');
    (sid.starts_with("S-1-") && sid.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b'S'))
        .then(|| sid.to_owned())
}

/// The keychain copy of this device's identity in one vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceIdentity {
    pub(crate) v: u32,
    pub(crate) vault_id: String,
    pub(crate) device_id: String,
    pub(crate) install_fp: Option<String>,
    pub(crate) generation: u64,
}

impl DeviceIdentity {
    /// A brand-new identity: fresh UUIDv4 device id, generation 0. Its stream
    /// starts at seq 1 on the chain rooted at [`genesis_chain_head`].
    pub(crate) fn new(vault_id: &str, install_fp: Option<String>) -> Option<Self> {
        is_id(vault_id).then(|| Self {
            v: 1,
            vault_id: vault_id.to_owned(),
            device_id: uuid::Uuid::new_v4().to_string(),
            install_fp,
            generation: 0,
        })
    }
}

/// `prev_seg_hash` of a device's first segment: `H("genesis" ‖ vault_id ‖ device_id)`.
pub(crate) fn genesis_chain_head(vault_id: &str, device_id: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"genesis");
    hasher.update(vault_id.as_bytes());
    hasher.update(device_id.as_bytes());
    hasher.finalize().into()
}

/// Reads the keychain identity. A damaged entry of any kind (not base64, not
/// JSON, wrong vault, unknown fields) reads as absent, which [`detect`] then
/// reports as T1 so the cycle rotates instead of failing forever. Only a
/// keychain that cannot be reached is an error.
pub(crate) fn load_identity(store: &impl SecretStore, vault_id: &str) -> Result<Option<DeviceIdentity>, SecretError> {
    let bytes = match read_secret(store, SyncSecret::Device { vault_id }) {
        Ok(Some(bytes)) => bytes,
        Ok(None) | Err(SecretError::Corrupt) => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(serde_json::from_slice::<DeviceIdentity>(&bytes)
        .ok()
        .filter(|identity| identity.v == 1 && identity.vault_id == vault_id && is_id(&identity.device_id)))
}

/// Writes the keychain identity. Call before committing the matching database
/// change, so a crash in between always leaves the keychain ahead.
pub(crate) fn save_identity(store: &impl SecretStore, identity: &DeviceIdentity) -> Result<(), SecretError> {
    let json = serde_json::to_vec(identity).map_err(|e| SecretError::Backend(e.to_string()))?;
    write_secret(store, SyncSecret::Device { vault_id: &identity.vault_id }, &json)
}

/// Step one of sealing a segment: advance the keychain generation. The caller
/// then commits the same generation to SQLite with the sealed segment.
pub(crate) fn advance_generation(store: &impl SecretStore, identity: &DeviceIdentity) -> Result<DeviceIdentity, SecretError> {
    let next = DeviceIdentity { generation: identity.generation + 1, ..identity.clone() };
    save_identity(store, &next)?;
    Ok(next)
}

/// Rotation step R4: a new identity is written to the keychain before any
/// database change. If the process dies before the SQLite transaction commits,
/// the next cycle sees T1 and rotates again; the abandoned id never published.
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
pub(crate) fn rotate(
    store: &impl SecretStore,
    vault_id: &str,
    install_fp: Option<String>,
) -> Result<DeviceIdentity, SecretError> {
    let identity = DeviceIdentity::new(vault_id, install_fp).ok_or(SecretError::InvalidName)?;
    save_identity(store, &identity)?;
    Ok(identity)
}

/// The SQLite copy (`vault_identity` + `meta.install_fp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalIdentity {
    pub(crate) device_id: String,
    pub(crate) install_fp: Option<String>,
    pub(crate) generation: u64,
    pub(crate) sealed_seq: u64,
    pub(crate) chain_head: [u8; 32],
}

/// The decrypted `devices/<self>.rec` high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteRecord {
    pub(crate) last_seq: u64,
    pub(crate) chain_head: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SeqRange {
    pub(crate) first: u64,
    pub(crate) last: u64,
}

/// One object listed under `log/<device_id>/`: its range from the name and a
/// digest of its bytes, so a byte-identical copy of our own upload (a Drive
/// resend that raced a slow listing) is not mistaken for a clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteSegment {
    pub(crate) range: SeqRange,
    pub(crate) digest: [u8; 32],
}

/// Everything [`detect`] looks at in one sync cycle.
#[derive(Debug, Default)]
pub(crate) struct Observation<'a> {
    pub(crate) keychain: Option<&'a DeviceIdentity>,
    pub(crate) local: Option<&'a LocalIdentity>,
    pub(crate) current_install_fp: Option<&'a str>,
    pub(crate) remote_record: Option<RemoteRecord>,
    /// Segment ranges this device has sealed under its current id.
    pub(crate) local_segments: &'a [SeqRange],
    /// Every object listed under `log/<device_id>/`, duplicates included.
    pub(crate) remote_segments: &'a [RemoteSegment],
    /// This cycle's own `put_create` hit an existing object with other content.
    pub(crate) put_create_conflict: bool,
    pub(crate) checkpoint_restored: bool,
    pub(crate) vault_switched: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Trigger {
    /// Keychain and SQLite name different device ids (or one is missing).
    T1DeviceMismatch,
    /// The install fingerprint changed: other hardware or OS user.
    T2InstallChanged,
    /// Keychain generation behind SQLite, or more than one ahead.
    T3PartialRollback,
    /// The remote high-water mark is past what this device sealed.
    T4FullRollback,
    /// Our own log holds a range we never sealed, or one range with two
    /// different contents.
    T5SameIdentityClone,
    /// Our create-only upload lost to another writer.
    T6NameTaken,
    T7CheckpointRestore,
    T8VaultSwitch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdentityCheck {
    /// No identity in this vault yet: create one, nothing to rotate.
    Fresh,
    /// Nothing points at a copy or rollback. `adopt_generation` is set when a
    /// crash left the keychain exactly one seal ahead; the database takes it.
    Healthy { adopt_generation: Option<u64> },
    /// Rotate before any further upload this cycle.
    Rotate(Vec<Trigger>),
}

pub(crate) fn detect(observation: &Observation<'_>) -> IdentityCheck {
    let (keychain, local) = match (observation.keychain, observation.local) {
        (None, None) => return IdentityCheck::Fresh,
        (Some(keychain), Some(local)) => (keychain, local),
        _ => {
            let mut triggers = vec![Trigger::T1DeviceMismatch];
            triggers.extend(event_triggers(observation));
            return IdentityCheck::Rotate(triggers);
        }
    };

    let mut triggers = Vec::new();
    if keychain.device_id != local.device_id {
        triggers.push(Trigger::T1DeviceMismatch);
    }
    if let Some(current) = observation.current_install_fp {
        // A stored copy without a fingerprint cannot vouch for this install.
        let stored = [keychain.install_fp.as_deref(), local.install_fp.as_deref()];
        if stored.iter().any(|fp| *fp != Some(current)) {
            triggers.push(Trigger::T2InstallChanged);
        }
    }
    let generation_gap = keychain.generation.checked_sub(local.generation);
    if !matches!(generation_gap, Some(0 | 1)) {
        triggers.push(Trigger::T3PartialRollback);
    }
    if let Some(remote) = observation.remote_record {
        if remote.last_seq > local.sealed_seq
            || (remote.last_seq == local.sealed_seq && remote.chain_head != local.chain_head)
        {
            triggers.push(Trigger::T4FullRollback);
        }
    }
    if own_log_was_written_elsewhere(observation, local.sealed_seq) {
        triggers.push(Trigger::T5SameIdentityClone);
    }
    triggers.extend(event_triggers(observation));

    if triggers.is_empty() {
        IdentityCheck::Healthy { adopt_generation: (generation_gap == Some(1)).then_some(keychain.generation) }
    } else {
        IdentityCheck::Rotate(triggers)
    }
}

fn own_log_was_written_elsewhere(observation: &Observation<'_>, sealed_seq: u64) -> bool {
    let mut digests: std::collections::BTreeMap<SeqRange, [u8; 32]> = std::collections::BTreeMap::new();
    observation.remote_segments.iter().any(|segment| {
        let range = segment.range;
        let conflicting = *digests.entry(range).or_insert(segment.digest) != segment.digest;
        conflicting || range.last > sealed_seq || !observation.local_segments.contains(&range)
    })
}

fn event_triggers(observation: &Observation<'_>) -> impl Iterator<Item = Trigger> {
    [
        (observation.put_create_conflict, Trigger::T6NameTaken),
        (observation.checkpoint_restored, Trigger::T7CheckpointRestore),
        (observation.vault_switched, Trigger::T8VaultSwitch),
    ]
    .into_iter()
    .filter_map(|(fired, trigger)| fired.then_some(trigger))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::secrets::MemoryStore;

    const VAULT: &str = "0b5e2a8c-3d6f-4f1a-9c2e-7a4b1d9e8f60";
    const FP_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FP_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn keychain(device: &str, generation: u64) -> DeviceIdentity {
        DeviceIdentity {
            v: 1,
            vault_id: VAULT.to_owned(),
            device_id: device.to_owned(),
            install_fp: Some(FP_A.to_owned()),
            generation,
        }
    }

    fn local(device: &str, generation: u64, sealed_seq: u64) -> LocalIdentity {
        LocalIdentity {
            device_id: device.to_owned(),
            install_fp: Some(FP_A.to_owned()),
            generation,
            sealed_seq,
            chain_head: [sealed_seq as u8; 32],
        }
    }

    fn observe<'a>(keychain: &'a DeviceIdentity, local: &'a LocalIdentity) -> Observation<'a> {
        Observation {
            keychain: Some(keychain),
            local: Some(local),
            current_install_fp: Some(FP_A),
            ..Observation::default()
        }
    }

    #[test]
    fn install_fingerprint_matches_an_independent_sha256() {
        // python3: hashlib.sha256(b"sayknow-install-v1" + b"00000000-0000-0000-0000-000000000001" + b"501").hexdigest()
        assert_eq!(
            install_fingerprint("00000000-0000-0000-0000-000000000001", "501"),
            "beb85cd941d30beb8129936d418b3294357c77337fcc09c724299e73cec6cc5c"
        );
        assert_ne!(install_fingerprint("m", "501"), install_fingerprint("m", "502"));
    }

    #[test]
    fn genesis_head_matches_an_independent_sha256() {
        // python3: hashlib.sha256(b"genesis" + b"vault-1" + b"device-1").hexdigest()
        assert_eq!(
            hex(&genesis_chain_head("vault-1", "device-1")),
            "33e00f26bb0cd9ae13c378a5dc56ce431b77f3bf0dc25f2e670c320b97678db4"
        );
    }

    #[test]
    fn windows_ids_are_parsed_strictly() {
        let reg = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\r\n    MachineGuid    REG_SZ    3f2504e0-4f89-11d3-9a0c-0305e82c3301\r\n\r\n";
        assert_eq!(parse_machine_guid(reg).as_deref(), Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301"));
        assert_eq!(parse_machine_guid("    MachineGuid    REG_SZ    not-a-guid"), None);
        assert_eq!(parse_machine_guid("    Other    REG_SZ    3f2504e0-4f89-11d3-9a0c-0305e82c3301"), None);
        assert_eq!(parse_machine_guid(""), None);

        let whoami = "\"desktop-1\\jay\",\"S-1-5-21-1004336348-1177238915-682003330-1001\"\r\n";
        assert_eq!(parse_user_sid(whoami).as_deref(), Some("S-1-5-21-1004336348-1177238915-682003330-1001"));
        assert_eq!(parse_user_sid("\"desktop-1\\jay\",\"jay\""), None);
        assert_eq!(parse_user_sid(""), None);
    }

    #[test]
    fn new_identities_are_unique_and_only_for_valid_vaults() {
        let a = DeviceIdentity::new(VAULT, None).unwrap();
        let b = DeviceIdentity::new(VAULT, None).unwrap();
        assert_ne!(a.device_id, b.device_id);
        assert!(is_id(&a.device_id));
        assert_eq!(a.generation, 0);
        assert_eq!(DeviceIdentity::new("../x", None), None);
    }

    #[test]
    fn keychain_identity_round_trips_and_damage_reads_as_absent() {
        let store = MemoryStore::default();
        let identity = keychain("dev-a", 3);
        save_identity(&store, &identity).unwrap();
        assert_eq!(load_identity(&store, VAULT).unwrap(), Some(identity.clone()));

        let account = format!("sync:device:{VAULT}");
        let other_vault = DeviceIdentity { vault_id: "other".to_owned(), ..identity.clone() };
        let unknown_field = format!(
            r#"{{"v":1,"vault_id":"{VAULT}","device_id":"dev-a","install_fp":null,"generation":0,"extra":1}}"#
        );
        for damaged in [
            "not base64!".to_owned(),
            data_encoding::BASE64.encode(b"{not json"),
            data_encoding::BASE64.encode(&serde_json::to_vec(&other_vault).unwrap()),
            data_encoding::BASE64.encode(unknown_field.as_bytes()),
        ] {
            store.put_raw(&account, &damaged);
            assert_eq!(load_identity(&store, VAULT).unwrap(), None, "{damaged:?}");
            let db = local("dev-a", 3, 0);
            let observation = Observation { keychain: None, local: Some(&db), ..Observation::default() };
            assert_eq!(detect(&observation), IdentityCheck::Rotate(vec![Trigger::T1DeviceMismatch]));
        }
    }

    #[test]
    fn an_unreachable_keychain_is_an_error_not_a_missing_identity() {
        let store = MemoryStore::default();
        let identity = keychain("dev-a", 3);
        save_identity(&store, &identity).unwrap();
        let account = format!("sync:device:{VAULT}");
        let before = store.raw(&account);
        store.fail_next_gets(1);
        assert!(matches!(load_identity(&store, VAULT), Err(SecretError::Backend(_))));
        assert_eq!(store.raw(&account), before, "the stored identity is untouched");
        assert_eq!(load_identity(&store, VAULT).unwrap(), Some(identity));
    }

    #[test]
    fn nothing_stored_is_a_fresh_vault_not_a_rotation() {
        assert_eq!(detect(&Observation::default()), IdentityCheck::Fresh);
    }

    #[test]
    fn matching_copies_are_healthy() {
        let (k, l) = (keychain("dev-a", 4), local("dev-a", 4, 9));
        assert_eq!(detect(&observe(&k, &l)), IdentityCheck::Healthy { adopt_generation: None });
    }

    #[test]
    fn a_crash_between_keychain_and_database_seal_is_healed_not_rotated() {
        let (k, l) = (keychain("dev-a", 5), local("dev-a", 4, 9));
        assert_eq!(detect(&observe(&k, &l)), IdentityCheck::Healthy { adopt_generation: Some(5) });
    }

    #[test]
    fn each_trigger_fires_on_its_own_condition() {
        let k = keychain("dev-a", 4);
        let l = local("dev-a", 4, 9);
        let rotate = |trigger| IdentityCheck::Rotate(vec![trigger]);

        let other = local("dev-b", 4, 9);
        assert_eq!(detect(&observe(&k, &other)), rotate(Trigger::T1DeviceMismatch));
        assert_eq!(
            detect(&Observation { keychain: None, ..observe(&k, &l) }),
            rotate(Trigger::T1DeviceMismatch)
        );
        assert_eq!(
            detect(&Observation { local: None, ..observe(&k, &l) }),
            rotate(Trigger::T1DeviceMismatch)
        );

        assert_eq!(
            detect(&Observation { current_install_fp: Some(FP_B), ..observe(&k, &l) }),
            rotate(Trigger::T2InstallChanged)
        );
        let migrated_db = LocalIdentity { install_fp: Some(FP_B.to_owned()), ..l.clone() };
        assert_eq!(detect(&observe(&k, &migrated_db)), rotate(Trigger::T2InstallChanged));

        for (kg, lg) in [(3, 4), (6, 4), (0, 9)] {
            let (k2, l2) = (keychain("dev-a", kg), local("dev-a", lg, 9));
            assert_eq!(detect(&observe(&k2, &l2)), rotate(Trigger::T3PartialRollback), "{kg} vs {lg}");
        }

        let ahead = RemoteRecord { last_seq: 10, chain_head: [9; 32] };
        assert_eq!(
            detect(&Observation { remote_record: Some(ahead), ..observe(&k, &l) }),
            rotate(Trigger::T4FullRollback)
        );
        let diverged = RemoteRecord { last_seq: 9, chain_head: [0xEE; 32] };
        assert_eq!(
            detect(&Observation { remote_record: Some(diverged), ..observe(&k, &l) }),
            rotate(Trigger::T4FullRollback)
        );
        let behind = RemoteRecord { last_seq: 7, chain_head: [0xEE; 32] };
        assert_eq!(
            detect(&Observation { remote_record: Some(behind), ..observe(&k, &l) }),
            IdentityCheck::Healthy { adopt_generation: None }
        );

        let sealed = [SeqRange { first: 1, last: 5 }, SeqRange { first: 6, last: 9 }];
        let seg = |first, last, digest: u8| RemoteSegment { range: SeqRange { first, last }, digest: [digest; 32] };
        for remote in [
            vec![seg(1, 5, 1), seg(6, 10, 2)],
            vec![seg(1, 5, 1), seg(1, 5, 9)],
            vec![seg(1, 4, 1)],
        ] {
            let observation = Observation { local_segments: &sealed, remote_segments: &remote, ..observe(&k, &l) };
            assert_eq!(detect(&observation), rotate(Trigger::T5SameIdentityClone), "{remote:?}");
        }
        for remote in [vec![seg(6, 9, 2)], vec![seg(1, 5, 1), seg(1, 5, 1), seg(6, 9, 2)]] {
            let observation = Observation { local_segments: &sealed, remote_segments: &remote, ..observe(&k, &l) };
            assert_eq!(detect(&observation), IdentityCheck::Healthy { adopt_generation: None }, "{remote:?}");
        }

        assert_eq!(
            detect(&Observation { put_create_conflict: true, ..observe(&k, &l) }),
            rotate(Trigger::T6NameTaken)
        );
        assert_eq!(
            detect(&Observation { checkpoint_restored: true, ..observe(&k, &l) }),
            rotate(Trigger::T7CheckpointRestore)
        );
        assert_eq!(
            detect(&Observation { vault_switched: true, ..observe(&k, &l) }),
            rotate(Trigger::T8VaultSwitch)
        );
    }

    #[test]
    fn a_copy_without_a_fingerprint_cannot_vouch_for_this_install() {
        // The identity was created while the ids were unreadable; once they can
        // be read, a copy that never recorded them could be from anywhere.
        let k = DeviceIdentity { install_fp: None, ..keychain("dev-a", 4) };
        let l = local("dev-a", 4, 9);
        assert_eq!(detect(&observe(&k, &l)), IdentityCheck::Rotate(vec![Trigger::T2InstallChanged]));
        let both_unset = LocalIdentity { install_fp: None, ..l.clone() };
        assert_eq!(detect(&observe(&k, &both_unset)), IdentityCheck::Rotate(vec![Trigger::T2InstallChanged]));
        assert_eq!(
            detect(&Observation { current_install_fp: None, ..observe(&k, &both_unset) }),
            IdentityCheck::Healthy { adopt_generation: None }
        );
    }

    #[test]
    fn an_unreadable_fingerprint_disables_only_t2() {
        let k = keychain("dev-a", 4);
        let l = local("dev-a", 4, 9);
        assert_eq!(
            detect(&Observation { current_install_fp: None, ..observe(&k, &l) }),
            IdentityCheck::Healthy { adopt_generation: None }
        );
        let other = local("dev-b", 4, 9);
        assert_eq!(
            detect(&Observation { current_install_fp: None, ..observe(&k, &other) }),
            IdentityCheck::Rotate(vec![Trigger::T1DeviceMismatch])
        );
    }

    #[test]
    fn generation_advances_in_the_keychain_first() {
        let store = MemoryStore::default();
        let identity = keychain("dev-a", 4);
        save_identity(&store, &identity).unwrap();
        let next = advance_generation(&store, &identity).unwrap();
        assert_eq!(next.generation, 5);
        assert_eq!(load_identity(&store, VAULT).unwrap(), Some(next));
    }

    /// AC 17: a crash anywhere around R4 converges by rotating again, and the
    /// abandoned id is never reused.
    #[test]
    fn rotation_interrupted_at_each_step_converges_to_a_fresh_id() {
        let store = MemoryStore::default();
        let original = keychain("dev-a", 4);
        save_identity(&store, &original).unwrap();
        let db = local("dev-a", 4, 9);
        let mut used = vec![original.device_id.clone()];

        // Crash before the keychain write: nothing changed, the cause still holds.
        store.fail_next_sets(1);
        assert!(rotate(&store, VAULT, Some(FP_B.to_owned())).is_err());
        assert_eq!(load_identity(&store, VAULT).unwrap(), Some(original.clone()));
        let stored = load_identity(&store, VAULT).unwrap().unwrap();
        let observation = Observation { current_install_fp: Some(FP_B), ..observe(&stored, &db) };
        assert!(matches!(detect(&observation), IdentityCheck::Rotate(_)));

        // Crash after the keychain write, before the database commit: T1.
        let abandoned = rotate(&store, VAULT, Some(FP_B.to_owned())).unwrap();
        used.push(abandoned.device_id.clone());
        let stored = load_identity(&store, VAULT).unwrap().unwrap();
        let observation = Observation { current_install_fp: Some(FP_B), ..observe(&stored, &db) };
        assert_eq!(
            detect(&observation),
            IdentityCheck::Rotate(vec![Trigger::T1DeviceMismatch, Trigger::T2InstallChanged, Trigger::T3PartialRollback])
        );

        // Rotate again and commit: both copies agree on an id never used before.
        let settled = rotate(&store, VAULT, Some(FP_B.to_owned())).unwrap();
        assert!(!used.contains(&settled.device_id));
        let committed = LocalIdentity {
            device_id: settled.device_id.clone(),
            install_fp: settled.install_fp.clone(),
            generation: settled.generation,
            sealed_seq: 0,
            chain_head: genesis_chain_head(VAULT, &settled.device_id),
        };
        let stored = load_identity(&store, VAULT).unwrap().unwrap();
        let observation = Observation { current_install_fp: Some(FP_B), ..observe(&stored, &committed) };
        assert_eq!(detect(&observation), IdentityCheck::Healthy { adopt_generation: None });
    }
}
