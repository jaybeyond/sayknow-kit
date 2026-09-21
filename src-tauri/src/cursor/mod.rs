//! Cursor agent protocol.
//!
//! Cursor is not an OpenAI-compatible endpoint: it speaks Connect-RPC with
//! protobuf payloads over a bidirectional HTTP/2 stream. The pieces are split
//! so the protocol logic can be tested without a socket or a live account:
//!
//! - [`frame`]   Connect framing, pure functions
//! - [`request`] building the opening `AgentRunRequest` and its blob store
//! - [`session`] the message-level state machine
//! - [`exec`]    running the agent's tool requests on this machine
//! - [`transport`] the h2 + TLS plumbing
//! - [`audit`]   the record of what the agent actually did
//!
//! Wire details are ported from sayknow-cli
//! `packages/ai/src/providers/cursor.ts` (copied 2026-09-19). sayknow-cli
//! itself is never modified; when it changes, the citations in these files are
//! the map back to the source.

pub mod audit;
pub mod exec;
pub mod frame;
pub mod request;
pub mod session;
pub mod transport;

/// Generated from `proto/agent.proto` by `build.rs`.
#[allow(clippy::all, clippy::pedantic, dead_code, unused_qualifications)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/agent.v1.rs"));
}
