//! GitHub GraphQL client, queries, and response types.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ENDPOINT: &str = "https://api.github.com/graphql";
/// A stalled read must fail, or a palette search stays in flight for the whole session.
/// Mutations are not bounded: GitHub can still apply the write after the client gives up.
const GRAPHQL_TIMEOUT: Duration = Duration::from_secs(20);
/// The only hosts an image request may send the token to.
const TOKEN_HOSTS: [&str; 3] = ["github.com", "api.github.com", "raw.githubusercontent.com"];
/// Image downloads larger than this are refused.
const IMAGE_CAP: usize = 10 << 20;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    token: String,
    /// The REST `core` bucket, from the last REST response's `x-ratelimit-*` headers.
    /// Separate from GraphQL's, which responses carry in their body.
    rest_rate: Arc<Mutex<Option<RateLimit>>>,
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
        Ok(Self { http, token, rest_rate: Arc::default() })
    }

    pub fn rest_rate(&self) -> Option<RateLimit> {
        self.rest_rate.lock().ok().and_then(|r| r.clone())
    }

    /// GET a REST URL, recording the rate limit. Non-2xx statuses are returned, not raised.
    async fn rest_send(&self, url: &str) -> Result<reqwest::Response> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        if let (Some(remaining), Some(limit)) = (header("x-ratelimit-remaining"), header("x-ratelimit-limit"))
            && let Ok(mut r) = self.rest_rate.lock()
        {
            *r = Some(RateLimit { remaining, limit });
        }
        Ok(resp)
    }

    /// GET a REST URL; an HTTP error becomes `HTTP {status}: {message}`.
    async fn rest_get(&self, url: &str) -> Result<reqwest::Response> {
        let resp = self.rest_send(url).await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or_default();
            bail!("HTTP {status}: {}", body.get("message").and_then(Value::as_str).unwrap_or("request failed"));
        }
        Ok(resp)
    }

    async fn post(&self, query: &str, variables: Value, timeout: Option<Duration>) -> Result<(reqwest::StatusCode, Value)> {
        let mut req = self.http.post(ENDPOINT).bearer_auth(&self.token).json(&json!({ "query": query, "variables": variables }));
        if let Some(limit) = timeout {
            req = req.timeout(limit);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.with_context(|| format!("HTTP {status}"))?;
        Ok((status, body))
    }

    async fn graphql<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<(T, Option<RateLimit>)> {
        let (status, body) = self.post(query, variables, Some(GRAPHQL_TIMEOUT)).await?;
        let (data, rate) = take_data(status, body, false)?;
        Ok((serde_json::from_value(data)?, rate))
    }

    /// Like `graphql`, but keeps `data` when every error is `type: NOT_FOUND` (an aliased
    /// lookup of a PR number that one of the repos doesn't have).
    async fn graphql_partial<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<(T, Option<RateLimit>)> {
        let (status, body) = self.post(query, variables, Some(GRAPHQL_TIMEOUT)).await?;
        let (data, rate) = take_data(status, body, true)?;
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

    /// Fetch one page (`first` ≤ 100) of PR search results.
    pub async fn search_page(&self, query: &str, after: Option<String>, first: u32) -> Result<SearchPage> {
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
        let (r, rate): (R, _) = self.graphql(SEARCH_QUERY, json!({ "q": query, "after": after, "first": first })).await?;
        // Non-PR nodes come back as `{}`; skip anything that doesn't parse.
        let prs = r.search.nodes.into_iter().filter_map(|n| serde_json::from_value::<PrSummary>(n).ok()).collect();
        let next = if r.search.page_info.has_next_page { r.search.page_info.end_cursor } else { None };
        Ok(SearchPage { prs, total: r.search.issue_count, next, rate })
    }

    /// One page (100) of the viewer's non-archived repos, most recently pushed first.
    pub async fn viewer_repos_page(&self, after: Option<String>) -> Result<RepoPage> {
        #[derive(Deserialize)]
        struct R {
            viewer: V,
        }
        #[derive(Deserialize)]
        struct V {
            repositories: Conn,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Conn {
            total_count: usize,
            page_info: PageInfo,
            nodes: Vec<Option<RepoInfo>>,
        }
        let (r, rate): (R, _) = self.graphql(VIEWER_REPOS_QUERY, json!({ "after": after })).await?;
        let c = r.viewer.repositories;
        let next = if c.page_info.has_next_page { c.page_info.end_cursor } else { None };
        Ok(RepoPage { repos: c.nodes.into_iter().flatten().collect(), total: c.total_count, next, rate })
    }

    /// Descriptions for up to 100 PR ids (search matches them); ids that no longer resolve are skipped.
    pub async fn pr_bodies(&self, ids: &[String]) -> Result<(Vec<PrBody>, Option<RateLimit>)> {
        #[derive(Deserialize)]
        struct R {
            nodes: Vec<Option<Value>>,
        }
        let (r, rate): (R, _) = self.graphql_partial(BODIES_QUERY, json!({ "ids": ids })).await?;
        let bodies = r.nodes.into_iter().flatten().filter_map(|n| serde_json::from_value::<PrBody>(n).ok()).collect();
        Ok((bodies, rate))
    }

    /// PR `n` in each of `repos` (`owner/name`, at most 12), in one request of aliased lookups.
    /// Repos without that number are left out.
    pub async fn prs_by_number(&self, repos: &[String], n: u64) -> Result<(Vec<PrSummary>, Option<RateLimit>)> {
        if repos.is_empty() {
            return Ok((Vec::new(), None));
        }
        let (data, rate): (Value, _) = self.graphql_partial(&number_query(repos), json!({ "n": n })).await?;
        Ok((parse_number_hits(&data, repos.len()), rate))
    }

    pub async fn pr_detail(&self, id: &str) -> Result<(PrDetail, Option<RateLimit>)> {
        #[derive(Deserialize)]
        struct R {
            node: Option<PrDetail>,
        }
        let (r, rate): (R, _) = self.graphql(DETAIL_QUERY, json!({ "id": id })).await?;
        Ok((r.node.ok_or_else(|| anyhow!("pull request not found"))?, rate))
    }

    /// Changed files with patches (REST; GraphQL has no diffs). Capped at 1000 files.
    pub async fn pr_files(&self, name_with_owner: &str, number: u64) -> Result<Vec<FileDiff>> {
        let mut out = Vec::new();
        for page in 1..=10 {
            let url = format!("https://api.github.com/repos/{name_with_owner}/pulls/{number}/files?per_page=100&page={page}");
            let files: Vec<FileDiff> = self.rest_get(&url).await?.json().await?;
            let done = files.len() < 100;
            out.extend(files);
            if done {
                break;
            }
        }
        Ok(out)
    }

    /// Workflow runs for a head commit, or the latest on a branch (history).
    pub async fn workflow_runs(&self, nwo: &str, q: RunsQuery<'_>) -> Result<Vec<WorkflowRunInfo>> {
        #[derive(Deserialize)]
        struct R {
            workflow_runs: Vec<WorkflowRunInfo>,
        }
        let url = match q {
            RunsQuery::HeadSha(sha) => format!("https://api.github.com/repos/{nwo}/actions/runs?head_sha={sha}&per_page=100"),
            RunsQuery::Branch { name, per_page } => {
                format!("https://api.github.com/repos/{nwo}/actions/runs?branch={}&event=pull_request&per_page={per_page}", url_encode(name))
            }
        };
        let r: R = self.rest_get(&url).await?.json().await?;
        Ok(r.workflow_runs)
    }

    /// Jobs (with steps) of a run's latest attempt.
    pub async fn run_jobs(&self, nwo: &str, run_id: u64) -> Result<Vec<Job>> {
        #[derive(Deserialize)]
        struct R {
            total_count: usize,
            jobs: Vec<Job>,
        }
        let mut out = Vec::new();
        for page in 1..=5 {
            let url = format!("https://api.github.com/repos/{nwo}/actions/runs/{run_id}/jobs?per_page=100&page={page}");
            let r: R = self.rest_get(&url).await?.json().await?;
            let done = r.jobs.len() < 100 || out.len() + r.jobs.len() >= r.total_count;
            out.extend(r.jobs);
            if done {
                break;
            }
        }
        Ok(out)
    }

    /// A finished job's log, keeping the last `cap` bytes (trimmed as it streams, so a huge
    /// log never sits in memory whole). The API redirects to a blob that only exists once the
    /// job completes; until then this returns `pending`.
    pub async fn job_log(&self, nwo: &str, job_id: u64, cap: usize) -> Result<JobLog> {
        let mut resp = self.rest_send(&format!("https://api.github.com/repos/{nwo}/actions/jobs/{job_id}/logs")).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND && resp.url().host_str() != Some("api.github.com") {
            return Ok(JobLog { text: String::new(), dropped_lines: 0, pending: true });
        }
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or_default();
            bail!("HTTP {status}: {}", body.get("message").and_then(Value::as_str).unwrap_or("request failed"));
        }
        let mut tail = crate::actions_log::Tail::new(cap);
        while let Some(chunk) = resp.chunk().await? {
            tail.push(&chunk);
        }
        let (kept, dropped_lines) = tail.finish();
        Ok(JobLog { text: String::from_utf8_lossy(&kept).into_owned(), dropped_lines, pending: false })
    }

    /// Annotations of a job (a job id is also its check-run id). Capped at 1000.
    pub async fn job_annotations(&self, nwo: &str, job_id: u64) -> Result<Vec<Annotation>> {
        let mut out = Vec::new();
        for page in 1..=10 {
            let url = format!("https://api.github.com/repos/{nwo}/check-runs/{job_id}/annotations?per_page=100&page={page}");
            let anns: Vec<Annotation> = self.rest_get(&url).await?.json().await?;
            let done = anns.len() < 100;
            out.extend(anns);
            if done {
                break;
            }
        }
        Ok(out)
    }

    /// How many workflows the repo has; zero means it doesn't use Actions.
    pub async fn workflow_count(&self, nwo: &str) -> Result<usize> {
        #[derive(Deserialize)]
        struct R {
            total_count: usize,
        }
        let r: R = self.rest_get(&format!("https://api.github.com/repos/{nwo}/actions/workflows?per_page=1")).await?.json().await?;
        Ok(r.total_count)
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

    /// Download an image for inline display: the bytes and the content type (lowercased).
    /// `auth` asks for the token, which only goes to `TOKEN_HOSTS`; reqwest drops it on a
    /// redirect to another host (user-attachments redirect to S3).
    pub async fn fetch_image(&self, url: &str, auth: bool) -> Result<(Vec<u8>, String)> {
        let parsed = reqwest::Url::parse(url)?;
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        let mut req = self.http.get(url).header("Accept", "image/*").timeout(std::time::Duration::from_secs(15));
        // Never over plain HTTP: the first request would carry the token in clear text.
        if auth && parsed.scheme() == "https" && TOKEN_HOSTS.contains(&host.as_str()) {
            req = req.bearer_auth(&self.token);
        }
        let mut resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("HTTP {}", status.as_u16());
        }
        let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        let too_big = || anyhow!("larger than {} MiB", IMAGE_CAP >> 20);
        if resp.content_length().is_some_and(|n| n > IMAGE_CAP as u64) {
            return Err(too_big());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > IMAGE_CAP {
                return Err(too_big());
            }
        }
        Ok((bytes, ctype))
    }

    async fn mutate(&self, query: &str, vars: Value) -> Result<()> {
        let (status, body) = self.post(query, vars, None).await?;
        take_data(status, body, false)?;
        Ok(())
    }

    // ---------- stacks (docs/stacks.md) ----------

    /// A REST call with a JSON body under the stacks API version; the status and body come back
    /// as they are (callers decide what an error is). Records the rate limit like `rest_send`.
    async fn rest_json(&self, method: reqwest::Method, url: &str, body: Option<Value>) -> Result<(reqwest::StatusCode, Value)> {
        // reqwest has no default timeout: a stalled merge-async poll would hold the PR busy. Only
        // reads get one; a timed-out mutation may still have happened, so it isn't cut short.
        let read = method == reqwest::Method::GET;
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", STACKS_API_VERSION);
        if let Some(b) = body {
            req = req.json(&b);
        }
        if read {
            req = req.timeout(GRAPHQL_TIMEOUT);
        }
        let resp = req.send().await?;
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        if let (Some(remaining), Some(limit)) = (header("x-ratelimit-remaining"), header("x-ratelimit-limit"))
            && let Ok(mut r) = self.rest_rate.lock()
        {
            *r = Some(RateLimit { remaining, limit });
        }
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or_default();
        Ok((status, body))
    }

    /// `aheadBy`/`behindBy` of each `(child head, its base)` pair, in one aliased query.
    /// Branches that no longer exist (a merged parent's deleted head) are left out.
    pub async fn compare_links(&self, nwo: &str, pairs: &[(String, String)]) -> Result<(Vec<crate::stacks::Link>, Option<RateLimit>)> {
        if pairs.is_empty() {
            return Ok((Vec::new(), None));
        }
        let (data, rate): (Value, _) = self.graphql_partial(&compare_query(nwo, pairs), json!({})).await?;
        Ok((parse_links(&data, pairs), rate))
    }

    /// The files of `base...head` (REST compare; the same shape as `pulls/{n}/files`, GitHub
    /// stops at 300 files).
    pub async fn compare_files(&self, nwo: &str, base: &str, head: &str) -> Result<Vec<FileDiff>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            files: Vec<FileDiff>,
        }
        let url = format!("https://api.github.com/repos/{nwo}/compare/{}...{}?per_page=100", path_encode(base), path_encode(head));
        let r: R = self.rest_get(&url).await?.json().await?;
        Ok(r.files)
    }

    /// Bring a PR's branch up to date with its base, refusing if the head moved since `head_oid`.
    pub async fn update_branch(&self, pr_id: &str, head_oid: &str, method: UpdateMethod) -> Result<()> {
        self.mutate(
            "mutation($id:ID!,$oid:GitObjectID,$m:PullRequestBranchUpdateMethod){ updatePullRequestBranch(input:{pullRequestId:$id, expectedHeadOid:$oid, updateMethod:$m}){ clientMutationId } }",
            json!({ "id": pr_id, "oid": head_oid, "m": method.api_name() }),
        )
        .await
    }

    /// Link open PRs (bottom first) into a native stack; returns the stack's number.
    pub async fn create_stack(&self, nwo: &str, numbers: &[u64]) -> Result<u64> {
        let url = format!("https://api.github.com/repos/{nwo}/stacks");
        let (status, body) = self.rest_json(reqwest::Method::POST, &url, Some(json!({ "pull_requests": numbers }))).await?;
        parse_created_stack(status, &body)
    }

    /// Start an asynchronous merge of PR `n` (with its open downstack when it's stacked).
    pub async fn merge_async(&self, nwo: &str, n: u64, sha: &str, method: MergeMethod) -> Result<MergeJob> {
        let url = format!("https://api.github.com/repos/{nwo}/pulls/{n}/merge-async");
        let (status, body) = self.rest_json(reqwest::Method::PUT, &url, Some(merge_async_body(sha, method))).await?;
        parse_merge_job(status, &body)
    }

    pub async fn merge_async_status(&self, nwo: &str, n: u64, uuid: &str) -> Result<MergeJob> {
        let url = format!("https://api.github.com/repos/{nwo}/pulls/{n}/merge-async/{}", path_encode(uuid));
        let (status, body) = self.rest_json(reqwest::Method::GET, &url, None).await?;
        parse_merge_job(status, &body)
    }
}

/// The stacks and merge-async endpoints are documented under this REST version.
const STACKS_API_VERSION: &str = "2026-03-10";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMethod {
    Merge,
    Rebase,
}

impl UpdateMethod {
    fn api_name(self) -> &'static str {
        match self {
            Self::Merge => "MERGE",
            Self::Rebase => "REBASE",
        }
    }
}

/// Update a branch by rebase, falling back to a merge of the base when GitHub refuses the
/// rebase (`canBeRebased` is false on many chained PRs). Any other failure (network, rate
/// limit, a moved head) is reported as is, never retried as a merge. `update` runs one attempt.
pub async fn rebase_or_merge<F, Fut>(update: F) -> Result<String>
where
    F: Fn(UpdateMethod) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    match update(UpdateMethod::Rebase).await {
        Ok(()) => Ok("Branch rebased onto its base".into()),
        Err(rebase) if !format!("{rebase:#}").to_lowercase().contains("rebase") => Err(rebase.context("couldn't rebase the branch")),
        Err(rebase) => match update(UpdateMethod::Merge).await {
            Ok(()) => Ok(format!("Rebase refused ({}); merged the base in instead", first_line(&format!("{rebase:#}")))),
            Err(merge) => Err(anyhow!("couldn't update the branch: rebase: {}; merge: {}", first_line(&format!("{rebase:#}")), first_line(&format!("{merge:#}")))),
        },
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// Where an asynchronous merge stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeJob {
    /// `pending`, `merged`, `enqueued` or `failed`.
    pub status: String,
    pub uuid: Option<String>,
    /// GitHub's message (the failure reason when `failed`).
    pub message: Option<String>,
}

fn merge_async_body(sha: &str, method: MergeMethod) -> Value {
    json!({ "sha": sha, "merge_method": method.api_name().to_lowercase(), "merge_action": "default" })
}

/// 200 (already done), 202 (accepted) and 409 (one is already pending: keep following it)
/// carry a job; anything else is an error with GitHub's message.
fn parse_merge_job(status: reqwest::StatusCode, body: &Value) -> Result<MergeJob> {
    let ok = status.is_success() || status == reqwest::StatusCode::CONFLICT;
    let job_status = body.get("status").and_then(Value::as_str);
    match (ok, job_status) {
        (true, Some(st)) => {
            let details = body.get("details");
            let field = |k: &str| details.and_then(|d| d.get(k)).or_else(|| body.get(k)).and_then(Value::as_str).map(String::from);
            // A failed job documents its reason under `error`; older replies used `message`.
            Ok(MergeJob { status: st.to_string(), uuid: field("uuid"), message: field("error").or_else(|| field("message")) })
        }
        _ => bail!("{}", rest_error(status, body)),
    }
}

/// The stack number from a `POST /stacks` reply.
fn parse_created_stack(status: reqwest::StatusCode, body: &Value) -> Result<u64> {
    if !status.is_success() {
        bail!("{}", rest_error(status, body));
    }
    body.get("number").and_then(Value::as_u64).ok_or_else(|| anyhow!("GitHub didn't return the new stack's number"))
}

/// `HTTP 422: message (detail; detail)`, naming the "stacks not enabled" case plainly.
fn rest_error(status: reqwest::StatusCode, body: &Value) -> String {
    let msg = body.get("message").and_then(Value::as_str).unwrap_or("request failed");
    let details: Vec<String> = body
        .get("errors")
        .and_then(Value::as_array)
        .map(|errs| errs.iter().filter_map(|e| e.as_str().map(String::from).or_else(|| e.get("message").and_then(Value::as_str).map(String::from))).collect())
        .unwrap_or_default();
    let all = std::iter::once(msg.to_string()).chain(details.iter().cloned()).collect::<Vec<_>>().join(" ").to_lowercase();
    let mut out = format!("HTTP {}: {msg}", status.as_u16());
    if !details.is_empty() {
        out.push_str(&format!(" ({})", details.join("; ")));
    }
    if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY && all.contains("not enabled") {
        out = format!("Stacked PRs are not enabled for this repository — {out}");
    }
    out
}

/// Percent-encode a path segment, keeping `/` (branch names) readable.
fn path_encode(s: &str) -> String {
    s.split('/').map(url_encode).collect::<Vec<_>>().join("/")
}

/// `l0: ref(qualifiedName:"refs/heads/child") { compare(headRef:"base") { aheadBy behindBy status } } …`
fn compare_query(nwo: &str, pairs: &[(String, String)]) -> String {
    let lit = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let (owner, name) = nwo.split_once('/').unwrap_or((nwo, ""));
    let mut q = format!("query {{ rateLimit {{ remaining limit }} repository(owner:{}, name:{}) {{", lit(owner), lit(name));
    for (i, (child, base)) in pairs.iter().enumerate() {
        q.push_str(&format!(
            " l{i}: ref(qualifiedName:{}) {{ compare(headRef:{}) {{ aheadBy behindBy status }} }}",
            lit(&format!("refs/heads/{child}")),
            lit(base)
        ));
    }
    q.push_str(" } }");
    q
}

fn parse_links(data: &Value, pairs: &[(String, String)]) -> Vec<crate::stacks::Link> {
    let repo = data.get("repository");
    pairs
        .iter()
        .enumerate()
        .filter_map(|(i, (child, base))| {
            let c = repo?.get(format!("l{i}"))?.get("compare").filter(|c| !c.is_null())?;
            let n = |k: &str| c.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
            Some(crate::stacks::Link {
                child: child.clone(),
                parent: base.clone(),
                ahead: n("aheadBy"),
                behind: n("behindBy"),
                status: c.get("status").and_then(Value::as_str).unwrap_or_default().to_string(),
            })
        })
        .collect()
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

/// Which workflow runs to list.
#[derive(Debug, Clone, Copy)]
pub enum RunsQuery<'a> {
    HeadSha(&'a str),
    Branch { name: &'a str, per_page: u8 },
}

/// Percent-encode a query value (branch names may hold `/`, `#`, `+`…).
fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A workflow run from the REST Actions API.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRunInfo {
    pub id: u64,
    pub name: Option<String>,
    #[serde(default)]
    pub display_title: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub event: String,
    pub run_number: u64,
    #[serde(default)]
    pub run_attempt: u64,
    pub head_sha: String,
    #[serde(default)]
    pub path: String,
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
    pub run_started_at: Option<String>,
    pub actor: Option<Login>,
    pub triggering_actor: Option<Login>,
}

impl WorkflowRunInfo {
    pub fn name(&self) -> &str {
        self.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.path)
    }

    pub fn status(&self) -> &str {
        self.status.as_deref().unwrap_or("queued")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub id: u64,
    pub run_id: u64,
    #[serde(default)]
    pub run_attempt: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub name: String,
    pub number: u64,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Annotation {
    pub path: String,
    pub start_line: Option<u64>,
    pub annotation_level: String,
    pub title: Option<String>,
    pub message: String,
}

pub struct JobLog {
    pub text: String,
    /// Whole lines cut from the front to stay under the byte cap.
    pub dropped_lines: usize,
    /// The job hasn't finished, so there's no log yet.
    pub pending: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    pub name_with_owner: String,
    pub is_private: bool,
    pub pushed_at: Option<String>,
}

pub struct RepoPage {
    pub repos: Vec<RepoInfo>,
    pub total: usize,
    pub next: Option<String>,
    pub rate: Option<RateLimit>,
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

impl<T> Default for Nodes<T> {
    fn default() -> Self {
        Self { nodes: Vec::new() }
    }
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

#[derive(Debug, Clone, Default, Deserialize)]
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
    pub commits: LastCommit,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub labels: Nodes<LabelName>,
    // Change signals (docs/change-signals.md); all optional so older shapes still parse.
    #[serde(default)]
    pub reviews: Count,
    #[serde(default)]
    pub review_threads: Count,
    #[serde(default)]
    pub review_requests: Count,
    #[serde(default)]
    pub head_ref_oid: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN` (computed lazily by GitHub).
    #[serde(default)]
    pub mergeable: Option<String>,
    #[serde(default)]
    pub last_edited_at: Option<String>,
    /// GitHub's own notification read state for the viewer.
    #[serde(default)]
    pub is_read_by_viewer: Option<bool>,
    /// Set while the viewer's review is requested.
    #[serde(default)]
    pub viewer_latest_review_request: Option<Value>,
    // Stacks (docs/stacks.md).
    #[serde(default)]
    pub base_ref_name: String,
    /// Head branch lives in a fork, so it never chains.
    #[serde(default)]
    pub is_cross_repository: bool,
    /// The native GitHub stack it belongs to, and its 1-based position (1 = bottom).
    #[serde(default)]
    pub stack: Option<StackRef>,
    #[serde(default)]
    pub stack_entry: Option<StackEntryRef>,
}

/// `PullRequest.stack` in the list query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackRef {
    pub id: String,
    pub number: u64,
    pub size: usize,
    #[serde(default)]
    pub base_ref_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StackEntryRef {
    pub position: usize,
}

/// `PullRequest.stack` in the detail query, with its entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackDetail {
    pub id: String,
    pub number: u64,
    pub size: usize,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub entries: Counted<StackEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackEntry {
    pub position: usize,
    pub pull_request: Option<StackMemberPr>,
}

/// The signal fields of a stack member (it may not be in the loaded list).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackMemberPr {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: String,
    #[serde(default)]
    pub mergeable: Option<String>,
    #[serde(default)]
    pub merge_state_status: Option<String>,
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub commits: LastCommit,
}

impl StackMemberPr {
    pub fn ci_state(&self) -> Option<&str> {
        self.commits.nodes.iter().flatten().last()?.commit.status_check_rollup.as_ref().map(|r| r.state.as_str())
    }
}

/// `commits(last:1)`: the head commit's CI rollup plus the PR's commit count.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastCommit {
    #[serde(default)]
    pub total_count: usize,
    pub nodes: Vec<Option<CommitNode<RollupOnly>>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelName {
    pub name: String,
}

/// A PR description, fetched lazily for search.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrBody {
    pub id: String,
    pub updated_at: String,
    pub body: String,
}

impl PrSummary {
    pub fn ci_state(&self) -> Option<&str> {
        self.commits.nodes.iter().flatten().last()?.commit.status_check_rollup.as_ref().map(|r| r.state.as_str())
    }

    /// Take the signal fields of a fresher detail of the same PR, so a list row and the detail
    /// snapshot agree (no spurious unread when one replaces the other as a baseline).
    pub fn absorb(&mut self, d: &PrDetail, me: Option<&str>) {
        self.title = d.title.clone();
        self.state = d.state.clone();
        self.is_draft = d.is_draft;
        self.review_decision = d.review_decision.clone();
        self.updated_at = d.updated_at.clone();
        self.comments.total_count = d.comments.total_count;
        self.reviews.total_count = d.reviews.total_count;
        self.review_threads.total_count = d.review_threads.total_count;
        self.review_requests.total_count = d.review_requests.iter().count();
        self.commits.total_count = d.commits.total_count;
        self.head_ref_oid = d.head_ref_oid.clone();
        let rollup = d.rollup().map(|r| RollupState { state: r.state.clone() });
        self.commits.nodes = vec![Some(CommitNode { commit: RollupOnly { status_check_rollup: rollup } })];
        // `UNKNOWN` never overrides a known mergeability.
        if d.mergeable != "UNKNOWN" || self.mergeable.is_none() {
            self.mergeable = Some(d.mergeable.clone());
        }
        self.last_edited_at = d.last_edited_at.clone();
        self.head_ref_name = d.head_ref_name.clone();
        self.base_ref_name = d.base_ref_name.clone();
        self.stack = d.stack.as_ref().map(|s| StackRef { id: s.id.clone(), number: s.number, size: s.size, base_ref_name: s.base_ref_name.clone() });
        self.stack_entry = d.stack_entry.clone();
        if d.is_read_by_viewer.is_some() {
            self.is_read_by_viewer = d.is_read_by_viewer;
        }
        // `reviewRequests(first:20)` can miss the viewer. The detail carries the same
        // viewer-specific field as the list, so a dropped request clears the flag.
        if me.is_some() {
            self.viewer_latest_review_request = d.viewer_latest_review_request.clone();
        }
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
    /// Head branch lives in a fork (branch-based run history doesn't reach it).
    #[serde(default)]
    pub is_cross_repository: bool,
    pub can_be_rebased: bool,
    pub viewer_can_close: bool,
    pub viewer_can_reopen: bool,
    pub viewer_can_merge_as_admin: bool,
    pub labels: Nodes<Label>,
    pub assignees: Nodes<Login>,
    pub review_requests: Nodes<ReviewRequest>,
    pub participants: Nodes<Login>,
    pub repository: DetailRepo,
    pub status_check_rollup: Option<Rollup>,
    pub commits: CommitConnection,
    pub reviews: Counted<Review>,
    pub comments: Counted<Comment>,
    pub review_threads: Counted<ReviewThread>,
    // Change signals (docs/change-signals.md).
    #[serde(default)]
    pub last_edited_at: Option<String>,
    #[serde(default)]
    pub is_read_by_viewer: Option<bool>,
    /// Viewer-specific, including team and code-owner requests. Not derived from `reviewRequests(first:20)`.
    #[serde(default)]
    pub viewer_latest_review_request: Option<Value>,
    /// The last few merge/close/review-request/force-push/ready events, for "merged by alice".
    #[serde(default)]
    pub timeline_items: Nodes<TimelineEvent>,
    // Stacks (docs/stacks.md).
    #[serde(default)]
    pub stack: Option<StackDetail>,
    #[serde(default)]
    pub stack_entry: Option<StackEntryRef>,
}

impl PrDetail {
    pub fn rollup(&self) -> Option<&Rollup> {
        self.status_check_rollup.as_ref()
    }

    /// The latest timeline event of `typename` (`MergedEvent`, `ClosedEvent`, ...).
    pub fn last_event(&self, typename: &str) -> Option<&TimelineEvent> {
        self.timeline_items.iter().filter(|e| e.typename == typename).last()
    }
}

/// A connection with its total count (the list query's counters, so detail and list agree).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counted<T> {
    #[serde(default)]
    pub total_count: usize,
    pub nodes: Vec<Option<T>>,
}

impl<T> Default for Counted<T> {
    fn default() -> Self {
        Self { total_count: 0, nodes: Vec::new() }
    }
}

impl<T> Counted<T> {
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.nodes.iter().flatten()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEvent {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub actor: Option<Login>,
    #[serde(default)]
    pub created_at: String,
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
    #[serde(default)]
    pub default_branch_ref: Option<Named>,
}

impl DetailRepo {
    pub fn can_write(&self) -> bool {
        matches!(self.viewer_permission.as_deref(), Some("ADMIN" | "MAINTAIN" | "WRITE"))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitConnection {
    pub total_count: usize,
    pub nodes: Vec<Option<CommitNode<CommitInfo>>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitInfo {
    #[serde(default)]
    pub oid: String,
    pub abbreviated_oid: String,
    pub message_headline: String,
    pub committed_date: String,
    pub url: String,
    pub authors: Nodes<CommitAuthor>,
    pub status_check_rollup: Option<RollupState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitAuthor {
    pub name: Option<String>,
    pub user: Option<Login>,
}

impl CommitInfo {
    pub fn author(&self) -> String {
        let a = self.authors.iter().next();
        a.and_then(|a| a.user.as_ref().map(|u| u.login.clone()).or_else(|| a.name.clone())).unwrap_or_else(|| "unknown".into())
    }
}

/// A changed file from the REST `pulls/{n}/files` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct FileDiff {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
    pub previous_filename: Option<String>,
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
    #[serde(default)]
    pub id: String,
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
    #[serde(default)]
    pub id: String,
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

const VIEWER_REPOS_QUERY: &str = r#"
query($after:String) {
  rateLimit { remaining limit }
  viewer { repositories(first:100, after:$after,
      affiliations:[OWNER,COLLABORATOR,ORGANIZATION_MEMBER],
      ownerAffiliations:[OWNER,COLLABORATOR,ORGANIZATION_MEMBER],
      orderBy:{field:PUSHED_AT, direction:DESC}, isArchived:false) {
    totalCount pageInfo { hasNextPage endCursor }
    nodes { nameWithOwner isPrivate pushedAt } } }
}"#;

/// The fields of a list row; shared by the search and the by-number lookups.
macro_rules! pr_fields {
    () => {
        "id number title url isDraft state updatedAt
      author { login }
      repository { nameWithOwner }
      reviewDecision
      comments { totalCount }
      commits(last:1) { totalCount nodes { commit { statusCheckRollup { state } } } }
      headRefName
      labels(first:10) { nodes { name } }
      reviews { totalCount } reviewThreads { totalCount } reviewRequests { totalCount }
      headRefOid mergeable lastEditedAt isReadByViewer
      viewerLatestReviewRequest { asCodeOwner }
      baseRefName isCrossRepository stack { id number size baseRefName } stackEntry { position }"
    };
}

const SEARCH_QUERY: &str = concat!(
    r#"
query($q:String!, $after:String, $first:Int!) {
  rateLimit { remaining limit }
  search(query:$q, type:ISSUE, first:$first, after:$after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest {
      "#,
    pr_fields!(),
    r#"
    } }
  }
}"#
);

const BODIES_QUERY: &str = "query($ids:[ID!]!){ rateLimit{remaining limit} nodes(ids:$ids){ ... on PullRequest { id updatedAt body } } }";

/// `r0: repository(owner:"o", name:"n") { nameWithOwner pullRequest(number:$n) { … } } r1: …`
fn number_query(repos: &[String]) -> String {
    let mut q = String::from("query($n:Int!) { rateLimit { remaining limit }");
    for (i, r) in repos.iter().enumerate() {
        let (owner, name) = r.split_once('/').unwrap_or((r, ""));
        let lit = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
        q.push_str(&format!(
            " r{i}: repository(owner:{}, name:{}) {{ nameWithOwner pullRequest(number:$n) {{ {} }} }}",
            lit(owner),
            lit(name),
            pr_fields!()
        ));
    }
    q.push_str(" }");
    q
}

/// The PRs in an aliased by-number response, in alias order.
fn parse_number_hits(data: &Value, n: usize) -> Vec<PrSummary> {
    (0..n)
        .filter_map(|i| data.get(format!("r{i}"))?.get("pullRequest").filter(|v| !v.is_null()).cloned())
        .filter_map(|v| serde_json::from_value::<PrSummary>(v).ok())
        .collect()
}

/// Split a GraphQL response into `data` (minus `rateLimit`) and the rate limit. Errors fail it,
/// except that with `partial` a response whose errors are all `NOT_FOUND` keeps its data.
fn take_data(status: reqwest::StatusCode, mut body: Value, partial: bool) -> Result<(Value, Option<RateLimit>)> {
    if let Some(errors) = body.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let not_found = |e: &Value| e.get("type").and_then(Value::as_str) == Some("NOT_FOUND");
        let has_data = body.get("data").is_some_and(|d| !d.is_null());
        if !(partial && has_data && errors.iter().all(not_found)) {
            let msgs: Vec<&str> = errors.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect();
            bail!("{}", msgs.join("; "));
        }
    }
    if !status.is_success() {
        let msg = body.get("message").and_then(Value::as_str).unwrap_or("request failed");
        bail!("HTTP {status}: {msg}");
    }
    let mut data = body.get_mut("data").map(Value::take).filter(|d| !d.is_null()).ok_or_else(|| anyhow!("response has no data"))?;
    let rate = data.get_mut("rateLimit").map(Value::take).and_then(|v| serde_json::from_value(v).ok());
    Ok((data, rate))
}

const DETAIL_QUERY: &str = r#"
query($id:ID!) {
  rateLimit { remaining limit }
  node(id:$id) { ... on PullRequest {
    id number title body bodyHTML state isDraft mergeable mergeStateStatus reviewDecision
    createdAt updatedAt author { login } headRefName baseRefName
    additions deletions changedFiles headRefOid isCrossRepository canBeRebased
    viewerCanClose viewerCanReopen viewerCanMergeAsAdmin
    labels(first:20) { nodes { name color } }
    assignees(first:10) { nodes { login } }
    reviewRequests(first:20) { nodes { requestedReviewer {
      ... on User { login } ... on Bot { login } ... on Mannequin { login } ... on Team { name } } } }
    participants(first:50) { nodes { login } }
    repository { nameWithOwner mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed viewerPermission
      mentionableUsers(first:100) { nodes { login } } defaultBranchRef { name } }
    stackEntry { position }
    stack { id number size baseRefName entries(first:100) { totalCount nodes { position pullRequest {
      id number title state isDraft headRefName baseRefName headRefOid mergeable mergeStateStatus reviewDecision
      commits(last:1) { nodes { commit { statusCheckRollup { state } } } } } } } }
    statusCheckRollup { state contexts(first:100) { nodes { __typename
      ... on CheckRun { name status conclusion detailsUrl checkSuite { workflowRun { workflow { name } } } }
      ... on StatusContext { context state targetUrl } } } }
    commits(last:100) { totalCount nodes { commit { oid abbreviatedOid messageHeadline committedDate url
      authors(first:1) { nodes { name user { login } } } statusCheckRollup { state } } } }
    reviews(last:40) { totalCount nodes { id author { login } state body bodyHTML submittedAt } }
    comments(last:80) { totalCount nodes { id author { login } body bodyHTML createdAt } }
    reviewThreads(last:40) { totalCount nodes { isResolved path line
      comments(first:20) { nodes { id author { login } body bodyHTML createdAt } } } }
    lastEditedAt isReadByViewer
    viewerLatestReviewRequest { asCodeOwner }
    timelineItems(last:5, itemTypes:[MERGED_EVENT, CLOSED_EVENT, REVIEW_REQUESTED_EVENT, HEAD_REF_FORCE_PUSHED_EVENT, READY_FOR_REVIEW_EVENT]) {
      nodes { __typename
        ... on MergedEvent { actor { login } createdAt }
        ... on ClosedEvent { actor { login } createdAt }
        ... on ReadyForReviewEvent { actor { login } createdAt }
        ... on ReviewRequestedEvent { actor { login } createdAt }
        ... on HeadRefForcePushedEvent { actor { login } createdAt } } }
  } }
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_keeps_data_with_not_found_errors() {
        let body = json!({
            "data": { "rateLimit": { "remaining": 4999, "limit": 5000 }, "r0": { "nameWithOwner": "a/b", "pullRequest": null }, "r1": null },
            "errors": [
                { "type": "NOT_FOUND", "path": ["r0", "pullRequest"], "message": "Could not resolve to a PullRequest with the number of 14." },
                { "type": "NOT_FOUND", "path": ["r1"], "message": "Could not resolve to a Repository with the name 'a/c'." }
            ]
        });
        let (data, rate) = take_data(reqwest::StatusCode::OK, body.clone(), true).unwrap();
        assert_eq!(rate.unwrap().remaining, 4999);
        assert!(data["rateLimit"].is_null());
        assert!(parse_number_hits(&data, 2).is_empty());
        // Without `partial` the same response fails.
        assert!(take_data(reqwest::StatusCode::OK, body, false).is_err());
        // Any other error fails even with `partial`.
        let other = json!({ "data": { "r0": null }, "errors": [{ "type": "NOT_FOUND", "message": "x" }, { "type": "FORBIDDEN", "message": "nope" }] });
        assert_eq!(take_data(reqwest::StatusCode::OK, other, true).unwrap_err().to_string(), "x; nope");
        let no_data = json!({ "data": null, "errors": [{ "type": "NOT_FOUND", "message": "gone" }] });
        assert!(take_data(reqwest::StatusCode::OK, no_data, true).is_err());
    }

    #[test]
    fn number_query_aliases_each_repo() {
        let q = number_query(&["acme/one".to_string(), "acme/two".to_string()]);
        assert!(q.contains(r#"r0: repository(owner:"acme", name:"one")"#), "{q}");
        assert!(q.contains(r#"r1: repository(owner:"acme", name:"two")"#), "{q}");
        assert_eq!(q.matches("pullRequest(number:$n)").count(), 2);
        assert!(q.contains("headRefName"));
        let pr = json!({ "id": "P1", "number": 14, "title": "t", "url": "u", "isDraft": false, "state": "OPEN", "updatedAt": "2026-01-01T00:00:00Z",
            "author": null, "repository": { "nameWithOwner": "acme/two" }, "reviewDecision": null, "comments": { "totalCount": 0 },
            "commits": { "nodes": [] }, "headRefName": "feat", "labels": { "nodes": [{ "name": "bug" }] } });
        let data = json!({ "r0": { "nameWithOwner": "acme/one", "pullRequest": null }, "r1": { "nameWithOwner": "acme/two", "pullRequest": pr } });
        let hits = parse_number_hits(&data, 2);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].number, hits[0].head_ref_name.as_str(), hits[0].labels.iter().next().unwrap().name.as_str()), (14, "feat", "bug"));
    }

    #[test]
    fn list_rows_carry_stack_fields() {
        assert!(SEARCH_QUERY.contains("stack { id number size baseRefName } stackEntry { position }"));
        assert!(SEARCH_QUERY.contains("first:$first"));
        assert!(!SEARCH_QUERY.contains("entries("), "entries in the list query would double its cost");
        let pr = json!({ "id": "P2", "number": 194, "title": "t", "url": "u", "isDraft": false, "state": "MERGED", "updatedAt": "2026-09-12T11:53:37Z",
            "author": null, "repository": { "nameWithOwner": "stealth-factory/opscope" }, "reviewDecision": null, "comments": { "totalCount": 0 },
            "commits": { "nodes": [] }, "headRefName": "feature/ops-98", "baseRefName": "feature/ops-96", "isCrossRepository": false,
            "stack": { "id": "PRS_kwDOT3dPIs4AEFq1", "number": 196, "size": 2, "baseRefName": "main" }, "stackEntry": { "position": 2 } });
        let p: PrSummary = serde_json::from_value(pr).unwrap();
        assert_eq!((p.stack.as_ref().unwrap().number, p.stack.as_ref().unwrap().size, p.stack_entry.as_ref().unwrap().position), (196, 2, 2));
        assert_eq!(p.base_ref_name, "feature/ops-96");
        // An unstacked row (stack: null) and an old shape without the fields both parse.
        let plain = json!({ "id": "P1", "number": 1, "title": "t", "url": "u", "isDraft": false, "state": "OPEN", "updatedAt": "x",
            "author": null, "repository": { "nameWithOwner": "a/b" }, "reviewDecision": null, "comments": { "totalCount": 0 }, "commits": { "nodes": [] }, "stack": null });
        let p: PrSummary = serde_json::from_value(plain).unwrap();
        assert!(p.stack.is_none() && p.base_ref_name.is_empty());
    }

    #[test]
    fn detail_query_asks_for_the_whole_stack() {
        assert!(DETAIL_QUERY.contains("entries(first:100)"));
        assert!(DETAIL_QUERY.contains("defaultBranchRef { name }"));
    }

    #[test]
    fn compare_links_query_and_parse() {
        let pairs = vec![("feat/universal-search".to_string(), "feat/release".to_string()), ("feat/gone".to_string(), "feat/universal-search".to_string())];
        let q = compare_query("stealth-factory/gh-pr-manager", &pairs);
        assert!(q.contains(r#"repository(owner:"stealth-factory", name:"gh-pr-manager")"#), "{q}");
        assert!(q.contains(r#"l0: ref(qualifiedName:"refs/heads/feat/universal-search") { compare(headRef:"feat/release") { aheadBy behindBy status } }"#), "{q}");
        assert!(q.contains("l1: ref("));
        // As GitHub answered for #2 of the chain; l1's branch was deleted (NOT_FOUND, null).
        let data = json!({ "repository": { "l0": { "compare": { "aheadBy": 2, "behindBy": 1, "status": "DIVERGED" } }, "l1": null } });
        let links = parse_links(&data, &pairs);
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].child.as_str(), links[0].parent.as_str(), links[0].ahead, links[0].behind), ("feat/universal-search", "feat/release", 2, 1));
        assert_eq!(crate::stacks::needs_rebase(&links, "feat/universal-search"), Some(2));
    }

    #[test]
    fn path_encoding_keeps_slashes() {
        assert_eq!(path_encode("feat/a b#1"), "feat/a%20b%231");
    }

    #[test]
    fn merge_async_replies() {
        use reqwest::StatusCode as S;
        let body = merge_async_body("abc", MergeMethod::Squash);
        assert_eq!(body, json!({ "sha": "abc", "merge_method": "squash", "merge_action": "default" }));
        let pending = json!({ "status": "pending", "details": { "message": "Merge request accepted", "uuid": "u-1", "merge_method": "squash",
            "merge_action": "default", "expected_head_sha": "abc" } });
        assert_eq!(parse_merge_job(S::ACCEPTED, &pending).unwrap(), MergeJob { status: "pending".into(), uuid: Some("u-1".into()), message: Some("Merge request accepted".into()) });
        // 409: another request is pending; follow its uuid.
        assert_eq!(parse_merge_job(S::CONFLICT, &pending).unwrap().uuid.as_deref(), Some("u-1"));
        let merged = json!({ "status": "merged", "details": { "message": "Pull request merged", "sha": "def" } });
        assert_eq!(parse_merge_job(S::OK, &merged).unwrap().status, "merged");
        let failed = json!({ "status": "failed", "details": { "message": "Required status check \"ci\" is failing" } });
        let job = parse_merge_job(S::OK, &failed).unwrap();
        assert_eq!(crate::stacks::merge_step(&job, std::time::Duration::ZERO), crate::stacks::MergeStep::Failed("Required status check \"ci\" is failing".into()));
        // The documented shape: `details.error`.
        let failed = json!({ "status": "failed", "details": { "uuid": "u-1", "error": "Merge conflict" } });
        assert_eq!(parse_merge_job(S::OK, &failed).unwrap().message.as_deref(), Some("Merge conflict"));
        let draft = json!({ "message": "Pull request is in draft state", "documentation_url": "x" });
        assert_eq!(parse_merge_job(S::BAD_REQUEST, &draft).unwrap_err().to_string(), "HTTP 400: Pull request is in draft state");
    }

    #[test]
    fn create_stack_replies() {
        use reqwest::StatusCode as S;
        assert_eq!(parse_created_stack(S::CREATED, &json!({ "id": 1, "number": 201, "base": { "ref": "main" }, "open": true })).unwrap(), 201);
        let not_enabled = json!({ "message": "Validation Failed", "errors": [{ "message": "Stacked pull requests are not enabled for this repository" }] });
        let e = parse_created_stack(S::UNPROCESSABLE_ENTITY, &not_enabled).unwrap_err().to_string();
        assert!(e.starts_with("Stacked PRs are not enabled for this repository"), "{e}");
        assert!(e.contains("HTTP 422: Validation Failed (Stacked pull requests are not enabled"), "{e}");
        let gap = json!({ "message": "Validation Failed", "errors": ["Pull request #3 base ref does not match the head ref of #2"] });
        assert_eq!(parse_created_stack(S::UNPROCESSABLE_ENTITY, &gap).unwrap_err().to_string(), "HTTP 422: Validation Failed (Pull request #3 base ref does not match the head ref of #2)");
        assert!(parse_created_stack(S::CREATED, &json!({})).is_err());
    }

    #[tokio::test]
    async fn update_branch_falls_back_to_merge() {
        let calls = std::sync::Mutex::new(Vec::new());
        let msg = rebase_or_merge(|m| {
            calls.lock().unwrap().push(m);
            async move { if m == UpdateMethod::Rebase { Err(anyhow!("Rebase not possible: merge conflict")) } else { Ok(()) } }
        })
        .await
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![UpdateMethod::Rebase, UpdateMethod::Merge]);
        assert_eq!(msg, "Rebase refused (Rebase not possible: merge conflict); merged the base in instead");
        assert_eq!(rebase_or_merge(|_| async { Ok(()) }).await.unwrap(), "Branch rebased onto its base");
        // Only a refused rebase falls back; anything else is reported without a merge attempt.
        let calls = std::sync::Mutex::new(Vec::new());
        let e = rebase_or_merge(|m| {
            calls.lock().unwrap().push(m);
            async { Err(anyhow!("head moved")) }
        })
        .await
        .unwrap_err();
        assert_eq!(format!("{e:#}"), "couldn't rebase the branch: head moved");
        assert_eq!(*calls.lock().unwrap(), vec![UpdateMethod::Rebase]);
        let e = rebase_or_merge(|m| async move { Err(anyhow!(if m == UpdateMethod::Rebase { "Rebase not possible" } else { "conflict" })) }).await.unwrap_err();
        assert_eq!(e.to_string(), "couldn't update the branch: rebase: Rebase not possible; merge: conflict");
    }
}
