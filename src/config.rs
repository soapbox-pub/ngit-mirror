use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// npub or hex pubkey whose repo announcements are mirrored.
    pub owner: String,
    /// GitHub organization the mirrors live under.
    pub org: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Bootstrap relays. Relays listed in the announcements are added on top.
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
    /// Delete GitHub branches and tags that are not in the nostr state.
    #[serde(default)]
    pub prune: bool,
    /// Repo identifiers to skip.
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: u64,
    /// Maximum number of repos syncing at once.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

fn default_relays() -> Vec<String> {
    [
        "wss://relay.ngit.dev",
        "wss://git.shakespeare.diy",
        "wss://relay.ditto.pub",
        "wss://relay.damus.io",
    ]
    .map(String::from)
    .to_vec()
}

fn default_reconcile_interval() -> u64 {
    3600
}

fn default_concurrency() -> usize {
    4
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}
