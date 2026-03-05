use thiserror::Error;

/// Top-level error enum for the Symphony daemon.
///
/// All variants are flat (no wrapper enum) so callers use `?` directly with
/// a single `Error` type throughout the codebase.
#[derive(Debug, Error)]
pub enum Error {
    // ------------------------------------------------------------------ //
    // Workflow errors
    // ------------------------------------------------------------------ //
    /// The workflow file path does not exist on disk.
    #[error("workflow file not found: {path}")]
    MissingWorkflowFile { path: String },

    /// YAML front-matter could not be parsed.
    #[error("workflow front matter parse error: {message}")]
    WorkflowParseError { message: String },

    /// Front matter was parsed successfully but the root node is not a YAML
    /// mapping.
    #[error("workflow front matter root is not a YAML map")]
    WorkflowFrontMatterNotAMap,

    // ------------------------------------------------------------------ //
    // Template errors
    // ------------------------------------------------------------------ //
    /// A Liquid template could not be parsed.
    #[error("template parse error: {message}")]
    TemplateParseError { message: String },

    /// A Liquid template failed to render.
    #[error("template render error: {message}")]
    TemplateRenderError { message: String },

    // ------------------------------------------------------------------ //
    // Tracker config errors
    // ------------------------------------------------------------------ //
    /// `tracker.kind` is not a supported value (currently only "linear").
    #[error("unsupported tracker kind: {kind}")]
    UnsupportedTrackerKind { kind: String },

    /// `tracker.api_key` is missing or empty in the workflow config.
    #[error("tracker api_key is missing or empty")]
    MissingTrackerApiKey,

    /// `tracker.project_slug` is missing or empty in the workflow config.
    #[error("tracker project_slug is missing or empty")]
    MissingTrackerProjectSlug,

    // ------------------------------------------------------------------ //
    // Linear API errors
    // ------------------------------------------------------------------ //
    /// A `reqwest` transport-level error occurred while making a request to
    /// the Linear API.
    #[error("linear API request error: {0}")]
    LinearApiRequest(#[from] reqwest::Error),

    /// The Linear API returned a non-200 HTTP status.
    #[error("linear API returned status {status}: {body}")]
    LinearApiStatus { status: u16, body: String },

    /// The Linear GraphQL response contained a top-level `errors` array.
    #[error("linear GraphQL errors: {errors}")]
    LinearGraphqlErrors { errors: String },

    /// The Linear API response shape was not recognized.
    #[error("linear API unknown payload: {description}")]
    LinearUnknownPayload { description: String },

    /// A pagination cursor (`endCursor`) was expected in the Linear response
    /// but was absent.
    #[error("linear API response missing end cursor for pagination")]
    LinearMissingEndCursor,

    // ------------------------------------------------------------------ //
    // Workspace errors
    // ------------------------------------------------------------------ //
    /// A path would escape the workspace root (path traversal attempt).
    #[error("invalid workspace path (outside root): {path}")]
    InvalidWorkspacePath { path: String },

    /// `fs::create_dir_all` failed while setting up a workspace directory.
    #[error("workspace creation failed: {0}")]
    WorkspaceCreationFailed(#[source] std::io::Error),

    /// A hook script exited with a non-zero exit code.
    #[error(
        "hook script failed with exit code {}: {stderr}",
        exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    )]
    HookFailed {
        exit_code: Option<i32>,
        stderr: String,
    },

    /// A hook script exceeded its allowed wall-clock time.
    #[error("hook script timed out after {timeout_ms}ms")]
    HookTimeout { timeout_ms: u64 },

    // ------------------------------------------------------------------ //
    // Config validation errors
    // ------------------------------------------------------------------ //
    /// A configuration value failed validation.
    #[error("config validation error: {message}")]
    ConfigValidation { message: String },

    // ------------------------------------------------------------------ //
    // Agent errors
    // ------------------------------------------------------------------ //
    /// The `claude` executable was not found on `PATH` (or the configured
    /// path).
    #[error("claude executable not found: {command}")]
    ClaudeNotFound { command: String },

    /// No response was received from the agent within `read_timeout_ms`.
    #[error("agent response timed out (read_timeout_ms exceeded)")]
    ResponseTimeout,

    /// The agent turn did not complete within `turn_timeout_ms`.
    #[error("agent turn timed out (turn_timeout_ms exceeded)")]
    TurnTimeout,

    /// The agent returned a `turn/failed` event.
    #[error("agent turn failed: {message}")]
    TurnFailed { message: String },

    /// The agent returned a `turn/cancelled` event.
    #[error("agent turn was cancelled")]
    TurnCancelled,

    /// The agent requested interactive user input, which is not supported in
    /// daemon mode.
    #[error("agent requested user input (turn/input_required); hard fail")]
    TurnInputRequired,
}

/// Convenience `Result` alias used throughout the Symphony codebase.
pub type Result<T> = std::result::Result<T, Error>;

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers ---------------------------------------------------------- //

    fn assert_display(err: &Error, expected: &str) {
        assert_eq!(err.to_string(), expected);
    }

    // ---- workflow errors -------------------------------------------------- //

    #[test]
    fn test_missing_workflow_file() {
        let err = Error::MissingWorkflowFile {
            path: "/tmp/missing.yml".to_string(),
        };
        assert_display(&err, "workflow file not found: /tmp/missing.yml");
    }

    #[test]
    fn test_workflow_parse_error() {
        let err = Error::WorkflowParseError {
            message: "unexpected token at line 3".to_string(),
        };
        assert_display(
            &err,
            "workflow front matter parse error: unexpected token at line 3",
        );
    }

    #[test]
    fn test_workflow_front_matter_not_a_map() {
        let err = Error::WorkflowFrontMatterNotAMap;
        assert_display(&err, "workflow front matter root is not a YAML map");
    }

    // ---- template errors -------------------------------------------------- //

    #[test]
    fn test_template_parse_error() {
        let err = Error::TemplateParseError {
            message: "unclosed tag".to_string(),
        };
        assert_display(&err, "template parse error: unclosed tag");
    }

    #[test]
    fn test_template_render_error() {
        let err = Error::TemplateRenderError {
            message: "undefined variable `foo`".to_string(),
        };
        assert_display(&err, "template render error: undefined variable `foo`");
    }

    // ---- tracker config errors -------------------------------------------- //

    #[test]
    fn test_unsupported_tracker_kind() {
        let err = Error::UnsupportedTrackerKind {
            kind: "jira".to_string(),
        };
        assert_display(&err, "unsupported tracker kind: jira");
    }

    #[test]
    fn test_missing_tracker_api_key() {
        let err = Error::MissingTrackerApiKey;
        assert_display(&err, "tracker api_key is missing or empty");
    }

    #[test]
    fn test_missing_tracker_project_slug() {
        let err = Error::MissingTrackerProjectSlug;
        assert_display(&err, "tracker project_slug is missing or empty");
    }

    // ---- Linear API errors ------------------------------------------------ //

    #[test]
    fn test_linear_api_status() {
        let err = Error::LinearApiStatus {
            status: 401,
            body: "Unauthorized".to_string(),
        };
        assert_display(&err, "linear API returned status 401: Unauthorized");
    }

    #[test]
    fn test_linear_graphql_errors() {
        let err = Error::LinearGraphqlErrors {
            errors: r#"[{"message":"not found"}]"#.to_string(),
        };
        assert_display(&err, r#"linear GraphQL errors: [{"message":"not found"}]"#);
    }

    #[test]
    fn test_linear_unknown_payload() {
        let err = Error::LinearUnknownPayload {
            description: "missing data.issues field".to_string(),
        };
        assert_display(
            &err,
            "linear API unknown payload: missing data.issues field",
        );
    }

    #[test]
    fn test_linear_missing_end_cursor() {
        let err = Error::LinearMissingEndCursor;
        assert_display(
            &err,
            "linear API response missing end cursor for pagination",
        );
    }

    // ---- workspace errors ------------------------------------------------- //

    #[test]
    fn test_invalid_workspace_path() {
        let err = Error::InvalidWorkspacePath {
            path: "../escape/root".to_string(),
        };
        assert_display(
            &err,
            "invalid workspace path (outside root): ../escape/root",
        );
    }

    #[test]
    fn test_workspace_creation_failed() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let err = Error::WorkspaceCreationFailed(io_err);
        assert!(err.to_string().contains("workspace creation failed"));
        // Verify source is accessible via std::error::Error trait.
        use std::error::Error as StdError;
        assert!(err.source().is_some());
    }

    #[test]
    fn test_hook_failed_with_code() {
        let err = Error::HookFailed {
            exit_code: Some(1),
            stderr: "something went wrong".to_string(),
        };
        assert_display(
            &err,
            "hook script failed with exit code 1: something went wrong",
        );
    }

    #[test]
    fn test_hook_failed_without_code() {
        let err = Error::HookFailed {
            exit_code: None,
            stderr: "killed by signal".to_string(),
        };
        assert_display(
            &err,
            "hook script failed with exit code unknown: killed by signal",
        );
    }

    #[test]
    fn test_hook_timeout() {
        let err = Error::HookTimeout { timeout_ms: 30_000 };
        assert_display(&err, "hook script timed out after 30000ms");
    }

    // ---- config validation errors ----------------------------------------- //

    #[test]
    fn test_config_validation() {
        let err = Error::ConfigValidation {
            message: "active_states is empty".to_string(),
        };
        assert_display(&err, "config validation error: active_states is empty");
    }

    // ---- agent errors ----------------------------------------------------- //

    #[test]
    fn test_claude_not_found() {
        let err = Error::ClaudeNotFound {
            command: "claude".to_string(),
        };
        assert_display(&err, "claude executable not found: claude");
    }

    #[test]
    fn test_response_timeout() {
        let err = Error::ResponseTimeout;
        assert_display(&err, "agent response timed out (read_timeout_ms exceeded)");
    }

    #[test]
    fn test_turn_timeout() {
        let err = Error::TurnTimeout;
        assert_display(&err, "agent turn timed out (turn_timeout_ms exceeded)");
    }

    #[test]
    fn test_turn_failed() {
        let err = Error::TurnFailed {
            message: "internal error".to_string(),
        };
        assert_display(&err, "agent turn failed: internal error");
    }

    #[test]
    fn test_turn_cancelled() {
        let err = Error::TurnCancelled;
        assert_display(&err, "agent turn was cancelled");
    }

    #[test]
    fn test_turn_input_required() {
        let err = Error::TurnInputRequired;
        assert_display(
            &err,
            "agent requested user input (turn/input_required); hard fail",
        );
    }

    // ---- Result alias ----------------------------------------------------- //

    #[test]
    fn test_result_alias_ok() {
        let r: Result<i32> = Ok(42);
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn test_result_alias_err() {
        let r: Result<i32> = Err(Error::ResponseTimeout);
        assert!(r.is_err());
    }
}
