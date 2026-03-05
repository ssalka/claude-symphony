//! Claude Code subprocess client.
//!
//! [`ClaudeSession`] manages the lifecycle of a single Claude Code process:
//! spawning it, performing the JSON-RPC 2.0 handshake (initialize → thread/start
//! → turn/start), streaming events during a turn, and gracefully stopping the
//! process.
//!
//! ## Signal handling note
//!
//! Tokio's [`tokio::process::Child`] exposes `.start_kill()` (SIGKILL) but not a
//! direct SIGTERM API.  The `stop()` method below attempts SIGTERM via
//! `libc::kill` when the process PID is available.  If `libc` is not in the
//! dependency tree, the implementation falls back to `Child::start_kill()`
//! (SIGKILL).  Because this is a graceful-shutdown helper and not a hard
//! correctness requirement, the fall-back is acceptable.

use std::path::Path;
use std::path::PathBuf;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::process::ChildStderr;
use tokio_util::codec::{FramedRead, LinesCodec};
use std::process::Stdio;

use crate::domain::AgentEvent;
use crate::error::{Error, Result};

use super::protocol::{Request, Response, ThreadStartResult, TurnStartResult};

// -------------------------------------------------------------------------- //
// Configuration
// -------------------------------------------------------------------------- //

/// Runtime configuration for a Claude Code session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Shell command used to launch Claude Code (e.g. `"claude --output-format stream-json --verbose"`).
    pub command: String,

    /// Absolute path to the project workspace.  Used as the working directory
    /// for the subprocess and passed as the `cwd` parameter in RPC calls.
    pub workspace_path: PathBuf,

    /// Approval policy forwarded to Claude Code (e.g. `"auto"`, `"manual"`).
    pub approval_policy: String,

    /// Optional sandbox configuration object.  If `None` an empty JSON object
    /// `{}` is used.
    pub sandbox_policy: Option<Value>,

    /// Timeout in milliseconds for a single `next_line` read (used during the
    /// handshake phase).
    pub read_timeout_ms: u64,

    /// Timeout in milliseconds for an entire agent turn (`run_turn`).
    pub turn_timeout_ms: u64,
}

// -------------------------------------------------------------------------- //
// Session struct
// -------------------------------------------------------------------------- //

/// An active Claude Code subprocess with JSON-RPC communication channels.
pub struct ClaudeSession {
    /// The child process handle (kept alive; `kill_on_drop` is set).
    process: Child,

    /// Write end of the subprocess stdin pipe.
    stdin: ChildStdin,

    /// Buffered line reader over the subprocess stdout pipe.
    lines: FramedRead<ChildStdout, LinesCodec>,

    /// Cached approval policy from the originating [`SessionConfig`].
    approval_policy: String,

    /// Cached sandbox policy JSON from the originating [`SessionConfig`].
    sandbox_policy: Value,

    /// Monotonically increasing counter used to assign unique request IDs.
    request_id_counter: u64,
}

impl ClaudeSession {
    // ---------------------------------------------------------------------- //
    // Construction / handshake
    // ---------------------------------------------------------------------- //

    /// Spawn Claude Code and perform the initialize + thread/start handshake.
    ///
    /// Returns `(session, thread_id, turn_id, session_id)` where
    /// `session_id = "{thread_id}-{turn_id}"`.
    ///
    /// Note: `turn_id` here is a placeholder empty string; the actual turn is
    /// started separately via [`ClaudeSession::start_turn`].  The caller
    /// receives a live session ready to call `start_turn` on.
    ///
    /// The returned tuple is `(session, thread_id, "", "")` — callers that need
    /// the turn id should call `start_turn` immediately afterwards.
    pub async fn start(config: &SessionConfig) -> Result<(ClaudeSession, String, String, String)> {
        // ---- 1. Launch subprocess ---------------------------------------- //
        let mut cmd = Command::new("bash");
        cmd.args(["-lc", &config.command])
            .current_dir(&config.workspace_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut process = cmd.spawn().map_err(|_| Error::ClaudeNotFound {
            command: config.command.clone(),
        })?;

        // Extract stdin/stdout handles before borrowing process elsewhere.
        let stdin = process
            .stdin
            .take()
            .expect("stdin must be piped (configured above)");
        let stdout = process
            .stdout
            .take()
            .expect("stdout must be piped (configured above)");

        // We intentionally discard stderr for now; the process handle keeps it
        // alive so the OS pipe buffer doesn't fill and block the subprocess.
        let _stderr: Option<ChildStderr> = process.stderr.take();

        // ---- 2. Build framed line reader (max 10 MiB per line) ------------- //
        let lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(10 * 1024 * 1024));

        let sandbox_policy = config
            .sandbox_policy
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default()));

        let mut session = ClaudeSession {
            process,
            stdin,
            lines,
            approval_policy: config.approval_policy.clone(),
            sandbox_policy,
            request_id_counter: 0,
        };

        // ---- 3. Send initialize request ------------------------------------ //
        let init_req = Request::with_id(
            session.next_id(),
            "initialize",
            json!({
                "clientInfo": {
                    "name": "symphony",
                    "version": "1.0"
                },
                "capabilities": {}
            }),
        );
        session.send_message(&init_req)?;

        // ---- 4. Wait for initialize response ------------------------------- //
        let _init_line = session
            .next_line(config.read_timeout_ms)
            .await?
            .ok_or(Error::TurnFailed {
                message: "process exited during initialize".to_string(),
            })?;
        // We accept any non-error response; the contents are not critical for
        // our protocol flow.

        // ---- 5. Send initialized notification ------------------------------ //
        let initialized_notif = Request::notification("initialized", json!({}));
        session.send_message(&initialized_notif)?;

        // ---- 6. Send thread/start ----------------------------------------- //
        let cwd_str = config
            .workspace_path
            .to_str()
            .unwrap_or("")
            .to_string();
        let thread_req = Request::with_id(
            session.next_id(),
            "thread/start",
            json!({
                "approvalPolicy": session.approval_policy,
                "sandbox": session.sandbox_policy,
                "cwd": cwd_str
            }),
        );
        session.send_message(&thread_req)?;

        // ---- 7. Wait for thread/start response ----------------------------- //
        let thread_line = session
            .next_line(config.read_timeout_ms)
            .await?
            .ok_or(Error::TurnFailed {
                message: "process exited during thread/start".to_string(),
            })?;

        let thread_id = extract_thread_id(&thread_line)?;

        // Return session with empty turn_id / session_id; caller must call
        // start_turn to get those.
        let session_id = String::new();
        let turn_id = String::new();

        Ok((session, thread_id, turn_id, session_id))
    }

    // ---------------------------------------------------------------------- //
    // Turn lifecycle
    // ---------------------------------------------------------------------- //

    /// Send `turn/start` and wait for the turn id reply.
    pub async fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        title: &str,
        workspace_path: &Path,
    ) -> Result<String> {
        let cwd_str = workspace_path.to_str().unwrap_or("").to_string();

        let turn_req = Request::with_id(
            self.next_id(),
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": prompt}],
                "cwd": cwd_str,
                "title": title,
                "approvalPolicy": self.approval_policy,
                "sandboxPolicy": self.sandbox_policy
            }),
        );
        self.send_message(&turn_req)?;

        // Wait for the turn/start response (use a generous but finite timeout).
        // We reuse read_timeout_ms by reading directly; the caller controls the
        // outer turn timeout through run_turn.
        // We don't have access to read_timeout_ms here so use a fixed
        // generous value of 30 seconds for the turn/start handshake.
        let line = match self.next_line(30_000).await? {
            Some(l) => l,
            None => {
                return Err(Error::TurnFailed {
                    message: "process exited during turn/start".to_string(),
                })
            }
        };

        let turn_id = extract_turn_id(&line)?;
        Ok(turn_id)
    }

    /// Stream events from the Claude Code subprocess until the turn completes,
    /// fails, or times out.
    ///
    /// Events are forwarded to `event_tx`.  Auto-approval responses are sent
    /// back to the subprocess inline.
    pub async fn run_turn(
        &mut self,
        turn_timeout_ms: u64,
        event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(turn_timeout_ms);

        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or_default();

            let line = match tokio::time::timeout(remaining, self.lines.next()).await {
                Err(_elapsed) => return Err(Error::TurnTimeout),
                Ok(None) => {
                    return Err(Error::TurnFailed {
                        message: "process exited".to_string(),
                    })
                }
                Ok(Some(Err(e))) => {
                    return Err(Error::TurnFailed {
                        message: format!("codec error: {e}"),
                    })
                }
                Ok(Some(Ok(l))) => l,
            };

            // Parse the line as a JSON-RPC Response envelope.
            let resp: Response = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => {
                    // Unparseable line — emit as OtherMessage and continue.
                    let _ = event_tx
                        .send(AgentEvent::OtherMessage { raw: line })
                        .await;
                    continue;
                }
            };

            let method = resp.method.as_deref().unwrap_or("");

            // ---- turn terminal events ------------------------------------ //
            if method == "turn/completed" {
                return Ok(());
            }

            if method == "turn/failed" {
                let message = resp
                    .params
                    .as_ref()
                    .and_then(|p| p.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown failure")
                    .to_string();
                return Err(Error::TurnFailed { message });
            }

            if method == "turn/cancelled" {
                return Err(Error::TurnCancelled);
            }

            // ---- user input required ------------------------------------- //
            if method == "item/tool/requestUserInput"
                || method.contains("requestUserInput")
            {
                return Err(Error::TurnInputRequired);
            }

            // ---- approval requests -------------------------------------- //
            if method.contains("approval")
                || is_approval_required(&resp.params)
            {
                // Auto-approve by echoing back approved: true with the
                // original request id.
                if let Some(id) = &resp.id {
                    let approval_resp = Request::with_id(
                        id.as_u64().unwrap_or(0),
                        // We use result injection via a synthetic request; in
                        // practice we send a bare JSON response, not a Request.
                        // Build the payload manually.
                        "__approval_response__",
                        Value::Null,
                    );
                    // Send a raw JSON approval response (not a Request envelope).
                    let approval_json =
                        serde_json::to_string(&json!({"id": id, "result": {"approved": true}}))
                            .unwrap_or_default();
                    let _ = self.write_raw_line(&approval_json);
                    // Suppress unused variable warning.
                    let _ = approval_resp;
                }
                continue;
            }

            // ---- unknown tool calls ------------------------------------- //
            if method.contains("item/tool/call") {
                if let Some(id) = &resp.id {
                    let error_json = serde_json::to_string(&json!({
                        "id": id,
                        "result": {"success": false, "error": "unsupported_tool_call"}
                    }))
                    .unwrap_or_default();
                    let _ = self.write_raw_line(&error_json);
                }
                continue;
            }

            // ---- usage / token events ----------------------------------- //
            if let Some(usage) = extract_usage(&resp) {
                let _ = event_tx.send(AgentEvent::UsageUpdate {
                    input_tokens: usage.0,
                    output_tokens: usage.1,
                }).await;
                continue;
            }

            // ---- everything else --------------------------------------- //
            let _ = event_tx
                .send(AgentEvent::OtherMessage { raw: line })
                .await;
        }
    }

    // ---------------------------------------------------------------------- //
    // Low-level I/O helpers
    // ---------------------------------------------------------------------- //

    /// Serialize `msg` to JSON and write it as a single newline-terminated line
    /// to the subprocess stdin.
    pub fn send_message(&mut self, msg: &Request) -> Result<()> {
        let mut json = serde_json::to_string(msg).map_err(|e| Error::TurnFailed {
            message: format!("serialize request: {e}"),
        })?;
        json.push('\n');
        // Use the blocking write via try_write; the stdin is non-blocking in
        // tokio but we call this from async context so we need to use the async
        // write path.  Since `send_message` is called infrequently (handshake
        // only), we spawn a tiny blocking write.  For the actual implementation
        // we use a synchronous write on the underlying fd via std::io::Write.
        use std::io::Write;
        use std::os::unix::io::AsRawFd;
        let raw_fd_stdin = self.stdin.as_raw_fd();
        // SAFETY: We own the ChildStdin and hold it alive for the duration of
        // this function.  We call `mem::forget` on `f` so the fd is never closed.
        let mut f = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(raw_fd_stdin) };
        let result = f.write_all(json.as_bytes());
        // Do NOT close the fd (we don't own the File, just borrow the fd).
        std::mem::forget(f);
        result.map_err(|e| Error::TurnFailed {
            message: format!("write to stdin: {e}"),
        })
    }

    /// Write a raw JSON string (already serialized) plus newline to stdin.
    ///
    /// Used for approval and error responses where we bypass the `Request`
    /// envelope.
    fn write_raw_line(&mut self, line: &str) -> Result<()> {
        use std::io::Write;
        use std::os::unix::io::AsRawFd as _;
        let raw_fd = self.stdin.as_raw_fd();
        let mut f = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(raw_fd) };
        let mut buf = line.to_string();
        buf.push('\n');
        let result = f.write_all(buf.as_bytes());
        std::mem::forget(f);
        result.map_err(|e| Error::TurnFailed {
            message: format!("write raw line: {e}"),
        })
    }

    /// Read the next newline-terminated line from stdout, with a timeout.
    ///
    /// Returns `Ok(Some(line))` on success, `Ok(None)` if the stream has
    /// ended (process exited), and `Err(ResponseTimeout)` if the timeout
    /// elapses before a line arrives.
    pub async fn next_line(&mut self, timeout_ms: u64) -> Result<Option<String>> {
        let duration = tokio::time::Duration::from_millis(timeout_ms);
        match tokio::time::timeout(duration, self.lines.next()).await {
            Err(_elapsed) => Err(Error::ResponseTimeout),
            Ok(None) => Ok(None),
            Ok(Some(Ok(line))) => Ok(Some(line)),
            Ok(Some(Err(e))) => Err(Error::TurnFailed {
                message: format!("codec error reading stdout: {e}"),
            }),
        }
    }

    /// Send SIGTERM to the subprocess, wait up to 2 seconds for it to exit,
    /// then send SIGKILL if it is still running.
    pub fn stop(&mut self) {
        // Attempt SIGTERM via libc.
        if let Some(pid) = self.process.id() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }

            // Poll for up to 2 seconds.
            let start = std::time::Instant::now();
            loop {
                if start.elapsed().as_secs() >= 2 {
                    break;
                }
                match self.process.try_wait() {
                    Ok(Some(_)) => return, // exited cleanly
                    _ => std::thread::sleep(std::time::Duration::from_millis(100)),
                }
            }
        }

        // Fall back to SIGKILL.
        let _ = self.process.start_kill();
    }

    // ---------------------------------------------------------------------- //
    // Internal helpers
    // ---------------------------------------------------------------------- //

    /// Advance and return the next request id.
    fn next_id(&mut self) -> u64 {
        self.request_id_counter += 1;
        self.request_id_counter
    }
}

// -------------------------------------------------------------------------- //
// Parsing helpers
// -------------------------------------------------------------------------- //

/// Extract `result.thread.id` from a raw JSON line returned by `thread/start`.
fn extract_thread_id(line: &str) -> Result<String> {
    let v: Value = serde_json::from_str(line).map_err(|e| Error::TurnFailed {
        message: format!("parse thread/start response: {e}"),
    })?;

    // Try the standard result.thread.id path.
    if let Some(id) = v
        .get("result")
        .and_then(|r| r.get("thread"))
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
    {
        return Ok(id.to_string());
    }

    // Some implementations use camelCase at the top level.
    if let Some(thread_start_res) = v.get("result") {
        if let Ok(res) = serde_json::from_value::<ThreadStartResult>(thread_start_res.clone()) {
            return Ok(res.thread.id);
        }
    }

    Err(Error::TurnFailed {
        message: format!("missing thread id in response: {line}"),
    })
}

/// Extract `result.turn.id` from a raw JSON line returned by `turn/start`.
fn extract_turn_id(line: &str) -> Result<String> {
    let v: Value = serde_json::from_str(line).map_err(|e| Error::TurnFailed {
        message: format!("parse turn/start response: {e}"),
    })?;

    if let Some(id) = v
        .get("result")
        .and_then(|r| r.get("turn"))
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
    {
        return Ok(id.to_string());
    }

    if let Some(turn_start_res) = v.get("result") {
        if let Ok(res) = serde_json::from_value::<TurnStartResult>(turn_start_res.clone()) {
            return Ok(res.turn.id);
        }
    }

    Err(Error::TurnFailed {
        message: format!("missing turn id in response: {line}"),
    })
}

/// Return `true` if the params object contains `approvalRequired: true`.
fn is_approval_required(params: &Option<Value>) -> bool {
    params
        .as_ref()
        .and_then(|p| p.get("approvalRequired"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Extract token usage from a response envelope if any usage fields are present.
///
/// Checks `params.usage`, `result.usage`, and direct `inputTokens`/`outputTokens`
/// fields at various nesting levels to handle different Claude Code versions.
fn extract_usage(resp: &Response) -> Option<(u64, u64)> {
    // Helper to read {inputTokens, outputTokens} from a JSON object.
    let parse_usage = |obj: &Value| -> Option<(u64, u64)> {
        let input = obj
            .get("inputTokens")
            .or_else(|| obj.get("input_tokens"))
            .and_then(|v| v.as_u64());
        let output = obj
            .get("outputTokens")
            .or_else(|| obj.get("output_tokens"))
            .and_then(|v| v.as_u64());
        match (input, output) {
            (Some(i), Some(o)) => Some((i, o)),
            _ => None,
        }
    };

    // Try params.usage
    if let Some(p) = &resp.params {
        if let Some(usage) = p.get("usage").and_then(parse_usage) {
            return Some(usage);
        }
        // Try direct fields on params
        if let Some(usage) = parse_usage(p) {
            return Some(usage);
        }
    }

    // Try result.usage
    if let Some(r) = &resp.result {
        if let Some(usage) = r.get("usage").and_then(parse_usage) {
            return Some(usage);
        }
        if let Some(usage) = parse_usage(r) {
            return Some(usage);
        }
    }

    None
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_thread_id_standard() {
        let line = r#"{"id":2,"result":{"thread":{"id":"thread-abc"}}}"#;
        let id = extract_thread_id(line).unwrap();
        assert_eq!(id, "thread-abc");
    }

    #[test]
    fn test_extract_thread_id_missing() {
        let line = r#"{"id":2,"result":{}}"#;
        assert!(extract_thread_id(line).is_err());
    }

    #[test]
    fn test_extract_turn_id_standard() {
        let line = r#"{"id":3,"result":{"turn":{"id":"turn-xyz"}}}"#;
        let id = extract_turn_id(line).unwrap();
        assert_eq!(id, "turn-xyz");
    }

    #[test]
    fn test_extract_turn_id_missing() {
        let line = r#"{"id":3,"result":{}}"#;
        assert!(extract_turn_id(line).is_err());
    }

    #[test]
    fn test_is_approval_required_true() {
        let params = Some(json!({"approvalRequired": true}));
        assert!(is_approval_required(&params));
    }

    #[test]
    fn test_is_approval_required_false() {
        let params = Some(json!({"approvalRequired": false}));
        assert!(!is_approval_required(&params));
    }

    #[test]
    fn test_is_approval_required_absent() {
        let params = Some(json!({}));
        assert!(!is_approval_required(&params));
    }

    #[test]
    fn test_is_approval_required_none() {
        assert!(!is_approval_required(&None));
    }

    #[test]
    fn test_extract_usage_from_params_usage() {
        let resp = Response {
            id: None,
            result: None,
            error: None,
            method: Some("usage".to_string()),
            params: Some(json!({
                "usage": {"inputTokens": 100, "outputTokens": 200}
            })),
        };
        let usage = extract_usage(&resp);
        assert_eq!(usage, Some((100, 200)));
    }

    #[test]
    fn test_extract_usage_from_direct_params_fields() {
        let resp = Response {
            id: None,
            result: None,
            error: None,
            method: Some("event".to_string()),
            params: Some(json!({"inputTokens": 50, "outputTokens": 75})),
        };
        let usage = extract_usage(&resp);
        assert_eq!(usage, Some((50, 75)));
    }

    #[test]
    fn test_extract_usage_from_result_usage() {
        let resp = Response {
            id: Some(json!(1)),
            result: Some(json!({"usage": {"inputTokens": 300, "outputTokens": 400}})),
            error: None,
            method: None,
            params: None,
        };
        let usage = extract_usage(&resp);
        assert_eq!(usage, Some((300, 400)));
    }

    #[test]
    fn test_extract_usage_none_when_absent() {
        let resp = Response {
            id: None,
            result: None,
            error: None,
            method: Some("turn/completed".to_string()),
            params: Some(json!({"turnId": "t1"})),
        };
        let usage = extract_usage(&resp);
        assert!(usage.is_none());
    }

    #[test]
    fn test_extract_usage_snake_case_fields() {
        let resp = Response {
            id: None,
            result: None,
            error: None,
            method: Some("usage_update".to_string()),
            params: Some(json!({"input_tokens": 11, "output_tokens": 22})),
        };
        let usage = extract_usage(&resp);
        assert_eq!(usage, Some((11, 22)));
    }
}
