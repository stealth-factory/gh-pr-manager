//! Application state and input handling.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{ListState, TableState};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::editor::{Editor, VisualRow};
use crate::github::{Client, MergeMethod, PrDetail, PrSummary, RateLimit, ReviewEvent, SearchPage, Viewer};

/// Maximum PRs loaded per scope (GitHub search caps at 1000).
const MAX_PRS: usize = 300;

/// A rendered detail row plus its clickable link ranges (start col, end col, url).
pub type DetailRow = (ratatui::text::Line<'static>, Vec<(u16, u16, String)>);

/// Rows per PR in the detail-view sidebar.
pub const SIDEBAR_ITEM_H: usize = 2;

pub enum Msg {
    Input(Event),
    Viewer(Result<(Viewer, Option<RateLimit>)>),
    /// One page of search results; `first` replaces the list, later pages append.
    Prs { generation: u64, first: bool, res: Result<SearchPage> },
    Detail { id: String, res: Result<(PrDetail, Option<RateLimit>)> },
    /// A mutation finished; `Ok` carries the success message.
    Done { id: String, res: Result<String> },
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

/// Everything a key press or a click can trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Back,
    Refresh,
    OpenScopes,
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
    OpenUrl(String),
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
    Composer(Box<Composer>),
    Menu { pr_id: String, entries: Vec<MenuEntry>, sel: usize },
    Confirm { pr_id: String, title: String, message: String, op: PendingOp },
    Help,
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
    pub modal: Option<Rect>,
    pub modal_items: Vec<(Rect, usize)>,
    pub editor: Option<Rect>,
    pub editor_rows: Vec<VisualRow>,
    pub popup_items: Vec<(Rect, usize)>,
}

pub struct App {
    pub client: Client,
    pub tx: UnboundedSender<Msg>,
    pub cfg: Config,
    pub viewer: Option<Viewer>,
    pub scopes: Vec<Scope>,
    pub scope: usize,
    pub state_filter: StateFilter,
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
    pub detail_scroll: usize,
    /// Rendered rows for (pr id, width, fetched at); rebuilding them per frame is costly.
    pub detail_cache: Option<(String, usize, Instant, Vec<DetailRow>)>,
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
    pub fn new(client: Client, tx: UnboundedSender<Msg>, cfg: Config) -> Self {
        let mut scopes = vec![
            Scope { name: "My PRs".into(), qualifier: "author:@me".into() },
            Scope { name: "Review requested".into(), qualifier: "review-requested:@me".into() },
            Scope { name: "Involves me".into(), qualifier: "involves:@me".into() },
            Scope { name: "Assigned to me".into(), qualifier: "assignee:@me".into() },
        ];
        scopes.extend(cfg.scopes.iter().map(|s| Scope { name: s.name.clone(), qualifier: s.query.clone() }));
        let scope = cfg.default_scope.as_ref().and_then(|d| scopes.iter().position(|s| &s.name == d)).unwrap_or(0);
        Self {
            client,
            tx,
            cfg,
            viewer: None,
            scopes,
            scope,
            state_filter: StateFilter::Open,
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
            detail_cache: None,
            busy: HashSet::new(),
            drafts: HashMap::new(),
            screen: Screen::List,
            focus: Focus::Sidebar,
            modal: None,
            toast: None,
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
        format!("is:pr archived:false {} {} sort:updated-desc", self.state_filter.qualifier(), scope.qualifier)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn load_list(&mut self) {
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
    }

    fn refresh(&mut self) {
        self.load_list();
        if self.screen == Screen::Detail
            && let Some(id) = self.selected_pr().map(|p| p.id.clone())
        {
            self.load_detail(&id);
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
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input(ev) => self.on_event(ev),
            Msg::Viewer(Ok((v, rate))) => {
                self.rate = rate.or(self.rate.take());
                let mut extra = vec![Scope { name: format!("{} (my repos)", v.login), qualifier: format!("user:{}", v.login) }];
                extra.extend(v.orgs.iter().map(|o| Scope { name: format!("org: {o}"), qualifier: format!("org:{o}") }));
                let builtin = 4;
                let current = self.scopes[self.scope].clone();
                self.scopes.splice(builtin..builtin, extra);
                self.scope = self.scopes.iter().position(|s| *s == current).unwrap_or(0);
                // The configured default may name a scope that only exists now.
                if let Some(d) = &self.cfg.default_scope
                    && self.scopes[self.scope].name != *d
                    && let Some(i) = self.scopes.iter().position(|s| &s.name == d)
                {
                    self.scope = i;
                    self.load_list();
                }
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
                        let prev = self.selected_pr().map(|p| p.id.clone());
                        if first {
                            self.prs = page.prs;
                        } else {
                            let seen: HashSet<String> = self.prs.iter().map(|p| p.id.clone()).collect();
                            self.prs.extend(page.prs.into_iter().filter(|p| !seen.contains(&p.id)));
                        }
                        self.total = page.total;
                        self.loading_list = has_next && self.prs.len() < MAX_PRS;
                        self.rate = page.rate.or(self.rate.take());
                        self.list_error = None;
                        let vis = self.visible();
                        self.sel = prev
                            .and_then(|id| vis.iter().position(|&i| self.prs[i].id == id))
                            .unwrap_or(self.sel.min(vis.len().saturating_sub(1)));
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
                    }
                    Err(e) => {
                        self.detail_errors.insert(id, format!("{e:#}"));
                    }
                }
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
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) => {
                if let Some(Modal::Composer(c)) = &mut self.modal {
                    c.editor.insert_str(&s);
                } else if let Some(Modal::Scopes { filter, sel }) = &mut self.modal {
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

        let page = (self.hits.detail_height.max(4) - 2) as isize;
        let in_detail_pane = self.screen == Screen::Detail && self.focus == Focus::Detail;
        let action = match k.code {
            KeyCode::Char('q') => Some(if self.screen == Screen::Detail { Action::Back } else { Action::Quit }),
            KeyCode::Esc => Some(if self.screen == Screen::Detail { Action::Back } else { Action::ClearFilter }),
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Char('r') | KeyCode::F(5) => Some(Action::Refresh),
            KeyCode::Char('s') => Some(Action::OpenScopes),
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

    fn on_modal_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match self.modal.as_mut() {
            Some(Modal::Help) => self.modal = None,
            Some(Modal::Scopes { filter, sel }) => {
                let n = scope_matches(&self.scopes, filter).len();
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
        if let Some(modal) = self.modal.as_mut() {
            match modal {
                Modal::Composer(c) => {
                    let max = self.hits.editor_rows.len().saturating_sub(1) as isize;
                    c.editor.scroll = (c.editor.scroll as isize + delta).clamp(0, max) as usize;
                }
                Modal::Scopes { sel, .. } | Modal::Menu { sel, .. } => {
                    let n = self.hits.modal_items.len().max(1);
                    *sel = (*sel as isize + delta.signum()).clamp(0, n as isize - 1) as usize;
                }
                _ => {}
            }
            return;
        }
        if self.hits.detail.is_some_and(|r| r.contains(pos)) {
            self.focus = Focus::Detail;
            self.scroll_detail(delta);
        } else if let Some((r, offset)) = self.hits.sidebar.or(self.hits.table)
            && r.contains(pos)
        {
            // Wheel scrolls the viewport; selection follows if it would leave it.
            let n = self.visible().len();
            let per = if self.screen == Screen::List { 1 } else { SIDEBAR_ITEM_H };
            let page = r.height as usize / per;
            let new_off = (offset as isize + delta / per as isize).clamp(0, n.saturating_sub(page) as isize) as usize;
            if self.screen == Screen::List {
                *self.table_state.offset_mut() = new_off;
            } else {
                *self.side_state.offset_mut() = new_off;
                self.focus = Focus::Sidebar;
            }
            let rows = (r.height as usize / if self.screen == Screen::List { 1 } else { SIDEBAR_ITEM_H }).max(1);
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
            let idx = offset + (pos.y - r.y) as usize;
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
            self.select(offset + (pos.y - r.y) as usize / SIDEBAR_ITEM_H);
            return;
        }
        if let Some(r) = self.hits.detail
            && r.contains(pos)
        {
            self.focus = Focus::Detail;
            let row = self.detail_scroll + (pos.y - r.y) as usize;
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
            }
            Action::Refresh => {
                self.refresh();
                self.toast("Refreshing…", ToastKind::Info);
            }
            Action::OpenScopes => self.modal = Some(Modal::Scopes { filter: String::new(), sel: self.scope }),
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
            Action::OpenUrl(url) => match open_url(&url) {
                Ok(()) => self.toast(format!("Opened {url}"), ToastKind::Info),
                Err(e) => self.toast(format!("Couldn't open browser: {e}"), ToastKind::Error),
            },
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
            Some(Modal::Scopes { filter, sel }) => {
                let matches = scope_matches(&self.scopes, &filter);
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

pub fn scope_matches(scopes: &[Scope], filter: &str) -> Vec<usize> {
    let f = filter.to_lowercase();
    scopes
        .iter()
        .enumerate()
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
}
