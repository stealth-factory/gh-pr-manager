//! GitHub GraphQL client, queries, and response types.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const ENDPOINT: &str = "https://api.github.com/graphql";

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    token: String,
}

/// Token from `GH_TOKEN` / `GITHUB_TOKEN`, falling back to `gh auth token`.
pub async fn resolve_token() -> Result<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.trim().is_empty()
        {
            return Ok(t.trim().to_string());
        }
    }
    let out = tokio::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        .context("no GH_TOKEN/GITHUB_TOKEN set and failed to run `gh auth token`")?;
    if !out.status.success() {
        bail!("`gh auth token` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimit {
    pub remaining: i64,
    pub limit: i64,
}

impl Client {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent(concat!("gh-pr-manager/", env!("CARGO_PKG_VERSION"))).build()?;
        Ok(Self { http, token })
    }

    async fn graphql<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<(T, Option<RateLimit>)> {
        let resp = self
            .http
            .post(ENDPOINT)
            .bearer_auth(&self.token)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?;
        let status = resp.status();
        let mut body: Value = resp.json().await.with_context(|| format!("HTTP {status}"))?;
        if let Some(errors) = body.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            let msgs: Vec<&str> = errors.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect();
            bail!("{}", msgs.join("; "));
        }
        if !status.is_success() {
            let msg = body.get("message").and_then(Value::as_str).unwrap_or("request failed");
            bail!("HTTP {status}: {msg}");
        }
        let mut data = body.get_mut("data").map(Value::take).ok_or_else(|| anyhow!("response has no data"))?;
        let rate = data.get_mut("rateLimit").map(Value::take).and_then(|v| serde_json::from_value(v).ok());
        Ok((serde_json::from_value(data)?, rate))
    }

    pub async fn viewer(&self) -> Result<(Viewer, Option<RateLimit>)> {
        #[derive(Deserialize)]
        struct R {
            viewer: RawViewer,
        }
        #[derive(Deserialize)]
        struct RawViewer {
            login: String,
            organizations: Nodes<Login>,
        }
        let (r, rate): (R, _) = self.graphql(VIEWER_QUERY, json!({})).await?;
        let orgs = r.viewer.organizations.into_iter().map(|o| o.login).collect();
        Ok((Viewer { login: r.viewer.login, orgs }, rate))
    }

    /// Fetch one page of PR search results.
    pub async fn search_page(&self, query: &str, after: Option<String>) -> Result<SearchPage> {
        #[derive(Deserialize)]
        struct R {
            search: Search,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Search {
            issue_count: usize,
            page_info: PageInfo,
            nodes: Vec<Value>,
        }
        let (r, rate): (R, _) = self.graphql(SEARCH_QUERY, json!({ "q": query, "after": after })).await?;
        // Non-PR nodes come back as `{}`; skip anything that doesn't parse.
        let prs = r.search.nodes.into_iter().filter_map(|n| serde_json::from_value::<PrSummary>(n).ok()).collect();
        let next = if r.search.page_info.has_next_page { r.search.page_info.end_cursor } else { None };
        Ok(SearchPage { prs, total: r.search.issue_count, next, rate })
    }

    pub async fn pr_detail(&self, id: &str) -> Result<(PrDetail, Option<RateLimit>)> {
        #[derive(Deserialize)]
        struct R {
            node: Option<PrDetail>,
        }
        let (r, rate): (R, _) = self.graphql(DETAIL_QUERY, json!({ "id": id })).await?;
        Ok((r.node.ok_or_else(|| anyhow!("pull request not found"))?, rate))
    }

    pub async fn add_comment(&self, subject_id: &str, body: &str) -> Result<()> {
        self.mutate("mutation($id:ID!,$body:String!){ addComment(input:{subjectId:$id, body:$body}){ clientMutationId } }", json!({ "id": subject_id, "body": body }))
            .await
    }

    /// Merge, refusing if the head moved since `head_oid` (what the user saw).
    pub async fn merge(&self, pr_id: &str, method: MergeMethod, head_oid: &str) -> Result<()> {
        self.mutate(
            "mutation($id:ID!,$m:PullRequestMergeMethod!,$oid:GitObjectID){ mergePullRequest(input:{pullRequestId:$id, mergeMethod:$m, expectedHeadOid:$oid}){ clientMutationId } }",
            json!({ "id": pr_id, "m": method.api_name(), "oid": head_oid }),
        )
        .await
    }

    pub async fn close(&self, pr_id: &str) -> Result<()> {
        self.mutate("mutation($id:ID!){ closePullRequest(input:{pullRequestId:$id}){ clientMutationId } }", json!({ "id": pr_id })).await
    }

    pub async fn reopen(&self, pr_id: &str) -> Result<()> {
        self.mutate("mutation($id:ID!){ reopenPullRequest(input:{pullRequestId:$id}){ clientMutationId } }", json!({ "id": pr_id })).await
    }

    pub async fn review(&self, pr_id: &str, event: ReviewEvent, body: &str) -> Result<()> {
        self.mutate(
            "mutation($id:ID!,$e:PullRequestReviewEvent!,$body:String){ addPullRequestReview(input:{pullRequestId:$id, event:$e, body:$body}){ clientMutationId } }",
            json!({ "id": pr_id, "e": event.api_name(), "body": if body.is_empty() { Value::Null } else { body.into() } }),
        )
        .await
    }

    async fn mutate(&self, query: &str, vars: Value) -> Result<()> {
        let _: (Value, _) = self.graphql(query, vars).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    fn api_name(self) -> &'static str {
        match self {
            Self::Merge => "MERGE",
            Self::Squash => "SQUASH",
            Self::Rebase => "REBASE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEvent {
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    fn api_name(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

pub struct SearchPage {
    pub prs: Vec<PrSummary>,
    pub total: usize,
    pub next: Option<String>,
    pub rate: Option<RateLimit>,
}

#[derive(Debug, Clone)]
pub struct Viewer {
    pub login: String,
    pub orgs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Nodes<T> {
    pub nodes: Vec<Option<T>>,
}

impl<T> Nodes<T> {
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.nodes.iter().flatten()
    }
}

impl<T> IntoIterator for Nodes<T> {
    type Item = T;
    type IntoIter = std::iter::Flatten<std::vec::IntoIter<Option<T>>>;
    fn into_iter(self) -> Self::IntoIter {
        self.nodes.into_iter().flatten()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Login {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Count {
    #[serde(rename = "totalCount")]
    pub total_count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoRef {
    pub name_with_owner: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitNode<C> {
    pub commit: C,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollupOnly {
    pub status_check_rollup: Option<RollupState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RollupState {
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrSummary {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    pub state: String,
    pub updated_at: String,
    pub author: Option<Login>,
    pub repository: RepoRef,
    pub review_decision: Option<String>,
    pub comments: Count,
    pub commits: Nodes<CommitNode<RollupOnly>>,
}

impl PrSummary {
    pub fn ci_state(&self) -> Option<&str> {
        self.commits.iter().last()?.commit.status_check_rollup.as_ref().map(|r| r.state.as_str())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDetail {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    #[serde(rename = "bodyHTML")]
    pub body_html: String,
    pub state: String,
    pub is_draft: bool,
    pub mergeable: String,
    pub merge_state_status: Option<String>,
    pub review_decision: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub author: Option<Login>,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub head_ref_oid: String,
    pub can_be_rebased: bool,
    pub viewer_can_close: bool,
    pub viewer_can_reopen: bool,
    pub viewer_can_merge_as_admin: bool,
    pub labels: Nodes<Label>,
    pub assignees: Nodes<Login>,
    pub review_requests: Nodes<ReviewRequest>,
    pub participants: Nodes<Login>,
    pub repository: DetailRepo,
    pub commits: Nodes<CommitNode<DetailCommit>>,
    pub reviews: Nodes<Review>,
    pub comments: Nodes<Comment>,
    pub review_threads: Nodes<ReviewThread>,
}

impl PrDetail {
    pub fn rollup(&self) -> Option<&Rollup> {
        self.commits.iter().last()?.commit.status_check_rollup.as_ref()
    }

}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRequest {
    pub requested_reviewer: Option<Reviewer>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reviewer {
    pub login: Option<String>,
    pub name: Option<String>,
}

impl Reviewer {
    pub fn display(&self) -> String {
        self.login.clone().or_else(|| self.name.as_ref().map(|n| format!("team:{n}"))).unwrap_or_else(|| "?".into())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailRepo {
    pub name_with_owner: String,
    pub merge_commit_allowed: bool,
    pub squash_merge_allowed: bool,
    pub rebase_merge_allowed: bool,
    pub viewer_permission: Option<String>,
    pub mentionable_users: Nodes<Login>,
}

impl DetailRepo {
    pub fn can_write(&self) -> bool {
        matches!(self.viewer_permission.as_deref(), Some("ADMIN" | "MAINTAIN" | "WRITE"))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailCommit {
    pub status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rollup {
    pub state: String,
    pub contexts: Nodes<CheckContext>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckContext {
    #[serde(rename = "__typename")]
    pub typename: String,
    // CheckRun
    pub name: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub details_url: Option<String>,
    pub check_suite: Option<CheckSuite>,
    // StatusContext
    pub context: Option<String>,
    pub state: Option<String>,
    pub target_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckSuite {
    pub workflow_run: Option<WorkflowRun>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRun {
    pub workflow: Named,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Named {
    pub name: String,
}

impl CheckContext {
    pub fn label(&self) -> String {
        if self.typename == "CheckRun" {
            let name = self.name.clone().unwrap_or_default();
            match self.check_suite.as_ref().and_then(|s| s.workflow_run.as_ref()) {
                Some(w) => format!("{} / {name}", w.workflow.name),
                None => name,
            }
        } else {
            self.context.clone().unwrap_or_default()
        }
    }

    /// Normalised outcome: SUCCESS, FAILURE, PENDING, NEUTRAL, SKIPPED, ...
    pub fn outcome(&self) -> String {
        if self.typename == "CheckRun" {
            match self.status.as_deref() {
                Some("COMPLETED") => self.conclusion.clone().unwrap_or_else(|| "NEUTRAL".into()),
                _ => "PENDING".into(),
            }
        } else {
            self.state.clone().unwrap_or_else(|| "PENDING".into())
        }
    }

    pub fn url(&self) -> Option<&str> {
        self.details_url.as_deref().or(self.target_url.as_deref())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub author: Option<Login>,
    pub state: String,
    pub body: String,
    #[serde(rename = "bodyHTML")]
    pub body_html: String,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub author: Option<Login>,
    pub body: String,
    #[serde(rename = "bodyHTML")]
    pub body_html: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub is_resolved: bool,
    pub path: String,
    pub line: Option<u64>,
    pub comments: Nodes<Comment>,
}

pub fn login_of(l: &Option<Login>) -> &str {
    l.as_ref().map(|l| l.login.as_str()).unwrap_or("ghost")
}

const VIEWER_QUERY: &str = "query { viewer { login organizations(first:100){ nodes { login } } } rateLimit { remaining limit } }";

const SEARCH_QUERY: &str = r#"
query($q:String!, $after:String) {
  rateLimit { remaining limit }
  search(query:$q, type:ISSUE, first:50, after:$after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest {
      id number title url isDraft state updatedAt
      author { login }
      repository { nameWithOwner }
      reviewDecision
      comments { totalCount }
      commits(last:1) { nodes { commit { statusCheckRollup { state } } } }
    } }
  }
}"#;

const DETAIL_QUERY: &str = r#"
query($id:ID!) {
  rateLimit { remaining limit }
  node(id:$id) { ... on PullRequest {
    id number title body bodyHTML state isDraft mergeable mergeStateStatus reviewDecision
    createdAt updatedAt author { login } headRefName baseRefName
    additions deletions changedFiles headRefOid canBeRebased
    viewerCanClose viewerCanReopen viewerCanMergeAsAdmin
    labels(first:20) { nodes { name color } }
    assignees(first:10) { nodes { login } }
    reviewRequests(first:20) { nodes { requestedReviewer {
      ... on User { login } ... on Bot { login } ... on Mannequin { login } ... on Team { name } } } }
    participants(first:50) { nodes { login } }
    repository { nameWithOwner mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed viewerPermission
      mentionableUsers(first:100) { nodes { login } } }
    commits(last:1) { nodes { commit { statusCheckRollup { state contexts(first:80) { nodes { __typename
      ... on CheckRun { name status conclusion detailsUrl checkSuite { workflowRun { workflow { name } } } }
      ... on StatusContext { context state targetUrl } } } } } } }
    reviews(last:40) { nodes { author { login } state body bodyHTML submittedAt } }
    comments(last:80) { nodes { author { login } body bodyHTML createdAt } }
    reviewThreads(last:40) { nodes { isResolved path line comments(first:20) { nodes { author { login } body bodyHTML createdAt } } } }
  } }
}"#;
