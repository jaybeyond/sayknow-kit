//! Key material, HKDF subkey derivation, and HMAC helpers (plan §4.6).

use std::fmt;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub(crate) const KEY_LEN: usize = 32;

/// HKDF info for the key-encryption key that wraps the DEK under the RK.
const WRAP_INFO: &[u8] = b"sayknow/v1/wrap";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CryptoError {
    /// The OS random source failed; nothing was sealed.
    Rng,
    /// The envelope is shorter than the fixed header plus tag.
    Truncated,
    /// The envelope carries a format version this build does not read.
    UnsupportedVersion(u8),
    /// Authentication failed: wrong key, tampered bytes, or AAD mismatch.
    Decrypt,
    /// The plaintext exceeds what one envelope can encrypt.
    TooLarge,
    /// An AAD field does not fit its one-byte length prefix.
    FieldTooLong(&'static str),
    /// Decrypted plaintext disagrees with the AAD or the remote path.
    Mismatch(&'static str),
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rng => f.write_str("system random source unavailable"),
            Self::Truncated => f.write_str("encrypted object is truncated"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported envelope version {v}"),
            Self::Decrypt => f.write_str("encrypted object failed authentication"),
            Self::TooLarge => f.write_str("object is too large to encrypt"),
            Self::FieldTooLong(field) => write!(f, "{field} is too long for the envelope"),
            Self::Mismatch(field) => write!(f, "decrypted {field} does not match its location"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// A 256-bit secret (DEK, RK, KEK, or subkey). Wiped on drop and never
/// printed, so it cannot leak through logs or `{:?}`. Equality exists only in
/// tests: production code must not compare keys with a variable-time `==`.
/// Copies made inside the `hkdf`/`hmac` crates are outside our control.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) struct SecretKey([u8; KEY_LEN]);

impl SecretKey {
    /// Test fixtures only: a by-value array leaves a copy with the caller.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Copies key material straight into the wiped-on-drop buffer, so callers
    /// holding it in a `Zeroizing` buffer leave no stray copy behind.
    pub(crate) fn from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != KEY_LEN {
            return None;
        }
        let mut key = Self([0u8; KEY_LEN]);
        key.0.copy_from_slice(bytes);
        Some(key)
    }

    pub(crate) fn generate() -> Result<Self, CryptoError> {
        let mut key = Self([0u8; KEY_LEN]);
        fill_random(&mut key.0)?;
        Ok(key)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

pub(crate) fn fill_random(buf: &mut [u8]) -> Result<(), CryptoError> {
    getrandom::getrandom(buf).map_err(|_| CryptoError::Rng)
}

/// What a DEK-derived subkey is used for. Each purpose gets an independent key
/// so a flaw in one object kind cannot be replayed against another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    Segment,
    Snapshot,
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    Blob,
    DeviceRecord,
    BlobName,
    Manifest,
    KeyCheck,
}

impl Purpose {
    fn label(self) -> &'static str {
        match self {
            Self::Segment => "seg",
            Self::Snapshot => "snap",
            Self::Blob => "blob",
            Self::DeviceRecord => "rec",
            Self::BlobName => "name",
            Self::Manifest => "man",
            Self::KeyCheck => "check",
        }
    }
}

fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8]) -> SecretKey {
    let mut out = SecretKey([0u8; KEY_LEN]);
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out.0)
        // 32 bytes is far below HKDF-SHA256's 255 * 32 output limit.
        .expect("HKDF-SHA256 expand to 32 bytes cannot fail");
    out
}

/// `K_x = HKDF(DEK, salt=vault_id, info="sayknow/v1/"‖x‖"/epoch/"‖key_epoch)`.
pub(crate) fn derive_subkey(
    dek: &SecretKey,
    vault_id: &str,
    key_epoch: u32,
    purpose: Purpose,
) -> SecretKey {
    let info = format!("sayknow/v1/{}/epoch/{key_epoch}", purpose.label());
    hkdf_sha256(dek.as_bytes(), vault_id.as_bytes(), info.as_bytes())
}

/// `KEK = HKDF-SHA256(IKM=RK, salt=vault_id, info="sayknow/v1/wrap")`. Binding
/// the salt to the vault id means one RK cannot unwrap another vault's DEK.
pub(crate) fn derive_kek(recovery_key: &SecretKey, vault_id: &str) -> SecretKey {
    hkdf_sha256(recovery_key.as_bytes(), vault_id.as_bytes(), WRAP_INFO)
}

pub(crate) fn hmac_sha256(key: &SecretKey, message: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// Constant-time HMAC verification.
pub(crate) fn hmac_verify(key: &SecretKey, message: &[u8], tag: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message);
    mac.verify_slice(tag).is_ok()
}

/// Lowercase hex, as used for fingerprints and ids derived from hashes.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq_key(start: u8) -> SecretKey {
        let mut bytes = [0u8; KEY_LEN];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = start.wrapping_add(i as u8);
        }
        SecretKey::from_bytes(bytes)
    }

    // Expected values were computed with an independent Python stdlib HKDF
    // (hmac + hashlib) that reproduces RFC 5869 Appendix A.1 exactly.
    #[test]
    fn kek_matches_fixed_vector() {
        let rk = seq_key(0);
        let kek = derive_kek(&rk, "vault-test-0001");
        assert_eq!(
            hex(kek.as_bytes()),
            "1b8ea929b270b39219ecde58ddd4338ee448c305b24704a555244865ddcba5b6"
        );
    }

    #[test]
    fn kek_is_bound_to_vault_id() {
        let rk = seq_key(0);
        let other = derive_kek(&rk, "vault-test-0002");
        assert_eq!(
            hex(other.as_bytes()),
            "bd4edd8afa99646db8c84908adabae37a1ff3b0cbb77861f52216839aae23332"
        );
        assert_ne!(derive_kek(&rk, "vault-test-0001"), other);
    }

    #[test]
    fn subkeys_match_fixed_vectors_and_are_separated() {
        let dek = seq_key(0x40);
        let seg = derive_subkey(&dek, "vault-test-0001", 1, Purpose::Segment);
        let man = derive_subkey(&dek, "vault-test-0001", 1, Purpose::Manifest);
        assert_eq!(
            hex(seg.as_bytes()),
            "0cb8d302054f4b847cf6781d40c59abaf17bea06896ee4f57cff20993761f048"
        );
        assert_eq!(
            hex(man.as_bytes()),
            "95bb0c6b923109e71683f0e9e18b60bd74ee516602391aa9b934e3408250b821"
        );
        assert_ne!(seg, derive_subkey(&dek, "vault-test-0001", 2, Purpose::Segment));
        assert_ne!(seg, derive_subkey(&dek, "vault-test-0002", 1, Purpose::Segment));
    }

    #[test]
    fn every_purpose_gets_a_distinct_subkey() {
        let dek = seq_key(0x40);
        let purposes = [
            Purpose::Segment,
            Purpose::Snapshot,
            Purpose::Blob,
            Purpose::DeviceRecord,
            Purpose::BlobName,
            Purpose::Manifest,
            Purpose::KeyCheck,
        ];
        let keys: std::collections::HashSet<[u8; KEY_LEN]> = purposes
            .iter()
            .map(|p| *derive_subkey(&dek, "vault-test-0001", 1, *p).as_bytes())
            .collect();
        assert_eq!(keys.len(), purposes.len());
    }

    #[test]
    fn hmac_verify_rejects_tampering() {
        let key = seq_key(7);
        let tag = hmac_sha256(&key, b"manifest");
        assert!(hmac_verify(&key, b"manifest", &tag));
        assert!(!hmac_verify(&key, b"manifesT", &tag));
        assert!(!hmac_verify(&seq_key(8), b"manifest", &tag));
        assert!(!hmac_verify(&key, b"manifest", &tag[..31]));
    }

    #[test]
    fn debug_never_prints_key_bytes() {
        let printed = format!("{:?}", seq_key(0xAB));
        assert_eq!(printed, "SecretKey(<redacted>)");
    }
}
