//! Dungeon template loader.
//!
//! Two JSON files under `data/` are the source of truth for global
//! dungeon templates (the `guild_id IS NULL` rows in `dungeon_templates`):
//!
//! - **`data/wiki_dump.json`** — machine-written by `starship sync-wiki`.
//!   One entry per dungeon found on the RealmEye "Regular Dungeons" page.
//!   Committed so fresh clones can seed without running sync-wiki first.
//! - **`data/dungeon_overrides.json`** — hand-authored, committed. Two
//!   override patterns:
//!   1. **Additive** (`extends: "<wiki_name>"`): clone a wiki dungeon and
//!      tweak scalar fields (display_name, requires_vc, …). Useful for
//!      variants the wiki doesn't know about — e.g. Fullskip Void as a
//!      VC-required clone of The Void.
//!   2. **Patch** (no `extends`): target an existing wiki dungeon and
//!      override scalar fields and/or replace the default
//!      `(interest + key)` reactions with a hand-authored list.
//!
//! On every bot boot, `load_and_seed` merges the two files into a set of
//! *effective* templates and upserts them into Postgres.
//!
//! **Guild-specific** rows (created via `/dungeon create`, `/pingroles
//! set`, etc.) are untouched — `list_for_guild` already prefers them over
//! globals via `DISTINCT ON (name)`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::db;

const DATA_DIR: &str = "data";
const DUMP_FILE: &str = "data/wiki_dump.json";
const OVERRIDES_FILE: &str = "data/dungeon_overrides.json";

// ---------------------------------------------------------------------------
// Wiki dump (machine-written by sync-wiki).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WikiDump {
    pub generated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub dungeons: Vec<WikiDungeon>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiDungeon {
    pub name: String,
    pub display_name: String,
    pub wiki_path: String,
    pub portal: Option<WikiEmoji>,
    pub key: Option<WikiEmoji>,
    #[serde(default)]
    pub drops: Vec<WikiEmoji>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiEmoji {
    pub logical_name: String,
    pub img_url: String,
}

impl WikiDump {
    pub fn load() -> Result<Self> {
        load_json(DUMP_FILE)
    }

    pub fn save(&self) -> Result<()> {
        save_json(DUMP_FILE, self)
    }
}

// ---------------------------------------------------------------------------
// Overrides (hand-authored).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overrides(pub BTreeMap<String, DungeonOverride>);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DungeonOverride {
    /// Name of a wiki-dump dungeon to clone. When set, the override is
    /// additive — it creates a new effective dungeon named by this
    /// override's map key, starting from the extended source and applying
    /// overrides on top. `extends` must target a wiki-dump entry (no
    /// chains through other overrides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_vc: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub showcase_emoji: Option<Vec<String>>,
    /// When set, replaces the default `(interest + key)` reactions for
    /// this dungeon. The bot does not merge — a single `reactions` entry
    /// is treated as the complete desired set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<Vec<OverrideReaction>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideReaction {
    pub name: String,
    pub display_name: String,
    pub emoji: String,
    #[serde(default = "default_num_required")]
    pub num_required: i32,
    #[serde(default)]
    pub requires_confirmation: bool,
    #[serde(default)]
    pub sort_order: i32,
}

fn default_num_required() -> i32 {
    1
}

impl Overrides {
    pub fn load() -> Result<Self> {
        load_json(OVERRIDES_FILE)
    }
}

// ---------------------------------------------------------------------------
// Effective template (merged dump + override).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Effective {
    name: String,
    display_name: String,
    emoji: Option<String>,
    color: Option<i32>,
    message_title: Option<String>,
    message_description: Option<String>,
    requires_vc: bool,
    showcase_emoji: Vec<String>,
    thumbnail_url: Option<String>,
    reactions: Vec<OverrideReaction>,
}

impl Effective {
    /// Build from a wiki-dump dungeon with no override applied.
    /// `showcase_emoji` defaults to every drop the scraper saw; the
    /// per-guild bag-tier threshold decides which ones actually render.
    /// Reactions default to `interest` + `key` (if the dungeon has a key).
    fn from_dump(d: &WikiDungeon) -> Self {
        let showcase_emoji = d.drops.iter().map(|e| e.logical_name.clone()).collect();
        let reactions = default_reactions(d.key.as_ref());
        Effective {
            name: d.name.clone(),
            display_name: d.display_name.clone(),
            emoji: d.portal.as_ref().map(|p| p.logical_name.clone()),
            color: None,
            message_title: None,
            message_description: None,
            requires_vc: false,
            showcase_emoji,
            thumbnail_url: d.portal.as_ref().map(|p| p.img_url.clone()),
            reactions,
        }
    }

    /// Apply scalar + reaction overrides on top of an already-resolved
    /// effective template. Scalars only replace when `Some`; `reactions`
    /// only replaces when present.
    fn apply(&mut self, o: &DungeonOverride) {
        if let Some(v) = &o.display_name {
            self.display_name = v.clone();
        }
        if let Some(v) = &o.emoji {
            self.emoji = Some(v.clone());
        }
        if let Some(v) = o.color {
            self.color = Some(v);
        }
        if let Some(v) = &o.message_title {
            self.message_title = Some(v.clone());
        }
        if let Some(v) = &o.message_description {
            self.message_description = Some(v.clone());
        }
        if let Some(v) = o.requires_vc {
            self.requires_vc = v;
        }
        if let Some(v) = &o.showcase_emoji {
            self.showcase_emoji = v.clone();
        }
        if let Some(v) = &o.reactions {
            self.reactions = v.clone();
        }
    }
}

fn default_reactions(key: Option<&WikiEmoji>) -> Vec<OverrideReaction> {
    let mut out = vec![OverrideReaction {
        name: "interest".into(),
        display_name: "Joining".into(),
        emoji: "✅".into(),
        num_required: 1,
        requires_confirmation: false,
        sort_order: 0,
    }];
    if let Some(k) = key {
        out.push(OverrideReaction {
            name: "key".into(),
            display_name: "Key".into(),
            emoji: "🔑".into(),
            num_required: 1,
            requires_confirmation: false,
            sort_order: 1,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Seeder entry point.
// ---------------------------------------------------------------------------

/// Load the wiki dump and overrides from `data/`, merge them, and upsert
/// global dungeon templates + their reactions into Postgres.
///
/// Missing files are not an error. An absent dump means no seeding happens
/// this boot (the bot still runs — `/headcount` just has nothing to
/// offer); an absent overrides file is the normal clean case.
pub async fn load_and_seed(pool: &PgPool) -> Result<()> {
    let dump = WikiDump::load().context("loading wiki dump")?;
    let overrides = Overrides::load().context("loading overrides")?;

    if dump.dungeons.is_empty() && overrides.0.is_empty() {
        warn!(
            "no wiki dump at {DUMP_FILE} and no overrides at {OVERRIDES_FILE} — \
             skipping dungeon seed. Run `starship sync-wiki` to populate."
        );
        return Ok(());
    }

    let effective = merge(&dump, &overrides)?;
    for eff in &effective {
        seed_one(pool, eff).await?;
    }

    // Drop globals that the current effective set no longer mentions.
    // With raid lifecycle rows now deleted on terminal transitions, the
    // only thing that can still pin a stale global is a *live* headcount
    // or run — in which case we log and back off until the next boot.
    let keep: HashSet<&str> = effective.iter().map(|e| e.name.as_str()).collect();
    for name in db::dungeon::list_global_names(pool).await? {
        if keep.contains(name.as_str()) {
            continue;
        }
        match db::dungeon::delete_global_by_name(pool, &name).await {
            Ok(true) => info!(dungeon = %name, "removed stale global dungeon template"),
            Ok(false) => {}
            Err(e) => warn!(
                dungeon = %name,
                error = %e,
                "could not delete stale global dungeon template (likely a live raid still references it); will retry on next boot"
            ),
        }
    }

    info!(
        dungeons = effective.len(),
        overrides = overrides.0.len(),
        "seeded dungeon templates"
    );
    Ok(())
}

/// Pure merge — no I/O. Private to the module (tests use `use super::*`).
fn merge(dump: &WikiDump, overrides: &Overrides) -> Result<Vec<Effective>> {
    // Index wiki dump by name for quick lookup.
    let dump_by_name: BTreeMap<&str, &WikiDungeon> =
        dump.dungeons.iter().map(|d| (d.name.as_str(), d)).collect();

    // Validate every `extends` target before mutating anything.
    for (name, ovr) in &overrides.0 {
        if let Some(target) = &ovr.extends {
            if !dump_by_name.contains_key(target.as_str()) {
                bail!(
                    "override `{name}` extends `{target}`, which is not in the wiki dump"
                );
            }
        }
    }

    let mut by_name: BTreeMap<String, Effective> = BTreeMap::new();
    let mut seen_override_names: HashSet<String> = HashSet::new();

    // Pass 1: every wiki dungeon, with any matching patch override
    // (no-extends) applied.
    for dungeon in &dump.dungeons {
        let mut eff = Effective::from_dump(dungeon);
        if let Some(ovr) = overrides.0.get(&dungeon.name) {
            if ovr.extends.is_none() {
                eff.apply(ovr);
                seen_override_names.insert(dungeon.name.clone());
                info!(
                    dungeon = %dungeon.name,
                    reactions = eff.reactions.len(),
                    "applied patch override"
                );
            }
        }
        by_name.insert(eff.name.clone(), eff);
    }

    // Pass 2: every `extends` override spawns a new effective dungeon
    // derived from its source.
    for (name, ovr) in &overrides.0 {
        let Some(target) = &ovr.extends else { continue };
        let source = by_name
            .get(target.as_str())
            .cloned()
            .expect("extends target validated above");
        let mut eff = source;
        eff.name = name.clone();
        // The extended dungeon starts with the source's display_name; the
        // override is free to rename it (and usually should).
        eff.apply(ovr);
        by_name.insert(name.clone(), eff);
        seen_override_names.insert(name.clone());
    }

    // Warn for overrides that matched nothing — usually a typo in the
    // JSON key (e.g. the user wrote `oryx_sanctuary` instead of
    // `oryxs_sanctuary`). The key is silently dead until pointed out.
    for name in overrides.0.keys() {
        if !seen_override_names.contains(name) {
            warn!(
                "override `{name}` matched no wiki dungeon and has no `extends` — ignored"
            );
        }
    }

    Ok(by_name.into_values().collect())
}

async fn seed_one(pool: &PgPool, eff: &Effective) -> Result<()> {
    let id = db::dungeon::upsert_global_template(
        pool,
        &eff.name,
        &eff.display_name,
        eff.emoji.as_deref(),
        eff.color,
        eff.message_title.as_deref(),
        eff.message_description.as_deref(),
        eff.requires_vc,
        &eff.showcase_emoji,
        eff.thumbnail_url.as_deref(),
    )
    .await?;

    for r in &eff.reactions {
        db::dungeon::upsert_reaction(
            pool,
            id,
            &r.name,
            &r.display_name,
            &r.emoji,
            r.num_required,
            r.requires_confirmation,
            r.sort_order,
        )
        .await?;
    }

    let keep: Vec<String> = eff.reactions.iter().map(|r| r.name.clone()).collect();
    db::dungeon::delete_reactions_not_in(pool, id, &keep).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// JSON helpers.
// ---------------------------------------------------------------------------

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

fn save_json<T: Serialize>(rel_path: &str, value: &T) -> Result<()> {
    let data_dir = Path::new(DATA_DIR);
    if !data_dir.exists() {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
    }
    let path = PathBuf::from(rel_path);
    let mut body = serde_json::to_string_pretty(value).context("serializing JSON")?;
    body.push('\n');
    std::fs::write(&path, body)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_with(dungeons: Vec<WikiDungeon>) -> WikiDump {
        WikiDump {
            generated_at: None,
            dungeons,
        }
    }

    fn wiki_void() -> WikiDungeon {
        WikiDungeon {
            name: "the_void".into(),
            display_name: "The Void".into(),
            wiki_path: "/wiki/the-void".into(),
            portal: Some(WikiEmoji {
                logical_name: "portal_void".into(),
                img_url: "x".into(),
            }),
            key: Some(WikiEmoji {
                logical_name: "lost_halls_key".into(),
                img_url: "x".into(),
            }),
            drops: vec![],
        }
    }

    #[test]
    fn extends_clones_source_and_applies_overrides() {
        let dump = dump_with(vec![wiki_void()]);
        let mut ovr = Overrides::default();
        ovr.0.insert(
            "fullskip_void".into(),
            DungeonOverride {
                extends: Some("the_void".into()),
                display_name: Some("Fullskip Void".into()),
                requires_vc: Some(true),
                ..Default::default()
            },
        );

        let effective = merge(&dump, &ovr).expect("merge ok");
        let fullskip = effective
            .iter()
            .find(|e| e.name == "fullskip_void")
            .expect("fullskip exists");
        assert_eq!(fullskip.display_name, "Fullskip Void");
        assert!(fullskip.requires_vc);
        // Inherited from the_void.
        assert_eq!(fullskip.emoji.as_deref(), Some("portal_void"));
        assert_eq!(fullskip.reactions.len(), 2); // interest + key
        // Source still present unchanged.
        let source = effective
            .iter()
            .find(|e| e.name == "the_void")
            .expect("the_void exists");
        assert!(!source.requires_vc);
        assert_eq!(source.display_name, "The Void");
    }

    #[test]
    fn extends_unknown_target_errors() {
        let dump = dump_with(vec![wiki_void()]);
        let mut ovr = Overrides::default();
        ovr.0.insert(
            "fullskip_void".into(),
            DungeonOverride {
                extends: Some("not_a_dungeon".into()),
                ..Default::default()
            },
        );

        let err = merge(&dump, &ovr).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not_a_dungeon"), "got: {msg}");
    }

    #[test]
    fn patch_override_replaces_scalars_and_reactions() {
        let dump = dump_with(vec![wiki_void()]);
        let mut ovr = Overrides::default();
        ovr.0.insert(
            "the_void".into(),
            DungeonOverride {
                requires_vc: Some(true),
                reactions: Some(vec![OverrideReaction {
                    name: "interest".into(),
                    display_name: "Reacts".into(),
                    emoji: "✅".into(),
                    num_required: 1,
                    requires_confirmation: false,
                    sort_order: 0,
                }]),
                ..Default::default()
            },
        );

        let effective = merge(&dump, &ovr).expect("merge ok");
        let void = effective
            .iter()
            .find(|e| e.name == "the_void")
            .expect("the_void exists");
        assert!(void.requires_vc);
        assert_eq!(void.reactions.len(), 1);
        assert_eq!(void.reactions[0].name, "interest");
    }
}
