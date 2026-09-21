//! Tauri commands for Cursor chat.
//!
//! The webview cannot host this conversation: it is a bidirectional HTTP/2
//! stream that keeps writing after the response starts. So the whole turn runs
//! here, streams `cursor:delta` / `cursor:tool` events to the UI, and returns
//! the finished text.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use prost::Message as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use crate::cursor::audit::{AuditLog, Entry as AuditEntry};
use crate::cursor::exec::ExecHost;
use crate::cursor::frame::ConnectError;
use crate::cursor::request::{build_run_request, ChatMessage};
use crate::cursor::session::{Action, Session};
use crate::cursor::transport::{
    self, Connection, StreamEnd, TransportError, CURSOR_GET_USABLE_MODELS_PATH, CURSOR_HOST,
    CURSOR_RUN_PATH, HEARTBEAT_INTERVAL, IDLE_TIMEOUT,
};
use crate::cursor::pb;

/// Errors carrying this prefix mean "sign in again", not "retry".
pub const AUTH_ERROR_PREFIX: &str = "cursor-auth: ";

#[derive(Debug, Default)]
pub struct CursorState {
    /// Cancel switches, keyed by the request id the webview generated.
    inflight: Mutex<HashMap<String, watch::Sender<bool>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CursorChatResponse {
    pub content: String,
    pub model: String,
    pub tokens: i32,
}

#[derive(Debug, Clone, Serialize)]
struct DeltaEvent {
    request_id: String,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct ToolEvent {
    request_id: String,
    call_id: String,
    name: String,
    completed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CursorModel {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CursorChatRequest {
    pub request_id: String,
    pub access_token: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

fn auth_error(message: impl std::fmt::Display) -> String {
    format!("{AUTH_ERROR_PREFIX}{message}")
}

fn to_command_error(e: TransportError) -> String {
    match e {
        TransportError::Auth(m) => auth_error(m),
        other => other.to_string(),
    }
}

fn audit_log(app: &AppHandle) -> Arc<AuditLog> {
    let path = app
        .path()
        .app_log_dir()
        .or_else(|_| app.path().app_data_dir())
        .map(|dir| dir.join("cursor-exec-audit.log"))
        .unwrap_or_else(|_| std::env::temp_dir().join("sayknow-cursor-exec-audit.log"));
    Arc::new(AuditLog::new(path))
}

/// Run one Cursor chat turn.
#[tauri::command]
pub async fn cursor_chat_send(
    app: AppHandle,
    state: tauri::State<'_, CursorState>,
    request: CursorChatRequest,
) -> Result<CursorChatResponse, String> {
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    {
        let mut inflight = state.inflight.lock().map_err(|e| e.to_string())?;
        inflight.insert(request.request_id.clone(), cancel_tx);
    }

    let result = run_turn(&app, &request, &mut cancel_rx).await;

    if let Ok(mut inflight) = state.inflight.lock() {
        inflight.remove(&request.request_id);
    }

    let _ = app.emit(
        "cursor:done",
        serde_json::json!({
            "request_id": request.request_id,
            "ok": result.is_ok(),
        }),
    );

    result.map_err(to_command_error)
}

/// Cancel an in-flight turn. Unknown ids are a no-op: the turn may already
/// have finished on its own.
#[tauri::command]
pub fn cursor_chat_cancel(state: tauri::State<'_, CursorState>, request_id: String) {
    if let Ok(inflight) = state.inflight.lock() {
        if let Some(tx) = inflight.get(&request_id) {
            let _ = tx.send(true);
        }
    }
}

/// Models this account may use, via the unary `GetUsableModels` call.
#[tauri::command]
pub async fn cursor_list_models(access_token: String) -> Result<Vec<CursorModel>, String> {
    let mut connection = Connection::connect(CURSOR_HOST)
        .await
        .map_err(to_command_error)?;

    // Unary call, and it is NOT the streaming content type: probing the live
    // endpoint on 2026-09-19 showed `application/connect+proto` here answers
    // 415, while `application/proto` with an unframed body answers normally
    // (401 with a JSON body when the token is bad). This matches sayknow-cli's
    // `utils/discovery/cursor.ts:88-96`.
    let (response, mut send) = connection
        .open(
            CURSOR_GET_USABLE_MODELS_PATH,
            &access_token,
            "application/proto",
            false,
        )
        .await
        .map_err(to_command_error)?;

    let payload = pb::GetUsableModelsRequest::default().encode_to_vec();
    transport::write_body(&mut send, &payload, true)
        .await
        .map_err(to_command_error)?;

    // Headers come back only after the body, so the response is awaited here
    // rather than at open time.
    let mut body = transport::receive(response).await.map_err(to_command_error)?;
    let response = transport::read_body(&mut body)
        .await
        .map_err(to_command_error)?;
    let message = transport::decode_unary(&response).map_err(to_command_error)?;
    let decoded = pb::GetUsableModelsResponse::decode(message.as_slice())
        .map_err(|e| format!("could not decode the model list: {e}"))?;

    let mut models: Vec<CursorModel> = decoded
        .models
        .into_iter()
        .filter(|m| !m.model_id.trim().is_empty())
        .map(|m| {
            let name = [m.display_name, m.display_name_short, m.display_model_id]
                .into_iter()
                .find(|candidate| !candidate.trim().is_empty())
                .unwrap_or_else(|| m.model_id.clone());
            CursorModel {
                id: m.model_id,
                name,
            }
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

async fn run_turn(
    app: &AppHandle,
    request: &CursorChatRequest,
    cancel: &mut watch::Receiver<bool>,
) -> Result<CursorChatResponse, TransportError> {
    let built = build_run_request(&request.model, &request.messages);
    let mut session = Session::new(built.blobs);
    let audit = audit_log(app);
    // The turn is audited before it opens: an agent with unrestricted access
    // must be traceable even if the stream dies mid-command.
    audit.record(AuditEntry::new(
        "turn_start",
        built.conversation_id.clone(),
        format!("model={} messages={}", request.model, request.messages.len()),
    ));
    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut exec_host = ExecHost::new(audit, outbound_tx);

    let mut connection = Connection::connect(CURSOR_HOST).await?;
    let (response, mut send) = connection
        .open(
            CURSOR_RUN_PATH,
            &request.access_token,
            "application/connect+proto",
            false,
        )
        .await?;

    // The run request goes out first: Cursor sends response headers only once
    // it has seen the opening frame.
    transport::write_frame(&mut send, &built.bytes).await?;
    let mut body = transport::receive(response).await?;

    let mut deframer = crate::cursor::frame::Deframer::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await; // the first tick is immediate

    let mut deadline = Instant::now() + IDLE_TIMEOUT;
    let mut stream_error: Option<ConnectError> = None;

    let outcome = loop {
        if *cancel.borrow() {
            break Err(TransportError::Cancelled);
        }

        tokio::select! {
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    break Err(TransportError::Cancelled);
                }
            }
            _ = heartbeat.tick() => {
                transport::send_message(&mut send, &Session::heartbeat()).await?;
            }
            // Streamed shell output, pushed while the command is still running.
            Some(streamed) = outbound_rx.recv() => {
                transport::send_message(&mut send, &streamed).await?;
            }
            message = transport::next_message(&mut body, &mut deframer, deadline) => {
                let message = match message {
                    Ok(Some(Ok(message))) => message,
                    Ok(Some(Err(StreamEnd::Ended(err)))) => {
                        stream_error = err;
                        break Ok(());
                    }
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e),
                };

                deadline = Instant::now() + IDLE_TIMEOUT;

                let mut done = false;
                for action in session.handle(message) {
                    match action {
                        Action::Send(reply) => {
                            transport::send_message(&mut send, &reply).await?;
                        }
                        Action::Exec(exec) => {
                            for reply in exec_host.handle(exec).await {
                                transport::send_message(&mut send, &reply).await?;
                            }
                        }
                        Action::Delta(text) => {
                            let _ = app.emit(
                                "cursor:delta",
                                DeltaEvent {
                                    request_id: request.request_id.clone(),
                                    text,
                                },
                            );
                        }
                        Action::Thinking(_) => {}
                        Action::Tool { call_id, name, completed } => {
                            let _ = app.emit(
                                "cursor:tool",
                                ToolEvent {
                                    request_id: request.request_id.clone(),
                                    call_id,
                                    name,
                                    completed,
                                },
                            );
                        }
                        Action::Done => done = true,
                    }
                }

                if done {
                    break Ok(());
                }
            }
        }
    };

    // Whatever happened, nothing this turn spawned may outlive it.
    exec_host.shutdown().await;
    let _ = send.send_data(bytes::Bytes::new(), true);

    outcome?;

    if let Some(err) = stream_error {
        return Err(if err.is_auth() {
            TransportError::Auth(err.to_string())
        } else {
            TransportError::Other(err.to_string())
        });
    }

    let output = session.into_output();
    if output.text.trim().is_empty() {
        return Err(TransportError::Other(
            "Cursor returned an empty response".into(),
        ));
    }

    Ok(CursorChatResponse {
        content: output.text.trim().to_string(),
        model: request.model.clone(),
        tokens: output.tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors_are_tagged_for_the_webview() {
        let message = to_command_error(TransportError::Auth("HTTP 401".into()));
        assert!(message.starts_with(AUTH_ERROR_PREFIX));
        assert!(message.contains("HTTP 401"));
    }

    #[test]
    fn other_errors_are_not_tagged_as_auth() {
        let message = to_command_error(TransportError::Other("connection reset".into()));
        assert!(!message.starts_with(AUTH_ERROR_PREFIX));
        assert_eq!(message, "connection reset");
    }

    #[test]
    fn cancelling_an_unknown_request_is_a_no_op() {
        let state = CursorState::default();
        let (tx, rx) = watch::channel(false);
        state
            .inflight
            .lock()
            .unwrap()
            .insert("known".into(), tx);

        // Same shape the command uses, without needing a tauri State handle.
        if let Some(tx) = state.inflight.lock().unwrap().get("unknown") {
            let _ = tx.send(true);
        }
        assert!(!*rx.borrow(), "an unknown id must not cancel another turn");

        if let Some(tx) = state.inflight.lock().unwrap().get("known") {
            let _ = tx.send(true);
        }
        assert!(*rx.borrow());
    }
}
