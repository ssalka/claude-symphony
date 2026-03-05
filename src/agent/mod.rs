pub mod protocol;
pub mod session;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::config::ServiceConfig;
use crate::domain::{AgentEvent, Issue, WorkerEvent, WorkflowDefinition};
use crate::error::{Error, Result};
use crate::{prompt, workspace};

use self::session::{ClaudeSession, SessionConfig};

// -------------------------------------------------------------------------- //
// AgentRunner
// -------------------------------------------------------------------------- //

/// Glues workspace creation, prompt rendering, and Claude session management
/// together for a single issue attempt.
///
/// The runner is consumed by [`AgentRunner::run`] — construct a new one for
/// each issue dispatch.
pub struct AgentRunner {
    pub config: Arc<ServiceConfig>,
    pub workflow: Arc<WorkflowDefinition>,
    /// Channel used to forward [`WorkerEvent`]s back to the orchestrator.
    pub event_tx: tokio::sync::mpsc::Sender<WorkerEvent>,
}

impl AgentRunner {
    // ---------------------------------------------------------------------- //
    // Public API
    // ---------------------------------------------------------------------- //

    /// Run an agent for `issue`, attempt number `attempt`.
    ///
    /// Steps:
    /// 1. Create (or reuse) the workspace directory.
    /// 2. Render the prompt from the workflow template.
    /// 3. Start a Claude Code session.
    /// 4. Run up to `config.agent_max_turns` turns, forwarding
    ///    [`AgentEvent`]s as [`WorkerEvent::ClaudeUpdate`] to the orchestrator.
    /// 5. Return `Ok(())` on successful completion or `Err(_)` on failure.
    ///
    /// Cancellation is signalled via `cancel_rx`.  When the oneshot fires the
    /// current turn is interrupted, the session is stopped, and
    /// [`Error::TurnCancelled`] is returned.
    pub async fn run(
        self,
        issue: Issue,
        attempt: u32,
        cancel_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<()> {
        // ---- 1. Create workspace ----------------------------------------- //
        let workspace = workspace::create_for_issue(&issue.identifier, &self.config).await?;

        // ---- 2. Render prompt -------------------------------------------- //
        let prompt = prompt::render_prompt(&self.workflow.prompt_template, &issue, Some(attempt))?;

        // ---- 3. Build title ---------------------------------------------- //
        let title = format!("{}: {}", issue.identifier, issue.title);

        // ---- 4. Start session -------------------------------------------- //
        let session_config = SessionConfig {
            command: self.config.agent_command.clone(),
            workspace_path: workspace.path.clone(),
            approval_policy: self.config.agent_approval_policy.clone(),
            sandbox_policy: self.config.agent_sandbox_policy.clone(),
            read_timeout_ms: self.config.agent_read_timeout_ms,
            turn_timeout_ms: self.config.agent_turn_timeout_ms,
        };

        let (mut session, thread_id, _turn_id, _session_id) =
            ClaudeSession::start(&session_config).await?;

        // ---- 5. Convert the oneshot cancel into a watch so it can be
        //         observed across multiple turn iterations.                  //
        let (cancel_watch_tx, mut cancel_watch_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            let _ = cancel_rx.await;
            let _ = cancel_watch_tx.send(true);
        });

        // ---- 6. Turn loop ------------------------------------------------ //
        let max_turns = self.config.agent_max_turns;
        let turn_timeout_ms = self.config.agent_turn_timeout_ms;
        let issue_id = issue.id.clone();
        let event_tx = self.event_tx.clone();

        // Start the first turn with the rendered prompt.
        session
            .start_turn(&thread_id, &prompt, &title, &workspace.path)
            .await?;

        let mut turn_count = 0u32;
        loop {
            turn_count += 1;

            // Pre-turn cancellation check.
            if *cancel_watch_rx.borrow() {
                session.stop();
                return Err(Error::TurnCancelled);
            }

            // Create an inner channel for AgentEvents from run_turn.
            let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(100);

            // Run the turn concurrently with event forwarding and cancellation
            // watching.
            //
            // We use an approach where run_turn is driven on the current task
            // via a local future, and we interleave event forwarding.
            // Because run_turn holds &mut session, we cannot split it from
            // the event forwarding.  Instead we forward events inside a
            // separate spawned task and drive run_turn here.
            let (forward_tx, forward_rx) = tokio::sync::oneshot::channel::<()>();
            let forward_event_tx = event_tx.clone();
            let forward_issue_id = issue_id.clone();
            let forward_handle = tokio::spawn(async move {
                while let Some(event) = agent_rx.recv().await {
                    let _ = forward_event_tx
                        .send(WorkerEvent::ClaudeUpdate {
                            issue_id: forward_issue_id.clone(),
                            event,
                        })
                        .await;
                }
                // Signal that all forwarding is done.
                let _ = forward_tx.send(());
            });

            // Run the actual turn, watching for cancellation at the same time.
            let turn_result: Result<()> = tokio::select! {
                result = session.run_turn(turn_timeout_ms, &agent_tx) => result,
                _ = cancel_watch_rx.changed() => {
                    // Cancellation arrived while the turn was running.
                    // Drop agent_tx to close the channel so the forwarder
                    // exits cleanly, then stop the session.
                    drop(agent_tx);
                    // Wait for the event forwarder to drain.
                    let _ = forward_rx.await;
                    forward_handle.abort();
                    session.stop();
                    return Err(Error::TurnCancelled);
                }
            };

            // Drop agent_tx so the event-forwarder task knows the turn is done.
            drop(agent_tx);

            // Wait for all events to be forwarded before proceeding.
            let _ = forward_rx.await;
            forward_handle.abort();

            // Propagate any turn errors.
            turn_result?;

            // Successful turn.  Decide whether to continue.
            if turn_count >= max_turns {
                break;
            }

            // Start a continuation turn.
            session
                .start_turn(
                    &thread_id,
                    &self.config.agent_continuation_guidance,
                    &title,
                    &workspace.path,
                )
                .await?;
        }

        Ok(())
    }
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::domain::{AgentEvent, WorkerEvent, WorkerExitReason};

    // ---------------------------------------------------------------------- //
    // Helpers
    // ---------------------------------------------------------------------- //

    fn make_config() -> Arc<ServiceConfig> {
        Arc::new(ServiceConfig {
            tracker_kind: "linear".to_string(),
            tracker_api_key: "test-key".to_string(),
            tracker_project_slug: "test-project".to_string(),
            tracker_endpoint: None,
            workspace_root: PathBuf::from("/tmp/symphony-test-workspaces"),
            workspace_after_create: None,
            workspace_before_remove: None,
            agent_command: "claude --output-format stream-json --verbose".to_string(),
            agent_approval_policy: "auto".to_string(),
            agent_sandbox_policy: None,
            agent_max_turns: 5,
            agent_read_timeout_ms: 30_000,
            agent_turn_timeout_ms: 600_000,
            agent_stall_timeout_ms: 120_000,
            agent_max_retry_backoff_ms: 300_000,
            agent_continuation_guidance: "Continue working on the task.".to_string(),
            poll_interval_ms: 30_000,
            max_concurrent_agents: 3,
            max_concurrent_agents_by_state: HashMap::new(),
            active_states: vec![],
            terminal_states: vec![],
            server_enabled: false,
            server_port: 8080,
        })
    }

    fn make_workflow() -> Arc<WorkflowDefinition> {
        Arc::new(WorkflowDefinition {
            config: serde_yaml::Value::Null,
            prompt_template: "Work on issue {{ identifier }}: {{ title }}".to_string(),
        })
    }

    fn make_issue() -> Issue {
        Issue {
            id: "issue-test-1".to_string(),
            identifier: "ENG-1".to_string(),
            title: "Test issue".to_string(),
            description: Some("A test issue".to_string()),
            priority: Some(1),
            state: "In Progress".to_string(),
            branch_name: Some("eng-1-test".to_string()),
            url: Some("https://linear.app/eng-1".to_string()),
            labels: vec!["test".to_string()],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        }
    }

    // ---------------------------------------------------------------------- //
    // Construction tests
    // ---------------------------------------------------------------------- //

    #[test]
    fn test_agent_runner_can_be_constructed() {
        let (event_tx, _event_rx) = mpsc::channel::<WorkerEvent>(16);
        let runner = AgentRunner {
            config: make_config(),
            workflow: make_workflow(),
            event_tx,
        };
        // Verify fields are accessible.
        assert_eq!(runner.config.agent_max_turns, 5);
        assert_eq!(
            runner.workflow.prompt_template,
            "Work on issue {{ identifier }}: {{ title }}"
        );
    }

    #[test]
    fn test_agent_runner_config_fields() {
        let (event_tx, _event_rx) = mpsc::channel::<WorkerEvent>(16);
        let runner = AgentRunner {
            config: make_config(),
            workflow: make_workflow(),
            event_tx,
        };
        assert_eq!(runner.config.agent_approval_policy, "auto");
        assert_eq!(runner.config.agent_turn_timeout_ms, 600_000);
        assert_eq!(
            runner.config.agent_continuation_guidance,
            "Continue working on the task."
        );
    }

    #[test]
    fn test_agent_runner_workflow_template_renders() {
        let (event_tx, _event_rx) = mpsc::channel::<WorkerEvent>(16);
        let runner = AgentRunner {
            config: make_config(),
            workflow: make_workflow(),
            event_tx,
        };
        let issue = make_issue();
        let rendered =
            prompt::render_prompt(&runner.workflow.prompt_template, &issue, Some(1)).unwrap();
        assert_eq!(rendered, "Work on issue ENG-1: Test issue");
    }

    #[test]
    fn test_title_format() {
        let issue = make_issue();
        let title = format!("{}: {}", issue.identifier, issue.title);
        assert_eq!(title, "ENG-1: Test issue");
    }

    #[tokio::test]
    async fn test_cancellation_oneshot_fires_immediately() {
        // Verifies the oneshot → watch conversion logic works at the type level.
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let (watch_tx, mut watch_rx) = tokio::sync::watch::channel(false);

        // Spawn the bridge task (same pattern as AgentRunner::run).
        tokio::spawn(async move {
            let _ = cancel_rx.await;
            let _ = watch_tx.send(true);
        });

        // Fire cancellation.
        let _ = cancel_tx.send(());

        // The watch should become true quickly.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            watch_rx.changed(),
        )
        .await;
        assert!(result.is_ok(), "watch should update within 200ms");
        assert!(*watch_rx.borrow(), "watch value should be true after cancel");
    }

    #[test]
    fn test_worker_event_claude_update_channel() {
        let (tx, mut rx) = mpsc::channel::<WorkerEvent>(8);

        let ev = WorkerEvent::ClaudeUpdate {
            issue_id: "issue-1".to_string(),
            event: AgentEvent::Notification {
                message: "hello".to_string(),
            },
        };
        tx.try_send(ev).expect("channel should not be full");

        let received = rx.try_recv().expect("should have message");
        match received {
            WorkerEvent::ClaudeUpdate { issue_id, event } => {
                assert_eq!(issue_id, "issue-1");
                match event {
                    AgentEvent::Notification { message } => assert_eq!(message, "hello"),
                    _ => panic!("unexpected event variant"),
                }
            }
            _ => panic!("unexpected worker event variant"),
        }
    }

    #[test]
    fn test_worker_event_exited_normal() {
        let (tx, mut rx) = mpsc::channel::<WorkerEvent>(8);
        tx.try_send(WorkerEvent::WorkerExited {
            issue_id: "issue-2".to_string(),
            reason: WorkerExitReason::Normal,
        })
        .unwrap();

        let ev = rx.try_recv().unwrap();
        match ev {
            WorkerEvent::WorkerExited { issue_id, reason } => {
                assert_eq!(issue_id, "issue-2");
                assert!(matches!(reason, WorkerExitReason::Normal));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_worker_event_exited_abnormal() {
        let (tx, mut rx) = mpsc::channel::<WorkerEvent>(8);
        tx.try_send(WorkerEvent::WorkerExited {
            issue_id: "issue-3".to_string(),
            reason: WorkerExitReason::Abnormal {
                error: "timeout".to_string(),
            },
        })
        .unwrap();

        let ev = rx.try_recv().unwrap();
        match ev {
            WorkerEvent::WorkerExited { issue_id, reason } => {
                assert_eq!(issue_id, "issue-3");
                match reason {
                    WorkerExitReason::Abnormal { error } => {
                        assert_eq!(error, "timeout");
                    }
                    _ => panic!("expected Abnormal"),
                }
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_usage_event_forwarding() {
        let (tx, mut rx) = mpsc::channel::<WorkerEvent>(8);

        let ev = WorkerEvent::ClaudeUpdate {
            issue_id: "issue-4".to_string(),
            event: AgentEvent::UsageUpdate {
                input_tokens: 100,
                output_tokens: 200,
            },
        };
        tx.try_send(ev).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            WorkerEvent::ClaudeUpdate { issue_id, event } => {
                assert_eq!(issue_id, "issue-4");
                match event {
                    AgentEvent::UsageUpdate {
                        input_tokens,
                        output_tokens,
                    } => {
                        assert_eq!(input_tokens, 100);
                        assert_eq!(output_tokens, 200);
                    }
                    _ => panic!("expected UsageUpdate"),
                }
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_session_config_built_from_service_config() {
        // Verify that all fields needed to build a SessionConfig are present in
        // ServiceConfig with the right types.
        let cfg = make_config();
        let _session_cfg = SessionConfig {
            command: cfg.agent_command.clone(),
            workspace_path: cfg.workspace_root.clone(),
            approval_policy: cfg.agent_approval_policy.clone(),
            sandbox_policy: cfg.agent_sandbox_policy.clone(),
            read_timeout_ms: cfg.agent_read_timeout_ms,
            turn_timeout_ms: cfg.agent_turn_timeout_ms,
        };
        // If this compiled, the field mapping is correct.
    }
}
