//! Claude CLI subprocess runner.
//!
//! [`ClaudeRunner`] spawns `claude -p` processes for each turn, using the
//! `--output-format stream-json` protocol. Multi-turn continuation is handled
//! by passing `--resume <session_id>` on subsequent turns.
//!
//! Each turn is a standalone process — no long-lived JSON-RPC connection.

use std::path::PathBuf;

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio_util::codec::{FramedRead, LinesCodec};
use std::process::Stdio;

use crate::domain::{truncate, AgentEvent};
use crate::error::{Error, Result};

use super::protocol::{ContentBlock, StreamEvent};

// -------------------------------------------------------------------------- //
// Configuration
// -------------------------------------------------------------------------- //

/// Runtime configuration for a Claude CLI runner.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Base command used to launch Claude Code (e.g. `"claude"`).
    pub command: String,

    /// Absolute path to the project workspace. Used as the working directory
    /// for the subprocess.
    pub workspace_path: PathBuf,

    /// Timeout in milliseconds for an entire agent turn.
    pub turn_timeout_ms: u64,
}

// -------------------------------------------------------------------------- //
// ClaudeRunner
// -------------------------------------------------------------------------- //

/// Spawns per-turn `claude -p` processes and streams their output.
pub struct ClaudeRunner {
    config: SessionConfig,
    /// Session ID captured from the first `system.init` event, used for
    /// `--resume` on subsequent turns.
    session_id: Option<String>,
}

impl ClaudeRunner {
    /// Create a new runner. No process is spawned until `run_turn` is called.
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            session_id: None,
        }
    }

    /// Run a single turn by spawning a `claude -p` process.
    ///
    /// The prompt is written to stdin, then the process streams newline-delimited
    /// JSON events on stdout until a `result` event is emitted.
    ///
    /// On subsequent turns (when `session_id` is set), `--resume <id>` is passed
    /// to continue the conversation.
    pub async fn run_turn(
        &mut self,
        prompt: &str,
        event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        // ---- 1. Build command -------------------------------------------- //
        let mut cmd_str = format!(
            "{} -p - --output-format stream-json --verbose --dangerously-skip-permissions",
            self.config.command
        );
        if let Some(ref sid) = self.session_id {
            cmd_str.push_str(&format!(" --resume {sid}"));
        }

        tracing::debug!(command = %cmd_str, "Spawning claude process");

        // ---- 2. Spawn process -------------------------------------------- //
        let mut cmd = Command::new("bash");
        cmd.args(["-lc", &cmd_str])
            .current_dir(&self.config.workspace_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_remove("CLAUDECODE");

        let mut process = cmd.spawn().map_err(|_| Error::ClaudeNotFound {
            command: self.config.command.clone(),
        })?;

        // ---- 3. Write prompt to stdin, then close it --------------------- //
        let mut stdin = process
            .stdin
            .take()
            .expect("stdin must be piped (configured above)");
        let stdout = process
            .stdout
            .take()
            .expect("stdout must be piped (configured above)");
        let stderr = process
            .stderr
            .take()
            .expect("stderr must be piped (configured above)");

        // Write prompt and close stdin so claude knows input is complete.
        stdin.write_all(prompt.as_bytes()).await.map_err(|e| Error::TurnFailed {
            message: format!("write prompt to stdin: {e}"),
        })?;
        drop(stdin);

        // ---- 4. Drain stderr in background ------------------------------- //
        let stderr_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = String::new();
            while let Ok(n) = reader.read_line(&mut buf).await {
                if n == 0 {
                    break;
                }
                tracing::debug!(target: "claude_stderr", "{}", buf.trim_end());
                buf.clear();
            }
        });

        // ---- 5. Stream stdout events ------------------------------------- //
        let lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(10 * 1024 * 1024));
        let result = self
            .stream_events(lines, &mut process, event_tx, cancel)
            .await;

        // ---- 6. Cleanup -------------------------------------------------- //
        stderr_handle.abort();

        // Ensure the process is killed if it's still running.
        if result.is_err() {
            stop_process(&mut process).await;
        }

        result
    }

    /// Read and process stream events until a terminal `result` event.
    async fn stream_events(
        &mut self,
        mut lines: FramedRead<tokio::process::ChildStdout, LinesCodec>,
        process: &mut Child,
        event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(self.config.turn_timeout_ms);

        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or_default();

            let line = tokio::select! {
                result = tokio::time::timeout(remaining, lines.next()) => {
                    match result {
                        Err(_elapsed) => {
                            stop_process(process).await;
                            return Err(Error::TurnTimeout);
                        }
                        Ok(None) => {
                            // stdout closed without a result event
                            return Err(Error::TurnFailed {
                                message: "process exited without result event".to_string(),
                            });
                        }
                        Ok(Some(Err(e))) => {
                            return Err(Error::TurnFailed {
                                message: format!("codec error: {e}"),
                            });
                        }
                        Ok(Some(Ok(l))) => l,
                    }
                }
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        stop_process(process).await;
                        return Err(Error::TurnCancelled);
                    }
                    continue;
                }
            };

            // Parse the line as a StreamEvent.
            let event: StreamEvent = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => {
                    let _ = event_tx
                        .send(AgentEvent::OtherMessage { raw: line })
                        .await;
                    continue;
                }
            };

            match event {
                StreamEvent::System(sys) => {
                    if sys.subtype.as_deref() == Some("init") {
                        if let Some(ref sid) = sys.session_id {
                            self.session_id = Some(sid.clone());
                            let _ = event_tx
                                .send(AgentEvent::Notification {
                                    message: format!("Session started: {sid}"),
                                })
                                .await;
                        }
                    }
                }

                StreamEvent::Assistant(evt) => {
                    // Process content blocks.
                    for block in &evt.message.content {
                        match block {
                            ContentBlock::Text { text } => {
                                let _ = event_tx
                                    .send(AgentEvent::AssistantText {
                                        text: truncate(text.trim(), 500),
                                    })
                                    .await;
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                let _ = event_tx
                                    .send(AgentEvent::ToolEvent {
                                        tool_name: name.clone(),
                                        phase: "start".to_string(),
                                        summary: truncate(&input.to_string(), 120),
                                    })
                                    .await;
                            }
                            ContentBlock::ToolResult { .. } | ContentBlock::Other => {}
                        }
                    }

                    // Emit usage update if present.
                    if let Some(usage) = &evt.message.usage {
                        let _ = event_tx
                            .send(AgentEvent::UsageUpdate {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                            })
                            .await;
                    }
                }

                StreamEvent::Result(res) => {
                    // Emit final usage if present.
                    if let Some(usage) = &res.usage {
                        let _ = event_tx
                            .send(AgentEvent::UsageUpdate {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                            })
                            .await;
                    }

                    // Emit turn result.
                    let success = res.subtype.as_deref() == Some("success");
                    let _ = event_tx
                        .send(AgentEvent::TurnResult {
                            success,
                            duration_ms: res.duration_ms,
                            total_cost_usd: res.total_cost_usd,
                            num_turns: res.num_turns,
                        })
                        .await;

                    // Capture session_id from result if we don't have one yet.
                    if self.session_id.is_none() {
                        if let Some(sid) = res.session_id {
                            self.session_id = Some(sid);
                        }
                    }

                    if success {
                        return Ok(());
                    } else {
                        return Err(Error::TurnFailed {
                            message: res
                                .result
                                .unwrap_or_else(|| "unknown error".to_string()),
                        });
                    }
                }

                StreamEvent::RateLimitEvent(_) | StreamEvent::Unknown => {
                    let _ = event_tx
                        .send(AgentEvent::OtherMessage { raw: line })
                        .await;
                }
            }
        }
    }
}

// -------------------------------------------------------------------------- //
// Helpers
// -------------------------------------------------------------------------- //

/// Send SIGTERM to the process, wait briefly, then SIGKILL if still running.
async fn stop_process(process: &mut Child) {
    if let Some(pid) = process.id() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        loop {
            match process.try_wait() {
                Ok(Some(_)) => return,
                _ => {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    let _ = process.start_kill();
}


// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_runner_new() {
        let config = SessionConfig {
            command: "claude".to_string(),
            workspace_path: PathBuf::from("/tmp/test"),
            turn_timeout_ms: 600_000,
        };
        let runner = ClaudeRunner::new(config);
        assert!(runner.session_id.is_none());
    }

    #[test]
    fn test_session_config_clone() {
        let config = SessionConfig {
            command: "claude".to_string(),
            workspace_path: PathBuf::from("/tmp/test"),
            turn_timeout_ms: 600_000,
        };
        let cloned = config.clone();
        assert_eq!(cloned.command, "claude");
        assert_eq!(cloned.turn_timeout_ms, 600_000);
    }

}
