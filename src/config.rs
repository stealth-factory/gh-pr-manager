//! Optional user config at `$XDG_CONFIG_HOME/gh-pr-manager/config.toml`.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Extra handles offered by @mention autocomplete (bots are not in mentionableUsers).
    pub bots: Vec<String>,
    /// Extra scopes appended to the built-in ones.
    pub scopes: Vec<ScopeConfig>,
    /// PR list auto-refresh interval in seconds; 0 disables.
    pub refresh_secs: u64,
    /// Background refresh interval for the open PR's details; 0 disables.
    pub detail_refresh_secs: u64,
    /// Name of the scope to select on startup.
    pub default_scope: Option<String>,
    /// `owner/name` repository filter to apply on startup.
    pub default_repo: Option<String>,
    /// Inline images: "auto", "kitty", "sixel", "iterm2", "halfblocks" or "off".
    /// `GH_PR_MANAGER_IMAGES` overrides it.
    pub images: String,
    /// Most terminal rows one inline image may take.
    pub image_max_rows: u16,
    /// The `/` search palette.
    pub search: SearchConfig,
    /// Bell / desktop notifications for interrupt-worthy changes (off by default).
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Which changes notify: review_requested, approved, changes_requested, ci_failed, merged,
    /// closed, conflicts. `[]` turns notifications off.
    pub events: Vec<String>,
    /// Only PRs you authored or were asked to review.
    pub mine_only: bool,
    /// Ring the terminal bell (BEL).
    pub bell: bool,
    /// Desktop notification escape: "auto", "osc9", "osc777", "osc99" or "off".
    pub desktop: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            events: ["review_requested", "approved", "changes_requested", "ci_failed", "merged", "closed", "conflicts"].map(String::from).to_vec(),
            mine_only: true,
            bell: false,
            desktop: "off".into(),
        }
    }
}

impl NotifyConfig {
    /// Anything would be emitted at all.
    pub fn enabled(&self) -> bool {
        !self.events.is_empty() && (self.bell || !self.desktop.trim().eq_ignore_ascii_case("off"))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Search GitHub while typing; `false` searches only on Ctrl-g (or Enter with no local hits).
    pub live: bool,
    /// Fetch PR descriptions in the background so search can match them.
    pub bodies: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self { live: true, bodies: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScopeConfig {
    pub name: String,
    /// GitHub search qualifiers, e.g. `repo:owner/name` or `org:acme label:urgent`.
    pub query: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bots: ["claude", "copilot", "coderabbitai", "codex", "cursor", "gemini-code-assist", "devin-ai-integration", "dependabot"]
                .map(String::from)
                .to_vec(),
            scopes: Vec::new(),
            refresh_secs: 60,
            detail_refresh_secs: 20,
            default_scope: None,
            default_repo: None,
            images: "auto".into(),
            image_max_rows: 20,
            search: SearchConfig::default(),
            notify: NotifyConfig::default(),
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("gh-pr-manager").join("config.toml"))
}

pub fn load() -> anyhow::Result<Config> {
    let Some(path) = config_path() else { return Ok(Config::default()) };
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}
