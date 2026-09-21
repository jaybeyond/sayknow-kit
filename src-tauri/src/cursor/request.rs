//! Builds the `AgentRunRequest` Cursor expects at the head of a turn.
//!
//! Cursor does not take the prompt inline. Conversation history and system
//! prompts are stored as blobs, and only their ids travel in the request; the
//! server then asks for the bytes over the same stream. This mirrors
//! sayknow-cli `packages/ai/src/providers/cursor.ts` (`buildGrpcRequest`
//! :2530-2662, `buildRootPromptMessagesJson` :2337-2372,
//! `buildConversationTurns` :2382-2474, `createBlobId` :2156-2158), copied
//! 2026-09-19.

use std::collections::HashMap;

use prost::Message as _;
use sha2::{Digest, Sha256};

use super::pb;

/// Role of one chat message as the webview sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

/// Content-addressed store for the blobs the server pulls back.
#[derive(Debug, Default, Clone)]
pub struct BlobStore {
    blobs: HashMap<Vec<u8>, Vec<u8>>,
}

impl BlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `data` and return its id. The id is the sha256 of the content, so
    /// identical history across turns reuses the same id.
    pub fn put(&mut self, data: Vec<u8>) -> Vec<u8> {
        let id = blob_id(&data);
        self.blobs.insert(id.clone(), data);
        id
    }

    pub fn get(&self, id: &[u8]) -> Option<&Vec<u8>> {
        self.blobs.get(id)
    }

    pub fn insert(&mut self, id: Vec<u8>, data: Vec<u8>) {
        self.blobs.insert(id, data);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.blobs.len()
    }
}

/// sha256 of the blob contents.
pub fn blob_id(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// Stable UUID-shaped id derived from a content key, so unchanged history
/// hashes to the same blob ids across turns.
fn deterministic_message_id(key: &str) -> String {
    let hex = format!("{:x}", Sha256::digest(key.as_bytes()));
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn last_user_index(messages: &[ChatMessage]) -> Option<usize> {
    messages.iter().rposition(|m| m.role == Role::User)
}

/// One JSON blob per system prompt, so changing the last one does not
/// invalidate the cached prefix on the server.
fn system_prompt_jsons(messages: &[ChatMessage]) -> Vec<String> {
    let prompts: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.trim())
        .filter(|c| !c.is_empty())
        .collect();

    if prompts.is_empty() {
        return vec![
            serde_json::json!({ "role": "system", "content": "You are a helpful assistant." })
                .to_string(),
        ];
    }
    prompts
        .into_iter()
        .map(|content| serde_json::json!({ "role": "system", "content": content }).to_string())
        .collect()
}

/// `rootPromptMessagesJson`: the system prompts followed by every historical
/// message except the turn being sent. Cursor builds the model prompt from
/// this, not from `turns`.
fn root_prompt_messages(
    messages: &[ChatMessage],
    system_ids: &[Vec<u8>],
    blobs: &mut BlobStore,
) -> Vec<Vec<u8>> {
    let mut entries: Vec<Vec<u8>> = system_ids.to_vec();
    let stop = last_user_index(messages);

    for (i, msg) in messages.iter().enumerate() {
        if Some(i) == stop {
            break;
        }
        let json = match msg.role {
            Role::System => continue,
            Role::User => {
                if msg.content.trim().is_empty() {
                    continue;
                }
                serde_json::json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": msg.content }],
                })
            }
            Role::Assistant => {
                if msg.content.trim().is_empty() {
                    continue;
                }
                serde_json::json!({
                    "role": "assistant",
                    "content": [{ "type": "text", "text": msg.content }],
                })
            }
        };
        entries.push(blobs.put(json.to_string().into_bytes()));
    }

    entries
}

/// `turns`: the UI-side history view. Each turn is a user message blob plus
/// the assistant steps that answered it; the final user message is excluded
/// because it travels in the action instead.
fn conversation_turns(messages: &[ChatMessage], blobs: &mut BlobStore) -> Vec<Vec<u8>> {
    let mut turns = Vec::new();
    let last_user = last_user_index(messages);

    let mut i = 0usize;
    while i < messages.len() {
        if messages[i].role != Role::User {
            i += 1;
            continue;
        }
        if Some(i) == last_user {
            break;
        }

        let text = messages[i].content.clone();
        if text.trim().is_empty() {
            i += 1;
            continue;
        }

        let user_message = pb::UserMessage {
            text: text.clone(),
            message_id: deterministic_message_id(&format!("u:{}:{}", turns.len(), text)),
            ..Default::default()
        };
        let user_blob = blobs.put(user_message.encode_to_vec());

        let mut steps = Vec::new();
        i += 1;
        while i < messages.len() && messages[i].role != Role::User {
            if messages[i].role == Role::Assistant && !messages[i].content.trim().is_empty() {
                let step = pb::ConversationStep {
                    message: Some(pb::conversation_step::Message::AssistantMessage(
                        pb::AssistantMessage {
                            text: messages[i].content.clone(),
                        },
                    )),
                };
                steps.push(blobs.put(step.encode_to_vec()));
            }
            i += 1;
        }

        let turn = pb::ConversationTurnStructure {
            turn: Some(pb::conversation_turn_structure::Turn::AgentConversationTurn(
                pb::AgentConversationTurnStructure {
                    user_message: user_blob,
                    steps,
                    ..Default::default()
                },
            )),
        };
        turns.push(blobs.put(turn.encode_to_vec()));
    }

    turns
}

/// Everything the transport needs to open a turn.
pub struct RunRequest {
    /// Encoded `AgentClientMessage` carrying the run request.
    pub bytes: Vec<u8>,
    /// Blobs the server may ask for while the turn runs.
    pub blobs: BlobStore,
    pub conversation_id: String,
}

/// Build the opening request for one chat turn.
///
/// A fresh `conversation_id` is used per request: checkpoint caching is out of
/// scope, so history is re-sent every turn rather than resumed server-side.
pub fn build_run_request(model: &str, messages: &[ChatMessage]) -> RunRequest {
    let mut blobs = BlobStore::new();

    let system_ids: Vec<Vec<u8>> = system_prompt_jsons(messages)
        .into_iter()
        .map(|json| blobs.put(json.into_bytes()))
        .collect();

    let root_prompt_messages_json = root_prompt_messages(messages, &system_ids, &mut blobs);
    let turns = conversation_turns(messages, &mut blobs);

    let current = last_user_index(messages)
        .map(|i| messages[i].content.clone())
        .unwrap_or_default();

    let action = pb::ConversationAction {
        action: Some(if current.trim().is_empty() {
            pb::conversation_action::Action::ResumeAction(pb::ResumeAction::default())
        } else {
            pb::conversation_action::Action::UserMessageAction(pb::UserMessageAction {
                user_message: Some(pb::UserMessage {
                    text: current.clone(),
                    message_id: uuid::Uuid::new_v4().to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            })
        }),
    };

    let conversation_state = pb::ConversationStateStructure {
        root_prompt_messages_json,
        turns,
        ..Default::default()
    };

    let conversation_id = uuid::Uuid::new_v4().to_string();

    let run_request = pb::AgentRunRequest {
        conversation_state: Some(conversation_state),
        action: Some(action),
        model_details: Some(pb::ModelDetails {
            model_id: model.to_string(),
            display_model_id: model.to_string(),
            display_name: model.to_string(),
            ..Default::default()
        }),
        conversation_id: Some(conversation_id.clone()),
        ..Default::default()
    };

    let client_message = pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::RunRequest(run_request)),
    };

    RunRequest {
        bytes: client_message.encode_to_vec(),
        blobs,
        conversation_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
        }
    }

    #[test]
    fn blob_id_is_sha256_of_the_content() {
        let id = blob_id(b"cursor");
        assert_eq!(id.len(), 32);
        assert_eq!(id, Sha256::digest(b"cursor").to_vec());
    }

    #[test]
    fn identical_blobs_share_one_id() {
        let mut store = BlobStore::new();
        let a = store.put(b"same".to_vec());
        let b = store.put(b"same".to_vec());
        assert_eq!(a, b);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn stored_blobs_are_retrievable_by_id() {
        let mut store = BlobStore::new();
        let id = store.put(b"payload".to_vec());
        assert_eq!(store.get(&id).map(|v| v.as_slice()), Some(&b"payload"[..]));
        assert!(store.get(b"missing").is_none());
    }

    #[test]
    fn missing_system_prompt_falls_back_to_a_default() {
        let jsons = system_prompt_jsons(&[msg(Role::User, "hi")]);
        assert_eq!(jsons.len(), 1);
        assert!(jsons[0].contains("helpful assistant"));
    }

    #[test]
    fn each_system_prompt_gets_its_own_blob() {
        let jsons = system_prompt_jsons(&[
            msg(Role::System, "first"),
            msg(Role::System, "second"),
            msg(Role::User, "hi"),
        ]);
        assert_eq!(jsons.len(), 2);
        assert!(jsons[0].contains("first"));
        assert!(jsons[1].contains("second"));
    }

    #[test]
    fn current_user_turn_is_excluded_from_history() {
        let messages = vec![
            msg(Role::User, "first question"),
            msg(Role::Assistant, "first answer"),
            msg(Role::User, "second question"),
        ];
        let mut blobs = BlobStore::new();
        let entries = root_prompt_messages(&messages, &[], &mut blobs);

        let decoded: Vec<String> = entries
            .iter()
            .map(|id| String::from_utf8(blobs.get(id).cloned().unwrap()).unwrap())
            .collect();

        assert_eq!(decoded.len(), 2, "only prior turns belong in the history");
        assert!(decoded[0].contains("first question"));
        assert!(decoded[1].contains("first answer"));
        assert!(
            !decoded.iter().any(|d| d.contains("second question")),
            "the live turn travels in the action, not the history"
        );
    }

    #[test]
    fn turns_pair_a_user_message_with_its_answer() {
        let messages = vec![
            msg(Role::User, "q1"),
            msg(Role::Assistant, "a1"),
            msg(Role::User, "q2"),
        ];
        let mut blobs = BlobStore::new();
        let turns = conversation_turns(&messages, &mut blobs);

        assert_eq!(turns.len(), 1, "the unanswered live turn is not a history turn");

        let turn = pb::ConversationTurnStructure::decode(blobs.get(&turns[0]).unwrap().as_slice())
            .expect("turn decodes");
        let Some(pb::conversation_turn_structure::Turn::AgentConversationTurn(agent)) = turn.turn
        else {
            panic!("expected an agent conversation turn");
        };
        let user = pb::UserMessage::decode(blobs.get(&agent.user_message).unwrap().as_slice())
            .expect("user message decodes");
        assert_eq!(user.text, "q1");
        assert_eq!(agent.steps.len(), 1);
    }

    #[test]
    fn run_request_carries_the_live_turn_and_the_model() {
        let messages = vec![msg(Role::System, "be terse"), msg(Role::User, "ping")];
        let built = build_run_request("composer-1", &messages);

        let client = pb::AgentClientMessage::decode(built.bytes.as_slice()).expect("decodes");
        let pb::agent_client_message::Message::RunRequest(run) =
            client.message.expect("a run request")
        else {
            panic!("expected a run request");
        };

        assert_eq!(run.model_details.as_ref().unwrap().model_id, "composer-1");
        assert_eq!(run.conversation_id.as_deref(), Some(built.conversation_id.as_str()));

        let action = run.action.expect("action").action.expect("action variant");
        let pb::conversation_action::Action::UserMessageAction(user_action) = action else {
            panic!("expected a user message action");
        };
        assert_eq!(user_action.user_message.expect("user message").text, "ping");

        let state = run.conversation_state.expect("conversation state");
        assert_eq!(
            state.root_prompt_messages_json.len(),
            1,
            "one system prompt, no prior history"
        );
        assert!(state.turns.is_empty());
        assert!(built.blobs.get(&state.root_prompt_messages_json[0]).is_some());
    }

    #[test]
    fn an_empty_live_turn_resumes_instead_of_sending_an_empty_message() {
        let built = build_run_request("composer-1", &[msg(Role::Assistant, "prior")]);
        let client = pb::AgentClientMessage::decode(built.bytes.as_slice()).expect("decodes");
        let pb::agent_client_message::Message::RunRequest(run) = client.message.unwrap() else {
            panic!("expected a run request");
        };
        let action = run.action.unwrap().action.unwrap();
        assert!(matches!(
            action,
            pb::conversation_action::Action::ResumeAction(_)
        ));
    }

    #[test]
    fn every_conversation_id_is_fresh() {
        let a = build_run_request("m", &[msg(Role::User, "x")]);
        let b = build_run_request("m", &[msg(Role::User, "x")]);
        assert_ne!(a.conversation_id, b.conversation_id);
    }
}
