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
    /// Auto-refresh interval in seconds; 0 disables.
    pub refresh_secs: u64,
    /// Name of the scope to select on startup.
    pub default_scope: Option<String>,
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
            refresh_secs: 120,
            default_scope: None,
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
