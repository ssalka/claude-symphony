# Setting up Symphony

This guide walks you through running Symphony — the Linear-to-Claude-Code
automation daemon — against your project.

## Prerequisites

| Tool | Check | Install |
|------|-------|---------|
| Rust toolchain (stable) | `rustc --version` | https://rustup.rs |
| Claude Code CLI | `claude --version` | `pnpm i -g @anthropic/claude-code` |
| GitHub CLI (for PRs) | `gh --version` | https://cli.github.com |
| Linear API key | — | Linear → Settings → API → Personal API keys |

## 1. Build Symphony

```bash
cd path/to/claude-symphony
cargo build --release
```

Add the binary to your PATH (one-time):

```bash
# Add to ~/.zshrc or ~/.bashrc
export PATH="path/to/claude-symphony/target/release:$PATH"
```

Or call it by path: `path/to/claude-symphony/target/release/symphony`.

## 2. Set your Linear API key

Symphony reads `LINEAR_API_KEY` from the environment. Export it in your shell
profile so it is always available:

```bash
export LINEAR_API_KEY=lin_api_xxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

Verify it is set:

```bash
echo $LINEAR_API_KEY
```

## 3. Create the workspace root

Symphony will create one subdirectory here per Linear issue:

```bash
mkdir -p ~/workspaces
```

## 4. Configure the workflow file

Copy the example and fill in your details:

```bash
cp examples/WORKFLOW.md ./my_project/WORKFLOW.md
```

Open `WORKFLOW.md` in your project and set:

```yaml
tracker:
  # e.g. linear.app/org/project/myproject-56c29d052a37/overview → "myproject-56c29d052a37"
  project_slugs:
    - myproject-56c29d052a37
```

Update the `workspace` section to match your project:

```yaml
workspace:
  root: ~/workspaces
  after_create: |
    rsync -a --exclude node_modules /path/to/your/project/. . && npm install
```

Key settings at a glance:

| Setting | Value | Purpose |
|---------|-------|---------|
| `workspace.root` | `~/workspaces` | Where agent workspaces live |
| `workspace.after_create` | `rsync … && install` | Seeds each workspace from your local clone |
| `agent.approval_policy` | `auto` | Claude acts without prompting for approval |
| `agent.max_turns` | `40` | Max turns per issue before giving up |
| `orchestrator.active_states` | `[In Progress]` | Only dispatch for issues in this state |
| `orchestrator.max_concurrent_agents` | `2` | Run at most 2 agents at once |

## 5. Run Symphony

**Without HTTP API:**

```bash
symphony
```

**With HTTP dashboard (recommended for monitoring):**

```bash
symphony --port 8080
```

The `--port` flag overrides `server.enabled` and `server.port` in the YAML,
so you can leave `server.enabled: false` in the file and pass `--port` only
when you want the API.

## 6. Verify startup

Expected log output (with default `info` level):

```
INFO symphony: Loaded workflow: WORKFLOW.md
INFO symphony::orchestrator: Starting poll loop (interval=30000ms)
INFO symphony::server: Listening on 0.0.0.0:8080
```

Enable debug logging for more detail:

```bash
RUST_LOG=debug symphony
```

Or trace logging to include output from Claude Code:
```bash
RUST_LOG=trace symphony
```

### Check the HTTP API

```bash
# Overall daemon state
curl -s http://localhost:8080/api/v1/state | jq .

# State for a specific issue (e.g. ENG-42)
curl -s http://localhost:8080/api/v1/ENG-42 | jq .

# Force an immediate Linear poll (bypasses the 30 s interval)
curl -s -X POST http://localhost:8080/api/v1/refresh
```

A healthy empty state looks like:

```json
{
  "running": {},
  "retry": []
}
```

## 7. What happens when an issue goes "In Progress"

1. Symphony detects the state change on the next poll.
2. It creates `~/workspaces/ENG-42/` (using the identifier as the key).
3. `after_create` runs: rsync copies your project source, then your install command hydrates dependencies.
4. The Liquid prompt template is rendered with the issue's fields and sent to `claude`.
5. Claude Code runs inside the workspace, makes changes, commits, and opens a PR.
6. When the agent exits (or `max_turns` is reached), Symphony cleans up and marks the session complete.
7. The workspace directory is removed.

You can watch workspaces appear in real time:

```bash
watch -n 5 ls ~/workspaces/
```

## 8. Customising the prompt

The Liquid template in the `---` body of the workflow file controls exactly
what Claude is asked to do. All of these variables are available:

| Variable | Type | Description |
|----------|------|-------------|
| `identifier` | string | Issue ID, e.g. `ENG-42` |
| `title` | string | Issue title |
| `description` | string | Issue description (empty string if unset) |
| `state` | string | Current workflow state, e.g. `In Progress` |
| `priority` | integer \| nil | 1 = urgent, 2 = high, 3 = medium, 4 = low |
| `branch_name` | string \| nil | Suggested git branch name from Linear |
| `url` | string \| nil | Direct link to the issue in Linear |
| `labels` | array of strings | Issue labels |
| `blocked_by` | array of objects | Each has `.id`, `.identifier`, `.state` |
| `created_at` | string \| nil | ISO-8601 creation timestamp |
| `updated_at` | string \| nil | ISO-8601 last-updated timestamp |
| `attempt` | integer \| nil | Retry count (nil on first attempt, 1, 2, … on retries) |

Use standard Liquid syntax: `{{ variable }}`, `{% if variable %}…{% endif %}`,
`{% for item in array %}…{% endfor %}`.

## 9. Tuning concurrency and polling

Adjust these in the YAML front matter:

```yaml
orchestrator:
  # Raise if your machine has enough RAM/CPU for more parallel agents.
  max_concurrent_agents: 4

  # Reduce for faster feedback; increase to stay within Linear rate limits.
  poll_interval_ms: 15000

  # Optional: cap by state independently of the global limit.
  max_concurrent_agents_by_state:
    in progress: 2
    todo: 1
```

## Troubleshooting

**`MissingTrackerApiKey`** — `LINEAR_API_KEY` is not set or is empty. Re-export it and restart.

**`MissingTrackerProjectSlug`** — `project_slug` in the YAML is blank. Fill it in with your Linear team slug.

**`ClaudeNotFound`** — `claude` is not on `PATH`. Run `which claude` to confirm, or set `agent.command` to the full path.

**`after_create` fails** — The rsync source path must exist and any install commands must be on `PATH` in your login shell (Symphony spawns `bash -lc`).

**Workspaces not appearing** — Check that the issue's state matches one of the `active_states` entries exactly (case-insensitive comparison). Use `POST /api/v1/refresh` to trigger an immediate poll.
