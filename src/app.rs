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
use crate::changes::{self, Change, NewMarks, Snapshot};
use crate::config::Config;
use crate::editor::{Editor, VisualRow};
use crate::images::{ImageKey, ImageMsg, ImagePlacement, ImageStore};
use crate::github::{
    Annotation, Client, FileDiff, Job, MergeMethod, PrBody, PrDetail, PrSummary, RateLimit, RepoInfo, RepoPage, ReviewEvent, RunsQuery, SearchPage,
    Viewer, WorkflowRunInfo,
};
use crate::search::{self, Command, Doc, Match, Query, TokenBucket};
use crate::stacks::{self, Link, Member, RowView, Stack};
use crate::state::{ReposCache, Seen, State};

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
/// How long the refresh digest stays on the status line.
pub const DIGEST_SECS: u64 = 10;
/// Changes to a PR this soon after your own mutation on it are yours (search lags a little).
const OWN_SECS: u64 = 120;
/// PRs named in the refresh digest before "+n more".
const DIGEST_ITEMS: usize = 3;
/// Skimming with J/K (or the list keys in the detail view) reads a PR after it stayed open this long.
const DWELL: Duration = Duration::from_secs(1);
/// At most one notification per PR this often.
const NOTIFY_EVERY: Duration = Duration::from_secs(300);

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
    /// The needs-rebase compare query for the stack keyed `key`.
    Links { key: String, res: Result<(Vec<Link>, Option<RateLimit>)> },
    /// A stack's cumulative files (`Whole stack`), by `stack_files` key.
    StackFiles { key: String, res: Result<Vec<FileDiff>> },
    /// A stack member that wasn't loaded, fetched to open it (`id` is the one asked for).
    Splice { id: String, res: Result<(Vec<PrSummary>, Option<RateLimit>)> },
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
    Stack,
}

impl Tab {
    pub const ALL: [Tab; 8] = [Tab::Description, Tab::Conversation, Tab::Changes, Tab::Commits, Tab::Checks, Tab::Actions, Tab::Reviews, Tab::Stack];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Description => "Description",
            Tab::Conversation => "Conversation",
            Tab::Changes => "Changes",
            Tab::Commits => "Commits",
            Tab::Checks => "Checks",
            Tab::Actions => "Actions",
            Tab::Reviews => "Reviews",
            Tab::Stack => "Stack",
        }
    }

    /// For a tab bar too narrow for every full name.
    pub fn short_label(self) -> &'static str {
        match self {
            Tab::Description => "Desc",
            Tab::Conversation => "Conv",
            Tab::Changes => "Diff",
            Tab::Commits => "Log",
            Tab::Checks => "Checks",
            Tab::Actions => "Runs",
            Tab::Reviews => "Revs",
            Tab::Stack => "Stack",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// Timeline tabs, with `NEW` markers and `n`/`p` between them.
    pub fn has_new(self) -> bool {
        matches!(self, Tab::Conversation | Tab::Reviews | Tab::Commits)
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
    /// Show only unread PRs (`u`, the header's unread count).
    ToggleUnreadOnly,
    /// Mark every listed PR read (`U`).
    MarkAllRead,
    /// Sort unread PRs first (palette).
    ToggleUnreadFirst,
    /// Next / previous `NEW` entry in Conversation, Reviews or Commits (`n` / `p`).
    NextNew,
    PrevNew,
    /// A tab's `+n` badge: show the tab at its first new entry.
    OpenNew(Tab),
    /// Select (in the detail view: open) a PR by id; the refresh digest's items.
    JumpTo(String),
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
    /// A `#…` link handled inside the app: `fold:<group>`, `line:<line>`, `job:<id>`, `image:<key>`,
    /// `pr:<id>` (open a PR), `stackfile:<i>` (show a Stack tab file's patch).
    Internal(String),
    // Stacks (docs/stacks.md)
    /// `<` / `>`: the PR below (toward the trunk) / above in the stack.
    StackPrev,
    StackNext,
    /// Stack tab: move the cursor (members, or files once one is opened), open, back.
    StackMove(isize),
    StackSelect(usize),
    StackOpen,
    StackBack,
    StackOpenBrowser,
    StackUpdateBranch,
    StackLink,
    StackMergeUpTo,
    /// The bottom PR's page, which holds GitHub's stack map.
    StackOnGitHub,
    ToggleGroupStacks,
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
    StackUpdate,
    StackLink,
    StackMerge,
    /// A method for "merge the stack up to here" (the menu `M` opens).
    MergeStack(MergeMethod),
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
    /// Bring the branch up to date with its base (rebase, else merge), from this head commit.
    UpdateBranch { oid: String },
    /// Link these PRs (bottom first) into a GitHub stack.
    LinkStack { nwo: String, numbers: Vec<u64> },
    /// merge-async PR `up_to` (and its open downstack), polled to the end.
    MergeStack { nwo: String, up_to: u64, method: MergeMethod, sha: String, label: String },
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

/// What the last refresh changed, for the status line.
pub struct Digest {
    /// PRs that changed.
    pub count: usize,
    /// (PR id, `#n what`), most important first.
    pub items: Vec<(String, String)>,
    pub more: usize,
    pub at: Instant,
    /// The session's first load: changes since the stored baselines, not since the last refresh.
    pub since_last: bool,
}

/// The PR open in the detail view, and what you'd seen of it before (frozen at open, so the
/// `NEW` markers stay until you leave it).
pub struct Visit {
    pub id: String,
    /// Bumped per visit; keys the rendered tabs (their markers depend on it).
    pub serial: u64,
    pub started: Instant,
    pub since: Option<Seen>,
    pub marks: NewMarks,
    /// Checks whose outcome changed, and workflow runs `(id, attempt)` that finished, while it was open.
    pub checks: HashSet<String>,
    pub runs: HashSet<(u64, u64)>,
    /// Badge totals already shown, per tab (`SetTab`, `n`/`p`).
    pub ack: HashMap<Tab, usize>,
    /// The detail last taken as read (its fetch time).
    read_at: Option<Instant>,
}

/// Click targets recorded during the last render.
#[derive(Default)]
pub struct Hits {
    pub buttons: Vec<(Rect, Action)>,
    pub table: Option<(Rect, usize)>,
    pub sidebar: Option<(Rect, usize)>,
    pub detail: Option<Rect>,
    pub detail_links: Vec<Vec<(u16, u16, String)>>,
    /// Rows of the shown tab's `NEW` entries (their header rows), for `n`/`p`.
    pub new_rows: Vec<usize>,
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
    /// Stack tab's left column: rows area and scroll, and each row's entry (`stack_entries`).
    pub stack_list: Option<(Rect, usize)>,
    pub stack_list_rows: Vec<Option<usize>>,
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
    /// When the list on screen started loading; only details fetched since override its stacks.
    list_at: Option<Instant>,
    last_click: Option<(Instant, usize)>,
    pub hits: Hits,
    pub quit: bool,
    /// Unread PRs (in `prs`): what changed since you last opened each, empty when it's unread
    /// without a diff (GitHub says unread, or it newly appeared).
    pub unread: HashMap<String, Vec<Change>>,
    pub unread_only: bool,
    /// Read while the unread filter is on; they stay listed until it's toggled.
    unread_keep: HashSet<String>,
    pub digest: Option<Digest>,
    /// PRs you just changed yourself, and when; their next changes aren't news.
    own_pending: HashMap<String, Instant>,
    /// The list being replaced by the load in progress: its query, PRs, and whether it was complete.
    prev_list: Option<(String, Vec<PrSummary>, bool)>,
    /// The query of the load in flight, and of the list on screen once it fully landed.
    loading_query: String,
    list_query: String,
    list_complete: bool,
    pub visit: Option<Visit>,
    visit_serial: u64,
    /// A PR reached by skimming (J/K), and when; it's read after `DWELL`.
    dwell: Option<(String, Instant)>,
    /// Bring the shown tab's first `NEW` entry into view on the next frame (a badge click).
    pub jump_new: bool,
    /// The last `n`/`p` target (index into `hits.new_rows`) and the scroll it set.
    new_cursor: Option<(usize, usize)>,
    /// When each PR last notified, and how notifications go out (worked out on first use).
    notified: HashMap<String, Instant>,
    emission: Option<changes::Emission>,
    /// The last notification's text (status line and tests).
    pub last_notification: Option<String>,
    // Stacks (docs/stacks.md)
    /// Native stacks and implicit chains among the loaded PRs, and PR id → (stack, member).
    pub stacks: Vec<Stack>,
    pub membership: HashMap<String, (usize, usize)>,
    /// Default branch per repo (lowercased `owner/name`), learnt from details.
    default_branches: HashMap<String, String>,
    /// Needs-rebase compare results per stack key.
    pub links: HashMap<String, (Vec<Link>, Instant)>,
    links_loading: HashSet<String>,
    /// `Whole stack` files per key (trunk…top head).
    pub stack_files: HashMap<String, (Vec<FileDiff>, Instant)>,
    stack_files_loading: HashSet<String>,
    pub stack_files_errors: HashMap<String, String>,
    /// Stack tab cursor (an index into `stack_entries`; `None` follows the open PR), whether a
    /// file's patch is open, the file cursor, and the left column's scroll.
    pub stack_sel: Option<usize>,
    pub stack_open: bool,
    pub stack_file_sel: usize,
    pub stack_list_scroll: usize,
    pub stack_list_follow: bool,
    /// Members being fetched to be opened.
    splicing: HashSet<String>,
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
            list_at: None,
            last_click: None,
            hits: Hits::default(),
            quit: false,
            unread: HashMap::new(),
            unread_only: false,
            unread_keep: HashSet::new(),
            digest: None,
            own_pending: HashMap::new(),
            prev_list: None,
            loading_query: String::new(),
            list_query: String::new(),
            list_complete: false,
            visit: None,
            visit_serial: 0,
            dwell: None,
            jump_new: false,
            new_cursor: None,
            notified: HashMap::new(),
            emission: None,
            last_notification: None,
            stacks: Vec::new(),
            membership: HashMap::new(),
            default_branches: HashMap::new(),
            links: HashMap::new(),
            links_loading: HashSet::new(),
            stack_files: HashMap::new(),
            stack_files_loading: HashSet::new(),
            stack_files_errors: HashMap::new(),
            stack_sel: None,
            stack_open: false,
            stack_file_sel: 0,
            stack_list_scroll: 0,
            stack_list_follow: true,
            splicing: HashSet::new(),
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

    /// The rows on screen are no longer the last complete load of `list_query`.
    fn clear_list(&mut self) {
        self.prs.clear();
        self.prs_rev += 1;
        self.total = 0;
        self.list_complete = false;
    }

    fn load_list(&mut self) {
        // Counts describe the unfiltered list they came from; another scope or state voids them.
        if self.scope_repos_key != (self.scope, self.state_filter) {
            self.scope_repos.clear();
        }
        self.list_gen += 1;
        self.loading_list = true;
        self.last_refresh = Instant::now();
        self.loading_query = self.search_query();
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
            // `rebuild_stacks` ignores a stacked detail older than the list, so fetch it again.
            let stale = self
                .details
                .get(&id)
                .is_none_or(|(d, at)| at.elapsed() > Duration::from_secs(30) || (d.stack.is_some() && self.list_at.is_some_and(|l| *at < l)));
            if stale {
                self.load_detail(&id);
            }
        }
        self.ensure_files(false);
        self.ensure_runs(false);
        self.ensure_stack_data();
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
        matches!(self.tab, Tab::Changes | Tab::Actions | Tab::Stack)
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
        // A failed `Whole stack` compare is cached until the next refresh retries it.
        self.stack_files_errors.clear();
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

    /// Indices into `prs` the list shows: `visible_base`, narrowed to unread PRs when that filter is on.
    pub fn visible(&self) -> Vec<usize> {
        self.visible_rows().into_iter().map(|r| r.idx).collect()
    }

    /// The list's rows: `visible_base`, narrowed to unread PRs when that filter is on, unread
    /// first when asked, then each stack's PRs together (bottom first) with their tree glyphs.
    pub fn visible_rows(&self) -> Vec<RowView> {
        let mut base = self.visible_base();
        let unread = |i: usize| self.unread.contains_key(&self.prs[i].id) || self.unread_keep.contains(&self.prs[i].id);
        if self.unread_only {
            base.retain(|&i| unread(i));
        }
        if self.state.unread_first {
            // Stable: each group keeps its order.
            base.sort_by_key(|&i| !unread(i));
        }
        if self.state.group_stacks && !self.stacks.is_empty() {
            return stacks::grouped(&base, &self.prs, &self.stacks, &self.membership);
        }
        base.into_iter().map(|idx| RowView { idx, glyph: None }).collect()
    }

    /// Unread PRs among `visible_base`.
    pub fn unread_count(&self) -> usize {
        self.visible_base().iter().filter(|&&i| self.unread.contains_key(&self.prs[i].id)).count()
    }

    /// Indices into `prs` before the unread filter: all of them, or the pinned query's matches, best first.
    pub fn visible_base(&self) -> Vec<usize> {
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
            self.reset_stack_view();
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
        self.skim();
    }

    /// Moving through PRs in the detail view (J/K, the list keys, the wheel) reads one only
    /// after it stayed open for `DWELL`.
    fn skim(&mut self) {
        if self.screen == Screen::Detail {
            self.dwell = self.selected_pr().map(|p| (p.id.clone(), Instant::now()));
        }
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
        if self.digest.as_ref().is_some_and(|d| d.at.elapsed() > Duration::from_secs(DIGEST_SECS)) {
            self.digest = None;
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
        // A skimmed-to PR is read once the dwell is over.
        if self.dwell.as_ref().is_some_and(|(_, at)| at.elapsed() >= DWELL) {
            self.read_open();
        }
    }

    pub fn handle(&mut self, msg: Msg) {
        self.handle_msg(msg);
        // Whatever put a PR in the detail view (Enter, J/K, a click, the palette, a reload),
        // it's being read.
        self.read_open();
    }

    fn handle_msg(&mut self, msg: Msg) {
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
                        let page_ids: HashSet<String> = page.prs.iter().map(|p| p.id.clone()).collect();
                        // The list this load replaces, for the digest. A load that never finished
                        // (superseded or failed) leaves the last complete one in place.
                        if first && self.prev_list.is_none() {
                            let old = self.prs.iter().filter(|p| !self.search.spliced.contains(&p.id)).cloned().collect();
                            self.prev_list = Some((self.list_query.clone(), old, self.list_complete));
                        }
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
                            self.list_at = Some(self.last_refresh);
                        } else {
                            let seen: HashSet<String> = self.prs.iter().map(|p| p.id.clone()).collect();
                            self.prs.extend(page.prs.into_iter().filter(|p| !seen.contains(&p.id)));
                        }
                        self.absorb_details(&page_ids);
                        self.prs_rev += 1;
                        self.rebuild_stacks();
                        self.total = page.total;
                        self.loading_list = has_next && self.prs.len() < MAX_PRS;
                        self.observe(&page_ids);
                        if !self.loading_list {
                            self.finish_list(prev.as_deref());
                        }
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
                        let me = self.viewer.as_ref().map(|v| v.login.clone());
                        // While the PR is open: checks that changed (the Checks badge), and what
                        // others did since the last detail of this visit (digest, notification).
                        let mut ch = Vec::new();
                        if let Some(v) = self.visit.as_mut().filter(|v| v.id == id)
                            && let Some((old, at)) = self.details.get(&id)
                        {
                            // A cache from before this visit is the baseline, not news from it.
                            let during = *at >= v.started;
                            if during {
                                v.checks.extend(changes::checks_changed(old, &d));
                            }
                            if during && !self.own_pending.get(&id).is_some_and(|t| t.elapsed() < Duration::from_secs(OWN_SECS)) {
                                ch = changes::diff_detail(old, &d, me.as_deref(), &self.cfg.bots);
                            }
                        }
                        // Keep the list row in sync with the fresher detail. A new base, head or
                        // state, or a newly learnt trunk, can redraw implicit chains.
                        let mut chained = false;
                        let mut reranked = false;
                        if let Some(p) = self.prs.iter_mut().find(|p| p.id == id) {
                            let before = (p.base_ref_name.clone(), p.head_ref_name.clone(), p.state.clone());
                            p.absorb(&d, me.as_deref());
                            chained = before != (p.base_ref_name.clone(), p.head_ref_name.clone(), p.state.clone());
                            self.prs_rev += 1;
                            reranked = true;
                        }
                        if self.own(&id) {
                            self.mark_read(&id);
                        }
                        self.rebuild_unread();
                        if let Some(b) = &d.repository.default_branch_ref {
                            let old = self.default_branches.insert(d.repository.name_with_owner.to_lowercase(), b.name.clone());
                            chained |= old.as_ref() != Some(&b.name);
                        }
                        let stacked = chained || d.stack.is_some() || self.membership.contains_key(&id);
                        self.details.insert(id.clone(), (d, Instant::now()));
                        if stacked {
                            self.rebuild_stacks_keep();
                        }
                        self.refresh_marks();
                        if !ch.is_empty() {
                            let ch = vec![(id.clone(), ch)];
                            let prs = self.prs.clone();
                            self.set_digest(&ch, &prs, false);
                            self.notify(&ch, &prs);
                        }
                        // A pinned query re-ranks when the title or state changes. `sel` is an
                        // ordinal, so put it back on the PR that was open.
                        if reranked {
                            self.retain_list_sel(keep);
                        }
                        self.ensure_files(false);
                        self.ensure_runs(false);
                        // The needs-rebase compare follows the detail's cadence.
                        if self.selected_pr().is_some_and(|p| p.id == id) {
                            self.ensure_links(true);
                        }
                        self.ensure_stack_data();
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
                        self.runs_finished(&id, &sha, &runs);
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
                        self.own_pending.insert(id.clone(), Instant::now());
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
            Msg::Links { key, res } => {
                self.links_loading.remove(&key);
                match res {
                    Ok((links, rate)) => {
                        self.rate = rate.or(self.rate.take());
                        self.links.insert(key, (links, Instant::now()));
                    }
                    // A signal, not a feature: keep the last answer and try again next refresh.
                    Err(_) => {
                        if let Some(e) = self.links.get_mut(&key) {
                            e.1 = Instant::now();
                        } else {
                            self.links.insert(key, (Vec::new(), Instant::now()));
                        }
                    }
                }
            }
            Msg::StackFiles { key, res } => {
                self.stack_files_loading.remove(&key);
                match res {
                    Ok(files) => {
                        self.stack_files_errors.remove(&key);
                        self.stack_files.insert(key, (files, Instant::now()));
                    }
                    Err(e) => {
                        self.stack_files_errors.insert(key, format!("{e:#}"));
                    }
                }
            }
            Msg::Splice { id, res } => {
                self.splicing.remove(&id);
                match res {
                    Ok((prs, rate)) => {
                        self.rate = rate.or(self.rate.take());
                        let Some(pr) = prs.into_iter().find(|p| p.id == id) else {
                            return self.toast("That PR couldn't be loaded", ToastKind::Error);
                        };
                        if !self.prs.iter().any(|p| p.id == id) {
                            let prev = self.selected_pr().map(|p| p.id.clone());
                            self.queue_bodies(vec![(id.clone(), pr.updated_at.clone())]);
                            self.prs.insert(0, pr);
                            self.prs_rev += 1;
                            self.search.spliced.insert(id.clone());
                            self.rebuild_stacks();
                            self.keep_selected(prev);
                        }
                        self.run(Action::JumpTo(id));
                    }
                    Err(e) => self.toast(format!("Couldn't load that PR: {e:#}"), ToastKind::Error),
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
        let stack_tab = self.screen == Screen::Detail && self.tab == Tab::Stack;
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
            // Stack tab: members (or an opened member's files) like the Actions tree.
            KeyCode::Char('n') if stack_tab => Some(Action::StackMove(1)),
            KeyCode::Char('p') if stack_tab => Some(Action::StackMove(-1)),
            KeyCode::Down if in_detail_pane && stack_tab => Some(Action::StackMove(1)),
            KeyCode::Up if in_detail_pane && stack_tab => Some(Action::StackMove(-1)),
            KeyCode::Enter if in_detail_pane && stack_tab => Some(Action::StackOpen),
            KeyCode::Esc if stack_tab && self.stack_open => Some(Action::StackBack),
            KeyCode::Char('O') if stack_tab => Some(Action::StackOpenBrowser),
            KeyCode::Char('q') => Some(if self.screen == Screen::Detail { Action::Back } else { Action::Quit }),
            KeyCode::Esc => Some(match self.screen {
                Screen::Detail => Action::Back,
                // The unread filter goes before the search filter.
                Screen::List if self.unread_only => Action::ToggleUnreadOnly,
                Screen::List => Action::ClearSearch,
            }),
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Char('r') | KeyCode::F(5) => Some(Action::Refresh),
            KeyCode::Char('s') => Some(Action::OpenScopes),
            KeyCode::Char('S') => Some(Action::OpenRepos),
            KeyCode::Char('D') => Some(Action::CycleDensity),
            KeyCode::Char('I') => Some(Action::ToggleImages),
            KeyCode::Char('e') => Some(Action::ToggleRepoOfSelected),
            KeyCode::Char('f') => Some(Action::CycleState),
            KeyCode::Char('u') if !ctrl => Some(Action::ToggleUnreadOnly),
            KeyCode::Char('U') => Some(Action::MarkAllRead),
            KeyCode::Char('/') => Some(Action::OpenSearch),
            KeyCode::Char('o') => Some(Action::Browser),
            KeyCode::Char('c') => Some(Action::Comment),
            KeyCode::Char('m') => Some(Action::MergeMenu),
            KeyCode::Char('.') | KeyCode::Char(' ') => Some(Action::Menu),
            KeyCode::Char('a') => Some(Action::Approve),
            KeyCode::Char('R') => Some(Action::RequestChanges),
            KeyCode::Char('x') => Some(Action::CloseOrReopen),
            KeyCode::Char('y') => Some(Action::CopyCheckout),
            KeyCode::Char('<') => Some(Action::StackPrev),
            KeyCode::Char('>') => Some(Action::StackNext),
            KeyCode::Char('b') => Some(Action::StackUpdateBranch),
            KeyCode::Char('L') => Some(Action::StackLink),
            KeyCode::Char('M') => Some(Action::StackMergeUpTo),
            KeyCode::Char(c @ '1'..='8') if self.screen == Screen::Detail => Some(Action::SetTab(Tab::ALL[c as usize - '1' as usize])),
            KeyCode::Char(']') if self.screen == Screen::Detail => Some(Action::NextTab),
            KeyCode::Char('[') if self.screen == Screen::Detail => Some(Action::PrevTab),
            KeyCode::Char('n') if self.screen == Screen::Detail && self.tab == Tab::Changes => Some(Action::NextFile),
            KeyCode::Char('p') if self.screen == Screen::Detail && self.tab == Tab::Changes => Some(Action::PrevFile),
            KeyCode::Char('n') if self.screen == Screen::Detail && self.tab.has_new() => Some(Action::NextNew),
            KeyCode::Char('p') if self.screen == Screen::Detail && self.tab.has_new() => Some(Action::PrevNew),
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
        } else if let Some((r, offset)) = self.hits.stack_list
            && r.contains(pos)
        {
            let n = self.hits.stack_list_rows.len();
            self.stack_list_scroll = (offset as isize + delta).clamp(0, n.saturating_sub(r.height as usize) as isize) as usize;
            self.stack_list_follow = false;
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
                self.skim();
            } else if self.sel >= new_off + rows {
                self.select(new_off + rows - 1);
                self.skim();
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
        if let Some((r, offset)) = self.hits.stack_list
            && r.contains(pos)
        {
            self.focus = Focus::Detail;
            if let Some(Some(entry)) = self.hits.stack_list_rows.get(offset + (pos.y - r.y) as usize).copied() {
                self.run(Action::StackSelect(entry));
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
                self.clear_list();
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
                self.clear_list();
                self.load_list();
            }
            Action::ToggleUnreadOnly => {
                let prev = self.selected_pr().map(|p| p.id.clone());
                self.unread_only = !self.unread_only;
                self.unread_keep.clear();
                // The open PR stays listed (it's read), or the view would jump to another.
                if self.unread_only
                    && self.screen == Screen::Detail
                    && let Some(id) = &prev
                {
                    self.unread_keep.insert(id.clone());
                }
                self.keep_selected(prev);
                let msg = if self.unread_only { "Showing unread PRs only (u shows all)" } else { "Showing all PRs" };
                self.toast(msg, ToastKind::Info);
            }
            Action::MarkAllRead => {
                let ids: Vec<String> = self.visible().iter().map(|&i| self.prs[i].id.clone()).filter(|id| self.unread.contains_key(id)).collect();
                if ids.is_empty() {
                    return self.toast("Nothing unread here", ToastKind::Info);
                }
                for id in &ids {
                    self.mark_read(id);
                }
                self.save_state();
                self.toast(format!("Marked {} PR{} read", ids.len(), if ids.len() == 1 { "" } else { "s" }), ToastKind::Success);
            }
            Action::ToggleUnreadFirst => {
                let prev = self.selected_pr().map(|p| p.id.clone());
                self.state.unread_first = !self.state.unread_first;
                self.unread_keep.clear();
                if self.screen == Screen::Detail
                    && let Some(id) = &prev
                {
                    self.unread_keep.insert(id.clone());
                }
                crate::state::save(&self.state);
                self.keep_selected(prev);
                self.toast(if self.state.unread_first { "Unread PRs first" } else { "Sorted by last update" }, ToastKind::Info);
            }
            Action::NextNew => self.jump_new_entry(false),
            Action::PrevNew => self.jump_new_entry(true),
            Action::OpenNew(t) => {
                self.run(Action::SetTab(t));
                self.jump_new = t.has_new();
                self.new_cursor = None;
            }
            Action::JumpTo(id) => {
                let Some(idx) = self.prs.iter().position(|p| p.id == id) else {
                    return self.toast("That PR is no longer in the list", ToastKind::Info);
                };
                if !self.visible().contains(&idx) && self.unread_only {
                    self.unread_keep.insert(id.clone());
                }
                if !self.visible().contains(&idx) && self.search.pinned.is_some() {
                    self.pin(None, Some(id.clone()));
                }
                if let Some(pos) = self.visible().iter().position(|&i| i == idx) {
                    self.select(pos);
                }
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
                    self.new_cursor = None;
                    self.end_log_search_input();
                }
                // Showing a tab clears its badge; its `NEW` markers stay until you leave the PR.
                self.ack_tab(t);
                self.focus = Focus::Detail;
                self.ensure_files(false);
                self.ensure_runs(false);
                self.ensure_stack_data();
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
                if link == "nextnew" {
                    return self.jump_new_entry(false);
                }
                if let Some(id) = link.strip_prefix("pr:") {
                    return self.open_member(id);
                }
                if link == "stackback" {
                    return self.run(Action::StackBack);
                }
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
                    ("stackfile", Ok(i)) => {
                        self.stack_open = true;
                        self.stack_file_sel = i as usize;
                        self.detail_scroll = 0;
                    }
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
            Action::StackPrev | Action::StackNext => {
                let up = action == Action::StackNext;
                let Some(s) = self.selected_stack() else { return self.toast("Not in a stack", ToastKind::Info) };
                let Some(id) = self.selected_pr().map(|p| p.id.clone()) else { return };
                match stacks::neighbour(s, &id, up) {
                    Some(m) => {
                        let id = m.id.clone();
                        self.open_member(&id);
                    }
                    None => {
                        let end = if up { "top" } else { "bottom" };
                        let n = s.member(&id).map(|m| m.number).unwrap_or_default();
                        self.toast(format!("#{n} is the {end} of the stack"), ToastKind::Info);
                    }
                }
            }
            Action::StackMove(d) => {
                if self.stack_open {
                    let n = self.stack_entry_files().map_or(0, |f| f.len());
                    if n > 0 {
                        let i = (self.stack_file_sel as isize + d).clamp(0, n as isize - 1) as usize;
                        if i != self.stack_file_sel {
                            self.stack_file_sel = i;
                            self.detail_scroll = 0;
                        }
                    }
                    return;
                }
                let n = self.stack_entries().len();
                if n > 0 {
                    let i = (self.stack_cursor() as isize + d).clamp(0, n as isize - 1) as usize;
                    self.run(Action::StackSelect(i));
                }
            }
            Action::StackSelect(i) => {
                if Some(i) != self.stack_sel.or(Some(self.stack_cursor())) || self.stack_open {
                    self.detail_scroll = 0;
                }
                self.stack_sel = Some(i);
                self.stack_open = false;
                self.stack_file_sel = 0;
                self.stack_list_follow = true;
                self.ensure_stack_data();
            }
            Action::StackOpen => {
                if self.stack_open {
                    return;
                }
                match self.stack_entry_files().map(|f| f.len()) {
                    Some(n) if n > 0 => {
                        self.stack_open = true;
                        self.stack_file_sel = self.stack_file_sel.min(n - 1);
                        self.detail_scroll = 0;
                    }
                    Some(_) => self.toast("No file changes here", ToastKind::Info),
                    None => self.toast("Files are still loading", ToastKind::Info),
                }
            }
            Action::StackBack => {
                if self.stack_open {
                    self.stack_open = false;
                    self.detail_scroll = 0;
                } else {
                    self.run(Action::Back);
                }
            }
            Action::StackOpenBrowser => {
                let target = self.stack_cursor_member().map(|m| m.number);
                match (target, self.selected_stack()) {
                    (Some(n), Some(s)) => {
                        let url = format!("https://github.com/{}/pull/{n}", s.repo);
                        self.run(Action::OpenUrl(url));
                    }
                    _ => self.run(Action::Browser),
                }
            }
            Action::StackOnGitHub => {
                let Some(s) = self.selected_stack() else { return self.toast("Not in a stack", ToastKind::Info) };
                let bottom = s.members.iter().find(|m| !m.merged()).or(s.members.first()).map(|m| m.number);
                if let Some(n) = bottom {
                    let url = format!("https://github.com/{}/pull/{n}", s.repo);
                    self.run(Action::OpenUrl(url));
                }
            }
            Action::StackUpdateBranch => self.stack_update_branch(),
            Action::StackLink => self.stack_link(),
            Action::StackMergeUpTo => self.stack_merge_menu(),
            Action::ToggleGroupStacks => {
                let prev = self.selected_pr().map(|p| p.id.clone());
                self.state.group_stacks = !self.state.group_stacks;
                crate::state::save(&self.state);
                self.keep_selected(prev);
                self.toast(if self.state.group_stacks { "Grouping stacked PRs together" } else { "Stacks not grouped" }, ToastKind::Info);
            }
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
            // Stacked PRs (docs/stacks.md); `b` is the browser here, so update-branch is `B`.
            if self.selected_stack().is_some() {
                let block = |c| self.stack_block(c);
                entries.push(MenuEntry { item: MenuItem::StackUpdate, label: "Update branch onto its parent (rebase)".into(), key: 'B', disabled: block(Command::StackUpdate) });
                entries.push(MenuEntry { item: MenuItem::StackLink, label: "Link into a GitHub stack".into(), key: 'L', disabled: block(Command::StackLink) });
                entries.push(MenuEntry { item: MenuItem::StackMerge, label: "Merge the stack up to here…".into(), key: 'M', disabled: block(Command::StackMerge) });
            }
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
                let mut msg = format!("{verb} {what}\n“{}”\n\n{} → {}", d.title, d.head_ref_name, d.base_ref_name);
                // An unlinked chain: GitHub won't retarget the PR built on this one.
                if let Some(s) = self.selected_stack().filter(|s| !s.native())
                    && let Some(up) = stacks::neighbour(s, &d.id, true).filter(|m| m.open())
                {
                    msg.push_str(&format!("\n\n#{} targets this branch and won't be retargeted\n(link the chain with L to merge it as a stack)", up.number));
                }
                (verb.to_string(), msg)
            }
            PendingOp::Close => ("Close PR".into(), format!("Close {what} without merging?\n“{}”", d.title)),
            PendingOp::Reopen => ("Reopen PR".into(), format!("Reopen {what}?\n“{}”", d.title)),
            PendingOp::Approve => ("Approve PR".into(), format!("Submit an approving review on {what}?\n“{}”", d.title)),
            // Built by their own actions, with the stack member as the target.
            PendingOp::UpdateBranch { .. } | PendingOp::LinkStack { .. } | PendingOp::MergeStack { .. } => return,
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
                    // A native stack member merges through merge-async, its open downstack with it
                    // (plain merge refuses stacked PRs).
                    MenuItem::Merge(m) if self.selected_stack().is_some_and(|s| s.native()) => self.confirm_merge_stack(&pr_id, m),
                    MenuItem::Merge(m) => {
                        let oid = self.selected_detail().map(|d| d.head_ref_oid.clone()).unwrap_or_default();
                        self.confirm(PendingOp::Merge(m, oid))
                    }
                    MenuItem::MergeStack(m) => self.confirm_merge_stack(&pr_id, m),
                    MenuItem::StackUpdate => self.run(Action::StackUpdateBranch),
                    MenuItem::StackLink => self.run(Action::StackLink),
                    MenuItem::StackMerge => self.run(Action::StackMergeUpTo),
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
                PendingOp::UpdateBranch { oid } => {
                    crate::github::rebase_or_merge(|m| {
                        let (c, id, oid) = (client.clone(), id.clone(), oid.clone());
                        async move { c.update_branch(&id, &oid, m).await }
                    })
                    .await
                }
                PendingOp::LinkStack { nwo, numbers } => client.create_stack(&nwo, &numbers).await.map(|n| format!("Linked into GitHub stack {n}")),
                PendingOp::MergeStack { nwo, up_to, method, sha, label } => run_merge_async(&client, &nwo, up_to, &sha, method, &label).await,
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

    // ---------- change signals ----------

    /// A recent mutation of yours on PR `id`.
    fn own(&self, id: &str) -> bool {
        self.own_pending.get(id).is_some_and(|at| at.elapsed() < Duration::from_secs(OWN_SECS))
    }

    /// A list row whose open detail is newer takes the detail's fields: search results can lag
    /// a little, and a stale row would read as a change back and forth.
    fn absorb_details(&mut self, ids: &HashSet<String>) {
        let me = self.viewer.as_ref().map(|v| v.login.as_str());
        for p in self.prs.iter_mut().filter(|p| ids.contains(&p.id)) {
            if let Some((d, _)) = self.details.get(&p.id)
                && d.updated_at > p.updated_at
            {
                p.absorb(d, me);
            }
        }
    }

    /// Give the PRs in `ids` a baseline if they have none (quiet, unless GitHub says unread or
    /// the PR is new to a complete list), take your own changes as seen, and recompute unread.
    fn observe(&mut self, ids: &HashSet<String>) {
        let now = changes::now_secs();
        let me = self.viewer.as_ref().map(|v| v.login.clone());
        let appeared: Option<HashSet<&str>> = self
            .prev_list
            .as_ref()
            .filter(|(q, _, complete)| *complete && *q == self.loading_query)
            .map(|(_, old, _)| old.iter().map(|p| p.id.as_str()).collect());
        for p in self.prs.iter().filter(|p| ids.contains(&p.id)) {
            let own = self.own_pending.get(&p.id).is_some_and(|at| at.elapsed() < Duration::from_secs(OWN_SECS));
            let snap = Snapshot::of_summary(p, me.as_deref());
            let gh_unread = p.is_read_by_viewer == Some(false);
            match self.state.seen.get_mut(&p.id) {
                // Yours, or read on github.com since (GitHub's flag went false → true): what's
                // there now is seen.
                Some(s) if own || (s.gh_unread && p.is_read_by_viewer == Some(true)) => {
                    let ids = if own { std::mem::take(&mut s.ids) } else { Vec::new() };
                    *s = Seen { snap: snap.rebase_on(Some(&s.snap)), at: now, unread: false, ids, gh_unread };
                }
                // Still listed: keep it from being evicted (daily is plenty for a 30-day TTL).
                Some(s) => {
                    s.snap.learn_mergeable(snap.mergeable.as_deref());
                    if now - s.at > 86400 {
                        s.at = now;
                    }
                    if p.is_read_by_viewer.is_some() {
                        s.gh_unread = gh_unread;
                    }
                }
                None => {
                    let unread = !own && (gh_unread || appeared.as_ref().is_some_and(|old| !old.contains(p.id.as_str())));
                    self.state.seen.insert(p.id.clone(), Seen { snap, at: now, unread, ids: Vec::new(), gh_unread });
                }
            }
        }
        self.rebuild_unread();
    }

    fn rebuild_unread(&mut self) {
        let me = self.viewer.as_ref().map(|v| v.login.as_str());
        let seen = &self.state.seen;
        self.unread = self
            .prs
            .iter()
            .filter_map(|p| {
                let s = seen.get(&p.id)?;
                let c = changes::diff(&s.snap, &Snapshot::of_summary(p, me));
                (s.unread || !c.is_empty()).then(|| (p.id.clone(), c))
            })
            .collect();
    }

    /// The last page of a load landed: digest what changed since the previous load of the same
    /// query (none for a new scope, state or repo), and persist the baselines.
    fn finish_list(&mut self, open_id: Option<&str>) {
        let now: Vec<PrSummary> = self.prs.iter().filter(|p| !self.search.spliced.contains(&p.id)).cloned().collect();
        self.list_complete = now.len() >= self.total;
        let first_of_session = self.list_query.is_empty();
        match self.prev_list.take() {
            Some((q, old, old_complete)) if q == self.loading_query => {
                let me = self.viewer.as_ref().map(|v| v.login.as_str());
                let mut ch = changes::diff_list(&old, &now, me, old_complete && self.list_complete);
                ch.retain(|(id, _)| !self.own(id));
                // `old` too: PRs that left the list are only there.
                let all: Vec<PrSummary> = now.into_iter().chain(old).collect();
                self.set_digest(&ch, &all, false);
                self.notify(&ch, &all);
            }
            // Startup: what changed since you last looked (baselines from an earlier session;
            // ones seeded just now have no diff, so a first run stays quiet).
            _ if first_of_session => {
                let ch: Vec<(String, Vec<Change>)> =
                    now.iter().filter_map(|p| self.unread.get(&p.id).filter(|c| !c.is_empty()).map(|c| (p.id.clone(), c.clone()))).collect();
                self.set_digest(&ch, &now, true);
                self.notify(&ch, &now);
            }
            // A different query's digest would still point at the previous list.
            Some(_) => self.digest = None,
            _ => {}
        }
        self.list_query = self.loading_query.clone();
        self.own_pending.retain(|_, at| at.elapsed() < Duration::from_secs(OWN_SECS));
        // "Unread first" re-sorts on each reload. `sel` is still the previous ordinal here,
        // so the open PR is the id captured before this load, not `selected_pr()`.
        if self.state.unread_first && !self.unread_only {
            let open = open_id.filter(|_| self.screen == Screen::Detail);
            self.unread_keep.retain(|id| Some(id.as_str()) == open);
        }
        self.save_state();
    }

    fn set_digest(&mut self, ch: &[(String, Vec<Change>)], prs: &[PrSummary], since_last: bool) {
        if ch.is_empty() {
            self.digest = None;
            return;
        }
        let (items, more) = changes::digest_items(ch, prs, DIGEST_ITEMS);
        self.digest = Some(Digest { count: ch.len(), items, more, at: Instant::now(), since_last });
    }

    /// Take PR `id` as it is now as seen.
    fn mark_read(&mut self, id: &str) {
        let Some(p) = self.prs.iter().find(|p| p.id == id) else { return };
        let me = self.viewer.as_ref().map(|v| v.login.as_str());
        let prev = self.state.seen.get(id);
        let snap = Snapshot::of_summary(p, me).rebase_on(prev.map(|s| &s.snap));
        // The entry ids go: a read without the detail can't say which entries it covered, so
        // the next `NEW` marks go by time again.
        let gh_unread = prev.is_some_and(|s| s.gh_unread);
        self.state.seen.insert(id.to_string(), Seen { snap, at: changes::now_secs(), unread: false, ids: Vec::new(), gh_unread });
        if self.unread.remove(id).is_some() && (self.unread_only || self.state.unread_first) {
            self.unread_keep.insert(id.to_string());
        }
    }

    /// The PR in the detail view is being read: start its visit (freezing what you'd seen, for
    /// the `NEW` markers), then take it as seen, at the summary level at once and at the detail
    /// level (its entry ids) whenever a detail lands. Skimming with J/K waits for `DWELL`.
    fn read_open(&mut self) {
        if self.screen != Screen::Detail {
            self.visit = None;
            self.dwell = None;
            return;
        }
        let Some(id) = self.selected_pr().map(|p| p.id.clone()) else { return };
        if self.visit.as_ref().is_none_or(|v| v.id != id) {
            self.visit_serial += 1;
            self.new_cursor = None;
            self.visit = Some(Visit {
                id: id.clone(),
                serial: self.visit_serial,
                started: Instant::now(),
                since: self.state.seen.get(&id).cloned(),
                marks: NewMarks::default(),
                checks: HashSet::new(),
                runs: HashSet::new(),
                ack: HashMap::new(),
                read_at: None,
            });
            self.refresh_marks();
        }
        if self.dwell.as_ref().is_some_and(|(d, at)| *d == id && at.elapsed() < DWELL) {
            return;
        }
        self.dwell = None;
        let mut dirty = false;
        if self.unread.contains_key(&id) || !self.state.seen.contains_key(&id) {
            self.mark_read(&id);
            dirty = true;
        }
        let at = self.details.get(&id).map(|(_, at)| *at);
        if let Some(at) = at
            && (dirty || self.visit.as_ref().is_some_and(|v| v.read_at != Some(at)))
        {
            let ids = changes::known_ids(&self.details[&id].0);
            if let Some(s) = self.state.seen.get_mut(&id)
                && s.ids != ids
            {
                s.ids = ids;
                dirty = true;
            }
            if let Some(v) = self.visit.as_mut() {
                v.read_at = Some(at);
            }
        }
        if dirty {
            self.save_state();
        }
    }

    /// Recompute the open PR's `NEW` marks from its detail and the visit's frozen baseline.
    fn refresh_marks(&mut self) {
        let me = self.viewer.as_ref().map(|v| v.login.as_str());
        let Some(v) = self.visit.as_mut() else { return };
        v.marks = match (self.details.get(&v.id), &v.since) {
            (Some((d, _)), Some(s)) => changes::new_marks(d, &s.snap, &s.ids, me, &self.cfg.bots),
            _ => NewMarks::default(),
        };
    }

    /// The open PR's new entries (or changes) in tab `t`, whether or not the badge was shown.
    pub fn tab_new(&self, t: Tab) -> usize {
        let Some(v) = self.visit.as_ref().filter(|v| self.selected_pr().is_some_and(|p| p.id == v.id)) else { return 0 };
        let Some(d) = self.details.get(&v.id).map(|(d, _)| d) else { return 0 };
        let m = &v.marks;
        // Reviews both tabs show: a verdict, or words (empty COMMENTED ones hold inline comments).
        let review = |i: &&usize| d.reviews.iter().nth(**i).is_some_and(|r| r.state != "COMMENTED" || !changes::clean_body(&r.body).trim().is_empty());
        match t {
            Tab::Description => {
                let edited = v.since.as_ref().is_some_and(|s| d.last_edited_at.is_some() && d.last_edited_at != s.snap.edited_at && !s.snap.updated_at.is_empty());
                edited as usize
            }
            Tab::Conversation => m.comments.len() + m.reviews.iter().filter(review).count(),
            Tab::Commits => m.commits.len(),
            Tab::Checks => v.checks.len(),
            Tab::Actions => v.runs.len(),
            Tab::Reviews => m.reviews.iter().filter(review).count() + m.threads.len(),
            Tab::Changes | Tab::Stack => 0,
        }
    }

    /// Tab `t`'s badge (`+2`, or `✎` for an edited description) until the tab is shown.
    pub fn tab_badge(&self, t: Tab) -> Option<String> {
        let total = self.tab_new(t);
        let acked = self.visit.as_ref().and_then(|v| v.ack.get(&t)).copied().unwrap_or(0);
        match total.saturating_sub(acked) {
            0 => None,
            _ if t == Tab::Description => Some("✎".into()),
            n => Some(format!("+{n}")),
        }
    }

    fn ack_tab(&mut self, t: Tab) {
        let total = self.tab_new(t);
        if let Some(v) = self.visit.as_mut() {
            v.ack.insert(t, total);
        }
    }

    /// Workflow runs of PR `id`'s head that finished since its last runs load: the Actions
    /// badge, and the digest. A re-run keeps the run id and bumps `run_attempt`.
    fn runs_finished(&mut self, id: &str, sha: &str, runs: &[crate::github::WorkflowRunInfo]) {
        let Some(prev) = self.runs.get(id).filter(|e| e.sha == sha) else { return };
        let before: HashMap<u64, (&str, u64)> = prev.runs.iter().map(|r| (r.id, (r.status(), r.run_attempt))).collect();
        // A run that appears already completed was missed between polls; the first load of a head stays quiet.
        let done: Vec<&crate::github::WorkflowRunInfo> = runs
            .iter()
            .filter(|r| {
                r.head_sha == sha
                    && r.status() == "completed"
                    && before.get(&r.id).is_none_or(|(status, attempt)| *status != "completed" || *attempt < r.run_attempt)
            })
            .collect();
        if done.is_empty() {
            return;
        }
        let failed = done.iter().filter(|r| matches!(r.conclusion.as_deref(), Some("failure" | "timed_out" | "startup_failure" | "action_required"))).count();
        let passed = done.iter().filter(|r| r.conclusion.as_deref() == Some("success")).count();
        if let Some(v) = self.visit.as_mut().filter(|v| v.id == id) {
            v.runs.extend(done.iter().map(|r| (r.id, r.run_attempt)));
        }
        if !self.own(id) {
            let prs = self.prs.clone();
            self.set_digest(&[(id.to_string(), vec![Change::Runs { done: done.len(), failed, passed }])], &prs, false);
        }
    }

    /// Ring / notify for interrupt-worthy changes, per `[notify]`: at most once per PR per
    /// five minutes, and never while a popup is open (the status line still has the digest).
    fn notify(&mut self, ch: &[(String, Vec<Change>)], prs: &[PrSummary]) {
        if !self.cfg.notify.enabled() || self.modal.is_some() {
            return;
        }
        let fresh: Vec<(String, Vec<Change>)> = ch.iter().filter(|(id, _)| self.notified.get(id).is_none_or(|t| t.elapsed() >= NOTIFY_EVERY)).cloned().collect();
        let me = self.viewer.as_ref().map(|v| v.login.as_str());
        let Some((ids, text)) = changes::notification(&fresh, prs, me, &self.cfg.notify) else { return };
        let cfg = &self.cfg.notify;
        let em = *self.emission.get_or_insert_with(|| changes::emission(&cfg.desktop, cfg.bell, &|k| std::env::var(k).ok(), tmux_passthrough()));
        let bytes = em.bytes(&text);
        if bytes.is_empty() {
            return;
        }
        write_raw(&bytes);
        let now = Instant::now();
        for id in ids {
            self.notified.insert(id, now);
        }
        self.last_notification = Some(text);
    }

    /// `n` / `p`: bring the next / previous `NEW` entry of the shown tab into view, wrapping.
    fn jump_new_entry(&mut self, back: bool) {
        let rows = self.hits.new_rows.clone();
        if !self.tab.has_new() || rows.is_empty() {
            return self.toast("Nothing new here", ToastKind::Info);
        }
        let target = |r: usize| r.saturating_sub(1);
        let n = rows.len();
        // Step from the last jump while the view hasn't moved since; else from the view's top.
        let from = self.new_cursor.filter(|(i, set)| *i < n && self.detail_scroll == (*set).min(self.hits.detail_max_scroll)).map(|(i, _)| i);
        let i = match (from, back) {
            (Some(i), false) => (i + 1) % n,
            (Some(i), true) => (i + n - 1) % n,
            (None, false) => rows.iter().position(|&r| target(r) > self.detail_scroll).unwrap_or(0),
            (None, true) => rows.iter().rposition(|&r| target(r) < self.detail_scroll).unwrap_or(n - 1),
        };
        self.detail_scroll = target(rows[i]);
        self.new_cursor = Some((i, self.detail_scroll));
        self.ack_tab(self.tab);
    }

    fn save_state(&mut self) {
        self.state.prune_seen(changes::now_secs());
        crate::state::save(&self.state);
    }

    /// After the list's filtering changed: keep PR `prev` selected if it's still listed.
    fn keep_selected(&mut self, prev: Option<String>) {
        let vis = self.visible();
        self.sel = prev
            .as_ref()
            .and_then(|id| vis.iter().position(|&i| self.prs[i].id == *id))
            .unwrap_or(self.sel.min(vis.len().saturating_sub(1)));
        if vis.get(self.sel).map(|&i| self.prs[i].id.clone()) != prev {
            self.detail_scroll = 0;
            self.info_scroll = 0;
            self.file_sel = 0;
            self.file_list_scroll = 0;
            self.actions_sel = 0;
            self.actions_tree_scroll = 0;
            self.actions_tree_follow = true;
            self.reset_stack_view();
            if self.screen == Screen::Detail {
                self.ensure_detail();
            }
        }
    }

    // ---------- stacks (docs/stacks.md) ----------

    /// Recompute stacks and membership from the loaded PRs and details (selection not kept).
    fn rebuild_stacks(&mut self) {
        // A detail older than the list may describe a stack since changed or dissolved; of the
        // rest, the newest is applied last so it wins.
        let mut details: Vec<&(PrDetail, Instant)> =
            self.details.values().filter(|(d, at)| d.stack.is_some() && self.list_at.is_none_or(|l| *at >= l)).collect();
        details.sort_by_key(|(_, at)| *at);
        let details: Vec<&PrDetail> = details.into_iter().map(|(d, _)| d).collect();
        let db = &self.default_branches;
        // Unknown default branch: `main` / `master` count as trunks (so a `main → release` PR
        // never becomes everyone's parent).
        let is_trunk = |repo: &str, b: &str| match db.get(&repo.to_lowercase()) {
            Some(d) => d == b,
            None => b == "main" || b == "master",
        };
        self.stacks = stacks::build(&self.prs, &details, &is_trunk);
        self.membership = stacks::membership(&self.stacks);
    }

    /// `rebuild_stacks`, keeping the selected PR selected (grouping can move rows).
    fn rebuild_stacks_keep(&mut self) {
        let id = self.selected_pr().map(|p| p.id.clone());
        self.rebuild_stacks();
        if let Some(id) = id
            && let Some(pos) = self.visible().iter().position(|&i| self.prs[i].id == id)
        {
            self.sel = pos;
        }
    }

    fn reset_stack_view(&mut self) {
        self.stack_sel = None;
        self.stack_open = false;
        self.stack_file_sel = 0;
        self.stack_list_scroll = 0;
        self.stack_list_follow = true;
    }

    /// The stack the selected PR belongs to.
    pub fn selected_stack(&self) -> Option<&Stack> {
        let p = self.selected_pr()?;
        self.membership.get(&p.id).map(|&(s, _)| &self.stacks[s])
    }

    /// The Stack tab's left column: `None` for `Whole stack`, then members top-down.
    pub fn stack_entries(&self) -> Vec<Option<usize>> {
        let Some(s) = self.selected_stack() else { return Vec::new() };
        std::iter::once(None).chain((0..s.members.len()).rev().map(Some)).collect()
    }

    /// The Stack tab cursor: as moved, else on the open PR.
    pub fn stack_cursor(&self) -> usize {
        let entries = self.stack_entries();
        if let Some(i) = self.stack_sel.filter(|&i| i < entries.len()) {
            return i;
        }
        let me = self.selected_stack().zip(self.selected_pr()).and_then(|(s, p)| s.index_of(&p.id));
        entries.iter().position(|e| *e == me && me.is_some()).unwrap_or(0)
    }

    /// The member under the Stack tab cursor (`None` on `Whole stack`).
    pub fn stack_cursor_member(&self) -> Option<&Member> {
        let s = self.selected_stack()?;
        self.stack_entries().get(self.stack_cursor()).copied().flatten().map(|i| &s.members[i])
    }

    /// What `b` / `M` act on: the Stack tab's member, else the selected PR.
    fn stack_target(&self) -> Option<&Member> {
        let s = self.selected_stack()?;
        let tab = if self.screen == Screen::Detail && self.tab == Tab::Stack { self.stack_cursor_member() } else { None };
        tab.or_else(|| s.member(&self.selected_pr()?.id))
    }

    /// `Whole stack`'s compare: (cache key, base, head). The base is the lowest open member's
    /// base (merged ones are already in it), the head the top's commit.
    pub fn whole_stack_key(s: &Stack) -> Option<(String, String, String)> {
        let base = s.members.iter().find(|m| !m.merged()).map_or(s.trunk.clone(), |m| m.base.clone());
        let top = s.members.last()?;
        let head = if top.head_oid.is_empty() { top.head.clone() } else { top.head_oid.clone() };
        Some((format!("{}:{base}...{head}", s.repo), base, head))
    }

    /// The files of the Stack tab's cursor entry, once loaded.
    pub fn stack_entry_files(&self) -> Option<&Vec<FileDiff>> {
        let s = self.selected_stack()?;
        match self.stack_cursor_member() {
            Some(m) => self.files.get(&m.id).filter(|(_, _, sha)| m.head_oid.is_empty() || *sha == m.head_oid).map(|(f, ..)| f),
            None => self.stack_files.get(&Self::whole_stack_key(s)?.0).map(|(f, _)| f),
        }
    }

    /// The open PR's stack: its needs-rebase links, and the Stack tab's files (lazily).
    fn ensure_stack_data(&mut self) {
        if self.screen != Screen::Detail {
            return;
        }
        self.ensure_links(false);
        if self.tab != Tab::Stack {
            return;
        }
        let Some(s) = self.selected_stack().cloned() else { return };
        match self.stack_cursor_member().cloned() {
            Some(m) => {
                let stale = self.files.get(&m.id).is_none_or(|(_, at, sha)| (!m.head_oid.is_empty() && *sha != m.head_oid) || at.elapsed() > Duration::from_secs(60));
                if !stale || !self.files_loading.insert(m.id.clone()) {
                    return;
                }
                let (client, tx, nwo) = (self.client.clone(), self.tx.clone(), s.repo.clone());
                tokio::spawn(async move {
                    let res = client.pr_files(&nwo, m.number).await;
                    let _ = tx.send(Msg::Files { id: m.id, sha: m.head_oid, res });
                });
            }
            None => {
                let Some((key, base, head)) = Self::whole_stack_key(&s) else { return };
                // The trunk can move under the same top head, so a result expires like a member's files.
                let fresh = self.stack_files.get(&key).is_some_and(|(_, at)| at.elapsed() <= Duration::from_secs(60));
                if fresh || self.stack_files_errors.contains_key(&key) || !self.stack_files_loading.insert(key.clone()) {
                    return;
                }
                let (client, tx, nwo) = (self.client.clone(), self.tx.clone(), s.repo.clone());
                tokio::spawn(async move {
                    let res = client.compare_files(&nwo, &base, &head).await;
                    let _ = tx.send(Msg::StackFiles { key, res });
                });
            }
        }
    }

    /// One aliased compare query per stack (1 point): which open members lack commits of their
    /// base. `fresh` asks again unless it ran in the last 10 s (a detail refresh).
    fn ensure_links(&mut self, fresh: bool) {
        let Some(s) = self.selected_stack() else { return };
        let (key, pairs, nwo) = (s.key(), s.link_pairs(), s.repo.clone());
        let max = if fresh { 10 } else { self.cfg.detail_refresh_secs.max(20) };
        let stale = self.links.get(&key).is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(max));
        if pairs.is_empty() || !stale || self.rate.as_ref().is_some_and(|r| r.remaining < 100) || !self.links_loading.insert(key.clone()) {
            return;
        }
        let (client, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = client.compare_links(&nwo, &pairs).await;
            let _ = tx.send(Msg::Links { key, res });
        });
    }

    /// The (position, size, native) chip of PR `id`.
    pub fn chip(&self, id: &str) -> Option<(usize, usize, bool)> {
        let &(s, m) = self.membership.get(id)?;
        let st = &self.stacks[s];
        Some((st.members[m].position, st.size, st.native()))
    }

    /// Commits PR `id` lacks from its base, when the open stack's compare says it needs a rebase.
    pub fn needs_rebase_of(&self, id: &str) -> Option<usize> {
        let &(s, m) = self.membership.get(id)?;
        let st = &self.stacks[s];
        let mem = &st.members[m];
        if !mem.open() {
            return None;
        }
        stacks::needs_rebase(&self.links.get(&st.key())?.0, &mem.head)
    }

    /// `2/4 of stack 196`, `2/4 · not linked`.
    pub fn stack_words(&self, id: &str) -> Option<String> {
        let &(s, _) = self.membership.get(id)?;
        let (pos, size, _) = self.chip(id)?;
        Some(match self.stacks[s].number() {
            Some(n) => format!("{pos}/{size} of stack {n}"),
            None => format!("{pos}/{size} · not linked"),
        })
    }

    /// Why a stack command can't run on the selected PR (or the Stack tab's member).
    pub fn stack_block(&self, c: Command) -> Option<String> {
        let Some(s) = self.selected_stack() else { return Some("not stacked".into()) };
        let no_write = self.selected_detail().is_some_and(|d| !d.repository.can_write());
        let id = self.selected_pr().map(|p| p.id.clone()).unwrap_or_default();
        match c {
            Command::StackPrev => stacks::neighbour(s, &id, false).is_none().then(|| "bottom of the stack".into()),
            Command::StackNext => stacks::neighbour(s, &id, true).is_none().then(|| "top of the stack".into()),
            Command::StackLink => stacks::link_plan(s).err().or_else(|| no_write.then(|| "no write access".into())),
            Command::StackMerge => {
                let m = self.stack_target()?;
                stacks::merge_plan(s, &m.id).err().or_else(|| no_write.then(|| "no write access".into()))
            }
            Command::StackUpdate => {
                let m = self.stack_target()?;
                if !m.open() {
                    Some(format!("#{} is {}", m.number, m.state.to_lowercase()))
                } else if m.head_oid.is_empty() {
                    Some("head commit not known yet".into())
                } else {
                    no_write.then(|| "no write access".into())
                }
            }
            _ => None,
        }
    }

    /// Select PR `id` (a stack member), fetching it into the list first when it isn't loaded.
    fn open_member(&mut self, id: &str) {
        if self.prs.iter().any(|p| p.id == id) {
            return self.run(Action::JumpTo(id.to_string()));
        }
        let Some((nwo, n)) = self.stacks.iter().find_map(|s| s.member(id).map(|m| (s.repo.clone(), m.number))) else {
            return self.toast("That PR isn't loaded", ToastKind::Info);
        };
        if !self.splicing.insert(id.to_string()) {
            return;
        }
        self.toast(format!("Loading #{n}…"), ToastKind::Info);
        let (client, tx, id) = (self.client.clone(), self.tx.clone(), id.to_string());
        tokio::spawn(async move {
            let res = client.prs_by_number(&[nwo], n).await;
            let _ = tx.send(Msg::Splice { id, res });
        });
    }

    /// `b`: confirm bringing the member's branch up to date with its parent.
    fn stack_update_branch(&mut self) {
        let Some(_) = self.need_detail() else { return };
        if let Some(why) = self.stack_block(Command::StackUpdate) {
            return self.toast(format!("Can't update the branch: {why}"), ToastKind::Error);
        }
        let (Some(s), Some(m)) = (self.selected_stack().cloned(), self.stack_target().cloned()) else { return };
        let onto = match s.parent_of(&m.id).filter(|p| !p.merged()) {
            Some(p) => format!("#{} ({})", p.number, m.base),
            None => m.base.clone(),
        };
        let message = format!(
            "Rebase #{} ({}) onto {onto}?\n“{}”\n\nIf GitHub refuses the rebase, the base is merged in instead.",
            m.number,
            m.head,
            truncate_chars(&m.title, 56)
        );
        self.modal = Some(Modal::Confirm { pr_id: m.id.clone(), title: "Update branch".into(), message, op: PendingOp::UpdateBranch { oid: m.head_oid.clone() } });
    }

    /// `L`: confirm linking the implicit chain into a GitHub stack.
    fn stack_link(&mut self) {
        let Some(d) = self.need_detail() else { return };
        if let Some(why) = self.stack_block(Command::StackLink) {
            return self.toast(format!("Can't link: {why}"), ToastKind::Error);
        }
        let Some(s) = self.selected_stack().cloned() else { return };
        let Ok(numbers) = stacks::link_plan(&s) else { return };
        let chain = numbers.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(" → ");
        let message = format!(
            "Link {chain} into a GitHub stack on {}?\n\nGitHub refuses (422) when stacked PRs aren't enabled\nfor the repository or a base doesn't match.",
            s.repo
        );
        self.modal = Some(Modal::Confirm { pr_id: d.id.clone(), title: "Link stack".into(), message, op: PendingOp::LinkStack { nwo: s.repo.clone(), numbers } });
    }

    /// `M`: pick a merge method for "merge the stack up to here".
    fn stack_merge_menu(&mut self) {
        let Some(d) = self.need_detail() else { return };
        if let Some(why) = self.stack_block(Command::StackMerge) {
            return self.toast(format!("Can't merge the stack: {why}"), ToastKind::Error);
        }
        let Some(m) = self.stack_target().cloned() else { return };
        let repo = &d.repository;
        let entries: Vec<MenuEntry> = [
            (repo.squash_merge_allowed, MergeMethod::Squash, 's', "Squash and merge"),
            (repo.merge_commit_allowed, MergeMethod::Merge, 'm', "Create merge commits"),
            (repo.rebase_merge_allowed, MergeMethod::Rebase, 'r', "Rebase and merge"),
        ]
        .into_iter()
        .filter(|(ok, ..)| *ok)
        .map(|(_, method, key, label)| MenuEntry { item: MenuItem::MergeStack(method), label: format!("{label} up to #{}", m.number), key, disabled: None })
        .collect();
        if entries.is_empty() {
            return self.toast("This repository allows no merge methods", ToastKind::Error);
        }
        self.modal = Some(Modal::Menu { pr_id: m.id.clone(), entries, sel: 0 });
    }

    /// Confirm merge-async of member `id` and its open downstack.
    fn confirm_merge_stack(&mut self, id: &str, method: MergeMethod) {
        let Some(&(si, _)) = self.membership.get(id) else { return };
        let s = self.stacks[si].clone();
        let plan = match stacks::merge_plan(&s, id) {
            Ok(p) => p,
            Err(e) => return self.toast(format!("Can't merge the stack: {e}"), ToastKind::Error),
        };
        let Some(top) = plan.last().copied().cloned() else { return };
        let label = plan.iter().map(|m| format!("#{}", m.number)).collect::<Vec<_>>().join(", ");
        let how = match method {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge commit",
            MergeMethod::Rebase => "rebase",
        };
        let title = if plan.len() == 1 { "Merge stacked PR" } else { "Merge stack" };
        let message = format!(
            "Merge {label} (up to #{}) into {}\n{how} · all or nothing\n\nGitHub merges the open PRs below it too (merge-async);\nit stops if #{}'s head moved.",
            top.number, s.trunk, top.number
        );
        let op = PendingOp::MergeStack { nwo: s.repo.clone(), up_to: top.number, method, sha: top.head_oid.clone(), label };
        self.modal = Some(Modal::Confirm { pr_id: top.id.clone(), title: title.into(), message, op });
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
                    // It may complete an implicit chain.
                    self.rebuild_stacks_keep();
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
        // Unread-only hides a read or newly spliced row, and `visible` would then
        // fall back to another PR. Keep the one the palette chose.
        if self.unread_only
            && let Some(id) = &select
        {
            self.unread_keep.insert(id.clone());
        }
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
            self.reset_stack_view();
            if self.screen == Screen::Detail {
                self.ensure_detail();
            }
        }
    }

    fn set_scope(&mut self, idx: usize) {
        if idx != self.scope || self.prs.is_empty() {
            self.scope = idx;
            self.sel = 0;
            self.clear_list();
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
        if c == Command::NextNew && !(self.screen == Screen::Detail && self.tab.has_new()) {
            return Some("open a PR's Conversation, Commits or Reviews".into());
        }
        if matches!(c, Command::StackPrev | Command::StackNext | Command::StackUpdate | Command::StackLink | Command::StackMerge | Command::StackBrowser) {
            return self.stack_block(c);
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
        if c == Command::StackView {
            if self.screen == Screen::List {
                self.run(Action::OpenSelected);
            }
            return self.run(Action::SetTab(Tab::Stack));
        }
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
            Command::UnreadOnly => Action::ToggleUnreadOnly,
            Command::MarkAllRead => Action::MarkAllRead,
            Command::UnreadFirst => Action::ToggleUnreadFirst,
            Command::NextNew => Action::NextNew,
            Command::StackView => return,
            Command::StackPrev => Action::StackPrev,
            Command::StackNext => Action::StackNext,
            Command::StackUpdate => Action::StackUpdateBranch,
            Command::StackLink => Action::StackLink,
            Command::StackMerge => Action::StackMergeUpTo,
            Command::StackBrowser => Action::StackOnGitHub,
            Command::GroupStacks => Action::ToggleGroupStacks,
            Command::Help => Action::Help,
            Command::Quit => Action::Quit,
        };
        self.run(action);
    }
}

/// merge-async PR `n`, following a pending request every `MERGE_POLL` for up to `MERGE_CAP`.
async fn run_merge_async(client: &Client, nwo: &str, n: u64, sha: &str, method: MergeMethod, label: &str) -> Result<String> {
    let start = Instant::now();
    let mut job = client.merge_async(nwo, n, sha, method).await?;
    // Once accepted the merge runs on GitHub's side, so a failed status request is retried.
    let mut last_err: Option<anyhow::Error> = None;
    loop {
        match stacks::merge_step(&job, start.elapsed()) {
            stacks::MergeStep::Merged => return Ok(format!("Merged {label}")),
            stacks::MergeStep::Enqueued => return Ok(format!("{label}: added to the merge queue")),
            stacks::MergeStep::Failed(m) => anyhow::bail!("Merge failed: {m}"),
            stacks::MergeStep::TimedOut => match last_err {
                Some(e) => return Err(e.context(format!("The merge's status is unknown after {} s; check the PR on GitHub", stacks::MERGE_CAP.as_secs()))),
                None => anyhow::bail!("The merge is still pending after {} s; check the PR on GitHub", stacks::MERGE_CAP.as_secs()),
            },
            stacks::MergeStep::Poll(uuid) => {
                tokio::time::sleep(stacks::MERGE_POLL).await;
                match client.merge_async_status(nwo, n, &uuid).await {
                    Ok(j) => {
                        job = j;
                        last_err = None;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
        }
    }
}

/// `s` cut to `max` characters with `…`.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>()) }
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

/// Write escape bytes straight to the terminal, past ratatui's buffer (OSC 52, BEL, OSC 9…).
fn write_raw(bytes: &str) {
    use std::io::Write;
    if cfg!(test) || bytes.is_empty() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes.as_bytes());
    let _ = out.flush();
}

/// Inside tmux, whether it passes escape sequences through to the outer terminal.
fn tmux_passthrough() -> bool {
    if std::env::var_os("TMUX").is_none() || cfg!(test) {
        return false;
    }
    std::process::Command::new("tmux")
        .args(["show-options", "-gv", "allow-passthrough"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| matches!(String::from_utf8_lossy(&o.stdout).trim(), "on" | "all"))
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

#[cfg(test)]
mod change_tests {
    use super::*;
    use crate::search::tests::pr;
    use ratatui::crossterm::event::KeyEvent;
    use serde_json::json;

    fn test_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = Config::default();
        cfg.search.bodies = false;
        let detection = crate::images::Detection { picker: None, note: None, off_reason: String::new() };
        let mut app = App::new(Client::new("test".into()).unwrap(), tx, cfg, detection);
        app.state = State::default();
        app.viewer = Some(Viewer { login: "wiiiimm".into(), orgs: Vec::new() });
        app
    }

    /// A whole one-page load of the current query, as `load_list` would dispatch it.
    fn load(app: &mut App, prs: Vec<PrSummary>) {
        app.list_gen += 1;
        app.loading_query = app.search_query();
        let total = prs.len();
        app.handle(Msg::Prs { generation: app.list_gen, first: true, res: Ok(SearchPage { prs, total, next: None, rate: None }) });
    }

    fn key(app: &mut App, c: KeyCode) {
        app.handle(Msg::Input(Event::Key(KeyEvent::new(c, KeyModifiers::NONE))));
    }

    fn prs() -> Vec<PrSummary> {
        (1..=4).map(|n| pr(n, "acme/app", &format!("pr {n}"), "2026-10-19T00:00:00Z")).collect()
    }

    fn unread(app: &App) -> Vec<u64> {
        let mut v: Vec<u64> = app.prs.iter().filter(|p| app.unread.contains_key(&p.id)).map(|p| p.number).collect();
        v.sort();
        v
    }

    fn detail(p: &PrSummary, review: Option<&str>) -> PrDetail {
        serde_json::from_value(json!({
            "id": p.id, "number": p.number, "title": p.title, "body": "", "bodyHTML": "", "state": p.state, "isDraft": p.is_draft,
            "mergeable": "MERGEABLE", "reviewDecision": review, "createdAt": "2026-10-18T00:00:00Z", "updatedAt": "2026-10-19T01:00:00Z",
            "author": { "login": "alice" }, "headRefName": "feat", "baseRefName": "main", "additions": 1, "deletions": 1, "changedFiles": 1,
            "headRefOid": p.head_ref_oid, "canBeRebased": true, "viewerCanClose": true, "viewerCanReopen": false, "viewerCanMergeAsAdmin": false,
            "labels": { "nodes": [] }, "assignees": { "nodes": [] }, "reviewRequests": { "nodes": [] }, "participants": { "nodes": [] },
            "repository": { "nameWithOwner": "acme/app", "mergeCommitAllowed": true, "squashMergeAllowed": true, "rebaseMergeAllowed": true,
                "mentionableUsers": { "nodes": [] } },
            "commits": { "totalCount": 1, "nodes": [] }, "reviews": { "nodes": [] }, "comments": { "nodes": [] }, "reviewThreads": { "nodes": [] }
        }))
        .unwrap()
    }

    #[test]
    fn quiet_first_run_seeded_by_github_unread() {
        let mut app = test_app();
        let mut list = prs();
        list[1].is_read_by_viewer = Some(false);
        list[2].is_read_by_viewer = Some(true);
        load(&mut app, list.clone());
        // Only what GitHub calls unread is dotted; no digest on a first load.
        assert_eq!(unread(&app), vec![2]);
        assert!(app.unread[&list[1].id].is_empty());
        assert!(app.digest.is_none());
        // A baseline exists now: GitHub's flag no longer matters, real changes do.
        list[2].is_read_by_viewer = Some(false);
        list[0].comments.total_count = 2;
        load(&mut app, list.clone());
        assert_eq!(unread(&app), vec![1, 2]);
        let d = app.digest.as_ref().unwrap();
        assert_eq!((d.count, d.items[0].1.as_str()), (1, "#1 +2 comments"));
        // Baselines are persisted state.
        assert_eq!(app.state.seen[&list[0].id].snap.comments, 0);
        assert!(app.state.seen[&list[1].id].unread);
    }

    #[tokio::test]
    async fn opening_marks_read_and_u_composes_with_the_text_filter() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        for p in &mut list[..3] {
            p.comments.total_count = 1;
        }
        load(&mut app, list.clone());
        assert_eq!(unread(&app), vec![1, 2, 3]);
        let numbers = |app: &App| app.visible().iter().map(|&i| app.prs[i].number).collect::<Vec<_>>();
        key(&mut app, KeyCode::Char('u'));
        assert!(app.unread_only);
        assert_eq!(numbers(&app), vec![1, 2, 3]);
        // Selecting in the list doesn't read it; opening does.
        app.select(1);
        assert!(app.unread.contains_key(&list[1].id));
        key(&mut app, KeyCode::Enter);
        assert_eq!(unread(&app), vec![1, 3]);
        assert_eq!(app.state.seen[&list[1].id].snap.comments, 1);
        key(&mut app, KeyCode::Esc);
        // Read while filtering: it stays listed and selected until the filter is toggled.
        assert_eq!(numbers(&app), vec![1, 2, 3]);
        assert_eq!(app.selected_pr().unwrap().number, 2);
        // With a pinned query, both filters apply.
        app.pin(Some("pr 3".into()), None);
        assert_eq!(numbers(&app), vec![3]);
        app.pin(None, None);
        // Esc clears the unread filter before the search filter; u toggles it back on.
        key(&mut app, KeyCode::Esc);
        assert!(!app.unread_only);
        key(&mut app, KeyCode::Char('u'));
        assert_eq!(numbers(&app), vec![1, 3]);
        // The selection follows by id where it can (#2 is gone, the row below takes it).
        assert_eq!(app.selected_pr().unwrap().number, 3);
        // U marks everything listed read.
        key(&mut app, KeyCode::Char('U'));
        assert!(unread(&app).is_empty());
        assert!(app.toast.as_ref().is_some_and(|(m, ..)| m == "Marked 2 PRs read"));
    }

    #[tokio::test]
    async fn own_actions_never_count_in_either_order() {
        for detail_first in [false, true] {
            let mut app = test_app();
            let mut list = prs();
            load(&mut app, list.clone());
            let id = list[0].id.clone();
            app.handle(Msg::Done { id: id.clone(), res: Ok("Approved".into()) });
            list[0].review_decision = Some("APPROVED".into());
            list[0].reviews.total_count = 1;
            list[0].updated_at = "2026-10-19T01:00:00Z".into();
            // Someone else's change in the same refresh still counts.
            list[1].comments.total_count = 1;
            let d = detail(&list[0], Some("APPROVED"));
            if detail_first {
                app.handle(Msg::Detail { id: id.clone(), res: Ok((d, None)) });
                assert!(unread(&app).is_empty(), "detail first");
                load(&mut app, list.clone());
            } else {
                load(&mut app, list.clone());
                app.handle(Msg::Detail { id: id.clone(), res: Ok((d, None)) });
            }
            assert_eq!(unread(&app), vec![2], "detail_first={detail_first}");
            let dg = app.digest.as_ref().unwrap();
            assert_eq!(dg.items.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>(), vec![list[1].id.clone()]);
        }
    }

    #[test]
    fn digest_only_for_the_same_query_and_never_for_spliced_results() {
        let mut app = test_app();
        let list = prs();
        load(&mut app, list.clone());
        // A GitHub result opened from the palette, then the list refreshes without it.
        let mut remote = pr(99, "other/repo", "remote", "2026-10-19T00:00:00Z");
        remote.is_read_by_viewer = Some(false);
        app.prs.insert(0, remote.clone());
        app.search.spliced.insert(remote.id.clone());
        app.prs_rev += 1;
        load(&mut app, list.clone());
        assert!(app.digest.is_none(), "{:?}", app.digest.as_ref().map(|d| &d.items));
        // Another state filter is another list: baselines stay, no digest.
        let mut changed = list.clone();
        changed[0].comments.total_count = 5;
        app.state_filter = StateFilter::All;
        load(&mut app, changed);
        assert!(app.digest.is_none());
        assert_eq!(unread(&app), vec![1]);
    }

    #[test]
    fn a_new_session_digests_changes_since_the_stored_baselines() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        let stored = std::mem::take(&mut app.state);
        // Next session: same baselines, #3 got a comment and GitHub calls #4 unread meanwhile.
        let mut app = test_app();
        app.state = stored;
        list[2].comments.total_count = 1;
        list[3].is_read_by_viewer = Some(false);
        load(&mut app, list);
        assert_eq!(unread(&app), vec![3]);
        let d = app.digest.as_ref().unwrap();
        assert!(d.since_last);
        assert_eq!(d.items.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), vec!["#3 +1 comment"]);
    }

    #[test]
    fn new_to_a_complete_list_is_unread() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        list.push(pr(5, "acme/app", "pr 5", "2026-10-19T00:00:00Z"));
        list.remove(0);
        load(&mut app, list);
        assert_eq!(unread(&app), vec![5]);
        let d = app.digest.as_ref().unwrap();
        assert_eq!(d.count, 2);
        assert!(d.items.iter().any(|(_, t)| t == "#5 new") && d.items.iter().any(|(_, t)| t == "#1 left the list"), "{:?}", d.items);
    }

    #[test]
    fn quiet_refresh_clears_the_previous_digest() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        list[0].comments.total_count = 1;
        load(&mut app, list.clone());
        assert!(app.digest.is_some());
        load(&mut app, list);
        assert!(app.digest.is_none());
    }

    #[test]
    fn unread_filter_opens_the_pr_chosen_in_search() {
        let mut app = test_app();
        let list = prs();
        load(&mut app, list.clone());
        app.unread_only = true;
        assert!(app.visible().is_empty());
        app.pin(Some("pr 3".into()), Some(list[2].id.clone()));
        assert_eq!(app.selected_pr().map(|p| p.number), Some(3));
        // A GitHub hit that is not unread yet is inserted, then opened.
        let remote = pr(99, "other/repo", "remote", "2026-10-19T00:00:00Z");
        app.prs.insert(0, remote.clone());
        app.prs_rev += 1;
        app.pin(None, Some(remote.id.clone()));
        assert_eq!(app.selected_pr().map(|p| p.number), Some(99));
    }

    #[test]
    fn unknown_mergeable_is_learned_before_it_counts() {
        let mut app = test_app();
        let mut list = prs();
        list[0].mergeable = Some("UNKNOWN".into());
        load(&mut app, list.clone());
        list[0].mergeable = Some("MERGEABLE".into());
        load(&mut app, list.clone());
        assert!(app.digest.is_none());
        assert!(!app.unread.contains_key(&list[0].id));
        assert_eq!(app.state.seen[&list[0].id].snap.mergeable.as_deref(), Some("MERGEABLE"));
        list[0].mergeable = Some("CONFLICTING".into());
        load(&mut app, list.clone());
        assert!(app.unread.contains_key(&list[0].id));
    }

    #[test]
    fn clearing_the_list_is_not_a_complete_previous_load() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        assert!(app.list_complete);
        list[0].comments.total_count = 4;
        load(&mut app, list.clone());
        assert!(app.digest.is_some());
        app.clear_list();
        assert!(!app.list_complete);
        assert!(app.prs.is_empty());
        load(&mut app, list);
        assert!(app.digest.is_none());
    }

    fn comment(id: &str, who: &str, at: &str) -> Option<crate::github::Comment> {
        Some(crate::github::Comment { id: id.into(), author: Some(crate::github::Login { login: who.into() }), body: "hi".into(), body_html: String::new(), created_at: at.into() })
    }

    /// PR #1 got two comments from others after you last looked (and one of yours): open it.
    async fn open_with_news() -> (App, Vec<PrSummary>, PrDetail) {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        list[0].comments.total_count = 4;
        list[0].updated_at = "2026-10-19T03:00:00Z".into();
        load(&mut app, list.clone());
        let mut d = detail(&list[0], None);
        d.updated_at = "2026-10-19T03:00:00Z".into();
        d.comments.total_count = 4;
        d.comments.nodes = vec![
            comment("c1", "bob", "2026-10-18T09:00:00Z"),
            comment("c2", "bob", "2026-10-19T02:00:00Z"),
            comment("c3", "wiiiimm", "2026-10-19T02:30:00Z"),
            comment("c4", "eve", "2026-10-19T03:00:00Z"),
        ];
        app.select(0);
        key(&mut app, KeyCode::Enter);
        app.handle(Msg::Detail { id: list[0].id.clone(), res: Ok((d.clone(), None)) });
        (app, list, d)
    }

    fn draw(app: &mut App) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
    }

    #[tokio::test]
    async fn open_freezes_the_baseline_and_set_tab_clears_its_badge_only() {
        let (mut app, list, _) = open_with_news().await;
        // Read at once (summary and detail level), but the visit remembers what was new.
        assert!(unread(&app).is_empty());
        assert!(!app.state.seen[&list[0].id].ids.is_empty());
        assert_eq!(app.visit.as_ref().unwrap().marks.comments, vec![1, 3]);
        assert_eq!(app.tab_badge(Tab::Conversation).as_deref(), Some("+2"));
        assert_eq!(app.tab_badge(Tab::Commits), None);
        // The Checks badge counts checks that changed during the visit; showing Conversation leaves it.
        app.visit.as_mut().unwrap().checks.insert("ci / build".into());
        key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.tab, Tab::Conversation);
        assert_eq!(app.tab_badge(Tab::Conversation), None);
        assert_eq!(app.tab_badge(Tab::Checks).as_deref(), Some("+1"));
        // The markers stay: two NEW rows, and n / p walk them, wrapping.
        draw(&mut app);
        let rows = app.hits.new_rows.clone();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let mut seen = Vec::new();
        for c in ['n', 'n', 'n', 'p'] {
            key(&mut app, KeyCode::Char(c));
            seen.push(app.new_cursor.unwrap().0);
            draw(&mut app);
        }
        assert_eq!(seen, vec![0, 1, 0, 1]);
        // Leaving the PR ends the visit; coming back, nothing is new any more.
        key(&mut app, KeyCode::Esc);
        assert!(app.visit.is_none());
        key(&mut app, KeyCode::Enter);
        assert!(app.visit.as_ref().unwrap().marks.comments.is_empty());
        assert_eq!(app.tab_badge(Tab::Conversation), None);
        draw(&mut app);
        assert!(app.hits.new_rows.is_empty());
        key(&mut app, KeyCode::Char('n'));
        assert!(app.toast.as_ref().is_some_and(|(m, ..)| m == "Nothing new here"));
    }

    #[tokio::test]
    async fn a_badge_click_opens_the_tab_at_its_first_new_entry() {
        let (mut app, ..) = open_with_news().await;
        draw(&mut app);
        let badge = app.hits.buttons.iter().find(|(_, a)| *a == Action::OpenNew(Tab::Conversation)).map(|(r, _)| *r).expect("badge");
        app.on_click(Position::new(badge.x, badge.y));
        assert_eq!(app.tab, Tab::Conversation);
        draw(&mut app);
        assert!(!app.jump_new);
        assert_eq!(app.tab_badge(Tab::Conversation), None);
    }

    #[test]
    fn read_on_github_clears_unread() {
        let mut app = test_app();
        let mut list = prs();
        list[1].is_read_by_viewer = Some(false);
        load(&mut app, list.clone());
        assert_eq!(unread(&app), vec![2]);
        // #3 gets a comment and GitHub calls it unread too.
        list[2].comments.total_count = 1;
        list[2].is_read_by_viewer = Some(false);
        load(&mut app, list.clone());
        assert_eq!(unread(&app), vec![2, 3]);
        // You read both on github.com: false → true.
        list[1].is_read_by_viewer = Some(true);
        list[2].is_read_by_viewer = Some(true);
        load(&mut app, list.clone());
        assert!(unread(&app).is_empty());
        assert_eq!(app.state.seen[&list[2].id].snap.comments, 1);
        // `true` alone (no edge) doesn't hide a change.
        list[3].is_read_by_viewer = Some(true);
        list[3].comments.total_count = 2;
        load(&mut app, list);
        assert_eq!(unread(&app), vec![4]);
    }

    #[tokio::test]
    async fn skimming_reads_after_the_dwell() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        list[1].comments.total_count = 1;
        load(&mut app, list.clone());
        app.select(0);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('J'));
        assert_eq!(app.selected_pr().unwrap().number, 2);
        // The visit started (its baseline is frozen), but #2 isn't read yet.
        assert_eq!(app.visit.as_ref().unwrap().id, list[1].id);
        assert_eq!(unread(&app), vec![2]);
        app.dwell.as_mut().unwrap().1 -= DWELL;
        app.on_tick();
        assert!(unread(&app).is_empty());
    }

    #[test]
    fn unread_first_keeps_each_group_in_order() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        list[2].comments.total_count = 1;
        list[3].comments.total_count = 1;
        load(&mut app, list);
        app.select(1);
        app.run(Action::ToggleUnreadFirst);
        let numbers = |app: &App| app.visible().iter().map(|&i| app.prs[i].number).collect::<Vec<_>>();
        assert_eq!(numbers(&app), vec![3, 4, 1, 2]);
        // The selection follows the PR.
        assert_eq!(app.selected_pr().unwrap().number, 2);
        app.run(Action::ToggleUnreadFirst);
        assert_eq!(numbers(&app), vec![1, 2, 3, 4]);
    }

    #[test]
    fn comfortable_rows_carry_words_and_clicks_hit_the_right_pr() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        list[1].comments.total_count = 2;
        list[1].commits.total_count = 3;
        load(&mut app, list);
        app.state.density = crate::state::Density::Comfortable;
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 20)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let (r, _) = app.hits.table.unwrap();
        let text = |y: u16| (0..140).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>();
        // Row 1 (#2) spans r.y + 2 and r.y + 3; its second line holds the words.
        assert!(text(r.y + 2).contains("pr 2"), "{}", text(r.y + 2));
        assert!(text(r.y + 3).contains("2 new comments"), "{}", text(r.y + 3));
        app.on_click(Position::new(r.x + 30, r.y + 3));
        assert_eq!(app.selected_pr().unwrap().number, 2);
        app.on_click(Position::new(r.x + 30, r.y + 4));
        assert_eq!(app.selected_pr().unwrap().number, 3);
    }

    #[tokio::test]
    async fn finished_runs_feed_the_digest_and_the_actions_badge() {
        let (mut app, list, d) = open_with_news().await;
        let run = |status: &str, conclusion: Option<&str>| -> crate::github::WorkflowRunInfo {
            serde_json::from_value(json!({ "id": 9, "name": "CI", "status": status, "conclusion": conclusion, "event": "pull_request",
                "run_number": 1, "head_sha": d.head_ref_oid, "html_url": "u", "created_at": "2026-10-19T00:00:00Z", "updated_at": "2026-10-19T00:00:00Z",
                "run_started_at": null, "actor": null, "triggering_actor": null }))
            .unwrap()
        };
        let msg = |r| Msg::Runs { id: list[0].id.clone(), nwo: "acme/app".into(), sha: d.head_ref_oid.clone(), history: false, res: Ok((vec![r], false)) };
        app.handle(msg(run("in_progress", None)));
        app.digest = None;
        app.handle(msg(run("completed", Some("failure"))));
        assert_eq!(app.digest.as_ref().unwrap().items[0].1, "#1 1 run failed");
        assert_eq!(app.tab_badge(Tab::Actions).as_deref(), Some("+1"));
        // A completed re-run keeps the run id and raises the attempt.
        app.digest = None;
        let mut again = run("completed", Some("success"));
        again.run_attempt = 2;
        app.handle(msg(again.clone()));
        assert_eq!(app.digest.as_ref().unwrap().items[0].1, "#1 1 run passed");
        assert_eq!(app.tab_badge(Tab::Actions).as_deref(), Some("+2"));
        app.digest = None;
        app.handle(msg(again));
        assert!(app.digest.is_none());
        assert_eq!(app.tab_badge(Tab::Actions).as_deref(), Some("+2"));
        // Started and finished between two polls: it was never seen in progress.
        app.digest = None;
        let mut quick = run("completed", Some("success"));
        quick.id = 10;
        app.handle(msg(quick));
        assert_eq!(app.digest.as_ref().unwrap().items[0].1, "#1 1 run passed");
        app.digest = None;
        let mut cancelled = run("completed", Some("cancelled"));
        cancelled.id = 11;
        app.handle(msg(cancelled));
        assert_eq!(app.digest.as_ref().unwrap().items[0].1, "#1 1 run finished");
    }

    #[test]
    fn unread_first_reload_keeps_the_open_pr_pinned() {
        let mut app = test_app();
        let mut list = prs();
        load(&mut app, list.clone());
        list[1].comments.total_count = 1;
        load(&mut app, list.clone());
        app.run(Action::ToggleUnreadFirst);
        app.select(0);
        assert_eq!(app.selected_pr().map(|p| p.number), Some(2));
        app.screen = Screen::Detail;
        app.read_open();
        assert!(app.unread_keep.contains(&list[1].id));
        app.details.insert(list[1].id.clone(), (detail(&list[1], None), Instant::now()));
        list[3].comments.total_count = 1;
        load(&mut app, list.clone());
        assert_eq!(app.selected_pr().map(|p| p.number), Some(2));
        assert!(app.unread_keep.contains(&list[1].id));
        assert!(!app.unread_keep.contains(&list[3].id));
        assert_eq!(app.prs[app.visible()[0]].number, 2);
    }

    #[test]
    fn detail_refresh_keeps_selection_when_unread_first_reorders() {
        let mut app = test_app();
        let list = prs();
        load(&mut app, list.clone());
        app.run(Action::ToggleUnreadFirst);
        app.select(1);
        let mut d = detail(&list[1], None);
        d.comments.total_count = 3;
        d.updated_at = "2026-10-19T04:00:00Z".into();
        app.handle(Msg::Detail { id: list[1].id.clone(), res: Ok((d.clone(), None)) });
        assert_eq!(app.selected_pr().map(|p| p.number), Some(2));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.visit.as_ref().map(|v| v.id.as_str()), Some(list[1].id.as_str()));
        d.comments.total_count = 4;
        d.updated_at = "2026-10-19T05:00:00Z".into();
        app.handle(Msg::Detail { id: list[1].id.clone(), res: Ok((d, None)) });
        assert_eq!(app.selected_pr().map(|p| p.number), Some(2));
        assert_eq!(app.visit.as_ref().map(|v| v.id.as_str()), Some(list[1].id.as_str()));
    }

    #[tokio::test]
    async fn checks_from_before_the_visit_are_not_a_badge() {
        let (mut app, list, mut d) = open_with_news().await;
        let id = list[0].id.clone();
        let at = app.details[&id].1;
        app.visit.as_mut().unwrap().started = at + Duration::from_secs(30);
        d.status_check_rollup = Some(
            serde_json::from_value(json!({ "state": "SUCCESS", "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS" }
            ] } }))
            .unwrap(),
        );
        app.handle(Msg::Detail { id: id.clone(), res: Ok((d.clone(), None)) });
        assert_eq!(app.tab_badge(Tab::Checks), None);
        let at = app.details[&id].1;
        app.visit.as_mut().unwrap().started = at.checked_sub(Duration::from_millis(1)).unwrap();
        d.status_check_rollup = Some(
            serde_json::from_value(json!({ "state": "SUCCESS", "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS" },
                { "__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" }
            ] } }))
            .unwrap(),
        );
        app.handle(Msg::Detail { id, res: Ok((d, None)) });
        assert_eq!(app.tab_badge(Tab::Checks).as_deref(), Some("+1"));
    }

    #[tokio::test]
    async fn html_only_review_does_not_badge() {
        let (mut app, list, _) = open_with_news().await;
        let id = list[0].id.clone();
        let review = serde_json::from_value(json!({
            "id": "rhtml", "author": { "login": "eve" }, "state": "COMMENTED", "body": "<!-- bot -->", "bodyHTML": "", "submittedAt": "2026-10-19T04:00:00Z"
        }))
        .unwrap();
        app.details.get_mut(&id).unwrap().0.reviews.nodes.push(Some(review));
        app.visit.as_mut().unwrap().marks.reviews.push(0);
        assert_eq!(app.tab_badge(Tab::Conversation).as_deref(), Some("+2"));
    }

    #[tokio::test]
    async fn empty_approval_is_a_reviews_jump_target() {
        let (mut app, list, _) = open_with_news().await;
        let id = list[0].id.clone();
        let review = serde_json::from_value(json!({
            "id": "rappr", "author": { "login": "eve" }, "state": "APPROVED", "body": "", "bodyHTML": "", "submittedAt": "2026-10-19T04:00:00Z"
        }))
        .unwrap();
        app.details.get_mut(&id).unwrap().0.reviews.nodes.push(Some(review));
        app.visit.as_mut().unwrap().marks.reviews.push(0);
        assert_eq!(app.tab_badge(Tab::Reviews).as_deref(), Some("+1"));
        app.tab = Tab::Reviews;
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let text = (0..24).map(|y| (0..120).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("1 new since you last looked"), "{text}");
        assert_eq!(app.hits.new_rows.len(), 1, "{:?}", app.hits.new_rows);
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.new_cursor.map(|c| c.0), Some(0));
    }

    #[test]
    fn notifications_follow_config_and_rate_limit() {
        let mut app = test_app();
        let ci = |state: &str| crate::github::LastCommit {
            total_count: 1,
            nodes: vec![Some(crate::github::CommitNode { commit: crate::github::RollupOnly { status_check_rollup: Some(crate::github::RollupState { state: state.into() }) } })],
        };
        let mut list = prs();
        list[0].commits = ci("SUCCESS");
        load(&mut app, list.clone());
        // Off by default.
        list[0].commits = ci("FAILURE");
        load(&mut app, list.clone());
        assert!(app.last_notification.is_none());
        app.cfg.notify.bell = true;
        list[0].commits = ci("SUCCESS");
        load(&mut app, list.clone());
        list[0].commits = ci("FAILURE");
        load(&mut app, list.clone());
        assert_eq!(app.last_notification.as_deref(), Some("#1 CI failed"));
        // Once per PR per five minutes.
        app.last_notification = None;
        list[0].review_decision = Some("CHANGES_REQUESTED".into());
        load(&mut app, list.clone());
        assert!(app.last_notification.is_none());
        // Never over a popup.
        app.notified.clear();
        app.modal = Some(Modal::Help);
        list[0].review_decision = Some("APPROVED".into());
        load(&mut app, list);
        assert!(app.last_notification.is_none());
    }
}


#[cfg(test)]
mod stack_tests {
    use super::*;
    use crate::github::{StackEntryRef, StackRef};
    use crate::search::tests::pr;
    use ratatui::crossterm::event::KeyEvent;
    use serde_json::json;

    fn test_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cfg = Config::default();
        cfg.search.bodies = false;
        let detection = crate::images::Detection { picker: None, note: None, off_reason: String::new() };
        let mut app = App::new(Client::new("test".into()).unwrap(), tx, cfg, detection);
        app.state = State::default();
        app.viewer = Some(Viewer { login: "wiiiimm".into(), orgs: Vec::new() });
        app
    }

    fn load(app: &mut App, prs: Vec<PrSummary>) {
        app.list_gen += 1;
        app.loading_query = app.search_query();
        let total = prs.len();
        app.handle(Msg::Prs { generation: app.list_gen, first: true, res: Ok(SearchPage { prs, total, next: None, rate: None }) });
    }

    fn key(app: &mut App, c: KeyCode) {
        app.handle(Msg::Input(Event::Key(KeyEvent::new(c, KeyModifiers::NONE))));
    }

    /// The implicit chain #1 → #2 → #3 → #4 (newest first) and an unrelated #9 in between.
    fn chain() -> Vec<PrSummary> {
        let mk = |n: u64, head: &str, base: &str| {
            let mut p = pr(n, "acme/app", &format!("pr {n}"), "2026-10-19T00:00:00Z");
            p.head_ref_name = head.into();
            p.base_ref_name = base.into();
            p.head_ref_oid = format!("oid{n}");
            p
        };
        vec![mk(4, "d", "c"), mk(9, "solo", "main"), mk(3, "c", "b"), mk(2, "b", "a"), mk(1, "a", "main")]
    }

    fn detail(p: &PrSummary, stack: Option<serde_json::Value>) -> PrDetail {
        serde_json::from_value(json!({
            "id": p.id, "number": p.number, "title": p.title, "body": "", "bodyHTML": "", "state": p.state, "isDraft": p.is_draft,
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "reviewDecision": null, "createdAt": "2026-10-18T00:00:00Z", "updatedAt": "2026-10-19T00:00:00Z",
            "author": { "login": "alice" }, "headRefName": p.head_ref_name, "baseRefName": p.base_ref_name, "additions": 1, "deletions": 1, "changedFiles": 1,
            "headRefOid": p.head_ref_oid, "canBeRebased": false, "viewerCanClose": true, "viewerCanReopen": false, "viewerCanMergeAsAdmin": false,
            "labels": { "nodes": [] }, "assignees": { "nodes": [] }, "reviewRequests": { "nodes": [] }, "participants": { "nodes": [] },
            "repository": { "nameWithOwner": "acme/app", "mergeCommitAllowed": true, "squashMergeAllowed": true, "rebaseMergeAllowed": false,
                "viewerPermission": "WRITE", "mentionableUsers": { "nodes": [] }, "defaultBranchRef": { "name": "main" } },
            "commits": { "totalCount": 1, "nodes": [] }, "reviews": { "nodes": [] }, "comments": { "nodes": [] }, "reviewThreads": { "nodes": [] },
            "stack": stack
        }))
        .unwrap()
    }

    fn numbers(app: &App) -> Vec<u64> {
        app.visible().iter().map(|&i| app.prs[i].number).collect()
    }

    /// Open #n in the detail view with its detail loaded.
    fn open(app: &mut App, n: u64) {
        open_with(app, n, None);
    }

    fn open_with(app: &mut App, n: u64, stack: Option<serde_json::Value>) {
        let pos = app.visible().iter().position(|&i| app.prs[i].number == n).unwrap();
        app.select(pos);
        key(app, KeyCode::Enter);
        let p = app.selected_pr().unwrap().clone();
        let d = detail(&p, stack);
        app.handle(Msg::Detail { id: p.id.clone(), res: Ok((d, None)) });
    }

    #[test]
    fn grouping_is_on_by_default_and_keeps_the_selection() {
        let mut app = test_app();
        load(&mut app, chain());
        assert_eq!(numbers(&app), vec![1, 2, 3, 4, 9]);
        let glyphs: Vec<Option<&str>> = app.visible_rows().iter().map(|r| r.glyph).collect();
        assert_eq!(glyphs, vec![None, Some("├"), Some("├"), Some("└"), None]);
        assert_eq!(app.chip(&app.prs[0].id), Some((4, 4, false)));
        app.select(4);
        assert_eq!(app.selected_pr().unwrap().number, 9);
        app.run(Action::ToggleGroupStacks);
        assert_eq!(numbers(&app), vec![4, 9, 3, 2, 1]);
        assert_eq!(app.selected_pr().unwrap().number, 9);
        app.run(Action::ToggleGroupStacks);
        assert_eq!(app.selected_pr().unwrap().number, 9);
    }

    #[tokio::test]
    async fn tab_8_and_neighbour_keys() {
        let mut app = test_app();
        load(&mut app, chain());
        open(&mut app, 2);
        key(&mut app, KeyCode::Char('8'));
        assert_eq!(app.tab, Tab::Stack);
        // Cursor on the open PR: entries are Whole stack, #4, #3, #2, #1.
        assert_eq!(app.stack_cursor(), 3);
        assert_eq!(app.stack_cursor_member().unwrap().number, 2);
        key(&mut app, KeyCode::Char('>'));
        assert_eq!(app.selected_pr().unwrap().number, 3);
        assert_eq!(app.stack_cursor_member().unwrap().number, 3);
        key(&mut app, KeyCode::Char('<'));
        key(&mut app, KeyCode::Char('<'));
        assert_eq!(app.selected_pr().unwrap().number, 1);
        key(&mut app, KeyCode::Char('<'));
        assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("#1 is the bottom of the stack"));
        // n / p move the cursor; to Whole stack at the top.
        for _ in 0..5 {
            key(&mut app, KeyCode::Char('p'));
        }
        assert_eq!(app.stack_cursor(), 0);
        assert!(app.stack_cursor_member().is_none());
    }

    #[test]
    fn the_newest_detail_wins_and_a_reload_voids_older_ones() {
        let mut app = test_app();
        let mut list = chain();
        // #3 and #4 are a native stack whose bottom (#7) isn't loaded.
        for (i, pos) in [(0usize, 3usize), (2, 2)] {
            list[i].stack = Some(StackRef { id: "PRS_1".into(), number: 12, size: 3, base_ref_name: "main".into() });
            list[i].stack_entry = Some(StackEntryRef { position: pos });
        }
        load(&mut app, list.clone());
        let entry = |n: u64, pos: usize, state: &str| json!({ "position": pos, "pullRequest": { "id": format!("acme/app#{n}"), "number": n, "title": format!("pr {n}"),
            "state": state, "headRefName": format!("h{n}"), "baseRefName": "main", "headRefOid": format!("oid{n}") } });
        let stack = |s7: &str| json!({ "id": "PRS_1", "number": 12, "size": 3, "baseRefName": "main", "entries": { "nodes": [entry(7, 1, s7), entry(3, 2, "OPEN"), entry(4, 3, "OPEN")] } });
        let now = Instant::now();
        // #4's detail saw #7 merge after #3's was fetched; either HashMap order gives the same answer.
        app.details.insert(list[2].id.clone(), (detail(&list[2], Some(stack("OPEN"))), now));
        app.details.insert(list[0].id.clone(), (detail(&list[0], Some(stack("MERGED"))), now + Duration::from_secs(1)));
        app.rebuild_stacks();
        let state7 = |app: &App| app.stacks[0].members.iter().find(|m| m.number == 7).map(|m| m.state.clone());
        assert_eq!(state7(&app).as_deref(), Some("MERGED"));
        // A list loaded since both describes the stack from its own rows until a detail is refetched.
        app.last_refresh = now + Duration::from_secs(2);
        load(&mut app, list);
        assert_eq!(state7(&app), None);
        assert_eq!(app.stacks[0].members.iter().map(|m| m.number).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[tokio::test]
    async fn a_retargeted_pr_joins_its_chain_on_its_detail() {
        let mut app = test_app();
        load(&mut app, chain());
        let mut p = app.prs.iter().find(|p| p.number == 9).unwrap().clone();
        assert!(app.chip(&p.id).is_none());
        // #9 now sits on #4's branch.
        p.base_ref_name = "d".into();
        app.handle(Msg::Detail { id: p.id.clone(), res: Ok((detail(&p, None), None)) });
        assert_eq!(app.chip(&p.id), Some((5, 5, false)));
    }

    #[tokio::test]
    async fn an_unloaded_member_is_spliced_in() {
        let mut app = test_app();
        let mut list = chain();
        // #3 and #4 are a native stack whose bottom (#7) isn't loaded.
        for (i, pos) in [(0usize, 3usize), (2, 2)] {
            list[i].stack = Some(StackRef { id: "PRS_1".into(), number: 12, size: 3, base_ref_name: "main".into() });
            list[i].stack_entry = Some(StackEntryRef { position: pos });
        }
        load(&mut app, list.clone());
        open(&mut app, 3);
        let entry = |n: u64, pos: usize| json!({ "position": pos, "pullRequest": { "id": format!("acme/app#{n}"), "number": n, "title": format!("pr {n}"),
            "state": "OPEN", "headRefName": format!("h{n}"), "baseRefName": "main", "headRefOid": format!("oid{n}") } });
        let stack = json!({ "id": "PRS_1", "number": 12, "size": 3, "baseRefName": "main", "entries": { "nodes": [entry(7, 1), entry(3, 2), entry(4, 3)] } });
        let d = detail(&list[2], Some(stack));
        app.handle(Msg::Detail { id: list[2].id.clone(), res: Ok((d, None)) });
        let s = app.selected_stack().unwrap();
        assert_eq!(s.members.iter().map(|m| (m.number, m.loaded)).collect::<Vec<_>>(), vec![(7, false), (3, true), (4, true)]);
        key(&mut app, KeyCode::Char('<'));
        assert!(app.splicing.contains("acme/app#7"));
        app.handle(Msg::Splice { id: "acme/app#7".into(), res: Ok((vec![pr(7, "acme/app", "pr 7", "2026-10-01T00:00:00Z")], None)) });
        assert_eq!(app.selected_pr().unwrap().number, 7);
        assert!(app.search.spliced.contains("acme/app#7"));
    }

    #[tokio::test]
    async fn actions_stop_at_their_confirm() {
        let mut app = test_app();
        load(&mut app, chain());
        open(&mut app, 3);
        key(&mut app, KeyCode::Char('b'));
        match &app.modal {
            Some(Modal::Confirm { pr_id, message, op: PendingOp::UpdateBranch { oid }, .. }) => {
                assert_eq!((pr_id.as_str(), oid.as_str()), ("acme/app#3", "oid3"));
                assert!(message.starts_with("Rebase #3 (c) onto #2 (b)?"), "{message}");
            }
            _ => panic!("no update-branch confirm"),
        }
        key(&mut app, KeyCode::Char('n'));
        assert!(app.modal.is_none() && app.busy.is_empty());
        key(&mut app, KeyCode::Char('L'));
        match &app.modal {
            Some(Modal::Confirm { message, op: PendingOp::LinkStack { nwo, numbers }, .. }) => {
                assert_eq!((nwo.as_str(), numbers.clone()), ("acme/app", vec![1, 2, 3, 4]));
                assert!(message.starts_with("Link #1 → #2 → #3 → #4 into a GitHub stack on acme/app?"), "{message}");
            }
            _ => panic!("no link confirm"),
        }
        key(&mut app, KeyCode::Esc);
        // An implicit chain never merges as a stack.
        key(&mut app, KeyCode::Char('M'));
        assert!(app.modal.is_none());
        assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("link it first")), "{:?}", app.toast);
        // The plain merge menu warns that #4 won't be retargeted.
        key(&mut app, KeyCode::Char('m'));
        key(&mut app, KeyCode::Char('s'));
        match &app.modal {
            Some(Modal::Confirm { message, op: PendingOp::Merge(..), .. }) => assert!(message.contains("#4 targets this branch"), "{message}"),
            _ => panic!("no merge confirm"),
        }
    }

    #[tokio::test]
    async fn native_members_merge_through_merge_async() {
        let mut app = test_app();
        let mut list = chain();
        for (i, pos) in [(4usize, 1usize), (3, 2), (2, 3), (0, 4)] {
            list[i].stack = Some(StackRef { id: "PRS_9".into(), number: 31, size: 4, base_ref_name: "main".into() });
            list[i].stack_entry = Some(StackEntryRef { position: pos });
        }
        load(&mut app, list.clone());
        let entry = |i: usize, pos: usize| json!({ "position": pos, "pullRequest": { "id": list[i].id, "number": list[i].number, "title": list[i].title,
            "state": "OPEN", "headRefName": list[i].head_ref_name, "baseRefName": list[i].base_ref_name, "headRefOid": list[i].head_ref_oid } });
        let stack = json!({ "id": "PRS_9", "number": 31, "size": 4, "baseRefName": "main", "entries": { "nodes": [entry(4, 1), entry(3, 2), entry(2, 3), entry(0, 4)] } });
        open_with(&mut app, 3, Some(stack));
        assert!(app.selected_stack().unwrap().native());
        // `M` picks a method, then confirms the downstack.
        key(&mut app, KeyCode::Char('M'));
        assert!(matches!(&app.modal, Some(Modal::Menu { entries, .. }) if entries.len() == 2));
        key(&mut app, KeyCode::Char('s'));
        match &app.modal {
            Some(Modal::Confirm { pr_id, message, op: PendingOp::MergeStack { up_to, method, sha, .. }, .. }) => {
                assert_eq!((pr_id.as_str(), *up_to, *method, sha.as_str()), ("acme/app#3", 3, MergeMethod::Squash, "oid3"));
                assert!(message.starts_with("Merge #1, #2, #3 (up to #3) into main\nsquash · all or nothing"), "{message}");
            }
            _ => panic!("no merge-stack confirm"),
        }
        key(&mut app, KeyCode::Esc);
        // The Merge menu routes a native member to merge-async too.
        key(&mut app, KeyCode::Char('m'));
        key(&mut app, KeyCode::Char('m'));
        assert!(matches!(&app.modal, Some(Modal::Confirm { op: PendingOp::MergeStack { method: MergeMethod::Merge, .. }, .. })));
        key(&mut app, KeyCode::Esc);
        // Linking an already-linked stack is refused.
        key(&mut app, KeyCode::Char('L'));
        assert!(app.modal.is_none());
        assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("already linked (stack 31)")));
        // Done refetches like any mutation.
        app.handle(Msg::Done { id: "acme/app#3".into(), res: Ok("Merged #1, #2, #3".into()) });
        assert!(app.own_pending.contains_key("acme/app#3"));
        assert!(app.detail_loading.contains("acme/app#3"));
    }

    #[tokio::test]
    async fn chips_and_the_strip_render_in_every_density() {
        let mut app = test_app();
        load(&mut app, chain());
        for d in [crate::state::Density::Comfortable, crate::state::Density::Cozy, crate::state::Density::Compact] {
            app.state.density = d;
            app.screen = Screen::List;
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 24)).unwrap();
            term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let text = |y: u16| (0..140).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>();
            let (r, _) = app.hits.table.unwrap();
            let per = d.table_row_h() as u16;
            assert!(text(r.y).contains("#1 ⧉1/4"), "{d:?}: {}", text(r.y));
            assert!(text(r.y + per).contains("├#2 ⧉2/4"), "{d:?}: {}", text(r.y + per));
            assert!(text(r.y + 3 * per).contains("└#4 ⧉4/4"), "{d:?}: {}", text(r.y + 3 * per));
            // Clicks still land on the right PR.
            app.on_click(Position::new(r.x + 40, r.y + 2 * per));
            assert_eq!(app.selected_pr().unwrap().number, 3);
        }
        app.state.density = crate::state::Density::Cozy;
        open(&mut app, 2);
        app.links.insert(app.selected_stack().unwrap().key(), (vec![Link { child: "b".into(), parent: "a".into(), ahead: 2, behind: 0, status: "DIVERGED".into() }], Instant::now()));
        key(&mut app, KeyCode::Char('8'));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 30)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let screen: String = (0..30).map(|y| (0..160).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect();
        assert!(screen.contains("main ← #1 ← [#2]! ← #3 ← #4   2/4 · not linked"), "{screen}");
        assert!(screen.contains(" 8 Stack 2/4 "), "{screen}");
        assert!(screen.contains("Whole stack"), "{screen}");
        assert!(screen.contains("needs rebase: 2 commits behind"), "{screen}");
        // A long base ← head line wraps; the strip still follows it, collapsed or not.
        let long = "feature/a-very-long-branch-name-that-needs-the-whole-width-of-the-pane";
        let id = app.selected_pr().unwrap().id.clone();
        if let Some((d, _)) = app.details.get_mut(&id) {
            d.base_ref_name = long.into();
            d.head_ref_name = format!("{long}-2");
        }
        app.detail_cache = None;
        for collapsed in [true, false] {
            app.changes_info_expanded = !collapsed;
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
            term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let screen: String = (0..30).map(|y| (0..120).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect();
            assert!(screen.contains("Stack      main ← #1 ← [#2]!"), "{screen}");
        }
        // Clicking #3 in the strip opens it.
        let (row, link) = app.hits.info_links.iter().enumerate().find_map(|(i, l)| l.iter().find(|(_, _, u)| u == "#pr:acme/app#3").map(|l| (i, l.clone()))).unwrap();
        let info = app.hits.info.unwrap();
        app.on_click(Position::new(app.hits.detail_text_x + link.0, info.y + row as u16));
        assert_eq!(app.selected_pr().unwrap().number, 3);
    }
}
