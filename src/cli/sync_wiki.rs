// RealmEye wiki scraper: populates bot_emoji and dungeon_templates from
// https://www.realmeye.com/wiki/dungeons.
//
// CSS selectors and page structure may need adjustment if RealmEye redesigns
// their wiki. All selector constants are grouped at the top for easy updating.
//
// Run as: starship sync-wiki

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
    is_white_bag: bool,
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

pub async fn run(dry_run: bool) -> Result<()> {
    let config = Config::from_env()?;

    // curation.json is the user's allowlist of reactions and drops per
    // dungeon. Dungeons missing from the file are uncurated — treated as
    // "keep everything the scraper finds" so first-time syncs capture the
    // full picture and the curator can prompt for selections afterward.
    let curation = Curation::load()?;
    if curation.dungeons.is_empty() {
        info!("no curation found at data/curation.json — syncing everything");
    } else {
        info!(
            "loaded curation: {} dungeons curated — entries outside the allowlist will be skipped",
            curation.dungeons.len()
        );
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

            let dname = discord_name(&item.logical_name);
            let category = if item.is_white_bag { "drop_white" } else { "drop" };
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
                        )
                        .await?;
                    }
                }
                Ok(UploadOutcome::WouldUpload) => {
                    info!(
                        "[dry-run] would upsert bot_emoji name={} category={category}",
                        item.logical_name
                    );
                    summary.would_upsert_emoji += 1;
                }
                Err(e) => warn!("drop emoji {dname}: {e:#}"),
            }
        }

        // Upsert the dungeon template.
        let portal_emoji = dungeon.portal_img_url.as_ref().map(|_| portal_emoji_name.as_str());
        let showcase: Vec<String> = details
            .drop_items
            .iter()
            .filter(|i| i.is_white_bag)
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

    let doc = Html::parse_document(&html);
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
            .map(|src| absolute_url(src));

        let logical_name = slug_from_display(&display_name);
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

async fn scrape_dungeon_page(client: &Client, wiki_path: &str) -> Result<DungeonDetails> {
    let url = format!("{}{}", REALMEYE_BASE, wiki_path);
    let html = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .text()
        .await?;

    // Slice the "Drops of Interest" section out of the raw HTML before parsing
    // so CSS selectors inside only see the one relevant table. The section is
    // bounded by <h2 id="drops"> on the top and the next <h2 id="..."> below.
    let section_html = match extract_drops_section(&html) {
        Some(s) => s,
        None => {
            return Ok(DungeonDetails { drop_items: vec![] });
        }
    };

    let doc = Html::parse_fragment(&section_html);
    let row_sel = Selector::parse(SEL_DROP_ROWS).unwrap();
    let img_sel = Selector::parse(SEL_DROP_IMG).unwrap();

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
            .trim()
            .to_string();
        if display_name.is_empty() {
            continue;
        }
        let img_url = match img_el.value().attr("src") {
            Some(src) => absolute_url(src),
            None => continue,
        };
        let logical_name = slug_from_display(&display_name);

        // White-bag classification isn't reliably encoded in the RealmEye HTML
        // today, so every drop-of-interest is recorded as a generic drop.
        // Showcase stays empty and can be populated per-guild via the UI.
        drop_items.push(DropItem {
            logical_name,
            img_url,
            is_white_bag: false,
        });
    }

    Ok(DungeonDetails { drop_items })
}

/// Slice out the raw HTML between `<h2 id="drops">` and the next `<h2 id="...">`
/// sibling (or end-of-document). Returns `None` if there's no drops heading.
fn extract_drops_section(html: &str) -> Option<String> {
    let drops_marker = r#"<h2 id="drops""#;
    let start = html.find(drops_marker)?;
    let rest = &html[start..];
    // Find the next <h2 id="..."> header, which terminates the drops section.
    let end_rel = rest[1..]
        .find(r#"<h2 id=""#)
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    Some(rest[..end_rel].to_string())
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

fn slug_from_display(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
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
