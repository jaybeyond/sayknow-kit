//! Message-level state machine for one Cursor turn.
//!
//! Deliberately transport-free: it takes a decoded `AgentServerMessage` and
//! returns the actions the driver must perform. That keeps the protocol logic
//! testable without a socket, a TLS stack, or a live account.
//!
//! Mirrors sayknow-cli `packages/ai/src/providers/cursor.ts`
//! (`handleServerMessage` :625-657, `handleKvServerMessage` :659-710,
//! `handleExecServerMessage` :1003-1033, `processInteractionUpdate`
//! :1990-2134), copied 2026-09-19.

use super::pb;
use super::request::BlobStore;

/// What the driver should do with a server message.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write this client message back on the same stream.
    Send(pb::AgentClientMessage),
    /// Run an exec request; the driver answers asynchronously.
    Exec(pb::ExecServerMessage),
    /// Assistant text arrived.
    Delta(String),
    /// Model reasoning text arrived.
    Thinking(String),
    /// A tool call started or finished; carries a display label.
    Tool { call_id: String, name: String, completed: bool },
    /// The turn is over.
    Done,
}

/// Accumulated result of a turn.
#[derive(Debug, Default, Clone)]
pub struct TurnOutput {
    pub text: String,
    pub tokens: i32,
    pub done: bool,
}

/// Per-turn protocol state.
pub struct Session {
    blobs: BlobStore,
    output: TurnOutput,
}

impl Session {
    pub fn new(blobs: BlobStore) -> Self {
        Self {
            blobs,
            output: TurnOutput::default(),
        }
    }

    #[cfg(test)]
    pub fn output(&self) -> &TurnOutput {
        &self.output
    }

    pub fn into_output(self) -> TurnOutput {
        self.output
    }

    /// The heartbeat the driver sends every 5s to keep the stream open.
    pub fn heartbeat() -> pb::AgentClientMessage {
        pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::ClientHeartbeat(
                pb::ClientHeartbeat {},
            )),
        }
    }

    /// Handle one decoded server message.
    pub fn handle(&mut self, msg: pb::AgentServerMessage) -> Vec<Action> {
        let Some(message) = msg.message else {
            return Vec::new();
        };

        match message {
            pb::agent_server_message::Message::InteractionUpdate(update) => {
                self.interaction_update(update)
            }
            pb::agent_server_message::Message::KvServerMessage(kv) => self.kv(kv),
            pb::agent_server_message::Message::ExecServerMessage(exec) => self.exec(exec),
            // The checkpoint mirrors server-side state we deliberately do not
            // cache: history is rebuilt from the caller's messages each turn.
            pb::agent_server_message::Message::ConversationCheckpointUpdate(_) => Vec::new(),
            pb::agent_server_message::Message::ExecServerControlMessage(_) => Vec::new(),
            pb::agent_server_message::Message::InteractionQuery(_) => Vec::new(),
        }
    }

    fn interaction_update(&mut self, update: pb::InteractionUpdate) -> Vec<Action> {
        use pb::interaction_update::Message as U;

        let Some(message) = update.message else {
            return Vec::new();
        };

        match message {
            U::TextDelta(delta) => {
                if delta.text.is_empty() {
                    return Vec::new();
                }
                self.output.text.push_str(&delta.text);
                vec![Action::Delta(delta.text)]
            }
            U::ThinkingDelta(delta) => {
                if delta.text.is_empty() {
                    Vec::new()
                } else {
                    vec![Action::Thinking(delta.text)]
                }
            }
            U::TokenDelta(delta) => {
                self.output.tokens += delta.tokens;
                Vec::new()
            }
            U::ToolCallStarted(started) => vec![Action::Tool {
                call_id: started.call_id,
                name: tool_call_name(started.tool_call.as_ref()),
                completed: false,
            }],
            U::ToolCallCompleted(completed) => vec![Action::Tool {
                call_id: completed.call_id,
                name: tool_call_name(completed.tool_call.as_ref()),
                completed: true,
            }],
            // Only `turnEnded` ends a turn; a stop reason is not a reliable
            // completion signal.
            U::TurnEnded(_) => {
                self.output.done = true;
                vec![Action::Done]
            }
            _ => Vec::new(),
        }
    }

    /// Answer the server's blob reads and writes.
    ///
    /// A `getBlobArgs` for an unknown id is answered with an empty result
    /// rather than ignored: silence stalls the turn.
    fn kv(&mut self, kv: pb::KvServerMessage) -> Vec<Action> {
        use pb::kv_server_message::Message as K;

        let id = kv.id;
        let Some(message) = kv.message else {
            return Vec::new();
        };

        let reply = match message {
            K::GetBlobArgs(args) => {
                let blob_data = self.blobs.get(&args.blob_id).cloned();
                pb::kv_client_message::Message::GetBlobResult(pb::GetBlobResult { blob_data })
            }
            K::SetBlobArgs(args) => {
                self.blobs.insert(args.blob_id, args.blob_data);
                pb::kv_client_message::Message::SetBlobResult(pb::SetBlobResult { error: None })
            }
        };

        vec![Action::Send(pb::AgentClientMessage {
            message: Some(pb::agent_client_message::Message::KvClientMessage(
                pb::KvClientMessage {
                    id,
                    message: Some(reply),
                },
            )),
        })]
    }

    /// The context handshake is answered here; everything else is handed to
    /// the driver, which owns the filesystem and process work.
    fn exec(&mut self, exec: pb::ExecServerMessage) -> Vec<Action> {
        use pb::exec_server_message::Message as E;

        match exec.message {
            Some(E::RequestContextArgs(_)) => {
                vec![Action::Send(request_context_reply(exec.id, exec.exec_id))]
            }
            Some(_) => vec![Action::Exec(exec)],
            None => Vec::new(),
        }
    }
}

/// Empty-but-present context. Cursor asks for it before running a turn and
/// waits for the answer; the app advertises no rules, repos, or MCP tools.
pub fn request_context_reply(id: u32, exec_id: String) -> pb::AgentClientMessage {
    let result = pb::RequestContextResult {
        result: Some(pb::request_context_result::Result::Success(
            pb::RequestContextSuccess {
                request_context: Some(pb::RequestContext::default()),
            },
        )),
    };

    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                exec_id,
                message: Some(pb::exec_client_message::Message::RequestContextResult(result)),
            },
        )),
    }
}

fn tool_call_name(tool_call: Option<&pb::ToolCall>) -> String {
    let Some(call) = tool_call else {
        return "tool".to_string();
    };
    let Some(kind) = call.tool.as_ref() else {
        return "tool".to_string();
    };

    // `{:?}` on the oneof yields `VariantName(..)`; the variant name is the
    // tool identity and is stable, unlike any hand-kept mapping table.
    let debug = format!("{kind:?}");
    let name = debug.split('(').next().unwrap_or("tool");
    let mut out = String::with_capacity(name.len());
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out.trim_end_matches("_tool_call").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::request::BlobStore;

    fn server(message: pb::agent_server_message::Message) -> pb::AgentServerMessage {
        pb::AgentServerMessage {
            message: Some(message),
        }
    }

    fn update(message: pb::interaction_update::Message) -> pb::AgentServerMessage {
        server(pb::agent_server_message::Message::InteractionUpdate(
            pb::InteractionUpdate {
                message: Some(message),
            },
        ))
    }

    #[test]
    fn text_deltas_accumulate_and_stream() {
        let mut session = Session::new(BlobStore::new());

        let first = session.handle(update(pb::interaction_update::Message::TextDelta(
            pb::TextDeltaUpdate {
                text: "Hello ".into(),
            },
        )));
        let second = session.handle(update(pb::interaction_update::Message::TextDelta(
            pb::TextDeltaUpdate {
                text: "world".into(),
            },
        )));

        assert_eq!(first, vec![Action::Delta("Hello ".into())]);
        assert_eq!(second, vec![Action::Delta("world".into())]);
        assert_eq!(session.output().text, "Hello world");
        assert!(!session.output().done);
    }

    #[test]
    fn only_turn_ended_finishes_the_turn() {
        let mut session = Session::new(BlobStore::new());

        session.handle(update(pb::interaction_update::Message::TokenDelta(
            pb::TokenDeltaUpdate { tokens: 12 },
        )));
        assert!(!session.output().done);

        let actions = session.handle(update(pb::interaction_update::Message::TurnEnded(
            pb::TurnEndedUpdate {},
        )));
        assert_eq!(actions, vec![Action::Done]);
        assert!(session.output().done);
        assert_eq!(session.output().tokens, 12);
    }

    #[test]
    fn known_blob_requests_are_answered_with_the_bytes() {
        let mut blobs = BlobStore::new();
        let id = blobs.put(b"prompt bytes".to_vec());
        let mut session = Session::new(blobs);

        let actions = session.handle(server(
            pb::agent_server_message::Message::KvServerMessage(pb::KvServerMessage {
                id: 7,
                message: Some(pb::kv_server_message::Message::GetBlobArgs(
                    pb::GetBlobArgs {
                        blob_id: id.clone(),
                    },
                )),
                ..Default::default()
            }),
        ));

        let Action::Send(reply) = &actions[0] else {
            panic!("expected a reply");
        };
        let Some(pb::agent_client_message::Message::KvClientMessage(kv)) = &reply.message else {
            panic!("expected a kv reply");
        };
        assert_eq!(kv.id, 7, "the reply must echo the request id");
        let Some(pb::kv_client_message::Message::GetBlobResult(result)) = &kv.message else {
            panic!("expected a get-blob result");
        };
        assert_eq!(result.blob_data.as_deref(), Some(&b"prompt bytes"[..]));
    }

    #[test]
    fn unknown_blob_requests_are_still_answered() {
        let mut session = Session::new(BlobStore::new());

        let actions = session.handle(server(
            pb::agent_server_message::Message::KvServerMessage(pb::KvServerMessage {
                id: 3,
                message: Some(pb::kv_server_message::Message::GetBlobArgs(
                    pb::GetBlobArgs {
                        blob_id: b"nope".to_vec(),
                    },
                )),
                ..Default::default()
            }),
        ));

        let Action::Send(reply) = &actions[0] else {
            panic!("an unanswered blob request stalls the turn");
        };
        let Some(pb::agent_client_message::Message::KvClientMessage(kv)) = &reply.message else {
            panic!("expected a kv reply");
        };
        let Some(pb::kv_client_message::Message::GetBlobResult(result)) = &kv.message else {
            panic!("expected a get-blob result");
        };
        assert_eq!(result.blob_data, None);
    }

    #[test]
    fn set_blob_is_stored_and_acknowledged() {
        let mut session = Session::new(BlobStore::new());

        session.handle(server(pb::agent_server_message::Message::KvServerMessage(
            pb::KvServerMessage {
                id: 1,
                message: Some(pb::kv_server_message::Message::SetBlobArgs(
                    pb::SetBlobArgs {
                        blob_id: b"id".to_vec(),
                        blob_data: b"data".to_vec(),
                    },
                )),
                ..Default::default()
            },
        )));

        let actions = session.handle(server(
            pb::agent_server_message::Message::KvServerMessage(pb::KvServerMessage {
                id: 2,
                message: Some(pb::kv_server_message::Message::GetBlobArgs(
                    pb::GetBlobArgs {
                        blob_id: b"id".to_vec(),
                    },
                )),
                ..Default::default()
            }),
        ));

        let Action::Send(reply) = &actions[0] else {
            panic!("expected a reply");
        };
        let Some(pb::agent_client_message::Message::KvClientMessage(kv)) = &reply.message else {
            panic!("expected a kv reply");
        };
        let Some(pb::kv_client_message::Message::GetBlobResult(result)) = &kv.message else {
            panic!("expected a get-blob result");
        };
        assert_eq!(result.blob_data.as_deref(), Some(&b"data"[..]));
    }

    #[test]
    fn the_context_handshake_is_answered_locally() {
        let mut session = Session::new(BlobStore::new());

        let actions = session.handle(server(
            pb::agent_server_message::Message::ExecServerMessage(pb::ExecServerMessage {
                id: 9,
                exec_id: "exec-1".into(),
                message: Some(pb::exec_server_message::Message::RequestContextArgs(
                    pb::RequestContextArgs::default(),
                )),
                ..Default::default()
            }),
        ));

        let Action::Send(reply) = &actions[0] else {
            panic!("the handshake must be answered on the stream");
        };
        let Some(pb::agent_client_message::Message::ExecClientMessage(exec)) = &reply.message else {
            panic!("expected an exec reply");
        };
        assert_eq!(exec.id, 9);
        assert_eq!(exec.exec_id, "exec-1");
        let Some(pb::exec_client_message::Message::RequestContextResult(result)) = &exec.message
        else {
            panic!("expected a request-context result");
        };
        assert!(matches!(
            result.result,
            Some(pb::request_context_result::Result::Success(_))
        ));
    }

    #[test]
    fn other_exec_requests_are_handed_to_the_driver() {
        let mut session = Session::new(BlobStore::new());

        let actions = session.handle(server(
            pb::agent_server_message::Message::ExecServerMessage(pb::ExecServerMessage {
                id: 4,
                exec_id: "exec-2".into(),
                message: Some(pb::exec_server_message::Message::ReadArgs(pb::ReadArgs {
                    path: "/tmp/x".into(),
                    ..Default::default()
                })),
                ..Default::default()
            }),
        ));

        assert!(matches!(actions.as_slice(), [Action::Exec(_)]));
    }

    #[test]
    fn heartbeat_is_a_client_heartbeat() {
        let hb = Session::heartbeat();
        assert!(matches!(
            hb.message,
            Some(pb::agent_client_message::Message::ClientHeartbeat(_))
        ));
    }

    /// End-to-end at the protocol level, without a socket: a recorded server
    /// frame sequence is pushed through the deframer and the session, and the
    /// bytes the client would have written back are checked.
    #[test]
    fn a_recorded_turn_replays_into_text_and_the_right_replies() {
        use crate::cursor::frame::{frame, Deframer};
        use prost::Message as _;

        let mut blobs = BlobStore::new();
        let prompt_id = blobs.put(b"the system prompt".to_vec());
        let mut session = Session::new(blobs);

        let recorded = [
            // 1. the context handshake
            server(pb::agent_server_message::Message::ExecServerMessage(
                pb::ExecServerMessage {
                    id: 1,
                    exec_id: "ctx".into(),
                    message: Some(pb::exec_server_message::Message::RequestContextArgs(
                        pb::RequestContextArgs::default(),
                    )),
                    ..Default::default()
                },
            )),
            // 2. the server pulling the prompt blob back
            server(pb::agent_server_message::Message::KvServerMessage(
                pb::KvServerMessage {
                    id: 2,
                    message: Some(pb::kv_server_message::Message::GetBlobArgs(
                        pb::GetBlobArgs {
                            blob_id: prompt_id.clone(),
                        },
                    )),
                    ..Default::default()
                },
            )),
            // 3-4. two text deltas
            update(pb::interaction_update::Message::TextDelta(
                pb::TextDeltaUpdate {
                    text: "Streamed ".into(),
                },
            )),
            update(pb::interaction_update::Message::TextDelta(
                pb::TextDeltaUpdate {
                    text: "answer".into(),
                },
            )),
            // 5. end of turn
            update(pb::interaction_update::Message::TurnEnded(
                pb::TurnEndedUpdate {},
            )),
        ];

        // Serialize the whole conversation into one byte stream, then split it
        // at an arbitrary point so the deframer has to reassemble.
        let mut wire = Vec::new();
        for message in &recorded {
            wire.extend_from_slice(&frame(&message.encode_to_vec(), 0));
        }
        let split = wire.len() / 3;

        let mut deframer = Deframer::new();
        deframer.push(&wire[..split]);
        deframer.push(&wire[split..]);

        let mut written: Vec<pb::AgentClientMessage> = Vec::new();
        let mut deltas: Vec<String> = Vec::new();
        let mut done = false;

        while let Some(f) = deframer.next_frame() {
            let message = pb::AgentServerMessage::decode(f.payload.as_slice()).expect("decodes");
            for action in session.handle(message) {
                match action {
                    Action::Send(reply) => written.push(reply),
                    Action::Delta(text) => deltas.push(text),
                    Action::Done => done = true,
                    _ => {}
                }
            }
        }

        // What the driver sends on its timer, alongside the replies above.
        written.push(Session::heartbeat());

        assert!(done, "the turn must end on turnEnded");
        assert_eq!(deltas, vec!["Streamed ".to_string(), "answer".to_string()]);
        assert_eq!(session.output().text, "Streamed answer");

        let has_context_reply = written.iter().any(|m| {
            matches!(
                &m.message,
                Some(pb::agent_client_message::Message::ExecClientMessage(exec))
                    if matches!(
                        exec.message,
                        Some(pb::exec_client_message::Message::RequestContextResult(_))
                    )
            )
        });
        assert!(has_context_reply, "the handshake must be answered");

        let blob_reply = written.iter().find_map(|m| match &m.message {
            Some(pb::agent_client_message::Message::KvClientMessage(kv)) => match &kv.message {
                Some(pb::kv_client_message::Message::GetBlobResult(result)) => Some(result),
                _ => None,
            },
            _ => None,
        });
        assert_eq!(
            blob_reply.and_then(|r| r.blob_data.as_deref()),
            Some(&b"the system prompt"[..]),
            "the server's blob read must be answered with the stored bytes"
        );

        let heartbeats = written
            .iter()
            .filter(|m| {
                matches!(
                    m.message,
                    Some(pb::agent_client_message::Message::ClientHeartbeat(_))
                )
            })
            .count();
        assert_eq!(heartbeats, 1, "the stream is kept alive by heartbeats");
    }
}
