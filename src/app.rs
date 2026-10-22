//! Application state and input handling.

use std::cell::RefCell;
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
    Annotation, Client, FileDiff, Job, MergeMethod, PrBody, PrDetail, PrSummary, RateLimit, RepoInfo, RepoPage, ReviewEvent, RunsQuery, SearchPage,
    Viewer, WorkflowRunInfo,
};
use crate::search::{self, Command, Doc, Match, Query, TokenBucket};
use crate::state::{ReposCache, State};

/// Maximum PRs loaded per scope (GitHub search caps at 1000).
const MAX_PRS: usize = 300;
/// Maximum repos listed in the repo picker.
const MAX_REPOS: usize = 1000;
/// Bytes kept from the end of a job log.
const LOG_CAP: usize = 4 << 20;
/// Search palette: debounce before a GitHub text search / PR number lookup.
const REMOTE_IDLE: Duration = Duration::from_millis(400);
const NUMBER_IDLE: Duration = Duration::from_millis(150);
/// PR numbers are looked up in at most this many repos (one aliased request).
const NUMBER_REPOS: usize = 12;
/// Loaded PRs listed in the palette before a "N more" row.
pub const PALETTE_PRS: usize = 10;
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
    /// PR descriptions for search (the ids asked for, found or not).
    Bodies { ids: Vec<String>, res: Result<(Vec<PrBody>, Option<RateLimit>)> },
    /// A palette GitHub search or PR-number lookup.
    Remote { generation: u64, key: RemoteKey, res: Result<(Vec<PrSummary>, Option<RateLimit>)> },
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

    pub fn next(self) -> Self {
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
    SetState(StateFilter),
    /// Open the search palette (empty), or with the pinned query to edit it.
    OpenSearch,
    EditSearch,
    /// Move the palette highlight to selectable row `i` (clicking a section header); Enter.
    SearchSelect(usize),
    SearchEnter,
    /// Tab-complete the token being typed; append a token (the hint chips).
    SearchComplete,
    SearchInsert(String),
    /// Keep the query as the list filter without opening anything (Ctrl-f).
    PinSearch,
    ClearSearch,
    /// Search GitHub now (Ctrl-g, the `↗` row).
    SearchGitHub,
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
    CopyUrl,
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
    /// The `/` palette; its state lives in `App::search` (shared with the pinned query).
    Search,
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

/// What a palette GitHub request was for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteKey {
    /// A text search, by its GitHub query string.
    Text(String),
    /// PR `n` looked up in `repos`.
    Number { n: u64, repos: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    Text,
    Number(u64),
}

/// GitHub results shown under the local ones.
pub struct Remote {
    pub kind: RemoteKind,
    /// The GitHub query (text searches) the hits are for.
    pub query: String,
    pub hits: Vec<PrSummary>,
    docs: Vec<Doc>,
    pub loading: bool,
    pub error: Option<String>,
    /// Repositories a number lookup covers.
    pub repos: usize,
}

/// A palette row. Headers and notes are display-only.
#[derive(Debug, Clone)]
pub enum Item {
    Header(String),
    Pr { idx: usize, m: Match },
    /// A GitHub result (index into `Remote::hits`) that isn't in the loaded list.
    Remote { i: usize, m: Match },
    /// "N more": pins the query so the list shows them all.
    More(usize),
    Repo { nwo: String, count: Option<usize> },
    Scope(usize),
    Command { cmd: Command, disabled: Option<String> },
    Recent(String),
    SearchGitHub,
    Note(String),
}

impl Item {
    pub fn selectable(&self) -> bool {
        !matches!(self, Item::Header(_) | Item::Note(_))
    }
}

/// Identity of a palette row, so a later GitHub response can put the highlight back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchAnchor {
    Pr(String),
    Remote(String),
    More,
    Repo(String),
    Scope(usize),
    Command(Command),
    Recent(String),
    GitHub,
}

/// Highlight spans for one drawn PR row (char indices per field) and its description snippet.
#[derive(Default)]
pub struct Marks {
    pub title: Vec<u32>,
    pub repo: Vec<u32>,
    pub number: Vec<u32>,
    pub author: Vec<u32>,
    pub snippet: Option<(String, Vec<(usize, usize)>)>,
}

/// The search palette's input plus the query pinned as the list filter.
pub struct Search {
    pub text: String,
    pub pinned: Option<String>,
    /// Highlighted selectable row, and whether the user moved it (Enter on an untouched empty
    /// input just closes).
    pub sel: usize,
    pub moved: bool,
    /// The viewer's login, for `author:@me`.
    me: Option<String>,
    matcher: RefCell<nucleo_matcher::Matcher>,
    /// Parallel to `prs`, for `prs_rev`.
    docs: RefCell<(u64, Vec<Doc>)>,
    /// Ranked loaded PRs for (prs_rev, palette text) and (prs_rev, pinned query).
    results: RefCell<(u64, String, Vec<(usize, Match)>)>,
    pinned_results: RefCell<(u64, String, Vec<usize>)>,
    /// Stripped descriptions by PR id, with the `updatedAt` they were fetched at.
    pub bodies: HashMap<String, (String, String)>,
    pub bodies_loading: HashSet<String>,
    pub remote: Option<Remote>,
    remote_cache: HashMap<String, (Instant, Vec<PrSummary>)>,
    number_cache: HashMap<(String, u64), (Instant, Option<PrSummary>)>,
    remote_gen: u64,
    /// One GitHub request at a time; the next waits for the tick.
    inflight: bool,
    typed_at: Instant,
    /// The text changed since the last remote dispatch.
    dirty: bool,
    /// Ctrl-g / Enter: search now, last word included, even with `live = false`.
    force: bool,
    bucket: TokenBucket,
    pub paused_until: Option<Instant>,
    /// Why remote search is off right now (rate limit).
    pub blocked: Option<String>,
    /// Enter in number mode before the lookup landed: open #n if it turns out unique.
    open_on_arrival: Option<u64>,
    /// GitHub results opened from the palette, spliced into `prs`.
    spliced: HashSet<String>,
    /// Tab cycling: the token as typed and the completion shown.
    tab: Option<(String, usize)>,
}

impl Search {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            text: String::new(),
            pinned: None,
            sel: 0,
            moved: false,
            me: None,
            matcher: RefCell::new(search::matcher()),
            docs: RefCell::new((u64::MAX, Vec::new())),
            results: RefCell::new((u64::MAX, String::new(), Vec::new())),
            pinned_results: RefCell::new((u64::MAX, String::new(), Vec::new())),
            bodies: HashMap::new(),
            bodies_loading: HashSet::new(),
            remote: None,
            remote_cache: HashMap::new(),
            number_cache: HashMap::new(),
            remote_gen: 0,
            inflight: false,
            typed_at: now,
            dirty: false,
            force: false,
            bucket: TokenBucket::new(20, Duration::from_secs(3), now),
            paused_until: None,
            blocked: None,
            open_on_arrival: None,
            spliced: HashSet::new(),
            tab: None,
        }
    }

    pub fn query(&self) -> Query {
        search::parse(&self.text, self.me.as_deref())
    }

    pub fn pinned_query(&self) -> Option<Query> {
        self.pinned.as_deref().map(|p| search::parse(p, self.me.as_deref()))
    }

    /// Seconds until GitHub search may run again, when it's waiting on the budget or a 403.
    pub fn paused_for(&mut self) -> Option<u64> {
        let now = Instant::now();
        let pause = self.paused_until.filter(|t| *t > now).map(|t| t - now);
        let wait = Some(self.bucket.wait(now)).filter(|w| !w.is_zero());
        pause.max(wait).map(|d| d.as_secs().max(1))
    }
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
    /// Bumped whenever `prs` (or a description) changes; keys the search caches.
    pub prs_rev: u64,
    pub search: Search,
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
            prs_rev: 0,
            search: Search::new(),
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
                let res = client.search_page(&q, after, 50).await;
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

    /// Indices into `prs` the list shows: all of them, or the pinned query's matches, best first.
    pub fn visible(&self) -> Vec<usize> {
        let Some(pinned) = self.search.pinned.as_deref() else { return (0..self.prs.len()).collect() };
        {
            let cache = self.search.pinned_results.borrow();
            if cache.0 == self.prs_rev && cache.1 == pinned {
                return cache.2.iter().copied().filter(|&i| i < self.prs.len()).collect();
            }
        }
        let q = search::parse(pinned, self.search.me.as_deref());
        let mut out: Vec<usize> = self.with_docs(|docs, m| search::rank(m, &q, docs, &self.prs)).into_iter().map(|(i, _)| i).collect();
        // GitHub results opened from the palette stay in view even when they only matched remotely.
        for (i, p) in self.prs.iter().enumerate() {
            if self.search.spliced.contains(&p.id) && !out.contains(&i) {
                out.push(i);
            }
        }
        *self.search.pinned_results.borrow_mut() = (self.prs_rev, pinned.to_string(), out.clone());
        out
    }

    /// Run `f` over the search docs (parallel to `prs`), rebuilding them when `prs` changed.
    fn with_docs<R>(&self, f: impl FnOnce(&[Doc], &mut nucleo_matcher::Matcher) -> R) -> R {
        {
            let mut d = self.search.docs.borrow_mut();
            if d.0 != self.prs_rev || d.1.len() != self.prs.len() {
                let bodies = &self.search.bodies;
                d.1 = self.prs.iter().map(|p| Doc::new(p, bodies.get(&p.id).map(|(_, b)| b.as_str()))).collect();
                d.0 = self.prs_rev;
            }
        }
        let d = self.search.docs.borrow();
        let mut m = self.search.matcher.borrow_mut();
        f(&d.1, &mut m)
    }

    /// The palette text's matches among the loaded PRs, best first (cached per keystroke).
    pub fn palette_results(&self) -> Vec<(usize, Match)> {
        {
            let cache = self.search.results.borrow();
            if cache.0 == self.prs_rev && cache.1 == self.search.text {
                return cache.2.clone();
            }
        }
        let q = self.search.query();
        let out = if q.is_empty() { Vec::new() } else { self.with_docs(|docs, m| search::rank(m, &q, docs, &self.prs)) };
        *self.search.results.borrow_mut() = (self.prs_rev, self.search.text.clone(), out.clone());
        out
    }

    /// Highlights for loaded PR `idx` under query `q`; a snippet `snippet_w` wide when it hit the body.
    pub fn marks(&self, q: &Query, idx: usize, snippet_w: Option<usize>) -> Marks {
        self.with_docs(|docs, m| docs.get(idx).map(|d| marks(m, q, d, snippet_w)).unwrap_or_default())
    }

    /// Highlights for GitHub result `i`.
    pub fn remote_marks(&self, q: &Query, i: usize, snippet_w: Option<usize>) -> Marks {
        let Some(d) = self.search.remote.as_ref().and_then(|r| r.docs.get(i)) else { return Marks::default() };
        marks(&mut self.search.matcher.borrow_mut(), q, d, snippet_w)
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
        let composing = matches!(self.modal, Some(Modal::Composer(_) | Modal::Search));
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
        self.drive_search();
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
                self.search.me = Some(v.login.clone());
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
                        self.queue_bodies(page.prs.iter().map(|p| (p.id.clone(), p.updated_at.clone())).collect());
                        if first {
                            // A GitHub result opened from the palette survives while it's open.
                            let keep: Vec<PrSummary> = self
                                .prs
                                .iter()
                                .filter(|p| self.screen == Screen::Detail && prev.as_deref() == Some(p.id.as_str()) && self.search.spliced.contains(&p.id))
                                .filter(|p| !page.prs.iter().any(|n| n.id == p.id))
                                .cloned()
                                .collect();
                            self.search.spliced.retain(|id| keep.iter().any(|p| p.id == *id));
                            self.prs = keep;
                            self.prs.extend(page.prs);
                        } else {
                            let seen: HashSet<String> = self.prs.iter().map(|p| p.id.clone()).collect();
                            self.prs.extend(page.prs.into_iter().filter(|p| !seen.contains(&p.id)));
                        }
                        self.prs_rev += 1;
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
                let keep = self.selected_pr().map(|p| p.id.clone());
                match res {
                    Ok((d, rate)) => {
                        self.rate = rate.or(self.rate.take());
                        self.detail_errors.remove(&id);
                        // Keep the list row in sync with the fresher detail.
                        let mut reranked = false;
                        if let Some(p) = self.prs.iter_mut().find(|p| p.id == id) {
                            p.title = d.title.clone();
                            p.state = d.state.clone();
                            p.is_draft = d.is_draft;
                            p.review_decision = d.review_decision.clone();
                            self.prs_rev += 1;
                            reranked = true;
                        }
                        self.details.insert(id, (d, Instant::now()));
                        // A pinned query re-ranks when the title or state changes. `sel` is an
                        // ordinal, so put it back on the PR that was open.
                        if reranked {
                            self.retain_list_sel(keep);
                        }
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
            Msg::Bodies { ids, res } => {
                for id in &ids {
                    self.search.bodies_loading.remove(id);
                }
                let Ok((bodies, rate)) = res else { return };
                let keep = self.selected_pr().map(|p| p.id.clone());
                let anchor = self.capture_search_sel();
                self.rate = rate.or(self.rate.take());
                for b in bodies {
                    let stripped = search::strip_body(&b.body, search::BODY_CAP);
                    if let Some(r) = self.search.remote.as_mut()
                        && let Some(i) = r.hits.iter().position(|p| p.id == b.id)
                    {
                        r.docs[i].set_body(&stripped);
                    }
                    self.search.bodies.insert(b.id, (b.updated_at, stripped));
                }
                if self.search.bodies.len() > 1000 {
                    let alive: HashSet<&str> =
                        self.prs.iter().map(|p| p.id.as_str()).chain(self.search.remote.iter().flat_map(|r| r.hits.iter().map(|p| p.id.as_str()))).collect();
                    self.search.bodies.retain(|id, _| alive.contains(id.as_str()));
                }
                self.prs_rev += 1;
                // New description matches can insert a PR ahead of the one on screen.
                self.retain_list_sel(keep);
                self.restore_search_sel(anchor);
            }
            Msg::Remote { generation, key, res } => self.on_remote(generation, key, res),
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
                } else if matches!(self.modal, Some(Modal::Search)) {
                    let text = format!("{}{}", self.search.text, s.replace(['\n', '\r', '\t'], " "));
                    self.set_search_text(text);
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
            KeyCode::Esc => Some(if self.screen == Screen::Detail { Action::Back } else { Action::ClearSearch }),
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Char('r') | KeyCode::F(5) => Some(Action::Refresh),
            KeyCode::Char('s') => Some(Action::OpenScopes),
            KeyCode::Char('S') => Some(Action::OpenRepos),
            KeyCode::Char('D') => Some(Action::CycleDensity),
            KeyCode::Char('I') => Some(Action::ToggleImages),
            KeyCode::Char('e') => Some(Action::ToggleRepoOfSelected),
            KeyCode::Char('f') => Some(Action::CycleState),
            KeyCode::Char('/') => Some(Action::OpenSearch),
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
        // `/` opens the palette over modals that aren't text inputs or yes/no questions.
        if k.code == KeyCode::Char('/') && matches!(self.modal, Some(Modal::Help | Modal::Menu { .. } | Modal::Image { .. })) {
            return self.run(Action::OpenSearch);
        }
        match self.modal.as_mut() {
            Some(Modal::Help) => self.modal = None,
            Some(Modal::Search) => self.on_search_key(k),
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
                Modal::Search => self.move_search(delta),
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
            if outside && matches!(self.modal, Some(Modal::Search)) {
                self.close_search();
            } else if outside && !matches!(self.modal, Some(Modal::Composer(_))) {
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
                self.search.remote_cache.clear();
                self.repo = repo;
                if self.repo.is_none() && self.scopes[self.scope].qualifier.is_empty() {
                    self.scope = 0;
                    self.toast(format!("Scope reset to {}", self.scopes[0].name), ToastKind::Info);
                }
                self.pending_select = self.selected_pr().map(|p| p.id.clone());
                self.prs.clear();
                self.prs_rev += 1;
                self.total = 0;
                self.sel = 0;
                self.load_list();
            }
            Action::ToggleRepoOfSelected => {
                let Some(nwo) = self.selected_pr().map(|p| p.repository.name_with_owner.clone()) else { return };
                let same = self.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&nwo));
                self.run(Action::SetRepo(if same { None } else { Some(nwo) }));
            }
            Action::CycleState => self.run(Action::SetState(self.state_filter.next())),
            Action::SetState(s) => {
                if s == self.state_filter && !self.prs.is_empty() {
                    return;
                }
                self.state_filter = s;
                self.search.remote_cache.clear();
                self.pending_select = self.selected_pr().map(|p| p.id.clone());
                self.sel = 0;
                self.prs.clear();
                self.prs_rev += 1;
                self.total = 0;
                self.load_list();
            }
            Action::OpenSearch | Action::EditSearch => {
                // Never over a draft or a yes/no question (a header click while one is open).
                if matches!(self.modal, Some(Modal::Composer(_) | Modal::Confirm { .. })) {
                    return;
                }
                let text = if action == Action::EditSearch { self.search.pinned.clone().unwrap_or_default() } else { String::new() };
                self.modal = Some(Modal::Search);
                self.set_search_text(text);
            }
            Action::SearchSelect(i) => {
                self.search.sel = i;
                self.search.moved = true;
            }
            Action::SearchEnter => self.search_enter(),
            Action::SearchComplete => self.complete_search(),
            Action::SearchInsert(token) => {
                let t = &self.search.text;
                let sep = if t.is_empty() || t.ends_with(' ') { "" } else { " " };
                let text = format!("{t}{sep}{token}");
                self.set_search_text(text);
            }
            Action::PinSearch => {
                let text = self.search.text.trim().to_string();
                self.close_search();
                self.pin(if text.is_empty() { None } else { Some(text) }, None);
            }
            Action::ClearSearch => self.pin(None, None),
            Action::SearchGitHub => {
                self.search.force = true;
                self.search.dirty = true;
                self.search.paused_until = self.search.paused_until.filter(|t| *t > Instant::now());
                self.drive_search();
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
            Action::CopyUrl => {
                if let Some(url) = self.selected_pr().map(|p| p.url.clone()) {
                    copy_to_clipboard(&url);
                    self.toast(format!("Copied: {url}"), ToastKind::Success);
                }
            }
            Action::Help => self.modal = Some(Modal::Help),
            Action::ModalPick(i) => self.modal_pick(i),
            Action::ModalCancel if matches!(self.modal, Some(Modal::Search)) => self.close_search(),
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
        let entries = self.menu_entries(&d, merge_only);
        if !entries.iter().any(|e| matches!(e.item, MenuItem::Merge(_))) {
            self.toast("This repository allows no merge methods", ToastKind::Error);
        }
        if entries.is_empty() {
            return;
        }
        let sel = entries.iter().position(|e| e.disabled.is_none()).unwrap_or(0);
        self.modal = Some(Modal::Menu { pr_id: d.id.clone(), entries, sel });
    }

    /// The Actions menu for `d`, each entry with the reason it's disabled.
    fn menu_entries(&self, d: &PrDetail, merge_only: bool) -> Vec<MenuEntry> {
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
        entries
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
                    Some(&idx) => self.set_scope(idx),
                    None => self.modal = Some(Modal::Scopes { filter, sel }),
                }
            }
            Some(Modal::Search) => {
                self.modal = Some(Modal::Search);
                self.pick_search(i);
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
                    MenuItem::CopyUrl => self.run(Action::CopyUrl),
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

    // ---------- search palette ----------

    fn set_search_text(&mut self, text: String) {
        let s = &mut self.search;
        s.text = text;
        s.sel = 0;
        s.moved = false;
        s.tab = None;
        s.typed_at = Instant::now();
        s.dirty = true;
        s.force = false;
        s.open_on_arrival = None;
    }

    /// Close the palette, discarding what was typed (the pinned query stays).
    fn close_search(&mut self) {
        if matches!(self.modal, Some(Modal::Search)) {
            self.modal = None;
        }
        let s = &mut self.search;
        s.text.clear();
        s.remote = None;
        // Late responses for this palette session are dropped.
        s.remote_gen += 1;
        s.dirty = false;
        s.force = false;
        s.open_on_arrival = None;
        s.tab = None;
        s.blocked = None;
    }

    fn on_search_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Esc => self.close_search(),
            KeyCode::Enter => self.search_enter(),
            KeyCode::Char('f') if ctrl => self.run(Action::PinSearch),
            KeyCode::Char('g') if ctrl => self.run(Action::SearchGitHub),
            KeyCode::Char('u') if ctrl => self.set_search_text(String::new()),
            KeyCode::Char('w') if ctrl => self.set_search_text(delete_word(&self.search.text)),
            KeyCode::Backspace if alt || ctrl => self.set_search_text(delete_word(&self.search.text)),
            KeyCode::Char('p') if ctrl => self.move_search(-1),
            KeyCode::Char('n') if ctrl => self.move_search(1),
            KeyCode::Up => self.move_search(-1),
            KeyCode::Down => self.move_search(1),
            KeyCode::PageUp => self.move_search(-10),
            KeyCode::PageDown => self.move_search(10),
            KeyCode::Tab => self.complete_search(),
            KeyCode::Backspace => {
                let mut t = self.search.text.clone();
                t.pop();
                self.set_search_text(t);
            }
            KeyCode::Char(c) if !ctrl => {
                let t = format!("{}{c}", self.search.text);
                self.set_search_text(t);
            }
            _ => {}
        }
    }

    fn selectable_items(&self) -> Vec<Item> {
        search_items(self).into_iter().filter(Item::selectable).collect()
    }

    fn move_search(&mut self, delta: isize) {
        let n = self.selectable_items().len();
        if n == 0 {
            return;
        }
        self.search.sel = (self.search.sel as isize + delta).clamp(0, n as isize - 1) as usize;
        self.search.moved = true;
    }

    /// The PR number of a palette row, if it is one.
    fn item_number(&self, it: &Item) -> Option<u64> {
        match it {
            Item::Pr { idx, .. } => self.prs.get(*idx).map(|p| p.number),
            Item::Remote { i, .. } => self.search.remote.as_ref().and_then(|r| r.hits.get(*i)).map(|p| p.number),
            _ => None,
        }
    }

    fn search_enter(&mut self) {
        let q = self.search.query();
        if q.is_empty() && !self.search.moved {
            return self.close_search();
        }
        let items = self.selectable_items();
        let cur = items.get(self.search.sel);
        // `#14` + Enter before the lookup landed: wait for it, then open #14 if it's unique.
        if let Some(n) = q.number()
            && cur.and_then(|it| self.item_number(it)) != Some(n)
            && self.number_lookup_pending(n)
        {
            self.search.open_on_arrival = Some(n);
            self.search.force = true;
            self.search.dirty = true;
            return self.drive_search();
        }
        match cur {
            Some(_) => self.pick_search(self.search.sel),
            None => self.close_search(),
        }
    }

    fn number_lookup_pending(&self, n: u64) -> bool {
        let loading = self.search.remote.as_ref().is_some_and(|r| r.kind == RemoteKind::Number(n) && r.loading);
        let unknown = || {
            let repos = self.number_repos();
            !repos.is_empty() && repos.iter().any(|r| !self.search.number_cache.get(&(r.to_lowercase(), n)).is_some_and(|e| e.0.elapsed() < Duration::from_secs(600)))
        };
        loading || ((self.search.dirty || !self.cfg.search.live) && unknown())
    }

    fn pick_search(&mut self, i: usize) {
        let items = self.selectable_items();
        let Some(item) = items.get(i).cloned() else { return };
        self.search.sel = i;
        let text = self.search.text.trim().to_string();
        match item {
            Item::Pr { idx, .. } => {
                let id = self.prs[idx].id.clone();
                self.open_from_search(text, id);
            }
            Item::Remote { i, .. } => {
                let Some(pr) = self.search.remote.as_ref().and_then(|r| r.hits.get(i)).cloned() else { return };
                let id = pr.id.clone();
                if !self.prs.iter().any(|p| p.id == id) {
                    self.queue_bodies(vec![(id.clone(), pr.updated_at.clone())]);
                    self.prs.insert(0, pr);
                    self.prs_rev += 1;
                    if self.search.pinned.is_none() {
                        self.sel += 1;
                    }
                    self.search.spliced.insert(id.clone());
                }
                self.open_from_search(text, id);
            }
            Item::More(_) => self.run(Action::PinSearch),
            Item::Repo { nwo, .. } => {
                self.close_search();
                self.run(Action::SetRepo(Some(nwo)));
            }
            Item::Scope(idx) => {
                self.close_search();
                self.set_scope(idx);
            }
            Item::Command { cmd, disabled } => {
                if let Some(why) = disabled {
                    return self.toast(format!("{}: {why}", cmd.label().trim_end_matches('…')), ToastKind::Error);
                }
                self.close_search();
                self.run_command(cmd);
            }
            Item::Recent(r) => self.set_search_text(r),
            Item::SearchGitHub => self.run(Action::SearchGitHub),
            Item::Header(_) | Item::Note(_) => {}
        }
    }

    /// Pin `text`, select PR `id` and show it (list → detail; the detail view stays).
    fn open_from_search(&mut self, text: String, id: String) {
        self.close_search();
        self.pin(if text.is_empty() { None } else { Some(text) }, Some(id));
        if self.screen == Screen::List {
            self.run(Action::OpenSelected);
        } else {
            self.focus = Focus::Detail;
            self.ensure_detail();
        }
    }

    /// Make `text` the list filter (`None` clears it), keeping `select` or the current PR selected.
    fn pin(&mut self, text: Option<String>, select: Option<String>) {
        let prev = self.selected_pr().map(|p| p.id.clone());
        if let Some(t) = &text {
            self.state.push_recent_search(t);
            crate::state::save(&self.state);
        }
        if text != self.search.pinned {
            self.search.spliced.retain(|id| select.as_ref() == Some(id));
        }
        self.search.pinned = text;
        let vis = self.visible();
        let want = select.or(prev.clone());
        self.sel = want.and_then(|id| vis.iter().position(|&i| self.prs[i].id == id)).unwrap_or(0);
        if vis.get(self.sel).map(|&i| self.prs[i].id.clone()) != prev {
            self.detail_scroll = 0;
            self.info_scroll = 0;
            self.file_sel = 0;
            self.file_list_scroll = 0;
            self.actions_sel = 0;
            self.actions_tree_scroll = 0;
            self.actions_tree_follow = true;
            if self.screen == Screen::Detail {
                self.ensure_detail();
            }
        }
    }

    fn set_scope(&mut self, idx: usize) {
        if idx != self.scope || self.prs.is_empty() {
            self.scope = idx;
            self.sel = 0;
            self.prs.clear();
            self.prs_rev += 1;
            self.total = 0;
            self.screen = Screen::List;
            self.search.remote_cache.clear();
            self.load_list();
        }
    }

    /// Values `Tab` completes from: what's in the loaded list.
    fn search_values(&self) -> search::Values {
        let mut v = search::Values::default();
        let add = |list: &mut Vec<String>, s: &str| {
            if !s.is_empty() && !list.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                list.push(s.to_string());
            }
        };
        for (r, _) in &self.scope_repos {
            add(&mut v.repos, r);
        }
        for p in &self.prs {
            add(&mut v.repos, &p.repository.name_with_owner);
            add(&mut v.authors, crate::github::login_of(&p.author));
            add(&mut v.branches, &p.head_ref_name);
            for l in p.labels.iter() {
                add(&mut v.labels, &l.name);
            }
        }
        v
    }

    /// Tab: complete the token being typed; again to cycle through the candidates.
    fn complete_search(&mut self) {
        let (base, idx) = match self.search.tab.clone() {
            Some((b, i)) => (b, i + 1),
            None => (self.search.text.clone(), 0),
        };
        let q = search::parse(&base, self.search.me.as_deref());
        let cands = search::complete(&q, &self.search_values());
        let Some(span) = q.spans.last().filter(|_| !cands.is_empty()) else { return };
        let pick = &cands[idx % cands.len()];
        let mut text = format!("{}{pick}", &base[..span.start]);
        if cands.len() == 1 && !pick.ends_with(':') {
            text.push(' ');
        }
        self.set_search_text(text);
        if cands.len() > 1 {
            self.search.tab = Some((base, idx));
        }
    }

    /// Repos for resolving `repo:` and for number lookups, best first.
    fn known_repos(&self) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let names = self.scope_repos.iter().map(|(r, _)| r).chain(&self.state.recent_repos).chain(self.repos.iter().map(|r| &r.name_with_owner));
        names.filter(|r| seen.insert(r.to_lowercase())).cloned().collect()
    }

    /// Where a PR number is looked up: the active repo, else repos in view (by PR count), then recent ones.
    pub fn number_repos(&self) -> Vec<String> {
        if let Some(r) = &self.repo {
            return vec![r.clone()];
        }
        let in_view = if self.scope_repos.is_empty() { count_repos(&self.prs) } else { self.scope_repos.clone() };
        let mut out: Vec<String> = Vec::new();
        for r in in_view.iter().map(|(r, _)| r).chain(&self.state.recent_repos) {
            if out.len() == NUMBER_REPOS {
                break;
            }
            if !out.iter().any(|x| x.eq_ignore_ascii_case(r)) {
                out.push(r.clone());
            }
        }
        out
    }

    /// The GitHub query for the palette text; `all` counts the word being typed as complete.
    pub fn wanted_remote(&self, q: &Query, all: bool) -> Option<String> {
        let base = self.search_query().replace("sort:updated-desc", "");
        search::remote_query(q, &base, all, &self.known_repos())
    }

    /// The GitHub results, when they belong to what's typed now.
    pub fn current_remote(&self, q: &Query) -> Option<&Remote> {
        let r = self.search.remote.as_ref()?;
        match r.kind {
            RemoteKind::Number(n) => (q.number() == Some(n)).then_some(r),
            RemoteKind::Text => {
                let now = [self.wanted_remote(q, true), self.wanted_remote(q, false)];
                now.iter().flatten().any(|w| *w == r.query).then_some(r)
            }
        }
    }

    fn set_remote(&mut self, kind: RemoteKind, query: String, hits: Vec<PrSummary>, loading: bool, error: Option<String>, repos: usize) {
        // GitHub rows land ahead of repos, scopes and commands. Keep a moved highlight on the same row.
        let anchor = self.capture_search_sel();
        let bodies = &self.search.bodies;
        let docs = hits.iter().map(|p| Doc::new(p, bodies.get(&p.id).map(|(_, b)| b.as_str()))).collect();
        self.search.remote = Some(Remote { kind, query, hits, docs, loading, error, repos });
        self.restore_search_sel(anchor);
    }

    /// Show number-lookup hits and, unless the user moved the highlight, sit it on an exact `#n`.
    fn show_number(&mut self, n: u64, hits: Vec<PrSummary>, loading: bool, error: Option<String>, repos: usize) {
        self.set_remote(RemoteKind::Number(n), format!("#{n}"), hits, loading, error, repos);
        self.prefer_exact_number(n);
    }

    fn prefer_exact_number(&mut self, n: u64) {
        if self.search.moved {
            return;
        }
        let items = self.selectable_items();
        if items.get(self.search.sel).and_then(|it| self.item_number(it)) == Some(n) {
            return;
        }
        if let Some(i) = items.iter().position(|it| self.item_number(it) == Some(n)) {
            self.search.sel = i;
        }
    }

    fn capture_search_sel(&self) -> Option<SearchAnchor> {
        if !self.search.moved || !matches!(self.modal, Some(Modal::Search)) {
            return None;
        }
        self.selectable_items().get(self.search.sel).and_then(|it| self.anchor_of(it))
    }

    fn restore_search_sel(&mut self, saved: Option<SearchAnchor>) {
        let Some(saved) = saved else { return };
        let items = self.selectable_items();
        if let Some(i) = items.iter().position(|it| self.anchor_of(it).as_ref() == Some(&saved)) {
            self.search.sel = i;
            return;
        }
        if !items.is_empty() {
            self.search.sel = self.search.sel.min(items.len() - 1);
        }
    }

    fn anchor_of(&self, it: &Item) -> Option<SearchAnchor> {
        Some(match it {
            Item::Pr { idx, .. } => SearchAnchor::Pr(self.prs.get(*idx)?.id.clone()),
            Item::Remote { i, .. } => SearchAnchor::Remote(self.search.remote.as_ref()?.hits.get(*i)?.id.clone()),
            Item::More(_) => SearchAnchor::More,
            Item::Repo { nwo, .. } => SearchAnchor::Repo(nwo.clone()),
            Item::Scope(i) => SearchAnchor::Scope(*i),
            Item::Command { cmd, .. } => SearchAnchor::Command(*cmd),
            Item::Recent(s) => SearchAnchor::Recent(s.clone()),
            Item::SearchGitHub => SearchAnchor::GitHub,
            Item::Header(_) | Item::Note(_) => return None,
        })
    }

    /// After a pinned list re-ranks, keep `prev` selected (or clamp if it fell out of the filter).
    fn retain_list_sel(&mut self, prev: Option<String>) {
        let vis = self.visible();
        let sel = prev.as_ref().and_then(|id| vis.iter().position(|&i| self.prs[i].id == *id)).unwrap_or_else(|| self.sel.min(vis.len().saturating_sub(1)));
        let now = vis.get(sel).map(|&i| self.prs[i].id.clone());
        if now != prev {
            self.detail_scroll = 0;
            self.info_scroll = 0;
            self.file_sel = 0;
            self.file_list_scroll = 0;
            self.actions_sel = 0;
            self.actions_tree_scroll = 0;
            self.actions_tree_follow = true;
            if self.screen == Screen::Detail {
                self.ensure_detail();
            }
        }
        self.sel = sel;
    }

    /// Whether a GitHub request may go out now (one at a time, 20/min, not while paused or rate limited).
    fn remote_allowed(&mut self) -> bool {
        if self.rate.as_ref().is_some_and(|r| r.remaining < 100) {
            self.search.blocked = Some("rate limited — local results only".into());
            self.search.dirty = false;
            self.search.force = false;
            return false;
        }
        self.search.blocked = None;
        let now = Instant::now();
        if self.search.paused_until.is_some_and(|t| t > now) || self.search.inflight {
            return false;
        }
        self.search.bucket.take(now)
    }

    /// Debounced GitHub search while the palette is open (driven by the 200 ms tick).
    fn drive_search(&mut self) {
        if !matches!(self.modal, Some(Modal::Search)) || !(self.search.dirty || self.search.force) {
            return;
        }
        let q = self.search.query();
        let idle = self.search.typed_at.elapsed();
        let force = self.search.force;
        if q.number_mode {
            match q.number() {
                Some(n) if idle >= NUMBER_IDLE || force => self.dispatch_number(n),
                Some(_) => {}
                None => self.search.dirty = false,
            }
        } else if self.search.text.trim().chars().count() < 3 && !force {
            self.search.dirty = false;
        } else if idle >= REMOTE_IDLE || force {
            self.dispatch_text(&q);
        }
    }

    fn dispatch_text(&mut self, q: &Query) {
        let Some(rq) = self.wanted_remote(q, true) else {
            self.search.dirty = false;
            self.search.force = false;
            return;
        };
        if let Some((_, hits)) = self.search.remote_cache.get(&rq).filter(|(at, _)| at.elapsed() < Duration::from_secs(300)) {
            let hits = hits.clone();
            self.set_remote(RemoteKind::Text, rq, hits, false, None, 0);
            self.search.dirty = false;
            self.search.force = false;
            return;
        }
        let same = self.search.remote.as_ref().is_some_and(|r| r.kind == RemoteKind::Text && r.query == rq && (r.loading || r.error.is_none()));
        if same || (!self.cfg.search.live && !self.search.force) {
            self.search.dirty = false;
            self.search.force = false;
            return;
        }
        if !self.remote_allowed() {
            return;
        }
        self.search.remote_gen += 1;
        self.search.inflight = true;
        self.search.dirty = false;
        self.search.force = false;
        self.set_remote(RemoteKind::Text, rq.clone(), Vec::new(), true, None, 0);
        let (client, tx, generation) = (self.client.clone(), self.tx.clone(), self.search.remote_gen);
        tokio::spawn(async move {
            let res = client.search_page(&rq, None, 30).await.map(|p| (p.prs, p.rate));
            let _ = tx.send(Msg::Remote { generation, key: RemoteKey::Text(rq), res });
        });
    }

    /// Cached by-number hits for `n`, in candidate-repo order.
    fn number_hits(&self, repos: &[String], n: u64) -> Vec<PrSummary> {
        repos.iter().filter_map(|r| self.search.number_cache.get(&(r.to_lowercase(), n)).and_then(|(_, p)| p.clone())).collect()
    }

    fn dispatch_number(&mut self, n: u64) {
        let repos = self.number_repos();
        if repos.is_empty() {
            self.search.remote = None;
            self.search.dirty = false;
            self.search.force = false;
            return;
        }
        let fresh = |e: &(Instant, Option<PrSummary>)| e.0.elapsed() < Duration::from_secs(600);
        let unknown: Vec<String> = repos.iter().filter(|r| !self.search.number_cache.get(&(r.to_lowercase(), n)).is_some_and(fresh)).cloned().collect();
        let hits = self.number_hits(&repos, n);
        if unknown.is_empty() {
            self.search.dirty = false;
            self.search.force = false;
            self.show_number(n, hits, false, None, repos.len());
            return self.check_open_on_arrival();
        }
        if self.search.remote.as_ref().is_some_and(|r| r.kind == RemoteKind::Number(n) && r.loading) {
            self.search.dirty = false;
            return;
        }
        // Show what the cache has while waiting for a slot (or for Enter / Ctrl-g with `live = false`).
        self.show_number(n, hits.clone(), false, None, repos.len());
        if !self.cfg.search.live && !self.search.force {
            self.search.dirty = false;
            return;
        }
        if !self.remote_allowed() {
            return;
        }
        self.search.remote_gen += 1;
        self.search.inflight = true;
        self.search.dirty = false;
        self.search.force = false;
        self.show_number(n, hits, true, None, repos.len());
        let (client, tx, generation) = (self.client.clone(), self.tx.clone(), self.search.remote_gen);
        tokio::spawn(async move {
            let res = client.prs_by_number(&unknown, n).await;
            let _ = tx.send(Msg::Remote { generation, key: RemoteKey::Number { n, repos: unknown }, res });
        });
    }

    fn on_remote(&mut self, generation: u64, key: RemoteKey, res: Result<(Vec<PrSummary>, Option<RateLimit>)>) {
        self.search.inflight = false;
        let now = Instant::now();
        // Cache whatever came back, even when it's too late to show.
        let res = match res {
            Ok((prs, rate)) => {
                self.rate = rate.or(self.rate.take());
                match &key {
                    RemoteKey::Text(q) => {
                        if self.search.remote_cache.len() >= 30
                            && let Some(old) = self.search.remote_cache.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone())
                        {
                            self.search.remote_cache.remove(&old);
                        }
                        self.search.remote_cache.insert(q.clone(), (now, prs.clone()));
                    }
                    RemoteKey::Number { n, repos } => {
                        for r in repos {
                            let hit = prs.iter().find(|p| p.repository.name_with_owner.eq_ignore_ascii_case(r)).cloned();
                            self.search.number_cache.insert((r.to_lowercase(), *n), (now, hit));
                        }
                    }
                }
                Ok(prs)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let l = msg.to_lowercase();
                if l.contains("403") || l.contains("429") || l.contains("rate limit") {
                    self.search.paused_until = Some(now + Duration::from_secs(60));
                }
                Err(msg)
            }
        };
        if generation != self.search.remote_gen || !matches!(self.modal, Some(Modal::Search)) {
            return;
        }
        if let Ok(prs) = &res {
            self.queue_bodies(prs.iter().map(|p| (p.id.clone(), p.updated_at.clone())).collect());
        }
        match key {
            RemoteKey::Text(q) => match res {
                Ok(prs) => self.set_remote(RemoteKind::Text, q, prs, false, None, 0),
                Err(e) => self.set_remote(RemoteKind::Text, q, Vec::new(), false, Some(e), 0),
            },
            RemoteKey::Number { n, .. } => {
                // `#14` can still be in flight when the user has typed `#142` and pressed Enter.
                // That older response must not finish the pending open, or it reports "No PR #142".
                if self.search.query().number() != Some(n) {
                    if self.search.remote.as_ref().is_some_and(|r| r.kind == RemoteKind::Number(n)) {
                        self.search.remote = None;
                    }
                    return;
                }
                let repos = self.number_repos();
                let hits = self.number_hits(&repos, n);
                self.show_number(n, hits, false, res.err(), repos.len());
                if self.search.open_on_arrival == Some(n) {
                    self.check_open_on_arrival();
                }
            }
        }
    }

    /// After a number lookup lands: open #n if exactly one row has it, else highlight the first.
    fn check_open_on_arrival(&mut self) {
        let Some(n) = self.search.open_on_arrival else { return };
        if self.search.remote.as_ref().is_some_and(|r| r.loading) {
            return;
        }
        self.search.open_on_arrival = None;
        let items = self.selectable_items();
        let exact: Vec<usize> = items.iter().enumerate().filter(|(_, it)| self.item_number(it) == Some(n)).map(|(i, _)| i).collect();
        match exact.as_slice() {
            [only] => self.pick_search(*only),
            [first, ..] => self.search.sel = *first,
            [] => self.toast(format!("No PR #{n} in {} repositories", self.number_repos().len()), ToastKind::Info),
        }
    }

    /// Fetch descriptions not cached for their `updatedAt` yet, 100 per request.
    fn queue_bodies(&mut self, items: Vec<(String, String)>) {
        if !self.cfg.search.bodies || self.rate.as_ref().is_some_and(|r| r.remaining < 300) {
            return;
        }
        let want: Vec<String> = items
            .into_iter()
            .filter(|(id, upd)| !self.search.bodies_loading.contains(id) && self.search.bodies.get(id).is_none_or(|(u, _)| u != upd))
            .map(|(id, _)| id)
            .collect();
        for chunk in want.chunks(100) {
            let ids = chunk.to_vec();
            self.search.bodies_loading.extend(ids.iter().cloned());
            let (client, tx) = (self.client.clone(), self.tx.clone());
            tokio::spawn(async move {
                let res = client.pr_bodies(&ids).await;
                let _ = tx.send(Msg::Bodies { ids, res });
            });
        }
    }

    /// Descriptions loaded of the PRs in the list, while some are still loading.
    pub fn bodies_progress(&self) -> Option<(usize, usize)> {
        if self.search.bodies_loading.is_empty() {
            return None;
        }
        Some((self.prs.iter().filter(|p| self.search.bodies.contains_key(&p.id)).count(), self.prs.len()))
    }

    /// Why a palette command can't run on the selected PR (the Actions menu's reasons).
    pub fn command_block(&self, c: Command) -> Option<String> {
        if c.needs_pr() && self.selected_pr().is_none() {
            return Some("no PR selected".into());
        }
        let d = self.selected_detail()?;
        let entries = self.menu_entries(d, false);
        let reason = |f: fn(&MenuItem) -> bool| entries.iter().find(|e| f(&e.item)).and_then(|e| e.disabled.clone());
        match c {
            Command::Merge => {
                let merges: Vec<&MenuEntry> = entries.iter().filter(|e| matches!(e.item, MenuItem::Merge(_))).collect();
                match merges.first() {
                    None => Some("no merge methods allowed".into()),
                    Some(first) if merges.iter().all(|e| e.disabled.is_some()) => first.disabled.clone(),
                    _ => None,
                }
            }
            Command::Approve => reason(|i| *i == MenuItem::Approve),
            Command::RequestChanges => reason(|i| *i == MenuItem::RequestChanges),
            Command::CloseOrReopen => reason(|i| matches!(i, MenuItem::Close | MenuItem::Reopen)),
            _ => None,
        }
    }

    fn run_command(&mut self, c: Command) {
        let action = match c {
            Command::Comment => Action::Comment,
            Command::Merge => Action::MergeMenu,
            Command::Approve => Action::Approve,
            Command::RequestChanges => Action::RequestChanges,
            Command::CloseOrReopen => Action::CloseOrReopen,
            Command::Browser => Action::Browser,
            Command::CopyCheckout => Action::CopyCheckout,
            Command::CopyUrl => Action::CopyUrl,
            Command::Refresh => Action::Refresh,
            Command::Density => Action::CycleDensity,
            Command::StateOpen => Action::SetState(StateFilter::Open),
            Command::StateClosed => Action::SetState(StateFilter::Closed),
            Command::StateMerged => Action::SetState(StateFilter::Merged),
            Command::StateAll => Action::SetState(StateFilter::All),
            Command::OnlyRepo => Action::ToggleRepoOfSelected,
            Command::Images => Action::ToggleImages,
            Command::Help => Action::Help,
            Command::Quit => Action::Quit,
        };
        self.run(action);
    }
}

fn delete_word(s: &str) -> String {
    let t = s.trim_end();
    let cut = t.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    t[..cut].to_string()
}

fn marks(m: &mut nucleo_matcher::Matcher, q: &Query, d: &Doc, snippet_w: Option<usize>) -> Marks {
    use search::Field;
    Marks {
        title: search::highlight(m, q, Field::Title, d),
        repo: search::highlight(m, q, Field::Repo, d),
        number: search::highlight(m, q, Field::Number, d),
        author: search::highlight(m, q, Field::Author, d),
        snippet: snippet_w.and_then(|w| search::snippet(d, q, w)),
    }
}

/// The palette's rows, in section order: PRs (loaded, then GitHub), repositories, scopes, commands.
pub fn search_items(app: &App) -> Vec<Item> {
    let s = &app.search;
    let q = s.query();
    let mut items = Vec::new();
    if q.is_empty() && !q.number_mode {
        if app.state.recent_searches.is_empty() {
            items.push(Item::Note("Search PRs by title, description, #number, repo, author, branch or label — and repos, scopes, commands".into()));
        } else {
            items.push(Item::Header("Recent searches".into()));
            items.extend(app.state.recent_searches.iter().map(|r| Item::Recent(r.clone())));
        }
        return items;
    }
    let mut local = app.palette_results();
    // GitHub rows not already shown, re-ranked with the full text (number hits keep repo order).
    // A loaded PR that only matches in its description is kept when GitHub returned it and the
    // body cache has not (bodies off, rate limit, or a failed fetch).
    let remote = app.current_remote(&q);
    let mut rows: Vec<(usize, Match)> = Vec::new();
    if let Some(r) = remote {
        let mut added = false;
        {
            let mut m = s.matcher.borrow_mut();
            for (i, (p, d)) in r.hits.iter().zip(&r.docs).enumerate() {
                if let Some(idx) = app.prs.iter().position(|x| x.id == p.id) {
                    // Same PR, missed by local ranking (usually a body GitHub searched and we have not cached).
                    if !local.iter().any(|(j, _)| *j == idx)
                        && let Some(sc) = search::score(&mut m, &q, d, p, true)
                    {
                        local.push((idx, sc));
                        added = true;
                    }
                    continue;
                }
                if let Some(sc) = search::score(&mut m, &q, d, p, true) {
                    rows.push((i, sc));
                }
            }
        }
        if added {
            local.sort_by(|a, b| {
                let updated = |i: usize| crate::ui::parse_ts(&app.prs[i].updated_at).unwrap_or(0);
                search::order((&a.1, updated(a.0), a.0), (&b.1, updated(b.0), b.0))
            });
        }
        if r.kind == RemoteKind::Text {
            rows.sort_by(|a, b| search::order((&a.1, r.docs[a.0].updated, a.0), (&b.1, r.docs[b.0].updated, b.0)));
        }
    }
    let best = local.first().map_or(0, |(_, m)| m.score);
    items.push(Item::Header("Pull requests".into()));
    if q.number_mode && q.number().is_none() {
        items.push(Item::Note("type a PR number".into()));
    } else if local.is_empty() {
        items.push(Item::Note(format!("No loaded PRs match “{}”", s.text.trim())));
    }
    items.extend(local.iter().take(PALETTE_PRS).map(|&(idx, m)| Item::Pr { idx, m }));
    if local.len() > PALETTE_PRS {
        items.push(Item::More(local.len() - PALETTE_PRS));
    }
    if q.number_mode {
        if q.number().is_some() {
            if remote.is_some_and(|r| r.loading || r.error.is_some()) || !rows.is_empty() {
                items.push(Item::Header("From GitHub".into()));
            }
            if app.number_repos().is_empty() {
                items.push(Item::Note("No repositories to look the number up in — pick one with S".into()));
            }
        }
        items.extend(rows.into_iter().map(|(i, m)| Item::Remote { i, m }));
        return items;
    }
    if !rows.is_empty() {
        items.push(Item::Header("From GitHub".into()));
        items.extend(rows.into_iter().map(|(i, m)| Item::Remote { i, m }));
    }
    items.push(Item::SearchGitHub);

    // Other kinds only when they match about as well as the PRs do, three each at most.
    if s.text.trim().chars().count() < 2 || !q.has_free() {
        return items;
    }
    let floor = if local.is_empty() { 0 } else { best / 10 * 6 / 10 };
    let mut m = s.matcher.borrow_mut();
    let count_of = |nwo: &str| app.scope_repos.iter().find(|(r, _)| r.eq_ignore_ascii_case(nwo)).map(|(_, c)| *c);
    let mut repos: Vec<(u32, String)> =
        app.known_repos().into_iter().filter_map(|r| search::score_text(&mut m, &q, &r).filter(|sc| *sc >= floor).map(|sc| (sc, r))).collect();
    repos.sort_by(|a, b| b.0.cmp(&a.0));
    if !repos.is_empty() {
        items.push(Item::Header("Repositories".into()));
        items.extend(repos.into_iter().take(3).map(|(_, nwo)| Item::Repo { count: count_of(&nwo), nwo }));
    }
    let mut scopes: Vec<(u32, usize)> = scope_matches(&app.scopes, "", app.repo.is_some())
        .into_iter()
        .filter_map(|i| {
            let sc = &app.scopes[i];
            search::score_text(&mut m, &q, &format!("{} {}", sc.name, sc.qualifier)).filter(|x| *x >= floor).map(|x| (x, i))
        })
        .collect();
    scopes.sort_by(|a, b| b.0.cmp(&a.0));
    if !scopes.is_empty() {
        items.push(Item::Header("Scopes".into()));
        items.extend(scopes.into_iter().take(3).map(|(_, i)| Item::Scope(i)));
    }
    let cmds: Vec<Command> = search::commands(&q, &mut m).into_iter().filter(|(sc, _)| *sc as u32 >= floor).take(3).map(|(_, c)| c).collect();
    drop(m);
    if !cmds.is_empty() {
        items.push(Item::Header("Commands".into()));
        items.extend(cmds.into_iter().map(|cmd| Item::Command { disabled: app.command_block(cmd), cmd }));
    }
    items
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

#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::search::tests::pr;

    fn test_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = Config::default();
        cfg.search.bodies = false;
        let detection = crate::images::Detection { picker: None, note: None, off_reason: String::new() };
        let mut app = App::new(Client::new("test".into()).unwrap(), tx, cfg, detection);
        app.state.recent_searches.clear();
        app.state.recent_repos.clear();
        app.repos.clear();
        app
    }

    #[test]
    fn stale_remote_generations_are_dropped() {
        let mut app = test_app();
        app.modal = Some(Modal::Search);
        app.search.text = "shoulder".into();
        let q = "shoulder in:title,body is:pr sort:updated-desc".to_string();
        app.search.remote_gen = 2;
        app.set_remote(RemoteKind::Text, q.clone(), Vec::new(), true, None, 0);
        let hit = || Ok((vec![pr(7, "a/b", "R5 shoulder girdle", "2026-01-01T00:00:00Z")], None));
        app.handle(Msg::Remote { generation: 1, key: RemoteKey::Text(q.clone()), res: hit() });
        let r = app.search.remote.as_ref().unwrap();
        assert!(r.loading && r.hits.is_empty());
        // Still cached, so retyping the same words costs nothing.
        assert!(app.search.remote_cache.contains_key(&q));
        app.handle(Msg::Remote { generation: 2, key: RemoteKey::Text(q.clone()), res: hit() });
        let r = app.search.remote.as_ref().unwrap();
        assert!(!r.loading && r.hits.len() == 1);
        // A response after the palette closed is dropped too.
        app.close_search();
        let generation = app.search.remote_gen;
        app.handle(Msg::Remote { generation, key: RemoteKey::Text(q), res: hit() });
        assert!(app.search.remote.is_none());
    }

    #[test]
    fn number_lookup_fills_the_cache_per_repo() {
        let mut app = test_app();
        app.modal = Some(Modal::Search);
        app.search.text = "#14".into();
        app.prs = vec![pr(140, "acme/one", "x", "2026-01-02T00:00:00Z"), pr(3, "acme/two", "y", "2026-01-01T00:00:00Z")];
        app.prs_rev += 1;
        app.scope_repos = count_repos(&app.prs);
        assert_eq!(app.number_repos(), vec!["acme/one", "acme/two"]);
        let repos = vec!["acme/one".to_string(), "acme/two".to_string()];
        app.search.remote_gen = 1;
        app.handle(Msg::Remote { generation: 1, key: RemoteKey::Number { n: 14, repos }, res: Ok((vec![pr(14, "acme/two", "z", "2026-01-03T00:00:00Z")], None)) });
        assert!(app.search.number_cache.get(&("acme/one".into(), 14)).is_some_and(|(_, p)| p.is_none()));
        let items = search_items(&app);
        let numbers: Vec<u64> = items.iter().filter_map(|it| app.item_number(it)).collect();
        // Local prefix match first, then the GitHub exact hit.
        assert_eq!(numbers, vec![140, 14]);
        assert!(items.iter().all(|it| !matches!(it, Item::Repo { .. } | Item::Command { .. })));
    }

    #[test]
    fn cached_number_highlights_the_exact_hit() {
        let mut app = test_app();
        app.modal = Some(Modal::Search);
        app.prs = vec![pr(140, "acme/one", "x", "2026-01-02T00:00:00Z")];
        app.prs_rev += 1;
        app.scope_repos = count_repos(&app.prs);
        app.search.number_cache.insert(("acme/one".into(), 14), (Instant::now(), Some(pr(14, "acme/one", "exact", "2026-01-03T00:00:00Z"))));
        app.search.text = "#14".into();
        app.search.dirty = true;
        app.search.force = true;
        app.drive_search();
        let items = app.selectable_items();
        // Enter opens the highlight. The cached exact hit has to be that row, or #140 opens.
        assert_eq!(app.item_number(&items[app.search.sel]), Some(14));
        assert!(!app.number_lookup_pending(14));
    }

    #[test]
    fn stale_number_lookup_does_not_finish_a_newer_enter() {
        let mut app = test_app();
        app.modal = Some(Modal::Search);
        app.prs = vec![pr(140, "acme/one", "x", "2026-01-02T00:00:00Z")];
        app.prs_rev += 1;
        app.scope_repos = count_repos(&app.prs);
        app.search.text = "#142".into();
        app.search.open_on_arrival = Some(142);
        app.search.dirty = true;
        app.search.remote_gen = 1;
        app.set_remote(RemoteKind::Number(14), "#14".into(), Vec::new(), true, None, 1);
        app.handle(Msg::Remote {
            generation: 1,
            key: RemoteKey::Number { n: 14, repos: vec!["acme/one".into()] },
            res: Ok((Vec::new(), None)),
        });
        assert_eq!(app.search.open_on_arrival, Some(142));
        assert!(app.toast.as_ref().is_none_or(|(m, _, _)| !m.contains("142")));
        assert!(app.search.remote.as_ref().is_none_or(|r| r.kind != RemoteKind::Number(14)));
    }

    #[test]
    fn remote_rows_keep_a_moved_selection() {
        let mut app = test_app();
        app.modal = Some(Modal::Search);
        app.prs = vec![pr(1, "acme/form-atlas", "merge queue", "2026-01-01T00:00:00Z")];
        app.prs_rev += 1;
        app.scope_repos = count_repos(&app.prs);
        app.search.text = "form".into();
        let items = app.selectable_items();
        let repo_at = items.iter().position(|it| matches!(it, Item::Repo { .. })).expect("repo row");
        app.search.sel = repo_at;
        app.search.moved = true;
        let nwo = match &items[repo_at] {
            Item::Repo { nwo, .. } => nwo.clone(),
            _ => unreachable!(),
        };
        app.search.remote_gen = 1;
        let q = app.wanted_remote(&app.search.query(), true).unwrap();
        app.handle(Msg::Remote {
            generation: 1,
            key: RemoteKey::Text(q),
            res: Ok((vec![pr(9, "acme/other", "form from github", "2026-01-02T00:00:00Z")], None)),
        });
        let items = app.selectable_items();
        match &items[app.search.sel] {
            Item::Repo { nwo: n, .. } => assert_eq!(n, &nwo),
            other => panic!("selection jumped to {other:?}"),
        }
    }

    #[test]
    fn github_hit_keeps_a_loaded_pr_without_a_cached_body() {
        let mut app = test_app();
        let loaded = pr(1, "a/b", "unrelated title", "2026-01-01T00:00:00Z");
        app.prs = vec![loaded.clone()];
        app.prs_rev += 1;
        app.search.text = "shoulder".into();
        let q = app.wanted_remote(&app.search.query(), true).unwrap();
        app.set_remote(RemoteKind::Text, q, vec![loaded], false, None, 0);
        assert!(search_items(&app).iter().any(|it| matches!(it, Item::Pr { idx: 0, .. })));
    }

    #[test]
    fn bodies_and_detail_keep_the_selected_pr() {
        let mut app = test_app();
        let older = pr(1, "a/b", "unrelated older", "2026-01-01T00:00:00Z");
        let newer = pr(2, "a/b", "unrelated newer", "2026-01-05T00:00:00Z");
        app.prs = vec![older.clone(), newer.clone()];
        app.prs_rev += 1;
        app.search.bodies.insert(older.id.clone(), (older.updated_at.clone(), "mesh details".into()));
        app.search.pinned = Some("mesh".into());
        app.sel = 0;
        assert_eq!(app.selected_pr().unwrap().number, 1);
        app.handle(Msg::Bodies {
            ids: vec![newer.id.clone()],
            res: Ok((vec![PrBody { id: newer.id.clone(), updated_at: newer.updated_at.clone(), body: "mesh details".into() }], None)),
        });
        assert_eq!(app.visible(), vec![1, 0]);
        assert_eq!(app.selected_pr().unwrap().number, 1);

        app.search.pinned = Some("mesh".into());
        app.prs[0].title = "mesh alpha".into();
        app.prs[1].title = "zzz".into();
        app.search.bodies.clear();
        app.prs_rev += 1;
        app.sel = 0;
        assert_eq!(app.selected_pr().unwrap().number, 1);
        let detail = serde_json::from_value::<crate::github::PrDetail>(serde_json::json!({
            "id": newer.id,
            "number": 2,
            "title": "mesh beta",
            "body": "",
            "bodyHTML": "",
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": null,
            "reviewDecision": null,
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": newer.updated_at,
            "author": null,
            "headRefName": "feat",
            "baseRefName": "main",
            "additions": 0,
            "deletions": 0,
            "changedFiles": 0,
            "headRefOid": "abc",
            "isCrossRepository": false,
            "canBeRebased": true,
            "viewerCanClose": true,
            "viewerCanReopen": true,
            "viewerCanMergeAsAdmin": false,
            "labels": {"nodes": []},
            "assignees": {"nodes": []},
            "reviewRequests": {"nodes": []},
            "participants": {"nodes": []},
            "repository": {
                "nameWithOwner": "a/b",
                "mergeCommitAllowed": true,
                "squashMergeAllowed": true,
                "rebaseMergeAllowed": true,
                "viewerPermission": null,
                "mentionableUsers": {"nodes": []}
            },
            "statusCheckRollup": null,
            "commits": {"totalCount": 0, "nodes": []},
            "reviews": {"nodes": []},
            "comments": {"nodes": []},
            "reviewThreads": {"nodes": []}
        }))
        .unwrap();
        app.handle(Msg::Detail { id: newer.id.clone(), res: Ok((detail, None)) });
        assert_eq!(app.prs[1].title, "mesh beta");
        assert_eq!(app.selected_pr().unwrap().number, 1);
    }

    #[test]
    fn visible_follows_prs_rev() {
        let mut app = test_app();
        app.prs = vec![pr(1, "a/b", "alpha", "2026-01-01T00:00:00Z"), pr(2, "a/b", "beta", "2026-01-02T00:00:00Z")];
        app.prs_rev += 1;
        assert_eq!(app.visible(), vec![0, 1]);
        app.search.pinned = Some("beta".into());
        assert_eq!(app.visible(), vec![1]);
        app.prs.push(pr(3, "a/b", "beta two", "2026-01-03T00:00:00Z"));
        app.prs_rev += 1;
        assert_eq!(app.visible(), vec![2, 1]);
        app.search.pinned = None;
        assert_eq!(app.visible(), vec![0, 1, 2]);
    }

    #[test]
    fn palette_sections() {
        let mut app = test_app();
        app.prs = vec![pr(1, "acme/form-atlas", "merge queue fixes", "2026-01-01T00:00:00Z")];
        app.prs_rev += 1;
        app.scope_repos = count_repos(&app.prs);
        app.search.text = "merge".into();
        let items = search_items(&app);
        assert!(matches!(items[0], Item::Header(ref h) if h == "Pull requests"));
        assert!(matches!(items[1], Item::Pr { idx: 0, .. }));
        assert!(items.iter().any(|it| matches!(it, Item::SearchGitHub)));
        assert!(items.iter().any(|it| matches!(it, Item::Command { cmd: Command::Merge, .. })));
        // Without a selected PR, PR commands say why.
        app.search.pinned = Some("zzz".into());
        let items = search_items(&app);
        assert!(items.iter().any(|it| matches!(it, Item::Command { cmd: Command::Merge, disabled: Some(_) })));
        // Empty input lists recent searches.
        app.search.text.clear();
        app.state.recent_searches = vec!["form atlas".into()];
        assert!(matches!(search_items(&app)[1], Item::Recent(_)));
    }

    /// Per-keystroke cost: rank + palette rows + a full 160×50 frame, over 300 PRs with
    /// descriptions and 1000 repos.
    #[test]
    fn keystroke_is_fast() {
        let mut app = test_app();
        let (prs, _) = crate::search::tests::fixture();
        for p in &prs {
            let body: String = (0..300).map(|k| ["skin", "atlas", "mesh", "rig", "pose", "chart"][(p.number as usize + k) % 6]).collect::<Vec<_>>().join(" ");
            app.search.bodies.insert(p.id.clone(), (p.updated_at.clone(), crate::search::strip_body(&body, crate::search::BODY_CAP)));
        }
        app.prs = prs;
        app.prs_rev += 1;
        app.scope_repos = count_repos(&app.prs);
        app.repos = (0..1000).map(|i| RepoInfo { name_with_owner: format!("org{}/repo-{i}", i % 9), is_private: false, pushed_at: None }).collect();
        app.modal = Some(Modal::Search);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 50)).unwrap();
        let mut worst = Duration::ZERO;
        for text in ["f", "fo", "for", "form", "form ", "form a", "form at", "form atlas", "form atlas s", "form atlas skin", "#1", "#14", "#142"] {
            app.search.text = text.into();
            let t = Instant::now();
            term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let el = t.elapsed();
            worst = worst.max(el);
            eprintln!("keystroke {text:<16} {el:?}");
        }
        assert!(worst < Duration::from_millis(if cfg!(debug_assertions) { 100 } else { 16 }), "{worst:?}");
        app.modal = None;
        let t = Instant::now();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        eprintln!("frame without the palette {:?}", t.elapsed());
        let t = Instant::now();
        let items = search_items(&app);
        eprintln!("search_items {:?} ({} rows)", t.elapsed(), items.len());
    }

    #[test]
    fn deletes_words() {
        assert_eq!(delete_word("form atlas skin"), "form atlas ");
        assert_eq!(delete_word("form atlas "), "form ");
        assert_eq!(delete_word("form"), "");
    }
}
