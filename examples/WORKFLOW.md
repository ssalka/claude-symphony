---
# =============================================================================
# Symphony Workflow — Example
# Adapt this template to your project's language, tooling, and structure.
# =============================================================================

# ===========================================================================
# tracker: connects to your Linear workspace
# ===========================================================================
tracker:
  kind: linear
  # Reference an env var so the key is never committed to source control.
  api_key: $LINEAR_API_KEY
  # The slug of your Linear team (visible in the URL: linear.app/<slug>/...)
  # Replace this with your actual team slug before running.
  project_slug: YOUR_TEAM_SLUG

# ===========================================================================
# workspace: one isolated directory per Linear issue
# ===========================================================================
workspace:
  # A directory outside your source repo where workspaces are created.
  root: ~/workspaces

  # Run after Symphony creates the workspace directory.
  # Customize to match your project's setup:
  #   - Copy the source tree (excluding build artifacts for speed)
  #   - Install dependencies
  #
  # Examples:
  #   Node/pnpm:  rsync -a --exclude node_modules /path/to/project/. . && pnpm install
  #   Node/npm:   rsync -a --exclude node_modules /path/to/project/. . && npm install
  #   Python:     rsync -a --exclude .venv /path/to/project/. . && pip install -e .
  #   Rust:       rsync -a --exclude target /path/to/project/. .
  after_create: |
    rsync -a /path/to/your/project/. .

# ===========================================================================
# agent: how Claude Code is invoked
# ===========================================================================
agent:
  command: claude
  # auto: Claude decides when to use tools without asking; suitable for CI-like
  # environments where no human is present to approve individual tool calls.
  approval_policy: auto
  # 40 turns is generous for a typical feature or bug fix.
  max_turns: 40
  # How long to wait for the agent to produce any output (30 s default).
  read_timeout_ms: 30000
  # Cap on a single turn (10 min default — raise for very long compilations).
  turn_timeout_ms: 600000
  # If the agent has not made progress in 2 min, log a warning.
  stall_timeout_ms: 120000
  # Guidance injected when the agent needs to continue across turns.
  continuation_guidance: >
    Continue working on the task. Pick up exactly where you left off.

# ===========================================================================
# orchestrator: poll loop and concurrency controls
# ===========================================================================
orchestrator:
  # How often to check Linear for new or updated issues (30 s).
  poll_interval_ms: 30000
  # Conservative limit for a local development machine.
  max_concurrent_agents: 2
  # Dispatch agents for issues in these states.
  active_states:
    - In Progress
  # Stop tracking issues once they reach these states.
  terminal_states:
    - Done
    - Cancelled
    - Duplicate
  # Optional: move the issue to this state after the agent completes successfully.
  # Useful for routing completed work into a human review queue before closing.
  # review_state: In Review

# ===========================================================================
# server: optional HTTP dashboard (also enabled via --port CLI flag)
# ===========================================================================
server:
  enabled: false
  port: 8080

---
You are an autonomous software engineer working inside an isolated copy of
the project repository.

Your task is to resolve the following Linear issue end-to-end:

---
**Issue:** {{ identifier }} — {{ title }}
**State:** {{ state }}
{% if priority %}**Priority:** {{ priority }}{% endif %}
{% if labels.size > 0 %}**Labels:** {% for l in labels %}{{ l }}{% unless forloop.last %}, {% endunless %}{% endfor %}{% endif %}
{% if url %}**URL:** {{ url }}{% endif %}
{% if branch_name %}**Branch:** {{ branch_name }}{% endif %}
{% if blocked_by.size > 0 %}
**Blocked by:**
{% for b in blocked_by %}
- {{ b.identifier }} ({{ b.state }})
{% endfor %}
{% endif %}

**Description:**
{{ description }}

---
## Instructions

1. **Understand the codebase first.** Read relevant files before making any
   changes. Use `find`, `grep`, or direct file reads to locate the code that
   needs to change.

2. **Work on the correct branch.** The suggested branch name is
   `{{ branch_name }}`. Create it from `main` if it doesn't exist:
   ```
   git checkout main && git pull && git checkout -b {{ branch_name }}
   ```

3. **Make focused, minimal changes.** Only touch what is necessary to resolve
   this issue. Avoid unrelated refactors.

4. **Run checks before committing.** Use whatever check/test/lint commands
   your project provides. Fix any failures before proceeding.

5. **Commit your work** with a clear, descriptive message referencing the
   issue identifier:
   ```
   git commit -m "{{ identifier }}: <short summary of what you did>"
   ```

6. **Open a pull request** when the work is complete:
   ```
   gh pr create --title "{{ identifier }}: {{ title }}" --body "Closes {{ url }}"
   ```

{% if attempt and attempt > 1 %}
---
**Note:** This is attempt {{ attempt }}. A previous run did not complete
successfully. Review any partial work already committed on this branch before
continuing.
{% endif %}
