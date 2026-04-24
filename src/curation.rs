// Curation state + wiki scrape snapshot.
//
// Two JSON files under `data/`:
//   * `wiki-snapshot.json` — what the scraper saw on the last `sync-wiki` run.
//     Written by sync-wiki, read by curate. Commits mean the snapshot a
//     curate session was based on is reproducible.
//   * `curation.json` — the user's allowlist. One entry per curated dungeon
//     with `reactions` and `drops` arrays. Absence of a dungeon = "not
//     curated yet" → sync-wiki syncs everything and curate will prompt for
//     selections next time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const DATA_DIR: &str = "data";
const CURATION_FILE: &str = "data/curation.json";
const SNAPSHOT_FILE: &str = "data/wiki-snapshot.json";

// ---------------------------------------------------------------------------
// Curation (user's allowlist).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Curation {
    #[serde(default)]
    pub dungeons: BTreeMap<String, DungeonCuration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DungeonCuration {
    #[serde(default)]
    pub reactions: Vec<String>,
    #[serde(default)]
    pub drops: Vec<String>,
}

impl Curation {
    pub fn load() -> Result<Self> {
        load_json(CURATION_FILE)
    }

    pub fn save(&self) -> Result<()> {
        save_json(CURATION_FILE, self)
    }

    pub fn is_curated(&self, dungeon: &str) -> bool {
        self.dungeons.contains_key(dungeon)
    }

    /// Gatekeeper for reaction writes. Returns true for dungeons not yet in
    /// `curation.json` (nothing to filter against), or when `name` is in the
    /// dungeon's allowlist.
    pub fn should_keep_reaction(&self, dungeon: &str, name: &str) -> bool {
        match self.dungeons.get(dungeon) {
            None => true,
            Some(c) => c.reactions.iter().any(|r| r == name),
        }
    }

    /// Gatekeeper for drop emoji writes. Same semantics as
    /// `should_keep_reaction` but keyed on a drop's logical name.
    pub fn should_keep_drop(&self, dungeon: &str, name: &str) -> bool {
        match self.dungeons.get(dungeon) {
            None => true,
            Some(c) => c.drops.iter().any(|d| d == name),
        }
    }
}

// ---------------------------------------------------------------------------
// Wiki scrape snapshot.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WikiSnapshot {
    pub generated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub dungeons: Vec<SnapshotDungeon>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDungeon {
    pub name: String,
    pub display_name: String,
    pub wiki_path: String,
    pub portal: Option<SnapshotEmoji>,
    pub key: Option<SnapshotEmoji>,
    #[serde(default)]
    pub drops: Vec<SnapshotEmoji>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEmoji {
    pub logical_name: String,
    pub img_url: String,
}

impl WikiSnapshot {
    pub fn load() -> Result<Self> {
        load_json(SNAPSHOT_FILE)
    }

    pub fn save(&self) -> Result<()> {
        save_json(SNAPSHOT_FILE, self)
    }
}

// ---------------------------------------------------------------------------
// JSON helpers.
// ---------------------------------------------------------------------------

/// Read and deserialize a JSON file. Returns `T::default()` if the file
/// doesn't exist (which is the common case for a fresh checkout or before the
/// first `sync-wiki` run).
fn load_json<T: for<'de> Deserialize<'de> + Default>(rel_path: &str) -> Result<T> {
    let path = PathBuf::from(rel_path);
    if !path.exists() {
        return Ok(T::default());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(v)
}

/// Write pretty-printed JSON, creating the `data/` directory if needed.
fn save_json<T: Serialize>(rel_path: &str, value: &T) -> Result<()> {
    let data_dir = Path::new(DATA_DIR);
    if !data_dir.exists() {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
    }
    let path = PathBuf::from(rel_path);
    let mut body = serde_json::to_string_pretty(value)
        .context("serializing JSON")?;
    body.push('\n');
    std::fs::write(&path, body)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
