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
use reqwest::Client;
use scraper::{Html, Selector};
use tracing::{info, warn};

use crate::config::Config;
use crate::curation::{Curation, SnapshotDungeon, SnapshotEmoji, WikiSnapshot};
use crate::db;
use crate::db::emoji::ApplicationEmojiClient;

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

/// Rows in any dungeon index table.
const SEL_INDEX_ROWS: &str = "table.table-striped tr";
/// Name link (dungeon anchor) in a dungeon row — first cell.
const SEL_DUNGEON_NAME_CELL: &str = "td:first-child a";
/// Portal image — second cell.
const SEL_PORTAL_IMG: &str = "td:nth-child(2) img";
/// Key image — third cell (may be absent).
const SEL_KEY_IMG: &str = "td:nth-child(3) img";

/// Rows in the "Drops of Interest" table on a dungeon page.
const SEL_DROP_ROWS: &str = "table.table-striped tr";
/// Drop item image — first cell of a drop row.
const SEL_DROP_IMG: &str = "td:first-child img";

const REALMEYE_BASE: &str = "https://www.realmeye.com";
const DUNGEONS_PATH: &str = "/wiki/dungeons";

// Discord emoji constraints.
const EMOJI_MAX_SIDE: u32 = 128;

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
/// `<h2>` on the page. RealmEye uses a curly apostrophe in "Oryx's Castle"
/// so both variants are listed.
///
/// Included sections (everything not in this list): Realm Dungeons, Realm
/// Event Dungeons, Advanced Dungeons, Wormholes, Heroic Dungeons.
const EXCLUDED_SECTIONS: &[&str] = &[
    "contents",
    "oryx\u{2019}s castle",
    "oryx's castle",
    "special event dungeons",
    "other dungeons",
    "history",
];

/// Case-insensitive text matches (section heading name contains) for the
/// drops-of-interest section on a dungeon page. The wiki isn't consistent —
/// some dungeons use "Drops of Interest" while others use "Notable Drops" or
/// plain "Drops".
const DROP_SECTION_HEADINGS: &[&str] = &[
    "drops of interest",
    "notable drops",
    "drops",
];

/// Ordered list of bag tier names (matches the bag_tiers lookup table). Used
/// to classify items from their "Loot Bag" row on RealmEye. When an item
/// lists multiple bag tiers, we keep the rarest (last match wins).
const BAG_TIERS_ORDERED: &[&str] = &[
    "brown", "pink", "purple", "cyan", "blue", "orange", "red", "white",
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

/// Result of fetching an item's wiki page for bag-tier classification.
struct ItemBagInfo {
    /// Bag tier name (matches bag_tiers.name). `None` if the "Loot Bag" row
    /// wasn't found or no known colour word was present.
    tier: Option<String>,
    /// Absolute URL of the bag icon image inside the "Loot Bag" row, if any.
    /// Used once per tier to upload the `bag_<tier>` ui emoji.
    bag_image_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DrySummary {
    existing_reused: usize,
    would_upload: usize,
    would_upsert_emoji: usize,
    would_upsert_template: usize,
    would_upsert_reaction: usize,
}

enum UploadOutcome {
    /// Emoji already present on Discord; ID carried forward to the DB.
    Existing(u64, bool),
    /// Emoji freshly uploaded; ID returned by the Discord API.
    Uploaded(u64, bool),
    /// Dry-run: would upload, no network write performed.
    WouldUpload,
}

pub async fn run(dry_run: bool, purge: bool) -> Result<()> {
    let config = Config::from_env()?;

    // curation.json is the user's allowlist of reactions and drops per
    // dungeon. Dungeons missing from the file are uncurated — treated as
    // "keep everything the scraper finds" so first-time syncs capture the
    // full picture and the curator can prompt for selections afterward.
    let mut curation = Curation::load()?;
    let migrated = curation.migrate_legacy_slugs();
    if curation.dungeons.is_empty() {
        info!("no curation found at data/curation.json — syncing everything");
    } else {
        info!(
            "loaded curation: {} dungeons curated — entries outside the allowlist will be skipped",
            curation.dungeons.len()
        );
    }
    if migrated > 0 && !dry_run {
        curation.save()?;
        info!("migrated {migrated} legacy curation slug(s) (apostrophe fix) and rewrote data/curation.json");
    } else if migrated > 0 {
        info!("[dry-run] would migrate {migrated} legacy curation slug(s) (apostrophe fix)");
    }

    // In dry-run we never touch the DB: no migrate, no seed, no writes.
    let pool = if dry_run {
        info!("dry-run mode: no Discord POSTs and no DB writes will be performed");
        None
    } else {
        let p = db::create_pool(&config.database_url).await?;
        sqlx::migrate!("./migrations").run(&p).await?;
        db::dungeon::seed_builtins(&p, &curation).await?;
        Some(p)
    };

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
            purge_all(&emoji_api, pool.as_ref()).await?;
        }
    }

    // Emojis already registered on this application. We mutate this map as
    // new uploads succeed so the same logical emoji isn't uploaded twice in
    // a single run — e.g. `potion_of_wisdom` appears in many dungeons'
    // drops-of-interest tables and Discord 400s on duplicate names.
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

    info!("scraping dungeon list from RealmEye wiki…");
    let dungeons = scrape_dungeon_list(&client).await?;
    info!("found {} dungeons", dungeons.len());

    let mut summary = DrySummary::default();
    let mut snapshot = WikiSnapshot {
        generated_at: Some(chrono::Utc::now()),
        dungeons: Vec::with_capacity(dungeons.len()),
    };

    for dungeon in &dungeons {
        info!("processing: {}", dungeon.display_name);

        let mut snap_dungeon = SnapshotDungeon {
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
            snap_dungeon.portal = Some(SnapshotEmoji {
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
            )
            .await
            {
                Ok(UploadOutcome::Existing(emoji_id, animated))
                | Ok(UploadOutcome::Uploaded(emoji_id, animated)) => {
                    if let Some(pool) = &pool {
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
                    summary.would_upsert_emoji += 1;
                }
                Err(e) => warn!("portal emoji {portal_discord_name}: {e:#}"),
            }
        }

        // Upload key emoji.
        let mut key_emoji_name = None;
        if let Some(url) = &dungeon.key_img_url {
            let name = format!("key_{}", dungeon.name);
            let dname = discord_name(&name);
            snap_dungeon.key = Some(SnapshotEmoji {
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
            )
            .await
            {
                Ok(UploadOutcome::Existing(emoji_id, animated))
                | Ok(UploadOutcome::Uploaded(emoji_id, animated)) => {
                    if let Some(pool) = &pool {
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
                    key_emoji_name = Some(name);
                }
                Ok(UploadOutcome::WouldUpload) => {
                    info!("[dry-run] would upsert bot_emoji name={name} category=key");
                    summary.would_upsert_emoji += 1;
                    key_emoji_name = Some(name);
                }
                Err(e) => warn!("key emoji {dname}: {e:#}"),
            }
        }

        // Scrape the dungeon page for drops.
        let details = match scrape_dungeon_page(&client, &dungeon.wiki_path).await {
            Ok(d) => d,
            Err(e) => {
                warn!("failed to scrape {}: {e:#}", dungeon.display_name);
                DungeonDetails { drop_items: vec![] }
            }
        };

        // Upload drop item emojis.
        for item in &details.drop_items {
            snap_dungeon.drops.push(SnapshotEmoji {
                logical_name: item.logical_name.clone(),
                img_url: item.img_url.clone(),
            });

            // Curated-out drops aren't uploaded or written to the DB. They
            // still appear in the snapshot, so the curator sees them as
            // available to (re)select.
            if !curation.should_keep_drop(&dungeon.name, &item.logical_name) {
                info!(
                    "skipping drop {} (not in curation for {})",
                    item.logical_name, dungeon.name
                );
                continue;
            }

            // Fetch the item's wiki page to classify its bag tier and grab
            // the bag-icon image URL. Cache by drop-table img URL so we
            // don't re-fetch a given item across dungeons.
            let bag_info = match &item.item_wiki_path {
                Some(path) => {
                    if !item_page_cache.contains_key(&item.img_url) {
                        let info = scrape_item_page(&client, path).await.unwrap_or_else(|e| {
                            warn!("item page fetch {path} failed: {e:#}");
                            ItemBagInfo { tier: None, bag_image_url: None }
                        });
                        item_page_cache.insert(item.img_url.clone(), info);
                    }
                    item_page_cache.get(&item.img_url)
                }
                None => None,
            };
            let bag_tier = bag_info.and_then(|b| b.tier.clone());
            let bag_image_url = bag_info.and_then(|b| b.bag_image_url.clone());

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
                    )
                    .await
                    {
                        Ok(UploadOutcome::Existing(id, animated))
                        | Ok(UploadOutcome::Uploaded(id, animated)) => {
                            if let Some(pool) = &pool {
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
                            summary.would_upsert_emoji += 1;
                            uploaded_bag_tiers.insert(tier.to_string());
                        }
                        Err(e) => warn!("bag icon {bag_dname}: {e:#}"),
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
            )
            .await
            {
                Ok(UploadOutcome::Existing(emoji_id, animated))
                | Ok(UploadOutcome::Uploaded(emoji_id, animated)) => {
                    if let Some(pool) = &pool {
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
                    summary.would_upsert_emoji += 1;
                }
                Err(e) => warn!("drop emoji {dname}: {e:#}"),
            }
        }

        // Upsert the dungeon template.
        // showcase_emoji now carries *every* scraped drop for this dungeon
        // (not just white-bag items). The R2 renderer groups them by
        // bag_tier via the bot_emoji table and hides tiers below the
        // per-guild threshold. Built-in templates may still seed an initial
        // set but the scraper's full list is the source of truth.
        let portal_emoji = dungeon.portal_img_url.as_ref().map(|_| portal_emoji_name.as_str());
        let showcase: Vec<String> = details
            .drop_items
            .iter()
            .map(|i| i.logical_name.clone())
            .collect();

        let template_id = if let Some(pool) = &pool {
            db::dungeon::upsert_global_template(
                pool,
                &dungeon.name,
                &dungeon.display_name,
                portal_emoji,
                None,
                false,
                &showcase,
                dungeon.portal_img_url.as_deref(),
            )
            .await?
        } else {
            info!(
                "[dry-run] would upsert dungeon_template name={} display=\"{}\" portal={:?} showcase={:?}",
                dungeon.name, dungeon.display_name, portal_emoji, showcase
            );
            summary.would_upsert_template += 1;
            // Sentinel template id — no reactions will be inserted in dry-run.
            0
        };

        // Seed a basic interest reaction and key reaction. Filtered through
        // curation so the user's "only keep `interest`" decision doesn't get
        // resurrected by the next sync.
        let want_interest = curation.should_keep_reaction(&dungeon.name, "interest");
        let want_key = curation.should_keep_reaction(&dungeon.name, "key");

        if let Some(pool) = &pool {
            if want_interest {
                db::dungeon::upsert_reaction(
                    pool,
                    template_id,
                    "interest",
                    "Reacts",
                    "✅",
                    1,
                    false,
                    0,
                )
                .await?;
            }

            if want_key {
                if let Some(key_name) = &key_emoji_name {
                    db::dungeon::upsert_reaction(
                        pool,
                        template_id,
                        "key",
                        "Key",
                        key_name,
                        1,
                        true,
                        1,
                    )
                    .await?;
                }
            }
        } else {
            if want_interest {
                info!(
                    "[dry-run] would upsert reaction template={} key=interest label=\"Reacts\" emoji=\"✅\"",
                    dungeon.name
                );
                summary.would_upsert_reaction += 1;
            }
            if want_key {
                if let Some(key_name) = &key_emoji_name {
                    info!(
                        "[dry-run] would upsert reaction template={} key=key label=\"Key\" emoji={key_name} confirm=true",
                        dungeon.name
                    );
                    summary.would_upsert_reaction += 1;
                }
            }
        }

        snapshot.dungeons.push(snap_dungeon);
    }

    if !dry_run {
        snapshot.save()?;
        info!("wrote wiki snapshot → data/wiki-snapshot.json");
    } else {
        info!("[dry-run] would write data/wiki-snapshot.json ({} dungeons)", snapshot.dungeons.len());
    }

    if dry_run {
        info!(
            "[dry-run] summary: {} existing emojis reused, {} new uploads skipped, {} bot_emoji rows skipped, {} dungeon_templates skipped, {} dungeon_reactions skipped",
            summary.existing_reused,
            summary.would_upload,
            summary.would_upsert_emoji,
            summary.would_upsert_template,
            summary.would_upsert_reaction,
        );
    }

    info!("sync-wiki complete");
    Ok(())
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
async fn upload_if_new(
    http: &Client,
    api: &ApplicationEmojiClient,
    existing: &mut std::collections::HashMap<String, (u64, bool)>,
    discord_name: &str,
    img_url: &str,
    dry_run: bool,
    summary: &mut DrySummary,
) -> Result<UploadOutcome> {
    if let Some(&(id, animated)) = existing.get(discord_name) {
        info!("skipping {discord_name} (already registered as {id})");
        if dry_run {
            summary.existing_reused += 1;
        }
        return Ok(UploadOutcome::Existing(id, animated));
    }

    if dry_run {
        info!("[dry-run] would upload emoji {discord_name} from {img_url}");
        summary.would_upload += 1;
        return Ok(UploadOutcome::WouldUpload);
    }

    let image_bytes = download_and_resize(http, img_url).await?;
    let result = api.create(discord_name, &image_bytes).await?;
    info!("uploaded emoji {discord_name} -> {}", result.0);
    existing.insert(discord_name.to_string(), result);

    // Brief pause to stay under the application emoji rate limits.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // TODO: overflow path — if we hit the 2000 application emoji cap, fall
    // back to uploading to a guild emoji server and set source_guild_id.

    Ok(UploadOutcome::Uploaded(result.0, result.1))
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
    let row_sel = Selector::parse(SEL_INDEX_ROWS).unwrap();
    let name_sel = Selector::parse(SEL_DUNGEON_NAME_CELL).unwrap();
    let portal_sel = Selector::parse(SEL_PORTAL_IMG).unwrap();
    let key_sel = Selector::parse(SEL_KEY_IMG).unwrap();

    let mut dungeons = Vec::new();
    let mut seen_slugs = std::collections::HashSet::new();

    for row in doc.select(&row_sel) {
        // Continuation / header / empty rows don't have a dungeon link in the
        // first cell — they're skipped here. The dungeon's name cell carries
        // an <a href="/wiki/..."> that identifies it.
        let name_el = match row.select(&name_sel).next() {
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
            .select(&portal_sel)
            .next()
            .and_then(|el| el.value().attr("src"))
        {
            Some(src) => Some(absolute_url(src)),
            None => continue,
        };

        let key_img_url = row
            .select(&key_sel)
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

    let doc = Html::parse_fragment(&section_html);
    let row_sel = Selector::parse(SEL_DROP_ROWS).unwrap();
    let img_sel = Selector::parse(SEL_DROP_IMG).unwrap();
    let item_link_sel = Selector::parse(r#"td:first-child a[href^="/wiki/"]"#).unwrap();

    let mut drop_items = Vec::new();

    for row in doc.select(&row_sel) {
        // First-cell img both identifies the item and gives its emoji source.
        let img_el = match row.select(&img_sel).next() {
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
        let logical_name = slug_from_display(display_name);

        // Item's wiki page, used by scrape_item_page to fetch the Loot Bag
        // row. Prefer the explicit anchor over any slug-derived guess —
        // some item pages use disambiguated URLs (e.g. /wiki/item/Fire_Sword
        // rather than /wiki/fire_sword).
        let item_wiki_path = row
            .select(&item_link_sel)
            .next()
            .and_then(|el| el.value().attr("href"))
            .map(|s| s.to_string());

        drop_items.push(DropItem {
            logical_name,
            img_url,
            item_wiki_path,
        });
    }

    Ok(DungeonDetails { drop_items })
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
    // of our drops-section names.
    let start = find_heading_text_offset(&lower, DROP_SECTION_HEADINGS)?;

    // End at the next heading of any level. We only need to get *out* of
    // the drops table before whatever comes next.
    let tail = &lower[start + 1..];
    let mut next_heading: Option<usize> = None;
    for tag in &["<h1", "<h2", "<h3", "<h4"] {
        if let Some(rel) = tail.find(tag) {
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

/// Scan every `<hN ...>...</hN>` (N in 1..=4) and return the byte offset of
/// the first whose inner text contains any of `needles_lower` (already
/// lowercased). Used for flexible drop-section detection where the exact
/// heading text varies per dungeon page.
fn find_heading_text_offset(haystack_lower: &str, needles_lower: &[&str]) -> Option<usize> {
    for tag in &["<h1", "<h2", "<h3", "<h4"] {
        let close_tag = format!("</{}>", &tag[1..]);
        let mut cursor = 0;
        while let Some(rel) = haystack_lower[cursor..].find(tag) {
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
                return Some(open_start);
            }
            cursor = after_open + close_rel + close_tag.len();
        }
    }
    None
}

/// Fetch an item's RealmEye wiki page and pull the "Loot Bag" classification
/// plus the bag icon URL out of the stats infobox. Returns both values as
/// `None` if the item page has no loot-bag row (some items are untiered).
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
    // The stats infobox is `table.item`, `table.stats`, or similar class
    // combinations depending on the template. Cast a wide net and look
    // inside all <tr> rows for one whose first cell (th or td) contains
    // "loot bag".
    let row_sel = Selector::parse("table tr").unwrap();
    let img_sel = Selector::parse("img").unwrap();

    for row in doc.select(&row_sel) {
        let row_text = row.text().collect::<String>().to_lowercase();
        if !row_text.contains("loot bag") {
            continue;
        }
        // Classify by picking the bag colour word mentioned in the row.
        // Multiple colours can be listed (some items drop from several
        // bag tiers) — keep the rarest (highest sort_order in
        // BAG_TIERS_ORDERED).
        let mut tier: Option<&'static str> = None;
        for candidate in BAG_TIERS_ORDERED {
            if row_text.contains(&format!("{candidate} bag")) {
                tier = Some(candidate);
            }
        }

        // The bag icon lives inside the same row — img element whose src
        // typically points at /s/a/<hash>.png for bag sprites.
        let bag_image_url = row
            .select(&img_sel)
            .next()
            .and_then(|el| el.value().attr("src"))
            .map(absolute_url);

        return Ok(ItemBagInfo {
            tier: tier.map(|s| s.to_string()),
            bag_image_url,
        });
    }

    Ok(ItemBagInfo { tier: None, bag_image_url: None })
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
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Png,
        )
        .context("re-encoding image as PNG")?;

    Ok(buf)
}

// ---------------------------------------------------------------------------
// Utilities.
// ---------------------------------------------------------------------------

/// Normalise a display name to a logical slug. Apostrophes are *stripped*
/// (not replaced with underscores) so "Oryx's Sanctuary" collapses cleanly
/// to "oryxs_sanctuary" rather than "oryx_s_sanctuary" — the old behaviour
/// produced spurious single-letter segments that broke emoji name lookups.
fn slug_from_display(name: &str) -> String {
    let stripped: String = name
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .collect();
    stripped
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

#[cfg(test)]
mod tests {
    use super::{keep_dungeon_sections, slug_from_display};

    #[test]
    fn slug_strips_straight_apostrophe() {
        assert_eq!(slug_from_display("Oryx's Sanctuary"), "oryxs_sanctuary");
        assert_eq!(slug_from_display("Pirate's Cave"), "pirates_cave");
    }

    #[test]
    fn slug_strips_curly_apostrophe() {
        assert_eq!(slug_from_display("Oryx\u{2019}s Sanctuary"), "oryxs_sanctuary");
    }

    #[test]
    fn slug_basic_whitespace_and_punct() {
        assert_eq!(slug_from_display("Snake Pit"), "snake_pit");
        assert_eq!(slug_from_display("D.O.G. Realm"), "d_o_g_realm");
    }

    #[test]
    fn slug_collapses_multiple_separators() {
        assert_eq!(slug_from_display("  Lost   Halls  "), "lost_halls");
    }

    #[test]
    fn sections_keep_desired_drop_excluded() {
        // Mirrors the real RealmEye /wiki/dungeons heading layout as of
        // 2026-04: a TOC, several keep sections, an Oryx's Castle section
        // we want to drop, and trailing Special Event / Other / History.
        // The real page uses a curly apostrophe in "Oryx's Castle" — we
        // format-insert it so the escape is actually interpreted.
        let curly = '\u{2019}';
        let html = format!("\
            <h2 id=\"contents\">Contents</h2>\
            <p>TOC</p>\
            <h2 id=\"realm-dungeons\">Realm Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>REALM_ROW</td></tr></table>\
            <h2 id=\"advanced-dungeons\">Advanced Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>ADVANCED_ROW</td></tr></table>\
            <h2 id=\"oryx-s-castle\">Oryx{curly}s Castle</h2>\
            <table class=\"table-striped\"><tr><td>CASTLE_ROW_DROP</td></tr></table>\
            <h2 id=\"heroic\">Heroic Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>HEROIC_ROW</td></tr></table>\
            <h2 id=\"special-event-dungeons\">Special Event Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>EVENT_ROW_DROP</td></tr></table>\
            <h2 id=\"other-dungeons\">Other Dungeons</h2>\
            <table class=\"table-striped\"><tr><td>OTHER_ROW_DROP</td></tr></table>\
            <h2 id=\"history\">History</h2>\
            <p>log</p>\
        ");
        let kept = keep_dungeon_sections(&html);
        assert!(kept.contains("REALM_ROW"), "missing REALM: {kept}");
        assert!(kept.contains("ADVANCED_ROW"), "missing ADVANCED: {kept}");
        assert!(kept.contains("HEROIC_ROW"), "missing HEROIC: {kept}");
        assert!(!kept.contains("CASTLE_ROW_DROP"), "castle leaked: {kept}");
        assert!(!kept.contains("EVENT_ROW_DROP"), "event leaked: {kept}");
        assert!(!kept.contains("OTHER_ROW_DROP"), "other leaked: {kept}");
    }
}

/// Convert a logical name to a Discord-safe emoji name (alphanumeric + underscore,
/// max 32 chars, must start with alphanumeric).
fn discord_name(logical: &str) -> String {
    let safe: String = logical
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
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
// linger on the Discord application. Interactive: prompts for Y/N before
// touching anything. Never auto-invoked; gated behind --purge flag.
// ---------------------------------------------------------------------------

async fn purge_all(
    emoji_api: &ApplicationEmojiClient,
    pool: Option<&sqlx::PgPool>,
) -> Result<()> {
    use std::io::Write;

    let existing = emoji_api.list().await.unwrap_or_else(|e| {
        warn!("could not list application emojis for purge: {e:#}");
        HashMap::new()
    });

    print!(
        "--purge will DELETE {} application emoji(s) and TRUNCATE bot_emoji. Proceed? [y/N] ",
        existing.len()
    );
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        info!("purge aborted by user");
        return Ok(());
    }

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
