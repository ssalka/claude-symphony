# CLAUDE.md

## Commands

```bash
cargo build                   # build
cargo build --release         # optimized build
cargo check                   # fast syntax/type check
cargo test                    # run all tests
cargo test <name>             # filter tests by name
cargo test config::tests      # run all tests in a module
cargo test -- --nocapture     # show println! output
cargo clippy -- -D warnings   # lint
cargo fmt --check             # format check
cargo run -- ./WORKFLOW.md    # run with workflow file
cargo run -- ./WORKFLOW.md --port 8080  # run with HTTP API
```

Logging: `RUST_LOG=debug symphony ./WORKFLOW.md` (default: `symphony=info`)

## Architecture

Single binary (`symphony`) that polls Linear for issues, spins up isolated workspaces, and runs Claude Code as a subprocess agent for each issue.

**Data flow:**
```
main.rs
  -> WorkflowLoader (parses WORKFLOW.md YAML front-matter + Liquid prompt body, hot-reloads via notify)
  -> ServiceConfig::from_yaml()
  -> LinearClient (GraphQL, paginated)
  -> Orchestrator (poll loop + dispatch + reconcile + retry state machine)
      |-- AgentRunner (per issue: workspace -> prompt -> ClaudeSession subprocess -> turn loop)
  -> server::serve() (optional axum HTTP API)
```

**Key modules:**
- `domain.rs` — all shared data types (`Issue`, `LiveSession`, `RunningEntry`, `RetryEntry`, `OrchestratorState`, `WorkflowDefinition`, `AgentEvent`, `WorkerEvent`)
- `config.rs` — `ServiceConfig::from_yaml()`, `$ENV_VAR` resolution, `~` expansion
- `workflow.rs` — `WorkflowLoader` with file watcher + hot-reload via watch channel
- `orchestrator.rs` — poll loop, concurrency limits (`max_concurrent_agents`, per-state limits), exponential backoff retry
- `agent/session.rs` — `ClaudeSession`: spawns `bash -lc`, JSON-RPC handshake, `LinesCodec` stdout streaming, SIGTERM via `libc::kill`
- `agent/mod.rs` — `AgentRunner`: glues workspace + prompt + session, forwards events to orchestrator
- `workspace.rs` — create/cleanup workspace dirs, run `after_create`/`before_remove` shell hooks
- `server.rs` — axum: `GET /api/v1/state`, `GET /api/v1/:identifier`, `POST /api/v1/refresh`
- `error.rs` — flat `thiserror` enum for all error variants

**Workflow file format:** YAML front-matter (`---`) + Liquid template body. The front-matter configures tracker (Linear), workspace, agent, orchestrator, and server. The Liquid body is the prompt sent to Claude with variables: `identifier`, `title`, `description`, `state`, `priority`, `labels`, `blocked_by`, `branch_name`, `url`, `attempt`, etc.

All tests are in `#[cfg(test)]` modules inline in each source file (no separate `tests/` directory).
