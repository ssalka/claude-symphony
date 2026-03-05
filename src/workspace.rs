use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::config::ServiceConfig;
use crate::error::{Error, Result};

// -------------------------------------------------------------------------- //
// WorkspaceInfo
// -------------------------------------------------------------------------- //

/// Metadata about a workspace directory returned by [`create_for_issue`].
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    /// Absolute path to the workspace directory.
    pub path: std::path::PathBuf,
    /// Sanitized key derived from the issue identifier.
    pub workspace_key: String,
    /// `true` if the directory was created during this call, `false` if it
    /// already existed.
    pub created_now: bool,
}

// -------------------------------------------------------------------------- //
// Public API
// -------------------------------------------------------------------------- //

/// Replace every character that is not `[A-Za-z0-9._-]` with an underscore.
///
/// # Examples
/// ```
/// use symphony::workspace::sanitize_key;
/// assert_eq!(sanitize_key("ABC-123"), "ABC-123");
/// assert_eq!(sanitize_key("ABC/123"), "ABC_123");
/// assert_eq!(sanitize_key("hello world"), "hello_world");
/// ```
pub fn sanitize_key(identifier: &str) -> String {
    identifier
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Create (or reuse) a workspace directory for `identifier` under
/// `config.workspace_root`.
///
/// Steps:
/// 1. Sanitize the identifier into a workspace key.
/// 2. Compute the full workspace path.
/// 3. Perform a path-safety check to prevent directory traversal.
/// 4. Create the directory if it does not exist.
/// 5. Run `config.workspace_after_create` hook if the directory was just
///    created.
pub async fn create_for_issue(identifier: &str, config: &ServiceConfig) -> Result<WorkspaceInfo> {
    let workspace_key = sanitize_key(identifier);
    let workspace_path = config.workspace_root.join(&workspace_key);

    // Ensure workspace_root exists before canonicalizing.
    if !config.workspace_root.exists() {
        tokio::fs::create_dir_all(&config.workspace_root)
            .await
            .map_err(Error::WorkspaceCreationFailed)?;
    }

    // Canonicalize root and verify the path stays inside it.
    let canonical_root = config
        .workspace_root
        .canonicalize()
        .map_err(Error::WorkspaceCreationFailed)?;

    // The workspace directory may not exist yet, so canonicalize the parent
    // (the root) and reconstruct the expected canonical child path.
    let canonical_workspace = canonical_root.join(&workspace_key);

    if !canonical_workspace.starts_with(&canonical_root) {
        return Err(Error::InvalidWorkspacePath {
            path: workspace_path.to_string_lossy().into_owned(),
        });
    }

    // Create the workspace directory if needed.
    let created_now = if workspace_path.exists() {
        false
    } else {
        tokio::fs::create_dir_all(&workspace_path)
            .await
            .map_err(Error::WorkspaceCreationFailed)?;
        true
    };

    // Run the after-create hook when applicable.
    if created_now {
        if let Some(script) = &config.workspace_after_create {
            const HOOK_TIMEOUT_MS: u64 = 300_000; // 5 minutes
            run_hook(script, &workspace_path, HOOK_TIMEOUT_MS).await?;
        }
    }

    Ok(WorkspaceInfo {
        path: workspace_path,
        workspace_key,
        created_now,
    })
}

/// Execute a shell hook script inside `cwd`, waiting at most `timeout_ms`
/// milliseconds for it to complete.
///
/// Launches via `sh -lc <script>` so that the user's login shell environment
/// is available to the script.
pub async fn run_hook(script: &str, cwd: &std::path::Path, timeout_ms: u64) -> Result<()> {
    let child = Command::new("sh")
        .arg("-lc")
        .arg(script)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::WorkspaceCreationFailed)?;

    let result = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        child.wait_with_output(),
    )
    .await;

    match result {
        Err(_elapsed) => Err(Error::HookTimeout { timeout_ms }),
        Ok(Err(io_err)) => Err(Error::WorkspaceCreationFailed(io_err)),
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(())
            } else {
                Err(Error::HookFailed {
                    exit_code: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                })
            }
        }
    }
}

/// Remove a workspace directory for `identifier`.
///
/// If `config.workspace_before_remove` is set, the hook is run first
/// (best-effort: errors are logged but do not abort the removal).
///
/// Ignores `NotFound` errors from `remove_dir_all` so that calling this on an
/// already-absent workspace is a no-op.
pub async fn cleanup_workspace(identifier: &str, config: &ServiceConfig) -> Result<()> {
    let workspace_path = config.workspace_root.join(sanitize_key(identifier));

    // Run before-remove hook (best-effort).
    if let Some(script) = &config.workspace_before_remove {
        const HOOK_TIMEOUT_MS: u64 = 300_000;
        if let Err(err) = run_hook(script, &workspace_path, HOOK_TIMEOUT_MS).await {
            tracing::warn!(
                path = %workspace_path.display(),
                error = %err,
                "before_remove hook failed (ignored)"
            );
        }
    }

    match tokio::fs::remove_dir_all(&workspace_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::WorkspaceCreationFailed(e)),
    }

    Ok(())
}

/// Verify that `workspace_path` is still canonically inside `workspace_root`.
///
/// Called before launching an agent to guard against symlink-based escapes or
/// manual manipulation of the workspace directory.
pub fn verify_workspace_path(
    workspace_path: &std::path::Path,
    workspace_root: &std::path::Path,
) -> Result<()> {
    let canonical_root = workspace_root
        .canonicalize()
        .map_err(Error::WorkspaceCreationFailed)?;

    let canonical_workspace = workspace_path
        .canonicalize()
        .map_err(Error::WorkspaceCreationFailed)?;

    if canonical_workspace.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(Error::InvalidWorkspacePath {
            path: workspace_path.to_string_lossy().into_owned(),
        })
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

    // ---------------------------------------------------------------------- //
    // Helpers
    // ---------------------------------------------------------------------- //

    /// Build a minimal `ServiceConfig` whose `workspace_root` points at the
    /// given directory.
    fn make_config(workspace_root: PathBuf) -> ServiceConfig {
        ServiceConfig {
            tracker_kind: "linear".into(),
            tracker_api_key: "key".into(),
            tracker_project_slug: "proj".into(),
            tracker_endpoint: None,
            workspace_root,
            workspace_after_create: None,
            workspace_before_remove: None,
            agent_command: "claude".into(),
            agent_approval_policy: "auto".into(),
            agent_sandbox_policy: None,
            agent_max_turns: 20,
            agent_read_timeout_ms: 30_000,
            agent_turn_timeout_ms: 600_000,
            agent_stall_timeout_ms: 120_000,
            agent_max_retry_backoff_ms: 300_000,
            agent_continuation_guidance: "Continue.".into(),
            poll_interval_ms: 30_000,
            max_concurrent_agents: 3,
            max_concurrent_agents_by_state: HashMap::new(),
            active_states: vec![],
            terminal_states: vec![],
            active_states_original: vec![],
            terminal_states_original: vec![],
            server_enabled: false,
            server_port: 8080,
        }
    }

    // ---------------------------------------------------------------------- //
    // sanitize_key
    // ---------------------------------------------------------------------- //

    #[test]
    fn sanitize_key_allows_alphanumeric_dot_underscore_dash() {
        assert_eq!(sanitize_key("ABC-123"), "ABC-123");
        assert_eq!(sanitize_key("hello_world"), "hello_world");
        assert_eq!(sanitize_key("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn sanitize_key_replaces_slash() {
        assert_eq!(sanitize_key("ABC/123"), "ABC_123");
    }

    #[test]
    fn sanitize_key_replaces_space() {
        assert_eq!(sanitize_key("hello world"), "hello_world");
    }

    #[test]
    fn sanitize_key_replaces_special_characters() {
        assert_eq!(sanitize_key("foo@bar!baz"), "foo_bar_baz");
    }

    #[test]
    fn sanitize_key_empty_string() {
        assert_eq!(sanitize_key(""), "");
    }

    #[test]
    fn sanitize_key_all_special() {
        assert_eq!(sanitize_key("///"), "___");
    }

    // ---------------------------------------------------------------------- //
    // verify_workspace_path
    // ---------------------------------------------------------------------- //

    #[test]
    fn verify_workspace_path_valid() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("my-workspace");
        std::fs::create_dir_all(&child).unwrap();

        assert!(verify_workspace_path(&child, root.path()).is_ok());
    }

    #[test]
    fn verify_workspace_path_traversal_rejected() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();

        // `other` is a real path but outside `root`.
        let err = verify_workspace_path(other.path(), root.path()).unwrap_err();
        assert!(
            matches!(err, Error::InvalidWorkspacePath { .. }),
            "expected InvalidWorkspacePath, got: {err:?}"
        );
    }

    #[test]
    fn verify_workspace_path_nonexistent_workspace_returns_error() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist");

        // canonicalize fails for a non-existent path → should surface as an error.
        assert!(verify_workspace_path(&missing, root.path()).is_err());
    }

    // ---------------------------------------------------------------------- //
    // create_for_issue
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn create_for_issue_happy_path() {
        let root = tempfile::tempdir().unwrap();
        let config = make_config(root.path().to_path_buf());

        let info = create_for_issue("ENG-42", &config).await.unwrap();

        assert_eq!(info.workspace_key, "ENG-42");
        assert!(info.created_now);
        assert!(info.path.exists());
        assert!(info.path.is_dir());
    }

    #[tokio::test]
    async fn create_for_issue_existing_directory_not_recreated() {
        let root = tempfile::tempdir().unwrap();
        let config = make_config(root.path().to_path_buf());

        let first = create_for_issue("ENG-99", &config).await.unwrap();
        assert!(first.created_now);

        let second = create_for_issue("ENG-99", &config).await.unwrap();
        assert!(!second.created_now);
    }

    #[tokio::test]
    async fn create_for_issue_sanitizes_identifier() {
        let root = tempfile::tempdir().unwrap();
        let config = make_config(root.path().to_path_buf());

        let info = create_for_issue("my/issue id", &config).await.unwrap();
        assert_eq!(info.workspace_key, "my_issue_id");
        assert!(info.path.exists());
    }

    #[tokio::test]
    async fn create_for_issue_runs_after_create_hook() {
        let root = tempfile::tempdir().unwrap();
        let mut config = make_config(root.path().to_path_buf());
        // Write a sentinel file to prove the hook ran.
        config.workspace_after_create = Some("touch hook_ran.txt".into());

        let info = create_for_issue("HOOK-1", &config).await.unwrap();
        assert!(info.created_now);
        assert!(info.path.join("hook_ran.txt").exists());
    }

    // ---------------------------------------------------------------------- //
    // run_hook
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn run_hook_success() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_hook("echo hello", dir.path(), 5_000).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[tokio::test]
    async fn run_hook_failure_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_hook("exit 1", dir.path(), 5_000).await;

        match result {
            Err(Error::HookFailed { exit_code, .. }) => {
                assert_eq!(exit_code, Some(1));
            }
            other => panic!("expected HookFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_hook_captures_stderr_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_hook("echo 'oops' >&2; exit 2", dir.path(), 5_000).await;

        match result {
            Err(Error::HookFailed { stderr, exit_code }) => {
                assert_eq!(exit_code, Some(2));
                assert!(stderr.contains("oops"), "stderr: {stderr}");
            }
            other => panic!("expected HookFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_hook_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // 1 ms timeout against `sleep 10` should always trigger timeout.
        let result = run_hook("sleep 10", dir.path(), 1).await;

        match result {
            Err(Error::HookTimeout { timeout_ms }) => {
                assert_eq!(timeout_ms, 1);
            }
            other => panic!("expected HookTimeout, got: {other:?}"),
        }
    }
}
