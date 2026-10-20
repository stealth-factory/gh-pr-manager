//! Machine-written state at `$XDG_STATE_HOME/gh-pr-manager/state.json`: recent repos, a repo
//! list cache, and what you last saw of each PR.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::changes::Snapshot;
use crate::github::RepoInfo;
use crate::stats::Window;

/// Most PRs remembered in `seen`; the least recently touched go first.
const SEEN_CAP: usize = 2000;
/// `seen` entries untouched for this long are dropped.
const SEEN_TTL_SECS: i64 = 30 * 86400;

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub density: Density,
    /// Most recently picked repo filters, newest first.
    pub recent_repos: Vec<String>,
    pub repos_cache: Option<ReposCache>,
    /// Inline images shown (the `I` key).
    pub images_enabled: bool,
    /// Searches picked or pinned in the `/` palette, newest first.
    pub recent_searches: Vec<String>,
    /// What you last saw of each PR, by PR id; a row is unread when it differs from this.
    pub seen: HashMap<String, Seen>,
    /// Sort unread PRs to the top of the list.
    pub unread_first: bool,
    /// Keep each stack's PRs together in the list, bottom first (docs/stacks.md).
    pub group_stacks: bool,
    /// The stats panel (`T`); `None` follows `[stats] visible`.
    pub stats_visible: Option<bool>,
    /// The stats panel's last counts per scope string, for an instant first paint.
    pub stats_cache: HashMap<String, StatsCache>,
}

/// Stats cached for one scope (docs/stats-panel.md §3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatsCache {
    /// (UTC date, opened, merged); a figure that never arrived is `None`, never 0.
    pub days: Vec<(String, Option<i64>, Option<i64>)>,
    pub window: Option<Window>,
    /// The 24 h before `window` (its deltas).
    pub prev: Option<Window>,
    /// Unix seconds of the older of the two counts.
    pub at: i64,
}

/// Scopes kept in `stats_cache`, and how old an entry may get.
const STATS_CAP: usize = 8;
const STATS_TTL_SECS: i64 = 7 * 86400;

impl Default for State {
    fn default() -> Self {
        Self {
            density: Density::default(),
            recent_repos: Vec::new(),
            repos_cache: None,
            images_enabled: true,
            recent_searches: Vec::new(),
            seen: HashMap::new(),
            unread_first: false,
            group_stacks: true,
            stats_visible: None,
            stats_cache: HashMap::new(),
        }
    }
}

/// The baseline a PR is diffed against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    #[serde(flatten)]
    pub snap: Snapshot,
    /// Unix seconds this entry was last written or seen in a list (for eviction).
    #[serde(default)]
    pub at: i64,
    /// Unread with no diff yet: GitHub said unread when the baseline was taken, or the PR
    /// newly appeared in a list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unread: bool,
    /// Entry ids (comments, reviews, inline comments, commit oids; `changes::id_hash`) shown
    /// when it was last read in the detail view; exact `NEW` marks. Empty: go by time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<u32>,
    /// GitHub called it unread (`isReadByViewer == false`) last time; a later `true` means
    /// it was read on github.com.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gh_unread: bool,
}

/// How much spacing the UI uses, from roomy to tight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    Comfortable,
    #[default]
    Cozy,
    Compact,
}

impl Density {
    pub fn next(self) -> Self {
        match self {
            Density::Comfortable => Density::Cozy,
            Density::Cozy => Density::Compact,
            Density::Compact => Density::Comfortable,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Density::Comfortable => "Comfortable",
            Density::Cozy => "Cozy",
            Density::Compact => "Compact",
        }
    }

    /// Terminal rows per PR in the full list table.
    pub fn table_row_h(self) -> usize {
        if self == Density::Comfortable { 2 } else { 1 }
    }

    /// Terminal rows per PR in the detail-view sidebar.
    pub fn sidebar_item_h(self) -> usize {
        match self {
            Density::Comfortable => 3,
            Density::Cozy => 2,
            Density::Compact => 1,
        }
    }

    /// Blank rows above and below the detail tab bar.
    pub fn tab_margin(self) -> u16 {
        if self == Density::Compact { 0 } else { 1 }
    }

    pub fn column_spacing(self) -> u16 {
        match self {
            Density::Comfortable => 3,
            Density::Cozy => 2,
            Density::Compact => 1,
        }
    }

    /// Columns of padding on each side of the PR table.
    pub fn side_padding(self) -> u16 {
        if self == Density::Compact { 0 } else { 1 }
    }

    /// Whether to draw separator rules (above the table, under the detail action bar).
    pub fn rules(self) -> bool {
        self != Density::Compact
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReposCache {
    pub login: String,
    pub repos: Vec<RepoInfo>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))?;
    Some(base.join("gh-pr-manager").join("state.json"))
}

pub fn load() -> State {
    path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Best effort; failing to persist state should never interrupt the user. Tests never write.
pub fn save(state: &State) {
    if cfg!(test) {
        return;
    }
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string(state) {
        let _ = std::fs::write(p, s);
    }
}

impl State {
    /// Drop `seen` entries untouched for 30 days, then the oldest beyond the cap.
    pub fn prune_seen(&mut self, now: i64) {
        self.seen.retain(|_, s| now - s.at <= SEEN_TTL_SECS);
        if self.seen.len() > SEEN_CAP {
            let mut ats: Vec<i64> = self.seen.values().map(|s| s.at).collect();
            ats.sort_unstable_by(|a, b| b.cmp(a));
            let cutoff = ats[SEEN_CAP - 1];
            self.seen.retain(|_, s| s.at >= cutoff);
            // Ties at the cutoff: drop arbitrary ones to honour the cap exactly.
            let extra = self.seen.len().saturating_sub(SEEN_CAP);
            let drop: Vec<String> = self.seen.iter().filter(|(_, s)| s.at == cutoff).take(extra).map(|(k, _)| k.clone()).collect();
            for k in drop {
                self.seen.remove(&k);
            }
        }
    }

    /// Drop stats older than a week, then all but the newest `STATS_CAP` scopes.
    pub fn prune_stats(&mut self, now: i64) {
        self.stats_cache.retain(|_, c| now - c.at <= STATS_TTL_SECS);
        while self.stats_cache.len() > STATS_CAP {
            let Some(oldest) = self.stats_cache.iter().min_by_key(|(_, c)| c.at).map(|(k, _)| k.clone()) else { break };
            self.stats_cache.remove(&oldest);
        }
    }

    pub fn push_recent(&mut self, repo: &str) {
        self.recent_repos.retain(|r| !r.eq_ignore_ascii_case(repo));
        self.recent_repos.insert(0, repo.to_string());
        self.recent_repos.truncate(10);
    }

    pub fn push_recent_search(&mut self, query: &str) {
        let q = query.trim();
        if q.is_empty() {
            return;
        }
        self.recent_searches.retain(|r| !r.eq_ignore_ascii_case(q));
        self.recent_searches.insert(0, q.to_string());
        self.recent_searches.truncate(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_searches_dedupe_and_truncate() {
        let mut s = State::default();
        for i in 0..12 {
            s.push_recent_search(&format!("q{i}"));
        }
        assert_eq!(s.recent_searches.len(), 10);
        assert_eq!(s.recent_searches[0], "q11");
        s.push_recent_search("  Q5 ");
        assert_eq!(s.recent_searches[0], "Q5");
        assert_eq!(s.recent_searches.iter().filter(|r| r.eq_ignore_ascii_case("q5")).count(), 1);
        s.push_recent_search("   ");
        assert_eq!(s.recent_searches[0], "Q5");
    }

    #[test]
    fn seen_round_trips_and_older_files_load() {
        let mut s = State::default();
        let snap = Snapshot { state: "OPEN".into(), comments: 7, head: "d1e3597".into(), ci: Some("FAILURE".into()), ..Snapshot::default() };
        s.seen.insert("PR_1".into(), Seen { snap: snap.clone(), at: 42, unread: true, ids: vec![7, 9], gh_unread: true });
        let json = serde_json::to_string(&s).unwrap();
        // The snapshot is flattened into the entry, as docs/change-signals.md shows.
        assert!(json.contains(r#""PR_1":{"updated_at":"#), "{json}");
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen["PR_1"], Seen { snap, at: 42, unread: true, ids: vec![7, 9], gh_unread: true });
        // A state file from before `seen`, and an entry missing newer fields.
        let old: State = serde_json::from_str(r#"{"density":"Cozy","recent_repos":[]}"#).unwrap();
        assert!(old.seen.is_empty());
        let partial: State = serde_json::from_str(r#"{"seen":{"PR_2":{"comments":3}}}"#).unwrap();
        assert_eq!((partial.seen["PR_2"].snap.comments, partial.seen["PR_2"].at, partial.seen["PR_2"].unread), (3, 0, false));
    }

    #[test]
    fn stats_cache_round_trips_and_prunes() {
        let mut s = State::default();
        let c = StatsCache {
            days: vec![("2026-10-19".into(), Some(4), None), ("2026-10-20".into(), Some(1), Some(2))],
            window: Some(Window { opened: 14, merged: 8, closed: 0 }),
            prev: Some(Window { opened: 11, merged: 8, closed: 2 }),
            at: 1_000_000,
        };
        s.stats_cache.insert("org:acme".into(), c.clone());
        s.stats_visible = Some(false);
        let back: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.stats_cache["org:acme"], c);
        assert_eq!(back.stats_visible, Some(false));
        // A state file from before the stats panel.
        let old: State = serde_json::from_str(r#"{"density":"Cozy","recent_repos":[],"unread_first":true}"#).unwrap();
        assert!(old.stats_cache.is_empty() && old.stats_visible.is_none());
        // A week old goes; beyond eight scopes the oldest go.
        let now = 100 * 86400;
        for i in 0..10 {
            s.stats_cache.insert(format!("k{i}"), StatsCache { at: now - i, ..StatsCache::default() });
        }
        s.stats_cache.insert("stale".into(), StatsCache { at: now - 8 * 86400, ..StatsCache::default() });
        s.prune_stats(now);
        assert_eq!(s.stats_cache.len(), STATS_CAP);
        assert!(s.stats_cache.contains_key("k0") && s.stats_cache.contains_key("k7") && !s.stats_cache.contains_key("k8"));
    }

    #[test]
    fn seen_cap_and_eviction() {
        let now = 100 * 86400;
        let mut s = State::default();
        for i in 0..2100 {
            s.seen.insert(format!("PR_{i}"), Seen { at: now - i as i64, ..Seen::default() });
        }
        s.seen.insert("stale".into(), Seen { at: now - 31 * 86400, ..Seen::default() });
        s.seen.insert("recent".into(), Seen { at: now - 29 * 86400, ..Seen::default() });
        s.prune_seen(now);
        assert!(!s.seen.contains_key("stale"));
        assert_eq!(s.seen.len(), SEEN_CAP);
        // The newest survive; the oldest beyond the cap (and the 29-day one) go.
        assert!(s.seen.contains_key("PR_0") && s.seen.contains_key("PR_1999"));
        assert!(!s.seen.contains_key("PR_2000") && !s.seen.contains_key("recent"));
    }
}
