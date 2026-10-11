//! Recovery key (RK) handling and the vault manifest (plan §4.6.2–§4.6.3).
//!
//! The RK is 256 random bits shown once as Base32 with a checksum. It wraps
//! the vault DEK through `KEK = HKDF(RK, vault_id, "sayknow/v1/wrap")`; the
//! wrapped DEK is stored in the manifest so a new device can join with the RK
//! alone. Losing every device and the RK makes the cloud copy unrecoverable,
//! by design.
//!
//! Unlocking is a pure function: it never touches the keychain or the remote,
//! so a wrong or mistyped RK cannot change any state.

use std::fmt;

use data_encoding::{BASE32_NOPAD, BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::crypto::{derive_kek, derive_subkey, hmac_sha256, hmac_verify, CryptoError, Purpose, SecretKey, KEY_LEN};
use super::envelope::{is_id, open, seal, ObjectAddress};

pub(crate) const MANIFEST_FORMAT: u32 = 1;
pub(crate) const SUITE: &str = "xchacha20poly1305-hkdf-sha256";
pub(crate) const RK_KIND_RANDOM: &str = "random256";
pub(crate) const INITIAL_KEY_EPOCH: u32 = 1;

const RK_CHECKSUM_LEN: usize = 3;
const RK_CHECKSUM_DOMAIN: &[u8] = b"sayknow-rk-v1";
const KEY_CHECK_MESSAGE: &[u8] = b"sayknow-key-check";
/// 32 key bytes + 3 checksum bytes = 280 bits = exactly 56 Base32 characters.
const RK_TEXT_LEN: usize = 56;
const RK_GROUP: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryError {
    /// The text is not a recovery key (wrong length or characters).
    Format,
    /// Well-formed but the checksum fails: almost certainly a typo.
    Checksum,
    /// A valid recovery key, but not the one for this vault.
    WrongKey,
    /// The manifest failed authentication after unlocking.
    Tampered,
    /// The manifest was written by a newer or different format.
    Unsupported,
    /// The manifest belongs to a different cloud account binding.
    BindingMismatch,
    /// The manifest names a different vault than the location it was read from.
    VaultMismatch,
    /// A new vault was requested with an id or timestamp outside the format.
    InvalidInput,
    Crypto(CryptoError),
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format => f.write_str("not a recovery key"),
            Self::Checksum => f.write_str("recovery key has a typo"),
            Self::WrongKey => f.write_str("recovery key does not open this vault"),
            Self::Tampered => f.write_str("vault manifest failed verification"),
            Self::Unsupported => f.write_str("vault manifest format is not supported"),
            Self::BindingMismatch => f.write_str("vault belongs to a different account"),
            Self::VaultMismatch => f.write_str("vault manifest is stored under the wrong vault"),
            Self::InvalidInput => f.write_str("vault id or creation time is malformed"),
            Self::Crypto(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for RecoveryError {}

impl From<CryptoError> for RecoveryError {
    fn from(err: CryptoError) -> Self {
        Self::Crypto(err)
    }
}

/// The user's recovery key. Displayed once, never stored by the app.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct RecoveryKey(SecretKey);

impl RecoveryKey {
    pub(crate) fn generate() -> Result<Self, CryptoError> {
        SecretKey::generate().map(Self)
    }

    fn checksum(key: &SecretKey) -> [u8; RK_CHECKSUM_LEN] {
        let digest = Sha256::new()
            .chain_update(RK_CHECKSUM_DOMAIN)
            .chain_update(key.as_bytes())
            .finalize();
        let mut out = [0u8; RK_CHECKSUM_LEN];
        out.copy_from_slice(&digest[..RK_CHECKSUM_LEN]);
        out
    }

    /// `XXXX-XXXX-…`: 14 groups of four Base32 characters.
    pub(crate) fn to_display(&self) -> Zeroizing<String> {
        let mut raw = Zeroizing::new([0u8; KEY_LEN + RK_CHECKSUM_LEN]);
        raw[..KEY_LEN].copy_from_slice(self.0.as_bytes());
        raw[KEY_LEN..].copy_from_slice(&Self::checksum(&self.0));
        let encoded = Zeroizing::new(BASE32_NOPAD.encode(raw.as_slice()));
        let mut out = Zeroizing::new(String::with_capacity(RK_TEXT_LEN + RK_TEXT_LEN / RK_GROUP));
        for (index, ch) in encoded.chars().enumerate() {
            if index > 0 && index % RK_GROUP == 0 {
                out.push('-');
            }
            out.push(ch);
        }
        out
    }

    /// Accepts the display form with any case, spacing, or dashes, and the
    /// digits most often typed for look-alike letters (0→O, 1→I, 8→B).
    pub(crate) fn parse(text: &str) -> Result<Self, RecoveryError> {
        // Sized up front so the buffer never reallocates and leaves an unwiped
        // copy behind; normalization never makes the text longer.
        let mut normalized = Zeroizing::new(String::with_capacity(text.len()));
        for c in text.chars().filter(|c| !c.is_whitespace() && *c != '-') {
            normalized.push(match c.to_ascii_uppercase() {
                '0' => 'O',
                '1' => 'I',
                '8' => 'B',
                other => other,
            });
        }
        if normalized.len() != RK_TEXT_LEN {
            return Err(RecoveryError::Format);
        }
        let raw = Zeroizing::new(
            BASE32_NOPAD
                .decode(normalized.as_bytes())
                .map_err(|_| RecoveryError::Format)?,
        );
        if raw.len() != KEY_LEN + RK_CHECKSUM_LEN {
            return Err(RecoveryError::Format);
        }
        let key = SecretKey::from_slice(&raw[..KEY_LEN]).ok_or(RecoveryError::Format)?;
        if Self::checksum(&key) != raw[KEY_LEN..] {
            return Err(RecoveryError::Checksum);
        }
        Ok(Self(key))
    }
}

/// U1: `text` parses as a recovery key, checksum included, after the same
/// normalization as [`RecoveryKey::parse`]. Text whose significant character
/// count is not exactly the RK length is rejected without being copied, so a
/// large clipboard entry costs one scan. The parsed key is dropped (and
/// zeroized) at once.
pub(crate) fn is_recovery_key(text: &str) -> bool {
    let significant = text.chars().filter(|c| !c.is_whitespace() && *c != '-').take(RK_TEXT_LEN + 1).count();
    significant == RK_TEXT_LEN && RecoveryKey::parse(text).is_ok()
}

/// The plaintext vault descriptor stored at `v1/<vault_id>/manifest.json`.
/// Binary fields are Base64. Every field except `manifest_mac` is covered by
/// `manifest_mac = HMAC(K_man, canonical(body))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) format: u32,
    pub(crate) vault_id: String,
    pub(crate) created_at: String,
    pub(crate) suite: String,
    pub(crate) key_epoch: u32,
    pub(crate) rk_kind: String,
    pub(crate) wrapped_dek: String,
    pub(crate) key_check: String,
    pub(crate) binding_hint: String,
    pub(crate) manifest_mac: String,
}

/// The authenticated part of the manifest, serialized in a fixed field order.
#[derive(Serialize)]
struct ManifestBody<'a> {
    format: u32,
    vault_id: &'a str,
    created_at: &'a str,
    suite: &'a str,
    key_epoch: u32,
    rk_kind: &'a str,
    wrapped_dek: &'a str,
    key_check: &'a str,
    binding_hint: &'a str,
}

impl Manifest {
    fn canonical_body(&self) -> Vec<u8> {
        serde_json::to_vec(&ManifestBody {
            format: self.format,
            vault_id: &self.vault_id,
            created_at: &self.created_at,
            suite: &self.suite,
            key_epoch: self.key_epoch,
            rk_kind: &self.rk_kind,
            wrapped_dek: &self.wrapped_dek,
            key_check: &self.key_check,
            binding_hint: &self.binding_hint,
        })
        .expect("manifest body serializes")
    }

    /// v1 reads exactly one format, suite, RK kind, and key epoch. A manifest
    /// from a later key-epoch rotation is held until the app is updated.
    fn check_supported(&self) -> Result<(), RecoveryError> {
        if self.format != MANIFEST_FORMAT
            || self.suite != SUITE
            || self.rk_kind != RK_KIND_RANDOM
            || self.key_epoch != INITIAL_KEY_EPOCH
        {
            return Err(RecoveryError::Unsupported);
        }
        Ok(())
    }

    /// The vault id inside the manifest must be the one its location names
    /// (the path segment, or Drive's `appProperties.vault_id`). Otherwise a
    /// valid manifest from another vault could be planted under this one.
    fn check_location(&self, expected_vault_id: &str) -> Result<(), RecoveryError> {
        if self.vault_id != expected_vault_id || !is_id(&self.vault_id) {
            return Err(RecoveryError::VaultMismatch);
        }
        Ok(())
    }
}

/// A freshly created vault. The RK must be shown to the user and re-entered
/// before anything is uploaded; the DEK goes to the keychain.
pub(crate) struct NewVault {
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) dek: SecretKey,
    pub(crate) recovery_key: RecoveryKey,
    pub(crate) manifest: Manifest,
}

fn key_check(dek: &SecretKey, vault_id: &str, key_epoch: u32) -> [u8; 32] {
    hmac_sha256(&derive_subkey(dek, vault_id, key_epoch, Purpose::KeyCheck), KEY_CHECK_MESSAGE)
}

fn binding_hint(manifest_key: &SecretKey, binding_fp: &str) -> [u8; 32] {
    hmac_sha256(manifest_key, binding_fp.as_bytes())
}

fn decode(field: &str) -> Result<Vec<u8>, RecoveryError> {
    BASE64.decode(field.as_bytes()).map_err(|_| RecoveryError::Tampered)
}

/// `created_at` is the vault's creation time as `YYYY-MM-DDTHH:MM:SSZ`; keeping
/// it to this ASCII shape keeps the canonical manifest body escape-free.
pub(crate) fn create_vault(vault_id: &str, created_at: &str, binding_fp: &str) -> Result<NewVault, RecoveryError> {
    if !is_id(vault_id) || !is_utc_timestamp(created_at) {
        return Err(RecoveryError::InvalidInput);
    }
    let dek = SecretKey::generate()?;
    let recovery_key = RecoveryKey::generate()?;
    let manifest = build_manifest(&dek, &recovery_key, vault_id, created_at, binding_fp)?;
    Ok(NewVault { dek, recovery_key, manifest })
}

fn is_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
}

fn build_manifest(
    dek: &SecretKey,
    recovery_key: &RecoveryKey,
    vault_id: &str,
    created_at: &str,
    binding_fp: &str,
) -> Result<Manifest, RecoveryError> {
    let key_epoch = INITIAL_KEY_EPOCH;
    let kek = derive_kek(&recovery_key.0, vault_id);
    let wrapped = seal(&kek, &ObjectAddress::wrapped_dek(vault_id, key_epoch), dek.as_bytes())?;
    let manifest_key = derive_subkey(dek, vault_id, key_epoch, Purpose::Manifest);
    let mut manifest = Manifest {
        format: MANIFEST_FORMAT,
        vault_id: vault_id.to_owned(),
        created_at: created_at.to_owned(),
        suite: SUITE.to_owned(),
        key_epoch,
        rk_kind: RK_KIND_RANDOM.to_owned(),
        wrapped_dek: BASE64.encode(&wrapped),
        key_check: BASE64.encode(&key_check(dek, vault_id, key_epoch)),
        binding_hint: BASE64.encode(&binding_hint(&manifest_key, binding_fp)),
        manifest_mac: String::new(),
    };
    sign_manifest(&mut manifest, &manifest_key);
    Ok(manifest)
}

fn sign_manifest(manifest: &mut Manifest, manifest_key: &SecretKey) {
    manifest.manifest_mac = BASE64.encode(&hmac_sha256(manifest_key, &manifest.canonical_body()));
}

/// Checks key_check, the MAC, and the account binding under `dek`.
/// `wrong_key` is what a key_check failure means to the caller: a different
/// DEK for devices verifying with their keychain copy, tampering for a joining
/// device whose DEK just authenticated out of the manifest itself.
fn verify_with_dek(
    manifest: &Manifest,
    dek: &SecretKey,
    binding_fp: &str,
    wrong_key: RecoveryError,
) -> Result<(), RecoveryError> {
    let check = decode(&manifest.key_check)?;
    let check_key = derive_subkey(dek, &manifest.vault_id, manifest.key_epoch, Purpose::KeyCheck);
    if !hmac_verify(&check_key, KEY_CHECK_MESSAGE, &check) {
        return Err(wrong_key);
    }
    let manifest_key = derive_subkey(dek, &manifest.vault_id, manifest.key_epoch, Purpose::Manifest);
    if !hmac_verify(&manifest_key, &manifest.canonical_body(), &decode(&manifest.manifest_mac)?) {
        return Err(RecoveryError::Tampered);
    }
    if !hmac_verify(&manifest_key, binding_fp.as_bytes(), &decode(&manifest.binding_hint)?) {
        return Err(RecoveryError::BindingMismatch);
    }
    Ok(())
}

/// Verifies a manifest read from `expected_vault_id`'s location with an
/// already-known DEK (devices that joined earlier re-check it on every read).
pub(crate) fn verify_manifest(
    manifest: &Manifest,
    expected_vault_id: &str,
    dek: &SecretKey,
    binding_fp: &str,
) -> Result<(), RecoveryError> {
    manifest.check_supported()?;
    manifest.check_location(expected_vault_id)?;
    verify_with_dek(manifest, dek, binding_fp, RecoveryError::WrongKey)
}

/// Opens a vault on a joining device: RK text → KEK → DEK, then full manifest
/// verification. Returns the DEK only when every check passes.
pub(crate) fn unlock(
    manifest: &Manifest,
    expected_vault_id: &str,
    recovery_text: &str,
    binding_fp: &str,
) -> Result<SecretKey, RecoveryError> {
    manifest.check_supported()?;
    manifest.check_location(expected_vault_id)?;
    let recovery_key = RecoveryKey::parse(recovery_text)?;
    let kek = derive_kek(&recovery_key.0, &manifest.vault_id);
    let wrapped = decode(&manifest.wrapped_dek)?;
    let address = ObjectAddress::wrapped_dek(&manifest.vault_id, manifest.key_epoch);
    let plaintext = open(&kek, &address, &wrapped).map_err(|err| match err {
        CryptoError::Decrypt => RecoveryError::WrongKey,
        other => RecoveryError::Crypto(other),
    })?;
    let dek = SecretKey::from_slice(&plaintext).ok_or(RecoveryError::Tampered)?;
    verify_with_dek(manifest, &dek, binding_fp, RecoveryError::Tampered)?;
    Ok(dek)
}

/// RK reissue (§4.6.3, AC 30): the same DEK under a new RK and KEK, in a new
/// manifest for the same vault and creation time. `current` must verify with
/// `dek` first, so a reissue never re-signs a manifest it does not trust. The
/// DEK is unchanged: devices that already joined keep working, and anyone who
/// kept the old RK *and* a copy of the old manifest can still decrypt; only a
/// new join with the old RK against the new manifest fails.
pub(crate) fn rewrap(current: &Manifest, dek: &SecretKey, binding_fp: &str) -> Result<NewVault, RecoveryError> {
    verify_manifest(current, &current.vault_id, dek, binding_fp)?;
    let recovery_key = RecoveryKey::generate()?;
    let manifest = build_manifest(dek, &recovery_key, &current.vault_id, &current.created_at, binding_fp)?;
    Ok(NewVault { dek: dek.clone(), recovery_key, manifest })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::hex;

    const VAULT: &str = "vault-test-0001";
    const BINDING: &str = "gdrive:sub-123";
    const CREATED: &str = "2026-10-10T00:00:00Z";

    // Recovery key bytes 00..1f. Its display form and every manifest value
    // below were computed with an independent Python stdlib implementation
    // (hashlib/hmac/base64, HKDF checked against RFC 5869 A.1).
    const FIXED_RK_TEXT: &str = "AAAQ-EAYE-AUDA-OCAJ-BIFQ-YDIO-B4IB-CEQT-CQKR-MFYY-DENB-WHA5-DYPW-E5JN";

    fn fixed_rk() -> RecoveryKey {
        RecoveryKey(SecretKey::from_bytes(core::array::from_fn(|i| i as u8)))
    }

    fn fixed_dek() -> SecretKey {
        SecretKey::from_bytes([0x40; 32])
    }

    fn fixed_vault() -> Manifest {
        build_manifest(&fixed_dek(), &fixed_rk(), VAULT, CREATED, BINDING).unwrap()
    }

    #[test]
    fn fixed_recovery_key_has_a_pinned_display_form() {
        let text = fixed_rk().to_display();
        assert_eq!(text.as_str(), FIXED_RK_TEXT);
        assert_eq!(text.split('-').count(), 14);
        assert!(text.split('-').all(|group| group.len() == RK_GROUP));
        assert_eq!(RecoveryKey::parse(FIXED_RK_TEXT).unwrap(), fixed_rk());
    }

    #[test]
    fn parse_tolerates_case_spacing_and_look_alike_digits() {
        let rk = fixed_rk();
        let sloppy = FIXED_RK_TEXT.to_lowercase().replace('-', " ");
        assert_eq!(RecoveryKey::parse(&sloppy).unwrap(), rk);
        let swapped: String = FIXED_RK_TEXT
            .chars()
            .map(|c| match c {
                'O' => '0',
                'I' => '1',
                'B' => '8',
                other => other,
            })
            .collect();
        assert_ne!(swapped, FIXED_RK_TEXT);
        assert_eq!(RecoveryKey::parse(&swapped).unwrap(), rk);
    }

    #[test]
    fn generated_recovery_keys_round_trip() {
        let rk = RecoveryKey::generate().unwrap();
        assert_eq!(RecoveryKey::parse(&rk.to_display()).unwrap(), rk);
    }

    #[test]
    fn every_single_character_typo_is_rejected() {
        let text = FIXED_RK_TEXT.replace('-', "");
        for index in 0..text.len() {
            let mut chars: Vec<char> = text.chars().collect();
            chars[index] = if chars[index] == 'A' { 'C' } else { 'A' };
            let typo: String = chars.into_iter().collect();
            assert_eq!(RecoveryKey::parse(&typo), Err(RecoveryError::Checksum), "typo at {index}");
        }
    }

    #[test]
    fn malformed_text_is_rejected() {
        for text in ["", "ABCD", &"A".repeat(55), &"A".repeat(57), &"!".repeat(56)] {
            assert_eq!(RecoveryKey::parse(text), Err(RecoveryError::Format), "{text}");
        }
    }

    #[test]
    fn is_recovery_key_matches_parse_and_rejects_everything_else() {
        let generated = RecoveryKey::generate().unwrap().to_display();
        let spaced = FIXED_RK_TEXT.to_lowercase().replace('-', " ");
        let wrapped = format!("\n  {}\t\n", FIXED_RK_TEXT.replace('-', "\n"));
        for text in [FIXED_RK_TEXT, generated.as_str(), &FIXED_RK_TEXT.replace('-', ""), &spaced, &wrapped] {
            assert!(is_recovery_key(text), "{text:?}");
        }
        let mut typo: Vec<char> = FIXED_RK_TEXT.chars().collect();
        typo[0] = 'B';
        let typo: String = typo.into_iter().collect();
        assert_eq!(RecoveryKey::parse(&typo), Err(RecoveryError::Checksum));
        let surrounded = format!("my key is {FIXED_RK_TEXT}");
        let long = "A".repeat(1 << 20);
        for text in ["", "AAAA-AAAA", &typo, &surrounded, &long, &"A".repeat(56), &"!".repeat(56)] {
            assert!(!is_recovery_key(text), "{:?}", &text[..text.len().min(80)]);
        }
    }

    #[test]
    fn manifest_canonical_body_and_mac_are_pinned() {
        let dek = fixed_dek();
        let mut manifest = fixed_vault();
        manifest.wrapped_dek = "AQ==".into();
        sign_manifest(&mut manifest, &derive_subkey(&dek, VAULT, 1, Purpose::Manifest));
        assert_eq!(
            String::from_utf8(manifest.canonical_body()).unwrap(),
            concat!(
                r#"{"format":1,"vault_id":"vault-test-0001","created_at":"2026-10-10T00:00:00Z","#,
                r#""suite":"xchacha20poly1305-hkdf-sha256","key_epoch":1,"rk_kind":"random256","#,
                r#""wrapped_dek":"AQ==","key_check":"FVvboUStpIzYsfH1MrF9E4RnyiQSCI5y/hZ/5uwTptk=","#,
                r#""binding_hint":"sR0CX9fvnkXCsSNiZWZkqbwVaONieNmTbOIFVxFXEBA="}"#,
            )
        );
        assert_eq!(manifest.manifest_mac, "MndjbJnbIo+EI6hotoftum3Hv7MhK/64UD7mKLZ+7gs=");
        assert_eq!(verify_manifest(&manifest, VAULT, &dek, BINDING), Ok(()));
    }

    #[test]
    fn create_then_unlock_returns_the_same_dek() {
        let vault = create_vault(VAULT, CREATED, BINDING).unwrap();
        let text = vault.recovery_key.to_display();
        let dek = unlock(&vault.manifest, VAULT, &text, BINDING).unwrap();
        assert_eq!(dek, vault.dek);
        assert_eq!(verify_manifest(&vault.manifest, VAULT, &vault.dek, BINDING), Ok(()));
    }

    #[test]
    fn create_vault_refuses_ids_and_times_it_could_never_reopen() {
        for vault_id in ["", "vault/a", "vault é", ".hidden", &"v".repeat(65)] {
            assert!(
                matches!(create_vault(vault_id, CREATED, BINDING), Err(RecoveryError::InvalidInput)),
                "{vault_id:?}"
            );
        }
        for created_at in ["", "2026-10-10", "2026-10-10T00:00:00+09:00", "2026-10-10T00:00:00.5Z", "２026-10-10T00:00:00Z"] {
            assert!(
                matches!(create_vault(VAULT, created_at, BINDING), Err(RecoveryError::InvalidInput)),
                "{created_at:?}"
            );
        }
    }

    #[test]
    fn wrapped_dek_uses_the_fixed_kek_vector() {
        let manifest = fixed_vault();
        let kek = derive_kek(&fixed_rk().0, VAULT);
        assert_eq!(
            hex(kek.as_bytes()),
            "1b8ea929b270b39219ecde58ddd4338ee448c305b24704a555244865ddcba5b6"
        );
        let wrapped = BASE64.decode(manifest.wrapped_dek.as_bytes()).unwrap();
        let opened = open(&kek, &ObjectAddress::wrapped_dek(VAULT, 1), &wrapped).unwrap();
        assert_eq!(opened.as_slice(), fixed_dek().as_bytes());
        assert_eq!(unlock(&manifest, VAULT, FIXED_RK_TEXT, BINDING).unwrap(), fixed_dek());
    }

    #[test]
    fn wrong_recovery_key_fails_without_side_effects() {
        let manifest = fixed_vault();
        let before = manifest.clone();
        let other = RecoveryKey(SecretKey::from_bytes([0x5A; 32])).to_display();
        assert_eq!(unlock(&manifest, VAULT, &other, BINDING), Err(RecoveryError::WrongKey));
        assert_eq!(unlock(&manifest, VAULT, "not a key", BINDING), Err(RecoveryError::Format));
        assert_eq!(manifest, before);
    }

    #[test]
    fn manifest_must_name_the_vault_it_was_read_from() {
        let manifest = fixed_vault();
        assert_eq!(
            unlock(&manifest, "vault-test-0002", FIXED_RK_TEXT, BINDING),
            Err(RecoveryError::VaultMismatch)
        );
        assert_eq!(
            verify_manifest(&manifest, "vault-test-0002", &fixed_dek(), BINDING),
            Err(RecoveryError::VaultMismatch)
        );
        let mut moved = manifest;
        moved.vault_id = "vault-test-0002".into();
        // Relabelled to match its new location, the RK no longer unwraps it.
        assert_eq!(
            unlock(&moved, "vault-test-0002", FIXED_RK_TEXT, BINDING),
            Err(RecoveryError::WrongKey)
        );
    }

    #[test]
    fn every_manifest_field_is_authenticated() {
        let manifest = fixed_vault();
        let dek = fixed_dek();
        type Edit = fn(&mut Manifest);
        let tampered_cases: [(&str, Edit); 4] = [
            ("created_at", |m| m.created_at = "2030-01-01T00:00:00Z".into()),
            ("manifest_mac", |m| m.manifest_mac = BASE64.encode(&[0u8; 32])),
            // The DEK authenticated out of the manifest, so a bad key_check
            // can only mean tampering on the join path.
            ("key_check", |m| m.key_check = BASE64.encode(&[0u8; 32])),
            ("wrapped_dek", |m| m.wrapped_dek = "%%%".into()),
        ];
        for (field, edit) in tampered_cases {
            let mut tampered = manifest.clone();
            edit(&mut tampered);
            assert_eq!(
                unlock(&tampered, VAULT, FIXED_RK_TEXT, BINDING),
                Err(RecoveryError::Tampered),
                "{field}"
            );
        }
        let mut mac = manifest.clone();
        mac.created_at = "2030-01-01T00:00:00Z".into();
        assert_eq!(verify_manifest(&mac, VAULT, &dek, BINDING), Err(RecoveryError::Tampered));
        let mut binding = manifest.clone();
        binding.binding_hint = BASE64.encode(&[0u8; 32]);
        assert_eq!(
            unlock(&binding, VAULT, FIXED_RK_TEXT, BINDING),
            Err(RecoveryError::Tampered),
            "binding_hint is MAC-covered"
        );
        // With a keychain DEK, a key_check failure means a different vault key.
        let mut check = manifest.clone();
        check.key_check = BASE64.encode(&[0u8; 32]);
        assert_eq!(verify_manifest(&check, VAULT, &dek, BINDING), Err(RecoveryError::WrongKey));
        assert_eq!(
            verify_manifest(&manifest, VAULT, &SecretKey::from_bytes([0x41; 32]), BINDING),
            Err(RecoveryError::WrongKey)
        );
    }

    #[test]
    fn unsupported_format_suite_kind_or_epoch_is_held() {
        let manifest = fixed_vault();
        type Edit = fn(&mut Manifest);
        let cases: [Edit; 6] = [
            |m| m.format = 2,
            |m| m.suite = "aes-gcm".into(),
            |m| m.rk_kind = "passphrase".into(),
            |m| m.key_epoch = 0,
            |m| m.key_epoch = 2,
            |m| m.key_epoch = u32::MAX,
        ];
        for edit in cases {
            let mut future = manifest.clone();
            edit(&mut future);
            assert_eq!(unlock(&future, VAULT, FIXED_RK_TEXT, BINDING), Err(RecoveryError::Unsupported));
            assert_eq!(verify_manifest(&future, VAULT, &fixed_dek(), BINDING), Err(RecoveryError::Unsupported));
        }
    }

    #[test]
    fn manifest_rejects_unknown_fields() {
        let manifest = fixed_vault();
        let mut value = serde_json::to_value(&manifest).unwrap();
        value["extra"] = serde_json::json!("injected");
        assert!(serde_json::from_value::<Manifest>(value).is_err());
        let round = serde_json::from_value::<Manifest>(serde_json::to_value(&manifest).unwrap()).unwrap();
        assert_eq!(round, manifest);
    }

    #[test]
    fn other_account_binding_is_refused() {
        let manifest = fixed_vault();
        assert_eq!(
            unlock(&manifest, VAULT, FIXED_RK_TEXT, "gdrive:sub-999"),
            Err(RecoveryError::BindingMismatch)
        );
        assert_eq!(
            verify_manifest(&manifest, VAULT, &fixed_dek(), "onedrive:x"),
            Err(RecoveryError::BindingMismatch)
        );
    }
}
