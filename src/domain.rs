use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;

// -------------------------------------------------------------------------- //
// Tracker types
// -------------------------------------------------------------------------- //

/// A reference to another issue that blocks this one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerRef {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub state: Option<String>,
}

/// Normalized issue representation (spec §4.1.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub state: String,
    pub branch_name: Option<String>,
    pub url: Option<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<BlockerRef>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

// -------------------------------------------------------------------------- //
// Live session tracking
// -------------------------------------------------------------------------- //

/// Tracks the real-time state of an active Claude agent session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveSession {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub pid: u32,
    /// Most recent event type string received from the agent.
    pub last_event: Option<String>,
    pub last_event_timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pub last_event_message: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens last reported upstream — used for computing deltas.
    pub last_reported_input_tokens: u64,
    pub last_reported_output_tokens: u64,
    pub turn_count: u32,
}

// -------------------------------------------------------------------------- //
// Orchestrator accounting
// -------------------------------------------------------------------------- //

/// Cumulative token usage and wall-clock time across all Claude invocations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub seconds_running: f64,
}

// -------------------------------------------------------------------------- //
// Orchestrator state entries
// -------------------------------------------------------------------------- //

/// Entry for an issue that is currently being worked on by an agent.
#[derive(Debug)]
pub struct RunningEntry {
    /// Snapshot of the issue at dispatch time.
    pub issue: Issue,
    pub attempt: u32,
    /// Unique ID assigned at dispatch time; used to detect stale worker exits.
    pub dispatch_id: u64,
    pub live_session: Option<LiveSession>,
    pub started_at: std::time::Instant,
    /// Cancel this token to request cancellation of the agent.
    pub cancel_token: CancellationToken,
}

/// Entry for an issue that is waiting to be retried after a failure.
#[derive(Debug)]
pub struct RetryEntry {
    pub issue_id: String,
    pub identifier: String,
    /// 1-based attempt counter (1 = first retry).
    pub attempt: u32,
    /// Unix epoch milliseconds at which the retry should fire.
    pub due_at_ms: u64,
    pub error: String,
    /// Abort this handle to cancel the retry timer task.
    pub abort_handle: tokio::task::JoinHandle<()>,
}

// -------------------------------------------------------------------------- //
// Serializable snapshot used by the HTTP status endpoint
// -------------------------------------------------------------------------- //

/// Serializable snapshot of a `RunningEntry` (excludes non-serializable fields).
#[derive(Debug, Serialize)]
pub struct RunningEntrySnapshot {
    pub issue: Issue,
    pub attempt: u32,
    pub live_session: Option<LiveSession>,
    /// Seconds elapsed since the agent was dispatched.
    pub elapsed_secs: f64,
}

/// Serializable snapshot of a `RetryEntry`.
#[derive(Debug, Serialize)]
pub struct RetryEntrySnapshot {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: u32,
    pub due_at_ms: u64,
    pub error: String,
}

/// Serializable point-in-time snapshot of `OrchestratorState`, used by the
/// HTTP `/status` endpoint.
#[derive(Debug, Serialize)]
pub struct OrchestratorSnapshot {
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: usize,
    pub running: HashMap<String, RunningEntrySnapshot>,
    pub claimed: HashSet<String>,
    pub retry_attempts: HashMap<String, RetryEntrySnapshot>,
    pub completed: HashSet<String>,
    pub claude_totals: ClaudeTotals,
    pub claude_rate_limits: Option<serde_json::Value>,
}

// -------------------------------------------------------------------------- //
// Top-level orchestrator state
// -------------------------------------------------------------------------- //

/// Full runtime state of the orchestrator.
///
/// Not directly serializable due to `RunningEntry` and `RetryEntry` containing
/// `Instant`, `CancellationToken`, and `JoinHandle`. Use [`OrchestratorSnapshot`] for
/// serialization (e.g., HTTP status responses).
#[derive(Debug)]
pub struct OrchestratorState {
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: usize,
    pub running: HashMap<String, RunningEntry>,
    pub claimed: HashSet<String>,
    pub retry_attempts: HashMap<String, RetryEntry>,
    pub completed: HashSet<String>,
    pub claude_totals: ClaudeTotals,
    pub claude_rate_limits: Option<serde_json::Value>,
    /// Monotonically increasing counter; incremented each time a worker is dispatched.
    pub next_dispatch_id: u64,
}

impl OrchestratorState {
    /// Produce a serializable snapshot of the current state.
    pub fn snapshot(&self) -> OrchestratorSnapshot {
        let running = self
            .running
            .iter()
            .map(|(k, v)| {
                let elapsed_secs = v.started_at.elapsed().as_secs_f64();
                (
                    k.clone(),
                    RunningEntrySnapshot {
                        issue: v.issue.clone(),
                        attempt: v.attempt,
                        live_session: v.live_session.clone(),
                        elapsed_secs,
                    },
                )
            })
            .collect();

        let retry_attempts = self
            .retry_attempts
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    RetryEntrySnapshot {
                        issue_id: v.issue_id.clone(),
                        identifier: v.identifier.clone(),
                        attempt: v.attempt,
                        due_at_ms: v.due_at_ms,
                        error: v.error.clone(),
                    },
                )
            })
            .collect();

        OrchestratorSnapshot {
            poll_interval_ms: self.poll_interval_ms,
            max_concurrent_agents: self.max_concurrent_agents,
            running,
            claimed: self.claimed.clone(),
            retry_attempts,
            completed: self.completed.clone(),
            claude_totals: self.claude_totals.clone(),
            claude_rate_limits: self.claude_rate_limits.clone(),
        }
    }
}

// -------------------------------------------------------------------------- //
// Workflow definition
// -------------------------------------------------------------------------- //

/// Parsed workflow file: YAML front-matter config and the Liquid prompt template.
#[derive(Debug, Clone)]
pub struct WorkflowDefinition {
    pub config: serde_yaml::Value,
    pub prompt_template: String,
}

// -------------------------------------------------------------------------- //
// Agent / worker event types
// -------------------------------------------------------------------------- //

/// Events emitted by an agent session back to the orchestrator.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Token usage update from the agent.
    UsageUpdate {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A human-readable notification message from the agent.
    Notification { message: String },
    /// Assistant text streamed from the Claude subprocess.
    AssistantText { text: String },
    /// Tool use event (start or result) from the Claude subprocess.
    ToolEvent {
        tool_name: String,
        phase: String,
        summary: String,
    },
    /// Turn result event from the Claude CLI.
    TurnResult {
        success: bool,
        duration_ms: Option<u64>,
        total_cost_usd: Option<f64>,
        num_turns: Option<u32>,
    },
    /// Any other raw message not matched by a known event type.
    OtherMessage { raw: String },
}

/// Reason a worker task exited.
#[derive(Debug)]
pub enum WorkerExitReason {
    /// Agent completed its turn successfully.
    Normal,
    /// Agent failed with an error.
    Abnormal { error: String },
}

/// Events sent by worker tasks back to the orchestrator over a channel.
#[derive(Debug)]
pub enum WorkerEvent {
    /// A token-usage or notification update from the running Claude session.
    ClaudeUpdate { issue_id: String, event: AgentEvent },
    /// The worker task has finished (success or failure).
    WorkerExited {
        issue_id: String,
        /// The `dispatch_id` assigned when this worker was dispatched.
        dispatch_id: u64,
        reason: WorkerExitReason,
    },
}

// -------------------------------------------------------------------------- //
// Shared helpers
// -------------------------------------------------------------------------- //

/// Truncate a string to at most `max_len` bytes at a valid UTF-8 boundary,
/// appending "…" if truncated.
pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max_len);
        let mut result = s[..end].to_string();
        result.push('…');
        result
    }
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_totals_default() {
        let totals = ClaudeTotals::default();
        assert_eq!(totals.input_tokens, 0);
        assert_eq!(totals.output_tokens, 0);
        assert_eq!(totals.total_tokens, 0);
        assert_eq!(totals.seconds_running, 0.0);
    }

    #[test]
    fn test_issue_serialization_roundtrip() {
        let issue = Issue {
            id: "issue-1".to_string(),
            identifier: "ENG-42".to_string(),
            title: "Fix the bug".to_string(),
            description: Some("detailed description".to_string()),
            priority: Some(1),
            state: "In Progress".to_string(),
            branch_name: Some("eng-42-fix-bug".to_string()),
            url: Some("https://linear.app/eng-42".to_string()),
            labels: vec!["bug".to_string(), "p1".to_string()],
            blocked_by: vec![BlockerRef {
                id: Some("blocker-1".to_string()),
                identifier: Some("ENG-10".to_string()),
                state: Some("In Progress".to_string()),
            }],
            created_at: None,
            updated_at: None,
        };

        let json = serde_json::to_string(&issue).expect("serialize Issue");
        let decoded: Issue = serde_json::from_str(&json).expect("deserialize Issue");
        assert_eq!(decoded.id, issue.id);
        assert_eq!(decoded.identifier, issue.identifier);
        assert_eq!(decoded.labels.len(), 2);
        assert_eq!(decoded.blocked_by.len(), 1);
    }

    #[test]
    fn test_agent_event_clone() {
        let ev = AgentEvent::UsageUpdate {
            input_tokens: 100,
            output_tokens: 200,
        };
        let cloned = ev.clone();
        match cloned {
            AgentEvent::UsageUpdate {
                input_tokens,
                output_tokens,
            } => {
                assert_eq!(input_tokens, 100);
                assert_eq!(output_tokens, 200);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn test_agent_event_new_variants_clone() {
        let text_ev = AgentEvent::AssistantText {
            text: "Hello".to_string(),
        };
        let cloned = text_ev.clone();
        match cloned {
            AgentEvent::AssistantText { text } => assert_eq!(text, "Hello"),
            _ => panic!("unexpected variant"),
        }

        let tool_ev = AgentEvent::ToolEvent {
            tool_name: "Read".to_string(),
            phase: "start".to_string(),
            summary: "file_path: /src".to_string(),
        };
        let cloned = tool_ev.clone();
        match cloned {
            AgentEvent::ToolEvent {
                tool_name,
                phase,
                summary,
            } => {
                assert_eq!(tool_name, "Read");
                assert_eq!(phase, "start");
                assert_eq!(summary, "file_path: /src");
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn test_worker_exit_reason_debug() {
        let reason = WorkerExitReason::Abnormal {
            error: "timeout".to_string(),
        };
        let dbg = format!("{:?}", reason);
        assert!(dbg.contains("timeout"));
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_long_string() {
        let long = "a".repeat(20);
        let result = truncate(&long, 10);
        assert!(result.starts_with("aaaaaaaaaa"));
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_truncate_exact_length() {
        let s = "a".repeat(10);
        assert_eq!(truncate(&s, 10), s);
    }

    #[test]
    fn test_truncate_multibyte_boundary() {
        // "héllo" — 'é' is 2 bytes, so cutting at byte 2 would split it.
        let s = "héllo";
        let result = truncate(s, 2);
        // Should not panic; should cut before the multibyte char if needed.
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_live_session_default() {
        let ls = LiveSession::default();
        assert_eq!(ls.session_id, "");
        assert_eq!(ls.pid, 0);
        assert_eq!(ls.input_tokens, 0);
        assert!(ls.last_event.is_none());
    }

    #[test]
    fn test_orchestrator_snapshot_serializes() {
        let state = OrchestratorState {
            poll_interval_ms: 5000,
            max_concurrent_agents: 4,
            running: HashMap::new(),
            claimed: HashSet::new(),
            retry_attempts: HashMap::new(),
            completed: HashSet::new(),
            claude_totals: ClaudeTotals::default(),
            claude_rate_limits: None,
            next_dispatch_id: 1,
        };

        let snapshot = state.snapshot();
        let json = serde_json::to_string(&snapshot).expect("serialize OrchestratorSnapshot");
        assert!(json.contains("poll_interval_ms"));
        assert!(json.contains("max_concurrent_agents"));
    }
}
