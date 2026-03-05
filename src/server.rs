use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::domain::OrchestratorState;

// -------------------------------------------------------------------------- //
// Shared application state
// -------------------------------------------------------------------------- //

pub struct AppState {
    pub orchestrator_state: Arc<Mutex<OrchestratorState>>,
    pub refresh_tx: mpsc::Sender<()>,
}

// -------------------------------------------------------------------------- //
// Handlers
// -------------------------------------------------------------------------- //

/// GET / — simple HTML dashboard with a link to the state endpoint.
async fn dashboard() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html>
<head><title>Symphony</title></head>
<body>
<h1>Symphony</h1>
<p><a href="/api/v1/state">View state JSON</a></p>
</body>
</html>"#,
    )
}

/// GET /api/v1/state — serialized snapshot of orchestrator state.
async fn get_state(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    let state = app.orchestrator_state.lock().await;
    let snapshot = state.snapshot();
    Json(json!({
        "generated_at": Utc::now().to_rfc3339(),
        "poll_interval_ms": snapshot.poll_interval_ms,
        "max_concurrent_agents": snapshot.max_concurrent_agents,
        "running": snapshot.running,
        "retry_attempts": snapshot.retry_attempts,
        "completed_count": snapshot.completed.len(),
        "claude_totals": snapshot.claude_totals,
        "claude_rate_limits": snapshot.claude_rate_limits,
    }))
}

/// GET /api/v1/:identifier — per-issue debug details; 404 if not found.
async fn get_issue(
    Path(identifier): Path<String>,
    State(app): State<Arc<AppState>>,
) -> impl IntoResponse {
    let state = app.orchestrator_state.lock().await;
    let snapshot = state.snapshot();

    // Search running entries by issue_identifier or map key.
    for (id, entry) in &snapshot.running {
        if entry.issue.identifier == identifier || id == &identifier {
            return (StatusCode::OK, Json(json!(entry))).into_response();
        }
    }

    // Search retry entries by identifier or map key.
    for (id, retry) in &snapshot.retry_attempts {
        if retry.identifier == identifier || id == &identifier {
            return (StatusCode::OK, Json(json!(retry))).into_response();
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "not found", "identifier": identifier})),
    )
        .into_response()
}

/// POST /api/v1/refresh — trigger an immediate orchestrator poll tick.
async fn trigger_refresh(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    let _ = app.refresh_tx.try_send(());
    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "refresh triggered"})),
    )
}

// -------------------------------------------------------------------------- //
// Server entrypoint
// -------------------------------------------------------------------------- //

/// Start the HTTP server on `127.0.0.1:{port}`.
///
/// `orchestrator_state` is the shared orchestrator state protected by a
/// `Mutex`.  `refresh_tx` is a channel whose receiver causes the orchestrator
/// to perform an immediate poll tick.
pub async fn serve(
    port: u16,
    orchestrator_state: Arc<Mutex<OrchestratorState>>,
    refresh_tx: mpsc::Sender<()>,
) -> anyhow::Result<()> {
    let app_state = Arc::new(AppState {
        orchestrator_state,
        refresh_tx,
    });

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/v1/state", get(get_state))
        .route("/api/v1/:identifier", get(get_issue))
        .route("/api/v1/refresh", post(trigger_refresh))
        .with_state(app_state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// -------------------------------------------------------------------------- //
// Tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ClaudeTotals, OrchestratorState};
    use std::collections::{HashMap, HashSet};

    fn make_orchestrator_state() -> Arc<Mutex<OrchestratorState>> {
        Arc::new(Mutex::new(OrchestratorState {
            poll_interval_ms: 5000,
            max_concurrent_agents: 4,
            running: HashMap::new(),
            claimed: HashSet::new(),
            retry_attempts: HashMap::new(),
            completed: HashSet::new(),
            claude_totals: ClaudeTotals::default(),
            claude_rate_limits: None,
            next_dispatch_id: 1,
        }))
    }

    /// AppState can be constructed without panicking.
    #[tokio::test]
    async fn test_app_state_construction() {
        let orchestrator_state = make_orchestrator_state();
        let (refresh_tx, _refresh_rx) = mpsc::channel::<()>(1);

        let app_state = Arc::new(AppState {
            orchestrator_state,
            refresh_tx,
        });

        // Verify we can lock and snapshot without error.
        let state = app_state.orchestrator_state.lock().await;
        let snapshot = state.snapshot();
        assert_eq!(snapshot.poll_interval_ms, 5000);
        assert_eq!(snapshot.max_concurrent_agents, 4);
        assert!(snapshot.running.is_empty());
        assert!(snapshot.retry_attempts.is_empty());
        assert!(snapshot.completed.is_empty());
    }

    /// try_send on the refresh channel succeeds when a receiver exists.
    #[tokio::test]
    async fn test_refresh_channel_send() {
        let orchestrator_state = make_orchestrator_state();
        let (refresh_tx, mut refresh_rx) = mpsc::channel::<()>(1);

        let app_state = Arc::new(AppState {
            orchestrator_state,
            refresh_tx,
        });

        // Simulate what trigger_refresh does.
        let result = app_state.refresh_tx.try_send(());
        assert!(result.is_ok());

        // The receiver should have one message waiting.
        let msg = refresh_rx.try_recv();
        assert!(msg.is_ok());
    }

    /// try_send returns an error when the channel is full (capacity = 1).
    #[tokio::test]
    async fn test_refresh_channel_full_does_not_panic() {
        let orchestrator_state = make_orchestrator_state();
        let (refresh_tx, _refresh_rx) = mpsc::channel::<()>(1);

        let app_state = Arc::new(AppState {
            orchestrator_state,
            refresh_tx,
        });

        // Fill the channel.
        let _ = app_state.refresh_tx.try_send(());
        // Second send hits capacity — trigger_refresh silently ignores this.
        let _ = app_state.refresh_tx.try_send(());
        // No panic means the test passes.
    }

    /// OrchestratorSnapshot serializes to JSON and contains expected keys.
    #[tokio::test]
    async fn test_snapshot_serializes_to_json() {
        let orchestrator_state = make_orchestrator_state();
        let state = orchestrator_state.lock().await;
        let snapshot = state.snapshot();
        let json_val = serde_json::to_value(&snapshot).expect("serialize snapshot");

        assert!(json_val.get("poll_interval_ms").is_some());
        assert!(json_val.get("max_concurrent_agents").is_some());
        assert!(json_val.get("running").is_some());
        assert!(json_val.get("retry_attempts").is_some());
        assert!(json_val.get("completed").is_some());
        assert!(json_val.get("claude_totals").is_some());
    }
}
