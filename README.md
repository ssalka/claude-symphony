# claude-symphony

![Rust](https://img.shields.io/badge/rust-stable-orange.svg)
![Tokio](https://img.shields.io/badge/async-tokio-blue.svg)
![Linear](https://img.shields.io/badge/tracker-Linear-5E6AD2.svg)
![Claude Code](https://img.shields.io/badge/agent-Claude%20Code-blueviolet.svg)

A daemon that watches your Linear board and dispatches agents to resolve issues automatically. When an issue moves to "In Progress", Symphony creates an isolated workspace, seeds it with your codebase, renders a Liquid prompt from the issue's fields, and runs the agent in a subprocess until the work is done.

Inspired by [Symphony](https://github.com/openai/symphony) from OpenAI, re-made for Claude Code.

## Features

- Polls Linear for issues matching configurable workflow states
- Creates one isolated workspace directory per issue, seeded by a shell hook you control
- Renders a Liquid template prompt with full issue context (title, description, labels, branch name, blocked-by, retry count, and more)
- Runs Claude Code with JSON-RPC turn-by-turn streaming; enforces per-turn and stall timeouts
- Exponential-backoff retry on failure, with attempt count passed into the prompt
- Concurrency limits: global cap and optional per-state caps
- Hot-reloads the workflow file on save — no restart needed
- Optional HTTP API for monitoring and forcing immediate polls

## Quick start

### Prerequisites

| Tool | Check | Install |
|------|-------|---------|
| Rust (stable) | `rustc --version` | https://rustup.rs |
| Claude Code CLI | `claude --version` | `npm i -g @anthropic/claude-code` |
| GitHub CLI | `gh --version` | https://cli.github.com |
| Linear API key | — | Linear → Settings → API → Personal API keys |

### 1. Build

```bash
git clone https://github.com/your-org/claude-symphony
cd claude-symphony
cargo build --release
```

Add the binary to your PATH:

```bash
# Add to ~/.zshrc or ~/.bashrc
export PATH="$HOME/path/to/claude-symphony/target/release:$PATH"
```

### 2. Set your Linear API key

```bash
export LINEAR_API_KEY=lin_api_xxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

### 3. Create a workspace root

```bash
mkdir -p ~/workspaces
```

### 4. Configure your workflow file

Copy the example and fill in your details:

```bash
cp examples/WORKFLOW.md ./WORKFLOW.md
```

At minimum, set your Linear team slug and the `after_create` hook that seeds each workspace:

```yaml
tracker:
  api_key: $LINEAR_API_KEY
  project_slugs:
    - your-project-239b7e6def9a   # from the project url in Linear

workspace:
  root: ~/workspaces
  after_create: |
    rsync -a --exclude node_modules /path/to/your/project/. . && npm install
```

### 5. Run

```bash
# Without HTTP API
symphony WORKFLOW.md

# With HTTP dashboard
symphony WORKFLOW.md --port 8080
```

When a Linear issue enters a state listed in `orchestrator.active_states`, Symphony will pick it up within one poll interval.

## Workflow file format

The workflow file is a single Markdown file with a YAML front-matter block followed by a Liquid prompt template.

```
---
<YAML configuration>
---
<Liquid prompt template>
```

### Full configuration reference

```yaml
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY          # env var reference — never commit the raw key
  project_slugs:
    - your-project-239b7e6def9a     # from the project url in Linear

workspace:
  root: ~/workspaces
  after_create: |                   # runs after the workspace directory is created
    rsync -a --exclude node_modules /path/to/project/. . && pnpm install
  before_remove: |                  # optional: runs before the workspace is deleted
    echo "cleaning up"

agent:
  command: claude                   # path to Claude Code binary
  approval_policy: auto             # "auto" = no human approval prompts
  max_turns: 40                     # max turns before giving up
  read_timeout_ms: 30000            # wait up to 30 s for any output
  turn_timeout_ms: 600000           # max 10 min per turn
  stall_timeout_ms: 120000          # warn if no progress for 2 min
  continuation_guidance: >
    Continue working on the task. Pick up exactly where you left off.

orchestrator:
  poll_interval_ms: 30000           # check Linear every 30 s
  max_concurrent_agents: 2          # global concurrency cap
  max_concurrent_agents_by_state:   # optional per-state caps
    in progress: 2
  active_states:
    - In Progress                   # dispatch agents for issues in these states
  terminal_states:
    - Done
    - Cancelled
    - Duplicate

server:
  enabled: false
  port: 8080
```

### Template variables

The Liquid template body receives these variables for each issue:

| Variable | Type | Description |
|----------|------|-------------|
| `identifier` | string | Issue ID, e.g. `ENG-42` |
| `title` | string | Issue title |
| `description` | string | Issue description |
| `state` | string | Current workflow state, e.g. `In Progress` |
| `priority` | integer \| nil | 1 = urgent, 2 = high, 3 = medium, 4 = low |
| `labels` | array of strings | Issue labels |
| `branch_name` | string \| nil | Suggested git branch name from Linear |
| `url` | string \| nil | Direct link to the issue |
| `blocked_by` | array of objects | Each has `.identifier` and `.state` |
| `created_at` | string \| nil | ISO-8601 creation timestamp |
| `updated_at` | string \| nil | ISO-8601 last-updated timestamp |
| `attempt` | integer \| nil | Retry count (nil on first attempt, 1, 2, … on retries) |

Use standard Liquid syntax:

```liquid
**Issue:** {{ identifier }} — {{ title }}
{% if priority %}**Priority:** {{ priority }}{% endif %}
{% for label in labels %}{{ label }}{% endfor %}
{% if attempt and attempt > 1 %}This is retry attempt {{ attempt }}.{% endif %}
```

## HTTP API

Enable with `--port 8080` or `server.enabled: true` in the YAML.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/state` | Full daemon state (running sessions + retry queue) |
| `GET` | `/api/v1/:identifier` | State for a single issue, e.g. `/api/v1/ENG-42` |
| `POST` | `/api/v1/refresh` | Force an immediate Linear poll |
| `GET` | `/` | HTML dashboard |

```bash
curl -s http://localhost:8080/api/v1/state | jq .
curl -s http://localhost:8080/api/v1/ENG-42 | jq .
curl -s -X POST http://localhost:8080/api/v1/refresh
```

A healthy idle state:

```json
{
  "running": {},
  "retry": []
}
```

## Architecture

```
main.rs
  -> WorkflowLoader      — parse YAML front-matter + Liquid body; hot-reload via notify
  -> LinearClient        — GraphQL polling with pagination
  -> Orchestrator        — poll loop, dispatch, reconcile, exponential-backoff retry
      |-- AgentRunner    — per-issue: workspace setup -> prompt render -> ClaudeSession
  -> server::serve()     — optional axum HTTP API
```

**Key modules:**

| Module | Responsibility |
|--------|---------------|
| `domain.rs` | Shared types: `Issue`, `LiveSession`, `OrchestratorState`, `AgentEvent` |
| `config.rs` | `ServiceConfig::from_yaml()`, `$ENV_VAR` resolution, `~` expansion |
| `workflow.rs` | `WorkflowLoader` with file watcher and hot-reload |
| `orchestrator.rs` | Poll loop, concurrency limits, retry state machine |
| `agent/session.rs` | `ClaudeSession`: spawns `bash -lc`, JSON-RPC handshake, stdout streaming |
| `agent/mod.rs` | `AgentRunner`: glues workspace + prompt + session |
| `workspace.rs` | Create/cleanup workspace dirs, run shell hooks |
| `server.rs` | axum HTTP API |
| `error.rs` | Flat `thiserror` enum for all error variants |

## Development

```bash
cargo build                    # debug build
cargo build --release          # optimized build
cargo check                    # fast type check
cargo test                     # run all tests
cargo test -- --nocapture      # show println! output
cargo clippy -- -D warnings    # lint
cargo fmt --check              # format check
```

Logging is controlled by `RUST_LOG`:

```bash
RUST_LOG=debug symphony WORKFLOW.md    # verbose
RUST_LOG=trace symphony WORKFLOW.md    # includes Claude Code output
```

## Troubleshooting

**`MissingTrackerApiKey`** — `LINEAR_API_KEY` is not set. Export it and restart.

**`MissingTrackerProjectSlug`** — `project_slug` in the YAML is blank. Fill it in with your Linear team slug.

**`ClaudeNotFound`** — `claude` is not on `PATH`. Run `which claude` to verify, or set `agent.command` to the full path.

**`after_create` fails** — The rsync source path must exist, and any install commands must be available in your login shell (Symphony spawns `bash -lc`).

**Issues not picked up** — Confirm the issue's state exactly matches an entry in `active_states` (comparison is case-insensitive). Use `POST /api/v1/refresh` to trigger an immediate poll.
