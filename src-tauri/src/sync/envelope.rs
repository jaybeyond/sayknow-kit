//! The sealed-object format shared by every encrypted file in a vault
//! (plan §4.6.1):
//!
//! ```text
//! envelope = [1B version=0x01][24B nonce][ciphertext ‖ 16B tag]
//! aad      = "SKS1" ‖ kind ‖ u8len‖vault_id ‖ u32be key_epoch
//!            ‖ u8len‖device_id ‖ u8len‖object_ref ‖ u8 format=0x01
//! ```
//!
//! The AAD is rebuilt by the reader from the object's remote location, so a
//! file moved to another vault, device stream, sequence range, or object kind
//! fails authentication instead of being applied somewhere it does not belong.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use super::crypto::{fill_random, CryptoError, SecretKey};

pub(crate) const ENVELOPE_VERSION: u8 = 0x01;
pub(crate) const NONCE_LEN: usize = 24;
pub(crate) const TAG_LEN: usize = 16;
/// Version byte + nonce + tag: the size of an envelope around empty plaintext.
pub(crate) const MIN_ENVELOPE_LEN: usize = 1 + NONCE_LEN + TAG_LEN;

const AAD_MAGIC: &[u8; 4] = b"SKS1";
const AAD_FORMAT: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Segment,
    Snapshot,
    Blob,
    DeviceRecord,
    WrappedDek,
}

impl ObjectKind {
    fn code(self) -> u8 {
        match self {
            Self::Segment => 0x01,
            Self::Snapshot => 0x02,
            Self::Blob => 0x03,
            Self::DeviceRecord => 0x04,
            Self::WrappedDek => 0x05,
        }
    }
}

/// Where a sealed object lives. Everything here is authenticated as AAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectAddress {
    pub(crate) kind: ObjectKind,
    pub(crate) vault_id: String,
    pub(crate) key_epoch: u32,
    /// Empty for vault-wide objects (blobs, the wrapped DEK).
    pub(crate) device_id: String,
    pub(crate) object_ref: String,
}

impl ObjectAddress {
    pub(crate) fn segment(vault_id: &str, key_epoch: u32, device_id: &str, first: u64, last: u64) -> Self {
        Self {
            kind: ObjectKind::Segment,
            vault_id: vault_id.to_owned(),
            key_epoch,
            device_id: device_id.to_owned(),
            object_ref: segment_ref(first, last),
        }
    }

    pub(crate) fn snapshot(vault_id: &str, key_epoch: u32, device_id: &str, snap_id: &str) -> Self {
        Self {
            kind: ObjectKind::Snapshot,
            vault_id: vault_id.to_owned(),
            key_epoch,
            device_id: device_id.to_owned(),
            object_ref: snap_id.to_owned(),
        }
    }

    pub(crate) fn blob(vault_id: &str, key_epoch: u32, blob_id: &str) -> Self {
        Self {
            kind: ObjectKind::Blob,
            vault_id: vault_id.to_owned(),
            key_epoch,
            device_id: String::new(),
            object_ref: blob_id.to_owned(),
        }
    }

    pub(crate) fn device_record(vault_id: &str, key_epoch: u32, device_id: &str) -> Self {
        Self {
            kind: ObjectKind::DeviceRecord,
            vault_id: vault_id.to_owned(),
            key_epoch,
            device_id: device_id.to_owned(),
            object_ref: "rec".to_owned(),
        }
    }

    pub(crate) fn wrapped_dek(vault_id: &str, key_epoch: u32) -> Self {
        Self {
            kind: ObjectKind::WrappedDek,
            vault_id: vault_id.to_owned(),
            key_epoch,
            device_id: String::new(),
            object_ref: "dek".to_owned(),
        }
    }

    /// The provider-independent remote key for this object (plan §4.2), or
    /// `None` for the wrapped DEK (it lives inside the manifest) and for any
    /// address whose ids are not valid path components.
    pub(crate) fn remote_key(&self) -> Option<String> {
        let vault = &self.vault_id;
        let device = &self.device_id;
        let object = &self.object_ref;
        if !is_id(vault) {
            return None;
        }
        match self.kind {
            ObjectKind::Segment => {
                parse_segment_ref(object)?;
                is_id(device).then(|| format!("v1/{vault}/log/{device}/{object}.seg"))
            }
            ObjectKind::Snapshot => {
                (is_id(device) && is_id(object)).then(|| format!("v1/{vault}/snap/{device}/{object}.snap"))
            }
            ObjectKind::Blob => {
                let shard = blob_shard(object)?;
                (device.is_empty()).then(|| format!("v1/{vault}/blob/{shard}/{object}.blob"))
            }
            ObjectKind::DeviceRecord => {
                (is_id(device) && object == "rec").then(|| format!("v1/{vault}/devices/{device}.rec"))
            }
            ObjectKind::WrappedDek => None,
        }
    }

    /// Parses a remote key back into the address its AAD must carry. The key
    /// epoch is not part of the path; it comes from the vault manifest.
    pub(crate) fn from_remote_key(key: &str, key_epoch: u32) -> Option<Self> {
        let parts: Vec<&str> = key.split('/').collect();
        let ["v1", vault_id, kind, rest @ ..] = parts.as_slice() else {
            return None;
        };
        if !is_id(vault_id) {
            return None;
        }
        match (*kind, rest) {
            ("log", [device_id, file]) => {
                let range = file.strip_suffix(".seg")?;
                let (first, last) = parse_segment_ref(range)?;
                is_id(device_id).then(|| Self::segment(vault_id, key_epoch, device_id, first, last))
            }
            ("snap", [device_id, file]) => {
                let snap_id = file.strip_suffix(".snap")?;
                (is_id(device_id) && is_id(snap_id)).then(|| Self::snapshot(vault_id, key_epoch, device_id, snap_id))
            }
            ("blob", [shard, file]) => {
                let blob_id = file.strip_suffix(".blob")?;
                (blob_shard(blob_id) == Some(*shard)).then(|| Self::blob(vault_id, key_epoch, blob_id))
            }
            ("devices", [file]) => {
                let device_id = file.strip_suffix(".rec")?;
                is_id(device_id).then(|| Self::device_record(vault_id, key_epoch, device_id))
            }
            _ => None,
        }
    }

    fn aad(&self) -> Result<Vec<u8>, CryptoError> {
        let mut aad = Vec::with_capacity(
            AAD_MAGIC.len() + 1 + 1 + self.vault_id.len() + 4 + 1 + self.device_id.len() + 1 + self.object_ref.len() + 1,
        );
        aad.extend_from_slice(AAD_MAGIC);
        aad.push(self.kind.code());
        push_len_prefixed(&mut aad, &self.vault_id, "vault_id")?;
        aad.extend_from_slice(&self.key_epoch.to_be_bytes());
        push_len_prefixed(&mut aad, &self.device_id, "device_id")?;
        push_len_prefixed(&mut aad, &self.object_ref, "object_ref")?;
        aad.push(AAD_FORMAT);
        Ok(aad)
    }
}

/// Segment file stem: zero-padded so lexical order matches sequence order.
pub(crate) fn segment_ref(first: u64, last: u64) -> String {
    format!("{first:012}-{last:012}")
}

fn parse_segment_ref(range: &str) -> Option<(u64, u64)> {
    let (first, last) = range.split_once('-')?;
    if first.len() != 12 || last.len() != 12 {
        return None;
    }
    if !first.bytes().chain(last.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((first.parse().ok()?, last.parse().ok()?))
}

/// Longest id accepted in a remote path (UUIDs are 36, HMAC hex ids are 64).
pub(crate) const MAX_ID_LEN: usize = 64;

/// Vault, device, snapshot, and blob ids are app-generated, so the accepted
/// alphabet is deliberately narrow: ASCII letters, digits, `-`, `_`, `.`,
/// starting with a letter or digit. That rules out separators, dot-segments,
/// control characters, and anything a provider would need to percent-encode.
pub(crate) fn is_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_ID_LEN
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Blob ids are lowercase hex HMACs; the first two characters shard them.
fn blob_shard(blob_id: &str) -> Option<&str> {
    let hex = blob_id.len() >= 2
        && blob_id.len() <= MAX_ID_LEN
        && blob_id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if hex {
        blob_id.get(..2)
    } else {
        None
    }
}

fn push_len_prefixed(out: &mut Vec<u8>, value: &str, field: &'static str) -> Result<(), CryptoError> {
    let len = u8::try_from(value.len()).map_err(|_| CryptoError::FieldTooLong(field))?;
    out.push(len);
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn cipher(key: &SecretKey) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()))
}

/// Encrypts `plaintext` for `address` under a fresh random 192-bit nonce.
pub(crate) fn seal(key: &SecretKey, address: &ObjectAddress, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    seal_with_nonce(key, &nonce, address, plaintext)
}

fn seal_with_nonce(
    key: &SecretKey,
    nonce: &[u8; NONCE_LEN],
    address: &ObjectAddress,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let aad = address.aad()?;
    let ciphertext = cipher(key)
        .encrypt(XNonce::from_slice(nonce), Payload { msg: plaintext, aad: &aad })
        // Encryption only fails for plaintexts beyond the cipher's 256 GiB limit.
        .map_err(|_| CryptoError::TooLarge)?;
    let mut envelope = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    envelope.push(ENVELOPE_VERSION);
    envelope.extend_from_slice(nonce);
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

/// Decrypts an envelope that must belong at `address`. The plaintext is wiped
/// when dropped.
pub(crate) fn open(
    key: &SecretKey,
    address: &ObjectAddress,
    envelope: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if envelope.len() < MIN_ENVELOPE_LEN {
        return Err(CryptoError::Truncated);
    }
    if envelope[0] != ENVELOPE_VERSION {
        return Err(CryptoError::UnsupportedVersion(envelope[0]));
    }
    let aad = address.aad()?;
    let (nonce, ciphertext) = envelope[1..].split_at(NONCE_LEN);
    cipher(key)
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ciphertext, aad: &aad })
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}

/// Identity fields a decrypted segment claims about itself.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SegmentHeader<'a> {
    pub(crate) device_id: &'a str,
    pub(crate) first_seq: u64,
    pub(crate) last_seq: u64,
    pub(crate) op_seqs: &'a [u64],
}

/// Cross-checks a decrypted segment against the location it was opened at.
/// A vault member holding the DEK can produce an envelope that authenticates
/// under the right AAD while its plaintext claims another device or range;
/// that segment must be quarantined, never applied.
pub(crate) fn check_segment(header: &SegmentHeader<'_>, address: &ObjectAddress) -> Result<(), CryptoError> {
    if address.kind != ObjectKind::Segment {
        return Err(CryptoError::Mismatch("object kind"));
    }
    if header.device_id != address.device_id {
        return Err(CryptoError::Mismatch("device_id"));
    }
    if header.first_seq == 0
        || header.first_seq > header.last_seq
        || segment_ref(header.first_seq, header.last_seq) != address.object_ref
    {
        return Err(CryptoError::Mismatch("sequence range"));
    }
    let contiguous = header
        .op_seqs
        .iter()
        .zip(header.first_seq..=header.last_seq)
        .all(|(seq, expected)| *seq == expected);
    let expected_count = header.last_seq - header.first_seq + 1;
    if !contiguous || header.op_seqs.len() as u64 != expected_count {
        return Err(CryptoError::Mismatch("op sequence"));
    }
    Ok(())
}

/// Cross-checks the owner and id a decrypted snapshot claims.
pub(crate) fn check_snapshot_owner(device_id: &str, snap_id: &str, address: &ObjectAddress) -> Result<(), CryptoError> {
    if address.kind != ObjectKind::Snapshot {
        return Err(CryptoError::Mismatch("object kind"));
    }
    if device_id != address.device_id {
        return Err(CryptoError::Mismatch("device_id"));
    }
    if snap_id != address.object_ref {
        return Err(CryptoError::Mismatch("snap_id"));
    }
    Ok(())
}

/// Cross-checks the owner a decrypted device record claims. The record
/// plaintext must therefore carry its own `device_id`.
pub(crate) fn check_record_owner(device_id: &str, address: &ObjectAddress) -> Result<(), CryptoError> {
    if address.kind != ObjectKind::DeviceRecord {
        return Err(CryptoError::Mismatch("object kind"));
    }
    if device_id != address.device_id {
        return Err(CryptoError::Mismatch("device_id"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::sync::crypto::hex;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn key(byte: u8) -> SecretKey {
        SecretKey::from_bytes([byte; 32])
    }

    fn seg_addr() -> ObjectAddress {
        ObjectAddress::segment("vault-a", 1, "dev-a", 1, 3)
    }

    // draft-irtf-cfrg-xchacha-03, Appendix A.3.1 (AEAD_XCHACHA20_POLY1305).
    #[test]
    fn primitive_matches_published_xchacha_vector() {
        let key = unhex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let nonce = unhex("404142434445464748494a4b4c4d4e4f5051525354555657");
        let aad = unhex("50515253c0c1c2c3c4c5c6c7");
        let plaintext = unhex(
            "4c616469657320616e642047656e746c656d656e206f662074686520636c6173
             73206f66202739393a204966204920636f756c64206f6666657220796f75206f
             6e6c79206f6e652074697020666f7220746865206675747572652c2073756e73
             637265656e20776f756c642062652069742e",
        );
        let expected = unhex(
            "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb
             731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452
             2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9
             21f9664c97637da9768812f615c68b13b52e
             c0875924c1c7987947deafd8780acf49",
        );
        let out = XChaCha20Poly1305::new(Key::from_slice(&key))
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: &plaintext, aad: &aad })
            .unwrap();
        assert_eq!(hex(&out), hex(&expected));
    }

    #[test]
    fn aad_layout_is_fixed() {
        let mut expected = b"SKS1".to_vec();
        expected.push(0x01);
        expected.push(7);
        expected.extend_from_slice(b"vault-a");
        expected.extend_from_slice(&[0, 0, 0, 1]);
        expected.push(5);
        expected.extend_from_slice(b"dev-a");
        expected.push(25);
        expected.extend_from_slice(b"000000000001-000000000003");
        expected.push(0x01);
        assert_eq!(seg_addr().aad().unwrap(), expected);

        let dek = ObjectAddress::wrapped_dek("vault-a", 1).aad().unwrap();
        let mut expected_dek = b"SKS1".to_vec();
        expected_dek.extend_from_slice(&[0x05, 7]);
        expected_dek.extend_from_slice(b"vault-a");
        expected_dek.extend_from_slice(&[0, 0, 0, 1, 0, 3]);
        expected_dek.extend_from_slice(b"dek");
        expected_dek.push(0x01);
        assert_eq!(dek, expected_dek);
    }

    #[test]
    fn envelope_layout_is_version_nonce_ciphertext() {
        let k = key(9);
        let nonce = [0x42; NONCE_LEN];
        let addr = seg_addr();
        let env = seal_with_nonce(&k, &nonce, &addr, b"hello").unwrap();
        assert_eq!(env.len(), MIN_ENVELOPE_LEN + 5);
        assert_eq!(env[0], 0x01);
        assert_eq!(&env[1..25], &nonce);
        let raw = cipher(&k)
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: b"hello", aad: &addr.aad().unwrap() })
            .unwrap();
        assert_eq!(&env[25..], raw.as_slice());
        assert_eq!(open(&k, &addr, &env).unwrap().as_slice(), b"hello");
    }

    // Literal golden envelopes. The primitive is pinned to the published
    // XChaCha vector and the AAD bytes to `aad_layout_is_fixed`, so these
    // freeze the composed on-disk format for other implementations.
    #[test]
    fn segment_envelope_golden_vector() {
        let env = seal_with_nonce(&key(9), &[0x42; NONCE_LEN], &seg_addr(), b"hello").unwrap();
        assert_eq!(
            hex(&env),
            "01424242424242424242424242424242424242424242424242b8e5eae93c556670f2b50270efcde8feab647ff5b7"
        );
    }

    #[test]
    fn wrapped_dek_envelope_golden_vector() {
        let addr = ObjectAddress::wrapped_dek("vault-a", 1);
        let env = seal_with_nonce(&key(0x11), &[0x24; NONCE_LEN], &addr, &[0x40; 32]).unwrap();
        assert_eq!(
            hex(&env),
            concat!(
                "012424242424242424242424242424242424242424242424",
                "24430c95293a7f74c380b49057722a023ab4bd330f271fe1f07e7c6836a5648aa9a28ce5f605a37572091104050464b25d"
            )
        );
    }

    #[test]
    fn empty_plaintext_round_trips_at_minimum_length() {
        let env = seal(&key(1), &seg_addr(), b"").unwrap();
        assert_eq!(env.len(), MIN_ENVELOPE_LEN);
        assert!(open(&key(1), &seg_addr(), &env).unwrap().is_empty());
    }

    #[test]
    fn every_aad_field_is_authenticated() {
        let k = key(3);
        let base = seg_addr();
        let env = seal(&k, &base, b"payload").unwrap();
        let mut variants = Vec::new();
        let mut kind = base.clone();
        kind.kind = ObjectKind::Snapshot;
        variants.push(kind);
        let mut vault = base.clone();
        vault.vault_id = "vault-b".into();
        variants.push(vault);
        let mut epoch = base.clone();
        epoch.key_epoch = 2;
        variants.push(epoch);
        let mut device = base.clone();
        device.device_id = "dev-b".into();
        variants.push(device);
        variants.push(ObjectAddress::segment("vault-a", 1, "dev-a", 1, 4));
        for addr in variants {
            assert_eq!(open(&k, &addr, &env), Err(CryptoError::Decrypt), "{addr:?}");
        }
        assert_eq!(open(&key(4), &base, &env), Err(CryptoError::Decrypt));
    }

    #[test]
    fn tampered_bytes_fail_authentication() {
        let k = key(5);
        let env = seal(&k, &seg_addr(), b"payload").unwrap();
        for index in 1..env.len() {
            let mut bad = env.clone();
            bad[index] ^= 0x01;
            assert_eq!(open(&k, &seg_addr(), &bad), Err(CryptoError::Decrypt), "byte {index}");
        }
    }

    #[test]
    fn truncated_or_unknown_version_is_rejected() {
        let k = key(6);
        let env = seal(&k, &seg_addr(), b"x").unwrap();
        assert_eq!(open(&k, &seg_addr(), &env[..MIN_ENVELOPE_LEN - 1]), Err(CryptoError::Truncated));
        assert_eq!(open(&k, &seg_addr(), &[]), Err(CryptoError::Truncated));
        let mut v2 = env.clone();
        v2[0] = 0x02;
        assert_eq!(open(&k, &seg_addr(), &v2), Err(CryptoError::UnsupportedVersion(2)));
    }

    #[test]
    fn ten_thousand_seals_never_repeat_a_nonce() {
        let k = key(7);
        let addr = seg_addr();
        let mut nonces = HashSet::new();
        for _ in 0..10_000 {
            let env = seal(&k, &addr, b"n").unwrap();
            assert!(nonces.insert(env[1..25].to_vec()));
        }
    }

    #[test]
    fn oversized_aad_field_is_rejected() {
        let addr = ObjectAddress::device_record(&"v".repeat(256), 1, "dev");
        assert_eq!(seal(&key(1), &addr, b"x"), Err(CryptoError::FieldTooLong("vault_id")));
    }

    #[test]
    fn remote_keys_round_trip() {
        let addrs = [
            seg_addr(),
            ObjectAddress::snapshot("vault-a", 1, "dev-a", "snap-9"),
            ObjectAddress::blob("vault-a", 1, "ab12cd"),
            ObjectAddress::device_record("vault-a", 1, "dev-a"),
        ];
        for addr in addrs {
            let key = addr.remote_key().unwrap();
            assert_eq!(ObjectAddress::from_remote_key(&key, 1), Some(addr));
        }
        assert_eq!(seg_addr().remote_key().unwrap(), "v1/vault-a/log/dev-a/000000000001-000000000003.seg");
        assert_eq!(ObjectAddress::wrapped_dek("vault-a", 1).remote_key(), None);
    }

    #[test]
    fn malformed_remote_keys_are_rejected() {
        for key in [
            "v2/vault-a/log/dev-a/000000000001-000000000003.seg",
            "v1/vault-a/log/dev-a/1-3.seg",
            "v1/vault-a/log/dev-a/00000000000x-000000000003.seg",
            "v1/vault-a/log/../000000000001-000000000003.seg",
            "v1/vault-a/blob/zz/ab12cd.blob",
            "v1/vault-a/devices/dev-a.txt",
            "v1//devices/dev-a.rec",
            "v1/vault-a/log/dev-a/extra/000000000001-000000000003.seg",
        ] {
            assert_eq!(ObjectAddress::from_remote_key(key, 1), None, "{key}");
        }
    }

    #[test]
    fn segment_opened_elsewhere_than_its_aad_fails() {
        let k = key(8);
        let env = seal(&k, &seg_addr(), b"ops").unwrap();
        let moved = ObjectAddress::from_remote_key("v1/vault-a/log/dev-b/000000000001-000000000003.seg", 1).unwrap();
        assert_eq!(open(&k, &moved, &env), Err(CryptoError::Decrypt));
    }

    #[test]
    fn segment_plaintext_must_match_its_location() {
        let addr = seg_addr();
        let ok = SegmentHeader { device_id: "dev-a", first_seq: 1, last_seq: 3, op_seqs: &[1, 2, 3] };
        assert_eq!(check_segment(&ok, &addr), Ok(()));

        // A vault member re-encrypted these under the correct AAD, so the
        // envelope authenticates; only the plaintext check catches them.
        let cases = [
            (SegmentHeader { device_id: "dev-b", ..ok }, "device_id"),
            (SegmentHeader { first_seq: 2, ..ok }, "sequence range"),
            (SegmentHeader { last_seq: 4, op_seqs: &[1, 2, 3, 4], ..ok }, "sequence range"),
            (SegmentHeader { op_seqs: &[1, 3, 2], ..ok }, "op sequence"),
            (SegmentHeader { op_seqs: &[1, 2], ..ok }, "op sequence"),
            (SegmentHeader { op_seqs: &[1, 2, 3, 4], ..ok }, "op sequence"),
        ];
        for (header, field) in cases {
            assert_eq!(check_segment(&header, &addr), Err(CryptoError::Mismatch(field)), "{header:?}");
        }
        let zero = ObjectAddress::segment("vault-a", 1, "dev-a", 0, 0);
        let header = SegmentHeader { device_id: "dev-a", first_seq: 0, last_seq: 0, op_seqs: &[0] };
        assert_eq!(check_segment(&header, &zero), Err(CryptoError::Mismatch("sequence range")));
    }

    #[test]
    fn re_encrypted_segment_with_foreign_plaintext_is_detected_end_to_end() {
        let k = key(10);
        let addr = seg_addr();
        let forged = br#"{"device_id":"dev-b","first_seq":1,"last_seq":3}"#;
        let env = seal(&k, &addr, forged).unwrap();
        let plaintext = open(&k, &addr, &env).expect("authenticates under the correct AAD");
        let parsed: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
        let header = SegmentHeader {
            device_id: parsed["device_id"].as_str().unwrap(),
            first_seq: parsed["first_seq"].as_u64().unwrap(),
            last_seq: parsed["last_seq"].as_u64().unwrap(),
            op_seqs: &[1, 2, 3],
        };
        assert_eq!(check_segment(&header, &addr), Err(CryptoError::Mismatch("device_id")));
    }

    #[test]
    fn owner_checks_cover_snapshots_and_records() {
        let snap = ObjectAddress::snapshot("vault-a", 1, "dev-a", "snap-1");
        assert_eq!(check_snapshot_owner("dev-a", "snap-1", &snap), Ok(()));
        assert_eq!(check_snapshot_owner("dev-b", "snap-1", &snap), Err(CryptoError::Mismatch("device_id")));
        assert_eq!(check_snapshot_owner("dev-a", "snap-2", &snap), Err(CryptoError::Mismatch("snap_id")));
        let rec = ObjectAddress::device_record("vault-a", 1, "dev-a");
        assert_eq!(check_record_owner("dev-a", &rec), Ok(()));
        assert_eq!(check_record_owner("dev-b", &rec), Err(CryptoError::Mismatch("device_id")));
        assert_eq!(check_record_owner("dev-a", &snap), Err(CryptoError::Mismatch("object kind")));
        assert_eq!(check_snapshot_owner("dev-a", "rec", &rec), Err(CryptoError::Mismatch("object kind")));
    }

    #[test]
    fn remote_keys_refuse_ids_outside_the_safe_alphabet() {
        let long = "d".repeat(MAX_ID_LEN + 1);
        for addr in [
            ObjectAddress::segment("vault/a", 1, "dev-a", 1, 3),
            ObjectAddress::segment("vault-a", 1, "..", 1, 3),
            ObjectAddress::segment("vault-a", 1, "dév", 1, 3),
            ObjectAddress::segment("vault-a", 1, &long, 1, 3),
            ObjectAddress::snapshot("vault-a", 1, "dev-a", ".hidden"),
            ObjectAddress::blob("vault-a", 1, "AB12"),
            ObjectAddress::blob("vault-a", 1, "a"),
            ObjectAddress::device_record("vault-a", 1, "dev\u{0}a"),
        ] {
            assert_eq!(addr.remote_key(), None, "{addr:?}");
        }
        assert!(is_id(&"d".repeat(MAX_ID_LEN)));
        assert!(is_id("0189f2c4-7b1e-7000-8000-00000000abcd"));
        assert!(!is_id(""));
        assert!(!is_id("-leading"));
    }
}
