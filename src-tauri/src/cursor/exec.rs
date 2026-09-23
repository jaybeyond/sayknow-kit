//! Executes the agent's tool requests on this machine.
//!
//! The user chose unrestricted access: no workspace confinement and no
//! approval prompts. What remains mandatory is honesty about what ran — every
//! command and mutation is audited — plus output caps and a cancellation path
//! that kills the whole process group rather than leaking background shells.
//!
//! Every request is answered. An unimplemented capability replies with the
//! protocol's own rejected/error variant (see sayknow-cli
//! `packages/ai/src/providers/cursor.ts:1003-1238`) instead of being ignored,
//! because silence stalls the turn.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::audit::{AuditLog, Entry};
use super::pb;

/// Inline stdout/stderr cap. A runaway command must not exhaust memory or the
/// model's context.
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;
/// Cap for a single file read handed back to the model.
pub const MAX_READ_BYTES: usize = 512 * 1024;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;

/// Truncate on a char boundary and say so.
fn cap(mut text: String, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n… [truncated]");
    (text, true)
}

struct BackgroundShell {
    child: Child,
    command: String,
}

/// Runs tool requests and owns the processes they spawn.
///
/// Replies normally come back from [`ExecHost::handle`], but a streamed shell
/// has to push output while the command is still running, so the host also
/// holds a sender the driver drains and writes to the wire.
pub struct ExecHost {
    audit: Arc<AuditLog>,
    outbound: mpsc::UnboundedSender<pb::AgentClientMessage>,
    shells: HashMap<u32, BackgroundShell>,
    next_shell_id: u32,
}

impl ExecHost {
    pub fn new(
        audit: Arc<AuditLog>,
        outbound: mpsc::UnboundedSender<pb::AgentClientMessage>,
    ) -> Self {
        Self {
            audit,
            outbound,
            shells: HashMap::new(),
            next_shell_id: 1,
        }
    }

    /// Kill everything this turn spawned, process groups included.
    pub async fn shutdown(&mut self) {
        for (id, mut shell) in std::mem::take(&mut self.shells) {
            kill_process_group(&mut shell.child).await;
            self.audit.record(Entry::new(
                "shell_kill",
                shell.command.clone(),
                format!("background shell {id} killed on cancel"),
            ));
        }
    }

    /// Handle one exec request, returning the client messages to send back.
    ///
    /// `shellStream` answers with several messages (start, output, exit), so
    /// the return type is a list rather than a single reply.
    pub async fn handle(&mut self, exec: pb::ExecServerMessage) -> Vec<pb::AgentClientMessage> {
        use pb::exec_server_message::Message as E;

        let id = exec.id;
        let exec_id = exec.exec_id.clone();
        let Some(message) = exec.message else {
            return Vec::new();
        };

        match message {
            E::ReadArgs(args) => vec![self.read(id, exec_id, args)],
            E::LsArgs(args) => vec![self.ls(id, exec_id, args)],
            E::GrepArgs(args) => vec![self.grep(id, exec_id, args).await],
            E::WriteArgs(args) => vec![self.write(id, exec_id, args)],
            E::DeleteArgs(args) => vec![self.delete(id, exec_id, args)],
            E::ShellArgs(args) => vec![self.shell(id, exec_id, args).await],
            E::ShellStreamArgs(args) => self.shell_stream(id, exec_id, args).await,
            E::BackgroundShellSpawnArgs(args) => vec![self.background_shell(id, exec_id, args)],
            E::WriteShellStdinArgs(args) => vec![self.write_shell_stdin(id, exec_id, args).await],
            E::FetchArgs(args) => vec![self.fetch(id, exec_id, args).await],
            // Answered honestly rather than faked: this app ships no language
            // server, no MCP servers, and no screen/computer control.
            E::DiagnosticsArgs(args) => vec![reply(
                id,
                exec_id,
                pb::exec_client_message::Message::DiagnosticsResult(pb::DiagnosticsResult {
                    result: Some(pb::diagnostics_result::Result::Error(pb::DiagnosticsError {
                        path: args.path,
                        error: "SayKnow Kit has no language server, so diagnostics are unavailable"
                            .into(),
                    })),
                }),
            )],
            E::McpArgs(args) => vec![reply(
                id,
                exec_id,
                pb::exec_client_message::Message::McpResult(pb::McpResult {
                    result: Some(pb::mcp_result::Result::ToolNotFound(pb::McpToolNotFound {
                        name: args.name,
                        available_tools: Vec::new(),
                    })),
                }),
            )],
            E::ListMcpResourcesExecArgs(_) => vec![reply(
                id,
                exec_id,
                pb::exec_client_message::Message::ListMcpResourcesExecResult(
                    pb::ListMcpResourcesExecResult {
                        result: Some(pb::list_mcp_resources_exec_result::Result::Error(
                            pb::ListMcpResourcesError {
                                error: "no MCP servers are configured".into(),
                            },
                        )),
                    },
                ),
            )],
            E::ReadMcpResourceExecArgs(_) => vec![reply(
                id,
                exec_id,
                pb::exec_client_message::Message::ReadMcpResourceExecResult(
                    pb::ReadMcpResourceExecResult {
                        result: Some(pb::read_mcp_resource_exec_result::Result::Error(
                            pb::ReadMcpResourceError {
                                uri: String::new(),
                                error: "no MCP servers are configured".into(),
                            },
                        )),
                    },
                ),
            )],
            E::RecordScreenArgs(_) => vec![reply(
                id,
                exec_id,
                pb::exec_client_message::Message::RecordScreenResult(pb::RecordScreenResult {
                    result: Some(pb::record_screen_result::Result::Failure(
                        pb::RecordScreenFailure {
                            error: "screen recording is not enabled in SayKnow Kit".into(),
                        },
                    )),
                }),
            )],
            E::ComputerUseArgs(_) => vec![reply(
                id,
                exec_id,
                pb::exec_client_message::Message::ComputerUseResult(pb::ComputerUseResult {
                    result: Some(pb::computer_use_result::Result::Error(
                        pb::ComputerUseError {
                            error: "computer control is not enabled in SayKnow Kit".into(),
                            ..Default::default()
                        },
                    )),
                }),
            )],
            // The context handshake is answered by the session, not here.
            E::RequestContextArgs(_) => Vec::new(),
        }
    }

    fn read(&self, id: u32, exec_id: String, args: pb::ReadArgs) -> pb::AgentClientMessage {
        let path = args.path.clone();
        let result = match std::fs::read(&path) {
            Ok(bytes) => {
                let size = bytes.len() as i64;
                let text = String::from_utf8_lossy(&bytes).to_string();
                let lines = text.lines().count() as i32;
                let (content, truncated) = cap(text, MAX_READ_BYTES);
                self.audit
                    .record(Entry::new("read", &path, format!("{size} bytes")));
                pb::read_result::Result::Success(pb::ReadSuccess {
                    path: path.clone(),
                    total_lines: lines,
                    file_size: size,
                    truncated,
                    output: Some(pb::read_success::Output::Content(content)),
                    ..Default::default()
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                pb::read_result::Result::FileNotFound(pb::ReadFileNotFound { path: path.clone() })
            }
            Err(e) => pb::read_result::Result::Error(pb::ReadError {
                path: path.clone(),
                error: e.to_string(),
            }),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::ReadResult(pb::ReadResult {
                result: Some(result),
            }),
        )
    }

    fn ls(&self, id: u32, exec_id: String, args: pb::LsArgs) -> pb::AgentClientMessage {
        let path = args.path.clone();
        let result = match list_dir(&path) {
            Ok(root) => {
                self.audit.record(Entry::new("ls", &path, ""));
                pb::ls_result::Result::Success(pb::LsSuccess {
                    directory_tree_root: Some(root),
                })
            }
            Err(e) => pb::ls_result::Result::Error(pb::LsError {
                path: path.clone(),
                error: e.to_string(),
            }),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::LsResult(pb::LsResult {
                result: Some(result),
            }),
        )
    }

    /// Grep shells out to `rg` when present and falls back to `grep -rn`.
    async fn grep(&self, id: u32, exec_id: String, args: pb::GrepArgs) -> pb::AgentClientMessage {
        let path = args.path.clone().unwrap_or_else(|| ".".to_string());
        let pattern = args.pattern.clone();
        let output_mode = args.output_mode.clone().unwrap_or_else(|| "content".into());

        let mut command = Command::new("rg");
        command
            .arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never");
        if args.case_insensitive.unwrap_or(false) {
            command.arg("-i");
        }
        if output_mode == "files_with_matches" {
            command.arg("-l");
        }
        command.arg(&pattern).arg(&path);

        let ran = run_capture(command).await;
        let ran = match ran {
            Ok(out) => Ok(out),
            Err(_) => {
                let mut fallback = Command::new("grep");
                fallback.arg("-rn");
                if args.case_insensitive.unwrap_or(false) {
                    fallback.arg("-i");
                }
                fallback.arg(&pattern).arg(&path);
                run_capture(fallback).await
            }
        };

        let result = match ran {
            Ok(out) => {
                self.audit
                    .record(Entry::new("grep", &path, format!("pattern={pattern}")));
                let (stdout, truncated) = cap(out.stdout, MAX_OUTPUT_BYTES);
                let union = if output_mode == "files_with_matches" {
                    let files: Vec<String> =
                        stdout.lines().map(|l| l.to_string()).collect();
                    let total = files.len() as i32;
                    pb::grep_union_result::Result::Files(pb::GrepFilesResult {
                        files,
                        total_files: total,
                        client_truncated: truncated,
                        ripgrep_truncated: false,
                    })
                } else {
                    let matches: Vec<pb::GrepFileMatch> = stdout
                        .lines()
                        .filter_map(parse_grep_line)
                        .collect();
                    let total = matches.len() as i32;
                    pb::grep_union_result::Result::Content(pb::GrepContentResult {
                        matches,
                        total_lines: total,
                        total_matched_lines: total,
                        client_truncated: truncated,
                        ripgrep_truncated: false,
                    })
                };

                let mut workspace_results = HashMap::new();
                workspace_results.insert(
                    path.clone(),
                    pb::GrepUnionResult {
                        result: Some(union),
                    },
                );

                pb::grep_result::Result::Success(pb::GrepSuccess {
                    pattern: pattern.clone(),
                    path: path.clone(),
                    output_mode,
                    workspace_results,
                    active_editor_result: None,
                })
            }
            Err(e) => pb::grep_result::Result::Error(pb::GrepError { error: e }),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::GrepResult(pb::GrepResult {
                result: Some(result),
            }),
        )
    }

    fn write(&self, id: u32, exec_id: String, args: pb::WriteArgs) -> pb::AgentClientMessage {
        let path = args.path.clone();
        let bytes = if args.file_bytes.is_empty() {
            args.file_text.clone().into_bytes()
        } else {
            args.file_bytes.clone()
        };

        let written = std::path::Path::new(&path)
            .parent()
            .map(|parent| std::fs::create_dir_all(parent))
            .unwrap_or(Ok(()))
            .and_then(|_| std::fs::write(&path, &bytes));

        let result = match written {
            Ok(()) => {
                self.audit.record(Entry::new(
                    "write",
                    &path,
                    format!("{} bytes", bytes.len()),
                ));
                pb::write_result::Result::Success(pb::WriteSuccess {
                    path: path.clone(),
                    lines_created: args.file_text.lines().count() as i32,
                    file_size: bytes.len() as i32,
                    file_content_after_write: args
                        .return_file_content_after_write
                        .then(|| args.file_text.clone()),
                })
            }
            Err(e) => pb::write_result::Result::Error(pb::WriteError {
                path: path.clone(),
                error: e.to_string(),
            }),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::WriteResult(pb::WriteResult {
                result: Some(result),
            }),
        )
    }

    fn delete(&self, id: u32, exec_id: String, args: pb::DeleteArgs) -> pb::AgentClientMessage {
        let path = args.path.clone();
        let previous = std::fs::read_to_string(&path).unwrap_or_default();
        let size = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);

        let result = match std::fs::remove_file(&path) {
            Ok(()) => {
                self.audit
                    .record(Entry::new("delete", &path, format!("{size} bytes")));
                pb::delete_result::Result::Success(pb::DeleteSuccess {
                    path: path.clone(),
                    deleted_file: path.clone(),
                    file_size: size,
                    prev_content: cap(previous, MAX_READ_BYTES).0,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                pb::delete_result::Result::FileNotFound(pb::DeleteFileNotFound {
                    path: path.clone(),
                })
            }
            Err(e) => pb::delete_result::Result::Error(pb::DeleteError {
                path: path.clone(),
                error: e.to_string(),
            }),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::DeleteResult(pb::DeleteResult {
                result: Some(result),
            }),
        )
    }

    async fn shell(&self, id: u32, exec_id: String, args: pb::ShellArgs) -> pb::AgentClientMessage {
        let started = std::time::Instant::now();
        let outcome = run_shell(&args.command, &args.working_directory, shell_timeout(&args)).await;
        let elapsed = started.elapsed().as_millis() as i32;

        let result = match outcome {
            Ok(out) => {
                self.audit.record(Entry::new(
                    "shell",
                    args.command.clone(),
                    format!("cwd={} exit={}", args.working_directory, out.code),
                ));
                let (stdout, _) = cap(out.stdout, MAX_OUTPUT_BYTES);
                let (stderr, _) = cap(out.stderr, MAX_OUTPUT_BYTES);
                if out.code == 0 {
                    pb::shell_result::Result::Success(pb::ShellSuccess {
                        command: args.command.clone(),
                        working_directory: args.working_directory.clone(),
                        exit_code: out.code,
                        stdout,
                        stderr,
                        execution_time: elapsed,
                        ..Default::default()
                    })
                } else {
                    pb::shell_result::Result::Failure(pb::ShellFailure {
                        command: args.command.clone(),
                        working_directory: args.working_directory.clone(),
                        exit_code: out.code,
                        stdout,
                        stderr,
                        execution_time: elapsed,
                        aborted: out.timed_out,
                        ..Default::default()
                    })
                }
            }
            Err(e) => {
                self.audit.record(Entry::new(
                    "shell_error",
                    args.command.clone(),
                    e.clone(),
                ));
                pb::shell_result::Result::SpawnError(pb::ShellSpawnError {
                    command: args.command.clone(),
                    working_directory: args.working_directory.clone(),
                    error: e,
                })
            }
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::ShellResult(pb::ShellResult {
                result: Some(result),
                ..Default::default()
            }),
        )
    }

    /// Streamed shell: output goes out while the command is still running.
    ///
    /// A build or a test run is exactly where the agent needs to see progress,
    /// so stdout and stderr are forwarded as they arrive rather than collected
    /// and dumped at the end. The `start` event is returned so ordering is
    /// guaranteed; everything after it rides the outbound channel.
    async fn shell_stream(
        &self,
        id: u32,
        exec_id: String,
        args: pb::ShellArgs,
    ) -> Vec<pb::AgentClientMessage> {
        let start = stream_event(
            id,
            exec_id.clone(),
            pb::shell_stream::Event::Start(pb::ShellStreamStart {
                sandbox_policy: None,
            }),
        );

        let mut child = match shell_command(&args.command, &args.working_directory).spawn() {
            Ok(child) => child,
            Err(e) => {
                let error = e.to_string();
                self.audit.record(Entry::new(
                    "shell_stream_error",
                    args.command.clone(),
                    error.clone(),
                ));
                return vec![
                    start,
                    stream_event(
                        id,
                        exec_id.clone(),
                        pb::shell_stream::Event::Stderr(pb::ShellStreamStderr { data: error }),
                    ),
                    stream_event(
                        id,
                        exec_id,
                        pb::shell_stream::Event::Exit(pb::ShellStreamExit {
                            code: 1,
                            cwd: args.working_directory.clone(),
                            aborted: false,
                            ..Default::default()
                        }),
                    ),
                ];
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let outbound = self.outbound.clone();
        let timeout_ms = shell_timeout(&args);

        let pump_stdout = pump(stdout, {
            let outbound = outbound.clone();
            let exec_id = exec_id.clone();
            move |chunk| {
                let _ = outbound.send(stream_event(
                    id,
                    exec_id.clone(),
                    pb::shell_stream::Event::Stdout(pb::ShellStreamStdout { data: chunk }),
                ));
            }
        });
        let pump_stderr = pump(stderr, {
            let outbound = outbound.clone();
            let exec_id = exec_id.clone();
            move |chunk| {
                let _ = outbound.send(stream_event(
                    id,
                    exec_id.clone(),
                    pb::shell_stream::Event::Stderr(pb::ShellStreamStderr { data: chunk }),
                ));
            }
        });

        let wait = async {
            tokio::join!(pump_stdout, pump_stderr);
            child.wait().await
        };

        let (code, aborted) =
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), wait).await {
                Ok(Ok(status)) => (status.code().unwrap_or(-1), false),
                Ok(Err(e)) => {
                    let _ = outbound.send(stream_event(
                        id,
                        exec_id.clone(),
                        pb::shell_stream::Event::Stderr(pb::ShellStreamStderr {
                            data: e.to_string(),
                        }),
                    ));
                    (-1, false)
                }
                Err(_) => (124, true),
            };

        self.audit.record(Entry::new(
            "shell_stream",
            args.command.clone(),
            format!("cwd={} exit={code}", args.working_directory),
        ));

        let _ = outbound.send(stream_event(
            id,
            exec_id,
            pb::shell_stream::Event::Exit(pb::ShellStreamExit {
                code: code.max(0) as u32,
                cwd: args.working_directory.clone(),
                aborted,
                ..Default::default()
            }),
        ));

        vec![start]
    }

    fn background_shell(
        &mut self,
        id: u32,
        exec_id: String,
        args: pb::BackgroundShellSpawnArgs,
    ) -> pb::AgentClientMessage {
        let mut command = shell_command(&args.command, &args.working_directory);
        command.stdin(Stdio::piped());

        let result = match command.spawn() {
            Ok(child) => {
                let shell_id = self.next_shell_id;
                self.next_shell_id += 1;
                let pid = child.id().unwrap_or_default();
                self.shells.insert(
                    shell_id,
                    BackgroundShell {
                        child,
                        command: args.command.clone(),
                    },
                );
                self.audit.record(Entry::new(
                    "background_shell",
                    args.command.clone(),
                    format!("shell_id={shell_id} pid={pid}"),
                ));
                pb::background_shell_spawn_result::Result::Success(
                    pb::BackgroundShellSpawnSuccess {
                        shell_id,
                        command: args.command.clone(),
                        working_directory: args.working_directory.clone(),
                        pid: Some(pid),
                        ..Default::default()
                    },
                )
            }
            Err(e) => pb::background_shell_spawn_result::Result::Error(
                pb::BackgroundShellSpawnError {
                    command: args.command.clone(),
                    working_directory: args.working_directory.clone(),
                    error: e.to_string(),
                },
            ),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::BackgroundShellSpawnResult(
                pb::BackgroundShellSpawnResult {
                    result: Some(result),
                },
            ),
        )
    }

    async fn write_shell_stdin(
        &mut self,
        id: u32,
        exec_id: String,
        args: pb::WriteShellStdinArgs,
    ) -> pb::AgentClientMessage {
        let result = match self.shells.get_mut(&args.shell_id) {
            Some(shell) => match shell.child.stdin.as_mut() {
                Some(stdin) => match stdin.write_all(args.chars.as_bytes()).await {
                    Ok(()) => {
                        let _ = stdin.flush().await;
                        self.audit.record(Entry::new(
                            "shell_stdin",
                            shell.command.clone(),
                            format!("{} bytes", args.chars.len()),
                        ));
                        pb::write_shell_stdin_result::Result::Success(
                            pb::WriteShellStdinSuccess {
                                shell_id: args.shell_id,
                                terminal_file_length_before_input_written: 0,
                            },
                        )
                    }
                    Err(e) => pb::write_shell_stdin_result::Result::Error(
                        pb::WriteShellStdinError {
                            error: e.to_string(),
                        },
                    ),
                },
                None => pb::write_shell_stdin_result::Result::Error(pb::WriteShellStdinError {
                    error: "that shell has no open stdin".into(),
                }),
            },
            None => pb::write_shell_stdin_result::Result::Error(pb::WriteShellStdinError {
                error: format!("unknown shell id {}", args.shell_id),
            }),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::WriteShellStdinResult(pb::WriteShellStdinResult {
                result: Some(result),
            }),
        )
    }

    async fn fetch(&self, id: u32, exec_id: String, args: pb::FetchArgs) -> pb::AgentClientMessage {
        let url = args.url.clone();
        let mut command = Command::new("curl");
        command
            .arg("-sSL")
            .arg("--max-time")
            .arg("30")
            .arg(&url);

        let result = match run_capture(command).await {
            Ok(out) => {
                self.audit.record(Entry::new("fetch", &url, ""));
                let (body, _truncated) = cap(out.stdout, MAX_OUTPUT_BYTES);
                pb::fetch_result::Result::Success(pb::FetchSuccess {
                    url: url.clone(),
                    content: body,
                    status_code: out.code,
                    content_type: String::new(),
                })
            }
            Err(e) => pb::fetch_result::Result::Error(pb::FetchError {
                url: url.clone(),
                error: e,
            }),
        };

        reply(
            id,
            exec_id,
            pb::exec_client_message::Message::FetchResult(pb::FetchResult {
                result: Some(result),
            }),
        )
    }
}

fn shell_timeout(args: &pb::ShellArgs) -> u64 {
    if args.timeout > 0 {
        args.timeout as u64
    } else {
        DEFAULT_SHELL_TIMEOUT_MS
    }
}

fn reply(
    id: u32,
    exec_id: String,
    message: pb::exec_client_message::Message,
) -> pb::AgentClientMessage {
    pb::AgentClientMessage {
        message: Some(pb::agent_client_message::Message::ExecClientMessage(
            pb::ExecClientMessage {
                id,
                exec_id,
                message: Some(message),
            },
        )),
    }
}

fn stream_event(
    id: u32,
    exec_id: String,
    event: pb::shell_stream::Event,
) -> pb::AgentClientMessage {
    reply(
        id,
        exec_id,
        pb::exec_client_message::Message::ShellStream(pb::ShellStream { event: Some(event) }),
    )
}

/// Forward a child pipe to `emit` as the bytes arrive.
///
/// Reads raw chunks rather than lines: a build that prints a progress bar with
/// no newline must still reach the agent. Output stops being forwarded once
/// the cap is hit, so one runaway command cannot flood the stream.
async fn pump<R>(reader: Option<R>, mut emit: impl FnMut(String))
where
    R: AsyncReadExt + Unpin,
{
    let Some(mut reader) = reader else { return };
    let mut buffer = [0u8; 8192];
    let mut forwarded = 0usize;

    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if forwarded >= MAX_OUTPUT_BYTES {
                    continue;
                }
                let room = MAX_OUTPUT_BYTES - forwarded;
                let take = n.min(room);
                forwarded += take;
                let mut chunk = String::from_utf8_lossy(&buffer[..take]).to_string();
                if take < n {
                    chunk.push_str("\n… [truncated]");
                }
                emit(chunk);
            }
        }
    }
}

pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
}

fn shell_command(command: &str, working_directory: &str) -> Command {
    let mut cmd = platform_shell_command(command);
    if !working_directory.is_empty() && std::path::Path::new(working_directory).is_dir() {
        cmd.current_dir(working_directory);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Its own process group, so cancelling the turn can kill the whole tree
    // instead of orphaning children of the shell.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

#[cfg(unix)]
fn platform_shell_command(command: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(windows)]
fn platform_shell_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}

async fn run_shell(
    command: &str,
    working_directory: &str,
    timeout_ms: u64,
) -> Result<CommandOutput, String> {
    let mut child = shell_command(command, working_directory)
        .spawn()
        .map_err(|e| e.to_string())?;

    match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        child.wait_with_output_compat(),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(CommandOutput {
            stdout: String::new(),
            stderr: format!("command timed out after {timeout_ms}ms"),
            code: 124,
            timed_out: true,
        }),
    }
}

/// `wait_with_output` consumes the child, which loses the handle needed to
/// kill the group on timeout; this keeps the semantics while allowing the
/// timeout branch above to give up cleanly.
trait WaitWithOutputCompat {
    async fn wait_with_output_compat(&mut self) -> Result<CommandOutput, String>;
}

impl WaitWithOutputCompat for Child {
    async fn wait_with_output_compat(&mut self) -> Result<CommandOutput, String> {
        use tokio::io::AsyncReadExt;

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        if let Some(mut out) = self.stdout.take() {
            let _ = out.read_to_end(&mut stdout_buf).await;
        }
        if let Some(mut err) = self.stderr.take() {
            let _ = err.read_to_end(&mut stderr_buf).await;
        }

        let status = self.wait().await.map_err(|e| e.to_string())?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
            stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
            code: status.code().unwrap_or(-1),
            timed_out: false,
        })
    }
}

async fn run_capture(mut command: Command) -> Result<CommandOutput, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);

    let output = command.output().await.map_err(|e| e.to_string())?;
    let code = output.status.code().unwrap_or(-1);
    if code != 0 && output.stdout.is_empty() && !output.stderr.is_empty() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        code,
        timed_out: false,
    })
}

/// Kill the child and everything it spawned.
async fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Negative pid targets the process group created by `process_group(0)`.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

fn list_dir(path: &str) -> std::io::Result<pb::LsDirectoryTreeNode> {
    let mut node = pb::LsDirectoryTreeNode {
        abs_path: path.to_string(),
        children_were_processed: true,
        ..Default::default()
    };

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path().to_string_lossy().to_string();
        if entry.file_type()?.is_dir() {
            node.children_dirs.push(pb::LsDirectoryTreeNode {
                abs_path: entry_path,
                children_were_processed: false,
                ..Default::default()
            });
        } else {
            node.num_files += 1;
            node.children_files.push(pb::LsDirectoryTreeNodeFile {
                name: entry.file_name().to_string_lossy().to_string(),
                ..Default::default()
            });
        }
    }

    Ok(node)
}

/// `path:line:text` as emitted by both `rg --line-number` and `grep -rn`.
fn parse_grep_line(line: &str) -> Option<pb::GrepFileMatch> {
    let mut parts = line.splitn(3, ':');
    let file = parts.next()?.to_string();
    let line_number: i32 = parts.next()?.parse().ok()?;
    let text = parts.next().unwrap_or_default().to_string();
    Some(pb::GrepFileMatch {
        file,
        matches: vec![pb::GrepContentMatch {
            line_number,
            content: text,
            ..Default::default()
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host plus the channel its streamed output goes to.
    fn host() -> (ExecHost, mpsc::UnboundedReceiver<pb::AgentClientMessage>) {
        let dir = std::env::temp_dir().join(format!("cursor-exec-{}", uuid::Uuid::new_v4()));
        let (tx, rx) = mpsc::unbounded_channel();
        (
            ExecHost::new(Arc::new(AuditLog::new(dir.join("audit.log"))), tx),
            rx,
        )
    }

    fn exec(message: pb::exec_server_message::Message) -> pb::ExecServerMessage {
        pb::ExecServerMessage {
            id: 1,
            exec_id: "e".into(),
            message: Some(message),
            ..Default::default()
        }
    }

    fn exec_reply(msg: &pb::AgentClientMessage) -> &pb::exec_client_message::Message {
        let Some(pb::agent_client_message::Message::ExecClientMessage(exec)) = &msg.message else {
            panic!("expected an exec client message");
        };
        exec.message.as_ref().expect("a result")
    }

    #[cfg(windows)]
    fn test_shell_command(unix: &str, windows: &str) -> String {
        let _ = unix;
        windows.to_string()
    }

    #[cfg(not(windows))]
    fn test_shell_command(unix: &str, windows: &str) -> String {
        let _ = windows;
        unix.to_string()
    }

    #[test]
    fn output_caps_truncate_and_say_so() {
        let (text, truncated) = cap("x".repeat(200), 100);
        assert!(truncated);
        assert!(text.len() <= 120);
        assert!(text.ends_with("[truncated]"));

        let (small, untouched) = cap("short".into(), 100);
        assert!(!untouched);
        assert_eq!(small, "short");
    }

    #[tokio::test]
    async fn shell_success_carries_stdout_and_exit_zero() {
        let (mut host, _streamed) = host();
        let msgs = host
            .handle(exec(pb::exec_server_message::Message::ShellArgs(
                pb::ShellArgs {
                    command: test_shell_command("echo cursor-exec-ok", "echo cursor-exec-ok"),
                    working_directory: std::env::temp_dir().to_string_lossy().to_string(),
                    ..Default::default()
                },
            )))
            .await;

        let pb::exec_client_message::Message::ShellResult(result) = exec_reply(&msgs[0]) else {
            panic!("expected a shell result");
        };
        let Some(pb::shell_result::Result::Success(success)) = &result.result else {
            panic!("expected success, got {:?}", result.result);
        };
        assert_eq!(success.exit_code, 0);
        assert!(success.stdout.contains("cursor-exec-ok"));
    }

    #[tokio::test]
    async fn a_failing_command_is_a_failure_not_an_error() {
        let (mut host, _streamed) = host();
        let msgs = host
            .handle(exec(pb::exec_server_message::Message::ShellArgs(
                pb::ShellArgs {
                    command: test_shell_command("exit 3", "exit /B 3"),
                    ..Default::default()
                },
            )))
            .await;

        let pb::exec_client_message::Message::ShellResult(result) = exec_reply(&msgs[0]) else {
            panic!("expected a shell result");
        };
        let Some(pb::shell_result::Result::Failure(failure)) = &result.result else {
            panic!("expected failure");
        };
        assert_eq!(failure.exit_code, 3);
    }

    #[tokio::test]
    async fn a_hung_command_times_out_instead_of_hanging_the_turn() {
        let (mut host, _streamed) = host();
        let msgs = host
            .handle(exec(pb::exec_server_message::Message::ShellArgs(
                pb::ShellArgs {
                    command: test_shell_command("sleep 30", "ping -n 31 127.0.0.1 > nul"),
                    timeout: 300,
                    ..Default::default()
                },
            )))
            .await;

        let pb::exec_client_message::Message::ShellResult(result) = exec_reply(&msgs[0]) else {
            panic!("expected a shell result");
        };
        let Some(pb::shell_result::Result::Failure(failure)) = &result.result else {
            panic!("expected a failure");
        };
        assert!(failure.aborted);
    }

    #[tokio::test]
    async fn write_then_read_round_trips_through_the_filesystem() {
        let (mut host, _streamed) = host();
        let path = std::env::temp_dir()
            .join(format!("cursor-write-{}.txt", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();

        host.handle(exec(pb::exec_server_message::Message::WriteArgs(
            pb::WriteArgs {
                path: path.clone(),
                file_text: "written by cursor".into(),
                ..Default::default()
            },
        )))
        .await;

        let msgs = host
            .handle(exec(pb::exec_server_message::Message::ReadArgs(
                pb::ReadArgs {
                    path: path.clone(),
                    ..Default::default()
                },
            )))
            .await;

        let pb::exec_client_message::Message::ReadResult(result) = exec_reply(&msgs[0]) else {
            panic!("expected a read result");
        };
        let Some(pb::read_result::Result::Success(success)) = &result.result else {
            panic!("expected success");
        };
        assert_eq!(
            success.output,
            Some(pb::read_success::Output::Content("written by cursor".into()))
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reading_a_missing_file_reports_not_found() {
        let (mut host, _streamed) = host();
        let msgs = host
            .handle(exec(pb::exec_server_message::Message::ReadArgs(
                pb::ReadArgs {
                    path: "/definitely/not/here".into(),
                    ..Default::default()
                },
            )))
            .await;

        let pb::exec_client_message::Message::ReadResult(result) = exec_reply(&msgs[0]) else {
            panic!("expected a read result");
        };
        assert!(matches!(
            result.result,
            Some(pb::read_result::Result::FileNotFound(_))
        ));
    }

    #[tokio::test]
    async fn unsupported_capabilities_answer_instead_of_going_silent() {
        let (mut host, _streamed) = host();

        let mcp = host
            .handle(exec(pb::exec_server_message::Message::McpArgs(
                pb::McpArgs {
                    name: "some-tool".into(),
                    ..Default::default()
                },
            )))
            .await;
        let pb::exec_client_message::Message::McpResult(result) = exec_reply(&mcp[0]) else {
            panic!("expected an mcp result");
        };
        assert!(matches!(
            result.result,
            Some(pb::mcp_result::Result::ToolNotFound(_))
        ));

        let computer = host
            .handle(exec(pb::exec_server_message::Message::ComputerUseArgs(
                pb::ComputerUseArgs::default(),
            )))
            .await;
        let pb::exec_client_message::Message::ComputerUseResult(result) = exec_reply(&computer[0])
        else {
            panic!("expected a computer-use result");
        };
        assert!(matches!(
            result.result,
            Some(pb::computer_use_result::Result::Error(_))
        ));

        let diagnostics = host
            .handle(exec(pb::exec_server_message::Message::DiagnosticsArgs(
                pb::DiagnosticsArgs::default(),
            )))
            .await;
        let pb::exec_client_message::Message::DiagnosticsResult(result) =
            exec_reply(&diagnostics[0])
        else {
            panic!("expected a diagnostics result");
        };
        assert!(matches!(
            result.result,
            Some(pb::diagnostics_result::Result::Error(_))
        ));
    }

    #[tokio::test]
    async fn shell_stream_emits_start_then_streams_output_before_exit() {
        let (mut host, mut streamed) = host();
        let opening = host
            .handle(exec(pb::exec_server_message::Message::ShellStreamArgs(
                pb::ShellArgs {
                    command: test_shell_command("echo streamed", "echo streamed"),
                    ..Default::default()
                },
            )))
            .await;

        // `start` is returned so its ordering is guaranteed; the rest arrives
        // on the channel as the command produces it.
        assert_eq!(opening.len(), 1);
        let pb::exec_client_message::Message::ShellStream(first) = exec_reply(&opening[0]) else {
            panic!("expected a shell stream event");
        };
        assert!(matches!(
            first.event,
            Some(pb::shell_stream::Event::Start(_))
        ));

        let mut events = Vec::new();
        while let Ok(message) = streamed.try_recv() {
            let pb::exec_client_message::Message::ShellStream(stream) = exec_reply(&message) else {
                panic!("expected shell stream events");
            };
            events.push(stream.event.clone().expect("event"));
        }

        let stdout: String = events
            .iter()
            .filter_map(|e| match e {
                pb::shell_stream::Event::Stdout(out) => Some(out.data.clone()),
                _ => None,
            })
            .collect();
        assert!(stdout.contains("streamed"));
        assert!(matches!(
            events.last(),
            Some(pb::shell_stream::Event::Exit(exit)) if exit.code == 0
        ));
    }

    #[tokio::test]
    async fn shell_stream_pushes_output_while_the_command_is_still_running() {
        let (mut host, mut streamed) = host();

        // Prints, then keeps running. If output were collected and dumped at
        // the end, nothing would be readable here.
        let handle = tokio::spawn(async move {
            host.handle(exec(pb::exec_server_message::Message::ShellStreamArgs(
                pb::ShellArgs {
                    command: test_shell_command(
                        "echo early; sleep 2",
                        "echo early & ping -n 3 127.0.0.1 > nul",
                    ),
                    timeout: 5000,
                    ..Default::default()
                },
            )))
            .await
        });

        let early = tokio::time::timeout(std::time::Duration::from_millis(1500), async {
            loop {
                if let Some(message) = streamed.recv().await {
                    let pb::exec_client_message::Message::ShellStream(stream) =
                        exec_reply(&message)
                    else {
                        continue;
                    };
                    if let Some(pb::shell_stream::Event::Stdout(out)) = &stream.event {
                        if out.data.contains("early") {
                            return out.data.clone();
                        }
                    }
                }
            }
        })
        .await;

        assert!(
            early.is_ok(),
            "stdout must reach the agent before the command exits"
        );
        handle.await.expect("the stream task finishes");
    }

    #[tokio::test]
    async fn background_shells_accept_stdin_and_die_on_shutdown() {
        let (mut host, _streamed) = host();

        let spawned = host
            .handle(exec(
                pb::exec_server_message::Message::BackgroundShellSpawnArgs(
                    pb::BackgroundShellSpawnArgs {
                        command: test_shell_command("cat > /dev/null", "more > nul"),
                        ..Default::default()
                    },
                ),
            ))
            .await;
        let pb::exec_client_message::Message::BackgroundShellSpawnResult(result) =
            exec_reply(&spawned[0])
        else {
            panic!("expected a spawn result");
        };
        let Some(pb::background_shell_spawn_result::Result::Success(success)) = &result.result
        else {
            panic!("expected a spawned shell");
        };
        let shell_id = success.shell_id;

        let wrote = host
            .handle(exec(
                pb::exec_server_message::Message::WriteShellStdinArgs(pb::WriteShellStdinArgs {
                    shell_id,
                    chars: "hello\n".into(),
                }),
            ))
            .await;
        let pb::exec_client_message::Message::WriteShellStdinResult(result) = exec_reply(&wrote[0])
        else {
            panic!("expected a stdin result");
        };
        assert!(matches!(
            result.result,
            Some(pb::write_shell_stdin_result::Result::Success(_))
        ));

        host.shutdown().await;
        assert!(host.shells.is_empty(), "cancel must not leak shells");
    }

    #[tokio::test]
    async fn writing_to_an_unknown_shell_is_an_error_not_a_panic() {
        let (mut host, _streamed) = host();
        let msgs = host
            .handle(exec(
                pb::exec_server_message::Message::WriteShellStdinArgs(pb::WriteShellStdinArgs {
                    shell_id: 999,
                    chars: "x".into(),
                }),
            ))
            .await;

        let pb::exec_client_message::Message::WriteShellStdinResult(result) = exec_reply(&msgs[0])
        else {
            panic!("expected a stdin result");
        };
        assert!(matches!(
            result.result,
            Some(pb::write_shell_stdin_result::Result::Error(_))
        ));
    }

    #[test]
    fn grep_lines_parse_into_file_line_text() {
        let m = parse_grep_line("src/main.rs:42:fn main() {").expect("parsed");
        assert_eq!(m.file, "src/main.rs");
        assert_eq!(m.matches[0].line_number, 42);
        assert_eq!(m.matches[0].content, "fn main() {");
        assert!(parse_grep_line("not a grep line").is_none());
    }
}
