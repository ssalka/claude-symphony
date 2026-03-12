use std::path::PathBuf;
use std::sync::Arc;

use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::watch;

use crate::domain::WorkflowDefinition;
use crate::error::{Error, Result};

// -------------------------------------------------------------------------- //
// parse_workflow_file
// -------------------------------------------------------------------------- //

/// Parse a workflow file's text content into a [`WorkflowDefinition`].
///
/// The format is:
/// ```text
/// ---
/// <YAML front matter>
/// ---
/// <Liquid prompt template>
/// ```
///
/// If fewer than two `---` delimiters are found, returns
/// [`Error::MissingWorkflowFile`] (indicating the file has no front matter).
/// If the front-matter YAML root node is not a mapping, returns
/// [`Error::WorkflowFrontMatterNotAMap`].
pub fn parse_workflow_file(content: &str) -> Result<WorkflowDefinition> {
    // Split on `---`. The convention is that the file *starts* with `---`, so:
    //   segment[0] = "" (empty string before the first ---)
    //   segment[1] = YAML front matter
    //   segment[2] = prompt body (may be absent if only one --- was found)
    let segments: Vec<&str> = content.splitn(3, "---").collect();

    if segments.len() < 2 {
        return Err(Error::MissingWorkflowFile {
            path: "(content)".to_string(),
        });
    }

    // The front matter is in segment[1] when the file starts with `---`.
    // If the file does NOT start with `---`, treat segment[0] as the pre-amble
    // and segment[1] as the front matter, which is the same indexing.
    let front_matter_raw = segments[1];

    if front_matter_raw.trim().is_empty() {
        return Err(Error::MissingWorkflowFile {
            path: "(content)".to_string(),
        });
    }

    // Parse the front matter as YAML.
    let config: serde_yaml::Value =
        serde_yaml::from_str(front_matter_raw).map_err(|e| Error::WorkflowParseError {
            message: e.to_string(),
        })?;

    // The root must be a YAML mapping.
    if !config.is_mapping() {
        return Err(Error::WorkflowFrontMatterNotAMap);
    }

    // Everything after the second `---` is the prompt template.
    let prompt_template = if segments.len() >= 3 {
        segments[2].trim().to_string()
    } else {
        String::new()
    };

    Ok(WorkflowDefinition {
        config,
        prompt_template,
    })
}

// -------------------------------------------------------------------------- //
// WorkflowLoader
// -------------------------------------------------------------------------- //

/// Loads a workflow file from disk and optionally watches it for changes.
pub struct WorkflowLoader {
    path: PathBuf,
}

impl WorkflowLoader {
    /// Create a new loader for the given path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read and parse the workflow file from disk.
    pub fn load(&self) -> Result<WorkflowDefinition> {
        let content =
            std::fs::read_to_string(&self.path).map_err(|_| Error::MissingWorkflowFile {
                path: self.path.display().to_string(),
            })?;
        parse_workflow_file(&content)
    }

    /// Start a file-system watcher on the workflow file.
    ///
    /// Returns a [`watch::Receiver`] that delivers a new [`Arc<WorkflowDefinition>`]
    /// each time the file changes successfully. If a reload fails, the error is
    /// logged and the last good definition is retained.
    pub fn start_watcher(&self) -> Result<watch::Receiver<Arc<WorkflowDefinition>>> {
        // Perform the initial load synchronously so callers get an immediate value.
        let initial = Arc::new(self.load()?);
        let (tx, rx) = watch::channel(initial);

        let path = self.path.clone();

        // Spawn a background thread for the watcher (notify uses blocking I/O).
        std::thread::spawn(move || {
            let (fs_tx, fs_rx) = std::sync::mpsc::channel();

            let mut watcher = match RecommendedWatcher::new(fs_tx, NotifyConfig::default()) {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!("failed to create file watcher: {e}");
                    return;
                }
            };

            if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                tracing::error!("failed to watch workflow file {}: {e}", path.display());
                return;
            }

            // Keep the watcher alive for the duration of this thread.
            for event in fs_rx {
                match event {
                    Ok(_) => {
                        // Reload the file.
                        match std::fs::read_to_string(&path)
                            .map_err(|_| Error::MissingWorkflowFile {
                                path: path.display().to_string(),
                            })
                            .and_then(|content| parse_workflow_file(&content))
                        {
                            Ok(def) => {
                                // send() only fails if all receivers are dropped.
                                let _ = tx.send(Arc::new(def));
                            }
                            Err(e) => {
                                tracing::error!(
                                    "workflow reload failed for {}: {e}",
                                    path.display()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("file watcher error: {e}");
                    }
                }
            }
        });

        Ok(rx)
    }
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers ------------------------------------------------------------

    fn valid_content() -> &'static str {
        r#"---
tracker:
  kind: linear
  api_key: my-key
  project_slugs: [my-project]
workspace:
  root: /tmp
agent:
  command: claude
---
This is the prompt for {{ issue.title }}.
"#
    }

    // ---- parse_workflow_file ------------------------------------------------

    #[test]
    fn test_parse_valid_file() {
        let def = parse_workflow_file(valid_content()).unwrap();
        assert!(def.config.is_mapping());
        assert!(def.prompt_template.contains("prompt for"));
    }

    #[test]
    fn test_prompt_template_trimmed() {
        let def = parse_workflow_file(valid_content()).unwrap();
        assert!(!def.prompt_template.starts_with('\n'));
        assert!(!def.prompt_template.ends_with('\n'));
    }

    #[test]
    fn test_missing_front_matter_delimiter_returns_error() {
        // No `---` at all — treat as missing front matter.
        let content = "tracker:\n  kind: linear\n";
        let err = parse_workflow_file(content).unwrap_err();
        assert!(matches!(err, Error::MissingWorkflowFile { .. }));
    }

    #[test]
    fn test_empty_front_matter_returns_error() {
        let content = "---\n---\nPrompt here.";
        let err = parse_workflow_file(content).unwrap_err();
        assert!(matches!(err, Error::MissingWorkflowFile { .. }));
    }

    #[test]
    fn test_invalid_yaml_returns_parse_error() {
        let content = "---\n{bad yaml: [unclosed\n---\nPrompt";
        let err = parse_workflow_file(content).unwrap_err();
        assert!(matches!(err, Error::WorkflowParseError { .. }));
    }

    #[test]
    fn test_front_matter_not_a_map_returns_error() {
        // Front matter is a YAML sequence, not a map.
        let content = "---\n- item1\n- item2\n---\nPrompt here.";
        let err = parse_workflow_file(content).unwrap_err();
        assert!(matches!(err, Error::WorkflowFrontMatterNotAMap));
    }

    #[test]
    fn test_no_prompt_body_is_empty_string() {
        // Only one `---` separator: no prompt section.
        // With splitn(3, "---") on "---\ntracker:\n  kind: linear\n",
        // we get ["", "\ntracker:\n  kind: linear\n"] — only 2 segments,
        // so prompt_template will be empty.
        let content = "---\ntracker:\n  kind: linear\n  api_key: k\n  project_slugs: [p]\nworkspace:\n  root: /tmp\nagent:\n  command: claude\n";
        let def = parse_workflow_file(content).unwrap();
        assert_eq!(def.prompt_template, "");
    }

    #[test]
    fn test_config_fields_accessible() {
        let def = parse_workflow_file(valid_content()).unwrap();
        let tracker = def.config.get("tracker").unwrap();
        assert_eq!(tracker.get("kind").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(
            tracker.get("api_key").and_then(|v| v.as_str()),
            Some("my-key")
        );
    }

    #[test]
    fn test_multi_section_prompt_body() {
        // The prompt body itself contains `---` — should all be in prompt_template.
        let content = "---\ntracker:\n  kind: linear\n  api_key: k\n  project_slugs: [p]\nworkspace:\n  root: /tmp\nagent:\n  command: claude\n---\nLine one\n---\nLine two\n";
        let def = parse_workflow_file(content).unwrap();
        // The third segment (index 2) is "Line one\n---\nLine two\n".
        assert!(def.prompt_template.contains("Line one"));
        // splitn(3) means the third segment contains the rest including any
        // additional `---`.
        assert!(def.prompt_template.contains("Line two"));
    }

    // ---- WorkflowLoader -----------------------------------------------------

    #[test]
    fn test_loader_load_missing_file() {
        let loader = WorkflowLoader::new(PathBuf::from("/nonexistent/file.yml"));
        let err = loader.load().unwrap_err();
        assert!(matches!(err, Error::MissingWorkflowFile { .. }));
    }

    #[test]
    fn test_loader_load_valid_file() {
        use std::io::Write as _;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "{}", valid_content()).unwrap();
        let loader = WorkflowLoader::new(tmp.path().to_path_buf());
        let def = loader.load().unwrap();
        assert!(def.config.is_mapping());
    }
}
