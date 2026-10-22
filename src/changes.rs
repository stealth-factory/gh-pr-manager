//! "What changed" signals: snapshot the fields of a PR worth signalling, diff two snapshots,
//! and summarise a refresh. Pure; the app decides what the baselines are (docs/change-signals.md).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::github::{PrSummary, login_of};

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

pub fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
}
