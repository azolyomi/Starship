// RealmEye wiki scraper: populates bot_emoji and dungeon_templates from
// https://www.realmeye.com/wiki/dungeons.
//
// CSS selectors and page structure may need adjustment if RealmEye redesigns
// their wiki. All selector constants are grouped at the top for easy updating.
//
// Run as: starship sync-wiki

use anyhow::{Context, Result};
use base64::Engine as _;
use image::imageops::FilterType;
use reqwest::Client;
use scraper::{Html, Selector};
use tracing::{info, warn};

use crate::config::Config;
use crate::db;

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

    let emoji_guild_id = match config.emoji_guild_id {
        Some(id) => id,
        None => {
            warn!("EMOJI_GUILD_ID not set — emoji upload step will be skipped");
            warn!("Set EMOJI_GUILD_ID in .env to the ID of your emoji hosting server");
            0
        }
    };

    let client = Client::builder()
        .user_agent(&config.realmeye_user_agent)
        .build()?;

    info!("scraping dungeon list from RealmEye wiki…");
    let dungeons = scrape_dungeon_list(&client).await?;
    info!("found {} dungeons", dungeons.len());

    if emoji_guild_id != 0 {
        db::emoji::register_emoji_server(&pool, emoji_guild_id as i64, Some("sync-wiki target"))
            .await?;
    }

    for dungeon in &dungeons {
        info!("processing: {}", dungeon.display_name);

        // Upload portal emoji.
        let portal_emoji_name = format!("portal_{}", dungeon.name);
        if let Some(url) = &dungeon.portal_img_url {
            if let Ok(emoji_id) = upload_emoji(
                &client,
                &config.discord_token,
                emoji_guild_id,
                &portal_emoji_name,
                url,
            )
            .await
            {
                db::emoji::upsert(
                    &pool,
                    &portal_emoji_name,
                    emoji_id as i64,
                    Some(emoji_guild_id as i64),
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
            if let Ok(emoji_id) = upload_emoji(
                &client,
                &config.discord_token,
                emoji_guild_id,
                &name,
                url,
            )
            .await
            {
                db::emoji::upsert(
                    &pool,
                    &name,
                    emoji_id as i64,
                    Some(emoji_guild_id as i64),
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
                DungeonDetails {
                    showcase_emoji: vec![],
                    drop_items: vec![],
                }
            }
        };

        // Upload drop item emoji.
        for item in &details.drop_items {
            if let Ok(emoji_id) = upload_emoji(
                &client,
                &config.discord_token,
                emoji_guild_id,
                &item.logical_name,
                &item.img_url,
            )
            .await
            {
                let category = if item.is_white_bag { "drop_white" } else { "drop" };
                db::emoji::upsert(
                    &pool,
                    &item.logical_name,
                    emoji_id as i64,
                    Some(emoji_guild_id as i64),
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

    Ok(DungeonDetails {
        showcase_emoji,
        drop_items,
    })
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
// Discord emoji upload.
// ---------------------------------------------------------------------------

/// Upload an emoji image to a Discord guild. Returns the new emoji's snowflake ID.
/// Skips the upload if emoji_guild_id is 0 (not configured).
async fn upload_emoji(
    client: &Client,
    token: &str,
    guild_id: u64,
    emoji_name: &str,
    img_url: &str,
) -> Result<u64> {
    if guild_id == 0 {
        anyhow::bail!("emoji upload skipped: EMOJI_GUILD_ID not set");
    }

    let image_bytes = download_and_resize(client, img_url).await?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&image_bytes);
    let data_uri = format!("data:image/png;base64,{}", b64);

    let body = serde_json::json!({ "name": emoji_name, "image": data_uri });
    let url = format!("https://discord.com/api/v10/guilds/{}/emojis", guild_id);

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("uploading emoji to Discord")?;

    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after: f64 = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["retry_after"].as_f64())
            .unwrap_or(5.0);
        warn!("rate limited uploading emoji {emoji_name}, sleeping {retry_after}s");
        tokio::time::sleep(std::time::Duration::from_secs_f64(retry_after)).await;
        anyhow::bail!("rate limited — re-run sync-wiki to continue");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Discord emoji upload failed ({status}): {body}");
    }

    let json: serde_json::Value = resp.json().await?;
    let id: u64 = json["id"]
        .as_str()
        .context("emoji id not a string")?
        .parse()
        .context("parsing emoji id")?;

    info!("uploaded emoji {emoji_name} -> {id}");

    // Brief pause to avoid hammering the endpoint (50 emoji/day limit per guild).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(id)
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

fn absolute_url(src: &str) -> String {
    if src.starts_with("http") {
        src.to_string()
    } else if src.starts_with("//") {
        format!("https:{}", src)
    } else {
        format!("{}{}", REALMEYE_BASE, src)
    }
}
