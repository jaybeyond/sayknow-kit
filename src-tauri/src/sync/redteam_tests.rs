//! G1 red-team tests for the sync crypto core, written from the approved plan
//! (§4.2, §4.6–§4.6.3, AC 13/22/23) rather than from the implementation.
//!
//! Reference values were produced independently with the Python standard
//! library (`hmac` + `hashlib` HKDF that reproduces RFC 5869 A.1, `base64`).
//! The AAD and manifest MAC body are re-encoded here from the plan text, so a
//! drift in the production encoder fails these tests.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use data_encoding::{BASE32_NOPAD, BASE64};
use sha2::{Digest, Sha256};

use super::crypto::{derive_kek, derive_subkey, hex, hmac_sha256, CryptoError, Purpose, SecretKey};
use super::envelope::{
    check_segment, is_id, open, seal, ObjectAddress, ObjectKind, SegmentHeader, MIN_ENVELOPE_LEN, NONCE_LEN,
    TAG_LEN,
};
use super::recovery::{unlock, verify_manifest, Manifest, RecoveryError, RecoveryKey, RK_KIND_RANDOM, SUITE};

const VAULT: &str = "vault-test-0001";
const OTHER_VAULT: &str = "vault-test-0002";
const BINDING: &str = "gdrive:sub-123";
const CREATED: &str = "2026-10-10T00:00:00Z";

/// RK bytes 00..1f, Base32 of (RK ‖ SHA-256("sayknow-rk-v1" ‖ RK)[..3]) from Python.
const PY_RK_TEXT: &str = "AAAQ-EAYE-AUDA-OCAJ-BIFQ-YDIO-B4IB-CEQT-CQKR-MFYY-DENB-WHA5-DYPW-E5JN";

fn rk_bytes() -> [u8; 32] {
    core::array::from_fn(|i| i as u8)
}

fn dek_bytes() -> [u8; 32] {
    core::array::from_fn(|i| 0x40 + i as u8)
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn key32(s: &str) -> SecretKey {
    SecretKey::from_bytes(unhex(s).try_into().unwrap())
}

/// The plan's AAD, encoded independently of `ObjectAddress::aad`.
fn contract_aad(kind: u8, vault: &str, epoch: u32, device: &str, object_ref: &str) -> Vec<u8> {
    let mut out = b"SKS1".to_vec();
    out.push(kind);
    for (i, field) in [vault, device, object_ref].iter().enumerate() {
        if i == 1 {
            out.extend_from_slice(&epoch.to_be_bytes());
        }
        out.push(u8::try_from(field.len()).unwrap());
        out.extend_from_slice(field.as_bytes());
    }
    out.push(0x01);
    out
}

fn kind_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Segment => 1,
        ObjectKind::Snapshot => 2,
        ObjectKind::Blob => 3,
        ObjectKind::DeviceRecord => 4,
        ObjectKind::WrappedDek => 5,
    }
}

fn raw_open(key: &SecretKey, aad: &[u8], envelope: &[u8]) -> Option<Vec<u8>> {
    XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()))
        .decrypt(XNonce::from_slice(&envelope[1..25]), Payload { msg: &envelope[25..], aad })
        .ok()
}

/// The manifest MAC body exactly as §4.6.2 lists it (every field but the MAC),
/// in declaration order, compact JSON.
fn canonical_body(m: &Manifest) -> Vec<u8> {
    let s = |v: &str| serde_json::to_string(v).unwrap();
    format!(
        r#"{{"format":{},"vault_id":{},"created_at":{},"suite":{},"key_epoch":{},"rk_kind":{},"wrapped_dek":{},"key_check":{},"binding_hint":{}}}"#,
        m.format,
        s(&m.vault_id),
        s(&m.created_at),
        s(&m.suite),
        m.key_epoch,
        s(&m.rk_kind),
        s(&m.wrapped_dek),
        s(&m.key_check),
        s(&m.binding_hint)
    )
    .into_bytes()
}

/// Builds a manifest from the plan's recipe with explicit keys, so a test can
/// produce genuine manifests the production builder never would.
fn manifest_with_keys(
    kek: &SecretKey,
    dek: &SecretKey,
    k_man: &SecretKey,
    k_check: &SecretKey,
    vault: &str,
    epoch: u32,
) -> Manifest {
    let wrapped = seal(kek, &ObjectAddress::wrapped_dek(vault, epoch), dek.as_bytes()).unwrap();
    let mut m = Manifest {
        format: 1,
        vault_id: vault.to_owned(),
        created_at: CREATED.to_owned(),
        suite: SUITE.to_owned(),
        key_epoch: epoch,
        rk_kind: RK_KIND_RANDOM.to_owned(),
        wrapped_dek: BASE64.encode(&wrapped),
        key_check: BASE64.encode(&hmac_sha256(k_check, b"sayknow-key-check")),
        binding_hint: BASE64.encode(&hmac_sha256(k_man, BINDING.as_bytes())),
        manifest_mac: String::new(),
    };
    m.manifest_mac = BASE64.encode(&hmac_sha256(k_man, &canonical_body(&m)));
    m
}

fn genuine_manifest(rk: [u8; 32], dek: &SecretKey, vault: &str, epoch: u32) -> Manifest {
    manifest_with_keys(
        &derive_kek(&SecretKey::from_bytes(rk), vault),
        dek,
        &derive_subkey(dek, vault, epoch, Purpose::Manifest),
        &derive_subkey(dek, vault, epoch, Purpose::KeyCheck),
        vault,
        epoch,
    )
}

/// Independent RK display encoder (plan §4.6.3: Base32 + checksum).
fn rk_text(rk: [u8; 32]) -> String {
    let checksum = Sha256::new().chain_update(b"sayknow-rk-v1").chain_update(rk).finalize();
    let mut raw = rk.to_vec();
    raw.extend_from_slice(&checksum[..3]);
    let plain = BASE32_NOPAD.encode(&raw);
    plain.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-")
}

// ---------------------------------------------------------------- KEK / HKDF

/// RT-KDF-1: KEK and every subkey purpose match Python-stdlib HKDF vectors,
/// including a non-ASCII vault id and the u32::MAX epoch boundary.
#[test]
fn rt_kdf_vectors_match_independent_python_hkdf() {
    let rk = SecretKey::from_bytes(rk_bytes());
    let keks = [
        (VAULT, "1b8ea929b270b39219ecde58ddd4338ee448c305b24704a555244865ddcba5b6"),
        (OTHER_VAULT, "bd4edd8afa99646db8c84908adabae37a1ff3b0cbb77861f52216839aae23332"),
        ("vault-é", "d8a8fa3964a63e10de8aa499014f28b52a3f6a090a40c84fd8fd5226fd867df2"),
        ("v", "d26268ac42b9034163e106dcb6d313db0b7cde0d708234be4c869ca2c5a81e27"),
    ];
    for (vault, expected) in keks {
        assert_eq!(hex(derive_kek(&rk, vault).as_bytes()), expected, "{vault}");
    }

    let dek = SecretKey::from_bytes(dek_bytes());
    let subkeys = [
        (Purpose::Segment, 1, "0cb8d302054f4b847cf6781d40c59abaf17bea06896ee4f57cff20993761f048"),
        (Purpose::Segment, 2, "5960c1b55b077add7edd83ac846f7bb6e931e7c394027e9c5b6060ff654c175a"),
        (Purpose::Segment, u32::MAX, "8d724ce11119348c1d18e49ff51fa0423e85496519843df8b978a31d71c26d7a"),
        (Purpose::Snapshot, 1, "25ad51fd19dbaa1b7007c2578506eac1b7d37a196be2a7269a6dbd6843d62f9a"),
        (Purpose::Blob, 1, "c6bba90e9a219f530eed04340c8af7aefeb504ea54f68bb057ca02ae8c59e982"),
        (Purpose::DeviceRecord, 1, "37b4a44231f01fa4ab586a30d0700dfa486ca01e9119eb5767d01d66b52b384b"),
        (Purpose::BlobName, 1, "6f7586bd65ff95613ed0164c477e33218a448dfd1c9351e79bff5d321c8735b7"),
        (Purpose::Manifest, 1, "95bb0c6b923109e71683f0e9e18b60bd74ee516602391aa9b934e3408250b821"),
        (Purpose::Manifest, 2, "91d63ce6904ae114cf3f32b25ef4ca9fe2ac4ff811d1cece0cf2a653367f10e0"),
        (Purpose::KeyCheck, 1, "db914e72934c0aca37ce8dd922c7cc11c398a0b69570f3937e53e6779ad26137"),
        (Purpose::KeyCheck, u32::MAX, "72ac9aa3ec858eb287ed74f752f303044df4aae7b3bab337c415e5c093a10fa1"),
    ];
    for (purpose, epoch, expected) in subkeys {
        assert_eq!(hex(derive_subkey(&dek, VAULT, epoch, purpose).as_bytes()), expected, "{purpose:?}/{epoch}");
    }
}

/// RT-KDF-2: a manifest assembled only from Python-derived KEK/K_man/K_check
/// and the Python RK text unlocks to the expected DEK through production code.
#[test]
fn rt_kdf_python_built_manifest_unlocks_end_to_end() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let manifest = manifest_with_keys(
        &key32("1b8ea929b270b39219ecde58ddd4338ee448c305b24704a555244865ddcba5b6"),
        &dek,
        &key32("95bb0c6b923109e71683f0e9e18b60bd74ee516602391aa9b934e3408250b821"),
        &key32("db914e72934c0aca37ce8dd922c7cc11c398a0b69570f3937e53e6779ad26137"),
        VAULT,
        1,
    );
    assert_eq!(
        hex(&BASE64.decode(manifest.key_check.as_bytes()).unwrap()),
        "43a3116627f5a44aff2741366268108ac656d733fa7dc3048fe04dce7d5d509c"
    );
    assert_eq!(
        hex(&BASE64.decode(manifest.binding_hint.as_bytes()).unwrap()),
        "32baebfc0ab828aa2ba476ae11a7fb6592504c2cf7d8b3faa49771bb75cfe5de"
    );
    assert_eq!(unlock(&manifest, VAULT, PY_RK_TEXT, BINDING).unwrap(), dek);
    assert_eq!(verify_manifest(&manifest, VAULT, &dek, BINDING), Ok(()));
}

// ---------------------------------------------------------------- AAD

fn tricky_addresses() -> Vec<ObjectAddress> {
    let long = "x".repeat(255);
    vec![
        // Field-boundary shifts of the same concatenated bytes.
        ObjectAddress::snapshot("ab", 1, "c", "d"),
        ObjectAddress::snapshot("a", 1, "bc", "d"),
        ObjectAddress::snapshot("a", 1, "b", "cd"),
        ObjectAddress::snapshot("abc", 1, "", "d"),
        ObjectAddress::snapshot("a", 1, "", "bcd"),
        // A vault id that embeds what looks like an epoch and a length prefix.
        ObjectAddress::snapshot("v\0\0\0\x01\x01c", 1, "d", "e"),
        ObjectAddress::snapshot("v", 1, "c", "d"),
        // Epoch byte-boundary values.
        ObjectAddress::snapshot("v", 0, "c", "d"),
        ObjectAddress::snapshot("v", 256, "c", "d"),
        ObjectAddress::snapshot("v", 1 << 24, "c", "d"),
        ObjectAddress::snapshot("v", u32::MAX, "c", "d"),
        // Same strings, different kinds.
        ObjectAddress::device_record("v", 1, "c"),
        ObjectAddress::snapshot("v", 1, "c", "rec"),
        ObjectAddress::blob("v", 1, "dek"),
        ObjectAddress::wrapped_dek("v", 1),
        ObjectAddress::blob("v", 1, "rec"),
        ObjectAddress::segment("v", 1, "c", 1, 2),
        ObjectAddress::snapshot("v", 1, "c", "000000000001-000000000002"),
        ObjectAddress::segment("v", 1, "c", 12, 2),
        ObjectAddress::segment("v", 1, "c", 1, 22),
        // Maximum-length fields.
        ObjectAddress::snapshot(&long, 1, &long, &long),
        ObjectAddress::snapshot(&long, 1, &long, &long[1..]),
    ]
}

/// RT-AAD-1: no two distinct addresses (field swaps, boundary shifts, kinds,
/// epochs) share an AAD — an envelope opens only at its own address.
#[test]
fn rt_aad_distinct_addresses_never_cross_open() {
    let key = SecretKey::from_bytes([0x11; 32]);
    let addrs = tricky_addresses();
    let envelopes: Vec<Vec<u8>> = addrs.iter().map(|a| seal(&key, a, b"payload").unwrap()).collect();
    for (i, env) in envelopes.iter().enumerate() {
        for (j, addr) in addrs.iter().enumerate() {
            let result = open(&key, addr, env);
            if i == j {
                assert_eq!(result.unwrap().as_slice(), b"payload");
            } else {
                assert_ne!(addrs[i], addrs[j]);
                assert_eq!(result, Err(CryptoError::Decrypt), "{:?} opened at {addr:?}", addrs[i]);
            }
        }
    }
}

/// RT-AAD-2: production envelopes decrypt with the AAD re-encoded from the plan
/// text and with no other byte layout (e.g. without the trailing format byte).
#[test]
fn rt_aad_matches_contract_encoding() {
    let key = SecretKey::from_bytes([0x22; 32]);
    for addr in tricky_addresses() {
        let env = seal(&key, &addr, b"p").unwrap();
        let aad = contract_aad(kind_code(addr.kind), &addr.vault_id, addr.key_epoch, &addr.device_id, &addr.object_ref);
        assert_eq!(raw_open(&key, &aad, &env).as_deref(), Some(b"p".as_slice()), "{addr:?}");
        assert_eq!(raw_open(&key, &aad[..aad.len() - 1], &env), None);
    }
    let dek_env = seal(&key, &ObjectAddress::wrapped_dek("v", 1), b"k").unwrap();
    assert!(raw_open(&key, &contract_aad(5, "v", 1, "", "dek"), &dek_env).is_some());
}

/// RT-AAD-3: a 255-byte field is accepted, 256 bytes (counted in UTF-8 bytes,
/// not chars) is refused on both seal and open before any crypto.
#[test]
fn rt_aad_length_prefix_overflow_fails_closed() {
    let key = SecretKey::from_bytes([0x33; 32]);
    let ok = ObjectAddress::snapshot(&"v".repeat(255), 1, &"d".repeat(255), &"s".repeat(255));
    assert!(open(&key, &ok, &seal(&key, &ok, b"x").unwrap()).is_ok());
    let env = seal(&key, &ObjectAddress::device_record("v", 1, "d"), b"x").unwrap();
    for (addr, field) in [
        (ObjectAddress::device_record(&"v".repeat(256), 1, "d"), "vault_id"),
        (ObjectAddress::device_record("v", 1, &"é".repeat(128)), "device_id"),
        (ObjectAddress::snapshot("v", 1, "d", &"s".repeat(10_000)), "object_ref"),
    ] {
        assert_eq!(seal(&key, &addr, b"x"), Err(CryptoError::FieldTooLong(field)));
        assert_eq!(open(&key, &addr, &env), Err(CryptoError::FieldTooLong(field)));
    }
}

// ---------------------------------------------------------------- envelope

fn seg() -> ObjectAddress {
    ObjectAddress::segment("vault-a", 1, "dev-a", 1, 3)
}

/// RT-ENV-1: every truncation length 0..=41 is rejected (Truncated below 41,
/// Decrypt at 41 and above), and appending a byte fails authentication.
#[test]
fn rt_env_every_truncation_is_rejected() {
    let key = SecretKey::from_bytes([0x44; 32]);
    let env = seal(&key, &seg(), b"payload").unwrap();
    assert_eq!(env.len(), MIN_ENVELOPE_LEN + 7);
    assert_eq!(MIN_ENVELOPE_LEN, 41);
    for len in 0..=MIN_ENVELOPE_LEN {
        let expected = if len < MIN_ENVELOPE_LEN { CryptoError::Truncated } else { CryptoError::Decrypt };
        assert_eq!(open(&key, &seg(), &env[..len]), Err(expected), "len {len}");
    }
    for len in MIN_ENVELOPE_LEN..env.len() {
        assert_eq!(open(&key, &seg(), &env[..len]), Err(CryptoError::Decrypt), "len {len}");
    }
    let mut longer = env.clone();
    longer.push(0);
    assert_eq!(open(&key, &seg(), &longer), Err(CryptoError::Decrypt));

    let empty = seal(&key, &seg(), b"").unwrap();
    assert_eq!(empty.len(), 41);
    for len in 0..41 {
        assert_eq!(open(&key, &seg(), &empty[..len]), Err(CryptoError::Truncated), "empty len {len}");
    }
    assert!(open(&key, &seg(), &empty).unwrap().is_empty());
}

/// RT-ENV-2: all 256 version byte values: only 0x01 opens; the rest report
/// the version; a short envelope reports Truncated regardless of version.
#[test]
fn rt_env_every_version_byte() {
    let key = SecretKey::from_bytes([0x55; 32]);
    let env = seal(&key, &seg(), b"payload").unwrap();
    for version in 0..=u8::MAX {
        let mut bad = env.clone();
        bad[0] = version;
        let result = open(&key, &seg(), &bad);
        if version == 0x01 {
            assert_eq!(result.unwrap().as_slice(), b"payload");
        } else {
            assert_eq!(result, Err(CryptoError::UnsupportedVersion(version)));
        }
        let mut short = vec![version];
        short.extend_from_slice(&[0u8; 39]);
        assert_eq!(open(&key, &seg(), &short), Err(CryptoError::Truncated));
    }
}

/// RT-ENV-3: every single bit of nonce, ciphertext, and tag is authenticated.
#[test]
fn rt_env_every_nonce_and_tag_bit_flip_fails() {
    let key = SecretKey::from_bytes([0x66; 32]);
    let env = seal(&key, &seg(), b"payload").unwrap();
    let tag_start = env.len() - TAG_LEN;
    let mut nonce_bits = 0;
    let mut tag_bits = 0;
    for index in 1..env.len() {
        for bit in 0..8 {
            let mut bad = env.clone();
            bad[index] ^= 1 << bit;
            assert_eq!(open(&key, &seg(), &bad), Err(CryptoError::Decrypt), "byte {index} bit {bit}");
            if index <= NONCE_LEN {
                nonce_bits += 1;
            } else if index >= tag_start {
                tag_bits += 1;
            }
        }
    }
    assert_eq!((nonce_bits, tag_bits), (192, 128));
    // Zeroed tag and zeroed nonce are not accepted either.
    let mut zero_tag = env.clone();
    zero_tag[tag_start..].fill(0);
    assert_eq!(open(&key, &seg(), &zero_tag), Err(CryptoError::Decrypt));
    let mut zero_nonce = env;
    zero_nonce[1..=NONCE_LEN].fill(0);
    assert_eq!(open(&key, &seg(), &zero_nonce), Err(CryptoError::Decrypt));
}

/// RT-ENV-4: kind/key separation — an envelope sealed with one subkey does not
/// open with another purpose's subkey even at the same address.
#[test]
fn rt_env_wrong_purpose_key_fails() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let seg_key = derive_subkey(&dek, "vault-a", 1, Purpose::Segment);
    let env = seal(&seg_key, &seg(), b"ops").unwrap();
    for purpose in [Purpose::Snapshot, Purpose::Blob, Purpose::DeviceRecord, Purpose::BlobName, Purpose::Manifest, Purpose::KeyCheck] {
        assert_eq!(open(&derive_subkey(&dek, "vault-a", 1, purpose), &seg(), &env), Err(CryptoError::Decrypt));
    }
    assert_eq!(open(&derive_subkey(&dek, "vault-a", 2, Purpose::Segment), &seg(), &env), Err(CryptoError::Decrypt));
}

// ---------------------------------------------------------------- remote keys

/// RT-PATH-1: hostile remote keys (dot segments, empty segments, trailing or
/// leading slashes, backslashes, non-ASCII digits, wrong suffix/shape) never
/// parse to an address.
#[test]
fn rt_path_hostile_remote_keys_are_rejected() {
    let seg_name = "000000000001-000000000003.seg";
    let hostile = [
        String::new(),
        "v1".into(),
        "v1/v".into(),
        "v1/v/manifest.json".into(),
        format!("v1/../log/dev/{seg_name}"),
        format!("v1/./log/dev/{seg_name}"),
        format!("v1/v/log/../{seg_name}"),
        format!("v1/v/log/./{seg_name}"),
        format!("v1/v/log//{seg_name}"),
        format!("v1//log/dev/{seg_name}"),
        format!("/v1/v/log/dev/{seg_name}"),
        format!("v1/v/log/dev/{seg_name}/"),
        format!("v1/v/log/dev//{seg_name}"),
        format!("V1/v/log/dev/{seg_name}"),
        format!("v1\\v\\log\\dev\\{seg_name}"),
        format!("v1/v/log/de\\v/{seg_name}"),
        format!("v1/v\\x/log/dev/{seg_name}"),
        "v1/v/log/dev/000000000001-000000000003.SEG".into(),
        "v1/v/log/dev/000000000001-000000000003".into(),
        "v1/v/log/dev/000000000001_000000000003.seg".into(),
        "v1/v/log/dev/000000000001-000000000003-000000000004.seg".into(),
        "v1/v/log/dev/0000000000001-00000000003.seg".into(),
        "v1/v/log/dev/+00000000001-000000000003.seg".into(),
        "v1/v/log/dev/-00000000001-000000000003.seg".into(),
        "v1/v/log/dev/ 00000000001-000000000003.seg".into(),
        // Fullwidth and Arabic-Indic digits.
        "v1/v/log/dev/０００００００００００１-000000000003.seg".into(),
        "v1/v/log/dev/٠٠٠٠٠٠٠٠٠٠٠١-000000000003.seg".into(),
        "v1/v/log/dev/99999999999999999999-000000000003.seg".into(),
        "v1/v/snap/dev/.snap".into(),
        "v1/v/snap/dev/...snap".into(),
        "v1/v/snap/../s.snap".into(),
        "v1/v/devices/.rec".into(),
        "v1/v/devices/..rec".into(),
        "v1/v/devices/...rec".into(),
        "v1/v/devices/d.rec/".into(),
        "v1/v/devices//d.rec".into(),
        "v1/v/devices/a\\b.rec".into(),
        "v1/v/blob/a/a.blob".into(),
        "v1/v/blob/zz/ab12.blob".into(),
        "v1/v/blob//ab12.blob".into(),
        "v1/v/blob/ab/.blob".into(),
        "v1/v/blob/ab/ab/12.blob".into(),
        "v1/v/unknown/dev/x".into(),
    ];
    for key in &hostile {
        assert_eq!(ObjectAddress::from_remote_key(key, 1), None, "{key:?}");
    }
}

/// RT-PATH-2: every key the parser accepts round-trips byte-exactly, so two
/// remote objects can never map to the same AAD. Ids outside the strict
/// app-generated alphabet (unicode, leading dots, over 64 bytes, non-hex blob
/// ids) are refused in both directions instead of being parsed.
#[test]
fn rt_path_accepted_keys_round_trip_exactly() {
    let long = "d".repeat(65);
    let max = "d".repeat(64);
    let accepted = [
        "v1/vault-a/log/dev-a/000000000001-000000000003.seg".to_owned(),
        "v1/0189f2c4-7b1e-7000-8000-00000000abcd/log/dev_A.1/000000000001-000000000001.seg".to_owned(),
        "v1/v/devices/x.rec.rec".to_owned(),
        "v1/v/snap/d/a.snap.snap".to_owned(),
        "v1/v/blob/ab/ab.blob".to_owned(),
        format!("v1/v/devices/{max}.rec"),
    ];
    for remote in &accepted {
        let addr = ObjectAddress::from_remote_key(remote, 7).unwrap_or_else(|| panic!("{remote}"));
        assert_eq!(addr.key_epoch, 7);
        assert_eq!(addr.remote_key().as_deref(), Some(remote.as_str()));
    }
    let refused = [
        "v1/볼트/log/기기/000000000001-000000000001.seg".to_owned(),
        "v1/v/devices/dév🙂.rec".to_owned(),
        "v1/v/devices/....rec".to_owned(),
        "v1/v/blob/é/éa.blob".to_owned(),
        "v1/v/blob/AB/AB12.blob".to_owned(),
        format!("v1/v/devices/{long}.rec"),
        format!("v1/v/log/{long}/000000000001-000000000003.seg"),
    ];
    for remote in &refused {
        assert_eq!(ObjectAddress::from_remote_key(remote, 1), None, "{remote}");
    }
    // Parsed sequence ranges that check_segment must still refuse.
    for (remote, first, last) in [
        ("v1/v/log/d/000000000003-000000000001.seg", 3, 1),
        ("v1/v/log/d/000000000000-000000000000.seg", 0, 0),
    ] {
        let addr = ObjectAddress::from_remote_key(remote, 1).unwrap();
        let ops: Vec<u64> = (first..=last).collect();
        let header = SegmentHeader { device_id: "d", first_seq: first, last_seq: last, op_seqs: &ops };
        assert_eq!(check_segment(&header, &addr), Err(CryptoError::Mismatch("sequence range")), "{remote}");
    }
}

// ---------------------------------------------------------------- check_segment

fn h<'a>(device_id: &'a str, first_seq: u64, last_seq: u64, op_seqs: &'a [u64]) -> SegmentHeader<'a> {
    SegmentHeader { device_id, first_seq, last_seq, op_seqs }
}

/// RT-SEG-1: gaps, duplicates, reordering, empty op lists, u64::MAX ranges,
/// NFC/NFD device ids, and wrong kinds are all quarantined; no panics.
#[test]
fn rt_seg_cross_check_adversarial_headers() {
    let addr = ObjectAddress::segment("v", 1, "dev-a", 1, 3);
    let op = Err(CryptoError::Mismatch("op sequence"));
    let range = Err(CryptoError::Mismatch("sequence range"));
    assert_eq!(check_segment(&h("dev-a", 1, 3, &[1, 2, 3]), &addr), Ok(()));
    let bad: [&[u64]; 9] = [&[], &[1, 2, 4], &[1, 1, 2], &[1, 2, 2], &[1, 1, 2, 3], &[2, 3], &[0, 1, 2], &[1, 2, 3, 3], &[3, 2, 1]];
    for ops in bad {
        assert_eq!(check_segment(&h("dev-a", 1, 3, ops), &addr), op, "{ops:?}");
    }
    assert_eq!(check_segment(&h("DEV-A", 1, 3, &[1, 2, 3]), &addr), Err(CryptoError::Mismatch("device_id")));
    assert_eq!(check_segment(&h("", 1, 3, &[1, 2, 3]), &addr), Err(CryptoError::Mismatch("device_id")));
    let nfc = ObjectAddress::segment("v", 1, "caf\u{e9}", 1, 1);
    assert_eq!(check_segment(&h("cafe\u{301}", 1, 1, &[1]), &nfc), Err(CryptoError::Mismatch("device_id")));
    assert_eq!(check_segment(&h("dev-a", u64::MAX, u64::MAX, &[u64::MAX]), &addr), range);
    assert_eq!(check_segment(&h("dev-a", 3, 1, &[]), &addr), range);

    let huge = ObjectAddress::segment("v", 1, "dev-a", 1, u64::MAX);
    assert_eq!(check_segment(&h("dev-a", 1, u64::MAX, &[1]), &huge), op);
    assert_eq!(check_segment(&h("dev-a", 1, u64::MAX, &[]), &huge), op);
    let top = ObjectAddress::segment("v", 1, "dev-a", u64::MAX - 1, u64::MAX);
    assert_eq!(check_segment(&h("dev-a", u64::MAX - 1, u64::MAX, &[u64::MAX - 1, u64::MAX]), &top), Ok(()));
    assert_eq!(check_segment(&h("dev-a", u64::MAX - 1, u64::MAX, &[u64::MAX, u64::MAX]), &top), op);
    let zero = ObjectAddress::segment("v", 1, "dev-a", 0, u64::MAX);
    assert_eq!(check_segment(&h("dev-a", 0, u64::MAX, &[0]), &zero), range);

    for other in [
        ObjectAddress::snapshot("v", 1, "dev-a", "000000000001-000000000003"),
        ObjectAddress::device_record("v", 1, "dev-a"),
        ObjectAddress::blob("v", 1, "000000000001-000000000003"),
    ] {
        assert_eq!(
            check_segment(&h("dev-a", 1, 3, &[1, 2, 3]), &other),
            Err(CryptoError::Mismatch("object kind"))
        );
    }
}

// ---------------------------------------------------------------- recovery key

/// RT-RK-1: the Python-encoded RK parses and re-displays identically.
#[test]
fn rt_rk_python_vector_round_trips() {
    assert_eq!(rk_text(rk_bytes()), PY_RK_TEXT);
    let rk = RecoveryKey::parse(PY_RK_TEXT).unwrap();
    assert_eq!(rk.to_display().as_str(), PY_RK_TEXT);
}

/// RT-RK-2: hostile RK text — unicode digits/letters, unsupported separators,
/// 55/57 chars, valid Base32 with a wrong checksum, transposed groups — is
/// refused with the right class of error; benign separators are accepted.
#[test]
fn rt_rk_adversarial_parse() {
    let good = PY_RK_TEXT;
    let plain = good.replace('-', "");
    let expected = RecoveryKey::parse(good).unwrap();

    for benign in [
        plain.to_lowercase(),
        good.replace('-', " "),
        good.replace('-', "\t"),
        good.replace('-', "\n"),
        good.replace('-', " - "),
        format!("  {good}\r\n"),
        good.replace('-', "\u{3000}"),
        good.replace('-', "--"),
    ] {
        assert_eq!(RecoveryKey::parse(&benign).as_ref(), Ok(&expected), "{benign:?}");
    }

    for format_error in [
        plain[..55].to_owned(),
        format!("{plain}A"),
        good.replace('-', "_"),
        good.replace('-', "."),
        good.replace('-', "\u{2013}"),
        good.replace('-', "\u{200b}"),
        plain.replacen('A', "\u{ff21}", 1),
        plain.replacen('4', "\u{ff14}", 1),
        plain.replacen('4', "\u{664}", 1),
        plain.replacen("AA", "\u{e9}", 1),
        plain.replacen("AAA", "\u{ff21}", 1),
        plain.replacen('A', "9", 1),
        plain.replacen('A', "=", 1),
        format!("{}====", &plain[..52]),
        "A".repeat(56).replacen('A', "é", 1),
    ] {
        assert_eq!(RecoveryKey::parse(&format_error), Err(RecoveryError::Format), "{format_error:?}");
    }

    let mut last_flipped: Vec<char> = plain.chars().collect();
    let tail = last_flipped.len() - 1;
    last_flipped[tail] = if last_flipped[tail] == 'A' { 'B' } else { 'A' };
    let groups: Vec<&str> = good.split('-').collect();
    let mut swapped = groups.clone();
    swapped.swap(0, 1);
    let mut swapped_tail = groups.clone();
    swapped_tail.swap(12, 13);
    for checksum_error in [
        "A".repeat(56),
        last_flipped.into_iter().collect::<String>(),
        swapped.join("-"),
        swapped_tail.join("-"),
        format!("{}{}", &plain[1..], &plain[..1]),
    ] {
        assert_eq!(RecoveryKey::parse(&checksum_error), Err(RecoveryError::Checksum), "{checksum_error:?}");
    }
}

/// RT-RK-3 (AC 13): a valid RK for another vault, a mistyped RK, and garbage
/// all fail; the manifest is untouched and no DEK is returned.
#[test]
fn rt_rk_wrong_vault_key_fails_without_side_effects() {
    let dek_a = SecretKey::from_bytes(dek_bytes());
    let dek_b = SecretKey::from_bytes([0x99; 32]);
    let rk_b = [0xA5; 32];
    let manifest_a = genuine_manifest(rk_bytes(), &dek_a, VAULT, 1);
    let manifest_b = genuine_manifest(rk_b, &dek_b, OTHER_VAULT, 1);
    let before = manifest_a.clone();
    assert_eq!(unlock(&manifest_a, VAULT, &rk_text(rk_b), BINDING), Err(RecoveryError::WrongKey));
    assert_eq!(unlock(&manifest_b, OTHER_VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::WrongKey));
    assert_eq!(unlock(&manifest_a, VAULT, &PY_RK_TEXT.replacen('A', "B", 1), BINDING), Err(RecoveryError::Checksum));
    assert_eq!(unlock(&manifest_a, VAULT, "", BINDING), Err(RecoveryError::Format));
    assert_eq!(manifest_a, before);
    assert_eq!(unlock(&manifest_b, OTHER_VAULT, &rk_text(rk_b), BINDING).unwrap(), dek_b);
}

// ---------------------------------------------------------------- manifest

/// RT-MAN-1 (AC 23): every manifest field, tampered individually, makes both
/// `unlock` and `verify_manifest` fail; no DEK is ever returned.
#[test]
fn rt_man_every_field_tampered_fails_closed() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let manifest = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    assert_eq!(unlock(&manifest, VAULT, PY_RK_TEXT, BINDING).unwrap(), dek);

    let wrapped = BASE64.decode(manifest.wrapped_dek.as_bytes()).unwrap();
    let mut flipped = wrapped;
    flipped[5] ^= 0x80;
    let flipped = BASE64.encode(&flipped);
    let zero_mac = BASE64.encode(&[0u8; 32]);
    let real_mac = BASE64.decode(manifest.manifest_mac.as_bytes()).unwrap();
    type Edit = Box<dyn Fn(&mut Manifest)>;
    let unsupported = Err(RecoveryError::Unsupported);
    let wrong = Err(RecoveryError::WrongKey);
    let tampered = Err(RecoveryError::Tampered);
    let mismatch = Err(RecoveryError::VaultMismatch);
    let cases: Vec<(&str, Edit, Result<SecretKey, RecoveryError>)> = vec![
        ("format=0", Box::new(|m| m.format = 0), unsupported.clone()),
        ("format=2", Box::new(|m| m.format = 2), unsupported.clone()),
        ("format=max", Box::new(|m| m.format = u32::MAX), unsupported.clone()),
        ("suite", Box::new(|m| m.suite = "xchacha20poly1305-hkdf-sha512".into()), unsupported.clone()),
        ("suite case", Box::new(|m| m.suite = m.suite.to_uppercase()), unsupported.clone()),
        ("rk_kind", Box::new(|m| m.rk_kind = "passphrase".into()), unsupported.clone()),
        ("vault_id", Box::new(|m| m.vault_id = OTHER_VAULT.into()), mismatch.clone()),
        ("vault_id empty", Box::new(|m| m.vault_id = String::new()), mismatch.clone()),
        ("key_epoch=0", Box::new(|m| m.key_epoch = 0), unsupported.clone()),
        ("key_epoch=2", Box::new(|m| m.key_epoch = 2), unsupported.clone()),
        ("key_epoch=max", Box::new(|m| m.key_epoch = u32::MAX), unsupported.clone()),
        ("created_at", Box::new(|m| m.created_at = "2030-01-01T00:00:00Z".into()), tampered.clone()),
        ("created_at empty", Box::new(|m| m.created_at = String::new()), tampered.clone()),
        ("wrapped_dek nonce", Box::new(move |m| m.wrapped_dek = flipped.clone()), wrong.clone()),
        ("key_check zero", Box::new(|m| m.key_check = BASE64.encode(&[0u8; 32])), tampered.clone()),
        ("key_check empty", Box::new(|m| m.key_check = String::new()), tampered.clone()),
        ("key_check bad b64", Box::new(|m| m.key_check = "!!".into()), tampered.clone()),
        ("binding_hint", Box::new(|m| m.binding_hint = BASE64.encode(&[0u8; 32])), tampered.clone()),
        ("manifest_mac zero", Box::new(move |m| m.manifest_mac = zero_mac.clone()), tampered.clone()),
        ("manifest_mac empty", Box::new(|m| m.manifest_mac = String::new()), tampered.clone()),
        ("manifest_mac bad b64", Box::new(|m| m.manifest_mac = "%%%".into()), tampered.clone()),
        (
            "manifest_mac truncated",
            Box::new(move |m| m.manifest_mac = BASE64.encode(&real_mac[..31])),
            tampered.clone(),
        ),
        ("wrapped_dek bad b64", Box::new(|m| m.wrapped_dek = "%%%".into()), tampered.clone()),
        (
            "wrapped_dek truncated",
            Box::new(|m| m.wrapped_dek = BASE64.encode(&[1u8; 40])),
            Err(RecoveryError::Crypto(CryptoError::Truncated)),
        ),
        (
            "wrapped_dek version",
            Box::new(|m| {
                let mut w = BASE64.decode(m.wrapped_dek.as_bytes()).unwrap();
                w[0] = 2;
                m.wrapped_dek = BASE64.encode(&w);
            }),
            Err(RecoveryError::Crypto(CryptoError::UnsupportedVersion(2))),
        ),
    ];
    for (name, edit, expected) in cases {
        let mut m = manifest.clone();
        edit(&mut m);
        assert_eq!(unlock(&m, VAULT, PY_RK_TEXT, BINDING), expected, "unlock {name}");
        assert!(verify_manifest(&m, VAULT, &dek, BINDING).is_err(), "verify {name}");
    }
}

/// RT-MAN-2: a correctly wrapped DEK from another vault — even one sealed
/// under the same RK — does not unwrap in this vault's manifest, alone or
/// together with the other vault's key_check and MAC.
#[test]
fn rt_man_swapped_wrapped_dek_from_another_vault_fails() {
    let dek_a = SecretKey::from_bytes(dek_bytes());
    let dek_b = SecretKey::from_bytes([0x99; 32]);
    let a = genuine_manifest(rk_bytes(), &dek_a, VAULT, 1);
    let b_same_rk = genuine_manifest(rk_bytes(), &dek_b, OTHER_VAULT, 1);
    let mut swapped = a.clone();
    swapped.wrapped_dek = b_same_rk.wrapped_dek.clone();
    assert_eq!(unlock(&swapped, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::WrongKey));
    swapped.key_check = b_same_rk.key_check.clone();
    swapped.binding_hint = b_same_rk.binding_hint.clone();
    swapped.manifest_mac = b_same_rk.manifest_mac.clone();
    assert_eq!(unlock(&swapped, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::WrongKey));
    // Same vault, other epoch: the AAD epoch binds the wrap.
    let a2 = genuine_manifest(rk_bytes(), &dek_a, VAULT, 2);
    let mut epoch_swap = a.clone();
    epoch_swap.wrapped_dek = a2.wrapped_dek;
    assert_eq!(unlock(&epoch_swap, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::WrongKey));
    // A rewrapped DEK under a new RK (RK reissue) does not open with the old RK.
    let reissued = genuine_manifest([0x5A; 32], &dek_a, VAULT, 1);
    assert_eq!(unlock(&reissued, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::WrongKey));
    assert_eq!(unlock(&reissued, VAULT, &rk_text([0x5A; 32]), BINDING).unwrap(), dek_a);
}

/// RT-MAN-3: unknown, duplicate, missing, mistyped, and Q2 passphrase fields
/// (`wrapped_dek_pp`, `kdf`) are rejected at parse time; key order on the wire
/// does not matter because the MAC covers the canonical body.
#[test]
fn rt_man_json_shape_is_strict() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let manifest = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    let value = serde_json::to_value(&manifest).unwrap();
    for (field, injected) in [
        ("extra", serde_json::json!("x")),
        ("wrapped_dek_pp", serde_json::json!("AAAA")),
        ("kdf", serde_json::json!({"alg": "argon2id"})),
        ("MANIFEST_MAC", serde_json::json!("x")),
    ] {
        let mut v = value.clone();
        v[field] = injected;
        assert!(serde_json::from_value::<Manifest>(v).is_err(), "{field}");
    }
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("key_check");
    assert!(serde_json::from_value::<Manifest>(missing).is_err());
    let mut mistyped = value.clone();
    mistyped["key_epoch"] = serde_json::json!("1");
    assert!(serde_json::from_value::<Manifest>(mistyped).is_err());
    let mut negative = value.clone();
    negative["format"] = serde_json::json!(-1);
    assert!(serde_json::from_value::<Manifest>(negative).is_err());

    let text = serde_json::to_string(&manifest).unwrap();
    let duplicate = text.replacen('{', r#"{"vault_id":"vault-test-0002","#, 1);
    assert!(serde_json::from_str::<Manifest>(&duplicate).is_err());

    // Reordered keys with whitespace: same semantic content, still verifies.
    let mut fields: Vec<(String, serde_json::Value)> = value.as_object().unwrap().clone().into_iter().collect();
    fields.reverse();
    let reordered = format!(
        "{{ {} }}",
        fields.iter().map(|(k, v)| format!("{k:?} : {v}")).collect::<Vec<_>>().join(" ,\n ")
    );
    let parsed: Manifest = serde_json::from_str(&reordered).unwrap();
    assert_eq!(verify_manifest(&parsed, VAULT, &dek, BINDING), Ok(()));
}

/// RT-MAN-4: the MAC body is exactly the plan's field list; the binding hint is
/// HMAC(K_man, binding_fp) and any other account (including empty) is refused.
#[test]
fn rt_man_binding_and_mac_scope() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let manifest = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    assert_eq!(unlock(&manifest, VAULT, PY_RK_TEXT, BINDING).unwrap(), dek);
    for other in ["", "gdrive:sub-1234", "gdrive:sub-12", "GDRIVE:sub-123", "onedrive:sub-123"] {
        assert_eq!(unlock(&manifest, VAULT, PY_RK_TEXT, other), Err(RecoveryError::BindingMismatch), "{other}");
        assert_eq!(verify_manifest(&manifest, VAULT, &dek, other), Err(RecoveryError::BindingMismatch), "{other}");
    }
    // Production-built vaults use the same MAC body as the contract encoding.
    let vault = super::recovery::create_vault(VAULT, CREATED, BINDING).unwrap();
    let k_man = derive_subkey(&vault.dek, VAULT, 1, Purpose::Manifest);
    assert_eq!(
        BASE64.decode(vault.manifest.manifest_mac.as_bytes()).unwrap(),
        hmac_sha256(&k_man, &canonical_body(&vault.manifest)).to_vec()
    );
    assert_eq!(
        BASE64.decode(vault.manifest.binding_hint.as_bytes()).unwrap(),
        hmac_sha256(&k_man, BINDING.as_bytes()).to_vec()
    );
    assert_eq!(vault.manifest.key_epoch, 1);
}

/// RT-MAN-5 (was blocker G1-RT-B1, fixed): §4.6.2 fixes the v1 manifest at
/// `key_epoch:1` and §4.5 holds a future key_epoch with an update notice, so a
/// genuine (correctly MACed and wrapped) manifest at another epoch is refused
/// as Unsupported before any key is derived.
#[test]
fn rt_man_future_key_epoch_is_held() {
    let dek = SecretKey::from_bytes(dek_bytes());
    for epoch in [0, 2, u32::MAX] {
        let manifest = genuine_manifest(rk_bytes(), &dek, VAULT, epoch);
        assert_eq!(unlock(&manifest, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::Unsupported), "epoch {epoch}");
        assert_eq!(verify_manifest(&manifest, VAULT, &dek, BINDING), Err(RecoveryError::Unsupported), "epoch {epoch}");
    }
}

// ================================================================ pass 2
// Written after the G1-RT-B1 fix: strict ids, expected-vault binding,
// key_epoch == 1, and join-path Tampered classification.

// ---------------------------------------------------------------- ids

/// Every printable ASCII punctuation mark except the three the id alphabet
/// allows (`-`, `_`, `.`).
const FORBIDDEN_PUNCT: &str = "!\"#$%&'()*+,/:;<=>?@[\\]^`{|}~";

fn id_alnum() -> Vec<char> {
    ('0'..='9').chain('a'..='z').chain('A'..='Z').collect()
}

/// RT-ID-1 (§4.2 path components): the id alphabet is exactly ASCII
/// alphanumerics plus inner `-` `_` `.`, at most 64 bytes. Every one of the
/// 128 ASCII bytes is tried alone, leading, trailing, and inner; forbidden
/// ones are refused by `is_id`, by `remote_key()`, and by `from_remote_key`.
#[test]
fn rt_id_alphabet_and_length_boundaries() {
    let alnum = id_alnum();
    let inner_only = ['-', '_', '.'];
    let mut forbidden: Vec<char> = FORBIDDEN_PUNCT.chars().collect();
    forbidden.push(' ');
    forbidden.extend((0u8..0x20).chain([0x7f]).map(char::from));
    // The three classes partition ASCII, so no byte is left untested.
    let mut covered: Vec<u32> = alnum.iter().chain(&inner_only).chain(&forbidden).map(|c| *c as u32).collect();
    covered.sort_unstable();
    covered.dedup();
    assert_eq!(covered, (0..128).collect::<Vec<u32>>());
    assert_eq!(alnum.len() + inner_only.len() + forbidden.len(), 128);

    for c in &alnum {
        for id in [c.to_string(), format!("{c}x"), format!("x{c}"), format!("x{c}x")] {
            assert!(is_id(&id), "{id:?}");
            let key = format!("v1/{id}/devices/{id}.rec");
            assert_eq!(ObjectAddress::device_record(&id, 1, &id).remote_key().as_deref(), Some(key.as_str()));
            assert_eq!(ObjectAddress::from_remote_key(&key, 1), Some(ObjectAddress::device_record(&id, 1, &id)));
        }
    }
    for c in inner_only {
        assert!(is_id(&format!("x{c}")), "trailing {c:?}");
        assert!(is_id(&format!("x{c}{c}x")), "inner {c:?}");
        for id in [c.to_string(), format!("{c}x"), format!("{c}{c}"), format!("{c}{c}{c}"), format!("{c}0")] {
            assert!(!is_id(&id), "leading {id:?}");
            assert_eq!(ObjectAddress::device_record("v", 1, &id).remote_key(), None, "{id:?}");
            assert_eq!(ObjectAddress::snapshot("v", 1, "d", &id).remote_key(), None, "{id:?}");
            assert_eq!(ObjectAddress::blob(&id, 1, "ab").remote_key(), None, "{id:?}");
            assert_eq!(ObjectAddress::from_remote_key(&format!("v1/v/devices/{id}.rec"), 1), None, "{id:?}");
            assert_eq!(ObjectAddress::from_remote_key(&format!("v1/v/snap/d/{id}.snap"), 1), None, "{id:?}");
            assert_eq!(ObjectAddress::from_remote_key(&format!("v1/{id}/devices/d.rec"), 1), None, "{id:?}");
        }
    }
    for name in ["-rf", ".hidden", ".git", "_x", "-", "_", ".", "..", "..."] {
        assert!(!is_id(name), "{name:?}");
    }
    for c in &forbidden {
        for id in [c.to_string(), format!("{c}x"), format!("x{c}"), format!("x{c}x")] {
            assert!(!is_id(&id), "{id:?}");
            assert_eq!(ObjectAddress::device_record("v", 1, &id).remote_key(), None, "{id:?}");
            assert_eq!(ObjectAddress::segment("v", 1, &id, 1, 1).remote_key(), None, "{id:?}");
            assert_eq!(ObjectAddress::snapshot("v", 1, "d", &id).remote_key(), None, "{id:?}");
            assert_eq!(ObjectAddress::blob(&id, 1, "ab").remote_key(), None, "{id:?}");
            assert_eq!(ObjectAddress::from_remote_key(&format!("v1/v/devices/{id}.rec"), 1), None, "{id:?}");
            assert_eq!(ObjectAddress::from_remote_key(&format!("v1/v/snap/d/{id}.snap"), 1), None, "{id:?}");
            assert_eq!(ObjectAddress::from_remote_key(&format!("v1/{id}/devices/d.rec"), 1), None, "{id:?}");
        }
    }

    // 64 bytes accepted, 65 refused, whatever the leading class and filler.
    let fill: Vec<char> = alnum.iter().copied().chain(inner_only).collect();
    for lead in ['a', 'Z', '0', '9'] {
        for len in [1usize, 2, 63, 64, 65, 66, 128, 255, 256] {
            let id: String = std::iter::once(lead).chain(fill.iter().copied().cycle().take(len - 1)).collect();
            assert_eq!(id.len(), len);
            assert_eq!(is_id(&id), len <= 64, "{lead} len {len}");
            assert_eq!(ObjectAddress::device_record(&id, 1, &id).remote_key().is_some(), len <= 64, "len {len}");
        }
    }
    let max_mixed = format!("a{}zzz", "-_.9".repeat(15));
    assert_eq!(max_mixed.len(), 64);
    assert!(is_id(&max_mixed));
    assert!(!is_id(&format!("{max_mixed}a")));

    // Non-ASCII never passes, including 64-byte strings of two-byte chars.
    for id in [
        "é", "aé", "a\u{80}", "a\u{a0}b", "a\u{200b}", "\u{feff}a", "ａ", "aı", "a\u{301}", "a🙂", "a\u{2010}b",
        "a\u{2215}b", "a\u{ff0f}b", "a\u{ff0e}\u{ff0e}",
    ] {
        assert!(!is_id(id), "{id:?}");
    }
    assert!(!is_id(&"é".repeat(32)));
    assert!(!is_id(""));
}

/// RT-ID-2 (§4.2 `blob/<hh>/<blob_id>`, wrapped DEK path-less): blob ids are
/// lowercase hex of 2..=64 chars; every kind refuses an invalid vault id; the
/// segment stem must fit 12 digits; record/snapshot/segment refs are strict.
#[test]
fn rt_id_blob_ids_and_remote_key_validation() {
    let hex64 = "0123456789abcdef".repeat(4);
    let hex65 = format!("{hex64}0");
    for (id, ok) in [
        ("ab", true),
        ("00", true),
        ("ff", true),
        (hex64.as_str(), true),
        ("", false),
        ("a", false),
        (hex65.as_str(), false),
        ("AB", false),
        ("aB", false),
        ("Ab12", false),
        ("g0", false),
        ("0g", false),
        ("ab-c", false),
        ("ab.cd", false),
        ("ab_cd", false),
        ("ab cd", false),
        ("ab/cd", false),
        ("ab\0", false),
        ("abé", false),
        ("０a", false),
        ("..", false),
    ] {
        let addr = ObjectAddress::blob("v", 1, id);
        let expected = ok.then(|| format!("v1/v/blob/{}/{id}.blob", &id[..2]));
        assert_eq!(addr.remote_key(), expected, "{id:?}");
        if let Some(key) = expected {
            assert_eq!(ObjectAddress::from_remote_key(&key, 1), Some(addr), "{id:?}");
        }
    }
    let mut owned_blob = ObjectAddress::blob("v", 1, "ab");
    owned_blob.device_id = "d".into();
    assert_eq!(owned_blob.remote_key(), None);

    let long = "v".repeat(65);
    for vault in ["", ".", "..", "-v", "v/w", "v\\w", "v w", "볼트", long.as_str()] {
        for addr in [
            ObjectAddress::segment(vault, 1, "d", 1, 1),
            ObjectAddress::snapshot(vault, 1, "d", "s"),
            ObjectAddress::blob(vault, 1, "ab"),
            ObjectAddress::device_record(vault, 1, "d"),
            ObjectAddress::wrapped_dek(vault, 1),
        ] {
            assert_eq!(addr.remote_key(), None, "{vault:?} {:?}", addr.kind);
        }
    }
    for vault in ["v", VAULT] {
        assert_eq!(ObjectAddress::wrapped_dek(vault, 1).remote_key(), None);
    }

    assert_eq!(
        ObjectAddress::segment("v", 1, "d", 999_999_999_999, 999_999_999_999).remote_key().as_deref(),
        Some("v1/v/log/d/999999999999-999999999999.seg")
    );
    for (first, last) in [(1_000_000_000_000u64, 1_000_000_000_000u64), (1, 1_000_000_000_000), (u64::MAX, u64::MAX)] {
        assert_eq!(ObjectAddress::segment("v", 1, "d", first, last).remote_key(), None, "{first}-{last}");
    }
    for object_ref in [
        "",
        "1-2",
        "000000000001-00000000000a",
        "000000000001-000000000002/..",
        "../000000000001-000000000002",
        "000000000001-000000000002.seg",
    ] {
        let mut addr = ObjectAddress::segment("v", 1, "d", 1, 2);
        addr.object_ref = object_ref.into();
        assert_eq!(addr.remote_key(), None, "{object_ref:?}");
    }
    for object_ref in ["", "REC", "rec ", "dek", "x", "rec/.."] {
        let mut addr = ObjectAddress::device_record("v", 1, "d");
        addr.object_ref = object_ref.into();
        assert_eq!(addr.remote_key(), None, "{object_ref:?}");
    }
    let long_id = "s".repeat(65);
    for bad in ["", ".", "..", "-s", "s/t", "s\\t", "s\0", "s t", long_id.as_str()] {
        assert_eq!(ObjectAddress::snapshot("v", 1, "d", bad).remote_key(), None, "snap {bad:?}");
        assert_eq!(ObjectAddress::snapshot("v", 1, bad, "s").remote_key(), None, "snap dev {bad:?}");
        assert_eq!(ObjectAddress::segment("v", 1, bad, 1, 1).remote_key(), None, "seg dev {bad:?}");
        assert_eq!(ObjectAddress::device_record("v", 1, bad).remote_key(), None, "rec dev {bad:?}");
    }
}

/// Deterministic xorshift64 so failures replay exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ID_LEAD: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ID_REST: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-_.";

fn gen_id(rng: &mut Rng) -> String {
    let len = 1 + rng.below(64);
    let mut id = String::with_capacity(len);
    id.push(char::from(ID_LEAD[rng.below(ID_LEAD.len())]));
    for _ in 1..len {
        id.push(char::from(ID_REST[rng.below(ID_REST.len())]));
    }
    id
}

fn gen_blob_id(rng: &mut Rng) -> String {
    let len = 2 + rng.below(63);
    (0..len).map(|_| char::from(b"0123456789abcdef"[rng.below(16)])).collect()
}

fn gen_seq(rng: &mut Rng) -> u64 {
    match rng.below(4) {
        0 => 0,
        1 => 999_999_999_999,
        _ => rng.next() % 1_000_000_000_000,
    }
}

const HOSTILE_TOKENS: &[&str] = &[
    "", ".", "..", "/", "\\", "-", "_", "a", "Z", "0", "ab", "é", "\0", " ", "%2e", "%2F", "rec", ".rec", ".seg",
    ".snap", ".blob", "000000000001", "000000000001-000000000002", "v1", "log", "\n",
];

fn hostile(rng: &mut Rng) -> String {
    let n = rng.below(4);
    (0..n).map(|_| HOSTILE_TOKENS[rng.below(HOSTILE_TOKENS.len())]).collect()
}

/// RT-ID-3 (§4.2, §4.6.1 AAD from path): `remote_key` and `from_remote_key`
/// are exact inverses over 8,000 generated valid addresses of every kind, the
/// mapping is injective, and 40,000 hostile addresses/keys never panic and
/// never parse into anything that does not re-serialize to the same key.
#[test]
fn rt_id_remote_key_round_trip_fuzz() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut by_key: std::collections::HashMap<String, ObjectAddress> = std::collections::HashMap::new();
    for i in 0..8000 {
        let vault = gen_id(&mut rng);
        let epoch = rng.next() as u32;
        let addr = match i % 4 {
            0 => {
                let device = gen_id(&mut rng);
                let (first, last) = (gen_seq(&mut rng), gen_seq(&mut rng));
                ObjectAddress::segment(&vault, epoch, &device, first, last)
            }
            1 => {
                let device = gen_id(&mut rng);
                let snap = gen_id(&mut rng);
                ObjectAddress::snapshot(&vault, epoch, &device, &snap)
            }
            2 => {
                let blob = gen_blob_id(&mut rng);
                ObjectAddress::blob(&vault, epoch, &blob)
            }
            _ => {
                let device = gen_id(&mut rng);
                ObjectAddress::device_record(&vault, epoch, &device)
            }
        };
        let key = addr.remote_key().unwrap_or_else(|| panic!("valid address refused: {addr:?}"));
        assert!(key.starts_with(&format!("v1/{vault}/")), "{key}");
        assert_eq!(ObjectAddress::from_remote_key(&key, epoch).as_ref(), Some(&addr), "{key}");
        let mut epoch_free = addr.clone();
        epoch_free.key_epoch = 0;
        if let Some(previous) = by_key.insert(key.clone(), epoch_free.clone()) {
            assert_eq!(previous, epoch_free, "two addresses share {key}");
        }
    }

    let kinds = [ObjectKind::Segment, ObjectKind::Snapshot, ObjectKind::Blob, ObjectKind::DeviceRecord, ObjectKind::WrappedDek];
    let dirs = ["log", "snap", "blob", "devices", "x", ""];
    let suffixes = ["", ".seg", ".snap", ".blob", ".rec"];
    let (mut emitted, mut parsed) = (0, 0);
    for _ in 0..40_000 {
        let addr = ObjectAddress {
            kind: kinds[rng.below(kinds.len())],
            vault_id: hostile(&mut rng),
            key_epoch: 3,
            device_id: hostile(&mut rng),
            object_ref: hostile(&mut rng),
        };
        if let Some(key) = addr.remote_key() {
            emitted += 1;
            assert_eq!(ObjectAddress::from_remote_key(&key, 3).as_ref(), Some(&addr), "{key:?}");
        }
        let mut key = format!("v1/{}/{}", hostile(&mut rng), dirs[rng.below(dirs.len())]);
        for _ in 0..1 + rng.below(3) {
            key.push('/');
            key.push_str(&hostile(&mut rng));
        }
        key.push_str(suffixes[rng.below(suffixes.len())]);
        if let Some(addr) = ObjectAddress::from_remote_key(&key, 3) {
            parsed += 1;
            assert_eq!(addr.remote_key().as_deref(), Some(key.as_str()), "{key:?}");
        }
    }
    // The hostile generator must actually reach the accepting branches.
    assert!(emitted > 0 && parsed > 0, "emitted {emitted} parsed {parsed}");
}

// ---------------------------------------------------------------- precedence

const OTHER_BINDING: &str = "onedrive:sub-123";

fn resign(m: &mut Manifest, k_man: &SecretKey) {
    m.manifest_mac = BASE64.encode(&hmac_sha256(k_man, &canonical_body(m)));
}

/// RT-PREC-1 (AC 13/23 error classes): with up to five faults combined
/// (unsupported format/epoch × vault location × RK Format/Checksum/WrongKey ×
/// body/MAC tamper × wrong binding), `unlock` reports exactly the
/// highest-precedence fault: Unsupported > VaultMismatch > Format/Checksum >
/// WrongKey > Tampered > BindingMismatch; only the all-clear cell opens.
#[test]
fn rt_prec_unlock_combined_faults() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let genuine = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    let checksum_text = PY_RK_TEXT.replacen('A', "B", 1);
    let wrong_text = rk_text([0xA5; 32]);
    let mut cells = 0;
    for unsupported in 0..3 {
        for vault in 0..3 {
            for rk in 0..4 {
                for tamper in 0..3 {
                    for binding_bad in [false, true] {
                        let mut m = genuine.clone();
                        match unsupported {
                            1 => m.key_epoch = 2,
                            2 => m.format = 2,
                            _ => {}
                        }
                        let expected_vault = match vault {
                            1 => OTHER_VAULT,
                            2 => {
                                m.vault_id = "..".into();
                                ".."
                            }
                            _ => VAULT,
                        };
                        match tamper {
                            1 => m.created_at = "2030-01-01T00:00:00Z".into(),
                            2 => m.manifest_mac = BASE64.encode(&[0u8; 32]),
                            _ => {}
                        }
                        let text = match rk {
                            1 => "not-a-recovery-key",
                            2 => checksum_text.as_str(),
                            3 => wrong_text.as_str(),
                            _ => PY_RK_TEXT,
                        };
                        let binding = if binding_bad { OTHER_BINDING } else { BINDING };
                        let expected = if unsupported != 0 {
                            Err(RecoveryError::Unsupported)
                        } else if vault != 0 {
                            Err(RecoveryError::VaultMismatch)
                        } else if rk == 1 {
                            Err(RecoveryError::Format)
                        } else if rk == 2 {
                            Err(RecoveryError::Checksum)
                        } else if rk == 3 {
                            Err(RecoveryError::WrongKey)
                        } else if tamper != 0 {
                            Err(RecoveryError::Tampered)
                        } else if binding_bad {
                            Err(RecoveryError::BindingMismatch)
                        } else {
                            Ok(dek.clone())
                        };
                        assert_eq!(
                            unlock(&m, expected_vault, text, binding),
                            expected,
                            "unsupported{unsupported} vault{vault} rk{rk} tamper{tamper} binding_bad={binding_bad}"
                        );
                        cells += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cells, 216);

    // RK text is judged before the wrapped DEK is even decoded.
    let mut garbage_wrap = genuine.clone();
    garbage_wrap.wrapped_dek = "%%%".into();
    assert_eq!(unlock(&garbage_wrap, VAULT, "", BINDING), Err(RecoveryError::Format));
    assert_eq!(unlock(&garbage_wrap, VAULT, &checksum_text, BINDING), Err(RecoveryError::Checksum));
    assert_eq!(unlock(&garbage_wrap, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::Tampered));

    // A genuine hint for another account is caught by the MAC first; only a
    // DEK holder who re-signs it reaches the binding check.
    let k_man = derive_subkey(&dek, VAULT, 1, Purpose::Manifest);
    let mut hint_swap = genuine.clone();
    hint_swap.binding_hint = BASE64.encode(&hmac_sha256(&k_man, OTHER_BINDING.as_bytes()));
    assert_eq!(unlock(&hint_swap, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::Tampered));
    assert_eq!(unlock(&hint_swap, VAULT, PY_RK_TEXT, OTHER_BINDING), Err(RecoveryError::Tampered));
    resign(&mut hint_swap, &k_man);
    assert_eq!(unlock(&hint_swap, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::BindingMismatch));
    assert_eq!(unlock(&hint_swap, VAULT, PY_RK_TEXT, OTHER_BINDING).unwrap(), dek);
}

/// RT-PREC-2 (§4.6.2 verify with keychain DEK): `verify_manifest` reports
/// Unsupported > VaultMismatch > WrongKey (other DEK) > Tampered >
/// BindingMismatch across all 108 combined-fault cells.
#[test]
fn rt_prec_verify_combined_faults() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let other_dek = SecretKey::from_bytes([0x99; 32]);
    let genuine = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    let mut cells = 0;
    for unsupported in 0..3 {
        for vault in 0..3 {
            for wrong_dek in [false, true] {
                for tamper in 0..3 {
                    for binding_bad in [false, true] {
                        let mut m = genuine.clone();
                        match unsupported {
                            1 => m.key_epoch = u32::MAX,
                            2 => m.rk_kind = "passphrase".into(),
                            _ => {}
                        }
                        let expected_vault = match vault {
                            1 => OTHER_VAULT,
                            2 => {
                                m.vault_id = String::new();
                                ""
                            }
                            _ => VAULT,
                        };
                        match tamper {
                            1 => m.created_at = String::new(),
                            2 => m.manifest_mac = String::new(),
                            _ => {}
                        }
                        let key = if wrong_dek { &other_dek } else { &dek };
                        let binding = if binding_bad { OTHER_BINDING } else { BINDING };
                        let expected = if unsupported != 0 {
                            Err(RecoveryError::Unsupported)
                        } else if vault != 0 {
                            Err(RecoveryError::VaultMismatch)
                        } else if wrong_dek {
                            Err(RecoveryError::WrongKey)
                        } else if tamper != 0 {
                            Err(RecoveryError::Tampered)
                        } else if binding_bad {
                            Err(RecoveryError::BindingMismatch)
                        } else {
                            Ok(())
                        };
                        assert_eq!(
                            verify_manifest(&m, expected_vault, key, binding),
                            expected,
                            "unsupported{unsupported} vault{vault} wrong_dek={wrong_dek} tamper{tamper} binding_bad={binding_bad}"
                        );
                        cells += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cells, 108);
}

// ---------------------------------------------------------------- join path

/// RT-JOIN-1 (§4.6.3 합류: unwrap → verify): once the RK has authenticated
/// the wrapped DEK, a key_check that does not verify is tampering, never a
/// wrong key. 256 single-bit flips and 13 forged/derived/resized key_checks,
/// each with a stale and a correctly re-signed MAC and with either binding,
/// are all Tampered on `unlock` (1,076 cases); `verify_manifest` with the
/// keychain DEK keeps WrongKey as designed.
#[test]
fn rt_join_key_check_failure_is_always_tampered() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let other_dek = SecretKey::from_bytes([0x99; 32]);
    let genuine = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    let k_man = derive_subkey(&dek, VAULT, 1, Purpose::Manifest);
    let k_check = derive_subkey(&dek, VAULT, 1, Purpose::KeyCheck);
    let msg: &[u8] = b"sayknow-key-check";
    let real = hmac_sha256(&k_check, msg);
    assert_eq!(BASE64.decode(genuine.key_check.as_bytes()).unwrap(), real.to_vec());

    let mut candidates: Vec<(String, Vec<u8>)> = (0..256)
        .map(|bit| {
            let mut c = real.to_vec();
            c[bit / 8] ^= 1 << (bit % 8);
            (format!("bit {bit}"), c)
        })
        .collect();
    let derived = |key: &SecretKey, message: &[u8]| hmac_sha256(key, message).to_vec();
    for (name, check) in [
        ("zero", vec![0u8; 32]),
        ("ones", vec![0xff; 32]),
        ("empty", Vec::new()),
        ("31 bytes", real[..31].to_vec()),
        ("33 bytes", [&real[..], &[0u8][..]].concat()),
        ("64 bytes", [real, real].concat()),
        ("other dek", derived(&derive_subkey(&other_dek, VAULT, 1, Purpose::KeyCheck), msg)),
        ("other vault", derived(&derive_subkey(&dek, OTHER_VAULT, 1, Purpose::KeyCheck), msg)),
        ("epoch 2", derived(&derive_subkey(&dek, VAULT, 2, Purpose::KeyCheck), msg)),
        ("manifest purpose", derived(&k_man, msg)),
        ("raw dek", derived(&dek, msg)),
        ("other message", derived(&k_check, b"sayknow-key-check\0")),
        ("empty message", derived(&k_check, b"")),
    ] {
        candidates.push((name.to_owned(), check));
    }
    assert_eq!(candidates.len(), 269);

    let mut cases = 0;
    for (name, check) in &candidates {
        for resigned in [false, true] {
            for binding in [BINDING, OTHER_BINDING] {
                let mut m = genuine.clone();
                m.key_check = BASE64.encode(check);
                if resigned {
                    resign(&mut m, &k_man);
                }
                let got = unlock(&m, VAULT, PY_RK_TEXT, binding);
                assert_ne!(got, Err(RecoveryError::WrongKey), "{name} resigned={resigned}");
                assert_eq!(got, Err(RecoveryError::Tampered), "{name} resigned={resigned} {binding}");
                assert_eq!(verify_manifest(&m, VAULT, &dek, binding), Err(RecoveryError::WrongKey), "verify {name}");
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 1076);

    for bad in ["!!", "AAAA=", "A", "QUJD\n", " QUJD", "QUJD===="] {
        let mut m = genuine.clone();
        m.key_check = bad.into();
        assert_eq!(unlock(&m, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::Tampered), "{bad:?}");
        resign(&mut m, &k_man);
        assert_eq!(unlock(&m, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::Tampered), "resigned {bad:?}");
    }
}

/// RT-JOIN-2 (§4.6.3, AC 13/23): an RK holder who rewraps a different DEK, or
/// a plaintext of the wrong length, under the right KEK and AAD gets past the
/// AEAD but is then Tampered — never WrongKey and never a returned key.
#[test]
fn rt_join_post_unwrap_substitution_is_tampered() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let genuine = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    let k_man = derive_subkey(&dek, VAULT, 1, Purpose::Manifest);
    let kek = derive_kek(&SecretKey::from_bytes(rk_bytes()), VAULT);
    let addr = ObjectAddress::wrapped_dek(VAULT, 1);
    let mut one_bit = dek_bytes();
    one_bit[31] ^= 1;
    for (name, plaintext) in [
        ("zero dek", vec![0u8; 32]),
        ("other dek", vec![0x99; 32]),
        ("one bit", one_bit.to_vec()),
        ("len 0", Vec::new()),
        ("len 1", vec![0x40]),
        ("len 16", dek_bytes()[..16].to_vec()),
        ("len 31", dek_bytes()[..31].to_vec()),
        ("len 33", [&dek_bytes()[..], &[0u8][..]].concat()),
        ("len 64", [dek_bytes(), dek_bytes()].concat()),
        ("len 1024", vec![0x40; 1024]),
    ] {
        for resigned in [false, true] {
            for binding in [BINDING, OTHER_BINDING] {
                let mut m = genuine.clone();
                m.wrapped_dek = BASE64.encode(&seal(&kek, &addr, &plaintext).unwrap());
                if resigned {
                    resign(&mut m, &k_man);
                }
                assert_eq!(
                    unlock(&m, VAULT, PY_RK_TEXT, binding),
                    Err(RecoveryError::Tampered),
                    "{name} resigned={resigned} {binding}"
                );
            }
        }
    }
    // Control: a fresh re-wrap of the real DEK (new nonce) still opens.
    let mut rewrapped = genuine.clone();
    rewrapped.wrapped_dek = BASE64.encode(&seal(&kek, &addr, dek.as_bytes()).unwrap());
    resign(&mut rewrapped, &k_man);
    assert_eq!(unlock(&rewrapped, VAULT, PY_RK_TEXT, BINDING).unwrap(), dek);
}

// ---------------------------------------------------------------- epochs & location

/// RT-MAN-5b (§4.6.2 key_epoch:1, §4.5 hold, G1-RT-B1 regression): genuine
/// manifests at epochs 0/2/3/255/256/2^24/MAX-1/MAX are Unsupported from both
/// `unlock` and `verify_manifest` no matter which other faults are stacked
/// (wrong location, invalid ids, any RK text, any DEK, any binding, zero MAC).
#[test]
fn rt_man_future_epochs_win_over_every_other_fault() {
    let dek = SecretKey::from_bytes(dek_bytes());
    let other_dek = SecretKey::from_bytes([0x99; 32]);
    let checksum_text = PY_RK_TEXT.replacen('A', "B", 1);
    let wrong_text = rk_text([0xA5; 32]);
    let mut cases = 0;
    for epoch in [0u32, 2, 3, 255, 256, 1 << 24, u32::MAX - 1, u32::MAX] {
        for manifest_vault in [VAULT, OTHER_VAULT] {
            let genuine = genuine_manifest(rk_bytes(), &dek, manifest_vault, epoch);
            let mut zero_mac = genuine.clone();
            zero_mac.manifest_mac = BASE64.encode(&[0u8; 32]);
            for m in [&genuine, &zero_mac] {
                for expected_vault in [VAULT, OTHER_VAULT, "..", ""] {
                    for binding in [BINDING, "", OTHER_BINDING] {
                        for text in [PY_RK_TEXT, "", checksum_text.as_str(), wrong_text.as_str()] {
                            assert_eq!(
                                unlock(m, expected_vault, text, binding),
                                Err(RecoveryError::Unsupported),
                                "epoch {epoch} in {manifest_vault} at {expected_vault:?}"
                            );
                            cases += 1;
                        }
                        for key in [&dek, &other_dek] {
                            assert_eq!(
                                verify_manifest(m, expected_vault, key, binding),
                                Err(RecoveryError::Unsupported),
                                "verify epoch {epoch}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 8 * 2 * 2 * 4 * 3 * 6);
    // Control: the identical recipe at epoch 1 opens and verifies.
    let m = genuine_manifest(rk_bytes(), &dek, VAULT, 1);
    assert_eq!(unlock(&m, VAULT, PY_RK_TEXT, BINDING).unwrap(), dek);
    assert_eq!(verify_manifest(&m, VAULT, &dek, BINDING), Ok(()));
}

/// RT-MAN-6 (§4.2 vault isolation, §4.6.2, AC 23): a complete, genuine
/// OTHER_VAULT manifest planted at VAULT's location is VaultMismatch for every
/// RK text (including OTHER_VAULT's own correct RK) and every DEK; near-miss
/// ids and invalid-but-equal ids are refused too; rewriting the planted
/// manifest's vault_id to VAULT only yields WrongKey, never a key.
#[test]
fn rt_man_planted_other_vault_manifest_is_vault_mismatch() {
    let dek_a = SecretKey::from_bytes(dek_bytes());
    let dek_b = SecretKey::from_bytes([0x99; 32]);
    let rk_b = [0xA5; 32];
    let checksum_text = PY_RK_TEXT.replacen('A', "B", 1);
    // Distinct keys, and a worst case that reuses VAULT's RK and DEK.
    let planted = [
        (genuine_manifest(rk_b, &dek_b, OTHER_VAULT, 1), rk_b, dek_b.clone()),
        (genuine_manifest(rk_bytes(), &dek_a, OTHER_VAULT, 1), rk_bytes(), dek_a.clone()),
    ];
    for (m, rk, dek) in &planted {
        let own_text = rk_text(*rk);
        assert_eq!(unlock(m, OTHER_VAULT, &own_text, BINDING).unwrap(), *dek);
        for text in [own_text.as_str(), PY_RK_TEXT, "", checksum_text.as_str()] {
            for binding in [BINDING, OTHER_BINDING] {
                assert_eq!(unlock(m, VAULT, text, binding), Err(RecoveryError::VaultMismatch), "{text:?}");
            }
        }
        for key in [&dek_a, &dek_b] {
            assert_eq!(verify_manifest(m, VAULT, key, BINDING), Err(RecoveryError::VaultMismatch));
        }

        // Rewritten to claim VAULT: the KEK salt and AAD still bind it to OTHER_VAULT.
        let mut relabeled = m.clone();
        relabeled.vault_id = VAULT.into();
        for text in [own_text.as_str(), PY_RK_TEXT] {
            assert_eq!(unlock(&relabeled, VAULT, text, BINDING), Err(RecoveryError::WrongKey), "relabeled {text:?}");
        }
        assert!(verify_manifest(&relabeled, VAULT, dek, BINDING).is_err());
    }

    // Genuine manifests for near-miss ids are not VAULT's.
    let rk = SecretKey::from_bytes(rk_bytes());
    for near in [
        "VAULT-TEST-0001",
        "Vault-test-0001",
        "vault-test-000",
        "vault-test-00011",
        "vault-test-0001.",
        "vault-test-0001 ",
        " vault-test-0001",
        "vault-test-0001\0",
        "vault-test-0001/",
        "./vault-test-0001",
        "vault\u{2010}test\u{2010}0001",
        "vault-test-０００１",
    ] {
        let m = manifest_with_keys(
            &derive_kek(&rk, near),
            &dek_a,
            &derive_subkey(&dek_a, near, 1, Purpose::Manifest),
            &derive_subkey(&dek_a, near, 1, Purpose::KeyCheck),
            near,
            1,
        );
        assert_eq!(unlock(&m, VAULT, PY_RK_TEXT, BINDING), Err(RecoveryError::VaultMismatch), "{near:?}");
        assert_eq!(verify_manifest(&m, VAULT, &dek_a, BINDING), Err(RecoveryError::VaultMismatch), "{near:?}");
        let genuine = genuine_manifest(rk_bytes(), &dek_a, VAULT, 1);
        assert_eq!(unlock(&genuine, near, PY_RK_TEXT, BINDING), Err(RecoveryError::VaultMismatch), "at {near:?}");
    }

    // Invalid ids are refused even when the location names the same string.
    let long = "v".repeat(65);
    for bad in ["", ".", "..", "-v", "_v", ".v", "v/w", "v\\w", "v w", "볼트", long.as_str()] {
        let m = manifest_with_keys(
            &derive_kek(&rk, bad),
            &dek_a,
            &derive_subkey(&dek_a, bad, 1, Purpose::Manifest),
            &derive_subkey(&dek_a, bad, 1, Purpose::KeyCheck),
            bad,
            1,
        );
        assert_eq!(unlock(&m, bad, PY_RK_TEXT, BINDING), Err(RecoveryError::VaultMismatch), "{bad:?}");
        assert_eq!(verify_manifest(&m, bad, &dek_a, BINDING), Err(RecoveryError::VaultMismatch), "{bad:?}");
    }
    // The 64-byte boundary id itself still works end to end.
    let max = format!("v{}", "0".repeat(63));
    let m = genuine_manifest(rk_bytes(), &dek_a, &max, 1);
    assert_eq!(unlock(&m, &max, PY_RK_TEXT, BINDING).unwrap(), dek_a);
    assert_eq!(verify_manifest(&m, &max, &dek_a, BINDING), Ok(()));
}
