//! Application state and input handling.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{ListState, TableState};
use tokio::sync::mpsc::UnboundedSender;

use crate::actions_log::{self, ParsedLog};
use crate::config::Config;
use crate::editor::{Editor, VisualRow};
use crate::images::{ImageKey, ImageMsg, ImagePlacement, ImageStore};
use crate::github::{
    Annotation, Client, FileDiff, Job, MergeMethod, PrDetail, PrSummary, RateLimit, RepoInfo, RepoPage, ReviewEvent, RunsQuery, SearchPage, Viewer,
    WorkflowRunInfo,
};
use crate::state::{ReposCache, State};

/// Maximum PRs loaded per scope (GitHub search caps at 1000).
const MAX_PRS: usize = 300;
/// Maximum repos listed in the repo picker.
const MAX_REPOS: usize = 1000;
/// Bytes kept from the end of a job log.
const LOG_CAP: usize = 4 << 20;
/// Seconds between Actions polls while runs are in progress (and when the REST budget is low).
pub const POLL_SECS: u64 = 10;
const POLL_SECS_LOW: u64 = 30;
/// Seconds before asking again for a finished job's log that wasn't there yet.
const LOG_RETRY_SECS: u64 = 5;

/// A rendered detail row plus its clickable link ranges (start col, end col, url).
pub type DetailRow = (ratatui::text::Line<'static>, Vec<(u16, u16, String)>);
/// A built block of rows and the inline images placed in it.
pub type Built = (Vec<DetailRow>, Vec<ImagePlacement>);

pub enum Msg {
    Input(Event),
    Viewer(Result<(Viewer, Option<RateLimit>)>),
    /// One page of search results; `first` replaces the list, later pages append.
    Prs { generation: u64, first: bool, res: Result<SearchPage> },
    Detail { id: String, res: Result<(PrDetail, Option<RateLimit>)> },
    Files { id: String, sha: String, res: Result<Vec<FileDiff>> },
    /// Workflow runs for a PR's head (`history` adds earlier pushes); the flag is set when
    /// the list is empty because the repo has no workflows at all.
    Runs { id: String, nwo: String, sha: String, history: bool, res: Result<(Vec<WorkflowRunInfo>, bool)> },
    Jobs { run_id: u64, nwo: String, res: Result<Vec<Job>> },
    /// A parsed job log; `true` when the job hasn't produced one yet.
    Log { job_id: u64, res: Result<(ParsedLog, bool)> },
    Annotations { job_id: u64, res: Result<Vec<Annotation>> },
    /// A page of the viewer's repos for the repo picker; `done` when no more pages follow.
    Repos { first: bool, done: bool, res: Result<RepoPage> },
    /// A mutation finished; `Ok` carries the success message.
    Done { id: String, res: Result<String> },
    /// An inline image was downloaded and decoded, or encoded for another size.
    Image(ImageMsg),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub name: String,
    pub qualifier: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFilter {
    Open,
    Closed,
    Merged,
    All,
}

impl StateFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Closed => "Closed",
            Self::Merged => "Merged",
            Self::All => "All",
        }
    }

    fn qualifier(self) -> &'static str {
        match self {
            Self::Open => "is:open",
            Self::Closed => "is:closed is:unmerged",
            Self::Merged => "is:merged",
            Self::All => "",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::Merged,
            Self::Merged => Self::All,
            Self::All => Self::Open,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Detail,
}

/// Tabs under the PR info + description in the detail pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Description,
    Conversation,
    Changes,
    Commits,
    Checks,
    Actions,
    Reviews,
}

impl Tab {
    pub const ALL: [Tab; 7] = [Tab::Description, Tab::Conversation, Tab::Changes, Tab::Commits, Tab::Checks, Tab::Actions, Tab::Reviews];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Description => "Description",
            Tab::Conversation => "Conversation",
            Tab::Changes => "Changes",
            Tab::Commits => "Commits",
            Tab::Checks => "Checks",
            Tab::Actions => "Actions",
            Tab::Reviews => "Reviews",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Everything a key press or a click can trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Back,
    Refresh,
    OpenScopes,
    CycleDensity,
    OpenRepos,
    SetRepo(Option<String>),
    ToggleRepoOfSelected,
    CycleState,
    StartFilter,
    ClearFilter,
    OpenSelected,
    FocusSidebar,
    FocusDetail,
    ToggleFocus,
    Comment,
    Menu,
    MergeMenu,
    Approve,
    RequestChanges,
    CloseOrReopen,
    Browser,
    CopyCheckout,
    Help,
    ToggleImages,
    /// Open the image viewer (clicking an inline image).
    ViewImage(ImageKey),
    OpenUrl(String),
    SetTab(Tab),
    NextTab,
    PrevTab,
    NextFile,
    PrevFile,
    ToggleInfo,
    // Actions tab
    TreeNext,
    TreePrev,
    TreeToggle,
    TreeOpenBrowser,
    ToggleHistory,
    ActionsBack,
    LogNextError,
    LogNextWarning,
    LogFold,
    LogFoldAll,
    LogTimestamps,
    LogSearch,
    LogNextHit,
    LogPrevHit,
    /// A `#…` link handled inside the app: `fold:<group>`, `line:<line>`, `job:<id>`, `image:<key>`.
    Internal(String),
    // Modal-level actions
    ModalPick(usize),
    ModalCancel,
    ConfirmYes,
    Submit,
    InsertAt,
    AcceptMention(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeKind {
    Comment,
    Approve,
    RequestChanges,
}

pub struct Composer {
    pub pr_id: String,
    pub pr_label: String,
    pub kind: ComposeKind,
    pub editor: Editor,
    pub popup_sel: usize,
    /// Mention prefix the user dismissed with Esc, so the popup stays closed until it changes.
    pub popup_dismissed: Option<(usize, String)>,
    pub submitting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Merge(MergeMethod),
    Comment,
    Approve,
    RequestChanges,
    Close,
    Reopen,
    Browser,
    CopyCheckout,
    CopyUrl,
    ToggleRepo,
}

#[derive(Debug, Clone)]
pub struct MenuEntry {
    pub item: MenuItem,
    pub label: String,
    pub key: char,
    pub disabled: Option<String>,
}

#[derive(Debug, Clone)]
pub enum PendingOp {
    /// Merge method plus the head commit the user confirmed.
    Merge(MergeMethod, String),
    Close,
    Reopen,
    Approve,
}

pub enum Modal {
    Scopes { filter: String, sel: usize },
    Repos { filter: String, sel: usize },
    Composer(Box<Composer>),
    Menu { pr_id: String, entries: Vec<MenuEntry>, sel: usize },
    Confirm { pr_id: String, title: String, message: String, op: PendingOp },
    Help,
    /// One image fitted to the screen.
    Image { key: ImageKey, alt: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

/// Click targets recorded during the last render.
#[derive(Default)]
pub struct Hits {
    pub buttons: Vec<(Rect, Action)>,
    pub table: Option<(Rect, usize)>,
    pub sidebar: Option<(Rect, usize)>,
    pub detail: Option<Rect>,
    pub detail_links: Vec<Vec<(u16, u16, String)>>,
    /// X of the detail text column, for mapping clicks onto link ranges.
    pub detail_text_x: u16,
    pub detail_max_scroll: usize,
    pub detail_height: usize,
    /// Changes tab file list: rows area and scroll offset.
    pub file_list: Option<(Rect, usize)>,
    /// Actions tab tree: rows area and scroll offset, plus every row's tree node (blank
    /// spacer and push-header rows have none).
    pub actions_tree: Option<(Rect, usize)>,
    pub actions_tree_rows: Vec<Option<usize>>,
    /// Absolute row of `detail_links[0]` (the Actions pane only records the rows on screen).
    pub detail_links_from: usize,
    /// First job-log line on screen, for `z` and `E` when nothing was jumped to yet.
    pub log_top_line: Option<usize>,
    /// The PR info + description region above the tabs.
    pub info: Option<Rect>,
    pub info_links: Vec<Vec<(u16, u16, String)>>,
    pub info_max_scroll: usize,
    pub modal: Option<Rect>,
    pub modal_items: Vec<(Rect, usize)>,
    pub editor: Option<Rect>,
    pub editor_rows: Vec<VisualRow>,
    pub popup_items: Vec<(Rect, usize)>,
}

/// Workflow runs loaded for a PR, for the head commit they were fetched for.
pub struct RunsEntry {
    pub sha: String,
    pub history: bool,
    /// Head-commit runs first, then earlier pushes (newest first), each push's runs together.
    pub runs: Vec<WorkflowRunInfo>,
    pub at: Instant,
}

/// A row of the Actions tree, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeNode {
    /// Heading for the runs of an earlier push (history mode); never selected.
    Push(String),
    /// Index into `RunsEntry::runs`.
    Run(usize),
    /// Run id and index into its jobs.
    Job(u64, usize),
}

/// How the shown job log is folded, searched and stamped.
pub struct LogView {
    pub job: Option<u64>,
    /// When the shown log was fetched, to notice a refetch.
    pub at: Option<Instant>,
    pub folds: HashSet<usize>,
    pub timestamps: bool,
    pub search: Option<LogSearch>,
    /// Line indices on screen given `folds`.
    pub visible: Vec<usize>,
    /// Line the last jump landed on (E/W, search, a step or annotation click).
    pub cursor: Option<usize>,
    /// Line to bring into view on the next frame.
    pub scroll_to: Option<usize>,
}

pub struct LogSearch {
    pub query: String,
    /// Still typing the query.
    pub input: bool,
    pub hits: Vec<usize>,
}

pub struct App {
    pub client: Client,
    pub tx: UnboundedSender<Msg>,
    pub cfg: Config,
    pub viewer: Option<Viewer>,
    pub scopes: Vec<Scope>,
    pub scope: usize,
    pub state_filter: StateFilter,
    /// Optional `owner/name` filter layered on top of the scope.
    pub repo: Option<String>,
    /// The viewer's repos (most recently pushed first), cached across runs.
    pub repos: Vec<RepoInfo>,
    repos_fresh: Vec<RepoInfo>,
    pub repos_total: usize,
    pub repos_loading: bool,
    /// Repos in the last unfiltered list, with PR counts, and the scope and state they count.
    pub scope_repos: Vec<(String, usize)>,
    scope_repos_key: (usize, StateFilter),
    pub state: State,
    /// PR to keep selected across a reload triggered by a filter change.
    pending_select: Option<String>,
    pub prs: Vec<PrSummary>,
    pub total: usize,
    pub list_error: Option<String>,
    pub loading_list: bool,
    list_gen: u64,
    pub filter: String,
    pub filter_input: bool,
    pub sel: usize,
    pub table_state: TableState,
    pub side_state: ListState,
    pub details: HashMap<String, (PrDetail, Instant)>,
    pub detail_loading: HashSet<String>,
    pub detail_errors: HashMap<String, String>,
    /// Scroll of the active tab's content.
    pub detail_scroll: usize,
    pub info_scroll: usize,
    info_collapsed: bool,
    /// The Changes tab wants the room, so its info section starts collapsed separately.
    changes_info_expanded: bool,
    pub tab: Tab,
    /// Selected file in the Changes tab and the file list's scroll offset.
    pub file_sel: usize,
    pub file_list_scroll: usize,
    /// Changed files per PR id, with the head commit they were fetched for.
    pub files: HashMap<String, (Vec<FileDiff>, Instant, String)>,
    pub files_loading: HashSet<String>,
    pub files_errors: HashMap<String, String>,
    /// Actions tab: runs per PR id, jobs per run id, logs and annotations per job id.
    pub runs: HashMap<String, RunsEntry>,
    pub runs_loading: HashSet<String>,
    pub runs_errors: HashMap<String, String>,
    /// Repos (owner/name) without any workflows.
    pub no_workflows: HashSet<String>,
    pub jobs: HashMap<u64, (Vec<Job>, Instant)>,
    pub jobs_loading: HashSet<u64>,
    pub jobs_errors: HashMap<u64, String>,
    pub logs: HashMap<u64, (ParsedLog, Instant)>,
    pub logs_loading: HashSet<u64>,
    pub log_errors: HashMap<u64, String>,
    /// Completed jobs whose log blob didn't exist yet, and when that was seen; retried shortly.
    pub logs_pending: HashMap<u64, Instant>,
    /// Jobs whose log was asked for (Enter / click); fetched once the job completes.
    pub log_wanted: HashSet<u64>,
    pub annotations: HashMap<u64, Vec<Annotation>>,
    annotations_loading: HashSet<u64>,
    /// Tree cursor (an index into `tree()`) and the tree's scroll offset.
    pub actions_sel: usize,
    pub actions_tree_scroll: usize,
    /// Scroll the tree to the cursor on the next frame (false after wheel scrolling).
    pub actions_tree_follow: bool,
    /// Runs whose jobs are unfolded in the tree.
    pub actions_expanded: HashSet<u64>,
    /// PR ids also showing runs from earlier pushes.
    pub actions_history: HashSet<String>,
    pub log_view: LogView,
    actions_poll_at: Instant,
    /// Rendered tab rows keyed by "id:width:image rows:tab:density", stamped with the data's fetch time.
    pub tab_cache: HashMap<String, (Instant, Built)>,
    /// Rendered rows for (pr id, width, fetched at); rebuilding them per frame is costly.
    pub detail_cache: Option<(String, usize, Instant, Vec<DetailRow>)>,
    pub images: ImageStore,
    /// The images of the tab drawn last ("pr id:tab"), to keep the scroll steady as they load.
    pub last_placements: (String, Vec<ImagePlacement>),
    pub busy: HashSet<String>,
    drafts: HashMap<String, Editor>,
    pub screen: Screen,
    pub focus: Focus,
    pub modal: Option<Modal>,
    pub toast: Option<(String, ToastKind, Instant)>,
    pub rate: Option<RateLimit>,
    pub tick: usize,
    last_refresh: Instant,
    last_click: Option<(Instant, usize)>,
    pub hits: Hits,
    pub quit: bool,
}

impl App {
    pub fn new(client: Client, tx: UnboundedSender<Msg>, cfg: Config, images: crate::images::Detection) -> Self {
        let mut scopes = vec![
            Scope { name: "My PRs".into(), qualifier: "author:@me".into() },
            Scope { name: "Review requested".into(), qualifier: "review-requested:@me".into() },
            Scope { name: "Involves me".into(), qualifier: "involves:@me".into() },
            Scope { name: "Assigned to me".into(), qualifier: "assignee:@me".into() },
            // Only offered while a repo filter is set (an unscoped search is useless).
            Scope { name: "Everyone (all PRs in the repo)".into(), qualifier: String::new() },
        ];
        scopes.extend(cfg.scopes.iter().map(|s| Scope { name: s.name.clone(), qualifier: s.query.clone() }));
        let scope = cfg.default_scope.as_ref().and_then(|d| scopes.iter().position(|s| &s.name == d)).unwrap_or(0);
        let repo = cfg.default_repo.clone().filter(|r| looks_like_repo(r));
        // A repo-only scope without a repo would search all of GitHub.
        let scope = if repo.is_none() && scopes[scope].qualifier.is_empty() { 0 } else { scope };
        let state = crate::state::load();
        // The cache is applied only after `viewer()` confirms it belongs to this login.
        let repos = Vec::new();
        let toast = images.note.map(|n| (n, ToastKind::Info, Instant::now()));
        let images = ImageStore::new(images.picker, state.images_enabled, images.off_reason);
        Self {
            client,
            tx,
            cfg,
            viewer: None,
            scopes,
            scope,
            state_filter: StateFilter::Open,
            repo,
            repos,
            repos_fresh: Vec::new(),
            repos_total: 0,
            repos_loading: false,
            scope_repos: Vec::new(),
            scope_repos_key: (scope, StateFilter::Open),
            state,
            pending_select: None,
            prs: Vec::new(),
            total: 0,
            list_error: None,
            loading_list: false,
            list_gen: 0,
            filter: String::new(),
            filter_input: false,
            sel: 0,
            table_state: TableState::default(),
            side_state: ListState::default(),
            details: HashMap::new(),
            detail_loading: HashSet::new(),
            detail_errors: HashMap::new(),
            detail_scroll: 0,
            info_scroll: 0,
            info_collapsed: false,
            changes_info_expanded: false,
            tab: Tab::Description,
            file_sel: 0,
            file_list_scroll: 0,
            files: HashMap::new(),
            files_loading: HashSet::new(),
            files_errors: HashMap::new(),
            runs: HashMap::new(),
            runs_loading: HashSet::new(),
            runs_errors: HashMap::new(),
            no_workflows: HashSet::new(),
            jobs: HashMap::new(),
            jobs_loading: HashSet::new(),
            jobs_errors: HashMap::new(),
            logs: HashMap::new(),
            logs_loading: HashSet::new(),
            log_errors: HashMap::new(),
            logs_pending: HashMap::new(),
            log_wanted: HashSet::new(),
            annotations: HashMap::new(),
            annotations_loading: HashSet::new(),
            actions_sel: 0,
            actions_tree_scroll: 0,
            actions_tree_follow: true,
            actions_expanded: HashSet::new(),
            actions_history: HashSet::new(),
            log_view: LogView { job: None, at: None, folds: HashSet::new(), timestamps: true, search: None, visible: Vec::new(), cursor: None, scroll_to: None },
            actions_poll_at: Instant::now(),
            tab_cache: HashMap::new(),
            detail_cache: None,
            images,
            last_placements: (String::new(), Vec::new()),
            busy: HashSet::new(),
            drafts: HashMap::new(),
            screen: Screen::List,
            focus: Focus::Sidebar,
            modal: None,
            toast,
            rate: None,
            tick: 0,
            last_refresh: Instant::now(),
            last_click: None,
            hits: Hits::default(),
            quit: false,
        }
    }

    pub fn start(&mut self) {
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Msg::Viewer(client.viewer().await));
        });
        self.load_list();
    }

    // ---------- data loading ----------

    pub fn search_query(&self) -> String {
        let scope = &self.scopes[self.scope];
        let repo = self.repo.as_ref().map(|r| format!("repo:{r}")).unwrap_or_default();
        format!("is:pr archived:false {} {} {repo} sort:updated-desc", self.state_filter.qualifier(), scope.qualifier)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn load_list(&mut self) {
        // Counts describe the unfiltered list they came from; another scope or state voids them.
        if self.scope_repos_key != (self.scope, self.state_filter) {
            self.scope_repos.clear();
        }
        self.list_gen += 1;
        self.loading_list = true;
        self.last_refresh = Instant::now();
        let (client, tx, generation, q) = (self.client.clone(), self.tx.clone(), self.list_gen, self.search_query());
        tokio::spawn(async move {
            // Stream pages so the first 50 show up as soon as possible.
            let (mut after, mut loaded, mut first) = (None, 0, true);
            loop {
                let res = client.search_page(&q, after).await;
                let next = res.as_ref().ok().and_then(|p| p.next.clone());
                loaded += res.as_ref().map_or(0, |p| p.prs.len());
                if tx.send(Msg::Prs { generation, first, res }).is_err() {
                    break;
                }
                first = false;
                match next {
                    Some(n) if loaded < MAX_PRS => after = Some(n),
                    _ => break,
                }
            }
        });
    }

    /// Fetch the viewer's repos in the background (pages of 100, up to MAX_REPOS).
    fn load_repos(&mut self) {
        if self.repos_loading {
            return;
        }
        self.repos_loading = true;
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let (mut after, mut loaded, mut first) = (None, 0, true);
            loop {
                let res = client.viewer_repos_page(after).await;
                let next = res.as_ref().ok().and_then(|p| p.next.clone());
                let low = res.as_ref().ok().and_then(|p| p.rate.as_ref()).is_some_and(|r| r.remaining < 200);
                loaded += res.as_ref().map_or(0, |p| p.repos.len());
                let next = next.filter(|_| loaded < MAX_REPOS && !low);
                if tx.send(Msg::Repos { first, done: next.is_none(), res }).is_err() {
                    break;
                }
                first = false;
                match next {
                    Some(n) => after = Some(n),
                    None => break,
                }
            }
        });
    }

    fn load_detail(&mut self, id: &str) {
        if !self.detail_loading.insert(id.to_string()) {
            return;
        }
        let (client, tx, id) = (self.client.clone(), self.tx.clone(), id.to_string());
        tokio::spawn(async move {
            let res = client.pr_detail(&id).await;
            let _ = tx.send(Msg::Detail { id, res });
        });
    }

    /// Load the selected PR's detail if missing or stale.
    fn ensure_detail(&mut self) {
        if let Some(pr) = self.selected_pr() {
            let id = pr.id.clone();
            let stale = self.details.get(&id).is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(30));
            if stale {
                self.load_detail(&id);
            }
        }
        self.ensure_files(false);
        self.ensure_runs(false);
    }

    /// Load the selected PR's diff when the Changes tab is showing.
    fn ensure_files(&mut self, force: bool) {
        if self.screen != Screen::Detail || self.tab != Tab::Changes {
            return;
        }
        let Some(d) = self.selected_detail() else { return };
        let (id, nwo, number, sha) = (d.id.clone(), d.repository.name_with_owner.clone(), d.number, d.head_ref_oid.clone());
        let stale = force || self.files.get(&id).is_none_or(|(_, at, s)| *s != sha || at.elapsed() > Duration::from_secs(60));
        if !stale || !self.files_loading.insert(id.clone()) {
            return;
        }
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = client.pr_files(&nwo, number).await;
            let _ = tx.send(Msg::Files { id, sha, res });
        });
    }

    /// Load the selected PR's workflow runs when the Actions tab is showing.
    fn ensure_runs(&mut self, force: bool) {
        if self.screen != Screen::Detail || self.tab != Tab::Actions {
            return;
        }
        let Some(d) = self.selected_detail() else { return };
        let history = self.actions_history.contains(&d.id) && !d.is_cross_repository;
        let (id, nwo, sha, branch) = (d.id.clone(), d.repository.name_with_owner.clone(), d.head_ref_oid.clone(), d.head_ref_name.clone());
        let stale = force || self.runs.get(&id).is_none_or(|e| e.sha != sha || e.history != history || e.at.elapsed() > Duration::from_secs(60));
        if !stale || !self.runs_loading.insert(id.clone()) {
            return;
        }
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let mut res = client.workflow_runs(&nwo, RunsQuery::HeadSha(&sha)).await;
            if history && let Ok(runs) = &mut res {
                match client.workflow_runs(&nwo, RunsQuery::Branch { name: &branch, per_page: 30 }).await {
                    Ok(more) => {
                        let seen: HashSet<u64> = runs.iter().map(|r| r.id).collect();
                        runs.extend(more.into_iter().filter(|r| !seen.contains(&r.id)));
                    }
                    Err(e) => res = Err(e),
                }
            }
            // An empty list: either no runs for this commit or no workflows at all.
            let res = match res {
                Ok(runs) if runs.is_empty() => {
                    let none = client.workflow_count(&nwo).await.is_ok_and(|n| n == 0);
                    Ok((runs, none))
                }
                other => other.map(|runs| (runs, false)),
            };
            let _ = tx.send(Msg::Runs { id, nwo, sha, history, res });
        });
    }

    /// Load a run's jobs (with steps) if missing or from an earlier attempt, or again when forced.
    fn ensure_jobs(&mut self, run_id: u64, force: bool) {
        let attempt = self.current_runs().and_then(|e| e.runs.iter().find(|r| r.id == run_id)).map(|r| r.run_attempt);
        let cached = self.jobs.get(&run_id).is_some_and(|(jobs, _)| !rerun(jobs, attempt));
        if !force && cached {
            return;
        }
        let Some(nwo) = self.selected_detail().map(|d| d.repository.name_with_owner.clone()) else { return };
        if !self.jobs_loading.insert(run_id) {
            return;
        }
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = client.run_jobs(&nwo, run_id).await;
            let _ = tx.send(Msg::Jobs { run_id, nwo, res });
        });
    }

    /// Load a finished job's log; a running job has none yet (polling fetches it later).
    fn ensure_log(&mut self, job_id: u64, force: bool) {
        if !self.find_job(job_id).is_some_and(|j| j.status == "completed") {
            return;
        }
        if !force && (self.logs.contains_key(&job_id) || self.log_errors.contains_key(&job_id) || self.logs_pending.contains_key(&job_id)) {
            return;
        }
        let Some(nwo) = self.selected_detail().map(|d| d.repository.name_with_owner.clone()) else { return };
        if !self.logs_loading.insert(job_id) {
            return;
        }
        self.log_errors.remove(&job_id);
        self.logs_pending.remove(&job_id);
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = client.job_log(&nwo, job_id, LOG_CAP).await.map(|l| {
                let mut log = actions_log::parse(&l.text);
                log.dropped_lines += l.dropped_lines;
                (log, l.pending)
            });
            let _ = tx.send(Msg::Log { job_id, res });
        });
    }

    fn ensure_annotations(&mut self, job_id: u64) {
        if self.annotations.contains_key(&job_id) || !self.find_job(job_id).is_some_and(|j| j.status == "completed") {
            return;
        }
        let Some(nwo) = self.selected_detail().map(|d| d.repository.name_with_owner.clone()) else { return };
        if !self.annotations_loading.insert(job_id) {
            return;
        }
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = client.job_annotations(&nwo, job_id).await;
            let _ = tx.send(Msg::Annotations { job_id, res });
        });
    }

    /// Fetch what the node under the tree cursor shows: a run's jobs, a job's annotations and log.
    fn ensure_tree_data(&mut self) {
        match self.selected_node() {
            Some(TreeNode::Run(i)) => {
                if let Some(id) = self.current_runs().map(|e| e.runs[i].id) {
                    self.ensure_jobs(id, false);
                }
            }
            Some(TreeNode::Job(run_id, j)) => {
                if let Some(job_id) = self.jobs.get(&run_id).and_then(|(jobs, _)| jobs.get(j)).map(|j| j.id) {
                    self.ensure_annotations(job_id);
                    if self.log_wanted.contains(&job_id) {
                        self.ensure_log(job_id, false);
                    }
                }
            }
            _ => {}
        }
    }

    /// Poll while the Actions tab shows runs that haven't finished, backing off when the REST
    /// budget runs low and stopping when it's nearly gone.
    fn poll_actions(&mut self) {
        if self.screen != Screen::Detail || self.tab != Tab::Actions {
            return;
        }
        let Some(e) = self.current_runs() else { return };
        let live: Vec<u64> = e.runs.iter().filter(|r| r.status() != "completed").map(|r| r.id).collect();
        if live.is_empty() {
            return;
        }
        let every = match self.client.rest_rate() {
            Some(r) if r.remaining < 100 => return,
            Some(r) if r.remaining < 500 => POLL_SECS_LOW,
            _ => POLL_SECS,
        };
        if self.actions_poll_at.elapsed() < Duration::from_secs(every) {
            return;
        }
        self.actions_poll_at = Instant::now();
        self.ensure_runs(true);
        let shown = self.selected_run_id();
        for id in live {
            if Some(id) == shown || self.actions_expanded.contains(&id) {
                self.ensure_jobs(id, true);
            }
        }
    }

    fn scroll_info(&mut self, delta: isize) {
        let max = self.hits.info_max_scroll as isize;
        self.info_scroll = (self.info_scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    /// Changes when the layout changes enough to warrant a full repaint (clears any
    /// stray glyphs a terminal may have left behind).
    /// Showing or hiding images counts too: Sixel pixels can outlive the cells they were drawn in.
    pub fn layout_key(&self) -> (bool, Option<Tab>, bool, usize, usize, Option<String>, crate::state::Density, bool) {
        let detail = self.screen == Screen::Detail;
        let pr = if detail { self.selected_pr().map(|p| p.id.clone()) } else { None };
        (detail, detail.then_some(self.tab), self.modal.is_some(), self.file_sel, self.actions_sel, pr, self.state.density, self.images.enabled)
    }

    /// Terminal rows per PR in whichever list is on screen.
    fn rows_per_item(&self) -> usize {
        match self.screen {
            Screen::List => self.state.density.table_row_h(),
            Screen::Detail => self.state.density.sidebar_item_h(),
        }
    }

    /// The two-column tabs want the room, so they share a separate, initially collapsed state.
    fn wide_tab(&self) -> bool {
        matches!(self.tab, Tab::Changes | Tab::Actions)
    }

    pub fn info_collapsed(&self) -> bool {
        if self.wide_tab() { !self.changes_info_expanded } else { self.info_collapsed }
    }

    fn file_count(&self) -> usize {
        self.selected_pr().and_then(|p| self.files.get(&p.id)).map_or(0, |(f, ..)| f.len())
    }

    /// Show file `idx` in the Changes tab.
    fn select_file(&mut self, idx: usize) {
        let n = self.file_count();
        if n == 0 {
            return;
        }
        let idx = idx.min(n - 1);
        if idx != self.file_sel {
            self.file_sel = idx;
            self.detail_scroll = 0;
        }
    }

    fn refresh(&mut self) {
        self.load_list();
        if self.screen == Screen::Detail
            && let Some(id) = self.selected_pr().map(|p| p.id.clone())
        {
            self.load_detail(&id);
            self.ensure_files(true);
            self.ensure_runs(true);
            if let Some(run) = self.selected_run_id() {
                self.ensure_jobs(run, true);
            }
        }
    }

    // ---------- selection helpers ----------

    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.prs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                f.is_empty()
                    || p.title.to_lowercase().contains(&f)
                    || p.repository.name_with_owner.to_lowercase().contains(&f)
                    || crate::github::login_of(&p.author).to_lowercase().contains(&f)
                    || format!("#{}", p.number).contains(&f)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_pr(&self) -> Option<&PrSummary> {
        self.visible().get(self.sel).map(|&i| &self.prs[i])
    }

    pub fn selected_detail(&self) -> Option<&PrDetail> {
        self.selected_pr().and_then(|p| self.details.get(&p.id)).map(|(d, _)| d)
    }

    /// The selected PR's runs, if they were fetched for its current head commit.
    pub fn current_runs(&self) -> Option<&RunsEntry> {
        let d = self.selected_detail()?;
        self.runs.get(&d.id).filter(|e| e.sha == d.head_ref_oid)
    }

    /// The Actions tree, flattened in display order.
    pub fn tree(&self) -> Vec<TreeNode> {
        let Some(e) = self.current_runs() else { return Vec::new() };
        let mut out = Vec::new();
        let mut push = e.sha.as_str();
        for (i, r) in e.runs.iter().enumerate() {
            if r.head_sha != push {
                push = &r.head_sha;
                out.push(TreeNode::Push(r.head_sha.clone()));
            }
            out.push(TreeNode::Run(i));
            if self.actions_expanded.contains(&r.id)
                && let Some((jobs, _)) = self.jobs.get(&r.id)
            {
                out.extend((0..jobs.len()).map(|j| TreeNode::Job(r.id, j)));
            }
        }
        out
    }

    pub fn selected_node(&self) -> Option<TreeNode> {
        self.tree().get(self.actions_sel).cloned()
    }

    /// The run under the tree cursor, or the run of the job under it.
    fn selected_run_id(&self) -> Option<u64> {
        match self.selected_node()? {
            TreeNode::Run(i) => self.current_runs().map(|e| e.runs[i].id),
            TreeNode::Job(run_id, _) => Some(run_id),
            TreeNode::Push(_) => None,
        }
    }

    pub fn selected_job(&self) -> Option<&Job> {
        match self.selected_node()? {
            TreeNode::Job(run_id, j) => self.jobs.get(&run_id)?.0.get(j),
            _ => None,
        }
    }

    fn find_job(&self, job_id: u64) -> Option<&Job> {
        self.jobs.values().flat_map(|(jobs, _)| jobs).find(|j| j.id == job_id)
    }

    /// Identity of the tree cursor (run id, job id), stable across reloads.
    fn selected_key(&self) -> Option<(u64, Option<u64>)> {
        match self.selected_node()? {
            TreeNode::Job(run_id, _) => Some((run_id, self.selected_job().map(|j| j.id))),
            _ => self.selected_run_id().map(|r| (r, None)),
        }
    }

    /// Put the tree cursor back on `key` (or its run) after the tree changed.
    fn reselect(&mut self, key: Option<(u64, Option<u64>)>) {
        let tree = self.tree();
        let runs = self.current_runs().map(|e| e.runs.iter().map(|r| r.id).collect::<Vec<_>>()).unwrap_or_default();
        let is = |n: &TreeNode, run: u64, job: Option<u64>| match (n, job) {
            (TreeNode::Run(i), None) => runs.get(*i) == Some(&run),
            (TreeNode::Job(r, j), Some(job)) => *r == run && self.jobs.get(r).and_then(|(js, _)| js.get(*j)).is_some_and(|x| x.id == job),
            _ => false,
        };
        if let Some((run, job)) = key
            && let Some(i) = tree.iter().position(|n| is(n, run, job)).or_else(|| tree.iter().position(|n| is(n, run, None)))
        {
            if i != self.actions_sel {
                self.actions_sel = i;
                self.actions_tree_follow = true;
            }
        }
        self.actions_sel = self.actions_sel.min(tree.len().saturating_sub(1));
        // Push headings aren't selectable.
        if matches!(tree.get(self.actions_sel), Some(TreeNode::Push(_))) {
            self.actions_sel = (self.actions_sel + 1).min(tree.len().saturating_sub(1));
        }
        self.ensure_tree_data();
    }

    /// Move the tree cursor to node `idx`, skipping push headings in the direction of travel.
    fn select_node(&mut self, idx: usize) {
        let tree = self.tree();
        if tree.is_empty() {
            return;
        }
        let mut idx = idx.min(tree.len() - 1);
        let down = idx >= self.actions_sel;
        while matches!(tree[idx], TreeNode::Push(_)) {
            match (down, idx) {
                (true, i) if i + 1 < tree.len() => idx += 1,
                (false, i) if i > 0 => idx -= 1,
                _ => return,
            }
        }
        if idx != self.actions_sel {
            self.actions_sel = idx;
            self.detail_scroll = 0;
            if self.selected_job().is_some_and(|j| self.log_wanted.contains(&j.id)) {
                self.detail_scroll = usize::MAX;
            }
        }
        self.actions_tree_follow = true;
        self.ensure_tree_data();
    }

    /// Stop typing into the log search; the query and its hits stay.
    fn end_log_search_input(&mut self) {
        if let Some(s) = &mut self.log_view.search {
            s.input = false;
        }
    }

    fn move_tree(&mut self, delta: isize) {
        self.select_node((self.actions_sel as isize + delta).max(0) as usize);
    }

    /// Point the log view at `job_id`'s log, resetting folds for a different or refetched log.
    pub fn sync_log_view(&mut self, job_id: u64) {
        let Some((log, at)) = self.logs.get(&job_id) else { return };
        let v = &mut self.log_view;
        if v.job == Some(job_id) && v.at == Some(*at) {
            return;
        }
        if v.job != Some(job_id) {
            v.search = None;
            v.cursor = None;
        }
        v.job = Some(job_id);
        v.at = Some(*at);
        v.folds = actions_log::default_folds(log);
        v.visible = actions_log::visible_lines(log, &v.folds);
        if let Some(s) = &mut v.search {
            s.hits = actions_log::search(log, &s.query);
        }
    }

    /// The selected job's log, once it's shown, and synced into `log_view`.
    fn shown_log(&mut self) -> Option<u64> {
        let id = self.selected_job().map(|j| j.id).filter(|id| self.logs.contains_key(id))?;
        self.sync_log_view(id);
        Some(id)
    }

    /// Recompute the visible lines after a fold change.
    fn refold(&mut self) {
        if let Some((log, _)) = self.log_view.job.and_then(|j| self.logs.get(&j)) {
            self.log_view.visible = actions_log::visible_lines(log, &self.log_view.folds);
        }
    }

    /// Unfold whatever hides `line`, mark it and bring it into view.
    fn jump_to_line(&mut self, line: usize) {
        let Some(job) = self.shown_log() else { return };
        if let Some(g) = self.logs[&job].0.lines.get(line).and_then(|l| l.group)
            && self.logs[&job].0.groups[g].start != line
            && self.log_view.folds.remove(&g)
        {
            self.refold();
        }
        self.log_view.cursor = Some(line);
        self.log_view.scroll_to = Some(line);
    }

    /// Jump to the next line in `targets` after the cursor (or the top of the view), wrapping.
    fn jump_next(&mut self, targets: &[usize], back: bool) -> bool {
        if targets.is_empty() {
            return false;
        }
        let from = self.log_view.cursor.or(self.hits.log_top_line);
        let next = match (from, back) {
            (Some(c), false) => targets.iter().find(|&&t| t > c).or(targets.first()),
            (Some(c), true) => targets.iter().rev().find(|&&t| t < c).or(targets.last()),
            (None, false) => targets.first(),
            (None, true) => targets.last(),
        };
        if let Some(&line) = next {
            self.jump_to_line(line);
        }
        true
    }

    fn select(&mut self, idx: usize) {
        let n = self.visible().len();
        if n == 0 {
            self.sel = 0;
            return;
        }
        let idx = idx.min(n - 1);
        if idx != self.sel {
            self.sel = idx;
            self.detail_scroll = 0;
            self.info_scroll = 0;
            self.file_sel = 0;
            self.file_list_scroll = 0;
            self.actions_sel = 0;
            self.actions_tree_scroll = 0;
            self.actions_tree_follow = true;
        }
        if self.screen == Screen::Detail {
            self.ensure_detail();
        }
    }

    fn move_sel(&mut self, delta: isize) {
        let n = self.visible().len() as isize;
        if n == 0 {
            return;
        }
        self.select((self.sel as isize + delta).clamp(0, n - 1) as usize);
    }

    fn scroll_detail(&mut self, delta: isize) {
        let max = self.hits.detail_max_scroll as isize;
        self.detail_scroll = (self.detail_scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    pub fn toast(&mut self, msg: impl Into<String>, kind: ToastKind) {
        self.toast = Some((msg.into(), kind, Instant::now()));
    }

    // ---------- message handling ----------

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if let Some((_, kind, at)) = &self.toast {
            let ttl = if *kind == ToastKind::Error { 8 } else { 4 };
            if at.elapsed() > Duration::from_secs(ttl) {
                self.toast = None;
            }
        }
        let every = self.cfg.refresh_secs;
        let composing = matches!(self.modal, Some(Modal::Composer(_)));
        if every > 0 && !composing && !self.loading_list && self.last_refresh.elapsed() > Duration::from_secs(every) {
            self.refresh();
        }
        // The open PR refreshes on its own, faster cadence.
        let detail_every = self.cfg.detail_refresh_secs;
        if detail_every > 0
            && !composing
            && self.screen == Screen::Detail
            && let Some(id) = self.selected_pr().map(|p| p.id.clone())
            && !self.detail_loading.contains(&id)
            && self.details.get(&id).is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(detail_every))
        {
            self.load_detail(&id);
        }
        self.poll_actions();
        if self.screen == Screen::Detail
            && self.tab == Tab::Actions
            && let Some(id) = self.selected_job().map(|j| j.id)
            && self.logs_pending.get(&id).is_some_and(|at| at.elapsed() > Duration::from_secs(LOG_RETRY_SECS))
        {
            self.ensure_log(id, true);
        }
        self.images.evict();
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input(ev) => self.on_event(ev),
            Msg::Viewer(Ok((v, rate))) => {
                self.rate = rate.or(self.rate.take());
                // Search ORs repeated user:/org: qualifiers, so this is every PR you can see in
                // your own repos and all your orgs' repos.
                let everywhere = std::iter::once(format!("user:{}", v.login)).chain(v.orgs.iter().map(|o| format!("org:{o}"))).collect::<Vec<_>>().join(" ");
                let mut extra = vec![
                    Scope { name: "All PRs (my repos + orgs)".into(), qualifier: everywhere },
                    Scope { name: format!("{} (my repos)", v.login), qualifier: format!("user:{}", v.login) },
                ];
                extra.extend(v.orgs.iter().map(|o| Scope { name: format!("org: {o}"), qualifier: format!("org:{o}") }));
                let builtin = 5;
                let current = self.scopes[self.scope].clone();
                self.scopes.splice(builtin..builtin, extra);
                self.scope = self.scopes.iter().position(|s| *s == current).unwrap_or(0);
                // The configured default may name a scope that only exists now.
                if let Some(d) = &self.cfg.default_scope
                    && self.scopes[self.scope].name != *d
                    && let Some(i) = self.scopes.iter().position(|s| &s.name == d)
                    && (self.repo.is_some() || !self.scopes[i].qualifier.is_empty())
                {
                    self.scope = i;
                    self.load_list();
                }
                if let Some(repos) = cached_repos(self.state.repos_cache.as_ref(), &v.login) {
                    self.repos = repos;
                }
                self.load_repos();
                self.viewer = Some(v);
            }
            Msg::Viewer(Err(e)) => self.toast(format!("Failed to load account: {e:#}"), ToastKind::Error),
            Msg::Prs { generation, first, res } => {
                if generation != self.list_gen {
                    return;
                }
                self.loading_list = false;
                match res {
                    Ok(page) => {
                        let has_next = page.next.is_some();
                        let prev = if first { self.pending_select.take() } else { None }.or_else(|| self.selected_pr().map(|p| p.id.clone()));
                        if first {
                            self.prs = page.prs;
                        } else {
                            let seen: HashSet<String> = self.prs.iter().map(|p| p.id.clone()).collect();
                            self.prs.extend(page.prs.into_iter().filter(|p| !seen.contains(&p.id)));
                        }
                        self.total = page.total;
                        self.loading_list = has_next && self.prs.len() < MAX_PRS;
                        if self.repo.is_none() {
                            self.scope_repos = count_repos(&self.prs);
                            self.scope_repos_key = (self.scope, self.state_filter);
                        }
                        self.rate = page.rate.or(self.rate.take());
                        self.list_error = None;
                        let vis = self.visible();
                        self.sel = prev
                            .and_then(|id| vis.iter().position(|&i| self.prs[i].id == id))
                            .unwrap_or(self.sel.min(vis.len().saturating_sub(1)));
                        // A filter change can swap the open PR for one never loaded.
                        if self.screen == Screen::Detail && self.selected_pr().is_some_and(|p| !self.details.contains_key(&p.id)) {
                            self.ensure_detail();
                        }
                    }
                    Err(e) => self.list_error = Some(format!("{e:#}")),
                }
            }
            Msg::Detail { id, res } => {
                self.detail_loading.remove(&id);
                match res {
                    Ok((d, rate)) => {
                        self.rate = rate.or(self.rate.take());
                        self.detail_errors.remove(&id);
                        // Keep the list row in sync with the fresher detail.
                        if let Some(p) = self.prs.iter_mut().find(|p| p.id == id) {
                            p.title = d.title.clone();
                            p.state = d.state.clone();
                            p.is_draft = d.is_draft;
                            p.review_decision = d.review_decision.clone();
                        }
                        self.details.insert(id, (d, Instant::now()));
                        self.ensure_files(false);
                        self.ensure_runs(false);
                    }
                    Err(e) => {
                        self.detail_errors.insert(id, format!("{e:#}"));
                    }
                }
            }
            Msg::Repos { first, done, res } => match res {
                Ok(page) => {
                    self.rate = page.rate.clone().or(self.rate.take());
                    if first {
                        self.repos_fresh.clear();
                    }
                    self.repos_total = page.total;
                    self.repos_fresh.extend(page.repos);
                    // Show what we have once the fresh list overtakes the cache.
                    if self.repos_fresh.len() >= self.repos.len() || done {
                        self.repos = self.repos_fresh.clone();
                    }
                    if done {
                        self.repos_loading = false;
                        self.repos = std::mem::take(&mut self.repos_fresh);
                        if let Some(v) = &self.viewer {
                            self.state.repos_cache = Some(crate::state::ReposCache { login: v.login.clone(), repos: self.repos.clone() });
                            crate::state::save(&self.state);
                        }
                    }
                }
                Err(e) => {
                    self.repos_loading = false;
                    self.toast(format!("Couldn't load your repositories: {e:#}"), ToastKind::Error);
                }
            },
            Msg::Files { id, sha, res } => {
                self.files_loading.remove(&id);
                let head = self.details.get(&id).map(|(d, _)| d.head_ref_oid.clone());
                // The request captured the head it was started for. A newer head can land
                // while it is in flight, and `files_loading` stops that newer fetch from
                // starting. Drop the stale reply and fetch the head now on screen.
                if !files_match_head(head.as_deref(), &sha) {
                    self.ensure_files(false);
                    return;
                }
                match res {
                    Ok(files) => {
                        self.files_errors.remove(&id);
                        self.files.insert(id, (files, Instant::now(), sha));
                    }
                    Err(e) => {
                        self.files_errors.insert(id, format!("{e:#}"));
                    }
                }
            }
            Msg::Runs { id, nwo, sha, history, res } => {
                self.runs_loading.remove(&id);
                match res {
                    Ok((mut runs, no_workflows)) => {
                        self.runs_errors.remove(&id);
                        if no_workflows {
                            self.no_workflows.insert(nwo);
                        } else {
                            self.no_workflows.remove(&nwo);
                        }
                        let mut pushes: Vec<String> = Vec::new();
                        for r in &runs {
                            if !pushes.contains(&r.head_sha) {
                                pushes.push(r.head_sha.clone());
                            }
                        }
                        runs.sort_by_key(|r| (r.head_sha != sha, pushes.iter().position(|p| *p == r.head_sha)));
                        let current = self.selected_pr().is_some_and(|p| p.id == id);
                        let first = self.runs.get(&id).is_none_or(|e| e.sha != sha);
                        let key = if current && !first { self.selected_key() } else { None };
                        // Runs that finished or were re-run since their jobs were fetched need them again.
                        let refetch: Vec<u64> = runs
                            .iter()
                            .filter(|r| {
                                self.jobs.get(&r.id).is_some_and(|(j, _)| {
                                    (r.status() == "completed" && j.iter().any(|j| j.status != "completed")) || rerun(j, Some(r.run_attempt))
                                })
                            })
                            .map(|r| r.id)
                            .collect();
                        // Open the failing or running runs of the head commit on first sight.
                        let open: Vec<u64> = if first {
                            let live = |r: &&WorkflowRunInfo| crate::ui::outcome_rank(&crate::ui::rest_outcome(r.status(), r.conclusion.as_deref())) < 2;
                            runs.iter().filter(|r| r.head_sha == sha).filter(live).take(3).map(|r| r.id).collect()
                        } else {
                            Vec::new()
                        };
                        self.runs.insert(id, RunsEntry { sha, history, runs, at: Instant::now() });
                        if !current {
                            return;
                        }
                        self.actions_poll_at = Instant::now();
                        for run in &open {
                            self.actions_expanded.insert(*run);
                            self.ensure_jobs(*run, false);
                        }
                        for run in refetch {
                            self.ensure_jobs(run, true);
                        }
                        match open.first() {
                            Some(&run) => self.reselect(Some((run, None))),
                            None => self.reselect(key),
                        }
                        self.ensure_runs(false);
                    }
                    Err(e) => {
                        self.runs_errors.insert(id, format!("{e:#}"));
                    }
                }
            }
            Msg::Jobs { run_id, nwo, res } => {
                self.jobs_loading.remove(&run_id);
                match res {
                    Ok(jobs) => {
                        self.jobs_errors.remove(&run_id);
                        let key = self.selected_key();
                        let finished: Vec<u64> = jobs.iter().filter(|j| j.status == "completed" && self.log_wanted.contains(&j.id)).map(|j| j.id).collect();
                        self.jobs.insert(run_id, (jobs, Instant::now()));
                        self.reselect(key);
                        // A late reply for another repo's run: its logs load when it's shown again.
                        if self.selected_detail().is_none_or(|d| d.repository.name_with_owner != nwo) {
                            return;
                        }
                        for job in finished {
                            self.ensure_log(job, false);
                        }
                    }
                    Err(e) => {
                        self.jobs_errors.insert(run_id, format!("{e:#}"));
                    }
                }
            }
            Msg::Log { job_id, res } => {
                self.logs_loading.remove(&job_id);
                match res {
                    Ok((_, true)) => {
                        // A job can report completed before its log blob exists; on_tick retries.
                        self.logs_pending.insert(job_id, Instant::now());
                    }
                    Ok((mut log, false)) => {
                        if let Some(job) = self.find_job(job_id) {
                            actions_log::assign_steps(&mut log, &job.steps);
                        }
                        self.logs.insert(job_id, (log, Instant::now()));
                        if self.selected_job().is_some_and(|j| j.id == job_id) {
                            // Tail first.
                            self.detail_scroll = usize::MAX;
                        }
                    }
                    Err(e) => {
                        self.log_errors.insert(job_id, format!("{e:#}"));
                    }
                }
            }
            Msg::Annotations { job_id, res } => {
                self.annotations_loading.remove(&job_id);
                // Annotations are a bonus; a failure just shows none.
                self.annotations.insert(job_id, res.unwrap_or_default());
            }
            Msg::Done { id, res } => {
                self.busy.remove(&id);
                match res {
                    Ok(m) => {
                        if let Some(Modal::Composer(c)) = &self.modal
                            && c.pr_id == id
                        {
                            self.modal = None;
                        }
                        self.toast(m, ToastKind::Success);
                        self.details.remove(&id);
                        self.load_detail(&id);
                        self.load_list();
                    }
                    Err(e) => {
                        if let Some(Modal::Composer(c)) = &mut self.modal {
                            c.submitting = false;
                        }
                        self.toast(format!("{e:#}"), ToastKind::Error);
                    }
                }
            }
            Msg::Image(m) => {
                // Rows change when an image arrives; rebuilding re-lays out the tabs.
                if self.images.apply(m) {
                    self.tab_cache.clear();
                }
                self.images.pump(&self.client, &self.tx);
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) => {
                if let Some(Modal::Composer(c)) = &mut self.modal {
                    c.editor.insert_str(&s);
                } else if let Some(Modal::Scopes { filter, sel } | Modal::Repos { filter, sel }) = &mut self.modal {
                    filter.push_str(s.trim());
                    *sel = 0;
                } else if self.filter_input {
                    self.filter.push_str(s.trim());
                }
            }
            _ => {}
        }
    }

    // ---------- keyboard ----------

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            return self.on_modal_key(k);
        }
        if self.filter_input {
            match k.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_input = false;
                    self.sel = 0;
                }
                KeyCode::Enter => self.filter_input = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.sel = 0;
                }
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Down => self.move_sel(1),
                KeyCode::Char(c) if !ctrl => {
                    self.filter.push(c);
                    self.sel = 0;
                }
                _ => {}
            }
            return;
        }
        if let Some(s) = self.log_view.search.as_mut().filter(|s| s.input) {
            if self.screen == Screen::Detail && self.tab == Tab::Actions {
                return self.on_log_search_key(k);
            }
            // The prompt went away with the tab (a click); stop typing into it.
            s.input = false;
        }

        let page = (self.hits.detail_height.max(4) - 2) as isize;
        let in_detail_pane = self.screen == Screen::Detail && self.focus == Focus::Detail;
        let actions = self.screen == Screen::Detail && self.tab == Tab::Actions;
        let searching = self.log_view.search.is_some();
        let action = match k.code {
            // Actions tab first: it reuses keys that mean something else elsewhere.
            KeyCode::Char('f') if ctrl && actions => Some(Action::LogSearch),
            KeyCode::Char('n') if actions && searching => Some(Action::LogNextHit),
            KeyCode::Char('N') if actions && searching => Some(Action::LogPrevHit),
            KeyCode::Char('n') if actions => Some(Action::TreeNext),
            KeyCode::Char('p') if actions => Some(Action::TreePrev),
            KeyCode::Down if in_detail_pane && actions => Some(Action::TreeNext),
            KeyCode::Up if in_detail_pane && actions => Some(Action::TreePrev),
            KeyCode::Enter if in_detail_pane && actions => Some(Action::TreeToggle),
            KeyCode::Esc if actions && (in_detail_pane || searching) => Some(Action::ActionsBack),
            KeyCode::Char('O') if actions => Some(Action::TreeOpenBrowser),
            KeyCode::Char('H') if actions => Some(Action::ToggleHistory),
            KeyCode::Char('E') if actions => Some(Action::LogNextError),
            KeyCode::Char('W') if actions => Some(Action::LogNextWarning),
            KeyCode::Char('z') if actions => Some(Action::LogFold),
            KeyCode::Char('Z') if actions => Some(Action::LogFoldAll),
            KeyCode::Char('t') if actions => Some(Action::LogTimestamps),
            KeyCode::Char('q') => Some(if self.screen == Screen::Detail { Action::Back } else { Action::Quit }),
            KeyCode::Esc => Some(if self.screen == Screen::Detail { Action::Back } else { Action::ClearFilter }),
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Char('r') | KeyCode::F(5) => Some(Action::Refresh),
            KeyCode::Char('s') => Some(Action::OpenScopes),
            KeyCode::Char('S') => Some(Action::OpenRepos),
            KeyCode::Char('D') => Some(Action::CycleDensity),
            KeyCode::Char('I') => Some(Action::ToggleImages),
            KeyCode::Char('e') => Some(Action::ToggleRepoOfSelected),
            KeyCode::Char('f') => Some(Action::CycleState),
            KeyCode::Char('/') => Some(Action::StartFilter),
            KeyCode::Char('o') => Some(Action::Browser),
            KeyCode::Char('c') => Some(Action::Comment),
            KeyCode::Char('m') => Some(Action::MergeMenu),
            KeyCode::Char('.') | KeyCode::Char(' ') => Some(Action::Menu),
            KeyCode::Char('a') => Some(Action::Approve),
            KeyCode::Char('R') => Some(Action::RequestChanges),
            KeyCode::Char('x') => Some(Action::CloseOrReopen),
            KeyCode::Char('y') => Some(Action::CopyCheckout),
            KeyCode::Char(c @ '1'..='7') if self.screen == Screen::Detail => Some(Action::SetTab(Tab::ALL[c as usize - '1' as usize])),
            KeyCode::Char(']') if self.screen == Screen::Detail => Some(Action::NextTab),
            KeyCode::Char('[') if self.screen == Screen::Detail => Some(Action::PrevTab),
            KeyCode::Char('n') if self.screen == Screen::Detail && self.tab == Tab::Changes => Some(Action::NextFile),
            KeyCode::Char('p') if self.screen == Screen::Detail && self.tab == Tab::Changes => Some(Action::PrevFile),
            KeyCode::Down if in_detail_pane && self.tab == Tab::Changes => Some(Action::NextFile),
            KeyCode::Up if in_detail_pane && self.tab == Tab::Changes => Some(Action::PrevFile),
            KeyCode::Char('i') if self.screen == Screen::Detail => Some(Action::ToggleInfo),
            KeyCode::Tab | KeyCode::BackTab if self.screen == Screen::Detail => Some(Action::ToggleFocus),
            KeyCode::Char('h') | KeyCode::Left if self.screen == Screen::Detail => Some(Action::FocusSidebar),
            KeyCode::Char('l') | KeyCode::Right if self.screen == Screen::Detail => Some(Action::FocusDetail),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right if self.screen == Screen::List => Some(Action::OpenSelected),
            KeyCode::Enter if self.focus == Focus::Sidebar => Some(Action::FocusDetail),
            _ => None,
        };
        if let Some(a) = action {
            return self.run(a);
        }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') if in_detail_pane => self.scroll_detail(-1),
            KeyCode::Down | KeyCode::Char('j') if in_detail_pane => self.scroll_detail(1),
            KeyCode::PageUp if in_detail_pane => self.scroll_detail(-page),
            KeyCode::PageDown if in_detail_pane => self.scroll_detail(page),
            KeyCode::Char('u') if in_detail_pane && ctrl => self.scroll_detail(-page / 2),
            KeyCode::Char('d') if in_detail_pane && ctrl => self.scroll_detail(page / 2),
            KeyCode::Home | KeyCode::Char('g') if in_detail_pane => self.detail_scroll = 0,
            KeyCode::End | KeyCode::Char('G') if in_detail_pane => self.detail_scroll = self.hits.detail_max_scroll,
            KeyCode::Char('J') if in_detail_pane => self.move_sel(1),
            KeyCode::Char('K') if in_detail_pane => self.move_sel(-1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            _ => {}
        }
    }

    /// Typing a log search query (after Ctrl+F).
    fn on_log_search_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(s) = self.log_view.search.as_mut() else { return };
        match k.code {
            KeyCode::Esc => self.log_view.search = None,
            KeyCode::Enter => {
                s.input = false;
                if s.query.is_empty() {
                    self.log_view.search = None;
                } else if !self.run_search_jump(false) {
                    self.toast(format!("No matches for “{}”", self.log_view.search.as_ref().map_or("", |s| s.query.as_str())), ToastKind::Info);
                }
                return;
            }
            KeyCode::Backspace => {
                s.query.pop();
            }
            KeyCode::Char(c) if !ctrl => s.query.push(c),
            _ => return,
        }
        // Count matches as the query changes; Enter jumps.
        if let (Some(s), Some((log, _))) = (self.log_view.search.as_mut(), self.log_view.job.and_then(|j| self.logs.get(&j))) {
            s.hits = actions_log::search(log, &s.query);
        }
    }

    /// Jump to the next (or previous) search hit; false when there are none.
    fn run_search_jump(&mut self, back: bool) -> bool {
        let hits = self.log_view.search.as_ref().map(|s| s.hits.clone()).unwrap_or_default();
        self.jump_next(&hits, back)
    }

    fn on_modal_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match self.modal.as_mut() {
            Some(Modal::Help) => self.modal = None,
            Some(Modal::Image { .. }) => match k.code {
                KeyCode::Char('o') | KeyCode::Enter => self.modal_pick(0),
                KeyCode::Char('y') => self.modal_pick(1),
                _ => self.modal = None,
            },
            Some(Modal::Scopes { .. } | Modal::Repos { .. }) => {
                let n = match &self.modal {
                    Some(Modal::Scopes { filter, .. }) => scope_matches(&self.scopes, filter, self.repo.is_some()).len(),
                    Some(Modal::Repos { filter, .. }) => repo_items(self, filter).iter().filter(|i| i.value().is_some()).count(),
                    _ => 0,
                };
                let Some(Modal::Scopes { filter, sel } | Modal::Repos { filter, sel }) = self.modal.as_mut() else { return };
                match k.code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Enter => {
                        let s = *sel;
                        self.run(Action::ModalPick(s));
                    }
                    KeyCode::Up => *sel = sel.saturating_sub(1),
                    KeyCode::Char('p') if ctrl => *sel = sel.saturating_sub(1),
                    KeyCode::Down if n > 0 => *sel = (*sel + 1).min(n - 1),
                    KeyCode::Char('n') if ctrl && n > 0 => *sel = (*sel + 1).min(n - 1),
                    KeyCode::Backspace => {
                        filter.pop();
                        *sel = 0;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        filter.push(c);
                        *sel = 0;
                    }
                    _ => {}
                }
            }
            Some(Modal::Menu { entries, sel, .. }) => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(entries.len().saturating_sub(1)),
                KeyCode::Enter => {
                    let s = *sel;
                    self.run(Action::ModalPick(s));
                }
                KeyCode::Char(c) => {
                    if let Some(i) = entries.iter().position(|e| e.key == c) {
                        self.run(Action::ModalPick(i));
                    }
                }
                _ => {}
            },
            Some(Modal::Confirm { .. }) => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.run(Action::ConfirmYes),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                _ => {}
            },
            Some(Modal::Composer(c)) => {
                if c.submitting {
                    if k.code == KeyCode::Esc {
                        self.toast("Still sending…", ToastKind::Info);
                    }
                    return;
                }
                let popup = self.mention_candidates();
                let popup_open = !popup.is_empty();
                let Some(Modal::Composer(c)) = self.modal.as_mut() else { return };
                match k.code {
                    KeyCode::Char('s') if ctrl => return self.run(Action::Submit),
                    KeyCode::Enter if ctrl || alt => return self.run(Action::Submit),
                    KeyCode::Esc if popup_open => c.popup_dismissed = c.editor.mention_prefix(),
                    KeyCode::Esc => return self.run(Action::ModalCancel),
                    KeyCode::Up if popup_open => c.popup_sel = c.popup_sel.saturating_sub(1),
                    KeyCode::Down if popup_open => c.popup_sel = (c.popup_sel + 1).min(popup.len() - 1),
                    KeyCode::Tab | KeyCode::Enter if popup_open => {
                        let i = c.popup_sel;
                        return self.run(Action::AcceptMention(i));
                    }
                    KeyCode::Enter => c.editor.newline(),
                    KeyCode::Backspace if alt => c.editor.delete_word(),
                    KeyCode::Char('w') if ctrl => c.editor.delete_word(),
                    KeyCode::Backspace => c.editor.backspace(),
                    KeyCode::Delete => c.editor.delete(),
                    KeyCode::Left => c.editor.left(),
                    KeyCode::Right => c.editor.right(),
                    KeyCode::Up => c.editor.up(),
                    KeyCode::Down => c.editor.down(),
                    KeyCode::Home => c.editor.home(),
                    KeyCode::End => c.editor.end(),
                    KeyCode::Char('a') if ctrl => c.editor.home(),
                    KeyCode::Char('e') if ctrl => c.editor.end(),
                    KeyCode::Tab => c.editor.insert_str("  "),
                    KeyCode::Char(ch) if !ctrl => c.editor.insert_char(ch),
                    _ => {}
                }
                if !matches!(k.code, KeyCode::Up | KeyCode::Down) {
                    c.popup_sel = 0;
                }
            }
            None => {}
        }
    }

    // ---------- mouse ----------

    fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.on_click(pos),
            MouseEventKind::ScrollUp => self.on_wheel(pos, -3),
            MouseEventKind::ScrollDown => self.on_wheel(pos, 3),
            _ => {}
        }
    }

    fn on_wheel(&mut self, pos: Position, delta: isize) {
        // Lists can be longer than what's drawn, so count every selectable row.
        let list_len = match &self.modal {
            Some(Modal::Scopes { filter, .. }) => scope_matches(&self.scopes, filter, self.repo.is_some()).len(),
            Some(Modal::Repos { filter, .. }) => repo_items(self, filter).iter().filter(|i| i.value().is_some()).count(),
            Some(Modal::Menu { entries, .. }) => entries.len(),
            _ => 0,
        };
        if let Some(modal) = self.modal.as_mut() {
            match modal {
                Modal::Composer(c) => {
                    let max = self.hits.editor_rows.len().saturating_sub(1) as isize;
                    c.editor.scroll = (c.editor.scroll as isize + delta).clamp(0, max) as usize;
                }
                Modal::Scopes { sel, .. } | Modal::Repos { sel, .. } | Modal::Menu { sel, .. } => {
                    let n = list_len.max(1);
                    *sel = (*sel as isize + delta.signum()).clamp(0, n as isize - 1) as usize;
                }
                Modal::Image { .. } => self.modal = None,
                _ => {}
            }
            return;
        }
        if let Some((r, offset)) = self.hits.file_list
            && r.contains(pos)
        {
            let n = self.file_count();
            self.file_list_scroll = (offset as isize + delta).clamp(0, n.saturating_sub(r.height as usize) as isize) as usize;
        } else if let Some((r, offset)) = self.hits.actions_tree
            && r.contains(pos)
        {
            let n = self.hits.actions_tree_rows.len();
            self.actions_tree_scroll = (offset as isize + delta).clamp(0, n.saturating_sub(r.height as usize) as isize) as usize;
            self.actions_tree_follow = false;
        } else if self.hits.info.is_some_and(|r| r.contains(pos)) {
            self.focus = Focus::Detail;
            self.scroll_info(delta);
        } else if self.hits.detail.is_some_and(|r| r.contains(pos)) {
            self.focus = Focus::Detail;
            self.scroll_detail(delta);
        } else if let Some((r, offset)) = self.hits.sidebar.or(self.hits.table)
            && r.contains(pos)
        {
            // Wheel scrolls the viewport; selection follows if it would leave it.
            let n = self.visible().len();
            let per = self.rows_per_item();
            let page = r.height as usize / per;
            let new_off = (offset as isize + delta / per as isize).clamp(0, n.saturating_sub(page) as isize) as usize;
            if self.screen == Screen::List {
                *self.table_state.offset_mut() = new_off;
            } else {
                *self.side_state.offset_mut() = new_off;
                self.focus = Focus::Sidebar;
            }
            let rows = (r.height as usize / per).max(1);
            if self.sel < new_off {
                self.select(new_off);
            } else if self.sel >= new_off + rows {
                self.select(new_off + rows - 1);
            }
        }
    }

    fn on_click(&mut self, pos: Position) {
        // Modal content takes priority; clicking outside closes most modals.
        if self.modal.is_some() {
            if let Some(&(_, i)) = self.hits.popup_items.iter().find(|(r, _)| r.contains(pos)) {
                return self.run(Action::AcceptMention(i));
            }
            if let Some(&(_, i)) = self.hits.modal_items.iter().find(|(r, _)| r.contains(pos)) {
                return self.run(Action::ModalPick(i));
            }
            if let Some((_, a)) = self.hits.buttons.iter().find(|(r, _)| r.contains(pos)).cloned() {
                return self.run(a);
            }
            if let Some(er) = self.hits.editor
                && er.contains(pos)
                && let Some(Modal::Composer(c)) = self.modal.as_mut()
            {
                let vrow = c.editor.scroll + (pos.y - er.y) as usize;
                c.editor.click(&self.hits.editor_rows, vrow, (pos.x - er.x) as usize);
                c.popup_sel = 0;
                return;
            }
            let outside = self.hits.modal.is_none_or(|r| !r.contains(pos));
            if outside && !matches!(self.modal, Some(Modal::Composer(_))) {
                self.modal = None;
            }
            return;
        }
        if let Some((_, a)) = self.hits.buttons.iter().find(|(r, _)| r.contains(pos)).cloned() {
            return self.run(a);
        }
        if let Some((r, offset)) = self.hits.table
            && r.contains(pos)
        {
            let idx = offset + (pos.y - r.y) as usize / self.state.density.table_row_h();
            if idx < self.visible().len() {
                let double = self.last_click.is_some_and(|(t, i)| i == idx && t.elapsed() < Duration::from_millis(450));
                self.select(idx);
                self.last_click = Some((Instant::now(), idx));
                if double {
                    self.run(Action::OpenSelected);
                }
            }
            return;
        }
        if let Some((r, offset)) = self.hits.sidebar
            && r.contains(pos)
        {
            self.focus = Focus::Sidebar;
            self.select(offset + (pos.y - r.y) as usize / self.state.density.sidebar_item_h());
            return;
        }
        if let Some((r, offset)) = self.hits.file_list
            && r.contains(pos)
        {
            self.focus = Focus::Detail;
            self.select_file(offset + (pos.y - r.y) as usize);
            return;
        }
        if let Some((r, offset)) = self.hits.actions_tree
            && r.contains(pos)
        {
            self.focus = Focus::Detail;
            // Clicking a run folds/unfolds it; clicking a job shows its log.
            if let Some(Some(node)) = self.hits.actions_tree_rows.get(offset + (pos.y - r.y) as usize).copied() {
                self.select_node(node);
                if self.actions_sel == node {
                    self.run(Action::TreeToggle);
                }
            }
            return;
        }
        if let Some(r) = self.hits.info
            && r.contains(pos)
        {
            self.focus = Focus::Detail;
            let row = self.info_scroll + (pos.y - r.y) as usize;
            let x = pos.x.saturating_sub(self.hits.detail_text_x);
            let link = self.hits.info_links.get(row).and_then(|links| links.iter().find(|(a, b, _)| x >= *a && x < *b));
            if let Some((_, _, url)) = link.cloned() {
                self.run(Action::OpenUrl(url));
            }
            return;
        }
        if let Some(r) = self.hits.detail
            && r.contains(pos)
        {
            self.focus = Focus::Detail;
            let row = (self.detail_scroll + (pos.y - r.y) as usize).saturating_sub(self.hits.detail_links_from);
            let x = pos.x.saturating_sub(self.hits.detail_text_x);
            let link = self.hits.detail_links.get(row).and_then(|links| links.iter().find(|(a, b, _)| x >= *a && x < *b));
            if let Some((_, _, url)) = link.cloned() {
                self.run(Action::OpenUrl(url));
            }
        }
    }

    // ---------- actions ----------

    pub fn run(&mut self, action: Action) {
        match action {
            Action::Quit => self.quit = true,
            Action::Back => {
                self.screen = Screen::List;
                self.focus = Focus::Sidebar;
                self.end_log_search_input();
            }
            Action::Refresh => {
                self.refresh();
                if self.screen == Screen::Detail
                    && self.tab == Tab::Actions
                    && let Some(job) = self.selected_job().map(|j| j.id).filter(|j| self.log_wanted.contains(j))
                {
                    self.ensure_log(job, true);
                }
                self.toast("Refreshing…", ToastKind::Info);
            }
            Action::OpenScopes => {
                let sel = scope_matches(&self.scopes, "", self.repo.is_some()).iter().position(|&i| i == self.scope).unwrap_or(0);
                self.modal = Some(Modal::Scopes { filter: String::new(), sel });
            }
            Action::CycleDensity => {
                self.state.density = self.state.density.next();
                crate::state::save(&self.state);
                self.toast(format!("Density: {}", self.state.density.label()), ToastKind::Info);
            }
            Action::OpenRepos => {
                // Pre-select the active repo, else "This PR's repo", else "All repositories".
                let items = repo_items(self, "");
                let values: Vec<Option<String>> = items.iter().filter_map(RepoItem::value).collect();
                let sel = match &self.repo {
                    Some(r) => values.iter().position(|v| v.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(r))),
                    None => items.iter().any(|i| matches!(i, RepoItem::ThisPr(..))).then_some(1),
                }
                .unwrap_or(0);
                self.modal = Some(Modal::Repos { filter: String::new(), sel });
                if self.repos.is_empty() && self.viewer.is_some() {
                    self.load_repos();
                }
            }
            Action::SetRepo(repo) => {
                self.modal = None;
                if let Some(r) = &repo {
                    self.state.push_recent(r);
                    crate::state::save(&self.state);
                }
                if repo == self.repo {
                    return;
                }
                self.repo = repo;
                if self.repo.is_none() && self.scopes[self.scope].qualifier.is_empty() {
                    self.scope = 0;
                    self.toast(format!("Scope reset to {}", self.scopes[0].name), ToastKind::Info);
                }
                self.pending_select = self.selected_pr().map(|p| p.id.clone());
                self.prs.clear();
                self.total = 0;
                self.sel = 0;
                self.load_list();
            }
            Action::ToggleRepoOfSelected => {
                let Some(nwo) = self.selected_pr().map(|p| p.repository.name_with_owner.clone()) else { return };
                let same = self.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&nwo));
                self.run(Action::SetRepo(if same { None } else { Some(nwo) }));
            }
            Action::CycleState => {
                self.state_filter = self.state_filter.next();
                self.sel = 0;
                self.prs.clear();
                self.total = 0;
                self.load_list();
            }
            Action::StartFilter => {
                self.screen = Screen::List;
                self.filter_input = true;
            }
            Action::ClearFilter => {
                self.filter.clear();
                self.sel = 0;
            }
            Action::OpenSelected => {
                if self.selected_pr().is_some() {
                    self.screen = Screen::Detail;
                    self.focus = Focus::Detail;
                    self.detail_scroll = 0;
                    self.ensure_detail();
                }
            }
            Action::FocusSidebar => self.focus = Focus::Sidebar,
            Action::FocusDetail => self.focus = Focus::Detail,
            Action::ToggleFocus => {
                self.focus = if self.focus == Focus::Sidebar { Focus::Detail } else { Focus::Sidebar };
            }
            Action::Comment => self.open_composer(ComposeKind::Comment),
            Action::RequestChanges => {
                if let Some(why) = self.review_block() {
                    return self.toast(format!("Can't request changes: {why}"), ToastKind::Error);
                }
                self.open_composer(ComposeKind::RequestChanges)
            }
            Action::Approve => {
                if let Some(why) = self.review_block() {
                    return self.toast(format!("Can't approve: {why}"), ToastKind::Error);
                }
                self.confirm(PendingOp::Approve)
            }
            Action::Menu => self.open_menu(false),
            Action::MergeMenu => self.open_menu(true),
            Action::CloseOrReopen => {
                if let Some(d) = self.need_detail() {
                    let op = if d.state == "CLOSED" { PendingOp::Reopen } else { PendingOp::Close };
                    self.confirm(op);
                }
            }
            Action::Browser => {
                if let Some(url) = self.selected_pr().map(|p| p.url.clone()) {
                    self.run(Action::OpenUrl(url));
                }
            }
            Action::SetTab(t) => {
                if t != self.tab {
                    self.tab = t;
                    self.detail_scroll = 0;
                    self.end_log_search_input();
                }
                self.focus = Focus::Detail;
                self.ensure_files(false);
                self.ensure_runs(false);
            }
            Action::NextTab => self.run(Action::SetTab(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()])),
            Action::PrevTab => self.run(Action::SetTab(Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()])),
            Action::NextFile => self.select_file(self.file_sel + 1),
            Action::PrevFile => self.select_file(self.file_sel.saturating_sub(1)),
            Action::ToggleInfo => {
                if self.wide_tab() {
                    self.changes_info_expanded = !self.changes_info_expanded;
                } else {
                    self.info_collapsed = !self.info_collapsed;
                }
                self.info_scroll = 0;
            }
            Action::ToggleImages => {
                if !self.images.available() {
                    return self.toast(format!("Images are off: {}", self.images.off_reason), ToastKind::Info);
                }
                self.images.enabled = !self.images.enabled;
                self.state.images_enabled = self.images.enabled;
                crate::state::save(&self.state);
                self.tab_cache.clear();
                let msg = if self.images.enabled { format!("Images on ({})", self.images.protocol_label()) } else { "Images off".to_string() };
                self.toast(msg, ToastKind::Info);
            }
            Action::ViewImage(key) => {
                let alt = self.last_placements.1.iter().find(|p| p.key == key).map(|p| p.alt.clone());
                self.modal = Some(Modal::Image { key, alt: alt.filter(|a| !a.is_empty()).unwrap_or_else(|| "Image".into()) });
            }
            Action::OpenUrl(url) if url.starts_with('#') => self.run(Action::Internal(url[1..].to_string())),
            Action::OpenUrl(url) => match open_url(&url) {
                Ok(()) => self.toast(format!("Opened {url}"), ToastKind::Info),
                Err(e) => self.toast(format!("Couldn't open browser: {e}"), ToastKind::Error),
            },
            Action::TreeNext => self.move_tree(1),
            Action::TreePrev => self.move_tree(-1),
            Action::TreeToggle => match self.selected_node() {
                Some(TreeNode::Run(i)) => {
                    let Some(id) = self.current_runs().map(|e| e.runs[i].id) else { return };
                    if !self.actions_expanded.remove(&id) {
                        self.actions_expanded.insert(id);
                        self.ensure_jobs(id, false);
                    }
                }
                Some(TreeNode::Job(..)) => {
                    let Some(id) = self.selected_job().map(|j| j.id) else { return };
                    self.log_wanted.insert(id);
                    self.ensure_log(id, false);
                    self.detail_scroll = usize::MAX;
                }
                _ => {}
            },
            Action::TreeOpenBrowser => {
                let url = match self.selected_node() {
                    Some(TreeNode::Run(i)) => self.current_runs().map(|e| e.runs[i].html_url.clone()),
                    Some(TreeNode::Job(..)) => self.selected_job().and_then(|j| j.html_url.clone()),
                    _ => None,
                };
                match url {
                    Some(u) => self.run(Action::OpenUrl(u)),
                    None => self.toast("Select a run or job first", ToastKind::Info),
                }
            }
            Action::ToggleHistory => {
                let Some(d) = self.selected_detail() else { return };
                if d.is_cross_repository {
                    return self.toast("Run history isn't available for PRs from forks yet", ToastKind::Info);
                }
                let id = d.id.clone();
                let on = !self.actions_history.remove(&id);
                if on {
                    self.actions_history.insert(id);
                }
                self.ensure_runs(true);
                self.toast(if on { "Showing runs from earlier pushes too" } else { "Showing runs for the head commit only" }, ToastKind::Info);
            }
            Action::ActionsBack => {
                if self.log_view.search.take().is_some() {
                    return;
                }
                match self.selected_node() {
                    // Close the job back to its run.
                    Some(TreeNode::Job(run_id, _)) => self.reselect(Some((run_id, None))),
                    _ => self.run(Action::Back),
                }
            }
            Action::LogNextError | Action::LogNextWarning => {
                let Some(job) = self.shown_log() else { return self.toast("Open a job's log first (Enter on a job)", ToastKind::Info) };
                let (what, targets) = match action {
                    Action::LogNextError => ("errors", self.logs[&job].0.errors.clone()),
                    _ => ("warnings", self.logs[&job].0.warnings.clone()),
                };
                if !self.jump_next(&targets, false) {
                    self.toast(format!("No {what} in this log"), ToastKind::Info);
                }
            }
            Action::LogFold => {
                let Some(job) = self.shown_log() else { return };
                let log = &self.logs[&job].0;
                let line = self.log_view.cursor.filter(|c| self.log_view.visible.binary_search(c).is_ok()).or(self.hits.log_top_line);
                let Some(g) = line.and_then(|l| log.lines.get(l)).and_then(|l| l.group) else {
                    return self.toast("Not inside a group", ToastKind::Info);
                };
                let start = log.groups[g].start;
                self.run(Action::Internal(format!("fold:{g}")));
                self.log_view.cursor = Some(start);
                self.log_view.scroll_to = Some(start);
            }
            Action::LogFoldAll => {
                let Some(job) = self.shown_log() else { return };
                let n = self.logs[&job].0.groups.len();
                self.log_view.folds = if self.log_view.folds.len() < n { (0..n).collect() } else { HashSet::new() };
                self.refold();
            }
            Action::LogTimestamps => {
                self.log_view.timestamps = !self.log_view.timestamps;
            }
            Action::LogSearch => {
                if self.shown_log().is_none() {
                    return self.toast("Open a job's log first (Enter on a job)", ToastKind::Info);
                }
                let s = self.log_view.search.get_or_insert_with(|| LogSearch { query: String::new(), input: true, hits: Vec::new() });
                s.input = true;
            }
            Action::LogNextHit | Action::LogPrevHit => {
                if self.shown_log().is_some() && !self.run_search_jump(action == Action::LogPrevHit) {
                    self.toast("No matches", ToastKind::Info);
                }
            }
            Action::Internal(link) => {
                if let Some(key) = link.strip_prefix("image:") {
                    return self.run(Action::ViewImage(key.to_string()));
                }
                let (kind, arg) = link.split_once(':').unwrap_or((&link, ""));
                match (kind, arg.parse::<u64>()) {
                    ("fold", Ok(g)) => {
                        if self.shown_log().is_some() {
                            let g = g as usize;
                            if !self.log_view.folds.remove(&g) {
                                self.log_view.folds.insert(g);
                            }
                            self.refold();
                        }
                    }
                    ("line", Ok(line)) => self.jump_to_line(line as usize),
                    ("job", Ok(job)) => {
                        let Some(run) = self.find_job(job).map(|j| j.run_id) else { return };
                        self.actions_expanded.insert(run);
                        self.reselect(Some((run, Some(job))));
                        self.run(Action::TreeToggle);
                    }
                    _ => {}
                }
            }
            Action::CopyCheckout => {
                if let Some(p) = self.selected_pr() {
                    let cmd = format!("gh pr checkout {} -R {}", p.number, p.repository.name_with_owner);
                    copy_to_clipboard(&cmd);
                    self.toast(format!("Copied: {cmd}"), ToastKind::Success);
                }
            }
            Action::Help => self.modal = Some(Modal::Help),
            Action::ModalPick(i) => self.modal_pick(i),
            Action::ModalCancel => {
                if let Some(Modal::Composer(c)) = self.modal.take()
                    && !c.editor.is_blank()
                {
                    self.drafts.insert(c.pr_id.clone(), c.editor);
                    self.toast("Draft kept — press c to continue", ToastKind::Info);
                }
            }
            Action::ConfirmYes => self.confirm_yes(),
            Action::Submit => self.submit_composer(),
            Action::InsertAt => {
                if let Some(Modal::Composer(c)) = self.modal.as_mut() {
                    c.editor.insert_char('@');
                }
            }
            Action::AcceptMention(i) => {
                let cands = self.mention_candidates();
                if let (Some(login), Some(Modal::Composer(c))) = (cands.get(i), self.modal.as_mut())
                    && let Some((start, _)) = c.editor.mention_prefix()
                {
                    c.editor.complete_mention(start, login);
                    c.popup_sel = 0;
                }
            }
        }
    }

    /// Why the viewer can't review the selected PR, if its detail is loaded.
    fn review_block(&self) -> Option<String> {
        self.selected_detail().and_then(|d| review_block(d, self.viewer.as_ref()))
    }

    /// The selected PR's loaded detail, or a toast explaining it's still loading.
    fn need_detail(&mut self) -> Option<PrDetail> {
        if let Some(d) = self.selected_detail() {
            return Some(d.clone());
        }
        if self.selected_pr().is_some() {
            self.ensure_detail();
            self.toast("Loading PR details… try again in a moment", ToastKind::Info);
        }
        None
    }

    fn open_composer(&mut self, kind: ComposeKind) {
        let Some(p) = self.selected_pr() else { return };
        let (id, label) = (p.id.clone(), format!("{}#{}", p.repository.name_with_owner, p.number));
        self.ensure_detail();
        let editor = self.drafts.remove(&id).unwrap_or_default();
        self.modal = Some(Modal::Composer(Box::new(Composer {
            pr_id: id,
            pr_label: label,
            kind,
            editor,
            popup_sel: 0,
            popup_dismissed: None,
            submitting: false,
        })));
    }

    fn open_menu(&mut self, merge_only: bool) {
        let Some(d) = self.need_detail() else { return };
        let open = d.state == "OPEN";
        let can_write = d.repository.can_write();
        let merge_block = if !open {
            Some(format!("PR is {}", d.state.to_lowercase()))
        } else if d.is_draft {
            Some("PR is a draft".into())
        } else if d.mergeable == "CONFLICTING" {
            Some("has merge conflicts".into())
        } else if !can_write {
            Some("no write access".into())
        } else if d.merge_state_status.as_deref() == Some("BLOCKED") && !d.viewer_can_merge_as_admin {
            Some("blocked by branch protection".into())
        } else {
            None
        };
        let mut entries = Vec::new();
        let repo = &d.repository;
        for (allowed, method, key, label) in [
            (repo.squash_merge_allowed, MergeMethod::Squash, 's', "Squash and merge"),
            (repo.merge_commit_allowed, MergeMethod::Merge, 'm', "Create a merge commit"),
            (repo.rebase_merge_allowed, MergeMethod::Rebase, 'r', "Rebase and merge"),
        ] {
            if allowed {
                let mut disabled = merge_block.clone();
                if disabled.is_none() && method == MergeMethod::Rebase && !d.can_be_rebased {
                    disabled = Some("can't be rebased cleanly".into());
                }
                entries.push(MenuEntry { item: MenuItem::Merge(method), label: label.into(), key, disabled });
            }
        }
        if entries.is_empty() {
            self.toast("This repository allows no merge methods", ToastKind::Error);
        }
        if !merge_only {
            let review_block = review_block(&d, self.viewer.as_ref());
            entries.push(MenuEntry { item: MenuItem::Comment, label: "Comment…".into(), key: 'c', disabled: None });
            entries.push(MenuEntry { item: MenuItem::Approve, label: "Approve".into(), key: 'a', disabled: review_block.clone() });
            entries.push(MenuEntry { item: MenuItem::RequestChanges, label: "Request changes…".into(), key: 'R', disabled: review_block });
            let (item, label, key) = match d.state.as_str() {
                "CLOSED" => (MenuItem::Reopen, "Reopen pull request", 'o'),
                _ => (MenuItem::Close, "Close pull request", 'x'),
            };
            let permitted = if item == MenuItem::Reopen { d.viewer_can_reopen } else { d.viewer_can_close };
            let disabled = if d.state == "MERGED" { Some("already merged".into()) } else if !permitted { Some("no permission".into()) } else { None };
            entries.push(MenuEntry { item, label: label.into(), key, disabled });
            entries.push(MenuEntry { item: MenuItem::Browser, label: "Open in browser".into(), key: 'b', disabled: None });
            entries.push(MenuEntry { item: MenuItem::CopyCheckout, label: "Copy `gh pr checkout` command".into(), key: 'y', disabled: None });
            entries.push(MenuEntry { item: MenuItem::CopyUrl, label: "Copy PR URL".into(), key: 'u', disabled: None });
            let label = if self.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&d.repository.name_with_owner)) {
                "Show all repositories".to_string()
            } else {
                format!("Only PRs in {}", d.repository.name_with_owner)
            };
            entries.push(MenuEntry { item: MenuItem::ToggleRepo, label, key: 'e', disabled: None });
        }
        if entries.is_empty() {
            return;
        }
        let sel = entries.iter().position(|e| e.disabled.is_none()).unwrap_or(0);
        self.modal = Some(Modal::Menu { pr_id: d.id.clone(), entries, sel });
    }

    fn confirm(&mut self, op: PendingOp) {
        let Some(d) = self.need_detail() else { return };
        let what = format!("{}#{}", d.repository.name_with_owner, d.number);
        let (title, message) = match &op {
            PendingOp::Merge(m, _) => {
                let verb = match m {
                    MergeMethod::Squash => "Squash and merge",
                    MergeMethod::Merge => "Merge (merge commit)",
                    MergeMethod::Rebase => "Rebase and merge",
                };
                (verb.to_string(), format!("{verb} {what}\n“{}”\n\n{} → {}", d.title, d.head_ref_name, d.base_ref_name))
            }
            PendingOp::Close => ("Close PR".into(), format!("Close {what} without merging?\n“{}”", d.title)),
            PendingOp::Reopen => ("Reopen PR".into(), format!("Reopen {what}?\n“{}”", d.title)),
            PendingOp::Approve => ("Approve PR".into(), format!("Submit an approving review on {what}?\n“{}”", d.title)),
        };
        self.modal = Some(Modal::Confirm { pr_id: d.id.clone(), title, message, op });
    }

    fn modal_pick(&mut self, i: usize) {
        match self.modal.take() {
            // The image viewer's footer: 0 opens the image in the browser, 1 copies its URL.
            Some(Modal::Image { key, alt }) => {
                match i {
                    0 => self.run(Action::OpenUrl(key.clone())),
                    _ => {
                        copy_to_clipboard(&key);
                        self.toast(format!("Copied: {key}"), ToastKind::Success);
                    }
                }
                self.modal = Some(Modal::Image { key, alt });
            }
            Some(Modal::Repos { filter, sel }) => {
                let values: Vec<Option<String>> = repo_items(self, &filter).iter().filter_map(RepoItem::value).collect();
                match values.get(i) {
                    Some(v) => self.run(Action::SetRepo(v.clone())),
                    None => self.modal = Some(Modal::Repos { filter, sel }),
                }
            }
            Some(Modal::Scopes { filter, sel }) => {
                let matches = scope_matches(&self.scopes, &filter, self.repo.is_some());
                match matches.get(i) {
                    Some(&idx) => {
                        if idx != self.scope || self.prs.is_empty() {
                            self.scope = idx;
                            self.sel = 0;
                            self.prs.clear();
                self.total = 0;
                            self.screen = Screen::List;
                            self.load_list();
                        }
                    }
                    None => self.modal = Some(Modal::Scopes { filter, sel }),
                }
            }
            Some(Modal::Menu { pr_id, entries, sel: _ }) => {
                let Some(e) = entries.get(i).cloned() else { return };
                if let Some(why) = &e.disabled {
                    self.toast(format!("{}: {why}", e.label.trim_end_matches('…')), ToastKind::Error);
                    self.modal = Some(Modal::Menu { pr_id, entries, sel: i });
                    return;
                }
                match e.item {
                    MenuItem::Merge(m) => {
                        let oid = self.selected_detail().map(|d| d.head_ref_oid.clone()).unwrap_or_default();
                        self.confirm(PendingOp::Merge(m, oid))
                    }
                    MenuItem::Comment => self.open_composer(ComposeKind::Comment),
                    MenuItem::Approve => self.confirm(PendingOp::Approve),
                    MenuItem::RequestChanges => self.open_composer(ComposeKind::RequestChanges),
                    MenuItem::Close => self.confirm(PendingOp::Close),
                    MenuItem::Reopen => self.confirm(PendingOp::Reopen),
                    MenuItem::Browser => self.run(Action::Browser),
                    MenuItem::CopyCheckout => self.run(Action::CopyCheckout),
                    MenuItem::ToggleRepo => self.run(Action::ToggleRepoOfSelected),
                    MenuItem::CopyUrl => {
                        if let Some(url) = self.selected_pr().map(|p| p.url.clone()) {
                            copy_to_clipboard(&url);
                            self.toast(format!("Copied: {url}"), ToastKind::Success);
                        }
                    }
                }
            }
            other => self.modal = other,
        }
    }

    fn confirm_yes(&mut self) {
        let Some(Modal::Confirm { pr_id, op, .. }) = self.modal.take() else { return };
        if !self.busy.insert(pr_id.clone()) {
            return self.toast("An action is already running for this PR", ToastKind::Info);
        }
        let (client, tx, id) = (self.client.clone(), self.tx.clone(), pr_id);
        self.toast("Working…", ToastKind::Info);
        tokio::spawn(async move {
            let res = match op {
                PendingOp::Merge(m, oid) => client.merge(&id, m, &oid).await.map(|_| "Pull request merged".to_string()),
                PendingOp::Close => client.close(&id).await.map(|_| "Pull request closed".to_string()),
                PendingOp::Reopen => client.reopen(&id).await.map(|_| "Pull request reopened".to_string()),
                PendingOp::Approve => client.review(&id, ReviewEvent::Approve, "").await.map(|_| "Approved".to_string()),
            };
            let _ = tx.send(Msg::Done { id, res });
        });
    }

    fn submit_composer(&mut self) {
        let Some(Modal::Composer(c)) = self.modal.as_mut() else { return };
        if c.submitting {
            return;
        }
        let body = c.editor.text().trim().to_string();
        if body.is_empty() && c.kind != ComposeKind::Approve {
            return self.toast("Write something first", ToastKind::Error);
        }
        c.submitting = true;
        let (client, tx, id, kind) = (self.client.clone(), self.tx.clone(), c.pr_id.clone(), c.kind);
        self.busy.insert(id.clone());
        tokio::spawn(async move {
            let res = match kind {
                ComposeKind::Comment => client.add_comment(&id, &body).await.map(|_| "Comment posted".to_string()),
                ComposeKind::Approve => client.review(&id, ReviewEvent::Approve, &body).await.map(|_| "Approved".to_string()),
                ComposeKind::RequestChanges => {
                    client.review(&id, ReviewEvent::RequestChanges, &body).await.map(|_| "Changes requested".to_string())
                }
            };
            let _ = tx.send(Msg::Done { id, res });
        });
    }

    /// Autocomplete candidates for the `@mention` under the composer cursor.
    pub fn mention_candidates(&self) -> Vec<String> {
        let Some(Modal::Composer(c)) = &self.modal else { return Vec::new() };
        let Some(prefix) = c.editor.mention_prefix() else { return Vec::new() };
        if c.popup_dismissed.as_ref() == Some(&prefix) {
            return Vec::new();
        }
        let needle = prefix.1.to_lowercase();
        let mut pool: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |s: &str| {
            if seen.insert(s.to_lowercase()) {
                pool.push(s.to_string());
            }
        };
        for b in &self.cfg.bots {
            push(b);
        }
        if let Some((d, _)) = self.details.get(&c.pr_id) {
            if let Some(a) = &d.author {
                push(&a.login);
            }
            // Bots never show up in participants/mentionableUsers, but they do author comments.
            let authors = d.comments.iter().filter_map(|c| c.author.as_ref());
            let authors = authors.chain(d.reviews.iter().filter_map(|r| r.author.as_ref()));
            let authors = authors.chain(d.review_threads.iter().flat_map(|t| t.comments.iter().filter_map(|c| c.author.as_ref())));
            authors.for_each(|l| push(l.login.trim_end_matches("[bot]")));
            d.participants.iter().for_each(|l| push(&l.login));
            d.repository.mentionable_users.iter().for_each(|l| push(&l.login));
        }
        let me = self.viewer.as_ref().map(|v| v.login.to_lowercase());
        let mut starts: Vec<String> = Vec::new();
        let mut contains: Vec<String> = Vec::new();
        for p in pool {
            let l = p.to_lowercase();
            if Some(&l) == me.as_ref() {
                continue;
            }
            if l.starts_with(&needle) {
                starts.push(p);
            } else if l.contains(&needle) {
                contains.push(p);
            }
        }
        starts.extend(contains);
        starts.truncate(8);
        starts
    }
}

fn review_block(d: &PrDetail, viewer: Option<&Viewer>) -> Option<String> {
    if d.state != "OPEN" {
        Some("PR is not open".into())
    } else if viewer.is_some_and(|v| d.author.as_ref().is_some_and(|a| a.login == v.login)) {
        Some("can't review your own PR".into())
    } else {
        None
    }
}

pub fn scope_matches(scopes: &[Scope], filter: &str, repo_set: bool) -> Vec<usize> {
    let f = filter.to_lowercase();
    scopes
        .iter()
        .enumerate()
        .filter(|(_, s)| repo_set || !s.qualifier.is_empty())
        .filter(|(_, s)| f.is_empty() || s.name.to_lowercase().contains(&f) || s.qualifier.to_lowercase().contains(&f))
        .map(|(i, _)| i)
        .collect()
}

fn open_url(url: &str) -> std::io::Result<()> {
    let cmd = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Copy via the OSC 52 terminal escape, which works over SSH too.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_reference() {
        assert_eq!(super::base64(b"gh pr"), "Z2ggcHI=");
        assert_eq!(super::base64(b"ab"), "YWI=");
        assert_eq!(super::base64(b"abc"), "YWJj");
    }

    #[test]
    fn files_reply_is_kept_only_for_the_loaded_head() {
        assert!(super::files_match_head(Some("abc"), "abc"));
        assert!(!super::files_match_head(Some("def"), "abc"));
        assert!(super::files_match_head(None, "abc"));
    }

    fn job(attempt: u64) -> super::Job {
        super::Job {
            id: attempt,
            run_id: 1,
            run_attempt: attempt,
            name: String::new(),
            status: "completed".into(),
            conclusion: None,
            started_at: None,
            completed_at: None,
            html_url: None,
            labels: Vec::new(),
            steps: Vec::new(),
        }
    }

    #[test]
    fn rerun_refetches_only_an_older_attempt() {
        assert!(!super::rerun(&[job(2)], Some(2)));
        assert!(super::rerun(&[job(1)], Some(2)));
        assert!(!super::rerun(&[job(2)], Some(1)));
        assert!(!super::rerun(&[job(1), job(3)], Some(3)));
        assert!(super::rerun(&[job(1), job(2)], Some(3)));
        assert!(!super::rerun(&[], Some(2)));
        assert!(!super::rerun(&[job(1)], None));
    }

    #[test]
    fn cached_repos_stay_hidden_until_the_login_matches() {
        let cache = super::ReposCache {
            login: "ada".into(),
            repos: vec![super::RepoInfo { name_with_owner: "ada/private".into(), is_private: true, pushed_at: None }],
        };
        assert!(super::cached_repos(Some(&cache), "ada").is_some_and(|r| r.len() == 1));
        assert!(super::cached_repos(Some(&cache), "grace").is_none());
        assert!(super::cached_repos(None, "ada").is_none());
    }
}

/// A row in the repo picker. Headers and notes are display-only.
pub enum RepoItem {
    All { count: usize },
    ThisPr(String, Option<usize>),
    Header(String),
    Repo { nwo: String, count: Option<usize>, private: bool, pushed: Option<String> },
    UseTyped(String),
    Note(String),
}

impl RepoItem {
    /// What picking this row sets the filter to; `None` for non-selectable rows.
    pub fn value(&self) -> Option<Option<String>> {
        match self {
            RepoItem::All { .. } => Some(None),
            RepoItem::ThisPr(r, _) | RepoItem::UseTyped(r) | RepoItem::Repo { nwo: r, .. } => Some(Some(r.clone())),
            RepoItem::Header(_) | RepoItem::Note(_) => None,
        }
    }
}

fn looks_like_repo(s: &str) -> bool {
    let mut parts = s.split('/');
    let ok = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// A files reply is for the head currently loaded, or the detail is gone and nothing can contradict it.
fn files_match_head(head: Option<&str>, sha: &str) -> bool {
    head.is_none_or(|h| h == sha)
}

/// Repositories from disk, only when they were saved for `login`.
fn cached_repos(cache: Option<&ReposCache>, login: &str) -> Option<Vec<RepoInfo>> {
    cache.filter(|c| c.login == login).map(|c| c.repos.clone())
}

/// Whether cached jobs are behind the run's `attempt`. A re-run keeps the run id and
/// replaces the jobs. Jobs from a newer attempt than the run list are already current
/// (the list has not caught up); treating that as stale starts the same fetch on every reply.
fn rerun(jobs: &[Job], attempt: Option<u64>) -> bool {
    let cached = jobs.iter().map(|j| j.run_attempt).max();
    attempt.is_some_and(|a| cached.is_some_and(|cached| cached < a))
}

fn count_repos(prs: &[PrSummary]) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for p in prs {
        match counts.iter_mut().find(|(r, _)| *r == p.repository.name_with_owner) {
            Some(e) => e.1 += 1,
            None => counts.push((p.repository.name_with_owner.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

/// Picker rows: sectioned when `filter` is empty, otherwise one ranked flat list.
pub fn repo_items(app: &App, filter: &str) -> Vec<RepoItem> {
    let count_of = |nwo: &str| app.scope_repos.iter().find(|(r, _)| r.eq_ignore_ascii_case(nwo)).map(|(_, c)| *c);
    let info_of = |nwo: &str| app.repos.iter().find(|r| r.name_with_owner.eq_ignore_ascii_case(nwo));
    let repo_row = |nwo: &str| {
        let info = info_of(nwo);
        RepoItem::Repo {
            nwo: info.map_or(nwo.to_string(), |i| i.name_with_owner.clone()),
            count: count_of(nwo),
            private: info.is_some_and(|i| i.is_private),
            pushed: info.and_then(|i| i.pushed_at.clone()),
        }
    };
    let active = |nwo: &str| app.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(nwo));
    let this_pr = app.selected_pr().map(|p| p.repository.name_with_owner.clone()).filter(|r| !active(r));

    // Candidates in tier order, deduplicated.
    let mut seen: HashSet<String> = HashSet::new();
    let mut tiers: Vec<(&str, Vec<String>)> = vec![
        ("In this view", app.scope_repos.iter().map(|(r, _)| r.clone()).collect()),
        ("Recent", app.state.recent_repos.clone()),
        ("Your repositories", app.repos.iter().map(|r| r.name_with_owner.clone()).collect()),
    ];
    for (_, list) in tiers.iter_mut() {
        list.retain(|r| seen.insert(r.to_lowercase()));
    }

    let mut items = vec![RepoItem::All { count: app.scope_repos.iter().map(|(_, c)| c).sum() }];
    let f = filter.trim().to_lowercase();
    if f.is_empty() {
        if let Some(r) = this_pr {
            let c = count_of(&r);
            items.push(RepoItem::ThisPr(r, c));
        }
        for (title, list) in tiers {
            if list.is_empty() {
                continue;
            }
            items.push(RepoItem::Header(title.to_string()));
            items.extend(list.iter().map(|r| repo_row(r)));
        }
    } else {
        // Rank: name starts with > name contains > owner contains; ties keep tier order.
        let mut ranked: Vec<(u8, String)> = Vec::new();
        for (_, list) in &tiers {
            for nwo in list {
                let l = nwo.to_lowercase();
                let (owner, name) = l.split_once('/').unwrap_or(("", &l));
                let rank = if name.starts_with(&f) || l.starts_with(&f) {
                    0
                } else if name.contains(&f) || l.contains(&f) {
                    1
                } else if owner.contains(&f) {
                    2
                } else {
                    continue;
                };
                ranked.push((rank, nwo.clone()));
            }
        }
        ranked.sort_by_key(|(r, _)| *r);
        let exact = ranked.iter().any(|(_, n)| n.eq_ignore_ascii_case(filter.trim()));
        items.extend(ranked.iter().map(|(_, r)| repo_row(r)));
        if !exact && looks_like_repo(filter.trim()) {
            items.push(RepoItem::UseTyped(filter.trim().to_string()));
        }
        if ranked.is_empty() {
            items.push(RepoItem::Note(format!("No repositories match “{}”", filter.trim())));
        }
        // While searching, Enter should pick the best match, so "All" moves to the end.
        let all = items.remove(0);
        items.push(all);
    }
    if app.repos_loading {
        items.push(RepoItem::Note(format!("loading your repositories… ({} of {})", app.repos.len().max(app.repos_fresh.len()), app.repos_total)));
    }
    items
}
