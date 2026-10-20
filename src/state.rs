//! Machine-written state at `$XDG_STATE_HOME/gh-pr-manager/state.json`: recent repos and a repo list cache.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::github::RepoInfo;

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub density: Density,
    /// Most recently picked repo filters, newest first.
    pub recent_repos: Vec<String>,
    pub repos_cache: Option<ReposCache>,
    /// Inline images shown (the `I` key).
    pub images_enabled: bool,
}

impl Default for State {
    fn default() -> Self {
        Self { density: Density::default(), recent_repos: Vec::new(), repos_cache: None, images_enabled: true }
    }
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

/// Best effort; failing to persist state should never interrupt the user.
pub fn save(state: &State) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string(state) {
        let _ = std::fs::write(p, s);
    }
}

impl State {
    pub fn push_recent(&mut self, repo: &str) {
        self.recent_repos.retain(|r| !r.eq_ignore_ascii_case(repo));
        self.recent_repos.insert(0, repo.to_string());
        self.recent_repos.truncate(10);
    }
}
