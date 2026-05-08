// RealmEye wiki scraper: populates bot_emoji and dungeon_templates from
// https://www.realmeye.com/wiki/dungeons.
//
// CSS selectors and page structure may need adjustment if RealmEye redesigns
// their wiki. All selector constants are grouped at the top for easy updating.
//
// Run as: starship sync-wiki

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use image::imageops::FilterType;
use once_cell::sync::Lazy;
use reqwest::Client;
use scraper::{Html, Selector};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::Config;
use crate::db;
use crate::db::emoji::ApplicationEmojiClient;
use crate::templates::{WikiDump, WikiDungeon, WikiEmoji};
use crate::util::text::slug_from_display;

// ---------------------------------------------------------------------------
// Selector constants — update these if RealmEye changes their HTML.
//
// Structure as of 2026-04: the dungeon index lives at /wiki/dungeons and
// renders several sibling <table class="table table-striped"> blocks (one per
// section: regular dungeons, other dungeons, etc.). Each dungeon row has 5
// <td> cells: name / portal / key / drops-from / difficulty. Multi-tier
// dungeons use rowspan="3" on the name/portal/key/difficulty cells with the
// drops column split across up to three continuation <tr> rows that have
// 0 or 1 <td>. We identify a dungeon by a row whose first cell contains
// an <a href="/wiki/..."> link; continuation rows are ignored here because
// we scrape drops from each dungeon's dedicated page instead.
//
// Per-dungeon pages have an <h2 id="drops">Drops of Interest</h2> section
// followed by a single <table class="table table-striped"> whose data rows
// list one item per row (col 0 = item img, col 1 = source enemies).
// ---------------------------------------------------------------------------

// Selectors are parsed once at first use. The `expect` calls fire only if
// a hand-written literal here is invalid CSS — i.e. a static bug, not a
// runtime condition. Lifting them out of hot loops saves repeated parsing
// (`Selector::parse` walks a state machine on every call).

/// Rows in any dungeon index table.
static SEL_INDEX_ROWS: Lazy<Selector> =
    Lazy::new(|| Selector::parse("table.table-striped tr").expect("static selector"));
/// Name link (dungeon anchor) in a dungeon row — first cell.
static SEL_DUNGEON_NAME_CELL: Lazy<Selector> =
    Lazy::new(|| Selector::parse("td:first-child a").expect("static selector"));
/// Portal image — second cell.
static SEL_PORTAL_IMG: Lazy<Selector> =
    Lazy::new(|| Selector::parse("td:nth-child(2) img").expect("static selector"));
/// Key image — third cell (may be absent).
static SEL_KEY_IMG: Lazy<Selector> =
    Lazy::new(|| Selector::parse("td:nth-child(3) img").expect("static selector"));

/// Rows in the "Drops of Interest" table on a dungeon page.
static SEL_DROP_ROWS: Lazy<Selector> =
    Lazy::new(|| Selector::parse("table.table-striped tr").expect("static selector"));
/// Drop item image — first cell of a drop row.
static SEL_DROP_IMG: Lazy<Selector> =
    Lazy::new(|| Selector::parse("td:first-child img").expect("static selector"));
/// Internal-wiki anchor in the first cell of a drop row.
static SEL_DROP_ROW_ANCHOR: Lazy<Selector> =
    Lazy::new(|| Selector::parse(r#"td:first-child a[href^="/wiki/"]"#).expect("static selector"));
/// Any `<img>` (used inside an anchor or any table row).
static SEL_ANY_IMG: Lazy<Selector> = Lazy::new(|| Selector::parse("img").expect("static selector"));
/// Any table row (used by the bag-tier sweep).
static SEL_ANY_TABLE_ROW: Lazy<Selector> =
    Lazy::new(|| Selector::parse("table tr").expect("static selector"));

const REALMEYE_BASE: &str = "https://www.realmeye.com";
const DUNGEONS_PATH: &str = "/wiki/dungeons";
const CLASSES_PATH: &str = "/wiki/classes";
const STATUS_EFFECTS_PATH: &str = "/wiki/status-effects";

// Discord emoji constraints.
const EMOJI_MAX_SIDE: u32 = 128;

// ---------------------------------------------------------------------------
// Catalogue (classes / status effects) scraping.
//
// These pages don't contribute to wiki_dump.json — they only populate
// `bot_emoji` so the operator can reference logical names like
// `class_wizard` or `status_paralyzed` from `data/dungeon_overrides.json`.
// They are never auto-attached to a dungeon's reactions.
// ---------------------------------------------------------------------------

/// Every `<a>` on a page, used by both catalogue scrapers. Cheaper than
/// re-parsing the same selector twice and lets the parsers share filtering
/// logic.
static SEL_ANY_ANCHOR: Lazy<Selector> =
    Lazy::new(|| Selector::parse("a").expect("static selector"));

// ---------------------------------------------------------------------------
// Dungeon-level filtering. Applied after `scrape_dungeon_list` pulls the
// Regular Dungeons section, before any per-dungeon HTTP traffic. Slugs here
// are post-`slug_from_display` (apostrophes already stripped).
// ---------------------------------------------------------------------------

const EXCLUDED_SLUGS: &[&str] = &[
    "court_of_oryx",
    "oryxs_castle",
    "oryxs_chamber",
    "wine_cellar",
];

/// h2 section headings on /wiki/dungeons whose tables we skip. Matched as a
/// case-insensitive substring against the lowercased inner text of each
/// `<h2>` on the page.
///
/// We deliberately KEEP the "Oryx's Castle" section — it's where RealmEye
/// lists Oryx's Sanctuary (O3) alongside Court of Oryx, Castle, Chamber,
/// and Wine Cellar. The per-slug EXCLUDED_SLUGS denylist drops the four
/// dungeons we don't want while leaving O3 intact.
const EXCLUDED_SECTIONS: &[&str] = &[
    "contents",
    "special event dungeons",
    "other dungeons",
    "history",
];

/// Case-insensitive text matches (section heading name contains) for the
/// drops-of-interest section on a dungeon page. The wiki isn't consistent —
/// some dungeons use "Drops of Interest" while others use "Notable Drops" or
/// plain "Drops".
const DROP_SECTION_HEADINGS: &[&str] = &["drops of interest", "notable drops", "drops"];

/// Ordered list of bag tier names (matches the bag_tiers lookup table). Used
/// to classify items from their "Loot Bag" row on RealmEye. When an item
/// lists multiple bag tiers, we keep the rarest (last match wins). `shiny`
/// is appended for completeness but never appears in a "Loot Bag" row —
/// the scraper tags the shiny *variant* of each item with this tier
/// separately (see `scrape_item_page` shiny extraction).
const BAG_TIERS_ORDERED: &[&str] = &[
    "brown", "pink", "purple", "cyan", "blue", "orange", "red", "white", "shiny",
];

// ---------------------------------------------------------------------------
// Data types for intermediate scraping results.
// ---------------------------------------------------------------------------

struct DungeonEntry {
    name: String,
    display_name: String,
    wiki_path: String,
    portal_img_url: Option<String>,
    key_img_url: Option<String>,
}

struct DungeonDetails {
    drop_items: Vec<DropItem>,
}

struct DropItem {
    logical_name: String,
    img_url: String,
    /// RealmEye link to the item's own wiki page. Source for the bag-tier
    /// classification (parsed from the "Loot Bag" table row).
    item_wiki_path: Option<String>,
}

/// One entry from a catalogue page (class or status effect). The logical
/// name is the prefixed slug we store in `bot_emoji` (`class_wizard`,
/// `status_paralyzed`, …); display name is the human-readable label
/// pulled from the img alt; img URL is the absolute image URL ready for
/// download.
struct CatalogueEmoji {
    logical_name: String,
    display_name: String,
    img_url: String,
}

/// Result of fetching an item's wiki page for bag-tier classification.
struct ItemBagInfo {
    /// Bag tier name (matches bag_tiers.name). `None` if the "Loot Bag" row
    /// wasn't found or no known colour word was present.
    tier: Option<String>,
    /// Absolute URL of the bag icon image inside the "Loot Bag" row, if any.
    /// Used once per tier to upload the `bag_<tier>` ui emoji.
    bag_image_url: Option<String>,
    /// Absolute URL of the item's shiny sprite, if the page contains one.
    /// Shinies are rarer variants that share the item's wiki page — the
    /// caller uploads them as `<logical>_shiny` with bag_tier='shiny'.
    shiny_image_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Public surface for callers (CLI + Discord command).
// ---------------------------------------------------------------------------

/// Caller-supplied options for a sync run. Both `dry_run` and `purge` are
/// expected to be pre-confirmed: `run_core` does not prompt the user.
/// The CLI entry [`run`] handles the stdin Y/N prompt; the Discord command
/// requires the operator to type the literal string `PURGE` as a slash
/// command parameter.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub purge: bool,
}

/// Aggregate counters returned from a sync run. Tracks both real and
/// dry-run paths so callers can render a single unified summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncSummary {
    /// Number of dungeons returned by `scrape_dungeon_list`.
    pub dungeons_scraped: usize,
    /// Emojis newly POSTed to Discord this run.
    pub uploads_new: usize,
    /// Emojis already present on Discord that we reused.
    pub uploads_reused: usize,
    /// Dry-run only: emojis we would have uploaded.
    pub uploads_skipped_dry: usize,
    /// Dry-run only: bot_emoji rows we would have upserted.
    pub upserts_skipped_dry: usize,
    /// Count of warnings emitted during the run (failed scrapes, failed
    /// uploads, etc.). Each one is logged via `tracing::warn!` and also
    /// pushed through the progress channel as a [`ProgressEvent::Warning`].
    pub warnings: usize,
}

/// Progress events emitted by [`run_core`] while a sync is in flight.
/// Consumers (e.g. the Discord ephemeral renderer) keep a rolling state
/// machine and re-render on each event.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A new top-level phase started (e.g. "scraping dungeon list").
    Phase(String),
    /// Beginning work on a specific dungeon. `index` is 1-based.
    DungeonStart {
        name: String,
        index: usize,
        total: usize,
    },
    /// A non-fatal warning was emitted. `message` is the human-readable
    /// log line minus the `tracing` prefix.
    Warning(String),
    /// One emoji was freshly uploaded to Discord.
    UploadNew,
    /// One emoji was already on Discord; we reused the existing ID.
    UploadReused,
    /// Dry-run only: one upload was skipped.
    UploadSkipped,
}

/// Wrapper around an optional [`mpsc::Sender`] so call sites can emit
/// progress events without `if let Some(tx) = …` plumbing. `try_send` is
/// non-blocking: if the consumer falls behind we drop events rather than
/// stalling the scrape (the final summary is the source of truth, the
/// stream is best-effort).
#[derive(Clone, Default)]
pub struct Progress(Option<mpsc::Sender<ProgressEvent>>);

impl Progress {
    /// No-op sink for the CLI path where nobody is listening.
    pub fn noop() -> Self {
        Self(None)
    }

    /// Build a `(Progress, Receiver)` pair. The Discord command holds the
    /// receiver and drives an ephemeral message edit loop from it.
    pub fn channel() -> (Self, mpsc::Receiver<ProgressEvent>) {
        let (tx, rx) = mpsc::channel(64);
        (Self(Some(tx)), rx)
    }

    fn emit(&self, event: ProgressEvent) {
        if let Some(tx) = &self.0 {
            // Non-blocking: drop on full / closed. Final summary still
            // reaches the caller through `run_core`'s return value.
            let _ = tx.try_send(event);
        }
    }
}

enum UploadOutcome {
    /// Emoji already present on Discord; ID carried forward to the DB.
    Existing(u64, bool),
    /// Emoji freshly uploaded; ID returned by the Discord API.
    Uploaded(u64, bool),
    /// Dry-run: would upload, no network write performed.
    WouldUpload,
}

/// Helper used at every fallible side-effect inside `run_core` that we
/// want to surface to a watching operator: log via `tracing::warn!`, bump
/// the warning counter, and push a `Warning` event onto the progress
/// channel. Centralising the three steps keeps every warn site honest.
fn track_warn(progress: &Progress, summary: &mut SyncSummary, msg: String) {
    warn!("{msg}");
    summary.warnings += 1;
    progress.emit(ProgressEvent::Warning(msg));
}

/// Core sync pipeline. Caller is responsible for loading config, opening
/// (and migrating) the pool, and confirming any destructive flags. Returns
/// a [`SyncSummary`] so the Discord command can render counts back to the
/// operator without rescanning the run.
///
/// Pass `Progress::noop()` when nobody is listening (CLI path); pass a
/// channel-backed `Progress` to receive a stream of [`ProgressEvent`]s for
/// a live status display.
pub async fn run_core(
    config: &Config,
    pool: Option<&PgPool>,
    opts: SyncOptions,
    progress: &Progress,
) -> Result<SyncSummary> {
    let SyncOptions { dry_run, purge } = opts;

    if dry_run {
        info!("dry-run mode: no Discord POSTs and no DB writes will be performed");
    }

    let client = Client::builder()
        .user_agent(&config.realmeye_user_agent)
        .build()?;

    let emoji_api = ApplicationEmojiClient::new(
        client.clone(),
        &config.discord_token,
        config.discord_application_id,
    );

    // One-shot destructive reset: wipe every application emoji the bot owns
    // plus every bot_emoji row. Used when renaming a batch of logical names
    // (e.g. apostrophe slug fix: oryx_s_sanctuary -> oryxs_sanctuary) where
    // the old names would otherwise linger on Discord forever.
    if purge {
        if dry_run {
            info!("[dry-run] --purge would delete all application emojis and TRUNCATE bot_emoji");
        } else {
            progress.emit(ProgressEvent::Phase("purging existing emojis".into()));
            purge_all_unprompted(&emoji_api, pool).await?;
        }
    }

    // Emojis already registered on this application. We mutate this map as
    // new uploads succeed so the same logical emoji isn't uploaded twice in
    // a single run — e.g. `potion_of_wisdom` appears in many dungeons'
    // drops-of-interest tables and Discord 400s on duplicate names.
    progress.emit(ProgressEvent::Phase(
        "listing existing application emojis".into(),
    ));
    info!("fetching existing application emojis…");
    let mut existing = emoji_api.list().await.unwrap_or_else(|e| {
        warn!("could not list application emojis: {e:#} — will attempt all uploads");
        std::collections::HashMap::new()
    });
    info!("{} emojis already registered", existing.len());

    // Per-run cache of item-page fetches. Keyed on the item's drops-table
    // img URL so duplicate items across dungeons (e.g. Potion of Wisdom
    // drops from half the dungeon list) are fetched once.
    let mut item_page_cache: HashMap<String, ItemBagInfo> = HashMap::new();
    // Bag tiers we've already uploaded an icon for this run.
    let mut uploaded_bag_tiers: HashSet<String> = HashSet::new();

    progress.emit(ProgressEvent::Phase(
        "scraping dungeon list from RealmEye".into(),
    ));
    info!("scraping dungeon list from RealmEye wiki…");
    let dungeons = scrape_dungeon_list(&client).await?;
    info!("found {} dungeons", dungeons.len());

    let mut summary = SyncSummary {
        dungeons_scraped: dungeons.len(),
        ..Default::default()
    };
    let mut dump = WikiDump {
        generated_at: Some(chrono::Utc::now()),
        dungeons: Vec::with_capacity(dungeons.len()),
    };

    let total_dungeons = dungeons.len();
    for (idx, dungeon) in dungeons.iter().enumerate() {
        progress.emit(ProgressEvent::DungeonStart {
            name: dungeon.display_name.clone(),
            index: idx + 1,
            total: total_dungeons,
        });
        info!("processing: {}", dungeon.display_name);

        let mut dump_dungeon = WikiDungeon {
            name: dungeon.name.clone(),
            display_name: dungeon.display_name.clone(),
            wiki_path: dungeon.wiki_path.clone(),
            portal: None,
            key: None,
            drops: Vec::new(),
        };

        // Upload portal emoji.
        let portal_emoji_name = format!("portal_{}", dungeon.name);
        let portal_discord_name = discord_name(&portal_emoji_name);
        if let Some(url) = &dungeon.portal_img_url {
            dump_dungeon.portal = Some(WikiEmoji {
                logical_name: portal_emoji_name.clone(),
                img_url: url.clone(),
            });
            match upload_if_new(
                &client,
                &emoji_api,
                &mut existing,
                &portal_discord_name,
                url,
                dry_run,
                &mut summary,
                progress,
            )
            .await
            {
                Ok(UploadOutcome::Existing(emoji_id, animated))
                | Ok(UploadOutcome::Uploaded(emoji_id, animated)) => {
                    if let Some(pool) = pool {
                        db::emoji::upsert(
                            pool,
                            &portal_emoji_name,
                            emoji_id as i64,
                            &portal_discord_name,
                            animated,
                            None,
                            Some("portal"),
                            Some(url),
                            None,
                        )
                        .await?;
                    }
                }
                Ok(UploadOutcome::WouldUpload) => {
                    info!(
                        "[dry-run] would upsert bot_emoji name={portal_emoji_name} category=portal"
                    );
                    summary.upserts_skipped_dry += 1;
                }
                Err(e) => track_warn(
                    progress,
                    &mut summary,
                    format!("portal emoji {portal_discord_name}: {e:#}"),
                ),
            }
        }

        // Upload key emoji.
        if let Some(url) = &dungeon.key_img_url {
            let name = format!("key_{}", dungeon.name);
            let dname = discord_name(&name);
            dump_dungeon.key = Some(WikiEmoji {
                logical_name: name.clone(),
                img_url: url.clone(),
            });
            match upload_if_new(
                &client,
                &emoji_api,
                &mut existing,
                &dname,
                url,
                dry_run,
                &mut summary,
                progress,
            )
            .await
            {
                Ok(UploadOutcome::Existing(emoji_id, animated))
                | Ok(UploadOutcome::Uploaded(emoji_id, animated)) => {
                    if let Some(pool) = pool {
                        db::emoji::upsert(
                            pool,
                            &name,
                            emoji_id as i64,
                            &dname,
                            animated,
                            None,
                            Some("key"),
                            Some(url),
                            None,
                        )
                        .await?;
                    }
                }
                Ok(UploadOutcome::WouldUpload) => {
                    info!("[dry-run] would upsert bot_emoji name={name} category=key");
                    summary.upserts_skipped_dry += 1;
                }
                Err(e) => track_warn(progress, &mut summary, format!("key emoji {dname}: {e:#}")),
            }
        }

        // Scrape the dungeon page for drops.
        let details = match scrape_dungeon_page(&client, &dungeon.wiki_path).await {
            Ok(d) => d,
            Err(e) => {
                track_warn(
                    progress,
                    &mut summary,
                    format!("failed to scrape {}: {e:#}", dungeon.display_name),
                );
                DungeonDetails { drop_items: vec![] }
            }
        };

        // Upload drop item emojis. Shiny variants uploaded below are
        // pushed into `dump_dungeon.drops` so the seeder's showcase_emoji
        // derivation picks them up alongside their parent drops.
        for item in &details.drop_items {
            // Golden bags drop blueprints almost exclusively; we don't
            // track Golden as a tier, so blueprints would just bloat the
            // emoji set without ever rendering. Filter by name — cheap,
            // and bag classification would otherwise need a per-item HTTP
            // fetch we then throw away.
            if item.logical_name.contains("blueprint") {
                continue;
            }
            dump_dungeon.drops.push(WikiEmoji {
                logical_name: item.logical_name.clone(),
                img_url: item.img_url.clone(),
            });

            // Fetch the item's wiki page to classify its bag tier and grab
            // the bag-icon image URL. Cache by drop-table img URL so we
            // don't re-fetch a given item across dungeons.
            let bag_info = match &item.item_wiki_path {
                Some(path) => {
                    if !item_page_cache.contains_key(&item.img_url) {
                        let info = match scrape_item_page(&client, path).await {
                            Ok(info) => info,
                            Err(e) => {
                                track_warn(
                                    progress,
                                    &mut summary,
                                    format!("item page fetch {path} failed: {e:#}"),
                                );
                                ItemBagInfo {
                                    tier: None,
                                    bag_image_url: None,
                                    shiny_image_url: None,
                                }
                            }
                        };
                        item_page_cache.insert(item.img_url.clone(), info);
                    }
                    item_page_cache.get(&item.img_url)
                }
                None => None,
            };
            let bag_tier = bag_info.and_then(|b| b.tier.clone());
            let bag_image_url = bag_info.and_then(|b| b.bag_image_url.clone());
            let shiny_image_url = bag_info.and_then(|b| b.shiny_image_url.clone());

            // First time we see a bag tier's icon in this run, upload it as
            // the global `bag_<tier>` ui emoji so the renderer can use it.
            if let (Some(tier), Some(bag_url)) = (bag_tier.as_deref(), bag_image_url.as_deref()) {
                if !uploaded_bag_tiers.contains(tier) {
                    let bag_name = format!("bag_{tier}");
                    let bag_dname = discord_name(&bag_name);
                    match upload_if_new(
                        &client,
                        &emoji_api,
                        &mut existing,
                        &bag_dname,
                        bag_url,
                        dry_run,
                        &mut summary,
                        progress,
                    )
                    .await
                    {
                        Ok(UploadOutcome::Existing(id, animated))
                        | Ok(UploadOutcome::Uploaded(id, animated)) => {
                            if let Some(pool) = pool {
                                db::emoji::upsert(
                                    pool,
                                    &bag_name,
                                    id as i64,
                                    &bag_dname,
                                    animated,
                                    None,
                                    Some("ui"),
                                    Some(bag_url),
                                    None,
                                )
                                .await?;
                            }
                            uploaded_bag_tiers.insert(tier.to_string());
                        }
                        Ok(UploadOutcome::WouldUpload) => {
                            info!(
                                "[dry-run] would upsert bot_emoji name={bag_name} category=ui (bag icon for tier {tier})"
                            );
                            summary.upserts_skipped_dry += 1;
                            uploaded_bag_tiers.insert(tier.to_string());
                        }
                        Err(e) => track_warn(
                            progress,
                            &mut summary,
                            format!("bag icon {bag_dname}: {e:#}"),
                        ),
                    }
                }
            }

            let dname = discord_name(&item.logical_name);
            // Categorise white-bag items separately so operators can filter
            // the emoji picker by category if they want a "rare drops only"
            // view. Bag-tier grouping in the renderer (R2) is what actually
            // drives headcount/raid display.
            let category = if bag_tier.as_deref() == Some("white") {
                "drop_white"
            } else {
                "drop"
            };
            match upload_if_new(
                &client,
                &emoji_api,
                &mut existing,
                &dname,
                &item.img_url,
                dry_run,
                &mut summary,
                progress,
            )
            .await
            {
                Ok(UploadOutcome::Existing(emoji_id, animated))
                | Ok(UploadOutcome::Uploaded(emoji_id, animated)) => {
                    if let Some(pool) = pool {
                        db::emoji::upsert(
                            pool,
                            &item.logical_name,
                            emoji_id as i64,
                            &dname,
                            animated,
                            None,
                            Some(category),
                            Some(&item.img_url),
                            bag_tier.as_deref(),
                        )
                        .await?;
                    }
                }
                Ok(UploadOutcome::WouldUpload) => {
                    info!(
                        "[dry-run] would upsert bot_emoji name={} category={category} bag_tier={:?}",
                        item.logical_name, bag_tier
                    );
                    summary.upserts_skipped_dry += 1;
                }
                Err(e) => track_warn(progress, &mut summary, format!("drop emoji {dname}: {e:#}")),
            }

            // If the item page has a shiny sprite, upload it as a
            // separate emoji tagged with bag_tier='shiny' and append it
            // to the wiki dump's drops so the seeder derives it into the
            // effective dungeon's showcase_emoji.
            if let Some(shiny_url) = shiny_image_url.as_deref() {
                let shiny_logical = format!("{}_shiny", item.logical_name);
                let shiny_dname = discord_name(&shiny_logical);
                match upload_if_new(
                    &client,
                    &emoji_api,
                    &mut existing,
                    &shiny_dname,
                    shiny_url,
                    dry_run,
                    &mut summary,
                    progress,
                )
                .await
                {
                    Ok(UploadOutcome::Existing(id, animated))
                    | Ok(UploadOutcome::Uploaded(id, animated)) => {
                        if let Some(pool) = pool {
                            db::emoji::upsert(
                                pool,
                                &shiny_logical,
                                id as i64,
                                &shiny_dname,
                                animated,
                                None,
                                Some("drop_shiny"),
                                Some(shiny_url),
                                Some("shiny"),
                            )
                            .await?;
                        }
                        dump_dungeon.drops.push(WikiEmoji {
                            logical_name: shiny_logical,
                            img_url: shiny_url.to_string(),
                        });
                    }
                    Ok(UploadOutcome::WouldUpload) => {
                        info!(
                            "[dry-run] would upsert bot_emoji name={shiny_logical} category=drop_shiny bag_tier=shiny"
                        );
                        summary.upserts_skipped_dry += 1;
                        dump_dungeon.drops.push(WikiEmoji {
                            logical_name: shiny_logical,
                            img_url: shiny_url.to_string(),
                        });
                    }
                    Err(e) => track_warn(
                        progress,
                        &mut summary,
                        format!("shiny emoji {shiny_dname}: {e:#}"),
                    ),
                }
            }
        }

        dump.dungeons.push(dump_dungeon);
    }

    // ---------------------------------------------------------------------
    // Catalogue scrapes. These populate `bot_emoji` only — they're not
    // appended to `dump`, so `templates::load_and_seed` never auto-attaches
    // them to a dungeon's reactions. The operator references them by
    // logical name from `data/dungeon_overrides.json` (e.g. `class_warrior`,
    // `status_paralyzed`) when a specific dungeon needs them.
    // ---------------------------------------------------------------------
    upload_catalogue(
        "classes",
        "class",
        scrape_classes(&client).await,
        &client,
        &emoji_api,
        &mut existing,
        pool,
        dry_run,
        &mut summary,
        progress,
    )
    .await?;
    upload_catalogue(
        "status effects",
        "status_effect",
        scrape_status_effects(&client).await,
        &client,
        &emoji_api,
        &mut existing,
        pool,
        dry_run,
        &mut summary,
        progress,
    )
    .await?;

    progress.emit(ProgressEvent::Phase("writing wiki dump".into()));
    if !dry_run {
        dump.save()?;
        info!("wrote wiki dump → data/wiki_dump.json");
    } else {
        info!(
            "[dry-run] would write data/wiki_dump.json ({} dungeons)",
            dump.dungeons.len()
        );
    }

    info!("sync-wiki complete");
    Ok(summary)
}

/// CLI entry point used by `starship sync-wiki`. Loads config + opens (and
/// migrates) the pool, prompts for `--purge` confirmation on stdin if
/// requested, then defers to [`run_core`]. The Discord command does not
/// reuse this function — it skips env loading (it has live `BotData`
/// already) and uses a slash-command parameter for purge confirmation
/// rather than stdin.
pub async fn run(dry_run: bool, purge: bool) -> Result<()> {
    let config = Config::from_env()?;

    // In dry-run we never touch the DB: no migrate, no writes.
    // sync-wiki only writes `bot_emoji` (emoji mappings) + `data/wiki_dump.json`.
    // Dungeon templates + reactions are seeded separately on bot boot from
    // the dump + `data/dungeon_overrides.json` via `templates::load_and_seed`.
    let pool = if dry_run {
        None
    } else {
        let p = db::create_pool(&config.database_url).await?;
        sqlx::migrate!("./migrations").run(&p).await?;
        Some(p)
    };

    // The CLI prompts on stdin before doing anything destructive. Dry-run
    // logs the would-be action but skips the prompt — the operator hasn't
    // committed to anything yet.
    let purge_confirmed = if purge && !dry_run {
        confirm_purge_via_stdin(&config).await?
    } else {
        purge
    };

    let opts = SyncOptions {
        dry_run,
        purge: purge_confirmed,
    };
    let progress = Progress::noop();
    let summary = run_core(&config, pool.as_ref(), opts, &progress).await?;

    if dry_run {
        info!(
            "[dry-run] summary: {} existing emojis reused, {} new uploads skipped, {} bot_emoji rows skipped, {} warnings",
            summary.uploads_reused,
            summary.uploads_skipped_dry,
            summary.upserts_skipped_dry,
            summary.warnings,
        );
    }

    Ok(())
}

/// Interactive purge confirmation for the CLI path. Lists the application
/// emojis we'd delete, then prompts `[y/N]`. Lives outside [`run_core`]
/// because the Discord variant uses a typed parameter (`PURGE`) instead.
async fn confirm_purge_via_stdin(config: &Config) -> Result<bool> {
    use std::io::Write;

    let client = Client::builder()
        .user_agent(&config.realmeye_user_agent)
        .build()?;
    let emoji_api =
        ApplicationEmojiClient::new(client, &config.discord_token, config.discord_application_id);

    let existing = emoji_api.list().await.unwrap_or_else(|e| {
        warn!("could not list application emojis for purge prompt: {e:#}");
        HashMap::new()
    });

    print!(
        "--purge will DELETE {} application emoji(s) and TRUNCATE bot_emoji. Proceed? [y/N] ",
        existing.len()
    );
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let confirmed = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
    if !confirmed {
        info!("purge aborted by user");
    }
    Ok(confirmed)
}

// ---------------------------------------------------------------------------
// Upload helper.
// ---------------------------------------------------------------------------

/// Upload an emoji to the application if `discord_name` is not already in
/// `existing`. On success, inserts the new emoji into `existing` so a later
/// encounter of the same logical name in this run hits the existing-branch
/// instead of re-POSTing (Discord 400s on duplicate emoji names).
///
/// In dry-run mode, the function performs no writes and returns
/// `UploadOutcome::WouldUpload` for new emojis (existing emojis still return
/// their real ID so the dry-run log shows accurate reuse decisions).
#[allow(clippy::too_many_arguments)]
async fn upload_if_new(
    http: &Client,
    api: &ApplicationEmojiClient,
    existing: &mut std::collections::HashMap<String, (u64, bool)>,
    discord_name: &str,
    img_url: &str,
    dry_run: bool,
    summary: &mut SyncSummary,
    progress: &Progress,
) -> Result<UploadOutcome> {
    if let Some(&(id, animated)) = existing.get(discord_name) {
        info!("skipping {discord_name} (already registered as {id})");
        summary.uploads_reused += 1;
        progress.emit(ProgressEvent::UploadReused);
        return Ok(UploadOutcome::Existing(id, animated));
    }

    if dry_run {
        info!("[dry-run] would upload emoji {discord_name} from {img_url}");
        summary.uploads_skipped_dry += 1;
        progress.emit(ProgressEvent::UploadSkipped);
        return Ok(UploadOutcome::WouldUpload);
    }

    let image_bytes = download_and_resize(http, img_url).await?;
    let result = api.create(discord_name, &image_bytes).await?;
    info!("uploaded emoji {discord_name} -> {}", result.0);
    existing.insert(discord_name.to_string(), result);

    // Brief pause to stay under the application emoji rate limits.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    summary.uploads_new += 1;
    progress.emit(ProgressEvent::UploadNew);

    // TODO: overflow path — if we hit the 2000 application emoji cap, fall
    // back to uploading to a guild emoji server and set source_guild_id.

    Ok(UploadOutcome::Uploaded(result.0, result.1))
}

/// Upload a catalogue (classes / status effects) and write `bot_emoji`
/// rows. `scrape_result` is taken by value so the caller can pass an
/// already-awaited `Result` — a scrape failure is logged and swallowed
/// (we'd rather complete the rest of the run than abort sync-wiki over a
/// secondary index).
#[allow(clippy::too_many_arguments)]
async fn upload_catalogue(
    label: &str,
    category: &'static str,
    scrape_result: Result<Vec<CatalogueEmoji>>,
    http: &Client,
    api: &ApplicationEmojiClient,
    existing: &mut std::collections::HashMap<String, (u64, bool)>,
    pool: Option<&sqlx::PgPool>,
    dry_run: bool,
    summary: &mut SyncSummary,
    progress: &Progress,
) -> Result<()> {
    progress.emit(ProgressEvent::Phase(format!("uploading {label} catalogue")));
    let entries = match scrape_result {
        Ok(v) => v,
        Err(e) => {
            track_warn(
                progress,
                summary,
                format!("failed to scrape {label}: {e:#}"),
            );
            return Ok(());
        }
    };
    info!("scraped {} {label} entries", entries.len());

    for entry in &entries {
        let dname = discord_name(&entry.logical_name);
        match upload_if_new(
            http,
            api,
            existing,
            &dname,
            &entry.img_url,
            dry_run,
            summary,
            progress,
        )
        .await
        {
            Ok(UploadOutcome::Existing(emoji_id, animated))
            | Ok(UploadOutcome::Uploaded(emoji_id, animated)) => {
                if let Some(pool) = pool {
                    db::emoji::upsert(
                        pool,
                        &entry.logical_name,
                        emoji_id as i64,
                        &dname,
                        animated,
                        None,
                        Some(category),
                        Some(&entry.img_url),
                        None,
                    )
                    .await?;
                }
            }
            Ok(UploadOutcome::WouldUpload) => {
                info!(
                    "[dry-run] would upsert bot_emoji name={} category={category} ({})",
                    entry.logical_name, entry.display_name
                );
                summary.upserts_skipped_dry += 1;
            }
            Err(e) => track_warn(progress, summary, format!("{label} emoji {dname}: {e:#}")),
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Scraping helpers.
// ---------------------------------------------------------------------------

async fn scrape_dungeon_list(client: &Client) -> Result<Vec<DungeonEntry>> {
    let url = format!("{}{}", REALMEYE_BASE, DUNGEONS_PATH);
    let html = client
        .get(&url)
        .send()
        .await
        .context("fetching dungeon list")?
        .text()
        .await?;

    // RealmEye splits dungeons across several `<h2>` sections (Realm
    // Dungeons, Realm Event Dungeons, Advanced Dungeons, Oryx's Castle,
    // Wormholes, Heroic Dungeons, Special Event Dungeons, Other Dungeons).
    // We drop the sections we don't want (EXCLUDED_SECTIONS) and keep the
    // rest. Dungeon-level EXCLUDED_SLUGS below is a second safety net for
    // individual dungeons we don't want even from kept sections.
    let section_html = keep_dungeon_sections(&html);

    let doc = Html::parse_fragment(&section_html);

    let mut dungeons = Vec::new();
    let mut seen_slugs = std::collections::HashSet::new();

    for row in doc.select(&SEL_INDEX_ROWS) {
        // Continuation / header / empty rows don't have a dungeon link in the
        // first cell — they're skipped here. The dungeon's name cell carries
        // an <a href="/wiki/..."> that identifies it.
        let name_el = match row.select(&SEL_DUNGEON_NAME_CELL).next() {
            Some(el) => el,
            None => continue,
        };
        let href = match name_el.value().attr("href") {
            Some(h) if h.starts_with("/wiki/") => h.to_string(),
            _ => continue,
        };
        let display_name = name_el.text().collect::<String>().trim().to_string();
        if display_name.is_empty() {
            continue;
        }

        // Require a portal image in column 2 — filters out stray rows that
        // happen to have a first-cell link but aren't actually dungeon rows.
        let portal_img_url = match row
            .select(&SEL_PORTAL_IMG)
            .next()
            .and_then(|el| el.value().attr("src"))
        {
            Some(src) => Some(absolute_url(src)),
            None => continue,
        };

        let key_img_url = row
            .select(&SEL_KEY_IMG)
            .next()
            .and_then(|el| el.value().attr("src"))
            // RealmEye reuses the generic "dungeon-keys" icon as a placeholder
            // on some dungeons that don't have their own key — filter those out.
            .filter(|src| !src.contains("dungeon-keys"))
            .map(absolute_url);

        let logical_name = slug_from_display(&display_name);

        if EXCLUDED_SLUGS.contains(&logical_name.as_str()) {
            info!("excluding dungeon {logical_name} (on denylist)");
            continue;
        }

        if !seen_slugs.insert(logical_name.clone()) {
            // Same dungeon appears in multiple index tables; keep the first.
            continue;
        }

        dungeons.push(DungeonEntry {
            name: logical_name,
            display_name,
            wiki_path: href,
            portal_img_url,
            key_img_url,
        });
    }

    Ok(dungeons)
}

/// Walk every `<h2>` on the dungeons page, partition the HTML into
/// per-section ranges, and concatenate only the ranges whose heading text
/// isn't in `EXCLUDED_SECTIONS`.
///
/// Walking top-level `<h2>`s rather than searching for a single "keep"
/// heading is deliberate: RealmEye doesn't have a "Regular Dungeons"
/// umbrella heading — dungeons are split across Realm / Event / Advanced /
/// Wormholes / Heroic sections — so we identify each section and drop the
/// ones we don't want.
fn keep_dungeon_sections(html: &str) -> String {
    let lower = html.to_lowercase();
    let sections = find_h2_sections(&lower);

    if sections.is_empty() {
        // No headings parsed — return the raw HTML so the caller still
        // gets *something* to work with. Better a noisy over-scrape than
        // an empty result.
        return html.to_string();
    }

    let mut out = String::new();
    for (i, sec) in sections.iter().enumerate() {
        let end = sections
            .get(i + 1)
            .map(|next| next.start)
            .unwrap_or(html.len());
        if is_excluded_section(&sec.text_lower) {
            continue;
        }
        out.push_str(&html[sec.start..end]);
    }
    out
}

#[derive(Debug)]
struct H2Section {
    start: usize,
    text_lower: String,
}

/// Walk all `<h2 ...>...</h2>` occurrences and return their start offsets
/// plus inner text (lowercased). Malformed pairs are skipped.
fn find_h2_sections(haystack_lower: &str) -> Vec<H2Section> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = haystack_lower[cursor..].find("<h2") {
        let start = cursor + rel;
        // Advance past the opening tag (`<h2 ...>`).
        let after_open = match haystack_lower[start..].find('>') {
            Some(end) => start + end + 1,
            None => break,
        };
        // Find the matching closing tag.
        let close_rel = match haystack_lower[after_open..].find("</h2>") {
            Some(e) => e,
            None => break,
        };
        let text_lower = haystack_lower[after_open..after_open + close_rel].to_string();
        out.push(H2Section { start, text_lower });
        cursor = after_open + close_rel + "</h2>".len();
    }
    out
}

fn is_excluded_section(text_lower: &str) -> bool {
    EXCLUDED_SECTIONS
        .iter()
        .any(|needle| text_lower.contains(needle))
}

async fn scrape_dungeon_page(client: &Client, wiki_path: &str) -> Result<DungeonDetails> {
    let url = format!("{}{}", REALMEYE_BASE, wiki_path);
    let html = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .text()
        .await?;

    // Slice the drops section out of the raw HTML so CSS selectors inside
    // only see the one relevant table. Section header naming varies per
    // dungeon (e.g. Snake Pit uses "Notable Drops", not "Drops of
    // Interest"), so match against several known alternatives.
    let section_html = match extract_drops_section(&html) {
        Some(s) => s,
        None => {
            return Ok(DungeonDetails { drop_items: vec![] });
        }
    };

    Ok(DungeonDetails {
        drop_items: parse_drop_items(&section_html),
    })
}

/// Parse drop items out of a pre-sliced drops-section HTML fragment.
///
/// RealmEye groups multi-item drops (UT sets, potion pairs, tier bundles)
/// into a single `<td>` containing N consecutive `<a href="/wiki/…"><img></a>`
/// pairs. The previous implementation pulled `td:first-child img` and took
/// only the first hit per row, silently losing the rest — Void was missing
/// Tier 13 Armor, Potion of Mana, and the whole Void UT set; Cultist Hideout
/// was missing Potion of Mana and four of the five Necromancer UT pieces.
///
/// We now iterate every anchor inside the first cell, then fall back to any
/// bare `<img>`s in that cell whose src wasn't already captured — items that
/// don't have a dedicated wiki page still render as a standalone `<img>`.
fn parse_drop_items(section_html: &str) -> Vec<DropItem> {
    let doc = Html::parse_fragment(section_html);

    let mut drop_items = Vec::new();

    for row in doc.select(&SEL_DROP_ROWS) {
        let mut seen_srcs: HashSet<String> = HashSet::new();

        // Primary pass: every `<a href="/wiki/…"><img></a>` in the first
        // cell is one drop. Iterating all anchors is what captures grouped
        // rows like "Potion of Life + Potion of Mana".
        for anchor in row.select(&SEL_DROP_ROW_ANCHOR) {
            let img_el = match anchor.select(&SEL_ANY_IMG).next() {
                Some(el) => el,
                None => continue,
            };
            let display_name = img_el
                .value()
                .attr("alt")
                .or_else(|| img_el.value().attr("title"))
                .unwrap_or("")
                .trim();
            if display_name.is_empty() {
                continue;
            }
            let img_url = match img_el.value().attr("src") {
                Some(src) => absolute_url(src),
                None => continue,
            };
            seen_srcs.insert(img_url.clone());
            drop_items.push(DropItem {
                logical_name: slug_from_display(display_name),
                img_url,
                item_wiki_path: anchor.value().attr("href").map(|s| s.to_string()),
            });
        }

        // Fallback pass: imgs in the first cell that weren't inside an
        // anchor (items without their own wiki page).
        for img_el in row.select(&SEL_DROP_IMG) {
            let src = match img_el.value().attr("src") {
                Some(s) => s,
                None => continue,
            };
            let img_url = absolute_url(src);
            if seen_srcs.contains(&img_url) {
                continue;
            }
            let display_name = img_el
                .value()
                .attr("alt")
                .or_else(|| img_el.value().attr("title"))
                .unwrap_or("")
                .trim();
            if display_name.is_empty() {
                continue;
            }
            drop_items.push(DropItem {
                logical_name: slug_from_display(display_name),
                img_url,
                item_wiki_path: None,
            });
        }
    }

    drop_items
}

/// Slice out the raw HTML between the first drops-section heading and the
/// next sibling heading of the same level (or end-of-document). Returns
/// `None` if no recognised drops heading is found.
///
/// RealmEye uses a couple of different naming conventions: older dungeons
/// use `<h2 id="drops">Drops of Interest</h2>`; some newer pages use
/// "Notable Drops"; a few have just "Drops". Matching is case-insensitive
/// text search against a short allowlist (see `DROP_SECTION_HEADINGS`).
fn extract_drops_section(html: &str) -> Option<String> {
    let lower = html.to_lowercase();

    // Find the earliest `<hN>` (N in 1..=4) whose inner text contains one
    // of our drops-section names. Track the level so we stop at the next
    // heading of *equal or higher* level — subheadings inside the drops
    // section (e.g. per-boss drop tables on Cultist Hideout) must be
    // included, not used as terminators.
    let (start, level) = find_heading_with_level(&lower, DROP_SECTION_HEADINGS)?;

    let tail = &lower[start + 1..];
    let mut next_heading: Option<usize> = None;
    for lvl in 1..=level {
        let tag = format!("<h{lvl}");
        if let Some(rel) = tail.find(&tag) {
            next_heading = Some(match next_heading {
                Some(x) => x.min(rel),
                None => rel,
            });
        }
    }

    Some(match next_heading {
        Some(rel) => html[start..start + 1 + rel].to_string(),
        None => html[start..].to_string(),
    })
}

/// Like `find_heading_text_offset` but also returns the heading level
/// (1..=4) so callers can scope the section end to equal-or-higher headings.
fn find_heading_with_level(haystack_lower: &str, needles_lower: &[&str]) -> Option<(usize, u32)> {
    for lvl in 1u32..=4 {
        let tag = format!("<h{lvl}");
        let close_tag = format!("</h{lvl}>");
        let mut cursor = 0;
        while let Some(rel) = haystack_lower[cursor..].find(&tag) {
            let open_start = cursor + rel;
            let after_open = match haystack_lower[open_start..].find('>') {
                Some(e) => open_start + e + 1,
                None => break,
            };
            let close_rel = match haystack_lower[after_open..].find(&close_tag) {
                Some(e) => e,
                None => break,
            };
            let inner = &haystack_lower[after_open..after_open + close_rel];
            if needles_lower.iter().any(|n| inner.contains(n)) {
                return Some((open_start, lvl));
            }
            cursor = after_open + close_rel + close_tag.len();
        }
    }
    None
}

/// Fetch an item's RealmEye wiki page and pull the "Loot Bag" classification
/// plus the bag icon URL out of the stats infobox. Also looks for the item's
/// shiny sprite (an `<img alt="... (Shiny)">` anywhere on the page) so the
/// caller can upload shiny variants as distinct emojis.
async fn scrape_item_page(client: &Client, item_path: &str) -> Result<ItemBagInfo> {
    let url = format!("{}{}", REALMEYE_BASE, item_path);
    let html = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .text()
        .await?;

    // Be polite to RealmEye on item-page scrapes (we issue one of these per
    // unique drop; without throttling that's ~50 fast requests).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let doc = Html::parse_document(&html);

    // Pass 1 — find the "Loot Bag" row and classify.
    //
    // Colour lives in the img alt, not the row text: the RealmEye markup is
    // `<img alt="Assigned to White Bag" src="…">` with no visible colour
    // word in the row. Scanning `row.text()` misses every tier because the
    // visible text is literally just "Loot Bag" from the <th>.
    let mut tier: Option<&'static str> = None;
    let mut bag_image_url: Option<String> = None;

    'outer: for row in doc.select(&SEL_ANY_TABLE_ROW) {
        let row_text = row.text().collect::<String>().to_lowercase();
        if !row_text.contains("loot bag") {
            continue;
        }
        for img in row.select(&SEL_ANY_IMG) {
            let alt = img.value().attr("alt").unwrap_or("").to_lowercase();
            let title = img.value().attr("title").unwrap_or("").to_lowercase();
            let haystack = format!("{alt} {title}");
            // Rarest-wins: BAG_TIERS_ORDERED goes low→high, so a later
            // match overwrites an earlier one when an item drops from
            // multiple bag tiers.
            for candidate in BAG_TIERS_ORDERED {
                if haystack.contains(&format!("{candidate} bag")) {
                    tier = Some(candidate);
                }
            }
            if tier.is_some() && bag_image_url.is_none() {
                bag_image_url = img.value().attr("src").map(absolute_url);
            }
            if tier.is_some() {
                break 'outer;
            }
        }
    }

    // Pass 2 — find the shiny sprite. RealmEye has used a couple of
    // conventions for alt text across recent redesigns: `<Name> (Shiny)`
    // on older pages, `Shiny <Name>` on some newer ones, and occasionally
    // bare `Shiny` on sprite-sheet cells. Accept all of them, but skip
    // projectile/bullet cells and obvious non-sprite UI chrome.
    let mut shiny_image_url: Option<String> = None;
    for img in doc.select(&SEL_ANY_IMG) {
        let alt = img.value().attr("alt").unwrap_or("");
        let alt_lower = alt.to_lowercase();
        if !alt_lower.contains("shiny") {
            continue;
        }
        // Skip projectile/bullet sprites and bag/icon UI chrome that also
        // happen to have "shiny" in their alt text.
        if alt_lower.contains("projectile")
            || alt_lower.contains("bullet")
            || alt_lower.contains("fragment")
            || alt_lower.contains(" bag")
            || alt_lower.contains("icon")
        {
            continue;
        }
        shiny_image_url = img.value().attr("src").map(absolute_url);
        break;
    }

    Ok(ItemBagInfo {
        tier: tier.map(|s| s.to_string()),
        bag_image_url,
        shiny_image_url,
    })
}

// ---------------------------------------------------------------------------
// Catalogue scrapers (classes + status effects).
//
// Both pages render their entries as `<a><img alt="…" src="…"></a>` pairs.
// They differ in two ways:
//
// - **Classes** link out to `/wiki/<class>` (one-word lowercase slugs); we
//   take the slug from the URL so we don't accidentally pick up sidebar
//   wiki links (e.g. `/wiki/Special:Recent`) that happen to have an img.
// - **Status effects** link to in-page anchors `#<status>`; the alt text is
//   the canonical label, and we slug from it via `slug_from_display`.
//
// Neither list is added to `wiki_dump.json` — these emojis exist only in
// `bot_emoji` so the operator can name them from `dungeon_overrides.json`.
// ---------------------------------------------------------------------------

async fn scrape_classes(client: &Client) -> Result<Vec<CatalogueEmoji>> {
    let url = format!("{}{}", REALMEYE_BASE, CLASSES_PATH);
    let html = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .text()
        .await?;
    Ok(parse_class_entries(&html))
}

async fn scrape_status_effects(client: &Client) -> Result<Vec<CatalogueEmoji>> {
    let url = format!("{}{}", REALMEYE_BASE, STATUS_EFFECTS_PATH);
    let html = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .text()
        .await?;
    Ok(parse_status_effect_entries(&html))
}

/// Pull every `<a href="/wiki/<slug>"><img alt="…" src="…"></a>` from the
/// classes index where `<slug>` is purely lowercase ASCII letters. The
/// lowercase-only filter is the cheap way to ignore namespaced wiki pages
/// (`/wiki/Special:Foo`, `/wiki/User:Bar`) and section anchors with
/// hyphens or colons that aren't class entries.
fn parse_class_entries(html: &str) -> Vec<CatalogueEmoji> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for anchor in doc.select(&SEL_ANY_ANCHOR) {
        let href = match anchor.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let slug = match href.strip_prefix("/wiki/") {
            Some(s) => s,
            None => continue,
        };
        if slug.is_empty() || !slug.chars().all(|c| c.is_ascii_lowercase()) {
            continue;
        }
        let img = match anchor.select(&SEL_ANY_IMG).next() {
            Some(i) => i,
            None => continue,
        };
        let alt = img.value().attr("alt").unwrap_or("").trim();
        if alt.is_empty() {
            continue;
        }
        let img_url = match img.value().attr("src") {
            Some(s) => absolute_url(s),
            None => continue,
        };

        let logical_name = format!("class_{slug}");
        if !seen.insert(logical_name.clone()) {
            continue;
        }
        out.push(CatalogueEmoji {
            logical_name,
            display_name: alt.to_string(),
            img_url,
        });
    }

    out
}

/// Pull every `<a href="#…"><img alt="…" src="…"></a>` from the status
/// effects index. The href anchor itself is unused — we slug from the alt
/// text, which is the canonical status name (e.g. "Lethal Strike" →
/// `status_lethal_strike`). Anchors without an img child are the textual
/// "No Icon" entries the bot can't render, so they're skipped.
fn parse_status_effect_entries(html: &str) -> Vec<CatalogueEmoji> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for anchor in doc.select(&SEL_ANY_ANCHOR) {
        let href = match anchor.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        if !href.starts_with('#') {
            continue;
        }
        let img = match anchor.select(&SEL_ANY_IMG).next() {
            Some(i) => i,
            None => continue,
        };
        let alt = img.value().attr("alt").unwrap_or("").trim();
        if alt.is_empty() {
            continue;
        }
        let img_url = match img.value().attr("src") {
            Some(s) => absolute_url(s),
            None => continue,
        };

        let slug = slug_from_display(alt);
        if slug.is_empty() {
            continue;
        }
        let logical_name = format!("status_{slug}");
        if !seen.insert(logical_name.clone()) {
            continue;
        }
        out.push(CatalogueEmoji {
            logical_name,
            display_name: alt.to_string(),
            img_url,
        });
    }

    out
}

// ---------------------------------------------------------------------------
// Image helpers.
// ---------------------------------------------------------------------------

async fn download_and_resize(client: &Client, url: &str) -> Result<Vec<u8>> {
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .bytes()
        .await?;

    let img = image::load_from_memory(&bytes).context("decoding image")?;
    let resized = img.resize(EMOJI_MAX_SIDE, EMOJI_MAX_SIDE, FilterType::Lanczos3);

    let mut buf = Vec::new();
    resized
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .context("re-encoding image as PNG")?;

    Ok(buf)
}

// ---------------------------------------------------------------------------
// Utilities.
// ---------------------------------------------------------------------------

/// Convert a logical name to a Discord-safe emoji name (alphanumeric + underscore,
/// max 32 chars, must start with alphanumeric).
fn discord_name(logical: &str) -> String {
    let safe: String = logical
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Discord names must be 2-32 chars and start with alphanumeric.
    let trimmed = safe.trim_start_matches('_');
    let s = if trimmed.is_empty() { "emoji" } else { trimmed };
    s.chars().take(32).collect()
}

fn absolute_url(src: &str) -> String {
    if src.starts_with("http") {
        src.to_string()
    } else if src.starts_with("//") {
        format!("https:{}", src)
    } else {
        format!("{}{}", REALMEYE_BASE, src)
    }
}

// ---------------------------------------------------------------------------
// Purge: wipe every application emoji + every bot_emoji row.
//
// Used once after the apostrophe-slug fix so stale `oryx_s_*` names don't
// linger on the Discord application. The CLI gates this behind a stdin
// Y/N prompt (see [`confirm_purge_via_stdin`]); the Discord command gates
// it behind a slash-command parameter that must equal the literal string
// "PURGE". Never auto-invoked.
// ---------------------------------------------------------------------------

/// Delete every application emoji this bot owns, then TRUNCATE
/// `bot_emoji`. Caller is responsible for any UX confirmation — this
/// function does not prompt.
async fn purge_all_unprompted(
    emoji_api: &ApplicationEmojiClient,
    pool: Option<&sqlx::PgPool>,
) -> Result<()> {
    let existing = emoji_api.list().await.unwrap_or_else(|e| {
        warn!("could not list application emojis for purge: {e:#}");
        HashMap::new()
    });

    info!("purging {} application emojis…", existing.len());
    for (name, (id, _animated)) in &existing {
        if let Err(e) = emoji_api.delete(*id).await {
            warn!("failed to delete emoji {name} ({id}): {e:#}");
        }
        // 100ms between deletes — Discord's per-route bucket refills
        // quickly enough that this keeps us well under rate limits without
        // adding material runtime to the purge.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    if let Some(pool) = pool {
        db::emoji::truncate(pool).await?;
        info!("truncated bot_emoji");
    }

    info!("purge complete; continuing with fresh scrape");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        extract_drops_section, keep_dungeon_sections, parse_class_entries, parse_drop_items,
        parse_status_effect_entries,
    };

    #[test]
    fn drops_section_preserves_subsection_tables() {
        // Cultist Hideout-shaped layout: an h2 "Drops of Interest" followed
        // by an h3 boss subheading whose drop table must stay inside the
        // returned slice. Prior to tracking the heading level, the h3
        // terminated the section and we lost the boss drops.
        let html = "\
            <h2 id=\"drops\">Drops of Interest</h2>\
            <table class=\"table-striped\"><tr><td>NORMAL</td></tr></table>\
            <h3>Malus</h3>\
            <table class=\"table-striped\"><tr><td>BOSS</td></tr></table>\
            <h2 id=\"history\">History</h2>\
            <p>log</p>\
        ";
        let extracted = extract_drops_section(html).expect("drops found");
        assert!(
            extracted.contains("NORMAL"),
            "normal drops missing: {extracted}"
        );
        assert!(
            extracted.contains("BOSS"),
            "boss drops missing: {extracted}"
        );
        assert!(!extracted.contains("log"), "history leaked: {extracted}");
    }

    #[test]
    fn multi_item_row_captures_every_anchor() {
        // Cultist Hideout / The Void style: a single <td> packs multiple
        // <a><img></a> pairs for grouped drops (UT sets, potion pairs, tier
        // bundles). The previous scraper grabbed only the first img via
        // `td:first-child img` and silently dropped the rest.
        let html = r##"
            <table class="table-striped">
                <tr><th>Item</th><th>Drops From</th></tr>
                <tr>
                    <td>
                        <a href="/wiki/potion-of-life"><img src="/s/p_life.png" alt="Potion of Life"></a>
                        <a href="/wiki/potion-of-mana"><img src="/s/p_mana.png" alt="Potion of Mana"></a>
                    </td>
                    <td>Malus</td>
                </tr>
                <tr>
                    <td>
                        <a href="/wiki/burial-blades"><img src="/s/bb.png" alt="Burial Blades"></a>
                        <a href="/wiki/staff-of-unholy-sacrifice"><img src="/s/sus.png" alt="Staff of Unholy Sacrifice"></a>
                        <a href="/wiki/skull-of-corrupted-souls"><img src="/s/scs.png" alt="Skull of Corrupted Souls"></a>
                        <a href="/wiki/ritual-robe"><img src="/s/rr.png" alt="Ritual Robe"></a>
                        <a href="/wiki/bloodshed-ring"><img src="/s/br.png" alt="Bloodshed Ring"></a>
                    </td>
                    <td>Malus</td>
                </tr>
            </table>
        "##;
        let items = parse_drop_items(html);
        let names: Vec<&str> = items.iter().map(|i| i.logical_name.as_str()).collect();
        assert_eq!(
            names,
            &[
                "potion_of_life",
                "potion_of_mana",
                "burial_blades",
                "staff_of_unholy_sacrifice",
                "skull_of_corrupted_souls",
                "ritual_robe",
                "bloodshed_ring",
            ]
        );
        // Every anchor-sourced item carries its wiki path through for the
        // downstream Loot Bag scrape.
        assert!(items.iter().all(|i| i.item_wiki_path.is_some()));
    }

    #[test]
    fn bare_img_without_anchor_still_captured() {
        // Defensive: an item cell with no anchor (hypothetically an item
        // without a dedicated wiki page) should still be picked up via the
        // fallback pass.
        let html = r##"
            <table class="table-striped">
                <tr>
                    <td><img src="/s/mystery.png" alt="Mystery Trinket"></td>
                    <td>Boss</td>
                </tr>
            </table>
        "##;
        let items = parse_drop_items(html);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].logical_name, "mystery_trinket");
        assert!(items[0].item_wiki_path.is_none());
    }

    #[test]
    fn sections_keep_desired_drop_excluded() {
        // Mirrors the real RealmEye /wiki/dungeons heading layout as of
        // 2026-04: a TOC, several keep sections, and trailing
        // Special Event / Other / History we want to drop. The Oryx's
        // Castle section is KEPT because it's where Oryx's Sanctuary
        // lives — the EXCLUDED_SLUGS denylist drops Castle/Chamber/Wine
        // Cellar individually while preserving O3.
        let curly = '\u{2019}';
        let html = format!(
            "\
            <h2 id=\"contents\">Contents</h2>\
            <p>TOC</p>\
            <h2 id=\"realm-dungeons\">Realm Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>REALM_ROW</td></tr></table>\
            <h2 id=\"advanced-dungeons\">Advanced Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>ADVANCED_ROW</td></tr></table>\
            <h2 id=\"oryx-s-castle\">Oryx{curly}s Castle</h2>\
            <table class=\"table-striped\"><tr><td>CASTLE_SECTION_KEEP</td></tr></table>\
            <h2 id=\"heroic\">Heroic Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>HEROIC_ROW</td></tr></table>\
            <h2 id=\"special-event-dungeons\">Special Event Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>EVENT_ROW_DROP</td></tr></table>\
            <h2 id=\"other-dungeons\">Other Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>OTHER_ROW_DROP</td></tr></table>\
            <h2 id=\"history\">History</h2>\
            <p>log</p>\
        "
        );
        let kept = keep_dungeon_sections(&html);
        assert!(kept.contains("REALM_ROW"), "missing REALM: {kept}");
        assert!(kept.contains("ADVANCED_ROW"), "missing ADVANCED: {kept}");
        assert!(kept.contains("HEROIC_ROW"), "missing HEROIC: {kept}");
        assert!(
            kept.contains("CASTLE_SECTION_KEEP"),
            "castle section should be kept (O3 lives there); got: {kept}"
        );
        assert!(!kept.contains("EVENT_ROW_DROP"), "event leaked: {kept}");
        assert!(!kept.contains("OTHER_ROW_DROP"), "other leaked: {kept}");
    }

    #[test]
    fn classes_parser_keeps_class_anchors_only() {
        // Mirrors RealmEye's /wiki/classes layout (2026-04): one anchor per
        // class with a `/wiki/<lowercase>` href and a child img bearing the
        // class name as alt. Sidebar/namespaced links and bare anchors
        // without imgs must be ignored. Duplicates collapse on logical
        // name.
        let html = r##"
            <a href="/wiki/Special:Recent">Recent changes</a>
            <a href="/wiki/wizard"><img src="/s/wiz.png" alt="Wizard"></a>
            <a href="/wiki/priest"><img src="/s/pri.png" alt="Priest"></a>
            <a href="/wiki/wizard"><img src="/s/wiz.png" alt="Wizard"></a>
            <a href="/wiki/main_page"><img src="/s/m.png" alt="Main Page"></a>
            <a href="/wiki/knight"><img src="/s/kt.png" alt="Knight"></a>
            <a href="/wiki/no-img-class"></a>
        "##;
        let entries = parse_class_entries(html);
        let names: Vec<&str> = entries.iter().map(|e| e.logical_name.as_str()).collect();
        // `main_page` has an underscore so it fails the lowercase-letters-only
        // filter and is excluded. Recent changes anchor has no img and is
        // dropped. Duplicate Wizard collapses to one row.
        assert_eq!(names, &["class_wizard", "class_priest", "class_knight"]);
        let wiz = &entries[0];
        assert_eq!(wiz.display_name, "Wizard");
        assert_eq!(wiz.img_url, "https://www.realmeye.com/s/wiz.png");
    }

    #[test]
    fn status_parser_keeps_anchored_imgs_only() {
        // Mirrors /wiki/status-effects: each icon is an `<a href="#name">`
        // whose only child is the icon img. The "No Icon" entries are
        // bare text anchors without imgs — they must not be picked up,
        // since the bot can't render an emoji without a sprite.
        let html = r##"
            <h2>Positive Status Effects</h2>
            <a href="#armored"><img src="/s/arm.png" alt="Armored"></a>
            <a href="#lethal-strike"><img src="/s/ls.png" alt="Lethal Strike"></a>
            <h2>Negative Status Effects</h2>
            <a href="#paralyzed"><img src="/s/par.png" alt="Paralyzed"></a>
            <a href="#paralyzed"><img src="/s/par.png" alt="Paralyzed"></a>
            <h2>No Icon</h2>
            <a href="#stasis">Stasis</a>
            <a href="/wiki/elsewhere"><img src="/s/x.png" alt="Elsewhere"></a>
        "##;
        let entries = parse_status_effect_entries(html);
        let names: Vec<&str> = entries.iter().map(|e| e.logical_name.as_str()).collect();
        // External `/wiki/` anchor is rejected; "No Icon" textual anchor
        // is rejected; multi-word alt slugs to underscored form;
        // duplicates collapse.
        assert_eq!(
            names,
            &["status_armored", "status_lethal_strike", "status_paralyzed"]
        );
        let ls = &entries[1];
        assert_eq!(ls.display_name, "Lethal Strike");
        assert_eq!(ls.img_url, "https://www.realmeye.com/s/ls.png");
    }
}
