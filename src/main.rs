pub mod agent;
pub mod config;
pub mod domain;
pub mod error;
pub mod orchestrator;
pub mod prompt;
pub mod server;
pub mod tracker;
pub mod workflow;
pub mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use crate::config::ServiceConfig;
use crate::orchestrator::Orchestrator;
use crate::tracker::linear::LinearClient;
use crate::workflow::WorkflowLoader;

// -------------------------------------------------------------------------- //
// CLI args
// -------------------------------------------------------------------------- //

#[derive(Parser, Debug)]
#[command(name = "symphony", about = "Orchestrate coding agents for Linear issues")]
struct Args {
    /// Path to WORKFLOW.md file
    #[arg(default_value = "./WORKFLOW.md")]
    workflow_path: String,

    /// Port for HTTP status server (optional)
    #[arg(long)]
    port: Option<u16>,
}

// -------------------------------------------------------------------------- //
// Shutdown signal
// -------------------------------------------------------------------------- //

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
}

// -------------------------------------------------------------------------- //
// Entry point
// -------------------------------------------------------------------------- //

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Init tracing subscriber (env-filter)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("symphony=info".parse()?),
        )
        .init();

    // 2. Parse CLI args
    let args = Args::parse();

    // 3. Validate workflow file exists
    let workflow_path = PathBuf::from(&args.workflow_path);
    if !workflow_path.exists() {
        eprintln!(
            "Error: Workflow file not found: {}",
            workflow_path.display()
        );
        std::process::exit(1);
    }

    // 4. Load workflow + start file watcher
    let loader = WorkflowLoader::new(workflow_path);
    let config_rx = loader.start_watcher()?;
    let initial_workflow = config_rx.borrow().as_ref().clone();

    // 5. Parse config to get tracker settings
    let config = ServiceConfig::from_yaml(&initial_workflow.config)?;

    // 6. Create tracker
    let tracker = Arc::new(LinearClient::new(
        config.tracker_api_key.clone(),
        config.tracker_endpoint.clone(),
    ));

    // 7. Create refresh channel for HTTP server
    let (refresh_tx, refresh_rx) = tokio::sync::mpsc::channel::<()>(10);

    // 8. Create orchestrator
    let orchestrator = Orchestrator::new(initial_workflow, config_rx, tracker, refresh_rx);
    let state = Arc::clone(&orchestrator.state);

    // 9. Run startup cleanup
    orchestrator.startup_cleanup(&config).await;

    // 10. Optionally start HTTP server
    if let Some(port) = args.port {
        tokio::spawn(server::serve(port, Arc::clone(&state), refresh_tx.clone()));
    }

    // 11. Set up graceful shutdown on SIGINT/SIGTERM
    let shutdown = shutdown_signal();

    // 12. Run orchestrator until shutdown
    tokio::select! {
        _ = orchestrator.run() => {
            tracing::info!("Orchestrator exited");
        }
        _ = shutdown => {
            tracing::info!("Shutting down...");
        }
    }

    Ok(())
}
