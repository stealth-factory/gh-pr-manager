# Actions tab — design

A seventh detail tab that drills into GitHub Actions for the PR: workflow runs → jobs → steps → logs and
annotations. Everything below was validated read-only with the authenticated `gh` CLI (account `wiiiimm`,
scopes `repo read:org workflow gist`) against `stealth-factory/opscope` PR #439 (head `bb33b53`), the failed
run 38370397836 (PR for `ops-67`), and live in-progress runs on `stealth-factory/form-atlas`.

## 1. Purpose and relation to Checks

Keep both tabs. **Checks** stays what it is: the merge-gating `statusCheckRollup` — every check run *and*
commit status on the head commit, including non-Actions apps (Vercel, CodeQL, commit statuses), one line
each, click to open. **Actions** answers "why is it red / what is it doing": per-run attempts, per-job steps
with durations, failure annotations, and the job log rendered in the terminal with folding and search. Checks
needs no new request; Actions costs REST calls, so it loads lazily like Changes.

- Name: `Actions`. `Tab::Actions` sits after `Checks` (related) and before `Reviews`, so `Tab::ALL` has 7
  entries, keys `1`–`7` (`'1'..='7'` in `on_key`; Reviews moves to `7` — acceptable at 0.1.0; append at 7
  instead if muscle memory matters more). Tab-bar label: ` 6 Actions 4 ✗ ` — run count plus the worst
  conclusion icon, via `ci_icon` on the uppercased REST conclusion.

## 2. Data (REST, all on the `core` 5000/h bucket; GraphQL is not used)

| Call (all `GET`, `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`) | Measured |
|---|---|
| `/repos/{nwo}/actions/runs?head_sha={headRefOid}&per_page=100` | 0.59–0.79 s; ~13 KB per run (4 runs = 54 KB) |
| `/repos/{nwo}/actions/runs/{run_id}/jobs?per_page=100` (jobs include `steps[]`) | 0.52–0.81 s; ~4 KB per job |
| `/repos/{nwo}/actions/jobs/{job_id}/logs` → 302 to `productionresultssa*.blob.core.windows.net` | 1.7–2.2 s; 57 KB (failed debian job, 699 lines) to 199 KB (test job, 2208 lines, longest line 256 cols) |
| `/repos/{nwo}/check-runs/{job_id}/annotations?per_page=100` (job id == check-run id, verified via `check_run_url`) | 0.44 s; 374 B |
| `/repos/{nwo}/actions/runs?branch={headRefName}&event=pull_request&per_page=30` (history, opt-in) | uncapped on `release`: 333 runs, 1.6 MB, 3.3 s — hence the cap |
| `/repos/{nwo}/actions/workflows?per_page=1` (only when the run list is empty) | 0.4 s |

Findings that shape the design:

- Filter by `head_sha`, not `branch`/`pull_requests`. `head_sha` returned all 4 runs for `bb33b53`, including
  two `event: "dynamic"` CodeQL runs that `event=pull_request` hides; `branch=` fails for fork PRs (the branch
  lives in the fork), and the `pull_requests[]` array on a run is empty once the PR is closed/merged.
- Attempts are real: run 39004921858 attempt 1 concluded `action_required` (Copilot's push awaited approval);
  wiiiimm re-ran it as attempt 2. The jobs endpoint returns the latest attempt only (`filter=latest`);
  `/attempts/1/jobs` returned 0 jobs because attempt 1 never ran. Show `attempt N`; older attempts are "later".
- Logs exist only after the job completes: for an in-progress job the API still 302s but the blob answers
  `404 BlobNotFound`. Steps, however, stream live via the jobs endpoint (`in_progress` / `pending` per step).
- Log format: UTF-8 BOM, then every line is `2026-10-13T08:36:27.4323154Z ` + text. Markers seen:
  `##[group]…`/`##[endgroup]` (30–32 per job, headers are the *command*, e.g. `##[group]Run cargo test`, not
  the step name), `##[error]Process completed with exit code 1.`; also possible: `##[warning] ##[notice]
  ##[debug] ##[command] ##[section]`. ANSI is SGR only: `0m 1m 36;1m 92m 94m 33m` (95–143 lines per job).
- Step ↔ log mapping: match each line's timestamp into `[step.started_at, step.completed_at]` (second
  precision). Name matching is unreliable (see above); the run-level `/logs` zip has per-step files but needs
  a zip crate — not worth it.
- reqwest 0.13.5 follows the 302 and `remove_sensitive_headers` strips `Authorization` cross-host (verified in
  `redirect.rs`), which the pre-signed blob URL requires. Keep the default redirect policy.
- GraphQL `commit.checkSuites{workflowRun checkRuns{steps annotations}}` was rejected: 3.75 s, rate cost 51,
  and 26 suites of which 21 are phantom `QUEUED` suites from installed apps (travis, vercel, netlify, …) with
  no check runs. Useful fact: GraphQL `CheckRun.databaseId` == REST job id.
- "Not using Actions": the runs call returns `200 {total_count: 0}` (verified on `wiiiimm/awesome-ai-plugins`);
  `/actions/workflows` `total_count: 0` distinguishes "no workflows" from "no runs for this commit". Actions
  disabled or no access → 403/404 with a `message`.
- Permissions: reads need `repo` on a classic token for private repos (fine-grained: *Actions: read*); public
  repos need nothing. `workflow` is not needed for reading. The gh token qualifies.

## 3. UX

Two columns like Changes (same 32 % / 24–56 col split, `│` divider). Left: a tree of runs and jobs. Right:
the selected node — run header, or job steps + annotations + log. Comfortable adds a blank row after each
run in the tree and uses `gap()` between the right-pane sections; Compact collapses both to zero.

```
 4 runs · 2 ✗ 1 ● 1 ✓          │ ✗ ci  #1125 · attempt 2 · pull_request · bb33b53 · 2m 9s · wiiiimm
▌✗ ci              #1125 2m09s │ failed 1 of 3 jobs · opened 4h ago                      [O browser]
   ✗ debian               4m39s│
   ✓ test                 1m22s│ ── Jobs ───────────────────────────────────────────────────────
   ✓ macos                1m27s│  ✗ debian   4m39s   failed at step 6 "Build a manifest-version…"
 ● E2E             #88  running│  ✓ test     1m22s
 ✓ pr-title        #1192    8s │  ✓ macos    1m27s
 ✓ Code Quality: PR #439   51s │
                               │ (Enter on a run folds/unfolds its jobs; Enter on a job shows it)
```

```
▌✗ debian                 4m39s│ ✗ debian · ubuntu-22.04 · 4m39s · attempt 2            [O browser]
   …                           │ ── Steps ──────────────────────────────────────────────────────
                               │  ✓ 1 Set up job                                            1s
                               │  ✓ 2 Run actions/checkout@fbc6f39…                         2s
                               │  ✗ 6 Build a manifest-version test package (never dev)  4m18s  ← click
                               │  ○ 7 Post Run Swatinem/rust-cache                     skipped
                               │ ── Annotations (1) ────────────────────────────────────────────
                               │  ✗ .github:200  Process completed with exit code 1.
                               │ ── Log · 699 lines · 1 error · tail ──────────────── /search  t ts
                               │ ▸ Run actions/checkout@fbc6f39…                     (203 lines)
                               │ ▾ Run cargo deb --no-build …                   step 6 · 4m18s
                               │   08:41:03  error: failed to run custom build command
                               │   08:41:04  ##[error]Process completed with exit code 1.   ◀ E
                               │ ▸ Post Run actions/checkout@fbc6f39…                  (4 lines)
```

Icons reuse `ci_icon` (`✓ ✗ ● ○ ·`); REST values map by uppercasing (`in_progress`→yellow `●`,
`action_required`/`startup_failure`/`timed_out`/`cancelled`→red `✗`, `skipped`/`neutral`/`stale`→dim `○`,
`queued`/`waiting`/`pending`→yellow `●`). Durations: `completed_at − started_at` as `4m39s`/`1h02m`; a
running job shows elapsed since `started_at`, recomputed each frame (the 200 ms tick keeps it moving).
Each run row carries `#run_number`, attempt (only when > 1), `event`, short `head_sha`, `triggering_actor`;
in history mode runs from older pushes are grouped under a dim `── earlier push 230ff22 ──` row.

Keys (detail pane focused, Actions tab):

| Key | Does |
|---|---|
| `↑ ↓` / `n p` / click | Move the tree cursor (mirrors Changes; `j k` keep scrolling the right pane) |
| `Enter` / click | Run: fold/unfold its jobs (loads jobs on first unfold). Job: show steps + load log and annotations |
| `O` | Open the selected run/job `html_url` (`o` still opens the PR) |
| `H` | Toggle history: runs from earlier pushes on this branch (same-repo PRs only; fork PRs: "later") |
| `E` / `W` | Jump to the next `##[error]` or annotation / next `##[warning]` (wraps; toast when none) |
| `z` / `Z` | Fold/unfold the group under the cursor line / all groups |
| `t` | Toggle timestamps (`HH:MM:SS`, UTC, from the line prefix) |
| `/` | Search within the shown log (replaces the list filter *only* while a log is shown); `Enter` jumps to the first hit, `n`/`N` step hits while a query is set, `Esc` clears the query |
| `g G` | Top / bottom of the log (opens at the bottom: tail first) |
| `Esc` | Clears search first, then closes the log back to the run, then (existing) leaves the detail view |
| `r` | Existing refresh also forces runs/jobs/log of the selection |

Mouse: wheel over the tree scrolls it (like `hits.file_list`); wheel over the right pane scrolls it; clicking
a `▸/▾` group header toggles that fold; clicking a step row jumps to its log section; clicking an annotation
jumps to the error line; clicking the run/job title opens the browser. Internal clicks use the `DetailRow`
link mechanism with a `#` prefix (`#fold:12`, `#line:688`, `#job:117476249069`); `Action::OpenUrl` routes
`#…` links to `Action::Internal(String)` instead of `open_url` (reuse this convention if another change already
introduced it).

Log rendering: strip the BOM; show the step name as a dim right-aligned suffix on the group that starts the
step (timestamp mapping); fold every group by default except the one containing the first error and the last
group, show `(n lines)` on folded headers; `##[error]` rows red, `##[warning]` yellow, `##[notice]` cyan,
`##[debug]` dim, `##[command]` bold; ANSI SGR rendered (bold/dim/italic/underline, fg 30–37/90–97, `38;5;n`,
`38;2;r;g;b`, reset), every other CSI/OSC stripped. No soft wrap: long lines are truncated to the pane with
`…` (search still matches the full text). Caps: keep the last 4 MiB / 200 000 lines of the body, drop whole
lines from the front, and show `… 12 345 earlier lines dropped — O opens the full log` as the first row.

States (right pane, all spinner/colour conventions as in `draw_detail`):
- loading: `⠋ Loading workflow runs…`; a job's log: `⠋ Fetching log (199 KB)…`
- running job: steps live; in place of the log `Log appears when the job finishes (polling every 10 s)`
- empty: `No workflow runs for bb33b53` + dim `H shows runs from earlier pushes`
- not using Actions (`total_count == 0` and zero workflows): `This repository doesn't use GitHub Actions`
- error (403/404/network): red `Couldn't load workflow runs: HTTP 403: Resource not accessible…` + `press r to retry`

## 4. Actions on runs — later, gated

`POST /repos/{nwo}/actions/runs/{id}/rerun-failed-jobs`, `POST …/runs/{id}/rerun`, `POST …/runs/{id}/cancel`
(all `202`; classic token needs `repo`, fine-grained *Actions: write*). Expose them only through the existing
More menu (`␣`/`.`) when the Actions tab is active and a run is selected, each via `Modal::Confirm` with new
`PendingOp::RerunFailed(run_id) | RerunAll(run_id) | CancelRun(run_id)`, disabled with a reason unless
`DetailRepo::can_write()` and the run state allows it (rerun: `completed`; cancel: `queued|in_progress`). On
`Done`, force-refresh runs. Ship v1 read-only; none of these were called during validation.

## 5. Refresh

- Runs are stale after 60 s (like files) and are re-fetched by `r` and the `refresh_secs` sweep via
  `ensure_runs(force)`, which mirrors `ensure_files` and bails unless `screen == Detail && tab == Actions`.
- While the tab is visible and any loaded run has `status != "completed"`, `on_tick` polls every 10 s: the run
  list, plus the jobs of the selected/unfolded run (≤ 2 calls / 10 s = 720/h worst case). Stop when all runs
  complete; when a polled job flips to `completed`, fetch its log if it is the shown one. Back off to 30 s when
  the REST `x-ratelimit-remaining` < 500 and stop polling below 100 (read the header in the shared REST helper;
  keep it separate from the GraphQL `app.rate`).
- A new push changes `head_ref_oid`; caches are keyed by `(pr id, head sha)` so the next `ensure_runs` fetches
  fresh and the old entry is dropped.

## 6. Rust-level additions

**`src/github.rs`**
- `async fn rest_get(&self, url: &str) -> Result<(reqwest::Response, Option<RateLimit>)>` — bearer, Accept,
  API-version header, HTTP-error → `bail!("HTTP {status}: {message}")`, rate from `x-ratelimit-*`; refactor
  `pr_files` onto it.
- `pub async fn workflow_runs(&self, nwo: &str, q: RunsQuery) -> Result<Vec<WorkflowRunInfo>>` where
  `enum RunsQuery<'a> { HeadSha(&'a str), Branch { name: &'a str, per_page: u8 } }`.
- `pub async fn run_jobs(&self, nwo: &str, run_id: u64) -> Result<Vec<Job>>` (paginate `per_page=100`, ≤ 5 pages).
- `pub async fn job_log(&self, nwo: &str, job_id: u64, cap: usize) -> Result<JobLog>`; blob 404 → `JobLog { pending: true, .. }`.
- `pub async fn job_annotations(&self, nwo: &str, job_id: u64) -> Result<Vec<Annotation>>`.
- `pub async fn workflow_count(&self, nwo: &str) -> Result<usize>`.
- Structs (serde, exact REST names): `WorkflowRunInfo { id: u64, name: String, display_title: String,
  status: String, conclusion: Option<String>, event: String, run_number: u64, run_attempt: u64, head_sha: String,
  head_branch: Option<String>, path: String, html_url: String, created_at: String, updated_at: String,
  run_started_at: Option<String>, actor: Option<Login>, triggering_actor: Option<Login> }`;
  `Job { id: u64, run_id: u64, run_attempt: u64, name: String, status: String, conclusion: Option<String>,
  started_at: Option<String>, completed_at: Option<String>, html_url: String, runner_name: Option<String>,
  labels: Vec<String>, steps: Vec<Step> }`; `Step { name: String, number: u64, status: String,
  conclusion: Option<String>, started_at: Option<String>, completed_at: Option<String> }`;
  `Annotation { path: String, start_line: Option<u64>, end_line: Option<u64>, annotation_level: String,
  title: Option<String>, message: String }`; `JobLog { text: String, dropped_bytes: usize, pending: bool }`.
- `DETAIL_QUERY`: add `isCrossRepository` (→ `PrDetail.is_cross_repository`, gates `H`) and `oid` on commits.

**`src/app.rs`**
- `Tab::Actions`, `Tab::ALL: [Tab; 7]`, label `"Actions"`, keys `'1'..='7'`; `layout_key` adds `actions_sel`.
- Fields: `runs: HashMap<String, RunsEntry>` (`RunsEntry { sha: String, history: bool, runs: Vec<WorkflowRunInfo>, at: Instant }`),
  `runs_loading: HashSet<String>`, `runs_errors: HashMap<String, String>`, `no_workflows: HashSet<String /*nwo*/>`,
  `jobs: HashMap<u64, (Vec<Job>, Instant)>`, `jobs_loading: HashSet<u64>`,
  `logs: HashMap<u64, (ParsedLog, Instant)>`, `logs_loading: HashSet<u64>`, `log_errors: HashMap<u64, String>`,
  `annotations: HashMap<u64, Vec<Annotation>>`, `actions_sel: usize`, `actions_tree_scroll: usize`,
  `actions_expanded: HashSet<u64>`, `actions_history: HashSet<String>`, `log_view: LogView` (`folds: HashSet<usize>`,
  `timestamps: bool`, `search: Option<LogSearch { query, input: bool, hits: Vec<usize>, cur: usize }>`, `visible: Vec<usize>`),
  `actions_gen: u64` (bumped on PR/head change; every Actions `Msg` carries it and stale ones are dropped),
  `actions_poll_at: Instant`.
- `Msg::Runs { generation, id, res }`, `Msg::Jobs { generation, run_id, res }`, `Msg::Log { generation, job_id, res }`,
  `Msg::Annotations { generation, job_id, res }`.
- `Action::TreeNext | TreePrev | TreeToggle | TreeOpenBrowser | ToggleHistory | LogNextError | LogNextWarning |
  LogFold | LogFoldAll | LogTimestamps | LogSearch | LogNextHit | LogPrevHit | Internal(String)`.
- `fn ensure_runs(&mut self, force: bool)`, `fn ensure_jobs(run_id)`, `fn ensure_log(job_id)`, `fn tree(&self) -> Vec<TreeNode>`
  (`enum TreeNode { Push(sha), Run(idx), Job(run_id, idx) }` flattened in display order), `fn poll_actions()` from `on_tick`.
- `Hits`: `actions_tree: Option<(Rect, usize)>` (same shape as `file_list`), `log: Option<Rect>` with `log_max_scroll`.

**`src/ui.rs`**
- `draw_actions(f, app, id, area)` (tree + right pane; called from `draw_detail` like `draw_changes`),
  `run_tree_lines`, `run_header_lines`, `jobs_summary_lines`, `steps_lines`, `annotation_lines`,
  `render_log(f, area, &ParsedLog, &LogView, scroll)` — styles only the visible window (`visible[scroll..scroll+h]`),
  so 200 k-line logs cost O(height) per frame rather than going through `tab_cache`/`render_rows`,
  `fmt_duration(secs: i64) -> String`, `rest_outcome(status, conclusion) -> String` (feeds `ci_icon`),
  `draw_tab_bar` count/icon for `Tab::Actions`, new rows in `draw_help` and the detail footer (`H`, `/`, `E`).
- Make `parse_ts` `pub(crate)` (or move it) so the log parser can reuse it.

**New `src/actions_log.rs`** (pure, no I/O)
- `pub struct ParsedLog { lines: Vec<LogLine>, groups: Vec<Group>, errors: Vec<usize>, warnings: Vec<usize>, dropped_lines: usize }`,
  `LogLine { ts: Option<i64>, kind: Kind, text: String /* ANSI kept */, group: Option<usize>, step: Option<usize> }`,
  `enum Kind { Plain, Group, EndGroup, Error, Warning, Notice, Debug, Command, Section }`, `Group { start, end, title, has_error }`.
- `pub fn parse(text: &str) -> ParsedLog`; `pub fn assign_steps(&mut ParsedLog, &[Step])` (by timestamp);
  `pub fn tail(bytes: &[u8], cap: usize) -> (&[u8], usize)`; `pub fn ansi_spans(text: &str, base: Style) -> Vec<Span<'static>>`;
  `pub fn strip_ansi(text: &str) -> String` (for search/width); `pub fn visible_lines(&ParsedLog, folds) -> Vec<usize>`;
  `pub fn search(&ParsedLog, query) -> Vec<usize>` (case-insensitive on stripped text).
- Tests: BOM stripped; timestamp prefix parsed and the text de-prefixed; `##[group]`/`##[endgroup]` pairing,
  an unterminated group closes at EOF, a stray `##[endgroup]` is ignored; `##[error]` sets `Kind::Error` and
  `Group.has_error`; `\x1b[36;1mcmd\x1b[0m` → cyan+bold span then reset; `\x1b[92m` bright green; `38;5;196`
  indexed; `\x1b[2K` and `\x1b]8;;url\x1b\\` stripped; unterminated escape at EOL stripped; `tail` keeps whole
  lines and counts dropped ones; `assign_steps` with a line before the first step and a running step without
  `completed_at`; `visible_lines` with a folded group yields only its header; `search` ignores ANSI and case;
  `fmt_duration(0/59/61/3661)`; `rest_outcome("completed", Some("action_required")) == "ACTION_REQUIRED"` and
  `("in_progress", None) == "IN_PROGRESS"`.

## Implementation notes

Deviations from the design above, as built:

- **Search is `Ctrl+F`, not `/`.** `/` stays the list filter (it's reserved for the coming universal search).
  `Ctrl+F` opens the query in the status bar (live match count), `Enter` jumps to the next match after the
  cursor, `n`/`N` step matches while a query is set (otherwise `n`/`p` move the tree), `Esc` clears.
- **Internal links** go through `Action::OpenUrl`: a `#`-prefixed URL becomes `Action::Internal` (`fold:<group>`,
  `line:<line>`, `job:<id>`) instead of `open_url`. No such convention existed before.
- **One scroll for the right pane.** Header, steps, annotations and the log share `detail_scroll`, so `j k`,
  wheel, `g G` and PgUp/PgDn work unchanged; only the rows on screen are built (`hits.detail_links_from` maps
  clicks onto them). There is no separate `hits.log` / `log_max_scroll`.
- **Selecting a job shows its steps and annotations; the log loads on `Enter`/click** (it's the costly call).
  A running job's log is fetched automatically once polling sees it complete. `z` and `E`/`W` work from the
  last jump target, else from the top log line on screen.
- **No `actions_gen`.** Runs are keyed by `(pr id, head sha)` and replies for another PR are stored but not
  acted on; jobs, logs and annotations are keyed by their globally unique ids, so stale replies are harmless.
- **REST rate limit** lives on `Client` (`rest_rate()`), updated by `rest_send`/`rest_get`, rather than being
  returned with every response. It isn't shown in the header (which shows the GraphQL bucket).
- **Tree:** runs carry a `▸/▾` fold glyph; on first load the head commit's failing or running runs (up to 3)
  are unfolded and the cursor starts on the first. Clicking a run folds/unfolds it; clicking a job shows its log.
- **Info header:** Actions shares Changes' collapsed-by-default info state (both are two-column tabs).
- `LogLine` also keeps `plain` (ANSI stripped) for search and width; `ParsedLog.step_first` maps steps to their
  first line for step clicks. Error/warning/notice rows render as `Error: …` like github.com, not `##[error]…`.
- `WorkflowRunInfo.name`/`status` are `Option` (the API can return null); unused fields (`head_branch`,
  `runner_name`, `end_line`) and the `oid` on commits were left out. `JobLog` reports `dropped_lines`, not bytes.
- Run/job actions (re-run, cancel) are not implemented, as planned for v1.
