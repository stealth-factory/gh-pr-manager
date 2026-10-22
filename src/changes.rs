//! "What changed" signals: snapshot the fields of a PR worth signalling, diff two snapshots,
//! and summarise a refresh. Pure; the app decides what the baselines are (docs/change-signals.md).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::NotifyConfig;
use crate::github::{PrDetail, PrSummary, login_of};

/// The signal fields of a PR at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub updated_at: String,
    pub state: String,
    pub is_draft: bool,
    pub comments: usize,
    pub reviews: usize,
    pub threads: usize,
    pub review_requests: usize,
    pub commits: usize,
    pub head: String,
    pub ci: Option<String>,
    pub review: Option<String>,
    pub mergeable: Option<String>,
    pub edited_at: Option<String>,
    pub asked_me: bool,
    /// The viewer authored it (a green CI is only news on your own PRs).
    pub mine: bool,
}

impl Snapshot {
    pub fn of_summary(p: &PrSummary, me: Option<&str>) -> Self {
        Self {
            updated_at: p.updated_at.clone(),
            state: p.state.clone(),
            is_draft: p.is_draft,
            comments: p.comments.total_count,
            reviews: p.reviews.total_count,
            threads: p.review_threads.total_count,
            review_requests: p.review_requests.total_count,
            commits: p.commits.total_count,
            head: p.head_ref_oid.clone(),
            ci: p.ci_state().map(str::to_string),
            review: p.review_decision.clone(),
            mergeable: p.mergeable.clone(),
            edited_at: p.last_edited_at.clone(),
            asked_me: p.viewer_latest_review_request.as_ref().is_some_and(|v| !v.is_null()),
            mine: me.is_some_and(|me| login_of(&p.author).eq_ignore_ascii_case(me)),
        }
    }

    /// The same fields read from a detail; agrees with `of_summary` of a row that absorbed it.
    pub fn of_detail(d: &PrDetail, me: Option<&str>) -> Self {
        Self {
            updated_at: d.updated_at.clone(),
            state: d.state.clone(),
            is_draft: d.is_draft,
            comments: d.comments.total_count,
            reviews: d.reviews.total_count,
            threads: d.review_threads.total_count,
            review_requests: d.review_requests.iter().count(),
            commits: d.commits.total_count,
            head: d.head_ref_oid.clone(),
            ci: d.rollup().map(|r| r.state.clone()),
            review: d.review_decision.clone(),
            mergeable: Some(d.mergeable.clone()),
            edited_at: d.last_edited_at.clone(),
            asked_me: d.viewer_latest_review_request.as_ref().is_some_and(|v| !v.is_null()),
            mine: me.is_some_and(|me| login_of(&d.author).eq_ignore_ascii_case(me)),
        }
    }

    /// `self` as a new baseline, keeping `prev`'s known mergeability over `UNKNOWN` (GitHub
    /// computes it lazily, so a later `CONFLICTING` must still diff against the last known value).
    pub fn rebase_on(mut self, prev: Option<&Snapshot>) -> Self {
        if !known_mergeable(self.mergeable.as_deref())
            && let Some(m) = prev.and_then(|p| p.mergeable.clone()).filter(|m| known_mergeable(Some(m)))
        {
            self.mergeable = Some(m);
        }
        self
    }

    /// Remember the first real mergeability. `UNKNOWN` is not a baseline, so a later
    /// `CONFLICTING` would otherwise have nothing to diff against.
    pub fn learn_mergeable(&mut self, new: Option<&str>) {
        if !known_mergeable(self.mergeable.as_deref()) && known_mergeable(new) {
            self.mergeable = new.map(str::to_string);
        }
    }
}

fn known_mergeable(m: Option<&str>) -> bool {
    matches!(m, Some("MERGEABLE" | "CONFLICTING"))
}

/// A failing CI rollup (the `✗` set of `ui::ci_icon`).
pub fn ci_failing(s: Option<&str>) -> bool {
    matches!(s, Some("FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// New to a complete list of the same query.
    Appeared,
    /// Gone from a complete list of the same query (merged, closed, or no longer matching).
    Left,
    State { from: String, to: String },
    Ready,
    Edited,
    Comments(usize),
    Reviews(usize),
    Threads(usize),
    /// Head moved; `n` new commits, `force` when the count didn't grow (force-push).
    Commits { n: usize, head: String, force: bool },
    Ci { from: Option<String>, to: String },
    Verdict { from: Option<String>, to: String },
    ReviewRequested,
    Mergeable { from: Option<String>, to: String },
    /// Workflow runs of the head commit finished (the Actions tab's polling).
    /// `passed` of them succeeded; `failed` of them failed. The rest finished some other way.
    Runs { done: usize, failed: usize, passed: usize },
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("+1 {one}") } else { format!("+{n} {one}s") }
}

impl Change {
    /// 1 = interrupt-worthy, 2 = worth a dot. Noise never becomes a `Change`.
    pub fn tier(&self) -> u8 {
        match self {
            Change::ReviewRequested | Change::Verdict { .. } => 1,
            Change::Ci { to, .. } if ci_failing(Some(to)) => 1,
            Change::State { to, .. } if to != "OPEN" => 1,
            Change::Mergeable { to, .. } if to == "CONFLICTING" => 1,
            _ => 2,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Change::Appeared => "new".into(),
            Change::Left => "left the list".into(),
            Change::State { to, .. } => match to.as_str() {
                "MERGED" => "merged".into(),
                "CLOSED" => "closed".into(),
                "OPEN" => "reopened".into(),
                other => other.to_lowercase(),
            },
            Change::Ready => "ready for review".into(),
            Change::Edited => "description edited".into(),
            Change::Comments(n) => plural(*n, "comment"),
            Change::Reviews(n) => plural(*n, "review"),
            Change::Threads(n) => plural(*n, "thread"),
            Change::Commits { force: true, .. } => "force-pushed".into(),
            Change::Commits { n, .. } => plural(*n, "commit"),
            Change::Ci { to, .. } if ci_failing(Some(to)) => "CI failed".into(),
            Change::Ci { .. } => "CI passed".into(),
            Change::Verdict { to, .. } if to == "APPROVED" => "approved".into(),
            Change::Verdict { .. } => "changes requested".into(),
            Change::ReviewRequested => "review requested".into(),
            Change::Mergeable { to, .. } if to == "CONFLICTING" => "conflicts".into(),
            Change::Mergeable { .. } => "conflicts resolved".into(),
            Change::Runs { failed, .. } if *failed > 0 => format!("{failed} run{} failed", if *failed == 1 { "" } else { "s" }),
            Change::Runs { done, passed, .. } if *done > 0 && *passed == *done => format!("{done} run{} passed", if *done == 1 { "" } else { "s" }),
            Change::Runs { done, .. } => format!("{done} run{} finished", if *done == 1 { "" } else { "s" }),
        }
    }

    /// Longer words for Comfortable's second row: `2 new comments`, `3 commits pushed`.
    pub fn words(&self) -> String {
        let n = |n: usize, one: &str| format!("{n} new {one}{}", if n == 1 { "" } else { "s" });
        match self {
            Change::Comments(c) => n(*c, "comment"),
            Change::Reviews(c) => n(*c, "review"),
            Change::Threads(c) => n(*c, "review thread"),
            Change::Commits { n: c, force: false, .. } => format!("{c} commit{} pushed", if *c == 1 { "" } else { "s" }),
            Change::Appeared => "new to this list".into(),
            other => other.label(),
        }
    }

    /// The `[notify]` event this is, if any (tier 1 only).
    pub fn notify(&self) -> Option<&'static str> {
        match self {
            Change::ReviewRequested => Some("review_requested"),
            Change::Verdict { to, .. } if to == "APPROVED" => Some("approved"),
            Change::Verdict { .. } => Some("changes_requested"),
            Change::Ci { to, .. } if ci_failing(Some(to)) => Some("ci_failed"),
            Change::State { to, .. } if to == "MERGED" => Some("merged"),
            Change::State { to, .. } if to == "CLOSED" => Some("closed"),
            Change::Mergeable { to, .. } if to == "CONFLICTING" => Some("conflicts"),
            _ => None,
        }
    }
}

/// What changed from `old` to `new`, most important first. Count decreases, `→ PENDING`, an
/// `updatedAt`-only bump and `UNKNOWN` mergeability are never changes.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut out = Vec::new();
    if !old.state.is_empty() && !new.state.is_empty() && old.state != new.state {
        out.push(Change::State { from: old.state.clone(), to: new.state.clone() });
    }
    if new.asked_me && !old.asked_me {
        out.push(Change::ReviewRequested);
    }
    if new.ci != old.ci
        && let Some(to) = new.ci.clone()
        && ((ci_failing(Some(&to)) && !ci_failing(old.ci.as_deref())) || (to == "SUCCESS" && new.mine))
    {
        out.push(Change::Ci { from: old.ci.clone(), to });
    }
    if new.review != old.review
        && let Some(to) = new.review.clone().filter(|r| r == "APPROVED" || r == "CHANGES_REQUESTED")
    {
        out.push(Change::Verdict { from: old.review.clone(), to });
    }
    if known_mergeable(old.mergeable.as_deref())
        && known_mergeable(new.mergeable.as_deref())
        && let Some(to) = new.mergeable.clone().filter(|m| Some(m) != old.mergeable.as_ref())
    {
        out.push(Change::Mergeable { from: old.mergeable.clone(), to });
    }
    if old.is_draft && !new.is_draft && new.state == "OPEN" {
        out.push(Change::Ready);
    }
    if new.edited_at.is_some() && new.edited_at != old.edited_at {
        out.push(Change::Edited);
    }
    if new.comments > old.comments {
        out.push(Change::Comments(new.comments - old.comments));
    }
    if new.reviews > old.reviews {
        out.push(Change::Reviews(new.reviews - old.reviews));
    }
    if new.threads > old.threads {
        out.push(Change::Threads(new.threads - old.threads));
    }
    if !old.head.is_empty() && !new.head.is_empty() && old.head != new.head {
        let n = new.commits.saturating_sub(old.commits);
        out.push(Change::Commits { n, head: new.head.clone(), force: n == 0 });
    }
    // Stable: tier first, then the field order above.
    out.sort_by_key(Change::tier);
    out
}

/// Per-PR changes between two loads of a list, matched by id (order doesn't matter), in `new`'s
/// order. `Appeared` / `Left` only when both lists are complete loads of the same query.
pub fn diff_list(old: &[PrSummary], new: &[PrSummary], me: Option<&str>, complete: bool) -> Vec<(String, Vec<Change>)> {
    let before: HashMap<&str, &PrSummary> = old.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut out = Vec::new();
    for p in new {
        let changes = match before.get(p.id.as_str()) {
            Some(o) => diff(&Snapshot::of_summary(o, me), &Snapshot::of_summary(p, me)),
            None if complete => vec![Change::Appeared],
            None => Vec::new(),
        };
        if !changes.is_empty() {
            out.push((p.id.clone(), changes));
        }
    }
    if complete {
        let now: std::collections::HashSet<&str> = new.iter().map(|p| p.id.as_str()).collect();
        out.extend(old.iter().filter(|p| !now.contains(p.id.as_str())).map(|p| (p.id.clone(), vec![Change::Left])));
    }
    out
}

/// The digest's items: (PR id, `#n label`), most important first, at most `max`, plus how many
/// more PRs changed.
pub fn digest_items(changes: &[(String, Vec<Change>)], prs: &[PrSummary], max: usize) -> (Vec<(String, String)>, usize) {
    let mut ranked: Vec<(u8, usize, &String, &Change)> =
        changes.iter().enumerate().filter_map(|(i, (id, c))| c.first().map(|c| (c.tier(), i, id, c))).collect();
    ranked.sort_by_key(|(tier, i, ..)| (*tier, *i));
    let number = |id: &str| prs.iter().find(|p| p.id == id).map(|p| format!("#{}", p.number));
    let items: Vec<(String, String)> = ranked
        .iter()
        .take(max)
        .map(|(_, _, id, c)| ((*id).clone(), format!("{} {}", number(id).unwrap_or_else(|| "a PR".into()), c.label())))
        .collect();
    let more = ranked.len() - items.len();
    (items, more)
}

/// `3 PRs changed · #151 CI failed · #437 review requested · +1 more` (the status line draws
/// the same from `digest_items`, with each item clickable).
#[cfg(test)]
pub fn digest(changes: &[(String, Vec<Change>)], prs: &[PrSummary], max: usize) -> String {
    let n = changes.iter().filter(|(_, c)| !c.is_empty()).count();
    let (items, more) = digest_items(changes, prs, max);
    let mut s = format!("{n} PR{} changed", if n == 1 { "" } else { "s" });
    for (_, t) in items {
        s.push_str(" · ");
        s.push_str(&t);
    }
    if more > 0 {
        s.push_str(&format!(" · +{more} more"));
    }
    s
}

/// A bot by the `[bot]` suffix or the configured list (`cfg.bots`).
pub fn is_bot(login: &str, bots: &[String]) -> bool {
    login.ends_with("[bot]") || bots.iter().any(|b| b.eq_ignore_ascii_case(login))
}

/// A short, stable hash of an entry id (comment, review, commit oid) for `Seen::ids`.
pub fn id_hash(id: &str) -> u32 {
    // FNV-1a.
    id.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

/// Every entry id a detail shows (comments, reviews, inline comments, commits), hashed and
/// sorted: what "seen" covers when you read it.
pub fn known_ids(d: &PrDetail) -> Vec<u32> {
    let mut v: Vec<u32> = d
        .comments
        .iter()
        .map(|c| c.id.as_str())
        .chain(d.reviews.iter().map(|r| r.id.as_str()))
        .chain(d.review_threads.iter().flat_map(|t| t.comments.iter().map(|c| c.id.as_str())))
        .chain(d.commits.nodes.iter().flatten().map(|c| c.commit.oid.as_str()))
        .filter(|id| !id.is_empty())
        .map(id_hash)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Entries new since a baseline, as indices into each connection's flattened nodes (in the
/// order `iter()` yields them). Threads are (thread, comment) pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewMarks {
    pub comments: Vec<usize>,
    pub reviews: Vec<usize>,
    pub threads: Vec<(usize, usize)>,
    pub commits: Vec<usize>,
}

/// What in `d` is new since `since` (the baseline frozen when the PR was opened). With `known`
/// (the entry ids seen when it was last read in detail) an entry is new when its id is
/// unknown, which stays exact after edits and deletions and catches review comments drafted
/// before you looked but published after. Without it, `createdAt > since.updated_at` decides
/// (equal is not new), and commits are the newest `commits` delta. The viewer's own entries and
/// bots' never count.
pub fn new_marks(d: &PrDetail, since: &Snapshot, known: &[u32], me: Option<&str>, bots: &[String]) -> NewMarks {
    let others = |a: &Option<crate::github::Login>| {
        let l = login_of(a);
        !me.is_some_and(|me| l.eq_ignore_ascii_case(me)) && !is_bot(l, bots)
    };
    let fresh = |id: &str, at: &str| {
        if !known.is_empty() && !id.is_empty() { known.binary_search(&id_hash(id)).is_err() } else { !at.is_empty() && at > since.updated_at.as_str() }
    };
    let mut m = NewMarks::default();
    if since.updated_at.is_empty() && known.is_empty() {
        return m;
    }
    m.comments = d.comments.iter().enumerate().filter(|(_, c)| others(&c.author) && fresh(&c.id, &c.created_at)).map(|(i, _)| i).collect();
    m.reviews = d
        .reviews
        .iter()
        .enumerate()
        .filter(|(_, r)| r.state != "PENDING" && others(&r.author) && fresh(&r.id, r.submitted_at.as_deref().unwrap_or("")))
        .map(|(i, _)| i)
        .collect();
    for (t, th) in d.review_threads.iter().enumerate() {
        m.threads.extend(th.comments.iter().enumerate().filter(|(_, c)| others(&c.author) && fresh(&c.id, &c.created_at)).map(|(i, _)| (t, i)));
    }
    let commits: Vec<&crate::github::CommitInfo> = d.commits.nodes.iter().flatten().map(|n| &n.commit).collect();
    let by_others = |c: &crate::github::CommitInfo| {
        let a = c.author();
        !me.is_some_and(|me| a.eq_ignore_ascii_case(me)) && !is_bot(&a, bots)
    };
    if !known.is_empty() {
        m.commits = commits.iter().enumerate().filter(|(_, c)| !c.oid.is_empty() && known.binary_search(&id_hash(&c.oid)).is_err() && by_others(c)).map(|(i, _)| i).collect();
    } else if !since.head.is_empty() && since.head != d.head_ref_oid {
        let n = d.commits.total_count.saturating_sub(since.commits).min(commits.len());
        m.commits = (commits.len() - n..commits.len()).filter(|&i| by_others(commits[i])).collect();
    }
    m
}

/// Per-PR changes between two details of the same PR, refined past what list counts can tell:
/// comments, reviews and inline comments count entries by others (not you, not bots).
pub fn diff_detail(old: &PrDetail, new: &PrDetail, me: Option<&str>, bots: &[String]) -> Vec<Change> {
    let base = Snapshot::of_detail(old, me);
    let marks = new_marks(new, &base, &known_ids(old), me, bots);
    let mut out: Vec<Change> = diff(&base, &Snapshot::of_detail(new, me))
        .into_iter()
        .filter(|c| !matches!(c, Change::Comments(_) | Change::Reviews(_) | Change::Threads(_)))
        .collect();
    // Review verdicts show up as `Verdict`; count the reviews that carry words.
    let reviews = marks.reviews.iter().filter(|&&i| new.reviews.iter().nth(i).is_some_and(|r| r.state != "COMMENTED" || !clean_body(&r.body).trim().is_empty())).count();
    let mut counts = Vec::new();
    if !marks.comments.is_empty() {
        counts.push(Change::Comments(marks.comments.len()));
    }
    if reviews > 0 {
        counts.push(Change::Reviews(reviews));
    }
    if !marks.threads.is_empty() {
        counts.push(Change::Threads(marks.threads.len()));
    }
    // Keep the field order: counts go before `Commits`.
    let at = out.iter().position(|c| matches!(c, Change::Commits { .. })).unwrap_or(out.len());
    out.splice(at..at, counts);
    out.sort_by_key(Change::tier);
    out
}

/// Labels of checks whose outcome changed to a finished one between two details (the Checks
/// tab's `+n`). A check that's gone pending is not news.
pub fn checks_changed(old: &PrDetail, new: &PrDetail) -> Vec<String> {
    let pending = |o: &str| matches!(o, "PENDING" | "EXPECTED" | "IN_PROGRESS" | "QUEUED" | "WAITING");
    let before: HashMap<String, String> = old.rollup().map(|r| r.contexts.iter().map(|c| (c.label(), c.outcome())).collect()).unwrap_or_default();
    let Some(r) = new.rollup() else { return Vec::new() };
    r.contexts
        .iter()
        .map(|c| (c.label(), c.outcome()))
        .filter(|(l, o)| !pending(o) && before.get(l) != Some(o))
        .map(|(l, _)| l)
        .collect()
}

/// The notification for a refresh's changes, if any: `#151 CI failed · #437 review requested`.
/// Only `cfg.events`, and with `mine_only` only PRs you authored or were asked to review.
/// Returns the PRs it names too (for the per-PR rate limit).
pub fn notification(ev: &[(String, Vec<Change>)], prs: &[PrSummary], me: Option<&str>, cfg: &NotifyConfig) -> Option<(Vec<String>, String)> {
    let mut ids = Vec::new();
    let mut parts = Vec::new();
    for (id, changes) in ev {
        let Some(p) = prs.iter().find(|p| p.id == *id) else { continue };
        let snap = Snapshot::of_summary(p, me);
        let picked: Vec<String> = changes
            .iter()
            .filter(|c| c.notify().is_some_and(|e| cfg.events.iter().any(|x| x == e)))
            .filter(|c| !cfg.mine_only || snap.mine || snap.asked_me || **c == Change::ReviewRequested)
            .map(Change::label)
            .collect();
        if !picked.is_empty() {
            ids.push(id.clone());
            parts.push(format!("#{} {}", p.number, picked.join(", ")));
        }
    }
    (!parts.is_empty()).then(|| (ids, parts.join(" · ")))
}

/// A desktop notification escape sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Osc9,
    Osc777,
    Osc99,
}

impl Desktop {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "osc9" => Some(Desktop::Osc9),
            "osc777" => Some(Desktop::Osc777),
            "osc99" => Some(Desktop::Osc99),
            _ => None,
        }
    }
}

/// Text that can't break out of an OSC string (no ESC, BEL or other controls; `;` separates
/// OSC 777 fields).
fn osc_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).map(|c| if c == ';' { ',' } else { c }).collect()
}

/// The escape sequence for a desktop notification.
pub fn osc(kind: Desktop, title: &str, body: &str) -> String {
    let (t, b) = (osc_text(title), osc_text(body));
    match kind {
        Desktop::Osc9 => format!("\x1b]9;{t}: {b}\x07"),
        Desktop::Osc777 => format!("\x1b]777;notify;{t};{b}\x07"),
        // Kitty: the title (d=0: more to come), then the body.
        Desktop::Osc99 => format!("\x1b]99;i=1:d=0;{t}\x1b\\\x1b]99;i=1:p=body;{b}\x1b\\"),
    }
}

/// tmux DCS pass-through: the sequence with every ESC doubled.
pub fn tmux_wrap(seq: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
}

/// How to deliver a notification: ring the bell, and/or send a desktop sequence (wrapped
/// for tmux pass-through when inside tmux).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emission {
    pub bell: bool,
    pub desktop: Option<Desktop>,
    pub tmux: bool,
}

impl Emission {
    /// The bytes to write for `body`.
    pub fn bytes(&self, body: &str) -> String {
        let mut out = String::new();
        if let Some(k) = self.desktop {
            let seq = osc(k, "gh-pr-manager", body);
            out.push_str(&if self.tmux { tmux_wrap(&seq) } else { seq });
        }
        if self.bell {
            out.push('\x07');
        }
        out
    }
}

/// What a notification emits, given the `desktop` and `bell` settings and the environment.
/// `auto` picks by terminal; Herdr, tmux without pass-through and unknown terminals get BEL only.
pub fn emission(desktop: &str, bell: bool, env: &dyn Fn(&str) -> Option<String>, tmux_passthrough: bool) -> Emission {
    let has = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    let term_program = env("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let herdr = term_program == "herdr" || has("HERDR_ENV");
    let tmux = has("TMUX");
    let key = desktop.trim().to_ascii_lowercase();
    // An unrecognised value behaves as `auto` (case doesn't matter). `off` stays off.
    let key = match key.as_str() {
        "off" | "auto" | "osc9" | "osc777" | "osc99" => key,
        _ => "auto".into(),
    };
    let desktop = match key.as_str() {
        "auto" => {
            let term = env("TERM").unwrap_or_default();
            let lc = env("LC_TERMINAL").unwrap_or_default().to_lowercase();
            let kind = if herdr || (tmux && !tmux_passthrough) {
                None
            } else if has("KITTY_WINDOW_ID") || term == "xterm-kitty" {
                Some(Desktop::Osc99)
            } else if term_program == "iterm.app" || lc == "iterm2" || term_program == "wezterm" || has("WEZTERM_PANE") {
                Some(Desktop::Osc9)
            } else if term_program == "ghostty" || has("GHOSTTY_RESOURCES_DIR") || term.starts_with("rxvt") {
                Some(Desktop::Osc777)
            } else {
                None
            };
            // Nothing better known: the bell is the notification.
            return Emission { bell: bell || kind.is_none(), desktop: kind, tmux };
        }
        named => Desktop::parse(named),
    };
    Emission { bell, desktop, tmux }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Strip HTML comments and carriage returns from GitHub markdown. A review whose body is only
/// an HTML comment is not shown, so it is not a badge or a digest entry either.
pub(crate) fn clean_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            Some(j) => rest = &rest[i + j + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    let out = out.replace('\r', "");
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines() {
        if l.trim().is_empty() && lines.last().is_none_or(|p| p.trim().is_empty()) {
            continue;
        }
        lines.push(l);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::tests::pr;

    fn snap() -> Snapshot {
        Snapshot {
            updated_at: "2026-10-19T05:00:00Z".into(),
            state: "OPEN".into(),
            comments: 3,
            reviews: 2,
            threads: 1,
            commits: 4,
            head: "aaa".into(),
            ci: Some("SUCCESS".into()),
            review: Some("REVIEW_REQUIRED".into()),
            mergeable: Some("MERGEABLE".into()),
            ..Snapshot::default()
        }
    }

    fn one(f: impl FnOnce(&mut Snapshot)) -> Vec<Change> {
        let old = snap();
        let mut new = old.clone();
        f(&mut new);
        diff(&old, &new)
    }

    #[test]
    fn each_field_yields_its_change() {
        assert_eq!(one(|s| s.state = "MERGED".into()), vec![Change::State { from: "OPEN".into(), to: "MERGED".into() }]);
        assert_eq!(one(|s| s.asked_me = true), vec![Change::ReviewRequested]);
        assert_eq!(one(|s| s.ci = Some("FAILURE".into())), vec![Change::Ci { from: Some("SUCCESS".into()), to: "FAILURE".into() }]);
        assert_eq!(
            one(|s| s.review = Some("APPROVED".into())),
            vec![Change::Verdict { from: Some("REVIEW_REQUIRED".into()), to: "APPROVED".into() }]
        );
        assert_eq!(
            one(|s| s.mergeable = Some("CONFLICTING".into())),
            vec![Change::Mergeable { from: Some("MERGEABLE".into()), to: "CONFLICTING".into() }]
        );
        assert_eq!(one(|s| s.edited_at = Some("2026-10-19T06:00:00Z".into())), vec![Change::Edited]);
        assert_eq!(one(|s| s.comments = 5), vec![Change::Comments(2)]);
        assert_eq!(one(|s| s.reviews = 3), vec![Change::Reviews(1)]);
        assert_eq!(one(|s| s.threads = 4), vec![Change::Threads(3)]);
        assert_eq!(
            one(|s| {
                s.head = "bbb".into();
                s.commits = 7;
            }),
            vec![Change::Commits { n: 3, head: "bbb".into(), force: false }]
        );
        let mut draft = snap();
        draft.is_draft = true;
        assert_eq!(diff(&draft, &snap()), vec![Change::Ready]);
    }

    #[test]
    fn noise_is_not_a_change() {
        // updatedAt-only bump, decreasing counts, → PENDING, UNKNOWN mergeability, title-ish noise.
        assert!(one(|s| s.updated_at = "2026-10-19T09:00:00Z".into()).is_empty());
        assert!(
            one(|s| {
                s.comments = 1;
                s.reviews = 0;
                s.threads = 0;
                s.commits = 2;
            })
            .is_empty()
        );
        assert!(one(|s| s.ci = Some("PENDING".into())).is_empty());
        assert!(one(|s| s.mergeable = Some("UNKNOWN".into())).is_empty());
        let mut unknown = snap();
        unknown.mergeable = Some("UNKNOWN".into());
        let mut conflicting = snap();
        conflicting.mergeable = Some("CONFLICTING".into());
        assert!(diff(&unknown, &conflicting).is_empty());
        // Green on someone else's PR is noise; on yours it is news.
        let mut red = snap();
        red.ci = Some("FAILURE".into());
        assert!(diff(&red, &snap()).is_empty());
        red.mine = true;
        let mut green = snap();
        green.mine = true;
        assert_eq!(diff(&red, &green), vec![Change::Ci { from: Some("FAILURE".into()), to: "SUCCESS".into() }]);
        // Failing → failing is not news; a dismissed verdict isn't either.
        let mut error = red.clone();
        error.ci = Some("ERROR".into());
        assert!(diff(&red, &error).is_empty());
        assert!(one(|s| s.review = None).is_empty());
    }

    #[test]
    fn force_push_is_head_moved_without_new_commits() {
        assert_eq!(one(|s| s.head = "ccc".into()), vec![Change::Commits { n: 0, head: "ccc".into(), force: true }]);
        assert_eq!(Change::Commits { n: 0, head: String::new(), force: true }.label(), "force-pushed");
        // No head known on either side (an older state file): nothing.
        let mut old = snap();
        old.head.clear();
        assert!(diff(&old, &snap()).is_empty());
    }

    #[test]
    fn tier_order() {
        let changes = one(|s| {
            s.comments = 9;
            s.ci = Some("FAILURE".into());
            s.edited_at = Some("x".into());
            s.asked_me = true;
        });
        assert_eq!(changes, vec![Change::ReviewRequested, Change::Ci { from: Some("SUCCESS".into()), to: "FAILURE".into() }, Change::Edited, Change::Comments(6)]);
        assert_eq!(changes.iter().map(Change::tier).collect::<Vec<_>>(), vec![1, 1, 2, 2]);
    }

    #[test]
    fn rebase_keeps_known_mergeability() {
        let mut unknown = snap();
        unknown.mergeable = Some("UNKNOWN".into());
        assert_eq!(unknown.clone().rebase_on(Some(&snap())).mergeable.as_deref(), Some("MERGEABLE"));
        assert_eq!(unknown.rebase_on(None).mergeable.as_deref(), Some("UNKNOWN"));
    }

    #[test]
    fn learn_mergeable_keeps_the_first_known_value() {
        let mut s = snap();
        s.mergeable = Some("UNKNOWN".into());
        s.learn_mergeable(Some("MERGEABLE"));
        assert_eq!(s.mergeable.as_deref(), Some("MERGEABLE"));
        s.learn_mergeable(Some("CONFLICTING"));
        assert_eq!(s.mergeable.as_deref(), Some("MERGEABLE"));
        s.learn_mergeable(None);
        assert_eq!(s.mergeable.as_deref(), Some("MERGEABLE"));
    }

    #[test]
    fn of_summary_reads_the_signal_fields() {
        let mut p = pr(7, "a/b", "t", "2026-01-01T00:00:00Z");
        p.comments.total_count = 4;
        p.viewer_latest_review_request = Some(serde_json::json!({ "asCodeOwner": false }));
        let s = Snapshot::of_summary(&p, Some("WIIIIMM"));
        assert_eq!((s.comments, s.asked_me, s.mine), (4, true, true));
        assert!(!Snapshot::of_summary(&p, Some("alice")).mine);
    }

    #[test]
    fn diff_list_matches_by_id_across_reorders() {
        let a = pr(1, "a/b", "one", "2026-01-01T00:00:00Z");
        let b = pr(2, "a/b", "two", "2026-01-01T00:00:00Z");
        let mut b2 = b.clone();
        b2.comments.total_count = 2;
        let c = pr(3, "a/b", "three", "2026-01-01T00:00:00Z");
        let old = vec![a.clone(), b.clone()];
        // Reordered, b got comments, c is new, a stays.
        let new = vec![b2.clone(), c.clone(), a.clone()];
        assert_eq!(diff_list(&old, &new, None, false), vec![(b.id.clone(), vec![Change::Comments(2)])]);
        let full = diff_list(&old, &new, None, true);
        assert_eq!(full, vec![(b.id.clone(), vec![Change::Comments(2)]), (c.id.clone(), vec![Change::Appeared])]);
        // a left a complete list.
        let gone = diff_list(&old, &[b2], None, true);
        assert!(gone.contains(&(a.id.clone(), vec![Change::Left])));
        assert!(!diff_list(&old, &[b.clone()], None, false).iter().any(|(_, c)| c.contains(&Change::Left)));
    }

    #[test]
    fn digest_ranks_and_truncates() {
        let prs: Vec<PrSummary> = (1..=5).map(|n| pr(n, "a/b", "t", "2026-01-01T00:00:00Z")).collect();
        let id = |n: usize| prs[n - 1].id.clone();
        let changes = vec![
            (id(1), vec![Change::Comments(2)]),
            (id(2), vec![Change::Ci { from: None, to: "FAILURE".into() }]),
            (id(3), vec![Change::ReviewRequested]),
            (id(4), vec![Change::Edited]),
            (id(5), vec![Change::Commits { n: 1, head: "x".into(), force: false }]),
        ];
        assert_eq!(digest(&changes, &prs, 3), "5 PRs changed · #2 CI failed · #3 review requested · #1 +2 comments · +2 more");
        assert_eq!(digest(&changes[..1], &prs, 3), "1 PR changed · #1 +2 comments");
        let (items, more) = digest_items(&changes, &prs, 1);
        assert_eq!((items[0].0.clone(), more), (id(2), 4));
    }
    /// A detail with comments/reviews/threads/commits as (id, author, at) triples.
    fn det(comments: &[(&str, &str, &str)], reviews: &[(&str, &str, &str, &str)], thread: &[(&str, &str, &str)], commits: &[(&str, &str)]) -> PrDetail {
        let c = |(id, a, at): &(&str, &str, &str)| serde_json::json!({ "id": id, "author": { "login": a }, "body": "b", "bodyHTML": "", "createdAt": at });
        let commit_nodes: Vec<serde_json::Value> = commits
            .iter()
            .map(|(oid, a)| {
                let commit = serde_json::json!({ "oid": oid, "abbreviatedOid": &oid[..2.min(oid.len())], "messageHeadline": "m",
                    "committedDate": "2026-10-19T00:00:00Z", "url": "u", "authors": { "nodes": [{ "name": a, "user": { "login": a } }] } });
                serde_json::json!({ "commit": commit })
            })
            .collect();
        let review_nodes: Vec<serde_json::Value> = reviews
            .iter()
            .map(|(id, a, st, at)| serde_json::json!({ "id": id, "author": { "login": a }, "state": st, "body": "", "bodyHTML": "", "submittedAt": at }))
            .collect();
        let comment_nodes: Vec<serde_json::Value> = comments.iter().map(c).collect();
        let thread_nodes: Vec<serde_json::Value> = thread.iter().map(c).collect();
        let repo = serde_json::json!({ "nameWithOwner": "acme/app", "mergeCommitAllowed": true, "squashMergeAllowed": true,
            "rebaseMergeAllowed": true, "mentionableUsers": { "nodes": [] } });
        let threads = serde_json::json!({ "totalCount": 1, "nodes": [{ "isResolved": false, "path": "a.rs", "line": 1, "comments": { "nodes": thread_nodes } }] });
        let mut v = serde_json::json!({
            "id": "P1", "number": 7, "title": "t", "body": "", "bodyHTML": "", "state": "OPEN", "isDraft": false,
            "mergeable": "MERGEABLE", "reviewDecision": null, "createdAt": "2026-10-18T00:00:00Z", "updatedAt": "2026-10-19T09:00:00Z",
            "author": { "login": "alice" }, "headRefName": "feat", "baseRefName": "main", "additions": 1, "deletions": 1, "changedFiles": 1,
            "headRefOid": commits.last().map(|c| c.0).unwrap_or("h0"), "canBeRebased": true, "viewerCanClose": true, "viewerCanReopen": false,
            "viewerCanMergeAsAdmin": false, "labels": { "nodes": [] }, "assignees": { "nodes": [] },
            "participants": { "nodes": [] }, "lastEditedAt": null, "isReadByViewer": true, "timelineItems": { "nodes": [] }
        });
        let o = v.as_object_mut().unwrap();
        o.insert("reviewRequests".into(), serde_json::json!({ "nodes": [{ "requestedReviewer": { "login": "wiiiimm" } }] }));
        o.insert("repository".into(), repo);
        o.insert("statusCheckRollup".into(), serde_json::json!({ "state": "FAILURE", "contexts": { "nodes": [] } }));
        o.insert("commits".into(), serde_json::json!({ "totalCount": commits.len(), "nodes": commit_nodes }));
        o.insert("reviews".into(), serde_json::json!({ "totalCount": reviews.len(), "nodes": review_nodes }));
        o.insert("comments".into(), serde_json::json!({ "totalCount": comments.len(), "nodes": comment_nodes }));
        o.insert("reviewThreads".into(), threads);
        serde_json::from_value(v)
        .unwrap()
    }

    fn bots() -> Vec<String> {
        vec!["coderabbitai".into()]
    }

    #[test]
    fn of_summary_and_of_detail_agree() {
        let d = det(&[("c1", "bob", "2026-10-19T01:00:00Z")], &[], &[], &[("aaa", "bob"), ("bbb", "bob")]);
        let mut p = pr(7, "acme/app", "old title", "2026-10-01T00:00:00Z");
        p.id = d.id.clone();
        p.author = d.author.clone();
        p.absorb(&d, Some("wiiiimm"));
        assert_eq!(Snapshot::of_summary(&p, Some("wiiiimm")), Snapshot::of_detail(&d, Some("wiiiimm")));
        assert!(diff(&Snapshot::of_detail(&d, Some("wiiiimm")), &Snapshot::of_summary(&p, Some("wiiiimm"))).is_empty());
        // An UNKNOWN detail doesn't erase a known mergeability.
        let mut unknown = d.clone();
        unknown.mergeable = "UNKNOWN".into();
        p.absorb(&unknown, None);
        assert_eq!(p.mergeable.as_deref(), Some("MERGEABLE"));
    }

    #[test]
    fn absorb_takes_the_viewer_review_request_from_the_detail_field() {
        let d = det(&[], &[], &[], &[("aaa", "bob")]);
        let mut p = pr(7, "acme/app", "t", "2026-10-01T00:00:00Z");
        p.id = d.id.clone();
        p.viewer_latest_review_request = Some(serde_json::json!({ "asCodeOwner": false }));
        // The detail lists the viewer in `reviewRequests`, but the viewer field is absent: the flag clears.
        p.absorb(&d, Some("wiiiimm"));
        assert!(p.viewer_latest_review_request.is_none());
        assert!(!Snapshot::of_detail(&d, Some("wiiiimm")).asked_me);
        let mut asked = d.clone();
        asked.review_requests.nodes.clear();
        asked.viewer_latest_review_request = Some(serde_json::json!({ "asCodeOwner": true }));
        p.absorb(&asked, Some("wiiiimm"));
        assert!(Snapshot::of_summary(&p, Some("wiiiimm")).asked_me);
        assert!(Snapshot::of_detail(&asked, Some("wiiiimm")).asked_me);
    }

    #[test]
    fn new_marks_by_time_exclude_viewer_and_bots() {
        let since = Snapshot { updated_at: "2026-10-19T05:00:00Z".into(), head: "bbb".into(), commits: 2, ..Snapshot::default() };
        let d = det(
            &[
                ("c1", "bob", "2026-10-19T04:00:00Z"),
                ("c2", "bob", "2026-10-19T05:00:00Z"), // equal: not new
                ("c3", "bob", "2026-10-19T06:00:00Z"),
                ("c4", "wiiiimm", "2026-10-19T06:00:00Z"),
                ("c5", "coderabbitai", "2026-10-19T06:00:00Z"),
                ("c6", "dependabot[bot]", "2026-10-19T06:00:00Z"),
            ],
            &[("r1", "carol", "APPROVED", "2026-10-19T07:00:00Z"), ("r2", "carol", "PENDING", "2026-10-19T07:00:00Z")],
            &[("t1", "dave", "2026-10-19T03:00:00Z"), ("t2", "dave", "2026-10-19T08:00:00Z")],
            &[("aaa", "bob"), ("bbb", "bob"), ("ccc", "bob"), ("ddd", "wiiiimm")],
        );
        let m = new_marks(&d, &since, &[], Some("wiiiimm"), &bots());
        assert_eq!(m.comments, vec![2]);
        assert_eq!(m.reviews, vec![0]);
        assert_eq!(m.threads, vec![(0, 1)]);
        // Two commits past the baseline's count; yours doesn't count.
        assert_eq!(m.commits, vec![2]);
        // No baseline at all: nothing is new.
        assert_eq!(new_marks(&d, &Snapshot::default(), &[], None, &[]), NewMarks::default());
    }

    #[test]
    fn new_marks_by_id_survive_edits_and_late_publishes() {
        let before = det(&[("c1", "bob", "2026-10-19T04:00:00Z")], &[], &[], &[("aaa", "bob")]);
        let known = known_ids(&before);
        let since = Snapshot { updated_at: "2026-10-19T05:00:00Z".into(), head: "aaa".into(), commits: 1, ..Snapshot::default() };
        // c1 was deleted, c0 (older, drafted in a review before you looked) shows up now, and
        // the branch was force-pushed: the rewritten commit is new, the count didn't grow.
        let after = det(&[("c0", "bob", "2026-10-19T03:00:00Z")], &[], &[], &[("zzz", "bob")]);
        let m = new_marks(&after, &since, &known, Some("wiiiimm"), &bots());
        assert_eq!((m.comments, m.commits), (vec![0], vec![0]));
        // The same entries again are not new.
        assert_eq!(new_marks(&before, &since, &known, None, &[]), NewMarks::default());
    }

    #[test]
    fn diff_detail_counts_others_entries_only() {
        let old = det(&[("c1", "bob", "2026-10-19T04:00:00Z")], &[], &[], &[("aaa", "bob")]);
        let mut new = det(
            &[("c1", "bob", "2026-10-19T04:00:00Z"), ("c2", "coderabbitai", "2026-10-19T10:00:00Z"), ("c3", "eve", "2026-10-19T10:00:00Z")],
            &[("r1", "carol", "CHANGES_REQUESTED", "2026-10-19T10:00:00Z")],
            &[],
            &[("aaa", "bob"), ("bbb", "bob")],
        );
        new.review_decision = Some("CHANGES_REQUESTED".into());
        let ch = diff_detail(&old, &new, Some("wiiiimm"), &bots());
        assert_eq!(
            ch,
            vec![
                Change::Verdict { from: None, to: "CHANGES_REQUESTED".into() },
                Change::Comments(1),
                Change::Reviews(1),
                Change::Commits { n: 1, head: "bbb".into(), force: false },
            ]
        );
        assert!(diff_detail(&new, &new, None, &[]).is_empty());
    }

    #[test]
    fn html_only_commented_review_is_not_a_change() {
        let old = det(&[], &[], &[], &[("aaa", "bob")]);
        let mut new = det(&[], &[("r1", "carol", "COMMENTED", "2026-10-19T10:00:00Z")], &[], &[("aaa", "bob")]);
        new.reviews.nodes[0].as_mut().unwrap().body = "<!-- bot -->".into();
        let ch = diff_detail(&old, &new, Some("wiiiimm"), &bots());
        assert!(ch.iter().all(|c| !matches!(c, Change::Reviews(_))), "{ch:?}");
    }

    #[test]
    fn checks_changed_skips_pending() {
        let ctx = |name: &str, status: &str, concl: Option<&str>| serde_json::json!({ "__typename": "CheckRun", "name": name, "status": status, "conclusion": concl });
        let mut old = det(&[], &[], &[], &[("aaa", "bob")]);
        let mut new = old.clone();
        old.status_check_rollup = serde_json::from_value(serde_json::json!({ "state": "PENDING", "contexts": { "nodes": [
            ctx("a", "IN_PROGRESS", None), ctx("b", "COMPLETED", Some("SUCCESS")), ctx("c", "COMPLETED", Some("SUCCESS")) ] } })).unwrap();
        new.status_check_rollup = serde_json::from_value(serde_json::json!({ "state": "FAILURE", "contexts": { "nodes": [
            ctx("a", "COMPLETED", Some("FAILURE")), ctx("b", "COMPLETED", Some("SUCCESS")), ctx("c", "IN_PROGRESS", None), ctx("d", "COMPLETED", Some("SUCCESS")) ] } })).unwrap();
        assert_eq!(checks_changed(&old, &new), vec!["a".to_string(), "d".to_string()]);
    }

    #[test]
    fn words_for_comfortable() {
        assert_eq!(Change::Comments(2).words(), "2 new comments");
        assert_eq!(Change::Commits { n: 3, head: String::new(), force: false }.words(), "3 commits pushed");
        assert_eq!(Change::Ci { from: None, to: "FAILURE".into() }.words(), "CI failed");
        assert_eq!(Change::Runs { done: 2, failed: 1, passed: 1 }.label(), "1 run failed");
        assert_eq!(Change::Runs { done: 2, failed: 0, passed: 2 }.label(), "2 runs passed");
        assert_eq!(Change::Runs { done: 1, failed: 0, passed: 0 }.label(), "1 run finished");
    }

    #[test]
    fn osc_sequences_are_byte_exact() {
        assert_eq!(osc(Desktop::Osc9, "gh", "#1 CI failed"), "\x1b]9;gh: #1 CI failed\x07");
        assert_eq!(osc(Desktop::Osc777, "gh", "a;b"), "\x1b]777;notify;gh;a,b\x07");
        assert_eq!(osc(Desktop::Osc99, "gh", "x"), "\x1b]99;i=1:d=0;gh\x1b\\\x1b]99;i=1:p=body;x\x1b\\");
        // Nothing can end the sequence early.
        assert_eq!(osc(Desktop::Osc9, "gh", "a\x07\x1b]b"), "\x1b]9;gh: a]b\x07");
        assert_eq!(tmux_wrap("\x1b]9;x\x07"), "\x1bPtmux;\x1b\x1b]9;x\x07\x1b\\");
    }

    #[test]
    fn emission_by_terminal() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let e = |desktop: &str, bell: bool, pairs: &'static [(&'static str, &'static str)], pass: bool| emission(desktop, bell, &env(pairs), pass);
        assert_eq!(e("off", true, &[], false), Emission { bell: true, desktop: None, tmux: false });
        assert_eq!(e("auto", false, &[("TERM_PROGRAM", "iTerm.app")], false).desktop, Some(Desktop::Osc9));
        assert_eq!(e("auto", false, &[("TERM_PROGRAM", "ghostty")], false).desktop, Some(Desktop::Osc777));
        assert_eq!(e("auto", false, &[("TERM", "xterm-kitty")], false).desktop, Some(Desktop::Osc99));
        // Herdr, unknown terminals and tmux without pass-through: BEL only.
        for pairs in [&[("TERM_PROGRAM", "herdr")][..], &[("TERM", "xterm-256color")], &[("TMUX", "/tmp/t,1,0"), ("TERM_PROGRAM", "iTerm.app")]] {
            assert_eq!(e("auto", false, pairs, false), Emission { bell: true, desktop: None, tmux: pairs.iter().any(|(k, _)| *k == "TMUX") });
        }
        // tmux with pass-through wraps; a named sequence is sent even in Herdr.
        let t = e("auto", false, &[("TMUX", "/tmp/t,1,0"), ("LC_TERMINAL", "iTerm2")], true);
        assert!(t.tmux && t.bytes("x").starts_with("\x1bPtmux;"));
        assert_eq!(e("osc777", false, &[("TERM_PROGRAM", "herdr")], false).desktop, Some(Desktop::Osc777));
        assert_eq!(e("OSC9", false, &[], false).desktop, Some(Desktop::Osc9));
        assert_eq!(e("on", false, &[("TERM_PROGRAM", "iTerm.app")], false).desktop, Some(Desktop::Osc9));
        assert_eq!(e("  OFF ", false, &[], false), Emission { bell: false, desktop: None, tmux: false });
        let mut cfg = NotifyConfig::default();
        cfg.desktop = "OFF".into();
        assert!(!cfg.enabled());
        cfg.desktop = "OSC9".into();
        assert!(cfg.enabled());
        assert_eq!(Emission { bell: true, desktop: None, tmux: false }.bytes("x"), "\x07");
    }

    #[test]
    fn notification_filters_events_and_mine() {
        let mut cfg = NotifyConfig::default();
        let mut mine = pr(1, "a/b", "t", "2026-01-01T00:00:00Z");
        mine.author = Some(crate::github::Login { login: "wiiiimm".into() });
        let mut theirs = pr(2, "a/b", "t", "2026-01-01T00:00:00Z");
        theirs.author = Some(crate::github::Login { login: "alice".into() });
        let prs = vec![mine.clone(), theirs.clone()];
        let ev = vec![
            (mine.id.clone(), vec![Change::Ci { from: None, to: "FAILURE".into() }, Change::Comments(2)]),
            (theirs.id.clone(), vec![Change::Verdict { from: None, to: "APPROVED".into() }]),
        ];
        let (ids, text) = notification(&ev, &prs, Some("wiiiimm"), &cfg).unwrap();
        assert_eq!((ids, text.as_str()), (vec![mine.id.clone()], "#1 CI failed"));
        cfg.mine_only = false;
        assert_eq!(notification(&ev, &prs, Some("wiiiimm"), &cfg).unwrap().1, "#1 CI failed · #2 approved");
        // A review request is always about you; tier-2 changes never notify.
        let req = vec![(theirs.id.clone(), vec![Change::ReviewRequested])];
        cfg.mine_only = true;
        assert_eq!(notification(&req, &prs, Some("wiiiimm"), &cfg).unwrap().1, "#2 review requested");
        assert!(notification(&[(mine.id.clone(), vec![Change::Comments(1)])], &prs, Some("wiiiimm"), &cfg).is_none());
        cfg.events.clear();
        assert!(notification(&ev, &prs, Some("wiiiimm"), &cfg).is_none());
        assert!(!cfg.enabled() && !NotifyConfig::default().enabled());
    }
}
