//! Tracker abstraction and concrete implementations.
//!
//! The `Tracker` trait describes the minimal API surface required by the
//! Symphony orchestrator to interact with an issue tracker. Currently only
//! Linear is implemented; additional trackers can be added by implementing
//! this trait.

pub mod linear;

use crate::domain::Issue;
use crate::error::Result;

/// Abstraction over an issue-tracking backend.
///
/// All methods are async and return heap-allocated futures so that the trait
/// can be used as `dyn Tracker` without needing `async_trait`.
pub trait Tracker: Send + Sync {
    /// Fetch all open issues in `project_slugs` whose state is in
    /// `active_states`. Blocker information is included.
    fn fetch_candidate_issues<'a>(
        &'a self,
        active_states: &'a [String],
        project_slugs: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Issue>>> + Send + 'a>>;

    /// Fetch issues filtered by arbitrary state names. Returns an empty vec
    /// immediately if `states` is empty.
    fn fetch_issues_by_states<'a>(
        &'a self,
        states: &'a [String],
        project_slugs: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Issue>>> + Send + 'a>>;

    /// Fetch a minimal view (id / identifier / state) for the given issue IDs.
    /// Returns an empty vec immediately if `ids` is empty.
    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        ids: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Issue>>> + Send + 'a>>;

    /// Transition `issue_id` to the workflow state named `state_name`.
    fn set_issue_state<'a>(
        &'a self,
        issue_id: &'a str,
        state_name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}
