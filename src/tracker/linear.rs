//! Linear GraphQL client implementing the [`Tracker`] trait.
//!
//! The client uses the Linear GraphQL API
//! (`https://api.linear.app/graphql`) and handles pagination, error mapping,
//! and response normalization automatically.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::domain::{BlockerRef, Issue};
use crate::error::{Error, Result};
use crate::tracker::Tracker;

// -------------------------------------------------------------------------- //
// Constants
// -------------------------------------------------------------------------- //

const DEFAULT_ENDPOINT: &str = "https://api.linear.app/graphql";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Page size embedded in the `FetchCandidates` GraphQL query (`first: 50`).
#[allow(dead_code)]
const PAGE_SIZE: u32 = 50;
/// Page size embedded in the `FetchIssueStates` GraphQL query (`first: 250`).
#[allow(dead_code)]
const STATES_PAGE_SIZE: u32 = 250;

// -------------------------------------------------------------------------- //
// GraphQL queries
// -------------------------------------------------------------------------- //

const FETCH_CANDIDATES_QUERY: &str = r#"
query FetchCandidates($projectSlug: String!, $states: [String!]!, $after: String) {
  issues(
    filter: {
      project: { slugId: { eq: $projectSlug } }
      state: { name: { in: $states } }
    }
    first: 50
    after: $after
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { name }
      branchName
      url
      labels { nodes { name } }
      relations {
        nodes {
          type
          relatedIssue {
            id
            identifier
            state { name }
          }
        }
      }
      createdAt
      updatedAt
    }
    pageInfo {
      hasNextPage
      endCursor
    }
  }
}
"#;

const FETCH_STATE_ID_QUERY: &str = r#"
query FetchStateIdForIssue($issueId: String!, $stateName: String!) {
  issue(id: $issueId) {
    team {
      states(filter: { name: { eq: $stateName } }) {
        nodes {
          id
          name
        }
      }
    }
  }
}
"#;

const UPDATE_ISSUE_STATE_MUTATION: &str = r#"
mutation UpdateIssueState($issueId: String!, $stateId: String!) {
  issueUpdate(id: $issueId, input: { stateId: $stateId }) {
    success
  }
}
"#;

const FETCH_ISSUE_STATES_QUERY: &str = r#"
query FetchIssueStates($ids: [ID!]!) {
  issues(filter: { id: { in: $ids } }, first: 250) {
    nodes {
      id
      identifier
      state { name }
    }
    pageInfo {
      hasNextPage
      endCursor
    }
  }
}
"#;

// -------------------------------------------------------------------------- //
// Raw response deserialization types
// -------------------------------------------------------------------------- //

#[derive(Debug, serde::Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RawState {
    name: String,
}

#[derive(Debug, serde::Deserialize)]
struct RawLabelNode {
    name: String,
}

#[derive(Debug, serde::Deserialize)]
struct RawLabels {
    nodes: Vec<RawLabelNode>,
}

#[derive(Debug, serde::Deserialize)]
struct RawRelatedIssue {
    id: Option<String>,
    identifier: Option<String>,
    state: Option<RawState>,
}

#[derive(Debug, serde::Deserialize)]
struct RawRelationNode {
    #[serde(rename = "type")]
    relation_type: Option<String>,
    #[serde(rename = "relatedIssue")]
    related_issue: Option<RawRelatedIssue>,
}

#[derive(Debug, serde::Deserialize)]
struct RawRelations {
    nodes: Vec<RawRelationNode>,
}

// ---- set_issue_state response types ----------------------------------------

#[derive(Debug, serde::Deserialize)]
struct WorkflowStateNode {
    id: String,
}

#[derive(Debug, serde::Deserialize)]
struct WorkflowStateConnection {
    nodes: Vec<WorkflowStateNode>,
}

#[derive(Debug, serde::Deserialize)]
struct TeamWithStates {
    states: WorkflowStateConnection,
}

#[derive(Debug, serde::Deserialize)]
struct IssueWithTeam {
    team: TeamWithStates,
}

#[derive(Debug, serde::Deserialize)]
struct FetchStateIdData {
    issue: IssueWithTeam,
}

#[derive(Debug, serde::Deserialize)]
struct IssueUpdateResult {
    success: bool,
}

#[derive(Debug, serde::Deserialize)]
struct UpdateIssueStateData {
    #[serde(rename = "issueUpdate")]
    issue_update: IssueUpdateResult,
}

/// Full issue node as returned by the candidate/states queries.
#[derive(Debug, serde::Deserialize)]
struct RawIssueNode {
    id: String,
    identifier: String,
    title: Option<String>,
    description: Option<String>,
    priority: Option<i64>,
    state: Option<RawState>,
    #[serde(rename = "branchName")]
    branch_name: Option<String>,
    url: Option<String>,
    labels: Option<RawLabels>,
    relations: Option<RawRelations>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
}

/// Minimal issue node used when only state is required.
#[derive(Debug, serde::Deserialize)]
struct RawStateNode {
    id: String,
    identifier: String,
    state: Option<RawState>,
}

#[derive(Debug, serde::Deserialize)]
struct IssueConnection<N> {
    nodes: Vec<N>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Debug, serde::Deserialize)]
struct IssuesData<N> {
    issues: IssueConnection<N>,
}

// -------------------------------------------------------------------------- //
// LinearClient
// -------------------------------------------------------------------------- //

/// HTTP client for the Linear GraphQL API.
pub struct LinearClient {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
}

impl LinearClient {
    /// Create a new `LinearClient`.
    ///
    /// `endpoint` defaults to `https://api.linear.app/graphql` when `None`.
    pub fn new(api_key: String, endpoint: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");

        LinearClient {
            client,
            api_key,
            endpoint: endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        }
    }

    // ---------------------------------------------------------------------- //
    // Low-level GraphQL helper
    // ---------------------------------------------------------------------- //

    /// POST a GraphQL query to the Linear endpoint and deserialize the `data`
    /// field into `T`.
    ///
    /// Error mapping:
    /// - Transport errors → `LinearApiRequest`
    /// - Non-2xx status  → `LinearApiStatus`
    /// - `errors` array  → `LinearGraphqlErrors`
    /// - Missing `data`  → `LinearUnknownPayload`
    async fn graphql<T>(&self, query: &str, variables: Value) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let response = self
            .client
            .post(&self.endpoint)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(Error::LinearApiRequest)?;

        let status = response.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let body_text = response.text().await.unwrap_or_default();
            return Err(Error::LinearApiStatus {
                status: status_code,
                body: body_text,
            });
        }

        let mut json: Value = response.json().await.map_err(Error::LinearApiRequest)?;

        // Check for GraphQL-level errors.
        if let Some(errors) = json.get("errors") {
            return Err(Error::LinearGraphqlErrors {
                errors: errors.to_string(),
            });
        }

        // Extract the `data` field.
        let data = json
            .get_mut("data")
            .ok_or_else(|| Error::LinearUnknownPayload {
                description: "response missing 'data' field".to_string(),
            })?
            .take();

        serde_json::from_value(data).map_err(|e| Error::LinearUnknownPayload {
            description: format!("failed to deserialize response data: {e}"),
        })
    }

    // ---------------------------------------------------------------------- //
    // Issue normalization helpers
    // ---------------------------------------------------------------------- //

    /// Convert a `RawIssueNode` into a domain `Issue`.
    fn normalize_full(node: RawIssueNode) -> Issue {
        let labels = node
            .labels
            .map(|l| l.nodes.into_iter().map(|n| n.name.to_lowercase()).collect())
            .unwrap_or_default();

        let blocked_by = node
            .relations
            .map(|r| {
                r.nodes
                    .into_iter()
                    .filter(|rn| rn.relation_type.as_deref() == Some("blocked_by"))
                    .filter_map(|rn| rn.related_issue)
                    .map(|ri| BlockerRef {
                        id: ri.id,
                        identifier: ri.identifier,
                        state: ri.state.map(|s| s.name),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let created_at = node
            .created_at
            .as_deref()
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());

        let updated_at = node
            .updated_at
            .as_deref()
            .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());

        Issue {
            id: node.id,
            identifier: node.identifier,
            title: node.title.unwrap_or_default(),
            description: node.description,
            priority: node.priority,
            state: node.state.map(|s| s.name).unwrap_or_default(),
            branch_name: node.branch_name,
            url: node.url,
            labels,
            blocked_by,
            created_at,
            updated_at,
        }
    }

    /// Convert a `RawStateNode` into a minimal domain `Issue` (most fields
    /// left empty/None).
    fn normalize_state_only(node: RawStateNode) -> Issue {
        Issue {
            id: node.id,
            identifier: node.identifier,
            title: String::new(),
            description: None,
            priority: None,
            state: node.state.map(|s| s.name).unwrap_or_default(),
            branch_name: None,
            url: None,
            labels: vec![],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        }
    }

    // ---------------------------------------------------------------------- //
    // Paginated fetch helpers
    // ---------------------------------------------------------------------- //

    /// Fetch all pages of a candidates/states query.
    async fn fetch_all_pages(
        &self,
        query: &str,
        base_variables: Value,
    ) -> Result<Vec<RawIssueNode>> {
        let mut all_nodes: Vec<RawIssueNode> = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let mut vars = base_variables.clone();
            if let Some(cursor) = &after {
                vars["after"] = Value::String(cursor.clone());
            } else {
                vars["after"] = Value::Null;
            }

            let data: IssuesData<RawIssueNode> = self.graphql(query, vars).await?;
            let page_info = data.issues.page_info;
            all_nodes.extend(data.issues.nodes);

            if !page_info.has_next_page {
                break;
            }
            after = Some(page_info.end_cursor.ok_or(Error::LinearMissingEndCursor)?);
        }

        Ok(all_nodes)
    }

    /// Fetch a single page of the state-only query (no pagination needed
    /// because `first: 250` covers typical batch sizes; if Linear reports
    /// `hasNextPage` we return an error rather than silently truncating).
    async fn fetch_state_page(&self, ids: &[String]) -> Result<Vec<RawStateNode>> {
        let vars = serde_json::json!({ "ids": ids });
        let data: IssuesData<RawStateNode> = self.graphql(FETCH_ISSUE_STATES_QUERY, vars).await?;

        // If Linear reports a second page beyond our 250-item request, surface
        // it as a missing-cursor error rather than silently truncating.
        if data.issues.page_info.has_next_page && data.issues.page_info.end_cursor.is_none() {
            return Err(Error::LinearMissingEndCursor);
        }

        Ok(data.issues.nodes)
    }

    // ---------------------------------------------------------------------- //
    // Public fetch methods (concrete implementations)
    // ---------------------------------------------------------------------- //

    /// Fetch all candidate issues in `project_slug` with states in
    /// `active_states`.
    pub async fn fetch_candidate_issues_impl(
        &self,
        active_states: &[String],
        project_slug: &str,
    ) -> Result<Vec<Issue>> {
        if active_states.is_empty() {
            return Ok(vec![]);
        }

        let vars = serde_json::json!({
            "projectSlug": project_slug,
            "states": active_states,
        });

        let nodes = self.fetch_all_pages(FETCH_CANDIDATES_QUERY, vars).await?;

        Ok(nodes.into_iter().map(Self::normalize_full).collect())
    }

    /// Fetch issues filtered by `states` in `project_slug`.
    pub async fn fetch_issues_by_states_impl(
        &self,
        states: &[String],
        project_slug: &str,
    ) -> Result<Vec<Issue>> {
        if states.is_empty() {
            return Ok(vec![]);
        }

        let vars = serde_json::json!({
            "projectSlug": project_slug,
            "states": states,
        });

        let nodes = self.fetch_all_pages(FETCH_CANDIDATES_QUERY, vars).await?;

        Ok(nodes.into_iter().map(Self::normalize_full).collect())
    }

    /// Transition `issue_id` to the workflow state named `state_name`.
    ///
    /// 1. Resolves the state name to an ID by querying the issue's team states.
    /// 2. Calls `issueUpdate` with the resolved state ID.
    pub async fn set_issue_state_impl(&self, issue_id: &str, state_name: &str) -> Result<()> {
        // Step 1: resolve state name → state ID within the issue's team.
        let vars = serde_json::json!({
            "issueId": issue_id,
            "stateName": state_name,
        });
        let data: FetchStateIdData = self.graphql(FETCH_STATE_ID_QUERY, vars).await?;
        let state_id = data
            .issue
            .team
            .states
            .nodes
            .into_iter()
            .next()
            .map(|n| n.id)
            .ok_or_else(|| Error::LinearUnknownPayload {
                description: format!(
                    "state '{}' not found in team workflow states",
                    state_name
                ),
            })?;

        // Step 2: update the issue.
        let vars = serde_json::json!({
            "issueId": issue_id,
            "stateId": state_id,
        });
        let data: UpdateIssueStateData = self.graphql(UPDATE_ISSUE_STATE_MUTATION, vars).await?;
        if !data.issue_update.success {
            return Err(Error::LinearUnknownPayload {
                description: "issueUpdate returned success=false".to_string(),
            });
        }

        Ok(())
    }

    /// Fetch minimal issue views (id / identifier / state) by ID.
    pub async fn fetch_issue_states_by_ids_impl(&self, ids: &[String]) -> Result<Vec<Issue>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let nodes = self.fetch_state_page(ids).await?;
        Ok(nodes.into_iter().map(Self::normalize_state_only).collect())
    }
}

// -------------------------------------------------------------------------- //
// Tracker trait implementation
// -------------------------------------------------------------------------- //

impl Tracker for LinearClient {
    fn fetch_candidate_issues<'a>(
        &'a self,
        active_states: &'a [String],
        project_slug: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Issue>>> + Send + 'a>> {
        Box::pin(self.fetch_candidate_issues_impl(active_states, project_slug))
    }

    fn fetch_issues_by_states<'a>(
        &'a self,
        states: &'a [String],
        project_slug: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Issue>>> + Send + 'a>> {
        Box::pin(self.fetch_issues_by_states_impl(states, project_slug))
    }

    fn fetch_issue_states_by_ids<'a>(
        &'a self,
        ids: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Issue>>> + Send + 'a>> {
        Box::pin(self.fetch_issue_states_by_ids_impl(ids))
    }

    fn set_issue_state<'a>(
        &'a self,
        issue_id: &'a str,
        state_name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.set_issue_state_impl(issue_id, state_name))
    }
}

// -------------------------------------------------------------------------- //
// Unit tests
// -------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------- //
    // normalize_full — label lowercasing
    // ---------------------------------------------------------------------- //

    fn make_raw_node(labels: Vec<&str>, relations: Vec<(&str, &str, &str)>) -> RawIssueNode {
        RawIssueNode {
            id: "id-1".to_string(),
            identifier: "ENG-1".to_string(),
            title: Some("Test issue".to_string()),
            description: None,
            priority: Some(1),
            state: Some(RawState {
                name: "In Progress".to_string(),
            }),
            branch_name: Some("eng-1-test".to_string()),
            url: Some("https://linear.app/eng-1".to_string()),
            labels: Some(RawLabels {
                nodes: labels
                    .into_iter()
                    .map(|n| RawLabelNode {
                        name: n.to_string(),
                    })
                    .collect(),
            }),
            relations: Some(RawRelations {
                nodes: relations
                    .into_iter()
                    .map(|(id, ident, state)| RawRelationNode {
                        relation_type: Some("blocked_by".to_string()),
                        related_issue: Some(RawRelatedIssue {
                            id: Some(id.to_string()),
                            identifier: Some(ident.to_string()),
                            state: Some(RawState {
                                name: state.to_string(),
                            }),
                        }),
                    })
                    .collect(),
            }),
            created_at: Some("2024-01-15T10:00:00.000Z".to_string()),
            updated_at: Some("2024-01-16T12:30:00.000Z".to_string()),
        }
    }

    #[test]
    fn test_normalize_full_labels_lowercased() {
        let node = make_raw_node(vec!["Bug", "P1", "NEEDS-REVIEW"], vec![]);
        let issue = LinearClient::normalize_full(node);
        assert_eq!(issue.labels, vec!["bug", "p1", "needs-review"]);
    }

    #[test]
    fn test_normalize_full_labels_already_lowercase() {
        let node = make_raw_node(vec!["bug", "enhancement"], vec![]);
        let issue = LinearClient::normalize_full(node);
        assert_eq!(issue.labels, vec!["bug", "enhancement"]);
    }

    #[test]
    fn test_normalize_full_no_labels() {
        let mut node = make_raw_node(vec![], vec![]);
        node.labels = None;
        let issue = LinearClient::normalize_full(node);
        assert!(issue.labels.is_empty());
    }

    // ---------------------------------------------------------------------- //
    // normalize_full — blocked_by extraction
    // ---------------------------------------------------------------------- //

    #[test]
    fn test_normalize_full_blocked_by_populated() {
        let node = make_raw_node(
            vec![],
            vec![
                ("blocker-id-1", "ENG-10", "In Progress"),
                ("blocker-id-2", "ENG-20", "Todo"),
            ],
        );
        let issue = LinearClient::normalize_full(node);
        assert_eq!(issue.blocked_by.len(), 2);
        assert_eq!(issue.blocked_by[0].id.as_deref(), Some("blocker-id-1"));
        assert_eq!(issue.blocked_by[0].identifier.as_deref(), Some("ENG-10"));
        assert_eq!(issue.blocked_by[0].state.as_deref(), Some("In Progress"));
        assert_eq!(issue.blocked_by[1].id.as_deref(), Some("blocker-id-2"));
        assert_eq!(issue.blocked_by[1].identifier.as_deref(), Some("ENG-20"));
        assert_eq!(issue.blocked_by[1].state.as_deref(), Some("Todo"));
    }

    #[test]
    fn test_normalize_full_no_relations() {
        let mut node = make_raw_node(vec![], vec![]);
        node.relations = None;
        let issue = LinearClient::normalize_full(node);
        assert!(issue.blocked_by.is_empty());
    }

    #[test]
    fn test_normalize_full_relation_without_related_issue_is_skipped() {
        let mut node = make_raw_node(vec![], vec![]);
        node.relations = Some(RawRelations {
            nodes: vec![RawRelationNode {
                relation_type: Some("blocked_by".to_string()),
                related_issue: None,
            }],
        });
        let issue = LinearClient::normalize_full(node);
        assert!(issue.blocked_by.is_empty());
    }

    // ---------------------------------------------------------------------- //
    // normalize_full — datetime parsing
    // ---------------------------------------------------------------------- //

    #[test]
    fn test_normalize_full_datetimes_parsed() {
        let node = make_raw_node(vec![], vec![]);
        let issue = LinearClient::normalize_full(node);
        assert!(issue.created_at.is_some());
        assert!(issue.updated_at.is_some());
    }

    #[test]
    fn test_normalize_full_invalid_datetime_becomes_none() {
        let mut node = make_raw_node(vec![], vec![]);
        node.created_at = Some("not-a-date".to_string());
        node.updated_at = None;
        let issue = LinearClient::normalize_full(node);
        assert!(issue.created_at.is_none());
        assert!(issue.updated_at.is_none());
    }

    // ---------------------------------------------------------------------- //
    // normalize_full — state field
    // ---------------------------------------------------------------------- //

    #[test]
    fn test_normalize_full_state_extracted() {
        let node = make_raw_node(vec![], vec![]);
        let issue = LinearClient::normalize_full(node);
        assert_eq!(issue.state, "In Progress");
    }

    #[test]
    fn test_normalize_full_missing_state_becomes_empty_string() {
        let mut node = make_raw_node(vec![], vec![]);
        node.state = None;
        let issue = LinearClient::normalize_full(node);
        assert_eq!(issue.state, "");
    }

    // ---------------------------------------------------------------------- //
    // normalize_state_only
    // ---------------------------------------------------------------------- //

    #[test]
    fn test_normalize_state_only_fields() {
        let node = RawStateNode {
            id: "sid-1".to_string(),
            identifier: "ENG-99".to_string(),
            state: Some(RawState {
                name: "Done".to_string(),
            }),
        };
        let issue = LinearClient::normalize_state_only(node);
        assert_eq!(issue.id, "sid-1");
        assert_eq!(issue.identifier, "ENG-99");
        assert_eq!(issue.state, "Done");
        assert!(issue.title.is_empty());
        assert!(issue.description.is_none());
        assert!(issue.labels.is_empty());
        assert!(issue.blocked_by.is_empty());
    }

    #[test]
    fn test_normalize_state_only_missing_state() {
        let node = RawStateNode {
            id: "sid-2".to_string(),
            identifier: "ENG-100".to_string(),
            state: None,
        };
        let issue = LinearClient::normalize_state_only(node);
        assert_eq!(issue.state, "");
    }

    // ---------------------------------------------------------------------- //
    // Early-return for empty inputs (pure logic, no HTTP call needed)
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn test_fetch_issues_by_states_empty_returns_empty() {
        // Build a client with a bogus API key — the early-return path must not
        // make any network requests.
        let client = LinearClient::new(
            "fake-key".to_string(),
            Some("http://127.0.0.1:19999/no-server".to_string()),
        );
        let result = client
            .fetch_issues_by_states_impl(&[], "my-project")
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_issue_states_by_ids_empty_returns_empty() {
        let client = LinearClient::new(
            "fake-key".to_string(),
            Some("http://127.0.0.1:19999/no-server".to_string()),
        );
        let result = client.fetch_issue_states_by_ids_impl(&[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_candidate_issues_empty_states_returns_empty() {
        let client = LinearClient::new(
            "fake-key".to_string(),
            Some("http://127.0.0.1:19999/no-server".to_string()),
        );
        let result = client
            .fetch_candidate_issues_impl(&[], "my-project")
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    // ---------------------------------------------------------------------- //
    // Error mapping — non-2xx status
    // ---------------------------------------------------------------------- //

    /// Spin up a tiny mock HTTP server that returns a fixed status + body.
    async fn run_mock_server(status: u16, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let response = format!(
            "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
            status,
            body.len(),
            body
        );

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        format!("http://{}/graphql", addr)
    }

    #[tokio::test]
    async fn test_non_2xx_maps_to_linear_api_status() {
        let endpoint = run_mock_server(401, "Unauthorized").await;
        let client = LinearClient::new("bad-key".to_string(), Some(endpoint));
        let err = client
            .fetch_issue_states_by_ids_impl(&["id-1".to_string()])
            .await
            .unwrap_err();
        match err {
            Error::LinearApiStatus { status, body } => {
                assert_eq!(status, 401);
                assert_eq!(body, "Unauthorized");
            }
            other => panic!("expected LinearApiStatus, got {:?}", other),
        }
    }

    // ---------------------------------------------------------------------- //
    // Error mapping — GraphQL errors array
    // ---------------------------------------------------------------------- //

    async fn run_json_mock_server(response_body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let http_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            response_body.len(),
            response_body
        );

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(http_response.as_bytes()).await;
            }
        });

        format!("http://{}/graphql", addr)
    }

    #[tokio::test]
    async fn test_graphql_errors_array_maps_to_linear_graphql_errors() {
        let body = r#"{"errors":[{"message":"Not found","locations":[],"path":[]}]}"#;
        let endpoint = run_json_mock_server(body).await;
        let client = LinearClient::new("key".to_string(), Some(endpoint));
        let err = client
            .fetch_issue_states_by_ids_impl(&["id-1".to_string()])
            .await
            .unwrap_err();
        match err {
            Error::LinearGraphqlErrors { errors } => {
                assert!(errors.contains("Not found"));
            }
            other => panic!("expected LinearGraphqlErrors, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_missing_data_field_maps_to_linear_unknown_payload() {
        let body = r#"{"something_else": true}"#;
        let endpoint = run_json_mock_server(body).await;
        let client = LinearClient::new("key".to_string(), Some(endpoint));
        let err = client
            .fetch_issue_states_by_ids_impl(&["id-1".to_string()])
            .await
            .unwrap_err();
        match err {
            Error::LinearUnknownPayload { description } => {
                assert!(description.contains("data"));
            }
            other => panic!("expected LinearUnknownPayload, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_malformed_data_maps_to_linear_unknown_payload() {
        // `data` field is present but the shape is wrong (not an `issues` object).
        let body = r#"{"data": {"not_issues": []}}"#;
        let endpoint = run_json_mock_server(body).await;
        let client = LinearClient::new("key".to_string(), Some(endpoint));
        let err = client
            .fetch_issue_states_by_ids_impl(&["id-1".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LinearUnknownPayload { .. }));
    }

    // ---------------------------------------------------------------------- //
    // Successful roundtrip with mock server
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn test_successful_fetch_issue_states() {
        let body = r#"{
            "data": {
                "issues": {
                    "nodes": [
                        {
                            "id": "issue-abc",
                            "identifier": "ENG-42",
                            "state": { "name": "In Progress" }
                        }
                    ],
                    "pageInfo": {
                        "hasNextPage": false,
                        "endCursor": null
                    }
                }
            }
        }"#;
        let endpoint = run_json_mock_server(body).await;
        let client = LinearClient::new("key".to_string(), Some(endpoint));
        let issues = client
            .fetch_issue_states_by_ids_impl(&["issue-abc".to_string()])
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "issue-abc");
        assert_eq!(issues[0].identifier, "ENG-42");
        assert_eq!(issues[0].state, "In Progress");
    }

    // ---------------------------------------------------------------------- //
    // Trait object usage
    // ---------------------------------------------------------------------- //

    #[tokio::test]
    async fn test_trait_object_dispatch() {
        // Verify the trait can be used via a Box<dyn Tracker> without any
        // compile error. The early-return path is exercised (empty ids).
        let client: Box<dyn Tracker> = Box::new(LinearClient::new(
            "fake".to_string(),
            Some("http://127.0.0.1:19999/no-server".to_string()),
        ));
        let result = client.fetch_issue_states_by_ids(&[]).await.unwrap();
        assert!(result.is_empty());
    }
}
