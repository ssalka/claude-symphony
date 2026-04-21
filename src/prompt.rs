//! Liquid template rendering for issue prompts.
//!
//! Converts an [`Issue`] (and an optional attempt counter) into a
//! `liquid::Object` globals map and renders a caller-supplied Liquid
//! template string against it.
//!
//! # Strict-mode note
//!
//! The `liquid` 0.26 crate does **not** expose a public API knob for
//! strict/fail-on-unknown-variable behaviour.  By default the engine
//! silently renders unknown variable references as the empty string.
//! We therefore cannot return `TemplateRenderError` for unknown
//! variables at this API level; templates that reference undefined
//! variables will simply produce an empty interpolation rather than an
//! error.  All *known* variables are populated below, so well-formed
//! templates always render correctly.  If strict mode becomes available
//! in a future version of the crate it should be enabled here.

use liquid::model::{KString, Value};

use crate::domain::{BlockerRef, Issue};
use crate::error::{Error, Result};

// -------------------------------------------------------------------------- //
// Helpers
// -------------------------------------------------------------------------- //

/// Convert an `Option<String>` into `Value::Scalar(s)` or `Value::Nil`.
fn opt_str_value(opt: Option<&str>) -> Value {
    match opt {
        Some(s) => Value::scalar(s.to_owned()),
        None => Value::Nil,
    }
}

/// Convert an `Option<i64>` into `Value::Scalar(i)` or `Value::Nil`.
fn opt_i64_value(opt: Option<i64>) -> Value {
    match opt {
        Some(n) => Value::scalar(n),
        None => Value::Nil,
    }
}

/// Build the liquid `Value::Object` for a single [`BlockerRef`].
fn blocker_to_value(b: &BlockerRef) -> Value {
    let mut obj = liquid::Object::new();
    obj.insert(KString::from_static("id"), opt_str_value(b.id.as_deref()));
    obj.insert(
        KString::from_static("identifier"),
        opt_str_value(b.identifier.as_deref()),
    );
    obj.insert(
        KString::from_static("state"),
        opt_str_value(b.state.as_deref()),
    );
    Value::Object(obj)
}

// -------------------------------------------------------------------------- //
// Public API
// -------------------------------------------------------------------------- //

/// Render `template` as a Liquid template with variables populated from
/// `issue` and the optional `attempt` counter.
///
/// # Errors
///
/// * [`Error::TemplateParseError`] – the template string is not valid Liquid.
/// * [`Error::TemplateRenderError`] – the template failed to render (e.g. a
///   filter was applied to an incompatible type).
pub fn render_prompt(
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
    plan: Option<&str>,
) -> Result<String> {
    // Build a parser with the full Liquid standard library.
    let parser =
        liquid::ParserBuilder::with_stdlib()
            .build()
            .map_err(|e| Error::TemplateParseError {
                message: e.to_string(),
            })?;

    // Compile the template.
    let tmpl = parser
        .parse(template)
        .map_err(|e| Error::TemplateParseError {
            message: e.to_string(),
        })?;

    // --------------------------------------------------------------------- //
    // Build the globals object.
    // --------------------------------------------------------------------- //
    let mut globals = liquid::Object::new();

    // --- scalar fields ---
    globals.insert(KString::from_static("id"), Value::scalar(issue.id.clone()));
    globals.insert(
        KString::from_static("identifier"),
        Value::scalar(issue.identifier.clone()),
    );
    globals.insert(
        KString::from_static("title"),
        Value::scalar(issue.title.clone()),
    );
    globals.insert(
        KString::from_static("description"),
        Value::scalar(issue.description.as_deref().unwrap_or("").to_owned()),
    );
    globals.insert(
        KString::from_static("priority"),
        opt_i64_value(issue.priority),
    );
    globals.insert(
        KString::from_static("state"),
        Value::scalar(issue.state.clone()),
    );
    globals.insert(
        KString::from_static("branch_name"),
        opt_str_value(issue.branch_name.as_deref()),
    );
    globals.insert(
        KString::from_static("url"),
        opt_str_value(issue.url.as_deref()),
    );

    // --- labels array ---
    let labels: Vec<Value> = issue
        .labels
        .iter()
        .map(|l| Value::scalar(l.clone()))
        .collect();
    globals.insert(KString::from_static("labels"), Value::Array(labels));

    // --- blocked_by array of objects ---
    let blocked_by: Vec<Value> = issue.blocked_by.iter().map(blocker_to_value).collect();
    globals.insert(KString::from_static("blocked_by"), Value::Array(blocked_by));

    // --- datetime fields ---
    let created_at = issue.created_at.as_ref().map(|dt| dt.to_rfc3339());
    globals.insert(
        KString::from_static("created_at"),
        opt_str_value(created_at.as_deref()),
    );

    let updated_at = issue.updated_at.as_ref().map(|dt| dt.to_rfc3339());
    globals.insert(
        KString::from_static("updated_at"),
        opt_str_value(updated_at.as_deref()),
    );

    // --- attempt ---
    globals.insert(
        KString::from_static("attempt"),
        match attempt {
            Some(n) => Value::scalar(n as i64),
            None => Value::Nil,
        },
    );

    // --- plan ---
    globals.insert(
        KString::from_static("plan"),
        match plan {
            Some(p) => Value::scalar(p.to_owned()),
            None => Value::scalar(String::new()),
        },
    );

    // Render.
    tmpl.render(&globals)
        .map_err(|e| Error::TemplateRenderError {
            message: e.to_string(),
        })
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockerRef, Issue};

    /// Construct a minimal `Issue` with all optional fields set to `None` /
    /// empty, to use as a base for individual test cases.
    fn base_issue() -> Issue {
        Issue {
            id: "issue-abc".to_string(),
            identifier: "ENG-1".to_string(),
            title: "Test issue".to_string(),
            description: None,
            priority: None,
            state: "Todo".to_string(),
            branch_name: None,
            url: None,
            labels: vec![],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        }
    }

    // ---- basic rendering -------------------------------------------------- //

    #[test]
    fn test_basic_scalar_fields() {
        let issue = Issue {
            id: "abc-123".to_string(),
            identifier: "ENG-99".to_string(),
            title: "Fix everything".to_string(),
            state: "In Progress".to_string(),
            ..base_issue()
        };

        let tmpl = "{{ identifier }}: {{ title }} [{{ state }}]";
        let out = render_prompt(tmpl, &issue, None, None).unwrap();
        assert_eq!(out, "ENG-99: Fix everything [In Progress]");
    }

    #[test]
    fn test_description_present() {
        let issue = Issue {
            description: Some("A detailed description.".to_string()),
            ..base_issue()
        };

        let out = render_prompt("{{ description }}", &issue, None, None).unwrap();
        assert_eq!(out, "A detailed description.");
    }

    // ---- Nil / optional fields -------------------------------------------- //

    #[test]
    fn test_description_none_renders_empty_string() {
        let issue = base_issue(); // description is None
        let out = render_prompt("desc:{{ description }}", &issue, None, None).unwrap();
        // description should be empty string when None
        assert_eq!(out, "desc:");
    }

    #[test]
    fn test_priority_none_is_nil() {
        let issue = base_issue(); // priority is None
                                  // Nil renders as empty string in Liquid
        let out = render_prompt("p:{{ priority }}", &issue, None, None).unwrap();
        assert_eq!(out, "p:");
    }

    #[test]
    fn test_priority_some_renders_integer() {
        let issue = Issue {
            priority: Some(2),
            ..base_issue()
        };
        let out = render_prompt("{{ priority }}", &issue, None, None).unwrap();
        assert_eq!(out, "2");
    }

    #[test]
    fn test_branch_name_none_is_nil() {
        let issue = base_issue();
        let out = render_prompt("branch:{{ branch_name }}", &issue, None, None).unwrap();
        assert_eq!(out, "branch:");
    }

    #[test]
    fn test_branch_name_some() {
        let issue = Issue {
            branch_name: Some("eng-1-fix".to_string()),
            ..base_issue()
        };
        let out = render_prompt("{{ branch_name }}", &issue, None, None).unwrap();
        assert_eq!(out, "eng-1-fix");
    }

    #[test]
    fn test_url_none_is_nil() {
        let issue = base_issue();
        let out = render_prompt("url:{{ url }}", &issue, None, None).unwrap();
        assert_eq!(out, "url:");
    }

    #[test]
    fn test_url_some() {
        let issue = Issue {
            url: Some("https://linear.app/issue/ENG-1".to_string()),
            ..base_issue()
        };
        let out = render_prompt("{{ url }}", &issue, None, None).unwrap();
        assert_eq!(out, "https://linear.app/issue/ENG-1");
    }

    // ---- labels array ----------------------------------------------------- //

    #[test]
    fn test_labels_empty() {
        let issue = base_issue();
        let out = render_prompt(
            "{% for l in labels %}{{l}},{% endfor %}",
            &issue,
            None,
            None,
        )
        .unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn test_labels_multiple() {
        let issue = Issue {
            labels: vec!["bug".to_string(), "p1".to_string(), "backend".to_string()],
            ..base_issue()
        };
        let out = render_prompt(
            "{% for l in labels %}{{ l }}{% unless forloop.last %},{% endunless %}{% endfor %}",
            &issue,
            None,
            None,
        )
        .unwrap();
        assert_eq!(out, "bug,p1,backend");
    }

    // ---- blocked_by array ------------------------------------------------- //

    #[test]
    fn test_blocked_by_empty() {
        let issue = base_issue();
        let out = render_prompt(
            "{% for b in blocked_by %}{{ b.identifier }}{% endfor %}",
            &issue,
            None,
            None,
        )
        .unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn test_blocked_by_single_full() {
        let issue = Issue {
            blocked_by: vec![BlockerRef {
                id: Some("blk-1".to_string()),
                identifier: Some("ENG-10".to_string()),
                state: Some("In Progress".to_string()),
            }],
            ..base_issue()
        };
        let out = render_prompt(
            "{% for b in blocked_by %}{{ b.id }}|{{ b.identifier }}|{{ b.state }}{% endfor %}",
            &issue,
            None,
            None,
        )
        .unwrap();
        assert_eq!(out, "blk-1|ENG-10|In Progress");
    }

    #[test]
    fn test_blocked_by_nil_fields() {
        let issue = Issue {
            blocked_by: vec![BlockerRef {
                id: None,
                identifier: None,
                state: None,
            }],
            ..base_issue()
        };
        let out = render_prompt(
            "{% for b in blocked_by %}[{{ b.id }}|{{ b.identifier }}|{{ b.state }}]{% endfor %}",
            &issue,
            None,
            None,
        )
        .unwrap();
        // All Nil fields render as empty strings in Liquid.
        assert_eq!(out, "[||]");
    }

    #[test]
    fn test_blocked_by_multiple() {
        let issue = Issue {
            blocked_by: vec![
                BlockerRef {
                    id: Some("b1".to_string()),
                    identifier: Some("ENG-5".to_string()),
                    state: Some("Done".to_string()),
                },
                BlockerRef {
                    id: Some("b2".to_string()),
                    identifier: Some("ENG-6".to_string()),
                    state: Some("Todo".to_string()),
                },
            ],
            ..base_issue()
        };
        let out = render_prompt(
            "{% for b in blocked_by %}{{ b.identifier }}{% unless forloop.last %},{% endunless %}{% endfor %}",
            &issue,
            None,
            None,
        )
        .unwrap();
        assert_eq!(out, "ENG-5,ENG-6");
    }

    // ---- attempt parameter ------------------------------------------------ //

    #[test]
    fn test_attempt_none_is_nil() {
        let issue = base_issue();
        let out = render_prompt("a:{{ attempt }}", &issue, None, None).unwrap();
        assert_eq!(out, "a:");
    }

    #[test]
    fn test_attempt_some() {
        let issue = base_issue();
        let out = render_prompt("attempt {{ attempt }}", &issue, Some(3), None).unwrap();
        assert_eq!(out, "attempt 3");
    }

    #[test]
    fn test_attempt_first() {
        let issue = base_issue();
        let out = render_prompt("{{ attempt }}", &issue, Some(1), None).unwrap();
        assert_eq!(out, "1");
    }

    // ---- datetime fields -------------------------------------------------- //

    #[test]
    fn test_created_at_none() {
        let issue = base_issue();
        let out = render_prompt("{{ created_at }}", &issue, None, None).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn test_created_at_some() {
        use chrono::{TimeZone, Utc};
        let issue = Issue {
            created_at: Some(Utc.with_ymd_and_hms(2024, 1, 15, 10, 30, 0).unwrap()),
            ..base_issue()
        };
        let out = render_prompt("{{ created_at }}", &issue, None, None).unwrap();
        // Should be an ISO-8601 string containing the date portion.
        assert!(out.contains("2024-01-15"), "got: {}", out);
    }

    // ---- error cases ------------------------------------------------------ //

    #[test]
    fn test_invalid_template_returns_parse_error() {
        let issue = base_issue();
        let err = render_prompt("{% if %}", &issue, None, None).unwrap_err();
        match err {
            Error::TemplateParseError { .. } => {}
            other => panic!("expected TemplateParseError, got {:?}", other),
        }
    }

    #[test]
    fn test_render_filter_error_returns_render_error() {
        // Applying an arithmetic filter to a non-numeric value should produce a
        // render error.
        let issue = base_issue(); // title = "Test issue" (string)
        let err = render_prompt("{{ title | divided_by: 0 }}", &issue, None, None).unwrap_err();
        // divided_by: 0 causes a render-time error in liquid
        match err {
            Error::TemplateRenderError { .. } | Error::TemplateParseError { .. } => {}
            other => panic!(
                "expected TemplateParseError or TemplateRenderError, got {:?}",
                other
            ),
        }
    }

    // ---- conditional logic in template ------------------------------------ //

    #[test]
    fn test_if_priority_present() {
        let issue_with = Issue {
            priority: Some(1),
            ..base_issue()
        };
        let issue_without = base_issue();
        let tmpl = "{% if priority %}P{{ priority }}{% else %}no-priority{% endif %}";

        let with_out = render_prompt(tmpl, &issue_with, None, None).unwrap();
        assert_eq!(with_out, "P1");

        let without_out = render_prompt(tmpl, &issue_without, None, None).unwrap();
        assert_eq!(without_out, "no-priority");
    }

    #[test]
    fn test_full_template() {
        use chrono::{TimeZone, Utc};

        let issue = Issue {
            id: "xyz-789".to_string(),
            identifier: "ENG-42".to_string(),
            title: "Implement feature X".to_string(),
            description: Some("Detailed description here.".to_string()),
            priority: Some(2),
            state: "In Progress".to_string(),
            branch_name: Some("eng-42-feature-x".to_string()),
            url: Some("https://linear.app/eng-42".to_string()),
            labels: vec!["feature".to_string(), "backend".to_string()],
            blocked_by: vec![BlockerRef {
                id: Some("b1".to_string()),
                identifier: Some("ENG-10".to_string()),
                state: Some("In Review".to_string()),
            }],
            created_at: Some(Utc.with_ymd_and_hms(2024, 3, 1, 9, 0, 0).unwrap()),
            updated_at: Some(Utc.with_ymd_and_hms(2024, 3, 5, 12, 0, 0).unwrap()),
        };

        let tmpl = r#"Issue {{ identifier }}: {{ title }}
State: {{ state }}
Branch: {{ branch_name }}
Priority: {{ priority }}
Labels: {% for l in labels %}{{ l }}{% unless forloop.last %}, {% endunless %}{% endfor %}
Blocked by: {% for b in blocked_by %}{{ b.identifier }}({{ b.state }}){% endfor %}
Attempt: {{ attempt }}"#;

        let out = render_prompt(tmpl, &issue, Some(2), None).unwrap();
        assert!(out.contains("Issue ENG-42: Implement feature X"));
        assert!(out.contains("State: In Progress"));
        assert!(out.contains("Branch: eng-42-feature-x"));
        assert!(out.contains("Priority: 2"));
        assert!(out.contains("Labels: feature, backend"));
        assert!(out.contains("Blocked by: ENG-10(In Review)"));
        assert!(out.contains("Attempt: 2"));
    }

    // ---- plan variable ------------------------------------------------------ //

    #[test]
    fn test_plan_none_renders_empty() {
        let issue = base_issue();
        let out = render_prompt("plan:{{ plan }}", &issue, None, None).unwrap();
        assert_eq!(out, "plan:");
    }

    #[test]
    fn test_plan_some_renders_text() {
        let issue = base_issue();
        let out = render_prompt("plan:{{ plan }}", &issue, None, Some("My plan")).unwrap();
        assert_eq!(out, "plan:My plan");
    }

    #[test]
    fn test_plan_conditional() {
        let issue = base_issue();
        let tmpl = "{% if plan != '' %}PLAN: {{ plan }}{% else %}no plan{% endif %}";
        let with = render_prompt(tmpl, &issue, None, Some("Do X")).unwrap();
        assert_eq!(with, "PLAN: Do X");
        let without = render_prompt(tmpl, &issue, None, None).unwrap();
        assert_eq!(without, "no plan");
    }
}
