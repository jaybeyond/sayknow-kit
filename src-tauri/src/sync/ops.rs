//! Op data model, canonical encoding, hashes, and the hybrid logical clock
//! (§4.3, §4.6.1 N-7).
//!
//! Every byte that is hashed or sealed goes through [`canonical_json`]: object
//! keys sorted by byte order, no whitespace, integers only. Remote segments
//! must already be in that form, so a re-encode can never change a hash.
//!
//! Contract deviations (G3 design, ops.rs):
//! - `canonical_json`, `body_hash`, `content_hash`, and `segment_hash` return
//!   `Result`: floats are rejected with `OpError::Float`, so an unvalidated
//!   value cannot be hashed silently.
//! - `Segment::decode` also rejects bytes that are not already canonical
//!   (`OpError::NotCanonical`), which covers duplicate keys and whitespace.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::envelope::is_id;

/// The only op and segment format this build writes and reads.
pub(crate) const FORMAT_V: u64 = 1;

/// Local clock advance is capped this far ahead of the wall clock (AC 24).
pub(crate) const MAX_CLOCK_SKEW_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpError {
    /// Ops never carry floats; they have no single canonical text.
    Float,
    /// Remote bytes differ from their own canonical re-encoding.
    NotCanonical,
    /// Written by a newer format; the stream is held until the app updates.
    FutureVersion(u64),
    Schema(String),
    /// Segment plaintext disagrees with its address (§4.6.1 N-7).
    CrossCheck(String),
    /// An integer SQLite cannot store (above `i64::MAX`); remote input that
    /// carries one is quarantined and its stream held (§4.5).
    Range(String),
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::Float => f.write_str("floats are not allowed in ops"),
            OpError::NotCanonical => f.write_str("bytes are not canonical JSON"),
            OpError::FutureVersion(v) => write!(f, "unsupported future format version {v}"),
            OpError::Schema(why) => write!(f, "schema violation: {why}"),
            OpError::CrossCheck(why) => write!(f, "segment cross-check failed: {why}"),
            OpError::Range(why) => write!(f, "integer out of range: {why}"),
        }
    }
}

impl std::error::Error for OpError {}

/// Largest integer SQLite stores (`INTEGER` is a signed 64-bit value).
pub(crate) const MAX_STORED: u64 = i64::MAX as u64;

fn in_range(field: &str, value: u64) -> Result<(), OpError> {
    if value > MAX_STORED {
        return Err(OpError::Range(format!("{field} {value} exceeds i64::MAX")));
    }
    Ok(())
}

fn schema(why: impl Into<String>) -> OpError {
    OpError::Schema(why.into())
}

/// Canonical JSON: keys sorted by byte order, no whitespace, integers as JSON
/// numbers, strings escaped exactly as `serde_json` escapes them.
pub(crate) fn canonical_json(value: &Value) -> Result<Vec<u8>, OpError> {
    let mut out = Vec::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) -> Result<(), OpError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                out.extend_from_slice(u.to_string().as_bytes());
            } else if let Some(i) = n.as_i64() {
                out.extend_from_slice(i.to_string().as_bytes());
            } else {
                return Err(OpError::Float);
            }
        }
        Value::String(s) => write_string(s, out)?,
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out)?;
                out.push(b':');
                write_canonical(&map[key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn write_string(s: &str, out: &mut Vec<u8>) -> Result<(), OpError> {
    serde_json::to_writer(&mut *out, s).map_err(|e| schema(e.to_string()))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, OpError> {
    serde_json::to_value(value).map_err(|e| schema(e.to_string()))
}

/// Hybrid logical clock stamp, ordered by `(wall_ms, ctr)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hlc {
    pub(crate) wall_ms: u64,
    pub(crate) ctr: u32,
}

/// This device's clock. Received stamps are stored verbatim; only the local
/// advance is clamped to `now + 24h` (§4.3, AC 24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct HlcClock {
    pub(crate) last: Hlc,
    pub(crate) clamp_warnings: u64,
}

impl HlcClock {
    pub(crate) fn tick(&mut self, now_ms: u64) -> Hlc {
        let wall = now_ms.max(self.last.wall_ms);
        let next = if wall > self.last.wall_ms {
            Hlc { wall_ms: wall, ctr: 0 }
        } else if self.last.ctr == u32::MAX {
            Hlc { wall_ms: wall.saturating_add(1), ctr: 0 }
        } else {
            Hlc { wall_ms: wall, ctr: self.last.ctr + 1 }
        };
        self.last = next;
        next
    }

    /// Advances `last` towards a received stamp, never past `now + 24h`.
    pub(crate) fn observe(&mut self, recv: Hlc, now_ms: u64) {
        let cap = now_ms.saturating_add(MAX_CLOCK_SKEW_MS);
        let target = if recv.wall_ms > cap {
            self.clamp_warnings += 1;
            Hlc { wall_ms: cap, ctr: 0 }
        } else {
            recv
        };
        if target > self.last {
            self.last = target;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Memo,
    Conv,
    Tr,
    Policy,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Memo => "memo",
            Kind::Conv => "conv",
            Kind::Tr => "tr",
            Kind::Policy => "policy",
        }
    }
}

/// §4.3 `body`. The dot that carries it is not part of it ([`Dot`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpBody {
    pub(crate) v: u64,
    pub(crate) op_id: String,
    pub(crate) origin_device: String,
    pub(crate) hlc: Hlc,
    pub(crate) ctx: BTreeMap<String, u64>,
    /// Op ids observed outside `ctx` (rotation, fork ranges); sorted, unique.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) obs: Vec<String>,
    pub(crate) kind: Kind,
    pub(crate) entity_id: String,
    #[serde(rename = "type")]
    pub(crate) op_type: String,
    pub(crate) payload: Value,
}

/// `(device, seq)`: where an op was published. Carried outside the body, so
/// one op can sit at several dots (I6 aliases).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct Dot {
    pub(crate) device: String,
    pub(crate) seq: u64,
}

/// Canonical UUIDv7 text (lowercase, hyphenated).
pub(crate) fn is_op_id(value: &str) -> bool {
    uuid::Uuid::try_parse(value)
        .map(|u| u.get_version_num() == 7 && u.hyphenated().to_string() == value)
        .unwrap_or(false)
}

pub(crate) fn body_hash(body: &OpBody) -> Result<[u8; 32], OpError> {
    Ok(sha256(&canonical_json(&to_value(body)?)?))
}

/// Excludes ctx, obs, and origin_device, so a restamped copy keeps it (I6).
pub(crate) fn content_hash(body: &OpBody) -> Result<[u8; 32], OpError> {
    let content = serde_json::json!({
        "op_id": body.op_id,
        "kind": body.kind,
        "entity_id": body.entity_id,
        "type": body.op_type,
        "payload": body.payload,
        "hlc": body.hlc,
    });
    Ok(sha256(&canonical_json(&content)?))
}

/// Op ids this op builds on. Message parents are msg ids, not op ids.
pub(crate) fn refs(body: &OpBody) -> Vec<&str> {
    if body.kind == Kind::Memo && body.op_type == "edit" {
        if let Some(base) = body.payload.get("base_op_id").and_then(Value::as_str) {
            return vec![base];
        }
    }
    Vec::new()
}

/// Repoints a reference after a reissue (R6). Changes `content_hash`.
pub(crate) fn remap_ref(body: &mut OpBody, old: &str, new: &str) -> bool {
    if body.kind != Kind::Memo || body.op_type != "edit" {
        return false;
    }
    match body.payload.get_mut("base_op_id") {
        Some(base) if base.as_str() == Some(old) => {
            *base = Value::String(new.to_owned());
            true
        }
        _ => false,
    }
}

impl OpBody {
    /// Full §4.3 schema check. Malformed remote input is an error, never a panic.
    pub(crate) fn validate(&self) -> Result<(), OpError> {
        if self.v > FORMAT_V {
            return Err(OpError::FutureVersion(self.v));
        }
        if self.v != FORMAT_V {
            return Err(schema(format!("op version {}", self.v)));
        }
        if !is_op_id(&self.op_id) {
            return Err(schema("op_id is not a UUIDv7"));
        }
        if !is_id(&self.origin_device) {
            return Err(schema("origin_device"));
        }
        if !is_id(&self.entity_id) {
            return Err(schema("entity_id"));
        }
        if !self.ctx.keys().all(|d| is_id(d)) {
            return Err(schema("ctx device id"));
        }
        if !self.obs.iter().all(|o| is_op_id(o)) || !self.obs.windows(2).all(|w| w[0] < w[1]) {
            return Err(schema("obs must be sorted unique op ids"));
        }
        // Shared decision: every integer a remote op carries (hlc wall, seqs,
        // ctx values) must fit SQLite's i64, even where it is stored as text.
        in_range("hlc.wall_ms", self.hlc.wall_ms)?;
        for (device, &seq) in &self.ctx {
            in_range(&format!("ctx[{device}]"), seq)?;
        }
        canonical_json(&self.payload)?;
        validate_payload(self)
    }
}

/// Checks the payload object's key set: every `required` key present, nothing
/// outside `required ∪ optional`.
fn fields<'a>(payload: &'a Value, required: &[&str], optional: &[&str]) -> Result<&'a Map<String, Value>, OpError> {
    let map = payload.as_object().ok_or_else(|| schema("payload must be an object"))?;
    if let Some(missing) = required.iter().find(|k| !map.contains_key(**k)) {
        return Err(schema(format!("payload missing {missing}")));
    }
    if let Some(extra) = map.keys().find(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str())) {
        return Err(schema(format!("payload has unknown field {extra}")));
    }
    Ok(map)
}

fn want_str<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str, OpError> {
    map.get(key).and_then(Value::as_str).ok_or_else(|| schema(format!("{key} must be a string")))
}

fn want_bool(map: &Map<String, Value>, key: &str) -> Result<bool, OpError> {
    map.get(key).and_then(Value::as_bool).ok_or_else(|| schema(format!("{key} must be a boolean")))
}

fn want_id(map: &Map<String, Value>, key: &str) -> Result<(), OpError> {
    if is_id(want_str(map, key)?) {
        Ok(())
    } else {
        Err(schema(format!("{key} is not an id")))
    }
}

/// An optional string field may be absent or null.
fn want_opt_str<'a>(map: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, OpError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(schema(format!("{key} must be a string"))),
    }
}

fn validate_payload(body: &OpBody) -> Result<(), OpError> {
    let p = &body.payload;
    match (body.kind, body.op_type.as_str()) {
        (Kind::Memo, "create") => {
            let m = fields(p, &["text", "pinned"], &[])?;
            want_str(m, "text")?;
            want_bool(m, "pinned")?;
        }
        (Kind::Memo, "edit") => {
            let m = fields(p, &["text", "base_op_id"], &[])?;
            want_str(m, "text")?;
            if !is_op_id(want_str(m, "base_op_id")?) {
                return Err(schema("base_op_id is not an op id"));
            }
        }
        (Kind::Memo, "pin") | (Kind::Tr, "tr.pin") => {
            want_bool(fields(p, &["pinned"], &[])?, "pinned")?;
        }
        (Kind::Memo, "note") => {
            want_str(fields(p, &["note"], &[])?, "note")?;
        }
        (Kind::Memo, "delete") | (Kind::Conv, "delete") | (Kind::Tr, "tr.delete") => {
            fields(p, &[], &[])?;
        }
        (Kind::Conv, "create") => {
            // `src_id` (G8): the webview's own id, so a load can hand it back.
            let m = fields(p, &["title"], &["src_id"])?;
            want_str(m, "title")?;
            want_opt_str(m, "src_id")?;
        }
        (Kind::Conv, "rename") => {
            want_str(fields(p, &["title"], &[])?, "title")?;
        }
        (Kind::Conv, "set_active") => {
            want_id(fields(p, &["leaf"], &[])?, "leaf")?;
        }
        (Kind::Conv, "msg.add") => {
            let m = fields(
                p,
                &["conv_id", "msg_id", "role", "content", "attachments", "ts"],
                &["parent_id", "model", "src_id"],
            )?;
            if want_str(m, "conv_id")? != body.entity_id {
                return Err(schema("msg.add conv_id differs from entity_id"));
            }
            want_id(m, "msg_id")?;
            if let Some(parent) = want_opt_str(m, "parent_id")? {
                if !is_id(parent) {
                    return Err(schema("parent_id is not an id"));
                }
            }
            if want_str(m, "role")?.is_empty() {
                return Err(schema("role is empty"));
            }
            want_str(m, "content")?;
            want_opt_str(m, "model")?;
            want_opt_str(m, "src_id")?;
            if !m.get("attachments").is_some_and(Value::is_array) {
                return Err(schema("attachments must be an array"));
            }
            if !m.get("ts").is_some_and(Value::is_u64) {
                return Err(schema("ts must be a non-negative integer"));
            }
        }
        (Kind::Tr, "tr.add") => {
            let m = p.as_object().ok_or_else(|| schema("payload must be an object"))?;
            if m.contains_key("pinned") {
                want_bool(m, "pinned")?;
            }
        }
        (Kind::Policy, "policy.retention") => {
            p.as_object().ok_or_else(|| schema("payload must be an object"))?;
        }
        (kind, op_type) => return Err(schema(format!("unknown op {}/{op_type}", kind.as_str()))),
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SegOp {
    pub(crate) seq: u64,
    pub(crate) body: OpBody,
}

/// §4.3 `segment`: the plaintext of one sealed `log/<device>/<first>-<last>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Segment {
    pub(crate) v: u64,
    pub(crate) device_id: String,
    pub(crate) first_seq: u64,
    pub(crate) last_seq: u64,
    /// Lowercase hex of the previous segment's [`segment_hash`] (or the
    /// genesis head).
    pub(crate) prev_seg_hash: String,
    pub(crate) ops: Vec<SegOp>,
}

impl Segment {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, OpError> {
        canonical_json(&to_value(self)?)
    }

    /// Parses canonical bytes. A newer `v` on the segment or any op is
    /// reported as `FutureVersion` before the canonical and strict field
    /// checks run: a newer format may use encodings v1 rejects (§4.5 hold).
    pub(crate) fn decode(bytes: &[u8]) -> Result<Segment, OpError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|e| schema(e.to_string()))?;
        let future = |v: Option<&Value>| v.and_then(Value::as_u64).filter(|v| *v > FORMAT_V);
        if let Some(v) = future(value.get("v")) {
            return Err(OpError::FutureVersion(v));
        }
        if let Some(ops) = value.get("ops").and_then(Value::as_array) {
            if let Some(v) = ops.iter().find_map(|op| future(op.get("body").and_then(|b| b.get("v")))) {
                return Err(OpError::FutureVersion(v));
            }
        }
        if canonical_json(&value)? != bytes {
            return Err(OpError::NotCanonical);
        }
        serde_json::from_value(value).map_err(|e| schema(e.to_string()))
    }

    /// Schema check of the segment and every op body.
    pub(crate) fn validate(&self) -> Result<(), OpError> {
        if self.v > FORMAT_V {
            return Err(OpError::FutureVersion(self.v));
        }
        if self.v != FORMAT_V {
            return Err(schema(format!("segment version {}", self.v)));
        }
        if !is_id(&self.device_id) {
            return Err(schema("segment device_id"));
        }
        let hex_ok = self.prev_seg_hash.len() == 64
            && self.prev_seg_hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !hex_ok {
            return Err(schema("prev_seg_hash must be 64 lowercase hex"));
        }
        in_range("first_seq", self.first_seq)?;
        in_range("last_seq", self.last_seq)?;
        for op in &self.ops {
            in_range("seq", op.seq)?;
            op.body.validate()?;
        }
        Ok(())
    }
}

/// §4.6.1 N-7: the plaintext must name the device and range it was fetched
/// under, and carry exactly the seqs `first..=last` in order.
pub(crate) fn check_plaintext(segment: &Segment, expected_device: &str, first: u64, last: u64) -> Result<(), OpError> {
    let fail = |why: &str| Err(OpError::CrossCheck(why.to_owned()));
    if segment.device_id != expected_device {
        return fail("device_id");
    }
    if segment.first_seq != first || segment.last_seq != last {
        return fail("seq range");
    }
    if first == 0 || first > last {
        return fail("empty or zero range");
    }
    if segment.ops.len() as u64 != last - first + 1 {
        return fail("op count");
    }
    if segment.ops.iter().zip(first..=last).any(|(op, want)| op.seq != want) {
        return fail("seqs not contiguous");
    }
    Ok(())
}

/// What the next segment's `prev_seg_hash` names.
pub(crate) fn segment_hash(segment: &Segment) -> Result<[u8; 32], OpError> {
    Ok(sha256(&segment.encode()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::hex;
    use serde_json::json;

    fn oid(n: u64) -> String {
        uuid::Builder::from_unix_timestamp_millis(1_700_000_000_000 + n, &[n as u8; 10]).into_uuid().to_string()
    }

    fn memo_body(op_type: &str, payload: Value) -> OpBody {
        OpBody {
            v: 1,
            op_id: oid(1),
            origin_device: "dev-a".to_owned(),
            hlc: Hlc { wall_ms: 1_000, ctr: 2 },
            ctx: BTreeMap::from([("dev-a".to_owned(), 4), ("dev-b".to_owned(), 7)]),
            obs: Vec::new(),
            kind: Kind::Memo,
            entity_id: "memo-1".to_owned(),
            op_type: op_type.to_owned(),
            payload,
        }
    }

    #[test]
    fn canonical_json_matches_an_independent_python_encoding() {
        // python3 -c 'import json,hashlib;v={"z":1,"a":[True,None,-3,{"é":"x\"\\\n\u0001","B":"한글😀"}],"aa":"\u007f\u2028","":0};s=json.dumps(v,sort_keys=True,separators=(",",":"),ensure_ascii=False);print(s);print(hashlib.sha256(s.encode()).hexdigest())'
        let value = json!({
            "z": 1,
            "a": [true, null, -3, {"é": "x\"\\\n\u{1}", "B": "한글😀"}],
            "aa": "\u{7f}\u{2028}",
            "": 0
        });
        let bytes = canonical_json(&value).unwrap();
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            "{\"\":0,\"a\":[true,null,-3,{\"B\":\"한글😀\",\"é\":\"x\\\"\\\\\\n\\u0001\"}],\"aa\":\"\u{7f}\u{2028}\",\"z\":1}"
        );
        assert_eq!(hex(&sha256(&bytes)), "fd8e035e4c5b990b268fed9b9e257cd8c8de9cd49a14a95d84b234154f022170");
    }

    #[test]
    fn canonical_json_rejects_floats_anywhere() {
        assert_eq!(canonical_json(&json!(1.5)), Err(OpError::Float));
        assert_eq!(canonical_json(&json!({"a": [1, {"b": 0.0}]})), Err(OpError::Float));
        assert_eq!(canonical_json(&json!(u64::MAX)).unwrap(), u64::MAX.to_string().into_bytes());
        assert_eq!(canonical_json(&json!(i64::MIN)).unwrap(), i64::MIN.to_string().into_bytes());
    }

    #[test]
    fn body_and_content_hashes_match_independent_vectors() {
        let body = memo_body("create", json!({"text": "hi", "pinned": false}));
        // python3 -c 'import json,hashlib;b={"v":1,"op_id":"018bcfe5-6801-7101-8101-010101010101","origin_device":"dev-a","hlc":{"wall_ms":1000,"ctr":2},"ctx":{"dev-a":4,"dev-b":7},"kind":"memo","entity_id":"memo-1","type":"create","payload":{"text":"hi","pinned":False}};c={k:b[k] for k in ("op_id","kind","entity_id","type","payload","hlc")};e=lambda v:json.dumps(v,sort_keys=True,separators=(",",":"),ensure_ascii=False).encode();print(hashlib.sha256(e(b)).hexdigest());print(hashlib.sha256(e(c)).hexdigest())'
        assert_eq!(body.op_id, "018bcfe5-6801-7101-8101-010101010101");
        assert_eq!(hex(&body_hash(&body).unwrap()), "a4ec42b2d1163d56ea1ee0abb9dd51d0ae23ab14e9592b39aa70159a445a888d");
        assert_eq!(hex(&content_hash(&body).unwrap()), "3d63eeb0267758e4a46e6abfc6dc0b77cf765ad77d2ade27211662246376ad44");
    }

    #[test]
    fn content_hash_survives_a_restamp_but_body_hash_does_not() {
        let body = memo_body("create", json!({"text": "hi", "pinned": false}));
        let mut restamped = body.clone();
        restamped.origin_device = "dev-a2".to_owned();
        restamped.ctx = BTreeMap::from([("dev-b".to_owned(), 3)]);
        restamped.obs = vec![oid(9)];
        assert_eq!(content_hash(&body), content_hash(&restamped));
        assert_ne!(body_hash(&body), body_hash(&restamped));
        let mut moved = body.clone();
        moved.hlc.ctr += 1;
        assert_ne!(content_hash(&body), content_hash(&moved));
    }

    #[test]
    fn empty_obs_is_omitted_and_present_obs_is_serialized() {
        let mut body = memo_body("delete", json!({}));
        assert!(to_value(&body).unwrap().get("obs").is_none());
        body.obs = vec![oid(3)];
        assert_eq!(to_value(&body).unwrap()["obs"], json!([oid(3)]));
        assert_eq!(to_value(&body).unwrap()["type"], json!("delete"));
    }

    #[test]
    fn refs_and_remap_follow_the_memo_edit_base() {
        let mut edit = memo_body("edit", json!({"text": "x", "base_op_id": oid(5)}));
        let before = content_hash(&edit).unwrap();
        assert_eq!(refs(&edit), vec![oid(5).as_str()]);
        assert!(!remap_ref(&mut edit, &oid(6), &oid(7)));
        assert!(remap_ref(&mut edit, &oid(5), &oid(7)));
        assert_eq!(refs(&edit), vec![oid(7).as_str()]);
        assert_ne!(content_hash(&edit).unwrap(), before);
        let mut msg = memo_body("create", json!({"text": "x", "pinned": true}));
        assert!(refs(&msg).is_empty());
        assert!(!remap_ref(&mut msg, &oid(5), &oid(7)));
    }

    #[test]
    fn hlc_ticks_monotonically_and_bumps_the_counter_on_a_stalled_wall() {
        let mut clock = HlcClock::default();
        assert_eq!(clock.tick(100), Hlc { wall_ms: 100, ctr: 0 });
        assert_eq!(clock.tick(100), Hlc { wall_ms: 100, ctr: 1 });
        assert_eq!(clock.tick(90), Hlc { wall_ms: 100, ctr: 2 });
        assert_eq!(clock.tick(101), Hlc { wall_ms: 101, ctr: 0 });
        clock.last = Hlc { wall_ms: 101, ctr: u32::MAX };
        let next = clock.tick(101);
        assert!(next > Hlc { wall_ms: 101, ctr: u32::MAX });
    }

    #[test]
    fn ac24_future_stamp_is_kept_verbatim_and_local_advance_is_clamped() {
        let now = 1_000_000_000;
        let hour = 60 * 60 * 1000;
        let remote = Hlc { wall_ms: now + 48 * hour, ctr: 5 };
        let mut body = memo_body("pin", json!({"pinned": true}));
        body.hlc = remote;
        let bytes = canonical_json(&to_value(&body).unwrap()).unwrap();
        let back: OpBody = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.hlc, remote, "received HLC is stored as sent");

        let mut clock = HlcClock::default();
        clock.observe(remote, now);
        assert_eq!(clock.clamp_warnings, 1);
        assert!(clock.last.wall_ms <= now + MAX_CLOCK_SKEW_MS);
        let local = clock.tick(now);
        assert!(local.wall_ms <= now + MAX_CLOCK_SKEW_MS);
        assert!(local < remote);

        clock.observe(Hlc { wall_ms: now + hour, ctr: 9 }, now);
        assert_eq!(clock.clamp_warnings, 1, "an in-range stamp is not a warning");
        // Exactly at the cap is not clamped; beyond it is, against the new now.
        let later = now + 2 * hour;
        clock.observe(Hlc { wall_ms: later + MAX_CLOCK_SKEW_MS, ctr: 0 }, later);
        assert_eq!(clock.clamp_warnings, 1);
        clock.observe(Hlc { wall_ms: now + 72 * hour, ctr: 0 }, later);
        assert_eq!(clock.clamp_warnings, 2);
        assert_eq!(clock.last, Hlc { wall_ms: later + MAX_CLOCK_SKEW_MS, ctr: 0 });
        assert!(clock.tick(later) > Hlc { wall_ms: later + MAX_CLOCK_SKEW_MS, ctr: 0 });
    }

    #[test]
    fn every_op_type_validates_and_malformed_payloads_are_errors() {
        let conv = |op_type: &str, payload: Value| OpBody {
            kind: Kind::Conv,
            entity_id: "conv-1".to_owned(),
            ..memo_body(op_type, payload)
        };
        let tr = |op_type: &str, payload: Value| OpBody { kind: Kind::Tr, ..memo_body(op_type, payload) };
        let good = [
            memo_body("create", json!({"text": "", "pinned": false})),
            memo_body("edit", json!({"text": "t", "base_op_id": oid(2)})),
            memo_body("pin", json!({"pinned": true})),
            memo_body("note", json!({"note": "n"})),
            memo_body("delete", json!({})),
            conv("create", json!({"title": "t"})),
            conv("rename", json!({"title": "u"})),
            conv("set_active", json!({"leaf": "msg-2"})),
            conv("delete", json!({})),
            conv(
                "msg.add",
                json!({"conv_id": "conv-1", "msg_id": "msg-1", "role": "user", "content": "hi", "attachments": [], "ts": 5}),
            ),
            conv(
                "msg.add",
                json!({"conv_id": "conv-1", "msg_id": "msg-2", "parent_id": "msg-1", "role": "assistant",
                       "content": "yo", "model": "m", "attachments": [{"blob_id": "ab"}], "ts": 6}),
            ),
            tr("tr.add", json!({"src": "a", "dst": "b", "pinned": false})),
            tr("tr.pin", json!({"pinned": true})),
            tr("tr.delete", json!({})),
            OpBody { kind: Kind::Policy, ..memo_body("policy.retention", json!({"days": 30})) },
        ];
        for body in &good {
            assert_eq!(body.validate(), Ok(()), "{}/{}", body.kind.as_str(), body.op_type);
        }

        let bad = [
            memo_body("create", json!({"text": "t"})),
            memo_body("create", json!({"text": 1, "pinned": false})),
            memo_body("create", json!({"text": "t", "pinned": false, "extra": 1})),
            memo_body("edit", json!({"text": "t", "base_op_id": "not-an-op"})),
            memo_body("pin", json!({"pinned": "yes"})),
            memo_body("note", json!([])),
            memo_body("delete", json!({"why": "x"})),
            memo_body("rename", json!({"title": "t"})),
            memo_body("pin", json!({"pinned": 1.5})),
            conv("set_active", json!({"leaf": "../x"})),
            conv(
                "msg.add",
                json!({"conv_id": "conv-2", "msg_id": "m", "role": "user", "content": "", "attachments": [], "ts": 1}),
            ),
            conv(
                "msg.add",
                json!({"conv_id": "conv-1", "msg_id": "m", "role": "user", "content": "", "attachments": {}, "ts": 1}),
            ),
            conv("msg.add", json!({"conv_id": "conv-1", "msg_id": "m", "role": "", "content": "", "attachments": [], "ts": 1})),
            conv(
                "msg.add",
                json!({"conv_id": "conv-1", "msg_id": "m", "role": "u", "content": "", "attachments": [], "ts": -1}),
            ),
            tr("tr.add", json!({"pinned": "no"})),
            tr("tr.add", json!("text")),
            tr("edit", json!({})),
            OpBody { kind: Kind::Policy, ..memo_body("policy.retention", json!(null)) },
        ];
        for body in &bad {
            assert!(matches!(body.validate(), Err(OpError::Schema(_) | OpError::Float)), "{body:?}");
        }

        let mut envelope_bad = memo_body("delete", json!({}));
        envelope_bad.op_id = uuid::Uuid::new_v4().to_string();
        assert!(envelope_bad.validate().is_err(), "op ids are UUIDv7");
        envelope_bad.op_id = oid(1).to_uppercase();
        assert!(envelope_bad.validate().is_err(), "op ids are canonical lowercase");
        let mut obs_bad = memo_body("delete", json!({}));
        obs_bad.obs = vec![oid(3), oid(2)];
        assert!(obs_bad.validate().is_err(), "obs must be sorted");
        let mut future = memo_body("delete", json!({}));
        future.v = 2;
        assert_eq!(future.validate(), Err(OpError::FutureVersion(2)));
    }

    /// G8: `src_id` is an optional string on conv `create` and `msg.add`
    /// (and free-form on `tr.add`); rename stays strict and other unknown
    /// keys are still refused.
    #[test]
    fn src_id_is_optional_on_creates_and_other_unknown_fields_stay_refused() {
        let conv = |op_type: &str, payload: Value| OpBody {
            kind: Kind::Conv,
            entity_id: "conv-1".to_owned(),
            ..memo_body(op_type, payload)
        };
        let msg = |extra: Value| {
            let mut p = json!({"conv_id": "conv-1", "msg_id": "msg-1", "role": "user", "content": "hi", "attachments": [], "ts": 5});
            for (k, v) in extra.as_object().unwrap() {
                p[k] = v.clone();
            }
            conv("msg.add", p)
        };
        let tr = |payload: Value| OpBody { kind: Kind::Tr, ..memo_body("tr.add", payload) };
        for body in [
            conv("create", json!({"title": "t"})),
            conv("create", json!({"title": "t", "src_id": "c-1"})),
            conv("create", json!({"title": "t", "src_id": null})),
            msg(json!({})),
            msg(json!({"src_id": "m-1"})),
            tr(json!({"source": "a", "src_id": "t-1"})),
        ] {
            assert_eq!(body.validate(), Ok(()), "{body:?}");
        }
        for body in [
            conv("create", json!({"title": "t", "src_id": 7})),
            conv("create", json!({"title": "t", "extra": "x"})),
            conv("rename", json!({"title": "t", "src_id": "c-1"})),
            msg(json!({"src_id": ["m-1"]})),
            msg(json!({"source_id": "m-1"})),
        ] {
            assert!(matches!(body.validate(), Err(OpError::Schema(_))), "{body:?}");
        }
    }

    fn segment() -> Segment {
        let op = |seq: u64, n: u64| SegOp {
            seq,
            body: OpBody { op_id: oid(n), ..memo_body("pin", json!({"pinned": seq % 2 == 0})) },
        };
        Segment {
            v: 1,
            device_id: "dev-a".to_owned(),
            first_seq: 5,
            last_seq: 6,
            prev_seg_hash: "ab".repeat(32),
            ops: vec![op(5, 10), op(6, 11)],
        }
    }

    #[test]
    fn segment_round_trips_canonically_and_rejects_non_canonical_bytes() {
        let seg = segment();
        seg.validate().unwrap();
        let bytes = seg.encode().unwrap();
        assert_eq!(Segment::decode(&bytes).unwrap(), seg);
        assert_eq!(segment_hash(&seg).unwrap(), sha256(&bytes));

        let pretty = serde_json::to_vec_pretty(&seg).unwrap();
        assert_eq!(Segment::decode(&pretty), Err(OpError::NotCanonical));
        let text = String::from_utf8(bytes.clone()).unwrap();
        let duplicated = text.replacen("{\"device_id\"", "{\"device_id\":\"dev-z\",\"device_id\"", 1);
        assert_eq!(Segment::decode(duplicated.as_bytes()), Err(OpError::NotCanonical));
        let extra = text.replacen("{\"device_id\"", "{\"aaa\":1,\"device_id\"", 1);
        assert!(matches!(Segment::decode(extra.as_bytes()), Err(OpError::Schema(_))));
        assert!(matches!(Segment::decode(b"not json"), Err(OpError::Schema(_))));
        let float = text.replacen("\"first_seq\":5", "\"first_seq\":5.0", 1);
        assert!(Segment::decode(float.as_bytes()).is_err());
    }

    #[test]
    fn future_versions_are_reported_before_the_strict_field_check() {
        let mut seg = segment();
        seg.v = 2;
        assert_eq!(Segment::decode(&seg.encode().unwrap()), Err(OpError::FutureVersion(2)));
        let mut seg = segment();
        seg.ops[1].body.v = 3;
        let mut value = to_value(&seg).unwrap();
        value["ops"][1]["body"]["new_field"] = json!(true);
        assert_eq!(Segment::decode(&canonical_json(&value).unwrap()), Err(OpError::FutureVersion(3)));
        // A newer format may use encodings v1 rejects (a float, whitespace):
        // still a future version, never a canonical-check failure.
        let mut value = to_value(&segment()).unwrap();
        value["v"] = json!(2);
        let mut bytes = canonical_json(&value).unwrap();
        bytes.insert(1, b' ');
        assert_eq!(Segment::decode(&bytes), Err(OpError::FutureVersion(2)));
        let mut value = to_value(&segment()).unwrap();
        value["ops"][0]["body"]["v"] = json!(2);
        value["ops"][0]["body"]["payload"]["weight"] = json!(0.5);
        let text = serde_json::to_vec(&value).unwrap();
        assert_eq!(Segment::decode(&text), Err(OpError::FutureVersion(2)));
    }

    #[test]
    fn plaintext_cross_check_enforces_device_range_and_contiguous_seqs() {
        let seg = segment();
        assert_eq!(check_plaintext(&seg, "dev-a", 5, 6), Ok(()));
        assert!(matches!(check_plaintext(&seg, "dev-b", 5, 6), Err(OpError::CrossCheck(_))));
        assert!(matches!(check_plaintext(&seg, "dev-a", 5, 7), Err(OpError::CrossCheck(_))));
        assert!(matches!(check_plaintext(&seg, "dev-a", 4, 6), Err(OpError::CrossCheck(_))));
        let mut gap = seg.clone();
        gap.ops[1].seq = 7;
        assert!(matches!(check_plaintext(&gap, "dev-a", 5, 6), Err(OpError::CrossCheck(_))));
        let mut short = seg.clone();
        short.ops.pop();
        assert!(matches!(check_plaintext(&short, "dev-a", 5, 6), Err(OpError::CrossCheck(_))));
        let mut swapped = seg.clone();
        swapped.ops.swap(0, 1);
        assert!(matches!(check_plaintext(&swapped, "dev-a", 5, 6), Err(OpError::CrossCheck(_))));
        let mut zero = seg;
        zero.first_seq = 0;
        assert!(matches!(check_plaintext(&zero, "dev-a", 0, 6), Err(OpError::CrossCheck(_))));
    }

    #[test]
    fn segment_schema_rejects_bad_hash_and_bad_bodies() {
        let mut seg = segment();
        seg.prev_seg_hash = "AB".repeat(32);
        assert!(seg.validate().is_err());
        let mut seg = segment();
        seg.ops[0].body.payload = json!({"pinned": "x"});
        assert!(seg.validate().is_err());
    }

    #[test]
    fn integers_sqlite_cannot_store_are_a_typed_range_error() {
        let max = i64::MAX as u64;
        let mut body = memo_body("pin", json!({"pinned": true}));
        body.hlc.wall_ms = max;
        assert_eq!(body.validate(), Ok(()), "i64::MAX itself is storable");
        body.hlc.wall_ms = max + 1;
        assert!(matches!(body.validate(), Err(OpError::Range(_))));
        // ctx values share the same bound, at the edge and beyond it.
        let mut body = memo_body("pin", json!({"pinned": true}));
        body.ctx = BTreeMap::from([("dev-b".to_owned(), max)]);
        assert_eq!(body.validate(), Ok(()), "a ctx value of i64::MAX is storable");
        for big in [max + 1, u64::MAX] {
            body.ctx = BTreeMap::from([("dev-a".to_owned(), 1), ("dev-b".to_owned(), big)]);
            assert!(matches!(body.validate(), Err(OpError::Range(_))), "ctx {big}");
        }
        let mut seg = segment();
        seg.ops[1].body.ctx.insert("dev-c".to_owned(), max + 1);
        assert!(matches!(seg.validate(), Err(OpError::Range(_))), "a bad ctx value fails the segment");

        let mut seg = segment();
        seg.ops[1].body.hlc.wall_ms = u64::MAX;
        assert!(matches!(seg.validate(), Err(OpError::Range(_))), "a bad op body fails the segment");
        for (first, last, seqs) in [(max + 1, max + 1, [max + 1, max + 1]), (5, max + 1, [5, 6]), (5, 6, [5, max + 1])] {
            let mut seg = segment();
            (seg.first_seq, seg.last_seq) = (first, last);
            seg.ops[0].seq = seqs[0];
            seg.ops[1].seq = seqs[1];
            assert!(matches!(seg.validate(), Err(OpError::Range(_))), "{first}-{last} {seqs:?}");
        }
        let mut seg = segment();
        (seg.first_seq, seg.last_seq, seg.ops[0].seq, seg.ops[1].seq) = (max - 1, max, max - 1, max);
        assert_eq!(seg.validate(), Ok(()));
    }

    #[test]
    fn plaintext_cross_check_never_overflows_at_u64_max() {
        let mut seg = segment();
        seg.ops.truncate(1);
        (seg.first_seq, seg.last_seq, seg.ops[0].seq) = (u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(check_plaintext(&seg, "dev-a", u64::MAX, u64::MAX), Ok(()));
        assert!(matches!(seg.validate(), Err(OpError::Range(_))), "validation still rejects it");
        let mut two = segment();
        (two.first_seq, two.last_seq, two.ops[0].seq, two.ops[1].seq) = (u64::MAX - 1, u64::MAX, u64::MAX - 1, u64::MAX);
        assert_eq!(check_plaintext(&two, "dev-a", u64::MAX - 1, u64::MAX), Ok(()));
        two.ops[1].seq = 0;
        assert!(matches!(check_plaintext(&two, "dev-a", u64::MAX - 1, u64::MAX), Err(OpError::CrossCheck(_))));
        two.ops.push(two.ops[0].clone());
        assert!(matches!(check_plaintext(&two, "dev-a", u64::MAX - 1, u64::MAX), Err(OpError::CrossCheck(_))));
    }
}
