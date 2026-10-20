//! End-to-end encrypted sync to user-owned storage.
//!
//! Everything that leaves the device is sealed here first: memos, AI
//! conversations (with their images), and translation history are encrypted
//! with a vault data key (DEK) that never reaches the webview or any server we
//! operate. A new device joins with the user's recovery key (RK).
//!
//! The format is fixed by the approved plan (§4.6): XChaCha20-Poly1305
//! envelopes, HKDF-SHA256 subkeys bound to the vault id and key epoch, and an
//! HMAC-authenticated manifest that carries the RK-wrapped DEK.

pub(crate) mod causal;
pub(crate) mod commands;
pub(crate) mod compaction;
pub(crate) mod crypto;
pub(crate) mod engine;
pub(crate) mod envelope;
pub(crate) mod identity;
pub(crate) mod memo_bridge;
pub(crate) mod merge;
pub(crate) mod migration;
pub(crate) mod oauth;
pub(crate) mod ops;
pub(crate) mod provider;
pub(crate) mod publish;
pub(crate) mod recovery;
#[cfg(test)] mod redteam_tests;
#[cfg(test)] mod redteam_g2_tests;
#[cfg(test)] mod redteam_g3_tests;
#[cfg(test)] mod redteam_g4_tests;
#[cfg(test)] mod redteam_g5_tests;
#[cfg(test)] mod redteam_g6_tests;
#[cfg(test)] mod redteam_g7_tests;
#[cfg(test)] mod redteam_g9_tests;
#[cfg(test)] mod redteam_g10_tests;
pub(crate) mod rotation;
pub(crate) mod secrets;
#[cfg(test)] mod sim3;
#[cfg(test)] mod sim_tests;
#[cfg(test)] mod live_tests;
pub(crate) mod store;
pub(crate) mod telemetry;
pub(crate) mod vault;
