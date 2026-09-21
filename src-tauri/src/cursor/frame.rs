//! Connect streaming framing.
//!
//! Every message on the wire is `flags | u32 big-endian length | payload`.
//! The flag bit `0b10` marks the end-stream frame, whose payload is JSON
//! rather than protobuf and carries the error, if any.
//!
//! Ported from sayknow-cli `packages/ai/src/providers/cursor.ts`
//! (`frameConnectMessage` :208-214, `parseConnectEndStream` :216-229, and the
//! read loop at :447-499), copied 2026-09-19.

pub const FLAG_END_STREAM: u8 = 0b0000_0010;
pub const FLAG_COMPRESSED: u8 = 0b0000_0001;

/// Wrap a payload in a Connect frame.
pub fn frame(payload: &[u8], flags: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn is_end_stream(&self) -> bool {
        self.flags & FLAG_END_STREAM != 0
    }

    pub fn is_compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
    }
}

/// Accumulates socket chunks and yields whole frames.
///
/// HTTP/2 data frames have nothing to do with Connect frames: one read can
/// carry several messages, or half of one. Both cases are normal.
#[derive(Debug, Default)]
pub struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Next complete frame, or `None` while the buffer holds a partial one.
    pub fn next_frame(&mut self) -> Option<Frame> {
        if self.buf.len() < 5 {
            return None;
        }
        let flags = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < 5 + len {
            return None;
        }
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Some(Frame { flags, payload })
    }

    /// Bytes held back because they do not yet form a whole frame.
    #[cfg(test)]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Error reported by the end-stream frame, if it carries one.
///
/// The payload is JSON: `{"error":{"code":"...","message":"..."}}`. A frame
/// without an `error` key is a clean end of stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectError {
    pub code: String,
    pub message: String,
}

impl ConnectError {
    /// Whether this error means the credentials, not the request, are the
    /// problem — the caller turns this into a re-login prompt.
    pub fn is_auth(&self) -> bool {
        matches!(
            self.code.as_str(),
            "unauthenticated" | "permission_denied" | "unauthorized"
        )
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Connect error {}: {}", self.code, self.message)
    }
}

/// Parse an end-stream payload.
///
/// Returns `Ok(None)` for a clean close, `Ok(Some(err))` for a reported error,
/// and `Err` when the payload is not the JSON the protocol promises.
pub fn parse_end_stream(payload: &[u8]) -> Result<Option<ConnectError>, String> {
    if payload.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| "Failed to parse Connect end stream".to_string())?;
    let Some(error) = value.get("error") else {
        return Ok(None);
    };
    let code = error
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error")
        .to_string();
    Ok(Some(ConnectError { code, message }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_flags_and_big_endian_length() {
        let framed = frame(b"hello", 0);
        assert_eq!(framed[0], 0);
        assert_eq!(&framed[1..5], &[0, 0, 0, 5]);
        assert_eq!(&framed[5..], b"hello");
    }

    #[test]
    fn deframes_two_messages_from_one_chunk() {
        let mut chunk = frame(b"one", 0);
        chunk.extend_from_slice(&frame(b"two", FLAG_END_STREAM));

        let mut d = Deframer::new();
        d.push(&chunk);

        let first = d.next_frame().expect("first frame");
        assert_eq!(first.payload, b"one");
        assert!(!first.is_end_stream());

        let second = d.next_frame().expect("second frame");
        assert_eq!(second.payload, b"two");
        assert!(second.is_end_stream());

        assert!(d.next_frame().is_none());
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn deframes_a_message_split_across_chunks() {
        let framed = frame(b"split-me", 0);
        let mut d = Deframer::new();

        d.push(&framed[..3]);
        assert!(d.next_frame().is_none(), "header alone is not a frame");

        d.push(&framed[3..7]);
        assert!(d.next_frame().is_none(), "partial payload is not a frame");

        d.push(&framed[7..]);
        assert_eq!(d.next_frame().expect("frame").payload, b"split-me");
    }

    #[test]
    fn zero_length_frame_is_a_frame() {
        let mut d = Deframer::new();
        d.push(&frame(b"", FLAG_END_STREAM));
        let f = d.next_frame().expect("frame");
        assert!(f.payload.is_empty());
        assert!(f.is_end_stream());
    }

    #[test]
    fn end_stream_without_error_is_a_clean_close() {
        assert_eq!(parse_end_stream(b"{}"), Ok(None));
        assert_eq!(parse_end_stream(b""), Ok(None));
    }

    #[test]
    fn end_stream_error_is_reported_with_code_and_message() {
        let err = parse_end_stream(br#"{"error":{"code":"unauthenticated","message":"token expired"}}"#)
            .expect("parsed")
            .expect("error");
        assert_eq!(err.code, "unauthenticated");
        assert_eq!(err.message, "token expired");
        assert!(err.is_auth());
    }

    #[test]
    fn end_stream_error_without_fields_falls_back() {
        let err = parse_end_stream(br#"{"error":{}}"#).expect("parsed").expect("error");
        assert_eq!(err.code, "unknown");
        assert_eq!(err.message, "Unknown error");
        assert!(!err.is_auth());
    }

    #[test]
    fn malformed_end_stream_is_an_error_not_a_clean_close() {
        assert!(parse_end_stream(b"not json").is_err());
    }
}
