use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::{Error, Result};

// -------------------------------------------------------------------------- //
// ServiceConfig
// -------------------------------------------------------------------------- //

/// Full configuration for the Symphony daemon, parsed from a workflow file's
/// YAML front matter.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    // tracker
    pub tracker_kind: String,
    pub tracker_api_key: String,
    pub tracker_project_slug: String,
    pub tracker_endpoint: Option<String>,
    // workspace
    pub workspace_root: PathBuf,
    pub workspace_after_create: Option<String>,
    pub workspace_before_remove: Option<String>,
    // agent
    pub agent_command: String,
    pub agent_approval_policy: String,
    pub agent_sandbox_policy: Option<serde_json::Value>,
    pub agent_max_turns: u32,
    pub agent_read_timeout_ms: u64,
    pub agent_turn_timeout_ms: u64,
    pub agent_stall_timeout_ms: u64,
    pub agent_max_retry_backoff_ms: u64,
    pub agent_continuation_guidance: String,
    // orchestrator
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: usize,
    pub max_concurrent_agents_by_state: HashMap<String, usize>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
    // server
    pub server_enabled: bool,
    pub server_port: u16,
}

// -------------------------------------------------------------------------- //
// Helpers
// -------------------------------------------------------------------------- //

/// Resolve a string value: if it starts with `$`, treat the rest as an env var
/// name and look it up. Returns `MissingTrackerApiKey` if the env var is unset
/// or empty.
fn resolve_env(raw: &str) -> Result<String> {
    if let Some(var_name) = raw.strip_prefix('$') {
        let val = std::env::var(var_name).unwrap_or_default();
        if val.is_empty() {
            return Err(Error::MissingTrackerApiKey);
        }
        Ok(val)
    } else {
        Ok(raw.to_string())
    }
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// Parse a YAML value that is either a sequence of strings or a single
/// comma-separated string. Returns each entry trimmed and lowercased.
fn parse_state_list(val: &serde_yaml::Value) -> Vec<String> {
    match val {
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_lowercase())
            .collect(),
        serde_yaml::Value::String(s) => s
            .split(',')
            .map(|part| part.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    }
}

/// Retrieve an optional string from a YAML map.
fn opt_str<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    map.get(key).and_then(|v| v.as_str())
}

/// Retrieve a required string from a YAML map, returning empty string if absent.
fn req_str<'a>(map: &'a serde_yaml::Mapping, key: &str) -> &'a str {
    map.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

// -------------------------------------------------------------------------- //
// Constructor
// -------------------------------------------------------------------------- //

impl ServiceConfig {
    /// Parse a `ServiceConfig` from a raw `serde_yaml::Value` (the YAML front
    /// matter of a workflow file).
    pub fn from_yaml(raw: &serde_yaml::Value) -> Result<ServiceConfig> {
        // Helper: get a sub-map or return an empty mapping.
        let empty_map = serde_yaml::Mapping::new();
        let get_map = |key: &str| -> &serde_yaml::Mapping {
            raw.get(key)
                .and_then(|v| v.as_mapping())
                .unwrap_or(&empty_map)
        };

        // ---- tracker --------------------------------------------------------
        let tracker = get_map("tracker");
        let tracker_kind = req_str(tracker, "kind").to_string();

        // api_key may be an env var reference.
        let raw_api_key = req_str(tracker, "api_key");
        let tracker_api_key = if raw_api_key.is_empty() {
            String::new()
        } else {
            resolve_env(raw_api_key)?
        };

        let tracker_project_slug = req_str(tracker, "project_slug").to_string();
        let tracker_endpoint = opt_str(tracker, "endpoint").map(str::to_string);

        // ---- workspace ------------------------------------------------------
        let workspace = get_map("workspace");
        let raw_root = req_str(workspace, "root");
        let workspace_root = if raw_root.is_empty() {
            PathBuf::from(".")
        } else {
            expand_tilde(raw_root)
        };
        let workspace_after_create = opt_str(workspace, "after_create").map(str::to_string);
        let workspace_before_remove = opt_str(workspace, "before_remove").map(str::to_string);

        // ---- agent ----------------------------------------------------------
        let agent = get_map("agent");
        let agent_command = req_str(agent, "command").to_string();
        let agent_approval_policy = opt_str(agent, "approval_policy")
            .unwrap_or("auto")
            .to_string();

        // sandbox_policy is an optional JSON object stored as serde_json::Value.
        let agent_sandbox_policy = agent
            .get("sandbox_policy")
            .and_then(|v| {
                // Convert from serde_yaml::Value to serde_json::Value.
                serde_json::to_value(v).ok()
            })
            .and_then(|v| if v.is_null() { None } else { Some(v) });

        let agent_max_turns = agent
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(20);

        let agent_read_timeout_ms = agent
            .get("read_timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);

        let agent_turn_timeout_ms = agent
            .get("turn_timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(600_000);

        let agent_stall_timeout_ms = agent
            .get("stall_timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000);

        let agent_max_retry_backoff_ms = agent
            .get("max_retry_backoff_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(300_000);

        let agent_continuation_guidance = opt_str(agent, "continuation_guidance")
            .unwrap_or("Continue working on the task.")
            .to_string();

        // ---- orchestrator ---------------------------------------------------
        let orchestrator = get_map("orchestrator");

        let poll_interval_ms = orchestrator
            .get("poll_interval_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);

        let max_concurrent_agents = orchestrator
            .get("max_concurrent_agents")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(3);

        let max_concurrent_agents_by_state = orchestrator
            .get("max_concurrent_agents_by_state")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let key = k.as_str()?.trim().to_lowercase();
                        let val = v.as_u64()? as usize;
                        Some((key, val))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let active_states = orchestrator
            .get("active_states")
            .map(|v| parse_state_list(v))
            .unwrap_or_default();

        let terminal_states = orchestrator
            .get("terminal_states")
            .map(|v| parse_state_list(v))
            .unwrap_or_default();

        // ---- server ---------------------------------------------------------
        let server = get_map("server");

        let server_enabled = server
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let server_port = server
            .get("port")
            .and_then(|v| v.as_u64())
            .map(|n| n as u16)
            .unwrap_or(8080);

        Ok(ServiceConfig {
            tracker_kind,
            tracker_api_key,
            tracker_project_slug,
            tracker_endpoint,
            workspace_root,
            workspace_after_create,
            workspace_before_remove,
            agent_command,
            agent_approval_policy,
            agent_sandbox_policy,
            agent_max_turns,
            agent_read_timeout_ms,
            agent_turn_timeout_ms,
            agent_stall_timeout_ms,
            agent_max_retry_backoff_ms,
            agent_continuation_guidance,
            poll_interval_ms,
            max_concurrent_agents,
            max_concurrent_agents_by_state,
            active_states,
            terminal_states,
            server_enabled,
            server_port,
        })
    }

    // -------------------------------------------------------------------------- //
    // Validation
    // -------------------------------------------------------------------------- //

    /// Check that all fields required to dispatch work are present and valid.
    ///
    /// Returns the first validation error found, or `Ok(())`.
    pub fn validate_for_dispatch(&self) -> Result<()> {
        if self.tracker_kind != "linear" {
            return Err(Error::UnsupportedTrackerKind {
                kind: self.tracker_kind.clone(),
            });
        }
        if self.tracker_api_key.is_empty() {
            return Err(Error::MissingTrackerApiKey);
        }
        if self.tracker_project_slug.is_empty() {
            return Err(Error::MissingTrackerProjectSlug);
        }
        if self.agent_command.is_empty() {
            return Err(Error::ClaudeNotFound {
                command: String::new(),
            });
        }
        if self.active_states.is_empty() {
            return Err(Error::ConfigValidation {
                message: "orchestrator.active_states is empty; no issues will be fetched".into(),
            });
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

    /// Build a minimal valid YAML value for `ServiceConfig::from_yaml`.
    fn minimal_yaml(api_key: &str) -> serde_yaml::Value {
        serde_yaml::from_str(&format!(
            r#"
tracker:
  kind: linear
  api_key: "{api_key}"
  project_slug: my-project

workspace:
  root: /tmp/workspaces

agent:
  command: claude

orchestrator:
  active_states:
    - in progress
    - todo
"#
        ))
        .unwrap()
    }

    // ---- basic parsing ------------------------------------------------------

    #[test]
    fn test_parse_minimal_config() {
        let yaml = minimal_yaml("my-api-key");
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();

        assert_eq!(cfg.tracker_kind, "linear");
        assert_eq!(cfg.tracker_api_key, "my-api-key");
        assert_eq!(cfg.tracker_project_slug, "my-project");
        assert_eq!(cfg.workspace_root, PathBuf::from("/tmp/workspaces"));
        assert_eq!(cfg.agent_command, "claude");
    }

    #[test]
    fn test_defaults_applied() {
        let yaml = minimal_yaml("key");
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();

        assert_eq!(cfg.agent_approval_policy, "auto");
        assert_eq!(cfg.agent_max_turns, 20);
        assert_eq!(cfg.agent_read_timeout_ms, 30_000);
        assert_eq!(cfg.agent_turn_timeout_ms, 600_000);
        assert_eq!(cfg.agent_stall_timeout_ms, 120_000);
        assert_eq!(cfg.agent_max_retry_backoff_ms, 300_000);
        assert_eq!(
            cfg.agent_continuation_guidance,
            "Continue working on the task."
        );
        assert_eq!(cfg.poll_interval_ms, 30_000);
        assert_eq!(cfg.max_concurrent_agents, 3);
        assert!(!cfg.server_enabled);
        assert_eq!(cfg.server_port, 8080);
    }

    #[test]
    fn test_explicit_values_override_defaults() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: "k"
  project_slug: proj

workspace:
  root: /tmp

agent:
  command: /usr/bin/claude
  approval_policy: always
  max_turns: 50
  read_timeout_ms: 5000
  turn_timeout_ms: 100000
  stall_timeout_ms: 60000
  max_retry_backoff_ms: 10000
  continuation_guidance: "Keep going!"

orchestrator:
  poll_interval_ms: 10000
  max_concurrent_agents: 5

server:
  enabled: true
  port: 9090
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();

        assert_eq!(cfg.agent_command, "/usr/bin/claude");
        assert_eq!(cfg.agent_approval_policy, "always");
        assert_eq!(cfg.agent_max_turns, 50);
        assert_eq!(cfg.agent_read_timeout_ms, 5000);
        assert_eq!(cfg.agent_turn_timeout_ms, 100_000);
        assert_eq!(cfg.agent_stall_timeout_ms, 60_000);
        assert_eq!(cfg.agent_max_retry_backoff_ms, 10_000);
        assert_eq!(cfg.agent_continuation_guidance, "Keep going!");
        assert_eq!(cfg.poll_interval_ms, 10_000);
        assert_eq!(cfg.max_concurrent_agents, 5);
        assert!(cfg.server_enabled);
        assert_eq!(cfg.server_port, 9090);
    }

    // ---- state list normalization -------------------------------------------

    #[test]
    fn test_state_list_from_yaml_sequence() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
workspace:
  root: /tmp
agent:
  command: claude
orchestrator:
  active_states:
    - In Progress
    - Todo
  terminal_states:
    - Done
    - Cancelled
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert_eq!(cfg.active_states, vec!["in progress", "todo"]);
        assert_eq!(cfg.terminal_states, vec!["done", "cancelled"]);
    }

    #[test]
    fn test_state_list_from_comma_separated_string() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
workspace:
  root: /tmp
agent:
  command: claude
orchestrator:
  active_states: "In Progress, Todo"
  terminal_states: "Done, Cancelled, Duplicate"
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert_eq!(cfg.active_states, vec!["in progress", "todo"]);
        assert_eq!(cfg.terminal_states, vec!["done", "cancelled", "duplicate"]);
    }

    // ---- env var resolution -------------------------------------------------

    #[test]
    fn test_env_var_resolution_success() {
        std::env::set_var("SYMPHONY_TEST_API_KEY", "resolved-key-123");
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: $SYMPHONY_TEST_API_KEY
  project_slug: proj
workspace:
  root: /tmp
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert_eq!(cfg.tracker_api_key, "resolved-key-123");
        std::env::remove_var("SYMPHONY_TEST_API_KEY");
    }

    #[test]
    fn test_env_var_resolution_missing_returns_error() {
        // Ensure the var is definitely not set.
        std::env::remove_var("SYMPHONY_TEST_MISSING_VAR_XYZ");
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: $SYMPHONY_TEST_MISSING_VAR_XYZ
  project_slug: proj
workspace:
  root: /tmp
agent:
  command: claude
"#,
        )
        .unwrap();
        let err = ServiceConfig::from_yaml(&yaml).unwrap_err();
        assert!(matches!(err, crate::error::Error::MissingTrackerApiKey));
    }

    // ---- tilde expansion ----------------------------------------------------

    #[test]
    fn test_tilde_expansion() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
workspace:
  root: ~/code/workspaces
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        // The path should not start with `~` after expansion.
        let root_str = cfg.workspace_root.to_string_lossy();
        assert!(
            !root_str.starts_with('~'),
            "tilde was not expanded: {root_str}"
        );
        assert!(
            root_str.ends_with("code/workspaces"),
            "unexpected suffix: {root_str}"
        );
    }

    // ---- max_concurrent_agents_by_state -------------------------------------

    #[test]
    fn test_max_concurrent_agents_by_state() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
workspace:
  root: /tmp
agent:
  command: claude
orchestrator:
  max_concurrent_agents_by_state:
    in progress: 2
    todo: 1
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert_eq!(cfg.max_concurrent_agents_by_state.get("in progress"), Some(&2));
        assert_eq!(cfg.max_concurrent_agents_by_state.get("todo"), Some(&1));
    }

    // ---- validate_for_dispatch ----------------------------------------------

    #[test]
    fn test_validate_for_dispatch_ok() {
        let yaml = minimal_yaml("good-key");
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert!(cfg.validate_for_dispatch().is_ok());
    }

    #[test]
    fn test_validate_for_dispatch_unsupported_tracker() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: jira
  api_key: k
  project_slug: p
workspace:
  root: /tmp
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        let err = cfg.validate_for_dispatch().unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::UnsupportedTrackerKind { .. }
        ));
    }

    #[test]
    fn test_validate_for_dispatch_missing_api_key() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: ""
  project_slug: p
workspace:
  root: /tmp
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        let err = cfg.validate_for_dispatch().unwrap_err();
        assert!(matches!(err, crate::error::Error::MissingTrackerApiKey));
    }

    #[test]
    fn test_validate_for_dispatch_missing_project_slug() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: ""
workspace:
  root: /tmp
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        let err = cfg.validate_for_dispatch().unwrap_err();
        assert!(matches!(err, crate::error::Error::MissingTrackerProjectSlug));
    }

    #[test]
    fn test_validate_for_dispatch_missing_claude_command() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
workspace:
  root: /tmp
agent:
  command: ""
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        let err = cfg.validate_for_dispatch().unwrap_err();
        assert!(matches!(err, crate::error::Error::ClaudeNotFound { .. }));
    }

    #[test]
    fn test_validate_for_dispatch_empty_active_states() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
workspace:
  root: /tmp
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        let err = cfg.validate_for_dispatch().unwrap_err();
        assert!(matches!(err, crate::error::Error::ConfigValidation { .. }));
    }

    // ---- optional fields ----------------------------------------------------

    #[test]
    fn test_workspace_hooks_optional() {
        let yaml = minimal_yaml("key");
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert!(cfg.workspace_after_create.is_none());
        assert!(cfg.workspace_before_remove.is_none());
    }

    #[test]
    fn test_workspace_hooks_present() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
workspace:
  root: /tmp
  after_create: "git clone ..."
  before_remove: "rm -rf ..."
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert_eq!(cfg.workspace_after_create.as_deref(), Some("git clone ..."));
        assert_eq!(cfg.workspace_before_remove.as_deref(), Some("rm -rf ..."));
    }

    #[test]
    fn test_tracker_endpoint_optional() {
        let yaml = minimal_yaml("key");
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert!(cfg.tracker_endpoint.is_none());
    }

    #[test]
    fn test_tracker_endpoint_present() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: k
  project_slug: p
  endpoint: https://api.example.com/graphql
workspace:
  root: /tmp
agent:
  command: claude
"#,
        )
        .unwrap();
        let cfg = ServiceConfig::from_yaml(&yaml).unwrap();
        assert_eq!(
            cfg.tracker_endpoint.as_deref(),
            Some("https://api.example.com/graphql")
        );
    }
}
