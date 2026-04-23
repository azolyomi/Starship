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
use crate::db;
use crate::db::emoji::ApplicationEmojiClient;

// ---------------------------------------------------------------------------
// Selector constants — update these if RealmEye changes their HTML.
// ---------------------------------------------------------------------------

/// Rows in the dungeon index table (one row per dungeon).
const SEL_DUNGEON_ROWS: &str = "table.tablesorter tbody tr";
/// Cell containing the dungeon name link within a row.
const SEL_DUNGEON_NAME_CELL: &str = "td:nth-child(2) a";
/// Cell containing the portal image within a row.
const SEL_PORTAL_IMG: &str = "td:first-child img";
/// Cell containing the key image within a row (optional).
const SEL_KEY_IMG: &str = "td:nth-child(3) img";

/// Rows in the "Drops of Interest" table on a dungeon page.
const SEL_DROP_ROWS: &str = "table.drops-table tbody tr, .drops-of-interest table tbody tr";
/// Item name link in a drop row.
const SEL_DROP_NAME: &str = "td:nth-child(2) a, td a";
/// Drop image in a drop row.
const SEL_DROP_IMG: &str = "td:first-child img";
/// Indicator that an item is a white bag drop (class or text to look for).
const SEL_WHITE_BAG_INDICATOR: &str = "img[alt*='White Bag'], .white-bag";

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
    showcase_emoji: Vec<String>,
    drop_items: Vec<DropItem>,
}

struct DropItem {
    logical_name: String,
    display_name: String,
    img_url: String,
    is_white_bag: bool,
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

pub async fn run() -> Result<()> {
    let config = Config::from_env()?;
    let pool = db::create_pool(&config.database_url).await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    db::dungeon::seed_builtins(&pool).await?;

    let client = Client::builder()
        .user_agent(&config.realmeye_user_agent)
        .build()?;

    let emoji_api = ApplicationEmojiClient::new(
        client.clone(),
        &config.discord_token,
        config.discord_application_id,
    );

    // Build a set of emojis already registered to this application so we can
    // skip unchanged ones and reconcile manual Developer-Portal edits.
    info!("fetching existing application emojis…");
    let existing = emoji_api.list().await.unwrap_or_else(|e| {
        warn!("could not list application emojis: {e:#} — will attempt all uploads");
        std::collections::HashMap::new()
    });
    info!("{} emojis already registered", existing.len());

    info!("scraping dungeon list from RealmEye wiki…");
    let dungeons = scrape_dungeon_list(&client).await?;
    info!("found {} dungeons", dungeons.len());

    for dungeon in &dungeons {
        info!("processing: {}", dungeon.display_name);

        // Upload portal emoji.
        let portal_emoji_name = format!("portal_{}", dungeon.name);
        let portal_discord_name = discord_name(&portal_emoji_name);
        if let Some(url) = &dungeon.portal_img_url {
            if let Ok((emoji_id, animated)) = upload_if_new(
                &client,
                &emoji_api,
                &existing,
                &portal_discord_name,
                url,
            )
            .await
            {
                db::emoji::upsert(
                    &pool,
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

        // Upload key emoji.
        let mut key_emoji_name = None;
        if let Some(url) = &dungeon.key_img_url {
            let name = format!("key_{}", dungeon.name);
            let discord_name = discord_name(&name);
            if let Ok((emoji_id, animated)) = upload_if_new(
                &client,
                &emoji_api,
                &existing,
                &discord_name,
                url,
            )
            .await
            {
                db::emoji::upsert(
                    &pool,
                    &name,
                    emoji_id as i64,
                    &discord_name,
                    animated,
                    None,
                    Some("key"),
                    Some(url),
                )
                .await?;
                key_emoji_name = Some(name);
            }
        }

        // Scrape the dungeon page for drops.
        let details = match scrape_dungeon_page(&client, &dungeon.wiki_path).await {
            Ok(d) => d,
            Err(e) => {
                warn!("failed to scrape {}: {e:#}", dungeon.display_name);
                DungeonDetails { showcase_emoji: vec![], drop_items: vec![] }
            }
        };

        // Upload drop item emojis.
        for item in &details.drop_items {
            let discord_name = discord_name(&item.logical_name);
            if let Ok((emoji_id, animated)) = upload_if_new(
                &client,
                &emoji_api,
                &existing,
                &discord_name,
                &item.img_url,
            )
            .await
            {
                let category = if item.is_white_bag { "drop_white" } else { "drop" };
                db::emoji::upsert(
                    &pool,
                    &item.logical_name,
                    emoji_id as i64,
                    &discord_name,
                    animated,
                    None,
                    Some(category),
                    Some(&item.img_url),
                )
                .await?;
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

        let template_id = db::dungeon::upsert_global_template(
            &pool,
            &dungeon.name,
            &dungeon.display_name,
            portal_emoji,
            None,
            false,
            &showcase,
            dungeon.portal_img_url.as_deref(),
        )
        .await?;

        // Seed a basic interest reaction and key reaction.
        db::dungeon::upsert_reaction(
            &pool,
            template_id,
            "interest",
            "Reacts",
            "react_green",
            1,
            false,
            0,
        )
        .await?;

        if let Some(key_name) = &key_emoji_name {
            db::dungeon::upsert_reaction(
                &pool,
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

    info!("sync-wiki complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Upload helper.
// ---------------------------------------------------------------------------

/// Upload an emoji to the application if `discord_name` is not already in
/// `existing`. Returns `(emoji_id, animated)` from either the existing record
/// or the newly uploaded one.
async fn upload_if_new(
    http: &Client,
    api: &ApplicationEmojiClient,
    existing: &std::collections::HashMap<String, (u64, bool)>,
    discord_name: &str,
    img_url: &str,
) -> Result<(u64, bool)> {
    if let Some(&(id, animated)) = existing.get(discord_name) {
        info!("skipping {discord_name} (already registered as {id})");
        return Ok((id, animated));
    }

    let image_bytes = download_and_resize(http, img_url).await?;
    let result = api.create(discord_name, &image_bytes).await?;
    info!("uploaded emoji {discord_name} -> {}", result.0);

    // Brief pause to stay under the application emoji rate limits.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // TODO: overflow path — if we hit the 2000 application emoji cap, fall
    // back to uploading to a guild emoji server and set source_guild_id.

    Ok(result)
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
    let row_sel = Selector::parse(SEL_DUNGEON_ROWS).unwrap();
    let name_sel = Selector::parse(SEL_DUNGEON_NAME_CELL).unwrap();
    let portal_sel = Selector::parse(SEL_PORTAL_IMG).unwrap();
    let key_sel = Selector::parse(SEL_KEY_IMG).unwrap();

    let mut dungeons = Vec::new();

    for row in doc.select(&row_sel) {
        let name_el = match row.select(&name_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let display_name = name_el.text().collect::<String>().trim().to_string();
        if display_name.is_empty() {
            continue;
        }
        let wiki_path = match name_el.value().attr("href") {
            Some(h) => h.to_string(),
            None => continue,
        };
        let logical_name = slug_from_display(&display_name);

        let portal_img_url = row
            .select(&portal_sel)
            .next()
            .and_then(|el| el.value().attr("src"))
            .map(|src| absolute_url(src));

        let key_img_url = row
            .select(&key_sel)
            .next()
            .and_then(|el| el.value().attr("src"))
            .map(|src| absolute_url(src));

        dungeons.push(DungeonEntry {
            name: logical_name,
            display_name,
            wiki_path,
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

    let doc = Html::parse_document(&html);
    let row_sel = Selector::parse(SEL_DROP_ROWS).unwrap();
    let name_sel = Selector::parse(SEL_DROP_NAME).unwrap();
    let img_sel = Selector::parse(SEL_DROP_IMG).unwrap();
    let wb_sel = Selector::parse(SEL_WHITE_BAG_INDICATOR).unwrap();

    let mut drop_items = Vec::new();
    let mut showcase_emoji = Vec::new();

    for row in doc.select(&row_sel) {
        let display_name = match row.select(&name_sel).next() {
            Some(el) => el.text().collect::<String>().trim().to_string(),
            None => continue,
        };
        if display_name.is_empty() {
            continue;
        }
        let img_url = match row
            .select(&img_sel)
            .next()
            .and_then(|el| el.value().attr("src"))
        {
            Some(src) => absolute_url(src),
            None => continue,
        };
        let logical_name = slug_from_display(&display_name);
        let is_white_bag = row.select(&wb_sel).next().is_some();

        if is_white_bag {
            showcase_emoji.push(logical_name.clone());
        }

        drop_items.push(DropItem {
            logical_name,
            display_name,
            img_url,
            is_white_bag,
        });
    }

    Ok(DungeonDetails { showcase_emoji, drop_items })
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
