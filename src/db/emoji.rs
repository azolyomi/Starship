use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine as _;
use reqwest::Client;
use serde_json::Value;
use sqlx::PgPool;

use crate::db::models::BotEmoji;

// ---------------------------------------------------------------------------
// DB helpers.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn upsert(
    pool: &PgPool,
    logical_name: &str,
    discord_emoji_id: i64,
    name_on_discord: &str,
    animated: bool,
    source_guild_id: Option<i64>,
    category: Option<&str>,
    realmeye_url: Option<&str>,
    bag_tier: Option<&str>,
) -> Result<()> {
    // bag_tier updates use COALESCE so a later write with None doesn't erase
    // an earlier value. A scrape that finds a drop's bag tier keeps that
    // classification even if a subsequent upsert (e.g. a re-run that fails
    // the item-page fetch) passes NULL.
    sqlx::query(
        r#"
        INSERT INTO bot_emoji
            (logical_name, discord_emoji_id, name_on_discord, animated,
             source_guild_id, category, realmeye_url, bag_tier)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (logical_name)
        DO UPDATE SET
            discord_emoji_id = EXCLUDED.discord_emoji_id,
            name_on_discord  = EXCLUDED.name_on_discord,
            animated         = EXCLUDED.animated,
            source_guild_id  = EXCLUDED.source_guild_id,
            category         = COALESCE(EXCLUDED.category, bot_emoji.category),
            realmeye_url     = COALESCE(EXCLUDED.realmeye_url, bot_emoji.realmeye_url),
            bag_tier         = COALESCE(EXCLUDED.bag_tier, bot_emoji.bag_tier),
            uploaded_at      = NOW()
        "#,
    )
    .bind(logical_name)
    .bind(discord_emoji_id)
    .bind(name_on_discord)
    .bind(animated)
    .bind(source_guild_id)
    .bind(category)
    .bind(realmeye_url)
    .bind(bag_tier)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_all_as_map(pool: &PgPool) -> Result<HashMap<String, BotEmoji>> {
    let all = get_all(pool).await?;
    Ok(all
        .into_iter()
        .map(|e| (e.logical_name.clone(), e))
        .collect())
}

pub async fn get_all(pool: &PgPool) -> Result<Vec<BotEmoji>> {
    let rows = sqlx::query_as::<_, BotEmoji>(
        r#"
        SELECT id, logical_name, discord_emoji_id, name_on_discord, animated,
               source_guild_id, category, realmeye_url, uploaded_at, bag_tier
        FROM bot_emoji ORDER BY logical_name
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Delete every row in bot_emoji. Used by `sync-wiki --purge` to clear stale
/// mappings after an emoji set has been wiped on the Discord side.
pub async fn truncate(pool: &PgPool) -> Result<()> {
    sqlx::query("TRUNCATE TABLE bot_emoji RESTART IDENTITY CASCADE")
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Discord Application Emoji API client.
// ---------------------------------------------------------------------------

pub struct ApplicationEmojiClient {
    client: Client,
    token: String,
    app_id: u64,
}

impl ApplicationEmojiClient {
    pub fn new(client: Client, token: impl Into<String>, app_id: u64) -> Self {
        Self {
            client,
            token: token.into(),
            app_id,
        }
    }

    fn base_url(&self) -> String {
        format!(
            "https://discord.com/api/v10/applications/{}/emojis",
            self.app_id
        )
    }

    fn auth(&self) -> String {
        format!("Bot {}", self.token)
    }

    /// Returns a map of `name_on_discord -> (emoji_id, animated)` for all
    /// emojis currently registered to the application.
    pub async fn list(&self) -> Result<HashMap<String, (u64, bool)>> {
        let resp = self
            .client
            .get(self.base_url())
            .header("Authorization", self.auth())
            .send()
            .await
            .context("listing application emojis")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET application emojis failed ({status}): {body}");
        }

        let json: Value = resp.json().await?;
        let items = json["items"].as_array().cloned().unwrap_or_default();

        let mut map = HashMap::new();
        for item in items {
            let id: u64 = item["id"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let name = item["name"].as_str().unwrap_or("").to_string();
            let animated = item["animated"].as_bool().unwrap_or(false);
            if id != 0 && !name.is_empty() {
                map.insert(name, (id, animated));
            }
        }
        Ok(map)
    }

    /// Upload a new application emoji. Returns `(emoji_id, animated)`.
    /// `name` must match Discord's emoji name rules (alphanumeric + underscores).
    pub async fn create(&self, name: &str, image_bytes: &[u8]) -> Result<(u64, bool)> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(image_bytes);
        let data_uri = format!("data:image/png;base64,{}", b64);
        let body = serde_json::json!({ "name": name, "image": data_uri });

        let resp = self
            .client
            .post(self.base_url())
            .header("Authorization", self.auth())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("creating application emoji")?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v["retry_after"].as_f64())
                .unwrap_or(5.0);
            anyhow::bail!("rate limited uploading emoji {name} (retry_after={retry_after}s) — re-run sync-wiki to continue");
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("create application emoji failed ({status}): {body}");
        }

        let json: Value = resp.json().await?;
        let id: u64 = json["id"]
            .as_str()
            .context("emoji id not a string in response")?
            .parse()
            .context("parsing emoji id")?;
        let animated = json["animated"].as_bool().unwrap_or(false);

        Ok((id, animated))
    }

    /// Delete an application emoji by ID. Returns `Ok(())` on success and on
    /// 404 (already gone), since the caller's intent is "not present on
    /// Discord anymore" either way.
    pub async fn delete(&self, emoji_id: u64) -> Result<()> {
        let url = format!("{}/{}", self.base_url(), emoji_id);
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("deleting application emoji")?;

        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("delete application emoji {emoji_id} failed ({status}): {body}");
    }
}
