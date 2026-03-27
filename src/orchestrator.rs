use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;

use crate::agent::AgentRunner;
use crate::config::ServiceConfig;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    truncate, AgentEvent, Issue, LiveSession, OrchestratorState, RetryEntry, RunningEntry,
    WorkerEvent, WorkerExitReason, WorkflowDefinition,
};
use crate::scheduling::{compute_backoff, compute_transitive_block_counts, sort_candidates};
use crate::tracker::Tracker;
use crate::workspace;

// -------------------------------------------------------------------------- //
// Orchestrator
// -------------------------------------------------------------------------- //

/// The central state machine and poll loop for Symphony.
///
/// Owns shared state (behind an `Arc<Mutex>`) which is also accessible to the
/// HTTP status server, drives the poll-dispatch loop, and processes events
/// emitted by worker tasks.
pub struct Orchestrator {
    pub state: Arc<tokio::sync::Mutex<OrchestratorState>>,
    config_rx: tokio::sync::watch::Receiver<Arc<WorkflowDefinition>>,
    tracker: Arc<dyn Tracker + Send + Sync>,
    event_tx: tokio::sync::mpsc::Sender<WorkerEvent>,
    event_rx: tokio::sync::mpsc::Receiver<WorkerEvent>,
    refresh_rx: tokio::sync::mpsc::Receiver<()>,
    plan_mode: bool,
}

impl Orchestrator {
    /// Construct a new `Orchestrator`.
    ///
    /// * `initial_workflow` — the workflow definition at startup.
    /// * `config_rx` — watch channel that delivers updated workflow definitions
    ///   whenever the workflow file changes.
    /// * `tracker` — the issue-tracker backend.
    /// * `refresh_rx` — mpsc channel; the HTTP server sends `()` to this to
    ///   trigger an immediate poll tick.
    pub fn new(
        initial_workflow: WorkflowDefinition,
        config_rx: tokio::sync::watch::Receiver<Arc<WorkflowDefinition>>,
        tracker: Arc<dyn Tracker + Send + Sync>,
        refresh_rx: tokio::sync::mpsc::Receiver<()>,
        plan_mode: bool,
    ) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<WorkerEvent>(256);

        // Parse initial config for state defaults (use defaults if parsing fails).
        let (poll_interval_ms, max_concurrent_agents) =
            match ServiceConfig::from_yaml(&initial_workflow.config) {
                Ok(cfg) => (cfg.poll_interval_ms, cfg.max_concurrent_agents),
                Err(_) => (30_000, 3),
            };

        let state = OrchestratorState {
            poll_interval_ms,
            max_concurrent_agents,
            running: std::collections::HashMap::new(),
            claimed: std::collections::HashSet::new(),
            retry_attempts: std::collections::HashMap::new(),
            completed: std::collections::HashSet::new(),
            claude_totals: crate::domain::ClaudeTotals::default(),
            claude_rate_limits: None,
            next_dispatch_id: 1,
        };

        Self {
            state: Arc::new(tokio::sync::Mutex::new(state)),
            config_rx,
            tracker,
            event_tx,
            event_rx,
            refresh_rx,
            plan_mode,
        }
    }

    // ---------------------------------------------------------------------- //
    // Startup cleanup
    // ---------------------------------------------------------------------- //

    /// Called once at startup to clean up workspaces for issues that are
    /// already in a terminal state.
    pub async fn startup_cleanup(&self, config: &ServiceConfig) {
        match self
            .tracker
            .fetch_issues_by_states(&config.terminal_states, &config.tracker_project_slugs)
            .await
        {
            Ok(issues) => {
                let mut state = self.state.lock().await;
                for issue in issues {
                    // Best-effort workspace cleanup.
                    if let Err(e) = workspace::cleanup_workspace(&issue.identifier, config).await {
                        tracing::warn!(
                            identifier = %issue.identifier,
                            error = %e,
                            "startup cleanup: workspace removal failed (ignored)"
                        );
                    }
                    state.completed.insert(issue.id.clone());
                }
                tracing::info!("Startup cleanup complete");
            }
            Err(e) => {
                tracing::warn!(error = %e, "startup cleanup: fetch terminal issues failed (ignored)");
            }
        }
    }

    // ---------------------------------------------------------------------- //
    // Main poll loop
    // ---------------------------------------------------------------------- //

    /// Run the orchestrator forever (until the process is killed).
    pub async fn run(mut self) {
        loop {
            tracing::debug!("Poll tick starting");

            // 1. Get current config snapshot.
            let workflow = self.config_rx.borrow().clone();
            let config = match ServiceConfig::from_yaml(&workflow.config) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "Config parse error; sleeping 30s before retry");
                    sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };

            // 2. Drain incoming WorkerEvents.
            self.process_events(&config).await;

            // Update state with current config values.
            {
                let mut state = self.state.lock().await;
                state.poll_interval_ms = config.poll_interval_ms;
                state.max_concurrent_agents = config.max_concurrent_agents;
            }

            // 3. Reconcile running issues.
            self.reconcile(&config).await;

            // 4. Validate dispatch config.
            if let Err(e) = config.validate_for_dispatch(self.plan_mode) {
                tracing::warn!(error = %e, "Config not ready for dispatch; skipping this tick");
            } else {
                // 5. Fetch candidates using effective active states.
                let (effective_states, effective_states_original) =
                    config.effective_active_states(self.plan_mode);
                let mut candidates = match self
                    .tracker
                    .fetch_candidate_issues(
                        effective_states_original,
                        &config.tracker_project_slugs,
                    )
                    .await
                {
                    Ok(c) => {
                        tracing::info!(count = c.len(), "Fetched candidate issues");
                        c
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Fetch candidates failed; sleeping until next tick");
                        let interval = config.poll_interval_ms;
                        tokio::select! {
                            _ = sleep(Duration::from_millis(interval)) => {}
                            _ = self.refresh_rx.recv() => {
                                tracing::info!("Manual refresh triggered");
                            }
                        }
                        continue;
                    }
                };

                // 6. Compute transitive block counts and sort candidates.
                let block_counts =
                    compute_transitive_block_counts(&candidates, &config.terminal_states);
                sort_candidates(&mut candidates, &block_counts);

                // 7. Dispatch eligible issues.
                for issue in candidates {
                    if self.is_eligible(&issue, &config, effective_states).await {
                        self.dispatch(issue, &config).await;
                    }
                }
            }

            // 8. Wait for next tick OR refresh signal.
            let interval = config.poll_interval_ms;
            tokio::select! {
                _ = sleep(Duration::from_millis(interval)) => {}
                _ = self.refresh_rx.recv() => {
                    tracing::info!("Manual refresh triggered");
                }
            }
        }
    }

    // ---------------------------------------------------------------------- //
    // Reconciliation
    // ---------------------------------------------------------------------- //

    /// Reconcile the running set against the tracker's current issue states.
    ///
    /// Part A: stall detection — cancel agents that appear stalled.
    /// Part B: fetch current state of running issues from tracker and cancel
    ///         any that have moved to a terminal or unrecognized state.
    async fn reconcile(&mut self, config: &ServiceConfig) {
        let running_ids: Vec<String>;

        // --- Part A: stall detection + collect running IDs (single lock) -----
        {
            let state = self.state.lock().await;
            running_ids = state.running.keys().cloned().collect();

            for (id, entry) in state.running.iter() {
                let elapsed_ms = entry.started_at.elapsed().as_millis() as u64;

                // Use live_session's last event timestamp for a more accurate
                // stall check if available.
                let stall_elapsed_ms = if let Some(ref ls) = entry.live_session {
                    if let Some(ts) = ls.last_event_timestamp {
                        let now = chrono::Utc::now();
                        now.signed_duration_since(ts).num_milliseconds().max(0) as u64
                    } else {
                        elapsed_ms
                    }
                } else {
                    elapsed_ms
                };

                if config.agent_stall_timeout_ms > 0
                    && stall_elapsed_ms > config.agent_stall_timeout_ms
                {
                    tracing::warn!(
                        issue_id = %id,
                        identifier = %entry.issue.identifier,
                        elapsed_ms = stall_elapsed_ms,
                        stall_timeout_ms = config.agent_stall_timeout_ms,
                        "Agent appears stalled; cancelling"
                    );
                    entry.cancel_token.cancel();
                }
            }
        }

        if running_ids.is_empty() {
            return;
        }

        // --- Part B: state refresh from tracker ------------------------------
        match self.tracker.fetch_issue_states_by_ids(&running_ids).await {
            Ok(current_issues) => {
                let mut ids_to_kill: Vec<String> = Vec::new();
                let mut ids_to_update: Vec<(String, String)> = Vec::new();

                // Classify issues — no lock needed, only reads config.
                let (effective_states, _) = config.effective_active_states(self.plan_mode);
                for issue in &current_issues {
                    let issue_state_lc = issue.state.to_lowercase();
                    if config.terminal_states.contains(&issue_state_lc) {
                        ids_to_kill.push(issue.id.clone());
                    } else if effective_states.contains(&issue_state_lc) {
                        ids_to_update.push((issue.id.clone(), issue.state.clone()));
                    } else {
                        tracing::warn!(
                            issue_id = %issue.id,
                            state = %issue.state,
                            "Running issue moved to unrecognized state; cancelling"
                        );
                        ids_to_kill.push(issue.id.clone());
                    }
                }

                // Apply updates and collect entries to kill (single lock).
                let entries_to_cancel: Vec<(String, RunningEntry)>;
                {
                    let mut state = self.state.lock().await;
                    for (id, new_state) in ids_to_update {
                        if let Some(entry) = state.running.get_mut(&id) {
                            entry.issue.state = new_state;
                        }
                    }

                    entries_to_cancel = ids_to_kill
                        .into_iter()
                        .filter_map(|id| {
                            let entry = state.running.remove(&id)?;
                            state.completed.insert(id.clone());
                            state.claimed.remove(&id);
                            Some((id, entry))
                        })
                        .collect();
                }

                // Cancel outside the lock.
                for (id, entry) in entries_to_cancel {
                    tracing::info!(
                        issue_id = %id,
                        identifier = %entry.issue.identifier,
                        state = %entry.issue.state,
                        "Reconcile: cancelling running agent (issue reached terminal/unknown state)"
                    );
                    entry.cancel_token.cancel();
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "State refresh failed during reconcile");
            }
        }
    }

    // ---------------------------------------------------------------------- //
    // Eligibility check
    // ---------------------------------------------------------------------- //

    /// Return `true` if `issue` should be dispatched this tick.
    async fn is_eligible(
        &self,
        issue: &Issue,
        config: &ServiceConfig,
        effective_states: &[String],
    ) -> bool {
        // Basic field validation.
        if issue.id.is_empty()
            || issue.identifier.is_empty()
            || issue.title.is_empty()
            || issue.state.is_empty()
        {
            return false;
        }

        let state_lc = issue.state.to_lowercase();

        // Must be in an effective active state (respects plan_mode).
        if !effective_states.contains(&state_lc) {
            return false;
        }

        // Must NOT be in a terminal state.
        if config.terminal_states.contains(&state_lc) {
            return false;
        }

        // In plan mode, only dispatch issues with the "needs plan" label,
        // and skip issues that already have planning labels.
        // In normal mode, skip issues that are pending or in-progress planning.
        if self.plan_mode {
            if !issue.labels.iter().any(|l| l == "needs plan") {
                return false;
            }
            if issue
                .labels
                .iter()
                .any(|l| l == "planning..." || l == "plan ready")
            {
                return false;
            }
        } else if issue
            .labels
            .iter()
            .any(|l| l == "needs plan" || l == "planning...")
        {
            return false;
        }

        let state = self.state.lock().await;

        // Must not already be running.
        if state.running.contains_key(&issue.id) {
            return false;
        }

        // Must not be claimed (pending retry / recently dispatched).
        if state.claimed.contains(&issue.id) {
            return false;
        }

        // Global concurrency limit.
        if state.running.len() >= config.max_concurrent_agents {
            return false;
        }

        // Per-state concurrency limit.
        if let Some(&max_for_state) = config.max_concurrent_agents_by_state.get(&state_lc) {
            let running_in_state = state
                .running
                .values()
                .filter(|e| e.issue.state.to_lowercase() == state_lc)
                .count();
            if running_in_state >= max_for_state {
                return false;
            }
        }

        // Todo blocker rule: all blockers must be in terminal states.
        if state_lc == "todo" {
            for blocker in &issue.blocked_by {
                if let Some(ref blocker_state) = blocker.state {
                    let blocker_state_lc = blocker_state.to_lowercase();
                    if !config.terminal_states.contains(&blocker_state_lc) {
                        return false;
                    }
                }
            }
        }

        true
    }

    // ---------------------------------------------------------------------- //
    // Dispatch
    // ---------------------------------------------------------------------- //

    /// Dispatch a worker for `issue`.
    async fn dispatch(&mut self, issue: Issue, config: &ServiceConfig) {
        let issue_id = issue.id.clone();
        let issue_identifier = issue.identifier.clone();

        // Create cancellation token.
        let cancel_token = CancellationToken::new();

        // Get attempt and assign dispatch_id under a single lock.
        let (attempt, dispatch_id) = {
            let mut state = self.state.lock().await;
            let attempt = state
                .retry_attempts
                .get(&issue_id)
                .map(|r| r.attempt)
                .unwrap_or(1);
            state.next_dispatch_id += 1;
            let dispatch_id = state.next_dispatch_id;

            // Create RunningEntry.
            let entry = RunningEntry {
                issue: issue.clone(),
                attempt,
                dispatch_id,
                live_session: None,
                started_at: std::time::Instant::now(),
                cancel_token: cancel_token.clone(),
            };

            state.running.insert(issue_id.clone(), entry);
            state.claimed.insert(issue_id.clone());

            // Abort any pending retry timer for this issue.
            if let Some(retry) = state.retry_attempts.remove(&issue_id) {
                retry.abort_handle.abort();
            }

            (attempt, dispatch_id)
        };

        tracing::info!(
            issue_id = %issue_id,
            identifier = %issue_identifier,
            attempt = attempt,
            dispatch_id = dispatch_id,
            "Dispatching worker"
        );

        if self.plan_mode {
            // --- Plan mode dispatch ---
            // No started_state transition in plan mode.
            let tracker = Arc::clone(&self.tracker);
            let runner = AgentRunner {
                config: Arc::new(config.clone()),
                workflow: Arc::clone(&self.config_rx.borrow()),
                event_tx: self.event_tx.clone(),
                plan: None,
            };
            let issue_id_clone = issue_id.clone();
            let issue_identifier_clone = issue_identifier.clone();
            let event_tx = self.event_tx.clone();

            tokio::spawn(async move {
                // 1. Remove "Needs Plan" label and add "Planning..." label.
                if let Err(e) = tracker.remove_label(&issue_id_clone, "Needs Plan").await {
                    tracing::warn!(
                        issue_id = %issue_id_clone,
                        identifier = %issue_identifier_clone,
                        error = %e,
                        "Failed to remove Needs Plan label"
                    );
                }
                if let Err(e) = tracker.add_label(&issue_id_clone, "Planning...").await {
                    tracing::warn!(
                        issue_id = %issue_id_clone,
                        identifier = %issue_identifier_clone,
                        error = %e,
                        "Failed to add Planning... label"
                    );
                }

                // 2. Run agent in plan mode.
                let result = runner.run_plan(issue, attempt, cancel_token).await;

                let reason = match result {
                    Ok(plan_text) => {
                        // 3. Post plan as comment (with sentinel marker).
                        let comment_body =
                            format!("<!-- symphony-plan -->\n{plan_text}");
                        if let Err(e) = tracker.post_comment(&issue_id_clone, &comment_body).await {
                            tracing::error!(
                                issue_id = %issue_id_clone,
                                error = %e,
                                "Failed to post plan comment"
                            );
                        }

                        // 4. Swap labels: remove "Planning...", add "Plan Ready".
                        if let Err(e) = tracker.remove_label(&issue_id_clone, "Planning...").await {
                            tracing::warn!(issue_id = %issue_id_clone, error = %e, "Failed to remove Planning... label");
                        }
                        if let Err(e) = tracker.add_label(&issue_id_clone, "Plan Ready").await {
                            tracing::warn!(issue_id = %issue_id_clone, error = %e, "Failed to add Plan Ready label");
                        }

                        WorkerExitReason::Normal
                    }
                    Err(e) => {
                        // Remove "Planning..." label on failure so issue can be retried.
                        let _ = tracker.remove_label(&issue_id_clone, "Planning...").await;
                        // Re-add "Needs Plan" so the issue is eligible for retry.
                        let _ = tracker.add_label(&issue_id_clone, "Needs Plan").await;
                        WorkerExitReason::Abnormal {
                            error: e.to_string(),
                        }
                    }
                };

                let _ = event_tx
                    .send(WorkerEvent::WorkerExited {
                        issue_id: issue_id_clone,
                        dispatch_id,
                        reason,
                    })
                    .await;
            });
        } else {
            // --- Normal mode dispatch ---
            // Transition to started_state if configured and the issue isn't already in that state.
            if let Some(ref started_state) = config.started_state {
                if issue.state.to_lowercase() != started_state.to_lowercase() {
                    tracing::info!(
                        issue_id = %issue_id,
                        identifier = %issue_identifier,
                        state = %started_state,
                        "Transitioning issue to started state"
                    );
                    match self.tracker.set_issue_state(&issue_id, started_state).await {
                        Ok(()) => {
                            tracing::info!(
                                issue_id = %issue_id,
                                identifier = %issue_identifier,
                                state = %started_state,
                                "Issue transitioned to started state"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                issue_id = %issue_id,
                                identifier = %issue_identifier,
                                state = %started_state,
                                error = %e,
                                "Failed to transition issue to started state; continuing dispatch"
                            );
                        }
                    }
                }
            }

            // If issue has "plan ready" label, fetch the plan and inject it.
            let plan = if issue.labels.iter().any(|l| l == "plan ready") {
                match self.tracker.fetch_comments(&issue_id).await {
                    Ok(comments) => {
                        // Find the most recent comment with the symphony-plan sentinel.
                        comments
                            .iter()
                            .rev()
                            .find(|c| c.body.contains("<!-- symphony-plan -->"))
                            .map(|c| {
                                c.body
                                    .replace("<!-- symphony-plan -->\n", "")
                                    .replace("<!-- symphony-plan -->", "")
                            })
                    }
                    Err(e) => {
                        tracing::warn!(
                            issue_id = %issue_id,
                            error = %e,
                            "Failed to fetch plan comments; proceeding without plan"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Spawn worker task.
            let runner = AgentRunner {
                config: Arc::new(config.clone()),
                workflow: Arc::clone(&self.config_rx.borrow()),
                event_tx: self.event_tx.clone(),
                plan,
            };
            let issue_id_clone = issue_id.clone();
            let event_tx = self.event_tx.clone();

            tokio::spawn(async move {
                let result = runner.run(issue, attempt, cancel_token).await;
                let reason = match result {
                    Ok(()) => WorkerExitReason::Normal,
                    Err(e) => WorkerExitReason::Abnormal {
                        error: e.to_string(),
                    },
                };
                let _ = event_tx
                    .send(WorkerEvent::WorkerExited {
                        issue_id: issue_id_clone,
                        dispatch_id,
                        reason,
                    })
                    .await;
            });
        }
    }

    // ---------------------------------------------------------------------- //
    // Event processing
    // ---------------------------------------------------------------------- //

    /// Drain all pending events from `event_rx` (non-blocking).
    async fn process_events(&mut self, config: &ServiceConfig) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                WorkerEvent::ClaudeUpdate { issue_id, event } => {
                    self.handle_claude_update(&issue_id, event).await;
                }
                WorkerEvent::WorkerExited {
                    issue_id,
                    dispatch_id,
                    reason,
                } => {
                    self.handle_worker_exit(
                        &issue_id,
                        dispatch_id,
                        reason,
                        config.agent_max_retry_backoff_ms,
                        config.review_state.clone(),
                    )
                    .await;
                }
            }
        }
    }

    // ---------------------------------------------------------------------- //
    // Claude update handler
    // ---------------------------------------------------------------------- //

    /// Handle a `ClaudeUpdate` event: update token totals and live session data.
    async fn handle_claude_update(&mut self, issue_id: &str, event: AgentEvent) {
        let mut state = self.state.lock().await;

        let identifier = state
            .running
            .get(issue_id)
            .map(|e| e.issue.identifier.clone())
            .unwrap_or_else(|| issue_id.to_string());

        // Determine the event name and message for live session update, plus
        // any arm-specific side effects.
        let (event_name, event_message) = match event {
            AgentEvent::UsageUpdate {
                input_tokens,
                output_tokens,
            } => {
                if let Some(entry) = state.running.get_mut(issue_id) {
                    let ls = entry.live_session.get_or_insert_with(LiveSession::default);

                    let delta_in = input_tokens.saturating_sub(ls.last_reported_input_tokens);
                    let delta_out = output_tokens.saturating_sub(ls.last_reported_output_tokens);

                    ls.input_tokens = input_tokens;
                    ls.output_tokens = output_tokens;
                    ls.last_reported_input_tokens = input_tokens;
                    ls.last_reported_output_tokens = output_tokens;

                    state.claude_totals.input_tokens += delta_in;
                    state.claude_totals.output_tokens += delta_out;
                    state.claude_totals.total_tokens += delta_in + delta_out;
                }
                ("usage_update".to_string(), None)
            }
            AgentEvent::Notification { message } => {
                // Ensure live session exists for notification events.
                if let Some(entry) = state.running.get_mut(issue_id) {
                    entry.live_session.get_or_insert_with(LiveSession::default);
                }
                tracing::trace!(issue_id = %issue_id, message = %message, "Agent notification");
                ("notification".to_string(), Some(message))
            }
            AgentEvent::AssistantText { text } => {
                tracing::trace!("[{identifier}] {text}");
                ("assistant_text".to_string(), Some(text))
            }
            AgentEvent::ToolEvent {
                tool_name,
                phase,
                summary,
            } => {
                tracing::trace!("[{identifier}] Tool {phase}: {tool_name} | {summary}");
                (
                    format!("tool_{phase}"),
                    Some(format!("{tool_name}: {summary}")),
                )
            }
            AgentEvent::TurnResult {
                success,
                duration_ms,
                total_cost_usd,
                ..
            } => {
                tracing::trace!(
                    "[{identifier}] Turn {}: {}ms, ${:.4}",
                    if success { "completed" } else { "failed" },
                    duration_ms.unwrap_or(0),
                    total_cost_usd.unwrap_or(0.0)
                );
                let msg = format!(
                    "{}; {}ms; ${:.4}",
                    if success { "success" } else { "failed" },
                    duration_ms.unwrap_or(0),
                    total_cost_usd.unwrap_or(0.0)
                );
                ("turn_result".to_string(), Some(msg))
            }
            AgentEvent::OtherMessage { raw } => {
                let msg = truncate(&raw, 120);
                tracing::trace!("[{identifier}] {msg}");
                ("other".to_string(), None)
            }
        };

        // Update live session fields.
        if let Some(entry) = state.running.get_mut(issue_id) {
            if let Some(ref mut ls) = entry.live_session {
                ls.last_event = Some(event_name);
                ls.last_event_timestamp = Some(chrono::Utc::now());
                if event_message.is_some() {
                    ls.last_event_message = event_message;
                }
            }
        }
    }

    // ---------------------------------------------------------------------- //
    // Worker exit handler
    // ---------------------------------------------------------------------- //

    /// Handle a `WorkerExited` event: remove from running, accumulate stats,
    /// schedule retry.
    async fn handle_worker_exit(
        &mut self,
        issue_id: &str,
        dispatch_id: u64,
        reason: WorkerExitReason,
        max_backoff_ms: u64,
        review_state: Option<String>,
    ) {
        let (attempt, identifier, elapsed_secs) = {
            let mut state = self.state.lock().await;

            // Ignore exits from stale workers (e.g. old worker cancelled by
            // reconcile when the issue was immediately re-dispatched).
            if state.running.get(issue_id).map(|e| e.dispatch_id) != Some(dispatch_id) {
                tracing::warn!(
                    issue_id = %issue_id,
                    dispatch_id = dispatch_id,
                    "Ignoring stale worker exit"
                );
                return;
            }

            let entry = state.running.remove(issue_id);

            let elapsed = entry
                .as_ref()
                .map(|e| e.started_at.elapsed().as_secs_f64())
                .unwrap_or(0.0);
            state.claude_totals.seconds_running += elapsed;

            let attempt = entry.as_ref().map(|e| e.attempt).unwrap_or(1);
            let identifier = entry
                .as_ref()
                .map(|e| e.issue.identifier.clone())
                .unwrap_or_default();

            (attempt, identifier, elapsed)
        };

        tracing::info!(
            issue_id = %issue_id,
            identifier = %identifier,
            attempt = attempt,
            elapsed_secs = %elapsed_secs,
            "Worker exited"
        );

        match reason {
            WorkerExitReason::Normal if self.plan_mode => {
                // In plan mode, normal exit means the plan was posted successfully.
                // Mark completed and remove from claimed — no review_state transition.
                tracing::info!(
                    issue_id = %issue_id,
                    identifier = %identifier,
                    "Plan worker completed; marking issue as completed"
                );
                let mut state = self.state.lock().await;
                state.completed.insert(issue_id.to_string());
                state.claimed.remove(issue_id);
            }
            WorkerExitReason::Normal => {
                if let Some(ref state_name) = review_state {
                    tracing::info!(
                        issue_id = %issue_id,
                        identifier = %identifier,
                        state = %state_name,
                        "Worker completed; transitioning issue to review state"
                    );
                    match self.tracker.set_issue_state(issue_id, state_name).await {
                        Ok(()) => {
                            tracing::info!(
                                issue_id = %issue_id,
                                identifier = %identifier,
                                state = %state_name,
                                "Issue transitioned to review state"
                            );
                            let mut state = self.state.lock().await;
                            state.completed.insert(issue_id.to_string());
                            state.claimed.remove(issue_id);
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(
                                issue_id = %issue_id,
                                identifier = %identifier,
                                state = %state_name,
                                error = %e,
                                "Failed to transition issue to review state; falling back to normal retry"
                            );
                        }
                    }
                }

                tracing::info!(
                    issue_id = %issue_id,
                    "Worker exited normally; scheduling continuation retry"
                );
                // Schedule a short delay before rechecking (allows tracker state to update).
                self.schedule_retry(issue_id, &identifier, 1, "normal exit", 1_000)
                    .await;
            }
            WorkerExitReason::Abnormal { error } => {
                let backoff = compute_backoff(attempt, max_backoff_ms);
                tracing::warn!(
                    issue_id = %issue_id,
                    identifier = %identifier,
                    attempt = attempt,
                    error = %error,
                    backoff_ms = backoff,
                    "Worker exited abnormally; scheduling retry"
                );
                self.schedule_retry(issue_id, &identifier, attempt + 1, &error, backoff)
                    .await;
            }
        }
    }

    // ---------------------------------------------------------------------- //
    // Retry scheduling
    // ---------------------------------------------------------------------- //

    /// Schedule a retry for `issue_id` after `delay_ms` milliseconds.
    ///
    /// The retry timer task sleeps, then removes the issue from `claimed` so
    /// that the next poll tick picks it up again.
    async fn schedule_retry(
        &mut self,
        issue_id: &str,
        identifier: &str,
        next_attempt: u32,
        reason: &str,
        delay_ms: u64,
    ) {
        let state_clone = Arc::clone(&self.state);
        let issue_id_owned = issue_id.to_string();
        let delay = Duration::from_millis(delay_ms);

        let handle = tokio::spawn(async move {
            sleep(delay).await;
            let mut state = state_clone.lock().await;
            // Only remove `claimed` here; `retry_attempts` is removed by
            // `dispatch()` when it actually picks up the issue, preserving
            // the `attempt` counter across the retry timer.
            state.claimed.remove(&issue_id_owned);
        });

        let due_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
            + delay_ms;

        {
            let mut state = self.state.lock().await;
            state.retry_attempts.insert(
                issue_id.to_string(),
                RetryEntry {
                    issue_id: issue_id.to_string(),
                    identifier: identifier.to_string(),
                    attempt: next_attempt,
                    due_at_ms,
                    error: reason.to_string(),
                    abort_handle: handle,
                },
            );
        }

        tracing::info!(
            issue_id = %issue_id,
            identifier = %identifier,
            next_attempt = next_attempt,
            delay_ms = delay_ms,
            reason = %reason,
            "Retry scheduled"
        );
    }
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockerRef, Comment};
    use std::sync::Mutex as StdMutex;
    use tokio_util::sync::CancellationToken;

    // ---------------------------------------------------------------------- //
    // Shared test helpers
    // ---------------------------------------------------------------------- //

    fn make_issue(id: &str, identifier: &str, state: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            title: "Test issue".to_string(),
            description: None,
            priority: None,
            state: state.to_string(),
            branch_name: None,
            url: None,
            labels: vec![],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        }
    }

    fn make_config() -> ServiceConfig {
        ServiceConfig {
            tracker_kind: "linear".to_string(),
            tracker_api_key: "test-key".to_string(),
            tracker_project_slugs: vec!["test-project".to_string()],
            tracker_endpoint: None,
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_after_create: None,
            workspace_before_remove: None,
            agent_command: "claude".to_string(),
            agent_approval_policy: "auto".to_string(),
            agent_sandbox_policy: None,
            agent_max_turns: 10,
            agent_read_timeout_ms: 30_000,
            agent_turn_timeout_ms: 600_000,
            agent_stall_timeout_ms: 120_000,
            agent_max_retry_backoff_ms: 300_000,
            agent_continuation_guidance: "Continue.".to_string(),
            poll_interval_ms: 30_000,
            max_concurrent_agents: 3,
            max_concurrent_agents_by_state: std::collections::HashMap::new(),
            active_states: vec!["in progress".to_string(), "todo".to_string()],
            terminal_states: vec!["done".to_string(), "cancelled".to_string()],
            active_states_original: vec!["In Progress".to_string(), "Todo".to_string()],
            terminal_states_original: vec!["Done".to_string(), "Cancelled".to_string()],
            planning_states: vec![],
            planning_states_original: vec![],
            started_state: None,
            review_state: None,
            server_enabled: false,
            server_port: 8080,
        }
    }

    /// No-op tracker for tests that don't need to spy on calls.
    struct NullTracker;

    impl crate::tracker::Tracker for NullTracker {
        fn fetch_candidate_issues<'a>(&'a self, _: &'a [String], _: &'a [String])
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Issue>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }

        fn fetch_issues_by_states<'a>(&'a self, _: &'a [String], _: &'a [String])
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Issue>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }

        fn fetch_issue_states_by_ids<'a>(&'a self, _: &'a [String])
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Issue>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }

        fn set_issue_state<'a>(&'a self, _: &'a str, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        { Box::pin(async { Ok(()) }) }

        fn add_label<'a>(&'a self, _: &'a str, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        { Box::pin(async { Ok(()) }) }

        fn remove_label<'a>(&'a self, _: &'a str, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        { Box::pin(async { Ok(()) }) }

        fn post_comment<'a>(&'a self, _: &'a str, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        { Box::pin(async { Ok(()) }) }

        fn fetch_comments<'a>(&'a self, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Comment>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }
    }

    /// Tracker that records `set_issue_state` calls for assertions.
    struct SpyTracker {
        calls: Arc<StdMutex<Vec<(String, String)>>>,
    }

    impl crate::tracker::Tracker for SpyTracker {
        fn fetch_candidate_issues<'a>(&'a self, _: &'a [String], _: &'a [String])
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Issue>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }

        fn fetch_issues_by_states<'a>(&'a self, _: &'a [String], _: &'a [String])
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Issue>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }

        fn fetch_issue_states_by_ids<'a>(&'a self, _: &'a [String])
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Issue>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }

        fn set_issue_state<'a>(&'a self, issue_id: &'a str, state_name: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        {
            let calls = Arc::clone(&self.calls);
            let issue_id = issue_id.to_string();
            let state_name = state_name.to_string();
            Box::pin(async move {
                calls.lock().unwrap().push((issue_id, state_name));
                Ok(())
            })
        }

        fn add_label<'a>(&'a self, _: &'a str, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        { Box::pin(async { Ok(()) }) }

        fn remove_label<'a>(&'a self, _: &'a str, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        { Box::pin(async { Ok(()) }) }

        fn post_comment<'a>(&'a self, _: &'a str, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<()>> + Send + 'a>>
        { Box::pin(async { Ok(()) }) }

        fn fetch_comments<'a>(&'a self, _: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<Vec<Comment>>> + Send + 'a>>
        { Box::pin(async { Ok(vec![]) }) }
    }

    /// Build a test orchestrator with a NullTracker.
    fn make_test_orchestrator(plan_mode: bool) -> Orchestrator {
        make_test_orchestrator_with_tracker(Arc::new(NullTracker), plan_mode)
    }

    /// Build a test orchestrator with a custom tracker.
    fn make_test_orchestrator_with_tracker(
        tracker: Arc<dyn crate::tracker::Tracker + Send + Sync>,
        plan_mode: bool,
    ) -> Orchestrator {
        let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::new(WorkflowDefinition {
            config: serde_yaml::Value::Null,
            prompt_template: String::new(),
        }));
        let (_refresh_tx, refresh_rx) = tokio::sync::mpsc::channel::<()>(1);
        std::mem::forget(config_tx);

        Orchestrator::new(
            WorkflowDefinition {
                config: serde_yaml::Value::Null,
                prompt_template: String::new(),
            },
            config_rx,
            tracker,
            refresh_rx,
            plan_mode,
        )
    }

    /// Build a spy orchestrator, returning (orchestrator, spy_calls).
    fn make_spy_orchestrator(plan_mode: bool) -> (Orchestrator, Arc<StdMutex<Vec<(String, String)>>>) {
        let spy_calls: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(vec![]));
        let tracker = Arc::new(SpyTracker {
            calls: Arc::clone(&spy_calls),
        });
        let orch = make_test_orchestrator_with_tracker(tracker, plan_mode);
        (orch, spy_calls)
    }

    /// Helper: compute effective_active_states for use in is_eligible tests.
    fn active_states(config: &ServiceConfig, plan_mode: bool) -> Vec<String> {
        let (states, _) = config.effective_active_states(plan_mode);
        states.to_vec()
    }

    // ---------------------------------------------------------------------- //
    // is_eligible
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn eligible_basic_happy_path() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);
        let issue = make_issue("id-1", "ENG-1", "In Progress");
        assert!(orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_empty_id() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);
        let issue = make_issue("", "ENG-1", "In Progress");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_empty_identifier() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);
        let issue = make_issue("id-1", "", "In Progress");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_empty_state() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);
        let issue = make_issue("id-1", "ENG-1", "");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_state_not_in_active_states() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);
        let issue = make_issue("id-1", "ENG-1", "Backlog");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_state_in_terminal_states() {
        let orch = make_test_orchestrator(false);
        let mut config = make_config();
        config.active_states.push("done".to_string());
        let states = active_states(&config, false);
        let issue = make_issue("id-1", "ENG-1", "Done");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_when_running() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);

        {
            let mut state = orch.state.lock().await;
            state.running.insert(
                "id-1".to_string(),
                RunningEntry {
                    issue: make_issue("id-1", "ENG-1", "In Progress"),
                    attempt: 1,
                    dispatch_id: 1,
                    live_session: None,
                    started_at: std::time::Instant::now(),
                    cancel_token: CancellationToken::new(),
                },
            );
        }

        let issue = make_issue("id-1", "ENG-1", "In Progress");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_when_claimed() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);

        {
            let mut state = orch.state.lock().await;
            state.claimed.insert("id-1".to_string());
        }

        let issue = make_issue("id-1", "ENG-1", "In Progress");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_global_concurrency_limit() {
        let orch = make_test_orchestrator(false);
        let mut config = make_config();
        config.max_concurrent_agents = 1;
        let states = active_states(&config, false);

        {
            let mut state = orch.state.lock().await;
            state.running.insert(
                "other-id".to_string(),
                RunningEntry {
                    issue: make_issue("other-id", "ENG-0", "In Progress"),
                    attempt: 1,
                    dispatch_id: 1,
                    live_session: None,
                    started_at: std::time::Instant::now(),
                    cancel_token: CancellationToken::new(),
                },
            );
        }

        let issue = make_issue("id-1", "ENG-1", "In Progress");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn ineligible_per_state_concurrency_limit() {
        let orch = make_test_orchestrator(false);
        let mut config = make_config();
        config
            .max_concurrent_agents_by_state
            .insert("in progress".to_string(), 1);
        let states = active_states(&config, false);

        {
            let mut state = orch.state.lock().await;
            state.running.insert(
                "other-id".to_string(),
                RunningEntry {
                    issue: make_issue("other-id", "ENG-0", "In Progress"),
                    attempt: 1,
                    dispatch_id: 1,
                    live_session: None,
                    started_at: std::time::Instant::now(),
                    cancel_token: CancellationToken::new(),
                },
            );
        }

        let issue = make_issue("id-1", "ENG-1", "In Progress");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn todo_blocker_rule_all_done() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);

        let mut issue = make_issue("id-1", "ENG-1", "Todo");
        issue.blocked_by = vec![
            BlockerRef {
                id: Some("b-1".to_string()),
                identifier: Some("ENG-0".to_string()),
                state: Some("Done".to_string()),
            },
            BlockerRef {
                id: Some("b-2".to_string()),
                identifier: Some("ENG-00".to_string()),
                state: Some("Cancelled".to_string()),
            },
        ];

        assert!(orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn todo_blocker_rule_blocker_not_done() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);

        let mut issue = make_issue("id-1", "ENG-1", "Todo");
        issue.blocked_by = vec![BlockerRef {
            id: Some("b-1".to_string()),
            identifier: Some("ENG-0".to_string()),
            state: Some("In Progress".to_string()),
        }];

        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn todo_blocker_rule_blocker_state_none_is_allowed() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);

        let mut issue = make_issue("id-1", "ENG-1", "Todo");
        issue.blocked_by = vec![BlockerRef {
            id: Some("b-1".to_string()),
            identifier: Some("ENG-0".to_string()),
            state: None,
        }];

        assert!(orch.is_eligible(&issue, &config, &states).await);
    }

    // ---------------------------------------------------------------------- //
    // review_state
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn review_state_calls_set_issue_state_on_normal_exit() {
        let (mut orch, spy_calls) = make_spy_orchestrator(false);

        {
            let mut state = orch.state.lock().await;
            state.running.insert(
                "issue-abc".to_string(),
                RunningEntry {
                    issue: make_issue("issue-abc", "ENG-42", "In Progress"),
                    attempt: 1,
                    dispatch_id: 7,
                    live_session: None,
                    started_at: std::time::Instant::now(),
                    cancel_token: CancellationToken::new(),
                },
            );
            state.claimed.insert("issue-abc".to_string());
        }

        orch.handle_worker_exit(
            "issue-abc",
            7,
            WorkerExitReason::Normal,
            300_000,
            Some("In Review".to_string()),
        )
        .await;

        let calls = spy_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "expected exactly one set_issue_state call");
        assert_eq!(calls[0].0, "issue-abc");
        assert_eq!(calls[0].1, "In Review");
        drop(calls);

        let state = orch.state.lock().await;
        assert!(state.completed.contains("issue-abc"));
        assert!(!state.claimed.contains("issue-abc"));
        assert!(!state.retry_attempts.contains_key("issue-abc"));
    }

    // ---------------------------------------------------------------------- //
    // started_state
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn started_state_calls_set_issue_state_on_dispatch() {
        let (mut orch, spy_calls) = make_spy_orchestrator(false);

        let mut config = make_config();
        config.started_state = Some("In Progress".to_string());

        let issue = make_issue("issue-xyz", "ENG-99", "Todo");
        orch.dispatch(issue, &config).await;

        let calls = spy_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "expected exactly one set_issue_state call");
        assert_eq!(calls[0].0, "issue-xyz");
        assert_eq!(calls[0].1, "In Progress");
    }

    #[tokio::test]
    async fn started_state_skipped_when_issue_already_in_that_state() {
        let (mut orch, spy_calls) = make_spy_orchestrator(false);

        let mut config = make_config();
        config.started_state = Some("In Progress".to_string());

        let issue = make_issue("issue-xyz", "ENG-99", "In Progress");
        orch.dispatch(issue, &config).await;

        let calls = spy_calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            0,
            "set_issue_state should not be called when issue is already in started_state"
        );
    }

    // ---------------------------------------------------------------------- //
    // Plan mode — is_eligible label skipping
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn plan_mode_skips_planning_label() {
        let orch = make_test_orchestrator(true);
        let config = make_config();
        let states = active_states(&config, true);
        let mut issue = make_issue("id-1", "ENG-1", "In Progress");
        issue.labels = vec!["needs plan".to_string(), "planning...".to_string()];
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn plan_mode_skips_plan_ready_label() {
        let orch = make_test_orchestrator(true);
        let config = make_config();
        let states = active_states(&config, true);
        let mut issue = make_issue("id-1", "ENG-1", "In Progress");
        issue.labels = vec!["needs plan".to_string(), "plan ready".to_string()];
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn plan_mode_requires_needs_plan_label() {
        let orch = make_test_orchestrator(true);
        let config = make_config();
        let states = active_states(&config, true);
        let issue = make_issue("id-1", "ENG-1", "In Progress");
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn plan_mode_allows_issue_with_needs_plan_label() {
        let orch = make_test_orchestrator(true);
        let config = make_config();
        let states = active_states(&config, true);
        let mut issue = make_issue("id-1", "ENG-1", "In Progress");
        issue.labels = vec!["needs plan".to_string()];
        assert!(orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn normal_mode_skips_planning_label() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);
        let mut issue = make_issue("id-1", "ENG-1", "In Progress");
        issue.labels = vec!["planning...".to_string()];
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }

    #[tokio::test]
    async fn normal_mode_skips_needs_plan_label() {
        let orch = make_test_orchestrator(false);
        let config = make_config();
        let states = active_states(&config, false);
        let mut issue = make_issue("id-1", "ENG-1", "In Progress");
        issue.labels = vec!["needs plan".to_string()];
        assert!(!orch.is_eligible(&issue, &config, &states).await);
    }
}
